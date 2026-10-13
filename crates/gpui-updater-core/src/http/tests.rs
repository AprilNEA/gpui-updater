#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use super::*;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    let result = download_once(transfer, &Fetch::new(&url), &dest, &mut |_, _| {});
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
    let body = get_string(&url, &[]).expect("expected recovery");
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
