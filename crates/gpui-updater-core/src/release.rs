//! Release metadata shared across update sources.

use std::time::SystemTime;

use semver::Version;

use crate::error::{Error, Result};

/// Format identifier of the deltas produced by the `delta` feature's
/// `delta::create`: per-file bsdiff patches against the installed tree.
pub const DELTA_FORMAT_FILES_BSDIFF_V1: &str = "files-bsdiff-v1";

/// A downloadable file attached to a release.
///
/// Marked `#[non_exhaustive]`: build one with [`Asset::new`] so new fields can
/// be added without breaking custom [`UpdateSource`](crate::UpdateSource)
/// implementations.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Asset {
    /// File name as published (e.g. `OpenLogi-v0.2.0-macos.dmg`).
    pub name: String,
    /// Direct download URL.
    pub url: String,
    /// Size in bytes (`0` if the source did not report it).
    pub size: u64,
}

impl Asset {
    /// Describe a downloadable file. Pass `0` for `size` when it is unknown.
    pub fn new(name: impl Into<String>, url: impl Into<String>, size: u64) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            size,
        }
    }
}

/// The latest applicable release resolved for the running platform.
///
/// Marked `#[non_exhaustive]`: build one with [`Release::new`] and the `with_*`
/// methods. Fields stay public for reading and adjusting.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Release {
    /// Parsed semantic version (the tag with any leading `v` stripped).
    pub version: Version,
    /// Human-readable release notes / changelog, when provided.
    pub notes: Option<String>,
    /// The platform artifact to download and install.
    pub asset: Asset,
    /// Inline detached minisign signature for `asset`, when published.
    pub signature: Option<String>,
    /// URL of a detached minisign signature for `asset`, when published.
    pub signature_url: Option<String>,
    /// Expected lowercase hex SHA-256 of `asset`, when the source could resolve
    /// it (e.g. from a `SHA256SUMS` file).
    pub sha256: Option<String>,
    /// Base64 Ed25519 signature over the whole `asset` file, in the format of a
    /// Sparkle appcast's `sparkle:edSignature` (what Sparkle's `sign_update`
    /// prints). Verified against
    /// [`EngineConfig::ed25519_public_key`](crate::EngineConfig::ed25519_public_key).
    pub ed25519_signature: Option<String>,
    /// Binary deltas that rebuild this release from older installed versions.
    /// Empty when the source publishes none. Used only with the `delta` feature;
    /// the full `asset` remains the fallback.
    pub deltas: Vec<DeltaAsset>,
    /// When the release was published, when the source says so. For display
    /// only; nothing in the engine depends on it.
    pub published_at: Option<SystemTime>,
    /// A web page about the release, such as its GitHub release page, for a
    /// "View release" link. The built-in sources keep only `http(s)` URLs.
    pub release_url: Option<String>,
}

impl Release {
    /// A release of `version` that ships `asset`, with no notes, checksum, or
    /// signatures yet.
    pub fn new(version: Version, asset: Asset) -> Self {
        Self {
            version,
            notes: None,
            asset,
            signature: None,
            signature_url: None,
            sha256: None,
            ed25519_signature: None,
            deltas: Vec::new(),
            published_at: None,
            release_url: None,
        }
    }

    /// Attach release notes (or a link to them).
    #[must_use]
    pub fn with_notes(mut self, notes: impl Into<String>) -> Self {
        self.notes = Some(notes.into());
        self
    }

    /// Attach the expected hex SHA-256 of the asset (stored lowercase).
    #[must_use]
    pub fn with_sha256(mut self, sha256: impl Into<String>) -> Self {
        self.sha256 = Some(sha256.into().trim().to_ascii_lowercase());
        self
    }

    /// Attach an inline minisign signature (the contents of a `.minisig`).
    #[must_use]
    pub fn with_minisign_signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = Some(signature.into());
        self
    }

    /// Attach the URL of a detached minisign signature.
    #[must_use]
    pub fn with_minisign_signature_url(mut self, url: impl Into<String>) -> Self {
        self.signature_url = Some(url.into());
        self
    }

    /// Attach a base64 Ed25519 signature in Sparkle's `sparkle:edSignature`
    /// format.
    #[must_use]
    pub fn with_ed25519_signature(mut self, signature: impl Into<String>) -> Self {
        self.ed25519_signature = Some(signature.into());
        self
    }

    /// Attach the binary deltas that rebuild this release from older
    /// versions.
    #[must_use]
    pub fn with_deltas(mut self, deltas: Vec<DeltaAsset>) -> Self {
        self.deltas = deltas;
        self
    }

    /// Attach the time the release was published.
    #[must_use]
    pub fn with_published_at(mut self, published_at: SystemTime) -> Self {
        self.published_at = Some(published_at);
        self
    }

    /// Attach the URL of a web page about the release. Pass an `http(s)` URL:
    /// apps typically open it in a browser.
    #[must_use]
    pub fn with_release_url(mut self, url: impl Into<String>) -> Self {
        self.release_url = Some(url.into());
        self
    }
}

/// `url`, trimmed, if it is an `http://` or `https://` URL a browser can open.
/// Anything else — blank, `file:`, `javascript:` — is dropped, because apps
/// hand this URL to the system browser.
pub(crate) fn web_page_url(url: Option<&str>) -> Option<String> {
    let url = url?.trim();
    let scheme = url.split_once("://").map(|(scheme, _)| scheme)?;
    let web = scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http");
    if !web {
        tracing::debug!(url, "ignoring a release page that is not an http(s) URL");
    }
    web.then(|| url.to_string())
}

/// A binary delta that upgrades one specific installed version to another.
///
/// Most deltas end at the release they are attached to. Sources that also list
/// deltas of earlier releases let the engine chain several of them when no
/// delta starts at the installed version.
///
/// Marked `#[non_exhaustive]`: build one with [`DeltaAsset::new`] and set the
/// public fields afterwards.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DeltaAsset {
    /// The installed version this delta applies to.
    pub from: Version,
    /// The version this delta produces.
    pub to: Version,
    /// Delta format, e.g. [`DELTA_FORMAT_FILES_BSDIFF_V1`]. Deltas in unknown
    /// formats are ignored.
    pub format: String,
    /// The delta file to download.
    pub asset: Asset,
    /// Expected lowercase hex SHA-256 of the delta file, when published.
    pub sha256: Option<String>,
    /// URL of a `SHA256SUMS`-style file listing the delta's checksum under
    /// `asset.name`, fetched only if this delta is actually used. Ignored when
    /// `sha256` is set.
    pub checksums_url: Option<String>,
    /// Inline detached minisign signature of the delta file, when published.
    pub signature: Option<String>,
    /// URL of a detached minisign signature of the delta file, when published.
    pub signature_url: Option<String>,
    /// Base64 Ed25519 signature of the delta file in Sparkle's
    /// `sparkle:edSignature` format, when published.
    pub ed25519_signature: Option<String>,
}

impl DeltaAsset {
    /// A delta in the [`DELTA_FORMAT_FILES_BSDIFF_V1`] format that upgrades
    /// `from` to `to`, with no checksum or signatures yet.
    pub fn new(from: Version, to: Version, asset: Asset) -> Self {
        Self {
            from,
            to,
            format: DELTA_FORMAT_FILES_BSDIFF_V1.to_string(),
            asset,
            sha256: None,
            checksums_url: None,
            signature: None,
            signature_url: None,
            ed25519_signature: None,
        }
    }
}

/// Parse a release tag such as `v1.2.3` or `1.2.3` into a [`Version`].
///
/// # Errors
/// Returns [`Error::Parse`] if the tag is not valid semver.
pub fn parse_tag(tag: &str) -> Result<Version> {
    let trimmed = tag.trim();
    let stripped = trimmed.strip_prefix('v').unwrap_or(trimmed);
    Version::parse(stripped).map_err(|e| Error::Parse(format!("tag `{tag}`: {e}")))
}

/// Parse a version the way feeds that are not strictly semver publish it.
///
/// Accepts everything [`parse_tag`] accepts, plus purely numeric versions with
/// one or two components, which are padded with zeros (`2` → `2.0.0`,
/// `1.4` → `1.4.0`). Sparkle feeds commonly publish such versions. Anything
/// else (four components, words, empty strings) is rejected rather than
/// guessed at, so a malformed feed never compares as an upgrade.
///
/// # Errors
/// Returns [`Error::Parse`] if the string is neither semver nor a 1–3
/// component numeric version.
pub fn parse_version_lenient(version: &str) -> Result<Version> {
    if let Ok(parsed) = parse_tag(version) {
        return Ok(parsed);
    }
    let trimmed = version.trim();
    let stripped = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let parts: Vec<&str> = stripped.split('.').collect();
    let numeric = !parts.is_empty()
        && parts.len() <= 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !numeric {
        return Err(Error::Parse(format!("version `{version}` is not semver")));
    }
    let mut padded = parts.join(".");
    for _ in parts.len()..3 {
        padded.push_str(".0");
    }
    Version::parse(&padded).map_err(|e| Error::Parse(format!("version `{version}`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::{Asset, Release, parse_tag, parse_version_lenient, web_page_url};
    use semver::Version;

    #[test]
    fn keeps_only_web_page_urls() {
        assert_eq!(
            web_page_url(Some(" https://a.example/x ")).as_deref(),
            Some("https://a.example/x")
        );
        assert_eq!(
            web_page_url(Some("HTTP://a.example")).as_deref(),
            Some("HTTP://a.example")
        );
        for bad in [
            None,
            Some(""),
            Some("javascript:alert(1)"),
            Some("file:///etc/passwd"),
            Some("ftp://a.example/x"),
            Some("/releases/1.2.0"),
        ] {
            assert_eq!(web_page_url(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn parses_v_prefixed_and_bare_tags() {
        assert_eq!(parse_tag("v1.2.3").unwrap().to_string(), "1.2.3");
        assert_eq!(parse_tag(" 1.2.3 ").unwrap().to_string(), "1.2.3");
        assert_eq!(parse_tag("v0.1.0-rc.1").unwrap().to_string(), "0.1.0-rc.1");
    }

    #[test]
    fn rejects_non_semver_tags() {
        assert!(parse_tag("latest").is_err());
        assert!(parse_tag("v1.2").is_err());
    }

    #[test]
    fn lenient_parse_pads_short_numeric_versions() {
        assert_eq!(parse_version_lenient("2").unwrap(), Version::new(2, 0, 0));
        assert_eq!(
            parse_version_lenient("v1.4").unwrap(),
            Version::new(1, 4, 0)
        );
        assert_eq!(
            parse_version_lenient(" 1.2.3 ").unwrap(),
            Version::new(1, 2, 3)
        );
        assert_eq!(
            parse_version_lenient("0.3.0-beta.2").unwrap().to_string(),
            "0.3.0-beta.2"
        );
    }

    #[test]
    fn lenient_parse_rejects_what_it_cannot_order() {
        for bad in ["", "1.2.3.4", "1..2", "build 42", "1.2b", "latest"] {
            assert!(parse_version_lenient(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn builder_sets_optional_fields() {
        let release = Release::new(
            Version::new(1, 0, 0),
            Asset::new("app.dmg", "https://example.test/app.dmg", 3),
        )
        .with_notes("notes")
        .with_sha256(" ABC ")
        .with_ed25519_signature("sig");
        assert_eq!(release.notes.as_deref(), Some("notes"));
        assert_eq!(release.sha256.as_deref(), Some("abc"));
        assert_eq!(release.ed25519_signature.as_deref(), Some("sig"));
        assert!(release.signature.is_none());
        assert_eq!(release.asset.size, 3);
    }
}
