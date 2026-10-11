//! [Sparkle] update driver for the `gpui-updater` and `gpui-updater-pre` GPUI
//! adapters.
//!
//! On macOS, Sparkle is the established updater: it installs into
//! `/Applications` for non-admin users, installs on quit, applies delta
//! updates, and ships a familiar update UI. This crate wraps Sparkle 2 as an
//! [`UpdateDriver`], so a GPUI app can hand its whole update flow to Sparkle
//! while views keep reading the same `UpdateStatus` from the adapter's
//! `Updater` (`gpui_updater::Updater` or `gpui_updater_pre::Updater`).
//!
//! ```ignore
//! // With `gpui-updater-pre`, import from `gpui_updater_pre` instead.
//! use gpui_updater::{AppcastSource, EngineConfig, Updater};
//! use gpui_updater_sparkle::{SparkleDriver, SparkleOptions};
//!
//! let updater = cx.new(|cx| match SparkleDriver::load(SparkleOptions::new()) {
//!     // Running from a bundle that embeds Sparkle.framework.
//!     Ok(Some(sparkle)) => Updater::with_driver(sparkle, cx),
//!     // Unbundled `cargo run`, other platforms, or a misconfigured bundle:
//!     // fall back to the built-in engine on the same appcast.
//!     other => {
//!         if let Err(e) = other {
//!             tracing::warn!("Sparkle unavailable: {e}");
//!         }
//!         Updater::new(AppcastSource::new(FEED_URL), EngineConfig::new(version), cx)
//!     }
//! });
//! ```
//!
//! # Bundle requirements
//!
//! Sparkle is loaded at run time from
//! `<App>.app/Contents/Frameworks/Sparkle.framework`, so building and testing
//! never need the framework. To ship it, the app bundle must:
//!
//! - embed Sparkle 2's `Sparkle.framework` in `Contents/Frameworks` and sign
//!   it (and its XPC services, `Autoupdate`, and `Updater.app`) with the app's
//!   identity before notarizing;
//! - set `CFBundleIdentifier`, `CFBundleVersion`, and
//!   `CFBundleShortVersionString` in `Info.plist`;
//! - set `SUPublicEDKey` (from Sparkle's `generate_keys`) and `SUFeedURL` in
//!   `Info.plist`, or pass the feed with [`SparkleOptions::feed_url`].
//!
//! [`SparkleDriver`] is available on every platform so call sites need no
//! `cfg`, but [`SparkleDriver::load`] only ever returns a driver on macOS.
//!
//! [Sparkle]: https://sparkle-project.org

// Panicking on failure is the point of a test assertion.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod error;
mod options;
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "only the macOS driver reports Sparkle errors")
)]
mod outcome;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(target_os = "macos"))]
mod unsupported;
#[cfg(not(target_os = "macos"))]
use unsupported as imp;

use std::fmt;

use gpui_updater_core::{DriverEvents, UpdateDriver};

pub use error::Error;
pub use options::{SparkleOptions, StagedUpdatePolicy};

/// An [`UpdateDriver`] backed by Sparkle 2.
///
/// Build it with [`load`](Self::load) on the main thread and hand it to
/// `gpui_updater::Updater::with_driver`. The `Updater` then maps its calls
/// onto Sparkle:
///
/// | `Updater` call         | Sparkle                                                                              |
/// | ---------------------- | ------------------------------------------------------------------------------------ |
/// | `check`                | `-[SPUUpdater checkForUpdates]` (shows Sparkle's UI)                                 |
/// | `download_and_install` | `-[SPUUpdater checkForUpdates]`, which brings the found update forward               |
/// | `restart`              | the immediate-install block under [`StagedUpdatePolicy::HostRelaunches`], else `checkForUpdates` |
///
/// Sparkle reports what it is doing through its updater delegate, which this
/// driver turns into `UpdateStatus` changes. Sparkle does not expose byte
/// progress to delegates, so downloads report `Downloading { downloaded: 0,
/// total: None }`; Sparkle's own window shows the progress bar.
pub struct SparkleDriver {
    inner: imp::Driver,
}

impl SparkleDriver {
    /// Load the Sparkle.framework embedded in the running app bundle.
    ///
    /// Returns `Ok(None)` when Sparkle does not apply: on platforms other than
    /// macOS, when the process is not running from a `.app` bundle (e.g.
    /// `cargo run`), or when the bundle does not embed `Sparkle.framework`.
    /// Fall back to `gpui_updater::Updater::new` in that case.
    ///
    /// # Errors
    /// - [`Error::NotMainThread`] when called off the main thread.
    /// - [`Error::MissingInfoKey`] when the bundle embeds Sparkle but its
    ///   `Info.plist` lacks `SUPublicEDKey`, or lacks `SUFeedURL` and no
    ///   [`feed_url`](SparkleOptions::feed_url) was given.
    /// - [`Error::Load`] when the framework fails to load or is not Sparkle 2.
    pub fn load(options: SparkleOptions) -> Result<Option<Self>, Error> {
        Ok(imp::Driver::load(options)?.map(|inner| Self { inner }))
    }

    /// Whether Sparkle can start a user-initiated check right now (it cannot
    /// while another update session is in progress). `false` before the
    /// driver is started.
    pub fn can_check_for_updates(&self) -> bool {
        self.inner.can_check_for_updates()
    }

    /// Check without showing UI unless an update is found, like Sparkle's
    /// scheduled checks. Does nothing while a session is in progress.
    pub fn check_in_background(&self) {
        self.inner.check_in_background();
    }

    /// Ask the feed whether an update exists without showing any Sparkle UI.
    /// The answer arrives as `UpdateStatus::Available` or
    /// `UpdateStatus::UpToDate`; call `Updater::download_and_install` to let
    /// Sparkle present the update.
    pub fn check_for_update_information(&self) {
        self.inner.check_for_update_information();
    }

    /// Whether Sparkle checks for updates on its own schedule. `None` before
    /// the driver is started.
    pub fn automatically_checks_for_updates(&self) -> Option<bool> {
        self.inner.automatically_checks_for_updates()
    }

    /// Turn Sparkle's scheduled checks on or off. Sparkle persists the choice
    /// in the app's user defaults, so only call this when the user changes
    /// the setting.
    pub fn set_automatically_checks_for_updates(&self, enabled: bool) {
        self.inner.set_automatically_checks_for_updates(enabled);
    }

    /// Whether Sparkle downloads (and stages) updates without asking. `None`
    /// before the driver is started.
    pub fn automatically_downloads_updates(&self) -> Option<bool> {
        self.inner.automatically_downloads_updates()
    }

    /// Turn silent downloads on or off. Sparkle persists the choice in the
    /// app's user defaults, so only call this when the user changes the
    /// setting.
    pub fn set_automatically_downloads_updates(&self, enabled: bool) {
        self.inner.set_automatically_downloads_updates(enabled);
    }
}

impl UpdateDriver for SparkleDriver {
    fn start(&mut self, events: DriverEvents) {
        self.inner.start(events);
    }

    fn check(&mut self) {
        self.inner.check();
    }

    fn install(&mut self) {
        self.inner.install();
    }

    fn relaunch(&mut self) {
        self.inner.relaunch();
    }
}

impl fmt::Debug for SparkleDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SparkleDriver")
            .field("inner", &self.inner)
            .finish()
    }
}
