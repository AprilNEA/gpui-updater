//! Where update metadata and release files live on a static host.
//!
//! The default layout is the one the README recommends for an object store
//! such as Cloudflare R2 or S3:
//!
//! ```text
//! channels/<channel>/latest.json          short cache: clients poll it
//! channels/<channel>/appcast-<arch>.xml   short cache: Sparkle polls it
//! releases/<tag>/<files>                  immutable
//! ```
//!
//! Every path is a template with `{channel}`, `{tag}`, `{version}`, and
//! `{arch}` placeholders, so any other layout works too.

use crate::util::join_url;

/// Paths and URLs of a static update host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FeedLayout {
    /// Public base URL, for example `https://updates.example.com`.
    pub base_url: String,
    /// Release channel, for example `stable`.
    pub channel: String,
    /// Directory of a channel's metadata. Default `channels/{channel}`.
    pub channel_dir: String,
    /// Static manifest file name. Default `latest.json`.
    pub manifest_name: String,
    /// Sparkle appcast file name. Default `appcast-{arch}.xml`.
    pub appcast_name: String,
    /// Directory of one release's files. Default `releases/{tag}`.
    pub release_dir: String,
}

impl FeedLayout {
    /// The default layout under `base_url` for `channel`.
    pub fn new(base_url: impl Into<String>, channel: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            channel: channel.into(),
            channel_dir: "channels/{channel}".into(),
            manifest_name: "latest.json".into(),
            appcast_name: "appcast-{arch}.xml".into(),
            release_dir: "releases/{tag}".into(),
        }
    }

    /// Replace the channel directory template.
    #[must_use]
    pub fn channel_dir(mut self, template: impl Into<String>) -> Self {
        self.channel_dir = template.into();
        self
    }

    /// Replace the manifest file name template.
    #[must_use]
    pub fn manifest_name(mut self, template: impl Into<String>) -> Self {
        self.manifest_name = template.into();
        self
    }

    /// Replace the appcast file name template; one feed for every
    /// architecture is a name without `{arch}`, such as `appcast.xml`.
    #[must_use]
    pub fn appcast_name(mut self, template: impl Into<String>) -> Self {
        self.appcast_name = template.into();
        self
    }

    /// Replace the release directory template.
    #[must_use]
    pub fn release_dir(mut self, template: impl Into<String>) -> Self {
        self.release_dir = template.into();
        self
    }

    fn fill(&self, template: &str, tag: &str, arch: &str) -> String {
        template
            .replace("{channel}", &self.channel)
            .replace("{tag}", tag)
            .replace("{version}", tag.strip_prefix('v').unwrap_or(tag))
            .replace("{arch}", arch)
    }

    /// Object key of the static manifest, for example
    /// `channels/stable/latest.json`.
    pub fn manifest_path(&self) -> String {
        join_path(
            &self.fill(&self.channel_dir, "", ""),
            &self.fill(&self.manifest_name, "", ""),
        )
    }

    /// Public URL of the static manifest.
    pub fn manifest_url(&self) -> String {
        join_url(&self.base_url, &self.manifest_path())
    }

    /// Object key of the appcast for `arch`.
    pub fn appcast_path(&self, arch: &str) -> String {
        join_path(
            &self.fill(&self.channel_dir, "", arch),
            &self.fill(&self.appcast_name, "", arch),
        )
    }

    /// Public URL of the appcast for `arch`, the value of `SUFeedURL`.
    pub fn appcast_url(&self, arch: &str) -> String {
        join_url(&self.base_url, &self.appcast_path(arch))
    }

    /// Object key prefix of the release `tag`, for example `releases/v1.2.3`.
    pub fn release_path(&self, tag: &str) -> String {
        self.fill(&self.release_dir, tag, "")
    }

    /// Public URL prefix of the release `tag`'s files.
    pub fn release_url(&self, tag: &str) -> String {
        join_url(&self.base_url, &self.release_path(tag))
    }

    /// Public URL of `file` in release `tag`.
    pub fn file_url(&self, tag: &str, file: &str) -> String {
        join_url(&self.release_url(tag), file)
    }
}

fn join_path(dir: &str, name: &str) -> String {
    let dir = dir.trim_matches('/');
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout() {
        let l = FeedLayout::new("https://updates.example.com/", "stable");
        assert_eq!(
            l.manifest_url(),
            "https://updates.example.com/channels/stable/latest.json"
        );
        assert_eq!(
            l.appcast_url("arm64"),
            "https://updates.example.com/channels/stable/appcast-arm64.xml"
        );
        assert_eq!(l.release_path("v1.2.3"), "releases/v1.2.3");
        assert_eq!(
            l.file_url("v1.2.3", "App.dmg"),
            "https://updates.example.com/releases/v1.2.3/App.dmg"
        );
    }

    #[test]
    fn custom_templates() {
        let l = FeedLayout::new("https://e", "beta")
            .channel_dir("")
            .manifest_name("{channel}.json")
            .appcast_name("{channel}-{arch}.xml")
            .release_dir("dl/{version}");
        assert_eq!(l.manifest_path(), "beta.json");
        assert_eq!(l.appcast_path("x86_64"), "beta-x86_64.xml");
        assert_eq!(l.release_path("v2.0.0"), "dl/2.0.0");
    }
}
