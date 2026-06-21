//! Minimal blocking HTTP helpers built on `ureq`.
//!
//! These run synchronously; the GPUI integration drives them from a background
//! executor so the UI thread is never blocked. Every request goes through a
//! configured [`Agent`] with connect and response-header timeouts (but no
//! body-read cap, so a large but still-progressing download is never killed),
//! and transient transport failures are retried with backoff — so a momentary
//! network blip, a VPN reconnect, or a dropped keep-alive doesn't abort an
//! update with a hard error.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use ureq::Agent;

use crate::error::{Error, Result};

/// `User-Agent` sent on every request. GitHub rejects API requests without one.
pub(crate) const USER_AGENT: &str = concat!("gpui-updater/", env!("CARGO_PKG_VERSION"));

/// Bound on DNS + TCP + TLS connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on receiving the response status line and headers. Caps a black-holed
/// connection (accepted but silent) so a download can't hang the UI forever,
/// without limiting how long a large, still-progressing artifact may take.
const RECV_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Retries after the initial attempt for a single request. With the backoff
/// below this gives a total retry window of ~11s — long enough to ride out a
/// VPN reconnect, where DNS resolution and connections drop for several seconds
/// while the tunnel re-establishes, without hanging a genuinely offline client.
const MAX_RETRIES: u32 = 5;

/// First backoff delay; doubles each retry up to [`RETRY_MAX_DELAY`].
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

/// Cap on any single backoff delay.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(4);

/// Exponential backoff for the `attempt`-th retry (0-based): 0.5s, 1s, 2s, 4s,
/// 4s — a ~11.5s window across [`MAX_RETRIES`] retries.
fn backoff(attempt: u32) -> Duration {
    RETRY_BASE_DELAY
        .saturating_mul(2u32.saturating_pow(attempt))
        .min(RETRY_MAX_DELAY)
}

/// A configured agent: shared User-Agent plus connect / response-header
/// timeouts. Cheap to build; one per top-level call.
fn agent() -> Agent {
    Agent::config_builder()
        .user_agent(USER_AGENT)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RECV_RESPONSE_TIMEOUT))
        .build()
        .into()
}

fn build(
    agent: &Agent,
    url: &str,
    headers: &[(&str, &str)],
) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req
}

fn http_err(e: impl std::fmt::Display) -> Error {
    Error::Http(e.to_string())
}

/// Wrap the final (non-retried) error, keeping the URL for a non-success status.
fn finalize(url: &str, e: ureq::Error) -> Error {
    match e {
        ureq::Error::StatusCode(code) => Error::Http(format!("GET {url} -> {code}")),
        other => http_err(other),
    }
}

/// Whether a failed request is worth retrying: transport-level hiccups are
/// transient and recover once the network settles. This explicitly covers a
/// failed DNS lookup (`HostNotFound`, or an `Io` "failed to lookup address
/// information" from `getaddrinfo`) — the host "gets lost" for a few seconds
/// while a VPN reconnects — alongside dropped/refused connections, timeouts, an
/// invalidated socket, and 5xx (plus the two "back off and retry" 4xx codes). A
/// 4xx like 404 or a malformed request won't change on a retry.
fn is_transient(error: &ureq::Error) -> bool {
    use ureq::Error;
    match error {
        Error::StatusCode(code) => *code >= 500 || matches!(*code, 408 | 429),
        Error::Io(_)
        | Error::Timeout(_)
        | Error::ConnectionFailed
        | Error::HostNotFound
        | Error::Protocol(_) => true,
        _ => false,
    }
}

/// Run `attempt` up to `MAX_RETRIES` extra times, backing off on transient
/// failures and returning the last error otherwise.
fn retrying<T>(
    mut attempt: impl FnMut() -> std::result::Result<T, ureq::Error>,
) -> std::result::Result<T, ureq::Error> {
    let mut tries: u32 = 0;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(e) if tries < MAX_RETRIES && is_transient(&e) => {
                sleep(backoff(tries));
                tries += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// One GET that fails with a typed [`ureq::Error`] (a non-success status maps to
/// [`ureq::Error::StatusCode`]) so [`retrying`] can classify it.
fn get_bytes(
    agent: &Agent,
    url: &str,
    headers: &[(&str, &str)],
) -> std::result::Result<Vec<u8>, ureq::Error> {
    let mut resp = build(agent, url, headers).call()?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ureq::Error::StatusCode(status.as_u16()));
    }
    resp.body_mut().read_to_vec()
}

/// `GET` a URL and deserialize the JSON body, retrying transient failures.
pub(crate) fn get_json<T: serde::de::DeserializeOwned>(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<T> {
    let agent = agent();
    let body = retrying(|| get_bytes(&agent, url, headers)).map_err(|e| finalize(url, e))?;
    serde_json::from_slice(&body).map_err(|e| Error::Parse(e.to_string()))
}

/// `GET` a URL and return the body as text, retrying transient failures.
pub(crate) fn get_string(url: &str, headers: &[(&str, &str)]) -> Result<String> {
    let agent = agent();
    let bytes = retrying(|| get_bytes(&agent, url, headers)).map_err(|e| finalize(url, e))?;
    String::from_utf8(bytes).map_err(|e| Error::Parse(e.to_string()))
}

/// Stream a URL to `dest`, invoking `progress(downloaded, total)` as bytes
/// arrive. `total` is `None` when the server omits `Content-Length`.
///
/// Redirects (e.g. a GitHub release asset to its storage backend) are followed
/// by `ureq` automatically. A transient transport failure — at connect or
/// mid-stream — restarts the download from byte zero (the partial file is
/// truncated and `progress` resets), up to [`MAX_RETRIES`] times.
pub(crate) fn download(
    url: &str,
    headers: &[(&str, &str)],
    dest: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let agent = agent();
    let mut tries: u32 = 0;
    loop {
        match download_once(&agent, url, headers, dest, &mut progress) {
            Ok(()) => return Ok(()),
            Err((_, transient)) if transient && tries < MAX_RETRIES => {
                sleep(backoff(tries));
                tries += 1;
                progress(0, None);
            }
            Err((e, _)) => return Err(e),
        }
    }
}

/// One download attempt. The bool in the error is whether it is worth retrying:
/// a connect/status/mid-stream transport failure is, a local write failure
/// (disk full, permissions) is not.
fn download_once(
    agent: &Agent,
    url: &str,
    headers: &[(&str, &str)],
    dest: &Path,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> std::result::Result<(), (Error, bool)> {
    let mut resp = build(agent, url, headers)
        .header("accept", "application/octet-stream")
        .call()
        .map_err(|e| {
            let transient = is_transient(&e);
            (http_err(e), transient)
        })?;
    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let transient = code >= 500 || matches!(code, 408 | 429);
        return Err((Error::Http(format!("GET {url} -> {code}")), transient));
    }

    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let mut file = File::create(dest).map_err(|e| (Error::Io(e), false))?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut downloaded = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| (Error::Io(e), true))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| (Error::Io(e), false))?;
        downloaded += n as u64;
        progress(downloaded, total);
    }
    file.flush().map_err(|e| (Error::Io(e), false))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

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

    fn handle(mut stream: TcpStream, fault: Option<Fault>) {
        match fault {
            Some(Fault::ResetAtConnect) => drop(stream),
            Some(Fault::Status503) => {
                read_request(&mut stream);
                let _ = stream
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n");
            }
            Some(Fault::PartialBody) => {
                read_request(&mut stream);
                let body = artifact();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body[..body.len() / 2]);
            }
            Some(Fault::NotFound) => {
                read_request(&mut stream);
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
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

    fn run_download(
        fault: Fault,
        fail_first: usize,
        tag: &str,
    ) -> (Result<()>, std::path::PathBuf) {
        let addr = spawn_server(fault, fail_first);
        let url = format!("http://{addr}/update.bin");
        let dest = temp_dest(tag);
        let result = download(&url, &[], &dest, |_, _| {});
        (result, dest)
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
}
