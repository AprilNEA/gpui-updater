//! Minimal blocking HTTP helpers built on `ureq`.
//!
//! These run synchronously; the GPUI integration drives them from a background
//! executor so the UI thread is never blocked.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use crate::error::{Error, Result};

/// `User-Agent` sent on every request. GitHub rejects API requests without one.
pub(crate) const USER_AGENT: &str = concat!("gpui-updater/", env!("CARGO_PKG_VERSION"));

fn build(
    url: &str,
    headers: &[(&str, &str)],
) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    let mut req = ureq::get(url).header("user-agent", USER_AGENT);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req
}

fn http_err(e: impl std::fmt::Display) -> Error {
    Error::Http(e.to_string())
}

/// `GET` a URL and deserialize the JSON body.
pub(crate) fn get_json<T: serde::de::DeserializeOwned>(
    url: &str,
    headers: &[(&str, &str)],
) -> Result<T> {
    let mut resp = build(url, headers).call().map_err(http_err)?;
    if !resp.status().is_success() {
        return Err(Error::Http(format!("GET {url} -> {}", resp.status())));
    }
    resp.body_mut()
        .read_json::<T>()
        .map_err(|e| Error::Parse(e.to_string()))
}

/// `GET` a URL and return the body as text.
pub(crate) fn get_string(url: &str, headers: &[(&str, &str)]) -> Result<String> {
    let mut resp = build(url, headers).call().map_err(http_err)?;
    if !resp.status().is_success() {
        return Err(Error::Http(format!("GET {url} -> {}", resp.status())));
    }
    resp.body_mut().read_to_string().map_err(http_err)
}

/// Stream a URL to `dest`, invoking `progress(downloaded, total)` as bytes
/// arrive. `total` is `None` when the server omits `Content-Length`.
///
/// Redirects (e.g. a GitHub release asset to its storage backend) are followed
/// by `ureq` automatically.
pub(crate) fn download(
    url: &str,
    headers: &[(&str, &str)],
    dest: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let mut resp = build(url, headers)
        .header("accept", "application/octet-stream")
        .call()
        .map_err(http_err)?;
    if !resp.status().is_success() {
        return Err(Error::Http(format!("GET {url} -> {}", resp.status())));
    }

    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let mut file = File::create(dest)?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut downloaded = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        downloaded += n as u64;
        progress(downloaded, total);
    }
    file.flush()?;
    Ok(())
}
