//! [`HttpOptions`]: request headers and the proxy for update traffic.

use std::fmt;

use crate::error::{Error, Result};

/// HTTP settings for an update source or for the engine's downloads: extra
/// request headers and the proxy.
///
/// By default no header is added and the proxy comes from the environment
/// (`ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`, and their lowercase forms),
/// as before. Pass the options to the source
/// (for example [`StaticManifestSource::http`](crate::StaticManifestSource::http))
/// for metadata requests, and to [`EngineConfig::http`](crate::EngineConfig::http)
/// for artifacts, deltas, and their checksum and signature files.
///
/// ```
/// use gpui_updater_core::HttpOptions;
///
/// let http = HttpOptions::new()
///     .header("authorization", "Bearer s3cr3t")
///     .proxy("http://proxy.internal:3128");
/// ```
#[derive(Clone, Default)]
pub struct HttpOptions {
    headers: Vec<(String, String)>,
    proxy: ProxyChoice,
}

#[derive(Clone, Default)]
enum ProxyChoice {
    #[default]
    FromEnv,
    Off,
    Url(String),
}

/// Options with nothing set, for callers that have none.
pub(crate) static DEFAULT: HttpOptions = HttpOptions::new();

impl HttpOptions {
    /// No extra headers, and the proxy from the environment.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            headers: Vec::new(),
            proxy: ProxyChoice::FromEnv,
        }
    }

    /// Send `name: value` with every request.
    ///
    /// Put a credential in `authorization`: it is dropped when a request is
    /// redirected, so a release asset's storage host never sees it. Every
    /// other header goes to redirect targets as well. A header the source
    /// sets itself, such as the `authorization` from
    /// [`GitHubSource::token`](crate::GitHubSource::token), takes precedence
    /// over one set here. An invalid name or value fails the request.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Send requests through the proxy at `url` instead of the one from the
    /// environment. The scheme is `http`, `https`, `socks4`, `socks4a`, or
    /// `socks5`; credentials go in the URL (`http://user:pass@host:port`).
    /// HTTPS requests are tunnelled, so TLS still ends at the update server.
    /// An invalid URL fails every request.
    #[must_use]
    pub fn proxy(mut self, url: impl Into<String>) -> Self {
        self.proxy = ProxyChoice::Url(url.into());
        self
    }

    /// Connect directly, ignoring the proxy environment variables.
    #[must_use]
    pub fn no_proxy(mut self) -> Self {
        self.proxy = ProxyChoice::Off;
        self
    }

    /// The proxy for an agent.
    pub(crate) fn resolve_proxy(&self) -> Result<Option<ureq::Proxy>> {
        match &self.proxy {
            ProxyChoice::FromEnv => Ok(ureq::Proxy::try_from_env()),
            ProxyChoice::Off => Ok(None),
            // The URL may hold credentials, so the error does not repeat it.
            ProxyChoice::Url(url) => ureq::Proxy::new(url)
                .map(Some)
                .map_err(|e| Error::Http(format!("invalid proxy URL: {e}"))),
        }
    }

    /// `own` followed by the headers set here that `own` does not already
    /// name.
    pub(crate) fn merge(&self, own: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut merged: Vec<(String, String)> = own
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        for (name, value) in &self.headers {
            if !own.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
                merged.push((name.clone(), value.clone()));
            }
        }
        merged
    }
}

/// Shows header names and whether a proxy is set, never header values or the
/// proxy URL, which may hold credentials.
impl fmt::Debug for HttpOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(k, _)| k.as_str()).collect();
        let proxy = match self.proxy {
            ProxyChoice::FromEnv => "from environment",
            ProxyChoice::Off => "none",
            ProxyChoice::Url(_) => "configured",
        };
        f.debug_struct("HttpOptions")
            .field("headers", &names)
            .field("proxy", &proxy)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_headers_win_over_configured_ones() {
        let http = HttpOptions::new()
            .header("Authorization", "Bearer mine")
            .header("x-team", "desktop");
        let merged = http.merge(&[("authorization", "Bearer source")]);
        assert_eq!(
            merged,
            [
                ("authorization".to_string(), "Bearer source".to_string()),
                ("x-team".to_string(), "desktop".to_string()),
            ]
        );
    }

    #[test]
    fn debug_hides_header_values_and_the_proxy_url() {
        let http = HttpOptions::new()
            .header("authorization", "Bearer s3cr3t")
            .proxy("http://user:hunter2@proxy:3128");
        let shown = format!("{http:?}");
        assert!(shown.contains("authorization"), "{shown}");
        assert!(
            !shown.contains("s3cr3t") && !shown.contains("hunter2"),
            "{shown}"
        );
    }

    #[test]
    fn rejects_an_invalid_proxy_url_without_repeating_it() {
        let err = HttpOptions::new()
            .proxy("ftp://user:hunter2@proxy")
            .resolve_proxy()
            .unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
        assert!(
            HttpOptions::new()
                .no_proxy()
                .resolve_proxy()
                .unwrap()
                .is_none()
        );
    }
}
