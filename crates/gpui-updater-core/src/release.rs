//! Release metadata shared across update sources.

use semver::Version;

use crate::error::{Error, Result};

/// Format identifier of the deltas produced by the `delta` feature's
/// `delta::create`: per-file bsdiff patches against the installed tree.
pub const DELTA_FORMAT_FILES_BSDIFF_V1: &str = "files-bsdiff-v1";

/// A downloadable file attached to a release.
#[derive(Debug, Clone)]
pub struct Asset {
    /// File name as published (e.g. `OpenLogi-v0.2.0-macos.dmg`).
    pub name: String,
    /// Direct download URL.
    pub url: String,
    /// Size in bytes (`0` if the source did not report it).
    pub size: u64,
}

/// The latest applicable release resolved for the running platform.
#[derive(Debug, Clone)]
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
    /// Binary deltas that rebuild this release from older installed versions.
    /// Empty when the source publishes none. Used only with the `delta` feature;
    /// the full `asset` remains the fallback.
    pub deltas: Vec<DeltaAsset>,
}

/// A binary delta that upgrades one specific installed version to the release
/// it is attached to.
#[derive(Debug, Clone)]
pub struct DeltaAsset {
    /// The installed version this delta applies to.
    pub from: Version,
    /// Delta format, e.g. [`DELTA_FORMAT_FILES_BSDIFF_V1`]. Deltas in unknown
    /// formats are ignored.
    pub format: String,
    /// The delta file to download.
    pub asset: Asset,
    /// Expected lowercase hex SHA-256 of the delta file, when published.
    pub sha256: Option<String>,
    /// Inline detached minisign signature of the delta file, when published.
    pub signature: Option<String>,
    /// URL of a detached minisign signature of the delta file, when published.
    pub signature_url: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::parse_tag;

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
}
