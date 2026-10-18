//! The static JSON release manifest, as one set of types for both sides.
//!
//! [`StaticManifestSource`](crate::StaticManifestSource) reads these types, and
//! release tooling (for example `gpui-updater-release`) writes them, so the
//! schema has a single definition. Every optional field is skipped when empty
//! on output, and every unknown field is kept in `extra` on input, so a manifest
//! round-trips without losing producer-specific metadata.
//!
//! ```
//! use gpui_updater_core::manifest::{Manifest, ManifestAsset};
//!
//! let mut asset = ManifestAsset::new("App-macos-arm64.dmg", "https://dl.example.com/v1.2.3/App-macos-arm64.dmg");
//! asset.os = Some("macos".into());
//! asset.arch = Some("arm64".into());
//! let mut manifest = Manifest::new("1.2.3");
//! manifest.assets.push(asset);
//!
//! let json = serde_json::to_string(&manifest).unwrap();
//! let back: Manifest = serde_json::from_str(&json).unwrap();
//! assert_eq!(back.assets[0].arch.as_deref(), Some("arm64"));
//! ```

use serde::{Deserialize, Deserializer, Serialize};

/// The manifest schema version this crate reads and writes.
pub const SCHEMA_VERSION: u32 = 1;

/// Fields that are not part of the schema, kept as-is.
pub type Extra = serde_json::Map<String, serde_json::Value>;

/// One release: its version and the files a client can install.
///
/// Required fields are `version` and `assets`. The other top-level fields
/// describe the release for people and for tooling; the engine reads
/// `schema_version`, `version`, `notes`, `notes_url`, and `expires`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Manifest {
    /// Schema version; a missing value reads as `1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    /// Application identifier, for example a bundle identifier.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub app_id: Option<String>,
    /// Release version (`1.2.3` or `v1.2.3`).
    pub version: String,
    /// The release's tag in version control.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub tag: Option<String>,
    /// Release channel, for example `stable` or `beta`.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub channel: Option<String>,
    /// RFC 3339 publication time.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub published_at: Option<String>,
    /// RFC 3339 time after which the manifest must not be trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// Human-facing release page.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub release_url: Option<String>,
    /// Release notes text, shown in preference to `notes_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Link to the release notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    /// The downloadable files, in the order a client should consider them.
    #[serde(default)]
    pub assets: Vec<ManifestAsset>,
    /// Unknown top-level fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl Manifest {
    /// A manifest for `version` with the current schema version and no assets.
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            schema_version: Some(SCHEMA_VERSION),
            app_id: None,
            version: version.into(),
            tag: None,
            channel: None,
            published_at: None,
            expires: None,
            release_url: None,
            notes: None,
            notes_url: None,
            assets: Vec::new(),
            extra: Extra::new(),
        }
    }
}

/// One downloadable file of a release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ManifestAsset {
    /// File name.
    pub name: String,
    /// Direct HTTPS download URL.
    pub url: String,
    /// Operating system, for example `macos`, `windows`, or `linux`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// CPU architecture, for example `arm64`, `x86_64`, or `universal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// Package format, for example `dmg`, `msi`, `zip`, or `tar.gz`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// MIME type.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub content_type: Option<String>,
    /// Size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Lowercase hex SHA-256 of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Inline minisign signature (the `.minisig` file's contents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// URL of the minisign signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_url: Option<String>,
    /// Base64 Ed25519 signature over the file, as Sparkle's `sign_update`
    /// prints it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ed25519_signature: Option<String>,
    /// Oldest operating-system version the file supports.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub minimum_os_version: Option<String>,
    /// Binary deltas that rebuild this file's contents from older versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deltas: Vec<ManifestDelta>,
    /// Unknown asset fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl ManifestAsset {
    /// An asset with only its required fields.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            os: None,
            arch: None,
            format: None,
            content_type: None,
            size: None,
            sha256: None,
            signature: None,
            signature_url: None,
            ed25519_signature: None,
            minimum_os_version: None,
            deltas: Vec::new(),
            extra: Extra::new(),
        }
    }
}

/// A binary delta that rebuilds an asset's contents from an older version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ManifestDelta {
    /// The installed version the delta applies to.
    pub from: String,
    /// The version the delta produces; defaults to the manifest's `version`.
    /// Set it to list deltas between older releases, which the engine can
    /// chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Direct HTTPS download URL.
    pub url: String,
    /// File name; defaults to the URL's last path segment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Delta format; defaults to `files-bsdiff-v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Lowercase hex SHA-256 of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Inline minisign signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// URL of the minisign signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_url: Option<String>,
    /// Base64 Ed25519 signature over the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ed25519_signature: Option<String>,
    /// Unknown delta fields.
    #[serde(flatten)]
    pub extra: Extra,
}

impl ManifestDelta {
    /// A delta with only its required fields.
    pub fn new(from: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: None,
            url: url.into(),
            name: None,
            format: None,
            size: None,
            sha256: None,
            signature: None,
            signature_url: None,
            ed25519_signature: None,
            extra: Extra::new(),
        }
    }
}

/// Descriptive fields that the engine never reads. Before they had a type, any
/// JSON value was ignored, so a value of the wrong type still reads as absent
/// instead of failing the whole manifest.
fn lenient<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(s) => Some(s),
        _ => None,
    })
}
