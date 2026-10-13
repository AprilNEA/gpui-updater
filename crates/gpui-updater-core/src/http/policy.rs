//! What the HTTP layer may fetch: which URLs, and how much of a response.
//!
//! Every request, and every redirect it follows, must use HTTPS: release
//! metadata and artifacts fetched over plain HTTP could be rewritten by anyone
//! on the network path. The only exception is a loopback host (`localhost`,
//! `127.0.0.0/8`, `::1`), which never leaves the machine, so local test
//! servers keep working; such a request follows no redirects, since one could
//! lead off the machine still over plain HTTP.

use std::net::IpAddr;

use ureq::http::Uri;
use ureq::http::uri::Scheme;

use crate::error::{Error, Result};

/// What a metadata response holds, which bounds how much of it is read into
/// memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Metadata {
    /// A release document: a manifest, an appcast, or a GitHub API listing.
    Document,
    /// A signature or checksums file published next to an artifact.
    Sidecar,
}

impl Metadata {
    /// The largest body accepted, in bytes.
    pub(crate) fn max_len(self) -> u64 {
        match self {
            // Large enough for an appcast with inline release notes or a page
            // of GitHub releases.
            Self::Document => 10 * 1024 * 1024,
            // A minisign signature is a few hundred bytes and a checksums file
            // one line per asset.
            Self::Sidecar => 1024 * 1024,
        }
    }
}

/// How a URL may be fetched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Transport {
    /// HTTPS, and every redirect must stay on HTTPS.
    Https,
    /// Plain HTTP to a loopback host, without following redirects.
    LoopbackHttp,
}

impl Transport {
    /// Classify `url`, rejecting plain HTTP to anything but a loopback host
    /// and every other scheme.
    pub(super) fn of(url: &str) -> Result<Self> {
        let uri: Uri = url
            .parse()
            .map_err(|e| Error::Http(format!("invalid URL {url}: {e}")))?;
        if uri.scheme() == Some(&Scheme::HTTPS) {
            Ok(Self::Https)
        } else if uri.scheme() == Some(&Scheme::HTTP) && uri.host().is_some_and(is_loopback) {
            Ok(Self::LoopbackHttp)
        } else {
            Err(Error::InsecureUrl(url.to_string()))
        }
    }
}

fn is_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Fail with [`Error::InsecureUrl`] unless `url` would be fetched over HTTPS
/// (or plain HTTP to a loopback host).
pub(crate) fn ensure_secure(url: &str) -> Result<()> {
    Transport::of(url).map(drop)
}
