//! GPUI integration: an observable `Entity<Updater>` that drives the blocking
//! [`UpdateEngine`] on the app's executors.
//!
//! The update runs on the background executor so the UI thread never blocks;
//! state transitions land back on the foreground via the entity and call
//! [`Context::notify`], so any view can `cx.observe(&updater, …)` to re-render.
//! When an update is staged, `App::set_restart_path` is set so the next
//! `cx.restart()` launches the new version.
//!
//! ```no_run
//! # use gpui::{AppContext as _, Context, Entity};
//! # use gpui_updater_pre::{EngineConfig, GitHubSource, UpdateStatus, Updater};
//! # use gpui_updater_pre::Version;
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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

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
/// Dropping the entity cancels its foreground task, not an already-running
/// blocking download or install. It does not roll back an installation.
pub struct Updater {
    status: UpdateStatus,
    available: Option<Release>,
    engine: Arc<BoxedEngine>,
    task: Option<Task<()>>,
}

impl Updater {
    /// Create an updater from a [`UpdateSource`] and [`EngineConfig`].
    pub fn new<S: UpdateSource>(source: S, config: EngineConfig, _cx: &mut Context<Self>) -> Self {
        let engine = UpdateEngine::new(Box::new(source) as Box<dyn UpdateSource>, config);
        Self {
            status: UpdateStatus::Idle,
            available: None,
            engine: Arc::new(engine),
            task: None,
        }
    }

    /// Current status (cheap to clone; read this in `render`).
    #[must_use]
    pub fn status(&self) -> &UpdateStatus {
        &self.status
    }

    /// The release discovered by the last successful [`check`](Self::check),
    /// if it was newer than the running version.
    #[must_use]
    pub fn available(&self) -> Option<&Release> {
        self.available.as_ref()
    }

    fn set_status(&mut self, status: UpdateStatus, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    /// Check the source for a newer release. No-op while already busy.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        if self.status.is_busy() {
            return;
        }
        self.set_status(UpdateStatus::Checking, cx);
        let engine = self.engine.clone();
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
                    Err(e) => this.set_status(UpdateStatus::Errored(e.to_string()), cx),
                }
            })
            .ok();
        }));
    }

    /// Download the available update, verify it, and swap it into place.
    /// No-op unless a newer release is [`available`](Self::available).
    ///
    /// On success the status becomes [`UpdateStatus::Staged`] and the app's
    /// restart path is set to the new binary; call [`restart`](Self::restart).
    pub fn download_and_install(&mut self, cx: &mut Context<Self>) {
        if self.status.is_busy() {
            return;
        }
        let Some(release) = self.available.clone() else {
            return;
        };
        let engine = self.engine.clone();
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
                    let result = engine.download(&release, |got, tot| {
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
                Ok(path) => path,
                Err(e) => {
                    this.update(cx, |this, cx| {
                        this.task = None;
                        this.set_status(UpdateStatus::Errored(e.to_string()), cx);
                    })
                    .ok();
                    return;
                }
            };

            this.update(cx, |this, cx| this.set_status(UpdateStatus::Installing, cx))
                .ok();

            let installed = {
                let engine = engine.clone();
                cx.background_executor()
                    .spawn(async move { engine.install(&artifact) })
                    .await
            };
            this.update(cx, |this, cx| {
                this.task = None;
                match installed {
                    Ok(installed) => {
                        if let Some(path) = &installed.restart_path {
                            cx.set_restart_path(path.clone());
                        }
                        let version = release.version.clone();
                        this.set_status(UpdateStatus::Staged(version), cx);
                    }
                    Err(e) => this.set_status(UpdateStatus::Errored(e.to_string()), cx),
                }
            })
            .ok();
        }));
    }

    /// Relaunch into the staged update (uses the restart path set during
    /// [`download_and_install`](Self::download_and_install)).
    pub fn restart(&self, cx: &mut Context<Self>) {
        cx.restart();
    }
}

#[cfg(test)]
mod tests;
