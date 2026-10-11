//! How the Sparkle driver is configured.

use std::path::PathBuf;

/// What happens once Sparkle has downloaded an update that it would install
/// when the app quits (Sparkle's `updater:willInstallUpdateOnQuit:` delegate
/// call). The `Updater` reports `UpdateStatus::Staged` either way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum StagedUpdatePolicy {
    /// Leave it to Sparkle: it installs on quit, or asks the user to relaunch
    /// the next time it shows its UI. `Updater::restart` opens that prompt.
    #[default]
    SparklePrompts,
    /// The app offers the relaunch itself (for example a "Restart to update"
    /// button). Sparkle shows no prompt for the staged update and still
    /// installs it on quit; `Updater::restart` installs and relaunches
    /// immediately.
    HostRelaunches,
}

/// Options for [`SparkleDriver::load`](crate::SparkleDriver::load).
///
/// Everything here is optional; by default Sparkle reads its feed and key
/// from `Info.plist`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SparkleOptions {
    /// Feed URL to use instead of `Info.plist`'s `SUFeedURL`.
    pub feed_url: Option<String>,
    /// `sparkle:channel` names to offer in addition to the default channel.
    pub channels: Vec<String>,
    /// Location of `Sparkle.framework`, if not
    /// `<App>.app/Contents/Frameworks/Sparkle.framework`.
    pub framework_path: Option<PathBuf>,
    /// See [`StagedUpdatePolicy`].
    pub staged_updates: StagedUpdatePolicy,
}

impl SparkleOptions {
    /// Default options: everything from `Info.plist`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use `url` as the feed instead of `SUFeedURL`.
    #[must_use]
    pub fn feed_url(mut self, url: impl Into<String>) -> Self {
        self.feed_url = Some(url.into());
        self
    }

    /// Also offer updates on the `sparkle:channel` named `channel`. Call
    /// repeatedly to allow several channels.
    #[must_use]
    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.channels.push(channel.into());
        self
    }

    /// Load Sparkle from `path` instead of the bundle's `Frameworks` folder.
    #[must_use]
    pub fn framework_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.framework_path = Some(path.into());
        self
    }

    /// Choose who offers the relaunch into a staged update.
    #[must_use]
    pub fn staged_updates(mut self, policy: StagedUpdatePolicy) -> Self {
        self.staged_updates = policy;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{SparkleOptions, StagedUpdatePolicy};

    #[test]
    fn builder_collects_options() {
        let options = SparkleOptions::new()
            .feed_url("https://example.test/appcast.xml")
            .channel("beta")
            .channel("nightly")
            .staged_updates(StagedUpdatePolicy::HostRelaunches);
        assert_eq!(
            options.feed_url.as_deref(),
            Some("https://example.test/appcast.xml")
        );
        assert_eq!(options.channels, ["beta", "nightly"]);
        assert!(options.framework_path.is_none());
        assert_eq!(options.staged_updates, StagedUpdatePolicy::HostRelaunches);
    }
}
