//! GPUI integration: an observable `Entity<Updater>` that drives the blocking
//! [`UpdateEngine`] on the app's executors, or an external [`UpdateDriver`].
//!
//! The update runs on the background executor so the UI thread never blocks;
//! state transitions land back on the foreground via the entity and call
//! [`Context::notify`], so any view can `cx.observe(&updater, …)` to re-render.
//! When an update is staged, `App::set_restart_path` is set so the next
//! `cx.restart()` launches the new version.
//!
//! With [`Updater::with_driver`], an external [`UpdateDriver`] (such as
//! Sparkle, via `gpui-updater-sparkle`) runs the whole flow instead and
//! reports [`DriverEvent`]s, which the entity maps onto the same
//! [`UpdateStatus`]. Views do not need to know which backend is in use.
//!
//! ```no_run
//! # use gpui::{AppContext as _, Context, Entity};
//! # use gpui_updater::{EngineConfig, GitHubSource, UpdateStatus, Updater};
//! # use gpui_updater::Version;
//! fn build(cx: &mut Context<()>) {
//!     let source = GitHubSource::new("AprilNEA", "OpenLogi")
//!         .asset_contains("macos")
//!         .asset_contains(".dmg")
//!         .with_checksums("SHA256SUMS");
//!     let version = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
//!     let updater: Entity<Updater> =
//!         cx.new(|cx| Updater::new(source, EngineConfig::new(version), cx));
//!
//!     // Manual, opt-in check (no background polling):
//!     updater.update(cx, |u, cx| u.check(cx));
//! }
//! ```

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{Context, Task};

pub use gpui_updater_core::*;

type BoxedEngine = UpdateEngine<Box<dyn UpdateSource>>;

/// A GPUI entity that checks for, downloads, and installs updates.
///
/// Construct it with [`cx.new`](gpui::AppContext::new) and hold the resulting
/// `Entity<Updater>`. All work is triggered explicitly — there is no background
/// polling — which suits a privacy-conscious "Check for updates" button. To
/// poll, call [`check`](Self::check) yourself on a timer.
///
/// Build it with [`Updater::new`] to run the built-in [`UpdateEngine`], or
/// with [`Updater::with_driver`] to delegate to an external
/// [`UpdateDriver`]. A driver may also check on its own schedule (Sparkle
/// does when automatic checks are on).
///
/// Dropping the entity cancels a running download (as
/// [`cancel`](Self::cancel) does), but not an install that has started. It
/// does not roll back an installation.
pub struct Updater {
    status: UpdateStatus,
    available: Option<Release>,
    backend: Backend,
    task: Option<Task<()>>,
    /// Stops the download in progress, if any.
    cancel: Option<CancelToken>,
}

enum Backend {
    Engine(Arc<BoxedEngine>),
    Driver {
        driver: Box<dyn UpdateDriver>,
        /// Applies queued driver events on the foreground; dropped with the
        /// `Updater`, which stops the pump.
        _events: Task<()>,
    },
}

impl Updater {
    /// Create an updater from a [`UpdateSource`] and [`EngineConfig`].
    pub fn new<S: UpdateSource>(source: S, config: EngineConfig, _cx: &mut Context<Self>) -> Self {
        let engine = UpdateEngine::new(Box::new(source) as Box<dyn UpdateSource>, config);
        Self {
            status: UpdateStatus::Idle,
            available: None,
            backend: Backend::Engine(Arc::new(engine)),
            task: None,
            cancel: None,
        }
    }

    /// Create an updater that delegates the whole flow to `driver`.
    ///
    /// The driver is started immediately; its [`DriverEvent`]s update
    /// [`status`](Self::status) on the foreground. [`check`](Self::check),
    /// [`download_and_install`](Self::download_and_install) and
    /// [`restart`](Self::restart) forward to the driver.
    pub fn with_driver<D: UpdateDriver>(mut driver: D, cx: &mut Context<Self>) -> Self {
        let (tx, mut rx) = mpsc::unbounded::<DriverEvent>();
        // Sending fails only once the pump below is gone, i.e. the `Updater`
        // was dropped. Nobody is left to tell, so the event is discarded.
        driver.start(DriverEvents::new(move |event| {
            tx.unbounded_send(event).ok();
        }));
        let events = cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                if this
                    .update(cx, |this, cx| this.apply_driver_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            status: UpdateStatus::Idle,
            available: None,
            backend: Backend::Driver {
                driver: Box::new(driver),
                _events: events,
            },
            task: None,
            cancel: None,
        }
    }

    /// Current status (cheap to clone; read this in `render`).
    #[must_use]
    pub fn status(&self) -> &UpdateStatus {
        &self.status
    }

    /// The release discovered by the last successful [`check`](Self::check),
    /// if it was newer than the running version.
    ///
    /// With a driver this is the release the driver last reported; its asset
    /// and signature fields describe what the driver will install, but the
    /// driver performs the download itself.
    #[must_use]
    pub fn available(&self) -> Option<&Release> {
        self.available.as_ref()
    }

    /// Whether this updater delegates to an external [`UpdateDriver`].
    #[must_use]
    pub fn is_driver_backed(&self) -> bool {
        matches!(self.backend, Backend::Driver { .. })
    }

    /// The driver of type `D`, if this updater was built with
    /// [`with_driver`](Self::with_driver) and that driver is a `D`. Use it to
    /// reach driver-specific settings (such as Sparkle's automatic checks).
    pub fn driver_mut<D: UpdateDriver>(&mut self) -> Option<&mut D> {
        match &mut self.backend {
            Backend::Driver { driver, .. } => (driver.as_mut() as &mut dyn Any).downcast_mut::<D>(),
            Backend::Engine(_) => None,
        }
    }

    fn set_status(&mut self, status: UpdateStatus, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    fn apply_driver_event(&mut self, event: DriverEvent, cx: &mut Context<Self>) {
        let status = match event {
            DriverEvent::Checking => UpdateStatus::Checking,
            DriverEvent::UpToDate => {
                // Unlike an engine check, a driver's "up to date" means it has
                // nothing to install, so `Settled` must not resurrect an
                // earlier release.
                self.available = None;
                UpdateStatus::UpToDate
            }
            DriverEvent::Found(release) => {
                let version = release.version.clone();
                self.available = Some(release);
                UpdateStatus::Available(version)
            }
            DriverEvent::Downloading { downloaded, total } => {
                UpdateStatus::Downloading { downloaded, total }
            }
            DriverEvent::Installing => UpdateStatus::Installing,
            DriverEvent::ReadyToRelaunch(version) => UpdateStatus::Staged(version),
            DriverEvent::Failed(error) => UpdateStatus::Errored(error),
            DriverEvent::Settled => {
                if !self.status.is_busy() {
                    return;
                }
                self.available
                    .as_ref()
                    .map_or(UpdateStatus::Idle, |release| {
                        UpdateStatus::Available(release.version.clone())
                    })
            }
            // `DriverEvent` is non-exhaustive; ignore events this adapter
            // does not know yet.
            _ => return,
        };
        self.set_status(status, cx);
    }

    /// Check for a newer release.
    ///
    /// With the engine this is a no-op while already busy. With a driver it
    /// forwards to [`UpdateDriver::check`], which may also bring an update
    /// prompt the driver is already showing to the front.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        let engine = match &mut self.backend {
            Backend::Driver { driver, .. } => {
                driver.check();
                return;
            }
            Backend::Engine(engine) => engine.clone(),
        };
        if self.status.is_busy() {
            return;
        }
        self.set_status(UpdateStatus::Checking, cx);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { engine.check() })
                .await;
            this.update(cx, |this, cx| {
                this.task = None;
                match result {
                    Ok(Some(release)) => {
                        let version = release.version.clone();
                        this.available = Some(release);
                        this.set_status(UpdateStatus::Available(version), cx);
                    }
                    Ok(None) => this.set_status(UpdateStatus::UpToDate, cx),
                    Err(e) => this.set_status(UpdateStatus::Errored(e.into()), cx),
                }
            })
            .ok();
        }));
    }

    /// Download the available update, verify it, and swap it into place.
    /// With the engine this is a no-op unless a newer release is
    /// [`available`](Self::available).
    ///
    /// On success the status becomes [`UpdateStatus::Staged`] and the app's
    /// restart path is set to the new binary; call [`restart`](Self::restart).
    /// With a driver this forwards to [`UpdateDriver::install`].
    pub fn download_and_install(&mut self, cx: &mut Context<Self>) {
        let engine = match &mut self.backend {
            Backend::Driver { driver, .. } => {
                driver.install();
                return;
            }
            Backend::Engine(engine) => engine.clone(),
        };
        if self.status.is_busy() {
            return;
        }
        let Some(release) = self.available.clone() else {
            return;
        };
        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.set_status(
            UpdateStatus::Downloading {
                downloaded: 0,
                total: None,
            },
            cx,
        );
        self.task = Some(cx.spawn(async move |this, cx| {
            // The blocking download runs on a background thread and reports
            // progress into shared atomics; this foreground task polls them so
            // the UI updates live without being spammed by every 64 KiB chunk.
            let downloaded = Arc::new(AtomicU64::new(0));
            let total = Arc::new(AtomicU64::new(0)); // 0 = unknown
            let done = Arc::new(AtomicBool::new(false));

            let dl_task = {
                let (engine, release) = (engine.clone(), release.clone());
                let (d, t, fin) = (downloaded.clone(), total.clone(), done.clone());
                cx.background_executor().spawn(async move {
                    let result = engine.download_cancellable(&release, &cancel, |got, tot| {
                        d.store(got, Ordering::Relaxed);
                        t.store(tot.unwrap_or(0), Ordering::Relaxed);
                    });
                    fin.store(true, Ordering::Relaxed);
                    result
                })
            };

            loop {
                let got = downloaded.load(Ordering::Relaxed);
                let tot = total.load(Ordering::Relaxed);
                this.update(cx, |this, cx| {
                    this.set_status(
                        UpdateStatus::Downloading {
                            downloaded: got,
                            total: (tot != 0).then_some(tot),
                        },
                        cx,
                    );
                })
                .ok();
                if done.load(Ordering::Relaxed) {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
            }

            let artifact = match dl_task.await {
                Ok(artifact) => artifact,
                Err(e) => {
                    this.update(cx, |this, cx| {
                        this.task = None;
                        this.cancel = None;
                        this.set_status(download_failed(e, release.version.clone()), cx);
                    })
                    .ok();
                    return;
                }
            };

            // An install cannot be cancelled.
            this.update(cx, |this, cx| {
                this.cancel = None;
                this.set_status(UpdateStatus::Installing, cx);
            })
            .ok();

            let installed = {
                let engine = engine.clone();
                cx.background_executor()
                    .spawn(async move { engine.install(&artifact) })
                    .await
            };
            this.update(cx, |this, cx| {
                this.task = None;
                let status = installed_status(installed, release.version, |path| {
                    cx.set_restart_path(path);
                });
                this.set_status(status, cx);
            })
            .ok();
        }));
    }

    /// Stop the download in progress.
    ///
    /// With the engine the download stops at its next checkpoint (see
    /// [`CancelToken`]), removes its partial file, and the status returns to
    /// [`UpdateStatus::Available`]. An install that has started is not
    /// stopped, and a call while nothing is downloading does nothing. With a
    /// driver this forwards to [`UpdateDriver::cancel`].
    pub fn cancel(&mut self, _cx: &mut Context<Self>) {
        match &mut self.backend {
            Backend::Driver { driver, .. } => driver.cancel(),
            Backend::Engine(_) => {
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
                }
            }
        }
    }

    /// Relaunch into the staged update.
    ///
    /// With the engine this uses the restart path set during
    /// [`download_and_install`](Self::download_and_install). With a driver it
    /// forwards to [`UpdateDriver::relaunch`].
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        match &mut self.backend {
            Backend::Driver { driver, .. } => driver.relaunch(),
            Backend::Engine(_) => cx.restart(),
        }
    }
}

// Official GPUI 0.2.2's TestPlatform discards restart paths. Keep the handoff
// isolated so its exact path and error/None behavior can be tested safely.
/// The status after a download that failed with `error`. Cancelling is not a
/// failure: the update is still there to download.
fn download_failed(error: Error, version: Version) -> UpdateStatus {
    if error.kind() == ErrorKind::Cancelled {
        UpdateStatus::Available(version)
    } else {
        UpdateStatus::Errored(error.into())
    }
}

impl Drop for Updater {
    fn drop(&mut self) {
        // Dropping the entity drops its foreground task, but the blocking
        // download runs on a background thread and only stops when told to.
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }
}

fn installed_status(
    installed: Result<Installed>,
    version: Version,
    set_restart_path: impl FnOnce(std::path::PathBuf),
) -> UpdateStatus {
    match installed {
        Ok(installed) => {
            if let Some(path) = installed.restart_path {
                set_restart_path(path);
            }
            UpdateStatus::Staged(version)
        }
        Err(e) => UpdateStatus::Errored(e.into()),
    }
}

#[cfg(test)]
mod tests;
