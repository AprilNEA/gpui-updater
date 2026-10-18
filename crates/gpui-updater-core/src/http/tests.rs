#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use super::*;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Deterministic "update artifact" bytes, stable so a download can be
/// asserted byte-for-byte and a truncated copy is detectable. Kept ASCII so
/// the same payload doubles as a text body for the metadata-fetch test.
fn artifact() -> Vec<u8> {
    (0u8..128).cycle().take(200 * 1024).collect()
}

/// How the fault server mistreats a connection.
#[derive(Clone, Copy)]
enum Fault {
    /// Accept then close before responding — a tunnel dropped at connect
    /// (the issue #306 `http error: io: …` shape).
    ResetAtConnect,
    /// Answer 503 — backend briefly unavailable.
    Status503,
    /// Send headers + half the body, then close — dropped mid-download.
    PartialBody,
    /// Answer 404 — a permanent error that must not be retried.
    NotFound,
    /// Read the request then go silent with the socket held open — a
    /// black-holed connection where response headers never arrive.
    SilentAfterRequest,
    /// Send headers + half the body, then go silent with the socket held
    /// open — a mid-download stall rather than a close.
    StallMidBody,
    /// Send headers immediately, then trickle the body in delayed chunks —
    /// a slow but always-progressing download.
    TrickleBody,
    /// Send the body without `Content-Length`, ending it by closing the
    /// connection.
    Unsized,
    /// Send a body one byte larger than a sidecar may be.
    OversizedSidecar,
    /// Redirect to a plain-HTTP URL off the machine.
    RedirectOffMachine,
}

/// Serve `fault` for the first `fail_first` connections, then a healthy
/// artifact. `fail_first = usize::MAX` never recovers.
fn spawn_server(fault: Fault, fail_first: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let n = seen.fetch_add(1, Ordering::SeqCst);
            let active = (n < fail_first).then_some(fault);
            thread::spawn(move || handle(stream, active));
        }
    });
    addr
}

fn read_request(stream: &mut TcpStream) {
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf);
}

fn serve_body(stream: &mut TcpStream, body: &[u8]) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(body);
}

fn serve_head(stream: &mut TcpStream, content_length: usize) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\n\r\n"
    );
}

fn handle(mut stream: TcpStream, fault: Option<Fault>) {
    match fault {
        Some(Fault::ResetAtConnect) => drop(stream),
        Some(Fault::Status503) => {
            read_request(&mut stream);
            let _ =
                stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n");
        }
        Some(Fault::PartialBody) => {
            read_request(&mut stream);
            let body = artifact();
            serve_head(&mut stream, body.len());
            let _ = stream.write_all(&body[..body.len() / 2]);
        }
        Some(Fault::NotFound) => {
            read_request(&mut stream);
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        }
        Some(Fault::SilentAfterRequest) => {
            read_request(&mut stream);
            thread::sleep(Duration::from_secs(10));
        }
        Some(Fault::StallMidBody) => {
            read_request(&mut stream);
            let body = artifact();
            serve_head(&mut stream, body.len());
            let _ = stream.write_all(&body[..body.len() / 2]);
            let _ = stream.flush();
            thread::sleep(Duration::from_secs(10));
        }
        Some(Fault::TrickleBody) => {
            read_request(&mut stream);
            let body = artifact();
            serve_head(&mut stream, body.len());
            for chunk in body.chunks(body.len() / 8) {
                let _ = stream.write_all(chunk);
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(150));
            }
        }
        Some(Fault::Unsized) => {
            read_request(&mut stream);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(&artifact());
        }
        Some(Fault::OversizedSidecar) => {
            read_request(&mut stream);
            let len = usize::try_from(Metadata::Sidecar.max_len()).unwrap() + 1;
            serve_body(&mut stream, &vec![b'x'; len]);
        }
        Some(Fault::RedirectOffMachine) => {
            read_request(&mut stream);
            let _ = stream.write_all(
                b"HTTP/1.1 302 Found\r\nLocation: http://updates.example.invalid/app.dmg\r\nContent-Length: 0\r\n\r\n",
            );
        }
        None => {
            read_request(&mut stream);
            serve_body(&mut stream, &artifact());
        }
    }
}

fn temp_dest(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("gpui-updater-test-{}-{tag}", std::process::id()))
}

fn run_download(fault: Fault, fail_first: usize, tag: &str) -> (Result<()>, std::path::PathBuf) {
    let addr = spawn_server(fault, fail_first);
    let url = format!("http://{addr}/update.bin");
    let dest = temp_dest(tag);
    let result = download(&Fetch::new(&url), &dest, |_, _| {});
    (result, dest)
}

fn sized_download(fault: Fault, expected: u64, tag: &str) -> Result<()> {
    let addr = spawn_server(fault, usize::MAX);
    let url = format!("http://{addr}/update.bin");
    let dest = temp_dest(tag);
    let result = download(&Fetch::new(&url).expect_len(expected), &dest, |_, _| {});
    let _ = std::fs::remove_file(&dest);
    result
}

#[test]
fn enforces_the_published_size() {
    let len = artifact().len() as u64;
    // An announced size other than the published one fails at once.
    let started = Instant::now();
    let addr = spawn_server(Fault::TrickleBody, usize::MAX);
    let url = format!("http://{addr}/update.bin");
    let err = download(
        &Fetch::new(&url).expect_len(len - 1),
        &temp_dest("size-cl"),
        |_, _| {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("published"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(2), "must not retry");
    // Without Content-Length, the body is cut off once it grows too large.
    let err = sized_download(Fault::Unsized, 1000, "size-unsized").unwrap_err();
    assert!(
        err.to_string()
            .contains("more than the published 1000 bytes"),
        "{err}"
    );
    // The exact size passes.
    assert!(sized_download(Fault::Unsized, len, "size-exact").is_ok());
}

#[test]
fn enforces_the_download_limit_without_a_published_size() {
    let addr = spawn_server(Fault::Unsized, usize::MAX);
    let url = format!("http://{addr}/update.bin");
    let dest = temp_dest("limit-unsized");
    let err = download(&Fetch::new(&url).max_len(1000), &dest, |_, _| {}).unwrap_err();
    assert!(
        err.to_string().contains("more than the allowed 1000 bytes"),
        "{err}"
    );
    // An announced size over the limit fails before the body is read.
    let addr = spawn_server(Fault::TrickleBody, usize::MAX);
    let url = format!("http://{addr}/update.bin");
    let started = Instant::now();
    let err = download(&Fetch::new(&url).max_len(1000), &dest, |_, _| {}).unwrap_err();
    assert!(err.to_string().contains("1000-byte limit"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "must not wait for the body"
    );
    // A published size over the limit fails without connecting at all.
    let fetch = Fetch::new("http://127.0.0.1:9/update.bin")
        .expect_len(2000)
        .max_len(1000);
    let err = download(&fetch, &dest, |_, _| {}).unwrap_err();
    assert!(err.to_string().contains("2000 bytes published"), "{err}");
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn bounds_metadata_bodies() {
    let addr = spawn_server(Fault::OversizedSidecar, usize::MAX);
    let url = format!("http://{addr}/app.dmg.minisig");
    let started = Instant::now();
    assert!(matches!(
        get_string(&url, &[], Metadata::Sidecar),
        Err(Error::Http(_))
    ));
    assert!(started.elapsed() < Duration::from_secs(2), "must not retry");
    // A document may be larger than a sidecar.
    let body = get_string(&url, &[], Metadata::Document).unwrap();
    assert_eq!(body.len() as u64, Metadata::Sidecar.max_len() + 1);
}

#[test]
fn allows_plain_http_only_to_loopback_hosts() {
    for url in ["https://example.com/app.dmg", "https://127.0.0.1/app.dmg"] {
        assert_eq!(Transport::of(url).unwrap(), Transport::Https, "{url}");
    }
    for url in [
        "http://127.0.0.1:8080/latest.json",
        "http://127.1.2.3/latest.json",
        "http://localhost/latest.json",
        "http://LOCALHOST:1/latest.json",
        "http://[::1]:8080/latest.json",
    ] {
        assert_eq!(
            Transport::of(url).unwrap(),
            Transport::LoopbackHttp,
            "{url}"
        );
    }
    for url in [
        "http://example.com/latest.json",
        "http://10.0.0.5/latest.json",
        "http://localhost.example.com/latest.json",
        "http://127.0.0.1.example.com/latest.json",
        "ftp://example.com/app.dmg",
    ] {
        assert!(
            matches!(Transport::of(url), Err(Error::InsecureUrl(u)) if u == url),
            "{url}"
        );
    }
    // Not even a network URL.
    assert!(Transport::of("file:///etc/passwd").is_err());
}

#[test]
fn refuses_plain_http_before_connecting() {
    // `.invalid` never resolves, so reaching DNS would fail differently.
    let url = "http://updates.example.invalid/latest.json";
    assert!(matches!(
        get_string(url, &[], Metadata::Document),
        Err(Error::InsecureUrl(_))
    ));
    let dest = temp_dest("insecure");
    assert!(matches!(
        download(&Fetch::new(url), &dest, |_, _| {}),
        Err(Error::InsecureUrl(_))
    ));
    assert!(!dest.exists());
}

#[test]
fn reports_redirects_to_plain_http_as_insecure() {
    // ureq checks `https_only` on every redirect hop and reports a downgrade
    // as `RequireHttpsOnly`.
    let err = http_err(ureq::Error::RequireHttpsOnly(
        "http://mirror.example.com/app.dmg".into(),
    ));
    assert!(matches!(err, Error::InsecureUrl(url) if url == "http://mirror.example.com/app.dmg"));
    assert!(!is_transient(&ureq::Error::RequireHttpsOnly(String::new())));
}

#[test]
fn plain_http_to_loopback_follows_no_redirects() {
    let addr = spawn_server(Fault::RedirectOffMachine, usize::MAX);
    let url = format!("http://{addr}/latest.json");
    let err = get_string(&url, &[], Metadata::Document).unwrap_err();
    assert!(err.to_string().contains("302"), "{err}");
    let dest = temp_dest("loopback-redirect");
    let err = download(&Fetch::new(&url), &dest, |_, _| {}).unwrap_err();
    assert!(err.to_string().contains("302"), "{err}");
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn recovers_from_connection_reset_at_connect() {
    let (result, dest) = run_download(Fault::ResetAtConnect, 2, "reset");
    assert!(result.is_ok(), "expected recovery, got {result:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), artifact());
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn recovers_from_503() {
    let (result, dest) = run_download(Fault::Status503, 2, "503");
    assert!(result.is_ok(), "expected recovery, got {result:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), artifact());
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn recovers_from_midstream_truncation() {
    let (result, dest) = run_download(Fault::PartialBody, 2, "partial");
    assert!(result.is_ok(), "expected recovery, got {result:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), artifact());
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn survives_repeated_disconnects_like_a_vpn_reconnect() {
    // Four consecutive drops — a VPN reconnect where the host is lost for
    // several seconds — then recovery. Exceeds the old 3-retry budget.
    let (result, dest) = run_download(Fault::ResetAtConnect, 4, "vpn-reconnect");
    assert!(result.is_ok(), "expected recovery, got {result:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), artifact());
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn gives_up_cleanly_on_permanent_failure() {
    // A permanent 404 must fail fast — not retried into the backoff window.
    let (result, dest) = run_download(Fault::NotFound, usize::MAX, "permanent");
    assert!(result.is_err(), "expected a clean error, got {result:?}");
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn slow_body_is_not_killed_while_progressing() {
    // Regression test for the receive-response deadline leaking into body
    // reads: the body takes ~1.2s against deadlines of 500ms, but each
    // chunk arrives well within the stall cap, so it must complete. With
    // `timeout_recv_response(500ms)` on the download agent this fails with
    // `Timeout(RecvResponse)` half a second after the headers.
    let addr = spawn_server(Fault::TrickleBody, usize::MAX);
    let url = format!("http://{addr}/update.bin");
    let dest = temp_dest("slow-body");
    let tight = Duration::from_millis(500);
    let agent = download_agent(tight);
    let transfer = Transfer {
        agent: &agent,
        pre_body: tight,
    };
    let result = download_with(transfer, &Fetch::new(&url), &dest, |_, _| {});
    assert!(
        result.is_ok(),
        "expected slow download to survive, got {result:?}"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), artifact());
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn black_holed_response_headers_fail_fast_and_transient() {
    let addr = spawn_server(Fault::SilentAfterRequest, usize::MAX);
    let url = format!("http://{addr}/update.bin");
    let dest = temp_dest("black-hole");
    let deadline = Duration::from_millis(300);
    let started = Instant::now();
    let agent = download_agent(deadline);
    let transfer = Transfer {
        agent: &agent,
        pre_body: deadline,
    };
    let result = download_once(
        transfer,
        &Fetch::new(&url),
        &dest,
        &mut Partial::default(),
        &mut |_, _| {},
    );
    let elapsed = started.elapsed();
    match result {
        Err((_, transient)) => assert!(transient, "a black-holed response is transient"),
        Ok(()) => panic!("expected the pre-body deadline to fire"),
    }
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn recovers_from_midstream_stall() {
    // First connection stalls silently mid-body (socket held open); the
    // body stall cap must kill it and the retry must complete the download.
    let addr = spawn_server(Fault::StallMidBody, 1);
    let url = format!("http://{addr}/update.bin");
    let dest = temp_dest("stall");
    let agent = download_agent(Duration::from_millis(300));
    let transfer = Transfer {
        agent: &agent,
        pre_body: PRE_BODY_TIMEOUT,
    };
    let result = download_with(transfer, &Fetch::new(&url), &dest, |_, _| {});
    assert!(result.is_ok(), "expected recovery, got {result:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), artifact());
    let _ = std::fs::remove_file(&dest);
}

#[test]
fn metadata_fetch_retries_transient_failures() {
    let addr = spawn_server(Fault::Status503, 2);
    let url = format!("http://{addr}/SHA256SUMS");
    let body = get_string(&url, &[], Metadata::Sidecar).expect("expected recovery");
    assert_eq!(body.into_bytes(), artifact());
}

#[test]
fn classifies_transient_versus_permanent() {
    use ureq::Error;
    assert!(is_transient(&Error::StatusCode(503)));
    assert!(is_transient(&Error::StatusCode(429)));
    assert!(is_transient(&Error::ConnectionFailed));
    assert!(is_transient(&Error::Io(
        std::io::ErrorKind::ConnectionReset.into()
    )));
    // DNS failures during a VPN reconnect — both shapes ureq can surface.
    assert!(is_transient(&Error::HostNotFound));
    assert!(is_transient(&Error::Io(std::io::Error::other(
        "failed to lookup address information: nodename nor servname provided, or not known",
    ))));
    assert!(!is_transient(&Error::StatusCode(404)));
    assert!(!is_transient(&Error::StatusCode(403)));
}

/// Cancel `token` from another thread after `delay`.
fn cancel_after(token: &CancelToken, delay: Duration) {
    let token = token.clone();
    thread::spawn(move || {
        thread::sleep(delay);
        token.cancel();
    });
}

#[test]
fn cancel_stops_the_body_and_removes_the_partial_file() {
    let addr = spawn_server(Fault::TrickleBody, usize::MAX);
    let url = format!("http://{addr}/update.bin");
    let dest = temp_dest("cancel-body");
    let token = CancelToken::new();
    let started = Instant::now();
    let result = download(&Fetch::new(&url).cancel(&token), &dest, |got, _| {
        if got > 0 {
            token.cancel();
        }
    });
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    assert!(!dest.exists(), "the partial file must be removed");
    // The whole trickled body would take ~1.2s.
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn cancel_interrupts_the_wait_for_response_headers() {
    let addr = spawn_server(Fault::SilentAfterRequest, usize::MAX);
    let url = format!("http://{addr}/update.bin");
    let dest = temp_dest("cancel-headers");
    let token = CancelToken::new();
    cancel_after(&token, Duration::from_millis(200));
    let started = Instant::now();
    // The default pre-body deadline is a minute; the cancel must not wait it out.
    let result = download(&Fetch::new(&url).cancel(&token), &dest, |_, _| {});
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn cancel_interrupts_the_retry_backoff() {
    let addr = spawn_server(Fault::Status503, usize::MAX);
    let url = format!("http://{addr}/update.bin");
    let dest = temp_dest("cancel-backoff");
    let token = CancelToken::new();
    cancel_after(&token, Duration::from_millis(300));
    let started = Instant::now();
    // Five backoffs add up to more than 11 seconds.
    let result = download(&Fetch::new(&url).cancel(&token), &dest, |_, _| {});
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_cancelled_token_stops_before_connecting() {
    let token = CancelToken::new();
    token.cancel();
    let dest = temp_dest("cancel-early");
    // Nothing listens on port 9; a request would fail with a network error.
    let result = download(
        &Fetch::new("http://127.0.0.1:9/update.bin").cancel(&token),
        &dest,
        |_, _| {},
    );
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
}

/// How [`spawn_range_server`] answers a `Range` request.
#[derive(Clone, Copy)]
enum Resume {
    /// With a `206` for the rest of the body.
    Honour,
    /// With a `200` and the whole body again.
    Ignore,
    /// With a `206` that starts one byte later than asked.
    WrongStart,
}

/// Serve the artifact, cutting the first response off halfway. `validator`
/// is a header line that names the body (such as `ETag: "v1"`), or empty.
/// Returns the URL and every request received, lowercased.
fn spawn_range_server(
    validator: &'static str,
    resume: Resume,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/update.bin", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    thread::spawn(move || {
        let validator = if validator.is_empty() {
            String::new()
        } else {
            format!("{validator}\r\n")
        };
        for (n, conn) in listener.incoming().enumerate() {
            let Ok(mut stream) = conn else { continue };
            let mut buf = [0u8; 4096];
            let len = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..len]).to_ascii_lowercase();
            log.lock().unwrap().push(request.clone());
            let body = artifact();
            let offset = request
                .lines()
                .find_map(|line| line.strip_prefix("range: bytes="))
                .and_then(|range| range.trim().trim_end_matches('-').parse::<usize>().ok());
            let partial = |stream: &mut TcpStream, from: usize| {
                let _ = write!(
                    stream,
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {from}-{}/{}\r\nContent-Length: {}\r\n{validator}\r\n",
                    body.len() - 1,
                    body.len(),
                    body.len() - from
                );
                let _ = stream.write_all(&body[from..]);
            };
            let whole = |stream: &mut TcpStream, cut: bool| {
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{validator}\r\n",
                    body.len()
                );
                let end = if cut { body.len() / 2 } else { body.len() };
                let _ = stream.write_all(&body[..end]);
            };
            match (n, offset, resume) {
                (0, _, _) => whole(&mut stream, true),
                (_, Some(from), Resume::Honour) => partial(&mut stream, from),
                (_, Some(from), Resume::WrongStart) => partial(&mut stream, from + 1),
                _ => whole(&mut stream, false),
            }
        }
    });
    (url, requests)
}

/// Download from a [`spawn_range_server`], checking the file and, when the
/// server honours the range, that progress never goes backwards. Returns the requests the server saw.
fn resumed_download(validator: &'static str, resume: Resume, tag: &str) -> Vec<String> {
    let (url, requests) = spawn_range_server(validator, resume);
    let dest = temp_dest(tag);
    let len = artifact().len() as u64;
    let mut seen = Vec::new();
    let result = download(&Fetch::new(&url).expect_len(len), &dest, |got, _| {
        seen.push(got);
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), artifact());
    let _ = std::fs::remove_file(&dest);
    let requests = requests.lock().unwrap().clone();
    // Only a server that honours the range lets the file continue.
    if matches!(resume, Resume::Honour)
        && requests.iter().skip(1).any(|r| r.contains("range: bytes="))
    {
        let first = seen.iter().position(|&got| got > 0).unwrap();
        assert!(
            seen[first..].windows(2).all(|w| w[0] <= w[1]),
            "progress went backwards: {seen:?}"
        );
    }
    requests
}

#[test]
fn resumes_a_dropped_download_with_its_etag() {
    let requests = resumed_download("ETag: \"v1\"", Resume::Honour, "resume-etag");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(!requests[0].contains("range:"), "{}", requests[0]);
    assert!(
        requests[1].contains("range: bytes=102400-"),
        "{}",
        requests[1]
    );
    assert!(requests[1].contains("if-range: \"v1\""), "{}", requests[1]);
}

#[test]
fn resumes_with_last_modified_when_there_is_no_etag() {
    let requests = resumed_download(
        "Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT",
        Resume::Honour,
        "resume-date",
    );
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests[1].contains("if-range: wed, 21 oct 2015 07:28:00 gmt"),
        "{}",
        requests[1]
    );
}

#[test]
fn starts_over_without_a_strong_validator() {
    for (validator, tag) in [("", "resume-none"), ("ETag: W/\"v1\"", "resume-weak")] {
        let requests = resumed_download(validator, Resume::Honour, tag);
        assert_eq!(requests.len(), 2, "{validator}: {requests:?}");
        assert!(
            !requests[1].contains("range:"),
            "{validator}: {}",
            requests[1]
        );
    }
}

#[test]
fn starts_over_when_the_server_ignores_the_range() {
    let requests = resumed_download("ETag: \"v1\"", Resume::Ignore, "resume-ignored");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests[1].contains("range: bytes=102400-"),
        "{}",
        requests[1]
    );
}

#[test]
fn starts_over_after_a_206_for_another_range() {
    let requests = resumed_download("ETag: \"v1\"", Resume::WrongStart, "resume-wrong");
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert!(
        requests[1].contains("range: bytes=102400-"),
        "{}",
        requests[1]
    );
    assert!(!requests[2].contains("range:"), "{}", requests[2]);
}

#[test]
fn parses_content_range() {
    assert_eq!(content_range("bytes 100-199/200"), Some((100, Some(200))));
    assert_eq!(content_range(" bytes 0-0/* "), Some((0, None)));
    for bad in [
        "bytes 100-99/200",
        "bytes 100-200/200",
        "bytes */200",
        "items 0-1/2",
        "bytes 1-2",
        "bytes x-2/3",
    ] {
        assert_eq!(content_range(bad), None, "{bad}");
    }
}
