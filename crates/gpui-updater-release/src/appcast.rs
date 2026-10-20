//! Write Sparkle appcasts, including `<sparkle:deltas>`.
//!
//! The output is what Sparkle 2 reads, and what
//! [`AppcastSource`](gpui_updater_core::AppcastSource) reads too. Every element
//! of an item is optional except the version and the enclosure, so the same
//! writer covers a one-item feed regenerated per release and a long history.
//!
//! ```no_run
//! use gpui_updater_release::{Appcast, AppcastItem, Enclosure, ReleaseNotes, SparkleKey};
//!
//! let key = SparkleKey::from_base64(&std::env::var("SPARKLE_PRIVATE_KEY").unwrap())?;
//! let dmg = std::path::Path::new("dist/App-v1.2.3-macos-arm64.dmg");
//! let enclosure = Enclosure::for_file(dmg, "https://dl.example.com/v1.2.3/App-v1.2.3-macos-arm64.dmg", Some(&key))?;
//! let item = AppcastItem::new("1.2.3", enclosure)
//!     .title("App 1.2.3")
//!     .minimum_system_version("13.0")
//!     .notes(ReleaseNotes::Markdown("- Faster".into()));
//! let feed = Appcast::new("App", "https://dl.example.com/appcast.xml").item(item);
//! feed.write("out/appcast.xml".as_ref())?;
//! # Ok::<_, gpui_updater_release::Error>(())
//! ```

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

use crate::error::{Error, Result};
use crate::sign::SparkleKey;

/// Sparkle's XML namespace.
pub const SPARKLE_NAMESPACE: &str = "http://www.andymatuschak.org/xml-namespaces/sparkle";

/// A whole appcast: one RSS channel with its items.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Appcast {
    /// Channel title, usually the app name.
    pub title: String,
    /// Channel link, usually the feed's own URL.
    pub link: String,
    /// Channel description.
    pub description: Option<String>,
    /// Channel language, for example `en`.
    pub language: Option<String>,
    /// Items, newest first.
    pub items: Vec<AppcastItem>,
}

impl Appcast {
    /// An empty feed.
    pub fn new(title: impl Into<String>, link: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            link: link.into(),
            description: None,
            language: None,
            items: Vec::new(),
        }
    }

    /// Set the channel description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the channel language.
    #[must_use]
    pub fn language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    /// Append an item.
    #[must_use]
    pub fn item(mut self, item: AppcastItem) -> Self {
        self.items.push(item);
        self
    }

    /// The feed as XML.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
        let _ = writeln!(
            out,
            "<rss version=\"2.0\" xmlns:sparkle=\"{SPARKLE_NAMESPACE}\">"
        );
        out.push_str("  <channel>\n");
        element(&mut out, 4, "title", &self.title);
        element(&mut out, 4, "link", &self.link);
        if let Some(description) = &self.description {
            element(&mut out, 4, "description", description);
        }
        if let Some(language) = &self.language {
            element(&mut out, 4, "language", language);
        }
        for item in &self.items {
            item.render(&mut out);
        }
        out.push_str("  </channel>\n</rss>\n");
        out
    }

    /// Write the feed, creating parent directories.
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(Error::io("create", parent))?;
        }
        fs::write(path, self.render()).map_err(Error::io("write", path))
    }
}

/// Release notes embedded in the item's `<description>`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReleaseNotes {
    /// Markdown (`sparkle:format="markdown"`, Sparkle 2.4 and later).
    Markdown(String),
    /// HTML, Sparkle's default.
    Html(String),
    /// Plain text (`sparkle:format="plain-text"`).
    PlainText(String),
}

/// When Sparkle treats the update as critical.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Critical {
    /// For everyone.
    Always,
    /// Only for installed versions below this `sparkle:version`.
    Below(String),
}

/// When Sparkle shows the update as informational only (no download).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Informational {
    /// For everyone.
    Always,
    /// Only for installed versions below any of these.
    Below(Vec<String>),
}

/// One release in the feed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AppcastItem {
    /// `<title>`.
    pub title: Option<String>,
    /// `<pubDate>`, RFC 2822.
    pub pub_date: Option<String>,
    /// `<sparkle:version>`: the bundle's `CFBundleVersion`, which Sparkle
    /// compares with the installed app's.
    pub version: String,
    /// `<sparkle:shortVersionString>`: the version shown to people.
    pub short_version: Option<String>,
    /// `<sparkle:channel>`; items without one are on the default channel.
    pub channel: Option<String>,
    /// `<sparkle:minimumSystemVersion>`.
    pub minimum_system_version: Option<String>,
    /// `<sparkle:maximumSystemVersion>`.
    pub maximum_system_version: Option<String>,
    /// `<sparkle:hardwareRequirements>`, for example `arm64`.
    pub hardware_requirements: Option<String>,
    /// `<sparkle:minimumAutoupdateVersion>`: below it, the update is not
    /// installed silently.
    pub minimum_autoupdate_version: Option<String>,
    /// `<sparkle:minimumUpdateVersion>`: below it, the update is not offered.
    pub minimum_update_version: Option<String>,
    /// `<sparkle:ignoreSkippedUpgradesBelowVersion>`.
    pub ignore_skipped_upgrades_below_version: Option<String>,
    /// `<sparkle:criticalUpdate>`.
    pub critical: Option<Critical>,
    /// `<sparkle:informationalUpdate>`.
    pub informational: Option<Informational>,
    /// `<sparkle:phasedRolloutInterval>` in seconds.
    pub phased_rollout_interval: Option<u64>,
    /// `<link>`: the release's web page.
    pub link: Option<String>,
    /// `<sparkle:releaseNotesLink>`: notes shown in the update window.
    pub release_notes_link: Option<String>,
    /// `<sparkle:fullReleaseNotesLink>`: the full history, opened in a browser.
    pub full_release_notes_link: Option<String>,
    /// `<description>`: notes embedded in the feed.
    pub notes: Option<ReleaseNotes>,
    /// The full download.
    pub enclosure: Enclosure,
    /// Deltas from older versions, under `<sparkle:deltas>`.
    pub deltas: Vec<DeltaEnclosure>,
    /// Extra XML placed verbatim at the end of the item, for elements this
    /// type does not model. Use the `sparkle:` prefix for Sparkle elements.
    pub extra_xml: Vec<String>,
}

impl AppcastItem {
    /// An item offering `enclosure` as `version`, published now.
    pub fn new(version: impl Into<String>, enclosure: Enclosure) -> Self {
        Self {
            title: None,
            pub_date: rfc2822(OffsetDateTime::now_utc()).ok(),
            version: version.into(),
            short_version: None,
            channel: None,
            minimum_system_version: None,
            maximum_system_version: None,
            hardware_requirements: None,
            minimum_autoupdate_version: None,
            minimum_update_version: None,
            ignore_skipped_upgrades_below_version: None,
            critical: None,
            informational: None,
            phased_rollout_interval: None,
            link: None,
            release_notes_link: None,
            full_release_notes_link: None,
            notes: None,
            enclosure,
            deltas: Vec::new(),
            extra_xml: Vec::new(),
        }
    }

    /// Set `<title>`.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set `<pubDate>` (RFC 2822); `None` leaves it out.
    #[must_use]
    pub fn pub_date(mut self, date: Option<String>) -> Self {
        self.pub_date = date;
        self
    }

    /// Set `<sparkle:shortVersionString>`.
    #[must_use]
    pub fn short_version(mut self, version: impl Into<String>) -> Self {
        self.short_version = Some(version.into());
        self
    }

    /// Set `<sparkle:channel>`.
    #[must_use]
    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = Some(channel.into());
        self
    }

    /// Set `<sparkle:minimumSystemVersion>`.
    #[must_use]
    pub fn minimum_system_version(mut self, version: impl Into<String>) -> Self {
        self.minimum_system_version = Some(version.into());
        self
    }

    /// Set `<sparkle:maximumSystemVersion>`.
    #[must_use]
    pub fn maximum_system_version(mut self, version: impl Into<String>) -> Self {
        self.maximum_system_version = Some(version.into());
        self
    }

    /// Set `<sparkle:hardwareRequirements>`.
    #[must_use]
    pub fn hardware_requirements(mut self, requirements: impl Into<String>) -> Self {
        self.hardware_requirements = Some(requirements.into());
        self
    }

    /// Set `<sparkle:minimumAutoupdateVersion>`.
    #[must_use]
    pub fn minimum_autoupdate_version(mut self, version: impl Into<String>) -> Self {
        self.minimum_autoupdate_version = Some(version.into());
        self
    }

    /// Set `<sparkle:minimumUpdateVersion>`.
    #[must_use]
    pub fn minimum_update_version(mut self, version: impl Into<String>) -> Self {
        self.minimum_update_version = Some(version.into());
        self
    }

    /// Set `<sparkle:ignoreSkippedUpgradesBelowVersion>`.
    #[must_use]
    pub fn ignore_skipped_upgrades_below_version(mut self, version: impl Into<String>) -> Self {
        self.ignore_skipped_upgrades_below_version = Some(version.into());
        self
    }

    /// Mark the update critical.
    #[must_use]
    pub fn critical(mut self, critical: Critical) -> Self {
        self.critical = Some(critical);
        self
    }

    /// Mark the update informational.
    #[must_use]
    pub fn informational(mut self, informational: Informational) -> Self {
        self.informational = Some(informational);
        self
    }

    /// Roll the update out in phases of `seconds`.
    #[must_use]
    pub fn phased_rollout_interval(mut self, seconds: u64) -> Self {
        self.phased_rollout_interval = Some(seconds);
        self
    }

    /// Set `<link>`.
    #[must_use]
    pub fn link(mut self, url: impl Into<String>) -> Self {
        self.link = Some(url.into());
        self
    }

    /// Set `<sparkle:releaseNotesLink>`.
    #[must_use]
    pub fn release_notes_link(mut self, url: impl Into<String>) -> Self {
        self.release_notes_link = Some(url.into());
        self
    }

    /// Set `<sparkle:fullReleaseNotesLink>`.
    #[must_use]
    pub fn full_release_notes_link(mut self, url: impl Into<String>) -> Self {
        self.full_release_notes_link = Some(url.into());
        self
    }

    /// Embed release notes.
    #[must_use]
    pub fn notes(mut self, notes: ReleaseNotes) -> Self {
        self.notes = Some(notes);
        self
    }

    /// Add a delta.
    #[must_use]
    pub fn delta(mut self, delta: DeltaEnclosure) -> Self {
        self.deltas.push(delta);
        self
    }

    /// Append raw XML to the item.
    #[must_use]
    pub fn extra_xml(mut self, xml: impl Into<String>) -> Self {
        self.extra_xml.push(xml.into());
        self
    }

    fn render(&self, out: &mut String) {
        out.push_str("    <item>\n");
        optional(out, "title", self.title.as_ref());
        optional(out, "pubDate", self.pub_date.as_ref());
        element(out, 6, "sparkle:version", &self.version);
        optional(
            out,
            "sparkle:shortVersionString",
            self.short_version.as_ref(),
        );
        optional(out, "sparkle:channel", self.channel.as_ref());
        self.render_requirements(out);
        self.render_policy(out);
        self.render_links_and_notes(out);
        self.enclosure.render(out);
        if !self.deltas.is_empty() {
            out.push_str("      <sparkle:deltas>\n");
            for delta in &self.deltas {
                delta.render(out);
            }
            out.push_str("      </sparkle:deltas>\n");
        }
        for xml in &self.extra_xml {
            let _ = writeln!(out, "      {xml}");
        }
        out.push_str("    </item>\n");
    }

    /// The system and installed-version requirements.
    fn render_requirements(&self, out: &mut String) {
        optional(
            out,
            "sparkle:minimumSystemVersion",
            self.minimum_system_version.as_ref(),
        );
        optional(
            out,
            "sparkle:maximumSystemVersion",
            self.maximum_system_version.as_ref(),
        );
        optional(
            out,
            "sparkle:hardwareRequirements",
            self.hardware_requirements.as_ref(),
        );
        optional(
            out,
            "sparkle:minimumAutoupdateVersion",
            self.minimum_autoupdate_version.as_ref(),
        );
        optional(
            out,
            "sparkle:minimumUpdateVersion",
            self.minimum_update_version.as_ref(),
        );
        optional(
            out,
            "sparkle:ignoreSkippedUpgradesBelowVersion",
            self.ignore_skipped_upgrades_below_version.as_ref(),
        );
    }

    /// Critical, informational, and phased-rollout settings.
    fn render_policy(&self, out: &mut String) {
        match &self.critical {
            Some(Critical::Always) => out.push_str("      <sparkle:criticalUpdate/>\n"),
            Some(Critical::Below(v)) => {
                let _ = writeln!(
                    out,
                    "      <sparkle:criticalUpdate sparkle:version=\"{}\"/>",
                    escape(v)
                );
            }
            None => {}
        }
        match &self.informational {
            Some(Informational::Always) => out.push_str("      <sparkle:informationalUpdate/>\n"),
            Some(Informational::Below(versions)) => {
                out.push_str("      <sparkle:informationalUpdate>\n");
                for v in versions {
                    element(out, 8, "sparkle:belowVersion", v);
                }
                out.push_str("      </sparkle:informationalUpdate>\n");
            }
            None => {}
        }
        if let Some(seconds) = self.phased_rollout_interval {
            element(
                out,
                6,
                "sparkle:phasedRolloutInterval",
                &seconds.to_string(),
            );
        }
    }

    /// The web links and the inline release notes.
    fn render_links_and_notes(&self, out: &mut String) {
        optional(out, "link", self.link.as_ref());
        optional(
            out,
            "sparkle:releaseNotesLink",
            self.release_notes_link.as_ref(),
        );
        optional(
            out,
            "sparkle:fullReleaseNotesLink",
            self.full_release_notes_link.as_ref(),
        );
        if let Some(notes) = &self.notes {
            let (format, text) = match notes {
                ReleaseNotes::Markdown(t) => (Some("markdown"), t),
                ReleaseNotes::Html(t) => (None, t),
                ReleaseNotes::PlainText(t) => (Some("plain-text"), t),
            };
            let format = format.map_or_else(String::new, |f| format!(" sparkle:format=\"{f}\""));
            let _ = writeln!(
                out,
                "      <description{format}>{}</description>",
                cdata(text)
            );
        }
    }
}

/// Write `<name>value</name>` at item depth when `value` is present.
fn optional(out: &mut String, name: &str, value: Option<&String>) {
    if let Some(value) = value {
        element(out, 6, name, value);
    }
}

/// The full download of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Enclosure {
    /// Download URL.
    pub url: String,
    /// Size in bytes.
    pub length: u64,
    /// MIME type.
    pub mime_type: String,
    /// `sparkle:edSignature`.
    pub ed_signature: Option<String>,
    /// `sparkle:os`, for example `macos`.
    pub os: Option<String>,
    /// `sparkle:installationType`, for example `package`.
    pub installation_type: Option<String>,
}

impl Enclosure {
    /// An enclosure with a known size and no signature.
    pub fn new(url: impl Into<String>, length: u64, mime_type: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            length,
            mime_type: mime_type.into(),
            ed_signature: None,
            os: None,
            installation_type: None,
        }
    }

    /// An enclosure for a file on disk, signed with `key` when given. The
    /// MIME type follows the extension (`.dmg`, `.zip`, `.pkg`, `.tar.*`).
    pub fn for_file(path: &Path, url: impl Into<String>, key: Option<&SparkleKey>) -> Result<Self> {
        let length = fs::metadata(path).map_err(Error::io("stat", path))?.len();
        let mut enclosure = Self::new(url, length, mime_for(path));
        enclosure.ed_signature = key.map(|key| key.sign_file(path)).transpose()?;
        Ok(enclosure)
    }

    /// Set `sparkle:os`.
    #[must_use]
    pub fn os(mut self, os: impl Into<String>) -> Self {
        self.os = Some(os.into());
        self
    }

    /// Set `sparkle:installationType`.
    #[must_use]
    pub fn installation_type(mut self, kind: impl Into<String>) -> Self {
        self.installation_type = Some(kind.into());
        self
    }

    fn render(&self, out: &mut String) {
        let _ = write!(
            out,
            "      <enclosure url=\"{}\" length=\"{}\" type=\"{}\"",
            escape(&self.url),
            self.length,
            escape(&self.mime_type)
        );
        attribute(out, "sparkle:os", self.os.as_ref());
        attribute(
            out,
            "sparkle:installationType",
            self.installation_type.as_ref(),
        );
        attribute(out, "sparkle:edSignature", self.ed_signature.as_ref());
        out.push_str("/>\n");
    }
}

/// One delta under `<sparkle:deltas>`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeltaEnclosure {
    /// Download URL.
    pub url: String,
    /// `sparkle:deltaFrom`: the `CFBundleVersion` the delta applies to.
    pub delta_from: String,
    /// Size in bytes.
    pub length: u64,
    /// `sparkle:edSignature`.
    pub ed_signature: Option<String>,
    /// `sparkle:deltaFromSparkleExecutableSize`: size of the old app's Sparkle
    /// executable. Sparkle 2.3+ skips the delta, without downloading it, when
    /// the installed app's differs.
    pub sparkle_executable_size: Option<u64>,
    /// `sparkle:deltaFromSparkleLocales`: some of the old app's Sparkle
    /// localizations, so a stripped installation skips the delta.
    pub sparkle_locales: Option<String>,
}

impl DeltaEnclosure {
    /// A delta with no signature or old-app attributes.
    pub fn new(url: impl Into<String>, delta_from: impl Into<String>, length: u64) -> Self {
        Self {
            url: url.into(),
            delta_from: delta_from.into(),
            length,
            ed_signature: None,
            sparkle_executable_size: None,
            sparkle_locales: None,
        }
    }

    /// A delta file on disk, signed with `key` when given.
    pub fn for_file(
        path: &Path,
        url: impl Into<String>,
        delta_from: impl Into<String>,
        key: Option<&SparkleKey>,
    ) -> Result<Self> {
        let length = fs::metadata(path).map_err(Error::io("stat", path))?.len();
        let mut delta = Self::new(url, delta_from, length);
        delta.ed_signature = key.map(|key| key.sign_file(path)).transpose()?;
        Ok(delta)
    }

    /// Set the old app's Sparkle attributes.
    #[must_use]
    pub fn old_sparkle(mut self, executable_size: Option<u64>, locales: Option<String>) -> Self {
        self.sparkle_executable_size = executable_size;
        self.sparkle_locales = locales;
        self
    }

    fn render(&self, out: &mut String) {
        let _ = write!(
            out,
            "        <enclosure url=\"{}\" sparkle:deltaFrom=\"{}\" length=\"{}\" type=\"application/octet-stream\"",
            escape(&self.url),
            escape(&self.delta_from),
            self.length,
        );
        if let Some(size) = self.sparkle_executable_size {
            let _ = write!(out, " sparkle:deltaFromSparkleExecutableSize=\"{size}\"");
        }
        attribute(
            out,
            "sparkle:deltaFromSparkleLocales",
            self.sparkle_locales.as_ref(),
        );
        attribute(out, "sparkle:edSignature", self.ed_signature.as_ref());
        out.push_str("/>\n");
    }
}

/// Fail unless Sparkle orders `version` correctly against plain releases.
///
/// Sparkle's `SUStandardVersionComparator` stops at the first `-`, so it reads
/// `1.2.0-rc.1` as `1.2.0`: whoever installed the pre-release would never be
/// offered the final 1.2.0. This accepts only dot-separated numbers, with at
/// least `min_parts` and at most `max_parts` of them.
pub fn ensure_sparkle_version(version: &str, min_parts: usize, max_parts: usize) -> Result<()> {
    let parts: Vec<_> = version.split('.').collect();
    let numeric = parts
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    if numeric && (min_parts..=max_parts).contains(&parts.len()) {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "Sparkle would misorder `{version}`: use {min_parts} to {max_parts} dot-separated numbers"
        )))
    }
}

/// `at` as RFC 2822, the format of `<pubDate>`.
pub fn rfc2822(at: OffsetDateTime) -> Result<String> {
    at.format(&Rfc2822)
        .map_err(|e| Error::invalid(format!("format a date: {e}")))
}

#[allow(clippy::case_sensitive_file_extension_comparisons)] // `name` is lowercased first.
fn mime_for(path: &Path) -> &'static str {
    let name = path.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".dmg") {
        "application/x-apple-diskimage"
    } else if name.ends_with(".zip") {
        "application/zip"
    } else if name.ends_with(".pkg") || name.ends_with(".mpkg") {
        "application/x-xar"
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        "application/gzip"
    } else if name.ends_with(".tar.xz") {
        "application/x-xz"
    } else if name.ends_with(".tar.bz2") {
        "application/x-bzip2"
    } else {
        "application/octet-stream"
    }
}

fn element(out: &mut String, indent: usize, name: &str, text: &str) {
    let _ = writeln!(out, "{:indent$}<{name}>{}</{name}>", "", escape(text));
}

fn attribute(out: &mut String, name: &str, value: Option<&String>) {
    if let Some(value) = value {
        let _ = write!(out, " {name}=\"{}\"", escape(value));
    }
}

/// `text` in a CDATA section, splitting any `]]>` it contains.
fn cdata(text: &str) -> String {
    format!("<![CDATA[{}]]>", text.replace("]]>", "]]]]><![CDATA[>"))
}

/// `text` escaped for an XML attribute value or element text.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            ch => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests;
