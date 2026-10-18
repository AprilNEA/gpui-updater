//! Minimal blocking HTTP helpers built on `ureq`.
//!
//! These run synchronously; the GPUI integration drives them from a background
//! executor so the UI thread is never blocked. Every phase of a request is
//! bounded so nothing hangs forever — DNS resolution, connect, and the wait for
//! response headers each have a deadline, while a download body has only a
//! stall cap, so a large but still-progressing artifact is never killed — and
//! transient transport failures are retried with backoff, so a momentary
//! network blip, a VPN reconnect, or a dropped keep-alive doesn't abort an
//! update with a hard error.
//!
//! Only HTTPS is spoken (see [`policy`]), and response sizes are bounded, so
//! a hostile server cannot exhaust memory or disk.

mod policy;

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig};

use policy::Transport;
pub(crate) use policy::{Metadata, ensure_secure};

use crate::cancel::{self, CancelToken};
use crate::error::{Error, Result};

/// `User-Agent` sent on every request. GitHub rejects API requests without one.
pub(crate) const USER_AGENT: &str = concat!("gpui-updater/", env!("CARGO_PKG_VERSION"));

/// Bound applied separately to DNS resolution (`timeout_resolve`) and to the
/// TCP + TLS connect (`timeout_connect`) — in `ureq` these are distinct phases
/// and `timeout_connect` alone leaves DNS unbounded.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on a whole metadata response. `ureq`'s receive-response deadline keeps
/// counting through body reads (`RecvBody` also checks the `RecvResponse`
/// deadline), so this caps headers and body together — fine for small JSON and
/// checksum bodies, wrong for artifacts, which is why downloads use
/// [`download_agent`] instead.
const RECV_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on a single body-read wait during a download. `ureq` re-arms the
/// current phase's deadline on every read, so this kills a stalled stream but
/// never one that keeps delivering bytes.
const BODY_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on everything before a download's first body byte — DNS, connect,
/// request send, and response headers — enforced by [`call_with_deadline`]
/// because setting `timeout_recv_response` would cap the whole body too.
const PRE_BODY_TIMEOUT: Duration = Duration::from_secs(60);

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

/// TLS setup shared by both agents: validate server certificates against the
/// operating system's trust store (`rustls-platform-verifier`) instead of
/// ureq's default bundled Mozilla roots. Enterprise TLS-inspection proxies
/// (Zscaler et al.) re-sign every connection with a private CA that exists
/// only in the OS store, so the bundled roots rejected every check behind one
/// with "invalid peer certificate: `UnknownIssuer`".
fn tls_config() -> TlsConfig {
    TlsConfig::builder()
        .root_certs(RootCerts::PlatformVerifier)
        .build()
}

/// Agent for small metadata requests: every phase through the (small) body is
/// bounded, including DNS. Cheap to build; one per top-level call.
fn metadata_agent() -> Agent {
    Agent::config_builder()
        .user_agent(USER_AGENT)
        .tls_config(tls_config())
        .https_only(true)
        .timeout_resolve(Some(CONNECT_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RECV_RESPONSE_TIMEOUT))
        .build()
        .into()
}

/// Agent for artifact downloads: bounded DNS and connect, a stall cap of
/// `stall` on body reads, and deliberately no receive-response deadline — see
/// [`RECV_RESPONSE_TIMEOUT`] for why that would cap the whole body.
fn download_agent(stall: Duration) -> Agent {
    Agent::config_builder()
        .user_agent(USER_AGENT)
        .tls_config(tls_config())
        .https_only(true)
        .timeout_resolve(Some(CONNECT_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_body(Some(stall))
        .build()
        .into()
}

/// A GET for `url` that may only follow redirects `transport` allows.
fn build(
    agent: &Agent,
    url: &str,
    headers: &[(&str, &str)],
    transport: Transport,
) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    let config = agent.get(url).config();
    let mut req = match transport {
        Transport::Https => config.https_only(true),
        // A redirect could lead off the machine, still over plain HTTP.
        Transport::LoopbackHttp => config.https_only(false).max_redirects(0),
    }
    .build();
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req
}

fn http_err(e: ureq::Error) -> Error {
    match e {
        // A redirect to plain HTTP.
        ureq::Error::RequireHttpsOnly(url) => Error::InsecureUrl(url),
        other => Error::Http(other.to_string()),
    }
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
                thread::sleep(backoff(tries));
                tries += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// One GET that fails with a typed [`ureq::Error`] (a non-success status maps to
/// [`ureq::Error::StatusCode`]) so [`retrying`] can classify it.
fn get_bytes(agent: &Agent, request: &Request<'_>) -> std::result::Result<Vec<u8>, ureq::Error> {
    let mut resp = build(agent, request.url, request.headers, request.transport).call()?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ureq::Error::StatusCode(status.as_u16()));
    }
    resp.body_mut()
        .with_config()
        .limit(request.kind.max_len())
        .read_to_vec()
}

/// One metadata request.
struct Request<'a> {
    url: &'a str,
    headers: &'a [(&'a str, &'a str)],
    transport: Transport,
    kind: Metadata,
}

/// `GET` a metadata URL, retrying transient failures.
fn get_metadata(url: &str, headers: &[(&str, &str)], kind: Metadata) -> Result<Vec<u8>> {
    let request = Request {
        url,
        headers,
        transport: Transport::of(url)?,
        kind,
    };
    let agent = metadata_agent();
    retrying(|| get_bytes(&agent, &request)).map_err(|e| finalize(url, e))
}

/// `GET` a URL and deserialize the JSON body, retrying transient failures.
pub(crate) fn get_json<T: serde::de::DeserializeOwned>(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<T> {
    let body = get_metadata(url, headers, Metadata::Document)?;
    serde_json::from_slice(&body).map_err(|e| Error::Parse(e.to_string()))
}

/// `GET` a URL and return the body as text, retrying transient failures.
/// A body larger than `kind` allows fails.
pub(crate) fn get_string(url: &str, headers: &[(&str, &str)], kind: Metadata) -> Result<String> {
    let bytes = get_metadata(url, headers, kind)?;
    String::from_utf8(bytes).map_err(|e| Error::Parse(e.to_string()))
}

/// One file to download.
#[derive(Clone, Copy)]
pub(crate) struct Fetch<'a> {
    pub(crate) url: &'a str,
    pub(crate) headers: &'a [(&'a str, &'a str)],
    /// The exact size the body must have, when known. A response announcing
    /// another size is rejected, and a body growing past it is cut off, so a
    /// hostile server cannot fill the disk.
    pub(crate) expected_len: Option<u64>,
    /// The most bytes accepted, whether or not a size was published.
    pub(crate) max_len: u64,
    /// Stops the download when cancelled.
    pub(crate) cancel: Option<&'a CancelToken>,
}

impl<'a> Fetch<'a> {
    pub(crate) fn new(url: &'a str) -> Self {
        Self {
            url,
            headers: &[],
            expected_len: None,
            max_len: u64::MAX,
            cancel: None,
        }
    }

    /// Stop the download, with [`Error::Cancelled`], once `cancel` is
    /// cancelled.
    pub(crate) fn cancel(self, cancel: &'a CancelToken) -> Self {
        Self {
            cancel: Some(cancel),
            ..self
        }
    }

    fn check_cancelled(&self) -> Result<()> {
        self.cancel.map_or(Ok(()), CancelToken::check)
    }

    /// Wait `duration` before a retry, or less if cancelled.
    fn pause(&self, duration: Duration) -> Result<()> {
        if let Some(cancel) = self.cancel {
            cancel.sleep(duration)
        } else {
            thread::sleep(duration);
            Ok(())
        }
    }

    /// Require a body of exactly `len` bytes; `0` means unknown.
    pub(crate) fn expect_len(self, len: u64) -> Self {
        Self {
            expected_len: (len > 0).then_some(len),
            ..self
        }
    }

    /// Refuse bodies larger than `len` bytes, even when no size is published.
    pub(crate) fn max_len(self, len: u64) -> Self {
        Self {
            max_len: len,
            ..self
        }
    }

    /// The most bytes the body may have: the published size when known.
    fn limit(&self) -> u64 {
        self.expected_len.unwrap_or(self.max_len)
    }
}

/// The agent and pre-body deadline a download runs with.
#[derive(Clone, Copy)]
struct Transfer<'a> {
    agent: &'a Agent,
    pre_body: Duration,
}

/// Stream a URL to `dest`, invoking `progress(downloaded, total)` as bytes
/// arrive. `total` is `None` when neither the server nor the caller knows the
/// size.
///
/// Redirects (e.g. a GitHub release asset to its storage backend) are followed
/// by `ureq` automatically, but never to plain HTTP. After a transient
/// transport failure — at connect or mid-stream — the download is retried.
/// When the response named its body with a strong `ETag` or a
/// `Last-Modified` date, the retry asks for the rest only (`Range` with
/// `If-Range`) and continues the file if the server answers `206` for exactly
/// that range. Otherwise it starts over from byte zero: the file is truncated
/// and `progress` resets. Up to [`MAX_RETRIES`] attempts in a row may fail
/// without moving the download forward. A cancelled download fails with
/// [`Error::Cancelled`] and removes the partial file.
pub(crate) fn download(
    fetch: &Fetch<'_>,
    dest: &Path,
    progress: impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let agent = download_agent(BODY_STALL_TIMEOUT);
    let transfer = Transfer {
        agent: &agent,
        pre_body: PRE_BODY_TIMEOUT,
    };
    download_with(transfer, fetch, dest, progress)
}

/// Retry loop around [`download_once`], parameterized so tests can run with
/// tight deadlines.
fn download_with(
    transfer: Transfer<'_>,
    fetch: &Fetch<'_>,
    dest: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<()> {
    if let Some(expected) = fetch.expected_len.filter(|&len| len > fetch.max_len) {
        return Err(Error::Http(format!(
            "GET {} -> {expected} bytes published, more than the {}-byte limit",
            fetch.url, fetch.max_len
        )));
    }
    let result = retry_download(transfer, fetch, dest, &mut progress);
    if matches!(result, Err(Error::Cancelled)) {
        // Nothing resumes a cancelled download, so its partial body is waste.
        let _ = std::fs::remove_file(dest);
    }
    result
}

/// What the attempts so far have left in the file, so the next attempt can
/// ask for the rest only.
#[derive(Debug, Default)]
struct Partial {
    /// Bytes of the body already in the file.
    len: u64,
    /// The strong `ETag`, or else the `Last-Modified` date, of the response
    /// those bytes came from. A `Range` request is only safe with an
    /// `If-Range` that names the body, so without one the next attempt
    /// starts over.
    validator: Option<String>,
    /// The size of the whole body, when known.
    total: Option<u64>,
}

impl Partial {
    /// The offset and validator for a `Range` request that continues this
    /// download, if there is one.
    fn resume_point(&self) -> Option<(u64, &str)> {
        let validator = self.validator.as_deref()?;
        (self.len > 0).then_some((self.len, validator))
    }
}

fn retry_download(
    transfer: Transfer<'_>,
    fetch: &Fetch<'_>,
    dest: &Path,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let mut partial = Partial::default();
    let mut tries: u32 = 0;
    loop {
        fetch.check_cancelled()?;
        let before = partial.len;
        match download_once(transfer, fetch, dest, &mut partial, progress) {
            Ok(()) => return Ok(()),
            Err((Error::Cancelled, _)) => return Err(Error::Cancelled),
            Err((_, transient)) if transient && tries < MAX_RETRIES => {
                if partial.resume_point().is_some() {
                    // An attempt that moved the download forward does not use
                    // up a retry.
                    if partial.len > before {
                        tries = 0;
                    }
                    progress(partial.len, partial.total);
                } else {
                    partial = Partial::default();
                    progress(0, None);
                }
                fetch.pause(backoff(tries))?;
                tries += 1;
            }
            Err((e, _)) => return Err(e),
        }
    }
}

/// Run a download's GET up to the response headers on a helper thread with a
/// hard deadline — the only way to bound a black-holed pre-body phase without
/// `timeout_recv_response` leaking into the body. A deadline miss is transient:
/// the canonical cause is a VPN drop black-holing an established connection.
fn call_with_deadline(
    transfer: Transfer<'_>,
    fetch: &Fetch<'_>,
    transport: Transport,
    resume: Option<(u64, &str)>,
) -> std::result::Result<ureq::http::Response<ureq::Body>, (Error, bool)> {
    let (url, deadline) = (fetch.url, transfer.pre_body);
    let range =
        resume.map(|(offset, validator)| (format!("bytes={offset}-"), validator.to_string()));
    let (tx, rx) = mpsc::channel();
    let agent = transfer.agent.clone();
    let owned_url = url.to_string();
    let owned_headers: Vec<(String, String)> = fetch
        .headers
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    // On a deadline miss this thread stays blocked until the OS gives up on the
    // socket, then exits; its send into the dropped channel is ignored.
    thread::spawn(move || {
        let mut req =
            build(&agent, &owned_url, &[], transport).header("accept", "application/octet-stream");
        if let Some((range, validator)) = &range {
            req = req
                .header("range", range.as_str())
                .header("if-range", validator.as_str());
        }
        for (k, v) in &owned_headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let _ = tx.send(req.call());
    });
    // Wait in short slices, so a cancel does not wait out the deadline.
    let started = Instant::now();
    let received = loop {
        let left = deadline.saturating_sub(started.elapsed());
        match rx.recv_timeout(left.min(cancel::POLL)) {
            Ok(received) => break Some(received),
            Err(RecvTimeoutError::Disconnected) => break None,
            Err(RecvTimeoutError::Timeout) => {
                if fetch.check_cancelled().is_err() {
                    return Err((Error::Cancelled, false));
                }
                if left.is_zero() {
                    break None;
                }
            }
        }
    };
    match received {
        Some(Ok(resp)) => Ok(resp),
        Some(Err(e)) => {
            let transient = is_transient(&e);
            Err((http_err(e), transient))
        }
        None => Err((
            Error::Http(format!("GET {url} -> no response within {deadline:?}")),
            true,
        )),
    }
}

/// One download attempt. The bool in the error is whether it is worth retrying:
/// a connect/status/mid-stream transport failure is, a local write failure
/// (disk full, permissions) or a size mismatch is not. `partial` records how
/// far the file got, for the next attempt.
fn download_once(
    transfer: Transfer<'_>,
    fetch: &Fetch<'_>,
    dest: &Path,
    partial: &mut Partial,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> std::result::Result<(), (Error, bool)> {
    let url = fetch.url;
    let transport = Transport::of(url).map_err(|e| (e, false))?;
    let mut resp = call_with_deadline(transfer, fetch, transport, partial.resume_point())?;
    let start = body_start(&resp, url, partial)?;

    // For a `206`, `Content-Length` counts the rest of the body only.
    let announced = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|rest| start + rest);
    if let (Some(expected), Some(announced)) = (fetch.expected_len, announced) {
        if announced != expected {
            return Err((
                Error::Http(format!(
                    "GET {url} -> {announced} bytes, but {expected} were published"
                )),
                false,
            ));
        }
    }
    if let Some(announced) = announced.filter(|&len| len > fetch.max_len) {
        return Err((
            Error::Http(format!(
                "GET {url} -> {announced} bytes, more than the {}-byte limit",
                fetch.max_len
            )),
            false,
        ));
    }
    let total = announced.or(fetch.expected_len);
    let limit = fetch.limit();
    let bound = if fetch.expected_len.is_some() {
        "published"
    } else {
        "allowed"
    };

    match validator_of(resp.headers()) {
        Some(validator) => partial.validator = Some(validator),
        // A `206` need not repeat the validator it was asked with.
        None if start == 0 => partial.validator = None,
        None => {}
    }
    partial.len = start;
    partial.total = total;

    let mut file = open_at(dest, start).map_err(|e| (Error::Io(e), false))?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut downloaded = start;
    loop {
        if fetch.check_cancelled().is_err() {
            return Err((Error::Cancelled, false));
        }
        let n = reader.read(&mut buf).map_err(|e| (Error::Io(e), true))?;
        if n == 0 {
            break;
        }
        downloaded += n as u64;
        if downloaded > limit {
            return Err((
                Error::Http(format!("GET {url} -> more than the {bound} {limit} bytes")),
                false,
            ));
        }
        file.write_all(&buf[..n])
            .map_err(|e| (Error::Io(e), false))?;
        partial.len = downloaded;
        progress(downloaded, total);
    }
    if downloaded < limit && fetch.expected_len.is_some() {
        return Err((
            Error::Http(format!(
                "GET {url} -> body ended after {downloaded} of {limit} bytes"
            )),
            true,
        ));
    }
    file.flush().map_err(|e| (Error::Io(e), false))?;
    Ok(())
}

/// Where the body of `resp` starts in the file: `partial.len` for a `206`
/// that continues the download, `0` for any other success. Fails for an
/// error status, and for a `206` that does not continue the file, after
/// which the next attempt starts over.
fn body_start(
    resp: &ureq::http::Response<ureq::Body>,
    url: &str,
    partial: &mut Partial,
) -> std::result::Result<u64, (Error, bool)> {
    let code = resp.status().as_u16();
    let asked = partial.resume_point().map(|(offset, _)| offset);
    match (code, asked) {
        (206, Some(offset)) => {
            let range = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(content_range);
            match range {
                Some((first, size))
                    if first == offset
                        && size.is_none_or(|size| partial.total.is_none_or(|t| t == size)) =>
                {
                    Ok(offset)
                }
                _ => {
                    *partial = Partial::default();
                    Err((
                        Error::Http(format!("GET {url} -> 206 for another range")),
                        true,
                    ))
                }
            }
        }
        (206, None) => Err((
            Error::Http(format!("GET {url} -> 206 without a range request")),
            false,
        )),
        // The file changed size or the server forgot it: start over.
        (416, Some(_)) => {
            *partial = Partial::default();
            Err((Error::Http(format!("GET {url} -> 416")), true))
        }
        (200..=299, _) => Ok(0),
        (code, _) => {
            let transient = code >= 500 || matches!(code, 408 | 429);
            Err((Error::Http(format!("GET {url} -> {code}")), transient))
        }
    }
}

/// The first byte and the size of the whole body in a
/// `Content-Range: bytes <first>-<last>/<size>` value, where `<size>` may be
/// `*` (unknown).
fn content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (span, size) = rest.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    let size = match size.trim() {
        "*" => None,
        size => Some(size.parse::<u64>().ok()?),
    };
    if last < first || size.is_some_and(|size| last >= size) {
        return None;
    }
    Some((first, size))
}

/// The value that names this exact body for `If-Range`: a strong `ETag`, or
/// else the `Last-Modified` date. A weak `ETag` (`W/"…"`) may not be used
/// with `If-Range`.
fn validator_of(headers: &ureq::http::HeaderMap) -> Option<String> {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    get("etag")
        .filter(|tag| !tag.starts_with("W/"))
        .or_else(|| get("last-modified"))
        .map(str::to_string)
}

/// Open `dest` for writing at byte `start`. At `0` the file is truncated;
/// otherwise it is cut to `start`, dropping any bytes past it.
fn open_at(dest: &Path, start: u64) -> io::Result<File> {
    if start == 0 {
        return File::create(dest);
    }
    let mut file = OpenOptions::new().write(true).open(dest)?;
    file.set_len(start)?;
    file.seek(SeekFrom::Start(start))?;
    Ok(file)
}

#[cfg(test)]
mod tests;
