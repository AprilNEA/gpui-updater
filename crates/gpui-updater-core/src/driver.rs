//! The contract for handing the whole update flow to an external updater.
//!
//! [`UpdateEngine`](crate::UpdateEngine) owns every step itself: it fetches
//! metadata, downloads, verifies, and installs. Some platforms already have a
//! mature updater that does all of that, and more: Sparkle on macOS installs
//! into `/Applications` for non-admin users, installs on quit, and applies
//! delta updates. An [`UpdateDriver`] wraps such an updater so the GPUI
//! `Updater` entity can present it with the same `UpdateStatus` and the same
//! `check` / `download_and_install` / `restart` calls.
//!
//! The driver reports what it is doing through [`DriverEvents`], a cheap,
//! thread-safe handle the driver receives in [`UpdateDriver::start`]. Events
//! are queued and applied on the GPUI foreground, so a driver may emit from any
//! thread and from inside its own callbacks without re-entering the app.
//!
//! This module does not depend on GPUI, so a driver crate (such as
//! `gpui-updater-sparkle`) builds and tests without it.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use semver::Version;

use crate::release::Release;
use crate::status::UpdateError;

/// An external updater that runs the whole check → download → install →
/// relaunch flow by itself.
///
/// Drivers are owned by the GPUI `Updater` entity, which calls every method on
/// the foreground (main) thread. They need not be `Send`: Sparkle objects, for
/// example, may only be touched from the main thread.
///
/// The `Any` supertrait lets the adapters hand the concrete driver back (their
/// `Updater::driver_mut`) for driver-specific settings.
pub trait UpdateDriver: Any {
    /// Start the driver. Called exactly once, before any other method.
    ///
    /// Keep `events` and report progress through it for as long as the driver
    /// lives.
    fn start(&mut self, events: DriverEvents);

    /// The user asked to check for updates now.
    fn check(&mut self);

    /// The user asked to install the update the driver last reported with
    /// [`DriverEvent::Found`].
    fn install(&mut self);

    /// The user asked to relaunch into the update the driver last reported with
    /// [`DriverEvent::ReadyToRelaunch`].
    fn relaunch(&mut self);

    /// The user asked to stop the download in progress. Report
    /// [`DriverEvent::Settled`] once it has stopped.
    ///
    /// The default does nothing: not every external updater can stop a
    /// download from outside its own interface.
    fn cancel(&mut self) {}
}

/// Something an [`UpdateDriver`] wants the app to know.
///
/// Each event maps onto the GPUI `UpdateStatus` of the same name, except
/// [`Settled`](Self::Settled), which tells the `Updater` that nothing is in
/// flight any more.
#[derive(Debug, Clone)]
#[non_exhaustive]
#[allow(
    clippy::large_enum_variant,
    reason = "events are rare and short-lived; boxing `Found` would only add API churn"
)]
pub enum DriverEvent {
    /// A check started.
    Checking,
    /// The check finished and the running version is the latest.
    UpToDate,
    /// The check found a newer release.
    Found(Release),
    /// The update is downloading. `total` is `None` when the size is unknown;
    /// drivers that cannot observe byte progress report
    /// `{ downloaded: 0, total: None }`.
    Downloading {
        /// Bytes downloaded so far.
        downloaded: u64,
        /// Expected total size in bytes, when known.
        total: Option<u64>,
    },
    /// The update is being verified, extracted, or installed.
    Installing,
    /// The update is staged; relaunching will run `version`.
    ReadyToRelaunch(Version),
    /// The current operation failed. The error's message is shown to the
    /// user; its kind tells the app what kind of failure it was.
    Failed(UpdateError),
    /// The driver finished its session (for example, the user dismissed an
    /// update prompt or cancelled a download). Nothing is in flight any more.
    Settled,
}

/// Where an [`UpdateDriver`] sends its [`DriverEvent`]s.
///
/// Cloning is cheap and the handle is `Send + Sync`, so it can move into
/// callbacks and background threads.
#[derive(Clone)]
pub struct DriverEvents {
    sink: Arc<dyn Fn(DriverEvent) + Send + Sync>,
}

impl DriverEvents {
    /// Wrap a function that receives every emitted event.
    ///
    /// The GPUI `Updater` builds its own handle; call this to test a driver or
    /// to drive one without GPUI.
    pub fn new(sink: impl Fn(DriverEvent) + Send + Sync + 'static) -> Self {
        Self {
            sink: Arc::new(sink),
        }
    }

    /// Report an event.
    pub fn emit(&self, event: DriverEvent) {
        (self.sink)(event);
    }
}

impl fmt::Debug for DriverEvents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverEvents").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn events_reach_the_sink_from_any_clone() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let events = {
            let seen = seen.clone();
            DriverEvents::new(move |event| seen.lock().unwrap().push(format!("{event:?}")))
        };
        let clone = events.clone();
        std::thread::spawn(move || clone.emit(DriverEvent::Checking))
            .join()
            .unwrap();
        events.emit(DriverEvent::Settled);
        assert_eq!(*seen.lock().unwrap(), ["Checking", "Settled"]);
    }
}
