//! [`UpdateSource`] backed by a static JSON release manifest.
//!
//! This source works with any HTTPS-hosted manifest: Cloudflare R2, AWS S3,
//! `MinIO`, Backblaze B2, GitHub Pages, GitHub Releases, or a normal static web
//! server. It does not speak the S3 API; the updater only needs a static JSON
//! document plus direct artifact URLs.
//!
//! Each asset may list binary deltas from older versions under `deltas`; see
//! [`StaticManifestSource`].

use std::time::SystemTime;

use semver::Version;
use serde::Deserialize;

use super::{UpdateSource, dates, trust};
use crate::error::{Error, Result};
use crate::http::{self, HttpOptions};
use crate::release::{
    Asset, DELTA_FORMAT_FILES_BSDIFF_V1, DeltaAsset, Release, parse_tag, web_page_url,
};

/// Reads release metadata from a static JSON manifest.
///
/// The manifest describes one latest release and a flat list of downloadable
/// assets. Unknown fields are ignored, so producers can include additional
/// metadata such as `app_id`, `channel`, or platform-specific
/// compatibility fields.
///
/// Required manifest fields are `version` and `assets`; required asset fields
/// are `name` and `url`.
///
/// An asset may carry a minisign `signature` / `signature_url`, a `sha256`, and
/// an `ed25519_signature` (base64, in the format Sparkle's `sign_update`
/// prints), so one release pipeline can sign for this manifest and a Sparkle
/// appcast with the same key.
///
/// An asset can also carry `deltas`: binary deltas that rebuild it from an
/// older installed version (see the `delta` feature). Each needs `from` and
/// `url`; `name`, `format` (default `files-bsdiff-v1`), `size`, `sha256`,
/// `signature`, and `signature_url` are optional and mean the same as on the
/// asset. An entry whose `from` is not valid semver is ignored.
///
/// ```json
/// "deltas": [{
///   "from": "0.8.12",
///   "url": "https://dl.example.com/v0.8.13/OpenLogi-macos-arm64.dmg.from-0.8.12.delta",
///   "size": 1850000,
///   "sha256": "…",
///   "signature_url": "https://dl.example.com/v0.8.13/OpenLogi-macos-arm64.dmg.from-0.8.12.delta.minisig"
/// }]
/// ```
///
/// ```no_run
/// use gpui_updater_core::StaticManifestSource;
///
/// let source = StaticManifestSource::new("https://dl.example.com/channels/stable/latest.json")
///     .os("macos")
///     .arch("arm64")
///     .format("dmg");
/// ```
pub struct StaticManifestSource {
    url: String,
    /// Case-insensitive substrings; an asset matches when its name contains all
    /// of them. Defaults to a single extension guessed from the running OS.
    asset_patterns: Vec<String>,
    /// Optional exact match against the asset's `os` field.
    os: Option<String>,
    /// Optional exact match against the asset's `arch` field.
    arch: Option<String>,
    /// Optional exact match against the asset's `format` field.
    format: Option<String>,
    /// Base64 minisign keys the manifest must be signed by; empty means the
    /// manifest is not verified.
    keys: Vec<String>,
    /// Where the manifest's signature lives; defaults to `<url>.minisig`.
    signature_url: Option<String>,
    /// Extra headers and the proxy for the manifest and its signature.
    http: HttpOptions,
}

impl StaticManifestSource {
    /// Create a static-manifest source, matching assets by an extension guessed
    /// from the current OS (`.dmg` / `.exe` / `.tar.gz`).
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            asset_patterns: vec![default_extension().to_string()],
            os: None,
            arch: None,
            format: None,
            keys: Vec::new(),
            signature_url: None,
            http: HttpOptions::new(),
        }
    }

    /// Fetch the manifest and its signature with `http`'s extra headers and proxy.
    #[must_use]
    pub fn http(mut self, http: HttpOptions) -> Self {
        self.http = http;
        self
    }

    /// Require the manifest to be signed by this base64 minisign public key.
    /// Call again to accept several, for example while rotating keys.
    #[must_use]
    pub fn minisign_public_key(mut self, key: impl Into<String>) -> Self {
        self.keys.push(key.into());
        self
    }

    /// Fetch the manifest's signature from `url` instead of `<url>.minisig`.
    #[must_use]
    pub fn signature_url(mut self, url: impl Into<String>) -> Self {
        self.signature_url = Some(url.into());
        self
    }

    /// Require the selected asset's name to contain `substr` (case-insensitive).
    /// Call multiple times to require several substrings (e.g. `macos` + `arm64`).
    #[must_use]
    pub fn asset_contains(mut self, substr: impl Into<String>) -> Self {
        self.asset_patterns.push(substr.into());
        self
    }

    /// Replace the asset-matching substrings entirely.
    #[must_use]
    pub fn asset_patterns(mut self, patterns: Vec<String>) -> Self {
        self.asset_patterns = patterns;
        self
    }

    /// Require the selected asset's `os` field to equal `os` (case-insensitive).
    ///
    /// Common values are `macos`, `windows`, and `linux`.
    #[must_use]
    pub fn os(mut self, os: impl Into<String>) -> Self {
        self.os = Some(os.into());
        self
    }

    /// Require the selected asset's `arch` field to equal `arch` (case-insensitive).
    ///
    /// Common values are `arm64`, `x86_64`, and `universal`.
    #[must_use]
    pub fn arch(mut self, arch: impl Into<String>) -> Self {
        self.arch = Some(arch.into());
        self
    }

    /// Require the selected asset's `format` field to equal `format`
    /// (case-insensitive).
    ///
    /// Common values are `dmg`, `zip`, `msi`, `exe`, `appimage`, and `tar.gz`.
    #[must_use]
    pub fn format(mut self, format: impl Into<String>) -> Self {
        if self.asset_patterns.len() == 1
            && self.asset_patterns[0].eq_ignore_ascii_case(default_extension())
        {
            self.asset_patterns.clear();
        }
        self.format = Some(format.into());
        self
    }

    /// The manifest URL this source fetches.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl UpdateSource for StaticManifestSource {
    fn fetch_latest(&self) -> Result<Release> {
        let manifest = self.fetch_manifest()?;
        manifest.validate_schema_version()?;
        if let Some(expires) = manifest.expires.as_deref().filter(|s| !s.trim().is_empty()) {
            trust::check_expiry(expires, SystemTime::now())?;
        }
        let version = parse_tag(&manifest.version)?;
        let selector = AssetSelector {
            name_patterns: &self.asset_patterns,
            os: self.os.as_deref(),
            arch: self.arch.as_deref(),
            format: self.format.as_deref(),
        };
        let main = select_asset(&manifest.assets, selector).ok_or(Error::NoMatchingAsset {
            target_os: std::env::consts::OS,
            target_arch: std::env::consts::ARCH,
        })?;

        let deltas = main
            .deltas
            .iter()
            .filter_map(|d| delta_asset(d, &version))
            .collect();
        Ok(Release {
            version,
            notes: manifest
                .notes
                .filter(|s| !s.trim().is_empty())
                .or(manifest.notes_url),
            asset: Asset {
                name: main.name.clone(),
                url: main.url.clone(),
                size: main.size.unwrap_or(0),
            },
            signature: main.signature.clone().filter(|s| !s.trim().is_empty()),
            signature_url: main.signature_url.clone().filter(|s| !s.trim().is_empty()),
            sha256: main.sha256.as_ref().map(|s| s.trim().to_ascii_lowercase()),
            ed25519_signature: main
                .ed25519_signature
                .clone()
                .filter(|s| !s.trim().is_empty()),
            deltas,
            published_at: manifest.published_at.as_deref().and_then(dates::rfc3339),
            release_url: web_page_url(manifest.release_url.as_deref()),
        })
    }
}

impl StaticManifestSource {
    /// Fetch and parse the manifest, verifying its signature first when keys
    /// are configured.
    fn fetch_manifest(&self) -> Result<Manifest> {
        if self.keys.is_empty() {
            return http::get_json(&self.url, &[], &self.http);
        }
        let signature_url = self
            .signature_url
            .clone()
            .unwrap_or_else(|| format!("{}.minisig", self.url));
        let body = trust::fetch_signed(&self.url, &signature_url, &self.keys, &self.http)?;
        serde_json::from_str(&body).map_err(|e| Error::Parse(e.to_string()))
    }
}

/// A manifest delta as a [`DeltaAsset`]; `to` defaults to the manifest's
/// own version.
fn delta_asset(delta: &ManifestDelta, version: &Version) -> Option<DeltaAsset> {
    let parse = |tag: &str, field: &str| match parse_tag(tag) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, field, "ignoring manifest delta with an invalid version");
            None
        }
    };
    let from = parse(&delta.from, "from")?;
    let to = match &delta.to {
        Some(to) if !to.trim().is_empty() => parse(to, "to")?,
        _ => version.clone(),
    };
    let non_empty = |s: &Option<String>| s.clone().filter(|s| !s.trim().is_empty());
    Some(DeltaAsset {
        from,
        to,
        format: non_empty(&delta.format)
            .unwrap_or_else(|| DELTA_FORMAT_FILES_BSDIFF_V1.to_string()),
        asset: Asset {
            name: non_empty(&delta.name).unwrap_or_else(|| url_file_name(&delta.url)),
            url: delta.url.clone(),
            size: delta.size.unwrap_or(0),
        },
        sha256: delta.sha256.as_ref().map(|s| s.trim().to_ascii_lowercase()),
        checksums_url: None,
        signature: non_empty(&delta.signature),
        signature_url: non_empty(&delta.signature_url),
        ed25519_signature: non_empty(&delta.ed25519_signature),
    })
}

/// The last path segment of `url`, without query or fragment.
fn url_file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => "update.delta".to_string(),
    }
}

fn default_extension() -> &'static str {
    match std::env::consts::OS {
        "macos" => ".dmg",
        "windows" => ".exe",
        _ => ".tar.gz",
    }
}

#[derive(Clone, Copy)]
struct AssetSelector<'a> {
    name_patterns: &'a [String],
    os: Option<&'a str>,
    arch: Option<&'a str>,
    format: Option<&'a str>,
}

fn asset_matches(asset: &ManifestAsset, selector: AssetSelector<'_>) -> bool {
    let lower_name = asset.name.to_ascii_lowercase();
    let name_matches = selector
        .name_patterns
        .iter()
        .all(|p| lower_name.contains(&p.to_ascii_lowercase()));
    name_matches
        && field_matches(asset.os.as_deref(), selector.os)
        && field_matches(asset.arch.as_deref(), selector.arch)
        && field_matches(asset.format.as_deref(), selector.format)
}

fn field_matches(actual: Option<&str>, want: Option<&str>) -> bool {
    want.is_none_or(|want| actual.is_some_and(|actual| actual.eq_ignore_ascii_case(want)))
}

fn select_asset<'a>(
    assets: &'a [ManifestAsset],
    selector: AssetSelector<'_>,
) -> Option<&'a ManifestAsset> {
    assets.iter().find(|a| asset_matches(a, selector))
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    schema_version: Option<u32>,
    version: String,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    notes_url: Option<String>,
    /// RFC 3339 time the release was published.
    #[serde(default)]
    published_at: Option<String>,
    /// A web page about the release.
    #[serde(default)]
    release_url: Option<String>,
    /// RFC 3339 time after which the manifest must not be trusted.
    #[serde(default)]
    expires: Option<String>,
    #[serde(default)]
    assets: Vec<ManifestAsset>,
}

impl Manifest {
    fn validate_schema_version(&self) -> Result<()> {
        match self.schema_version.unwrap_or(1) {
            1 => Ok(()),
            other => Err(Error::Parse(format!(
                "unsupported static manifest schema_version `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ManifestAsset {
    name: String,
    url: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    signature_url: Option<String>,
    #[serde(default)]
    ed25519_signature: Option<String>,
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    arch: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    deltas: Vec<ManifestDelta>,
}

#[derive(Debug, Deserialize)]
struct ManifestDelta {
    from: String,
    /// Defaults to the manifest's `version`; set it to list deltas between
    /// older releases, which the engine can chain.
    #[serde(default)]
    to: Option<String>,
    url: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    signature_url: Option<String>,
    #[serde(default)]
    ed25519_signature: Option<String>,
}

#[cfg(test)]
mod tests;
