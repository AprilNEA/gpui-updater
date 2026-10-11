//! [`UpdateSource`] backed by a Sparkle appcast.
//!
//! An appcast is the RSS 2.0 feed Sparkle (macOS) and WinSparkle (Windows)
//! read, with metadata in the `sparkle:` XML namespace. Reading it here lets one
//! feed serve both a Sparkle-driven macOS app and this crate's own engine on
//! other platforms (or on macOS when Sparkle is not embedded).

mod feed;
mod host;
#[cfg(test)]
mod tests;

use semver::Version;

use super::UpdateSource;
use crate::error::{Error, Result};
use crate::http;
use crate::release::{Asset, Release, parse_version_lenient};

use feed::{Enclosure, Item};
use host::{HostFacts, OsMatcher};

/// Which appcast field holds the version that is compared and reported.
///
/// Sparkle itself orders updates by `sparkle:version` (the bundle's
/// `CFBundleVersion`, often a build number) and shows
/// `sparkle:shortVersionString` (`CFBundleShortVersionString`) to users. This
/// crate orders by semver, so it reads the field that holds one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum AppcastVersion {
    /// Use `sparkle:shortVersionString`, falling back to `sparkle:version`
    /// when an item has no short version (Sparkle's own display rule).
    #[default]
    ShortVersionString,
    /// Use `sparkle:version` only. Pick this when your `CFBundleVersion` is
    /// itself a semantic version.
    BundleVersion,
}

impl AppcastVersion {
    /// Resolve an item's version from its short and bundle version strings.
    ///
    /// Both values are parsed with [`parse_version_lenient`], so `1.4` reads
    /// as `1.4.0`. A present but unparsable value is an error; it does not fall
    /// back to the other field.
    ///
    /// # Errors
    /// Returns [`Error::Parse`] when the selected field is missing or is not a
    /// version this crate can order.
    pub fn resolve(
        self,
        short_version: Option<&str>,
        bundle_version: Option<&str>,
    ) -> Result<Version> {
        fn present(v: Option<&str>) -> Option<&str> {
            v.map(str::trim).filter(|s| !s.is_empty())
        }
        let raw = match self {
            Self::ShortVersionString => present(short_version).or(present(bundle_version)),
            Self::BundleVersion => present(bundle_version),
        };
        let raw = raw.ok_or_else(|| Error::Parse("appcast item has no version".into()))?;
        parse_version_lenient(raw)
    }
}

/// Reads releases from a Sparkle appcast.
///
/// The source picks the highest-versioned `<item>` that applies to the running
/// system, then the best `<enclosure>` in it for this platform (see
/// [`os`](Self::os)). Items are skipped when they:
///
/// - are `sparkle:informationalUpdate` items (nothing to install);
/// - are on a `sparkle:channel` that was not allowed with
///   [`channel`](Self::channel);
/// - list `arm64` in `sparkle:hardwareRequirements` and the machine cannot run
///   arm64 code;
/// - fall outside `sparkle:minimumSystemVersion` /
///   `sparkle:maximumSystemVersion` (checked on macOS, or wherever a
///   [`system_version`](Self::system_version) is set);
/// - have a version that is not a version this crate can order (see
///   [`AppcastVersion`]).
///
/// The enclosure's `sparkle:edSignature` becomes
/// [`Release::ed25519_signature`], so a release verifies against the app's
/// `SUPublicEDKey` once it is set as
/// [`EngineConfig::ed25519_public_key`](crate::EngineConfig::ed25519_public_key).
/// An optional `gpui-updater:sha256` enclosure attribute (namespace
/// `https://github.com/AprilNEA/gpui-updater`) supplies a checksum for the
/// [`Checksum`](crate::Verification::Checksum) and
/// [`Strict`](crate::Verification::Strict) policies.
///
/// Phased rollouts, critical updates, `sparkle:minimumAutoupdateVersion`, and
/// delta enclosures are Sparkle features; this source ignores them and always
/// offers the full archive.
///
/// ```no_run
/// use gpui_updater_core::{AppcastSource, EngineConfig, Verification};
/// # use semver::Version;
///
/// let source = AppcastSource::new("https://dl.example.com/appcast.xml").channel("beta");
/// let config = EngineConfig::new(Version::new(1, 2, 0))
///     .ed25519_public_key("pfIShU4dEXqPd5ObYNfDBiQWcXozk7estwzTnF9BamQ=")
///     .verification(Verification::Signature);
/// ```
#[derive(Debug, Clone)]
pub struct AppcastSource {
    url: String,
    os: OsMatcher,
    channels: Vec<String>,
    version_field: AppcastVersion,
    system_version: Option<Version>,
}

impl AppcastSource {
    /// Create a source for the appcast at `url`, matching enclosures for the
    /// platform this binary was built for.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            os: OsMatcher::for_current_platform(),
            channels: Vec::new(),
            version_field: AppcastVersion::default(),
            system_version: None,
        }
    }

    /// Accept only enclosures whose `sparkle:os` equals `os`
    /// (case-insensitive). An enclosure without `sparkle:os` counts as
    /// `macos`, as in Sparkle.
    ///
    /// By default the source matches the running platform: on macOS `macos`;
    /// on Windows WinSparkle's `windows-x64` / `windows-arm64` / `windows-x86`,
    /// then `windows`, then enclosures without `sparkle:os`; on Linux
    /// `linux-x64` / `linux-arm64`, then `linux`. Writing `sparkle:os` on
    /// every enclosure of a shared feed avoids any ambiguity.
    #[must_use]
    pub fn os(mut self, os: &str) -> Self {
        self.os = OsMatcher::exact(os);
        self
    }

    /// Also offer items on the `sparkle:channel` named `channel` (exact match).
    /// Items without a channel are always offered. Call repeatedly to allow
    /// several channels.
    #[must_use]
    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.channels.push(channel.into());
        self
    }

    /// Choose which field holds the version. Defaults to
    /// [`AppcastVersion::ShortVersionString`].
    #[must_use]
    pub fn version_field(mut self, field: AppcastVersion) -> Self {
        self.version_field = field;
        self
    }

    /// Compare `sparkle:minimumSystemVersion` / `sparkle:maximumSystemVersion`
    /// against `version` instead of the detected OS version. Without this, the
    /// check runs on macOS only (other systems' versions are not comparable).
    #[must_use]
    pub fn system_version(mut self, version: Version) -> Self {
        self.system_version = Some(version);
        self
    }

    /// The appcast URL this source fetches.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Pick the newest applicable release among `items`.
    fn select(&self, items: Vec<Item>, host: &HostFacts) -> Result<Release> {
        let mut best: Option<Release> = None;
        for item in items {
            let Some(candidate) = self.candidate(item, host) else {
                continue;
            };
            if best.as_ref().is_none_or(|b| candidate.version > b.version) {
                best = Some(candidate);
            }
        }
        best.ok_or(Error::NoMatchingAsset {
            target_os: std::env::consts::OS,
            target_arch: std::env::consts::ARCH,
        })
    }

    /// The release `item` offers on this host, or `None` if it does not apply.
    fn candidate(&self, mut item: Item, host: &HostFacts) -> Option<Release> {
        if item.informational {
            return None;
        }
        if let Some(channel) = &item.channel {
            if !self.channels.iter().any(|allowed| allowed == channel) {
                return None;
            }
        }
        if item.hardware_requirements.iter().any(|r| r == "arm64") && !host.supports_arm64 {
            return None;
        }
        if let Some(system) = &host.system_version {
            let bound =
                |v: &Option<String>| v.as_deref().and_then(|v| parse_version_lenient(v).ok());
            if bound(&item.minimum_system_version).is_some_and(|min| *system < min)
                || bound(&item.maximum_system_version).is_some_and(|max| *system > max)
            {
                return None;
            }
        }

        let index = item
            .enclosures
            .iter()
            .enumerate()
            .filter_map(|(i, e)| self.os.rank(e.os.as_deref()).map(|rank| (rank, i)))
            .min_by_key(|(rank, _)| *rank)?
            .1;
        let enclosure: Enclosure = item.enclosures.swap_remove(index);

        let short = item
            .short_version
            .as_deref()
            .or(enclosure.short_version.as_deref());
        let bundle = item
            .bundle_version
            .as_deref()
            .or(enclosure.bundle_version.as_deref());
        let version = match self.version_field.resolve(short, bundle) {
            Ok(version) => version,
            Err(e) => {
                tracing::debug!("skipping appcast item: {e}");
                return None;
            }
        };

        let mut release = Release::new(
            version,
            Asset::new(asset_name(&enclosure.url), enclosure.url, enclosure.length),
        );
        release.notes = item.description.or(item.release_notes_link);
        release.ed25519_signature = enclosure.ed_signature;
        release.sha256 = enclosure.sha256;
        Some(release)
    }
}

impl UpdateSource for AppcastSource {
    fn fetch_latest(&self) -> Result<Release> {
        let xml = http::get_string(&self.url, &[])?;
        let items = feed::parse(&xml)?;
        let needs_arch = items.iter().any(|i| !i.hardware_requirements.is_empty());
        let host = HostFacts {
            system_version: self
                .system_version
                .clone()
                .or_else(host::current_system_version),
            supports_arm64: needs_arch && host::supports_arm64(),
        };
        self.select(items, &host)
    }
}

/// A safe local file name for the download: the URL's last path segment.
fn asset_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or_default();
    if name.is_empty() || name == "." || name == ".." || name.contains('\\') {
        "update".to_string()
    } else {
        name.to_string()
    }
}
