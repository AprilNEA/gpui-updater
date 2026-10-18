//! Build the static JSON manifest that
//! [`StaticManifestSource`](gpui_updater_core::StaticManifestSource) reads.
//!
//! ```no_run
//! use gpui_updater_release::{Classifier, FeedLayout, ManifestBuilder, SignaturePolicy};
//!
//! let layout = FeedLayout::new("https://updates.example.com", "stable");
//! let manifest = ManifestBuilder::for_layout(&layout, "v1.2.3")
//!     .app_id("com.example.App")
//!     .release_url("https://github.com/me/app/releases/tag/v1.2.3")
//!     .classifier(Classifier::default().minimum_os_version("macos", "13.0"))
//!     .signatures(SignaturePolicy::Required)
//!     .scan_dir("dist")?
//!     .require_os("macos")
//!     .build()?;
//! gpui_updater_release::write_manifest(&manifest, "out/latest.json".as_ref())?;
//! # Ok::<_, gpui_updater_release::Error>(())
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use gpui_updater_core::manifest::{Manifest, ManifestAsset, ManifestDelta};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::artifact::{Classified, Classifier};
use crate::error::{Error, Result};
use crate::layout::FeedLayout;
use crate::sign::{SparkleKey, sidecar};
use crate::util::{file_name, join_url, sha256_file};

/// Whether each file must have a minisign signature next to it
/// (`<file>.minisig`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignaturePolicy {
    /// Every asset and delta must have one; a missing signature is an error.
    #[default]
    Required,
    /// Reference a signature when it exists.
    IfPresent,
    /// Never reference signatures.
    Ignore,
}

/// How a minisign signature appears in the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignatureStyle {
    /// `signature_url`, pointing at `<file>.minisig` next to the file.
    #[default]
    Url,
    /// `signature`, the signature text itself: one request fewer per update.
    Inline,
    /// Both.
    Both,
}

/// The manifest's `published_at`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PublishedAt {
    /// The current time.
    #[default]
    Now,
    /// This RFC 3339 time, for reproducible output.
    At(String),
    /// Leave the field out.
    Omit,
}

/// What to do with a file that no rule classifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Unmatched {
    /// Leave it out (checksums, signatures, and other side files).
    #[default]
    Skip,
    /// Fail the build.
    Error,
}

/// A delta file to list under one asset's `deltas`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeltaFile {
    /// Name of the asset the delta rebuilds.
    pub asset: String,
    /// Version the delta applies to.
    pub from: String,
    /// Version it produces, when not the manifest's own.
    pub to: Option<String>,
    /// Delta format, when not `files-bsdiff-v1`.
    pub format: Option<String>,
    /// The delta on disk.
    pub path: PathBuf,
}

impl DeltaFile {
    /// A delta from `from` to the manifest's version, rebuilding `asset`.
    pub fn new(
        asset: impl Into<String>,
        from: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            asset: asset.into(),
            from: from.into(),
            to: None,
            format: None,
            path: path.into(),
        }
    }

    /// Recognize `<asset>.from-<version>.delta`, the name
    /// [`delta_file_name`] gives.
    pub fn from_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        let stem = name.strip_suffix(".delta")?;
        let (asset, from) = stem.rsplit_once(".from-")?;
        if asset.is_empty() || from.is_empty() {
            return None;
        }
        Some(Self::new(asset, from, path))
    }
}

/// The conventional delta file name: `<asset>.from-<version>.delta`.
pub fn delta_file_name(asset: &str, from: &str) -> String {
    format!("{asset}.from-{}.delta", from.trim_start_matches('v'))
}

/// Builds a [`Manifest`] from release files.
#[derive(Debug)]
pub struct ManifestBuilder {
    manifest: Manifest,
    download_base: String,
    classifier: Classifier,
    include_os: Option<Vec<String>>,
    exclude_os: Vec<String>,
    required: Vec<(String, Option<String>)>,
    signatures: SignaturePolicy,
    signature_style: SignatureStyle,
    sparkle_key: Option<SparkleKey>,
    published_at: PublishedAt,
    expires_after: Option<time::Duration>,
    files: Vec<PathBuf>,
    assets: Vec<ManifestAsset>,
    deltas: Vec<DeltaFile>,
    unmatched: Unmatched,
    sort_by_name: bool,
    sha256: bool,
}

impl ManifestBuilder {
    /// A manifest for `version`, whose files download from `download_base`
    /// (the URL prefix of the release's directory).
    pub fn new(version: impl Into<String>, download_base: impl Into<String>) -> Self {
        let version = version.into();
        let version = version.strip_prefix('v').unwrap_or(&version).to_string();
        Self {
            manifest: Manifest::new(version),
            download_base: download_base.into(),
            classifier: Classifier::default(),
            include_os: None,
            exclude_os: Vec::new(),
            required: Vec::new(),
            signatures: SignaturePolicy::default(),
            signature_style: SignatureStyle::default(),
            sparkle_key: None,
            published_at: PublishedAt::default(),
            expires_after: None,
            files: Vec::new(),
            assets: Vec::new(),
            deltas: Vec::new(),
            unmatched: Unmatched::default(),
            sort_by_name: true,
            sha256: true,
        }
    }

    /// A manifest for the release `tag` in `layout`: files download from the
    /// release directory, and `tag` and `channel` are filled in.
    pub fn for_layout(layout: &FeedLayout, tag: &str) -> Self {
        Self::new(tag, layout.release_url(tag))
            .tag(tag)
            .channel(layout.channel.clone())
    }

    /// Set `app_id`.
    #[must_use]
    pub fn app_id(mut self, app_id: impl Into<String>) -> Self {
        self.manifest.app_id = Some(app_id.into());
        self
    }

    /// Set `tag`.
    #[must_use]
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.manifest.tag = Some(tag.into());
        self
    }

    /// Set `channel`.
    #[must_use]
    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.manifest.channel = Some(channel.into());
        self
    }

    /// Set `release_url`.
    #[must_use]
    pub fn release_url(mut self, url: impl Into<String>) -> Self {
        self.manifest.release_url = Some(url.into());
        self
    }

    /// Set `notes`, the release notes text.
    #[must_use]
    pub fn notes(mut self, notes: impl Into<String>) -> Self {
        self.manifest.notes = Some(notes.into());
        self
    }

    /// Set `notes_url`.
    #[must_use]
    pub fn notes_url(mut self, url: impl Into<String>) -> Self {
        self.manifest.notes_url = Some(url.into());
        self
    }

    /// Set `published_at`.
    #[must_use]
    pub fn published_at(mut self, published_at: PublishedAt) -> Self {
        self.published_at = published_at;
        self
    }

    /// Set `expires` to this long after `published_at` (or now). Clients
    /// reject the manifest after that, which stops a stale copy from being
    /// replayed; republish before then.
    #[must_use]
    pub fn expires_after(mut self, duration: time::Duration) -> Self {
        self.expires_after = Some(duration);
        self
    }

    /// Set `expires` to an RFC 3339 time.
    #[must_use]
    pub fn expires_at(mut self, rfc3339: impl Into<String>) -> Self {
        self.manifest.expires = Some(rfc3339.into());
        self.expires_after = None;
        self
    }

    /// Add a top-level field that is not part of the schema.
    #[must_use]
    pub fn extra(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.manifest.extra.insert(key.into(), value);
        self
    }

    /// Replace the file-name rules.
    #[must_use]
    pub fn classifier(mut self, classifier: Classifier) -> Self {
        self.classifier = classifier;
        self
    }

    /// List only assets for these operating systems.
    #[must_use]
    pub fn include_os<I, S>(mut self, oses: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.include_os = Some(oses.into_iter().map(Into::into).collect());
        self
    }

    /// Leave out assets for this operating system.
    #[must_use]
    pub fn exclude_os(mut self, os: impl Into<String>) -> Self {
        self.exclude_os.push(os.into());
        self
    }

    /// Fail unless at least one asset is for `os`.
    #[must_use]
    pub fn require_os(mut self, os: impl Into<String>) -> Self {
        self.required.push((os.into(), None));
        self
    }

    /// Fail unless at least one asset is for `os` on `arch`.
    #[must_use]
    pub fn require_target(mut self, os: impl Into<String>, arch: impl Into<String>) -> Self {
        self.required.push((os.into(), Some(arch.into())));
        self
    }

    /// Set whether files need minisign signatures (default: required).
    #[must_use]
    pub fn signatures(mut self, policy: SignaturePolicy) -> Self {
        self.signatures = policy;
        self
    }

    /// Set how signatures appear (default: `signature_url`).
    #[must_use]
    pub fn signature_style(mut self, style: SignatureStyle) -> Self {
        self.signature_style = style;
        self
    }

    /// Also sign every asset and delta with this Sparkle Ed25519 key, into
    /// `ed25519_signature`.
    #[must_use]
    pub fn ed25519_key(mut self, key: SparkleKey) -> Self {
        self.sparkle_key = Some(key);
        self
    }

    /// Leave out `sha256` (default: computed).
    #[must_use]
    pub fn without_sha256(mut self) -> Self {
        self.sha256 = false;
        self
    }

    /// Set what happens to files no rule classifies (default: skipped).
    #[must_use]
    pub fn unmatched(mut self, unmatched: Unmatched) -> Self {
        self.unmatched = unmatched;
        self
    }

    /// Keep assets in the order they were added instead of by name.
    #[must_use]
    pub fn keep_order(mut self) -> Self {
        self.sort_by_name = false;
        self
    }

    /// Add one release file, classified by name.
    #[must_use]
    pub fn file(mut self, path: impl Into<PathBuf>) -> Self {
        self.files.push(path.into());
        self
    }

    /// Add every regular file in `dir` (not recursive). Delta files named
    /// `<asset>.from-<version>.delta` are attached to their asset.
    pub fn scan_dir(mut self, dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let entries = fs::read_dir(dir).map_err(Error::io("read directory", dir))?;
        for entry in entries {
            let path = entry.map_err(Error::io("read directory", dir))?.path();
            if !path.is_file() {
                continue;
            }
            if let Some(delta) = DeltaFile::from_path(&path) {
                self.deltas.push(delta);
            } else {
                self.files.push(path);
            }
        }
        Ok(self)
    }

    /// Add an asset you built yourself; it is listed as given.
    #[must_use]
    pub fn asset(mut self, asset: ManifestAsset) -> Self {
        self.assets.push(asset);
        self
    }

    /// Attach a delta file to an asset.
    #[must_use]
    pub fn delta(mut self, delta: DeltaFile) -> Self {
        self.deltas.push(delta);
        self
    }

    /// Hash, sign, and classify everything into the manifest.
    pub fn build(self) -> Result<Manifest> {
        let mut manifest = self.manifest.clone();
        let mut files = self.files.clone();
        files.sort();
        let mut assets = self.assets.clone();
        for path in &files {
            let name = file_name(path)?;
            let Some(classified) = self.classifier.classify(name) else {
                if self.unmatched == Unmatched::Error && !is_side_file(name) {
                    return Err(Error::invalid(format!("no rule classifies `{name}`")));
                }
                continue;
            };
            if !self.wants_os(&classified.os) {
                continue;
            }
            assets.push(self.asset_for(path, name, &classified)?);
        }
        if self.sort_by_name {
            assets.sort_by(|a, b| a.name.cmp(&b.name));
        }
        let mut seen = std::collections::BTreeSet::new();
        for asset in &assets {
            if !seen.insert(asset.name.as_str()) {
                return Err(Error::invalid(format!(
                    "two assets are named `{}`",
                    asset.name
                )));
            }
        }
        for delta in &self.deltas {
            let Some(asset) = assets.iter_mut().find(|a| a.name == delta.asset) else {
                if self.wants_asset_name(&delta.asset) {
                    return Err(Error::invalid(format!(
                        "delta `{}` is for `{}`, which is not an asset",
                        delta.path.display(),
                        delta.asset
                    )));
                }
                continue;
            };
            asset.deltas.push(self.delta_for(delta)?);
        }
        for asset in &mut assets {
            asset.deltas.sort_by(|a, b| version_order(&b.from, &a.from));
        }
        for (os, arch) in &self.required {
            let found = assets.iter().any(|a| {
                a.os.as_deref().is_some_and(|o| o.eq_ignore_ascii_case(os))
                    && arch.as_deref().is_none_or(|arch| {
                        a.arch
                            .as_deref()
                            .is_some_and(|a| a.eq_ignore_ascii_case(arch))
                    })
            });
            if !found {
                let target = arch
                    .as_deref()
                    .map_or_else(|| os.clone(), |arch| format!("{os}/{arch}"));
                return Err(Error::invalid(format!("no {target} asset in the release")));
            }
        }
        manifest.assets = assets;

        let now = OffsetDateTime::now_utc();
        let published = match &self.published_at {
            PublishedAt::Now => Some(now),
            PublishedAt::At(at) => Some(
                OffsetDateTime::parse(at, &Rfc3339)
                    .map_err(|e| Error::invalid(format!("published_at `{at}`: {e}")))?,
            ),
            PublishedAt::Omit => None,
        };
        manifest.published_at = match &self.published_at {
            PublishedAt::At(at) => Some(at.clone()),
            PublishedAt::Now => Some(rfc3339(now)?),
            PublishedAt::Omit => None,
        };
        if let Some(after) = self.expires_after {
            manifest.expires = Some(rfc3339(published.unwrap_or(now) + after)?);
        }
        Ok(manifest)
    }

    fn wants_os(&self, os: &str) -> bool {
        self.include_os
            .as_ref()
            .is_none_or(|include| include.iter().any(|o| o.eq_ignore_ascii_case(os)))
            && !self.exclude_os.iter().any(|o| o.eq_ignore_ascii_case(os))
    }

    /// Whether a delta's asset would have been listed, had it been present.
    fn wants_asset_name(&self, asset: &str) -> bool {
        self.classifier
            .classify(asset)
            .is_none_or(|classified| self.wants_os(&classified.os))
    }

    fn asset_for(&self, path: &Path, name: &str, classified: &Classified) -> Result<ManifestAsset> {
        let mut asset = ManifestAsset::new(name, join_url(&self.download_base, name));
        asset.os = Some(classified.os.clone());
        asset.arch.clone_from(&classified.arch);
        asset.format = Some(classified.format.clone());
        asset.content_type.clone_from(&classified.content_type);
        asset
            .minimum_os_version
            .clone_from(&classified.minimum_os_version);
        let signed = self.file_metadata(path, name)?;
        asset.size = Some(signed.size);
        asset.sha256 = signed.sha256;
        asset.signature = signed.signature;
        asset.signature_url = signed.signature_url;
        asset.ed25519_signature = signed.ed25519_signature;
        Ok(asset)
    }

    fn delta_for(&self, delta: &DeltaFile) -> Result<ManifestDelta> {
        let name = file_name(&delta.path)?;
        let mut out = ManifestDelta::new(
            delta.from.trim_start_matches('v'),
            join_url(&self.download_base, name),
        );
        out.to = delta
            .to
            .as_ref()
            .map(|t| t.trim_start_matches('v').to_string());
        out.format.clone_from(&delta.format);
        let signed = self.file_metadata(&delta.path, name)?;
        out.size = Some(signed.size);
        out.sha256 = signed.sha256;
        out.signature = signed.signature;
        out.signature_url = signed.signature_url;
        out.ed25519_signature = signed.ed25519_signature;
        Ok(out)
    }

    fn file_metadata(&self, path: &Path, name: &str) -> Result<FileMetadata> {
        let size = fs::metadata(path).map_err(Error::io("stat", path))?.len();
        let sha256 = if self.sha256 {
            Some(sha256_file(path)?)
        } else {
            None
        };
        let minisig = sidecar(path, "minisig");
        let (mut signature, mut signature_url) = (None, None);
        if self.signatures != SignaturePolicy::Ignore {
            if minisig.is_file() {
                if matches!(
                    self.signature_style,
                    SignatureStyle::Inline | SignatureStyle::Both
                ) {
                    signature =
                        Some(fs::read_to_string(&minisig).map_err(Error::io("read", &minisig))?);
                }
                if matches!(
                    self.signature_style,
                    SignatureStyle::Url | SignatureStyle::Both
                ) {
                    signature_url = Some(join_url(&self.download_base, &format!("{name}.minisig")));
                }
            } else if self.signatures == SignaturePolicy::Required {
                return Err(Error::invalid(format!(
                    "`{name}` has no minisign signature at `{}`",
                    minisig.display()
                )));
            }
        }
        let ed25519_signature = self
            .sparkle_key
            .as_ref()
            .map(|key| key.sign_file(path))
            .transpose()?;
        Ok(FileMetadata {
            size,
            sha256,
            signature,
            signature_url,
            ed25519_signature,
        })
    }
}

struct FileMetadata {
    size: u64,
    sha256: Option<String>,
    signature: Option<String>,
    signature_url: Option<String>,
    ed25519_signature: Option<String>,
}

/// Files that ride along with a release and are never assets.
#[allow(clippy::case_sensitive_file_extension_comparisons)] // `name` is lowercased first.
fn is_side_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".minisig") || lower.ends_with(".sig") || lower.starts_with("sha256sums")
}

/// Newest first when both parse as versions, else by text.
fn version_order(a: &str, b: &str) -> std::cmp::Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => a.cmp(b),
    }
}

fn rfc3339(at: OffsetDateTime) -> Result<String> {
    at.format(&Rfc3339)
        .map_err(|e| Error::invalid(format!("format a time: {e}")))
}

/// Write `manifest` as pretty JSON with a trailing newline, creating parent
/// directories.
pub fn write_manifest(manifest: &Manifest, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(Error::io("create", parent))?;
    }
    let json = serde_json::to_string_pretty(manifest)? + "\n";
    fs::write(path, json).map_err(Error::io("write", path))
}

/// Read a manifest back, for example to merge two builds.
pub fn read_manifest(path: &Path) -> Result<Manifest> {
    let text = fs::read_to_string(path).map_err(Error::io("read", path))?;
    Ok(serde_json::from_str(&text)?)
}

#[cfg(test)]
mod tests;
