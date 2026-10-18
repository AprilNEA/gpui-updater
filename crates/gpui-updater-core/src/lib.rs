//! Blocking cross-platform self-update, hosted on GitHub Releases, static JSON
//! manifests, or Sparkle appcasts. This core has no GPUI dependencies.
//!
//! GPUI ships no updater of its own, and Zed's `auto_update` is GPL-licensed and
//! wired to Zed's private update server. This crate is an independent, MIT/Apache
//! implementation of the same idea: a small state machine that checks a release
//! source, downloads the platform artifact, verifies it, and swaps it into place.
//!
//! # Layers
//!
//! - [`UpdateSource`] — where releases come from. [`GitHubSource`],
//!   [`StaticManifestSource`], and [`AppcastSource`] are built in.
//! - [`UpdateEngine`] — the blocking pipeline: [`check`](UpdateEngine::check) →
//!   [`download`](UpdateEngine::download) (with SHA-256, minisign, and
//!   Sparkle-compatible Ed25519 verification) → [`install`](UpdateEngine::install).
//! - [`install`] / [`verify`] — the platform install strategies and integrity
//!   checks, usable on their own.
//! - [`UpdateDriver`] — the contract for handing the whole flow to an external
//!   updater instead of the engine; `gpui-updater-sparkle` implements it with
//!   Sparkle on macOS.
//! - `delta` (behind the `delta` feature) — binary delta updates: create a
//!   per-file bsdiff delta in release CI, and let the engine rebuild the new
//!   version from the installed one, falling back to the full download on any
//!   problem. See the README's "Delta updates" section.
//! - [`UpdateStatus`] — backend-independent state used by the separate
//!   `gpui-updater` (official GPUI) and `gpui-updater-pre` adapters, whichever
//!   backend (engine or driver) they run.
//!
//! # Example (blocking engine)
//!
//! ```no_run
//! use gpui_updater_core::{EngineConfig, GitHubSource, UpdateEngine};
//! use semver::Version;
//!
//! let source = GitHubSource::new("AprilNEA", "OpenLogi")
//!     .asset_contains("macos")
//!     .asset_contains(".dmg")
//!     .with_checksums("SHA256SUMS")
//!     .with_minisig();
//!
//! let engine = UpdateEngine::new(
//!     source,
//!     EngineConfig::new(Version::parse(env!("CARGO_PKG_VERSION")).unwrap())
//!         .minisign_public_key("RWQ…"),
//! );
//!
//! if let Some(release) = engine.check()? {
//!     let artifact = engine.download(&release, |done, total| {
//!         if let Some(total) = total {
//!             eprintln!("{done}/{total}");
//!         }
//!     })?;
//!     engine.install(&artifact)?;
//! }
//! # Ok::<_, gpui_updater_core::Error>(())
//! ```
//!
//! [GPUI]: https://www.gpui.rs/

// Panicking on failure is the point of a test assertion.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod cancel;
mod engine;
mod error;
mod http;
mod release;
mod staging;
mod status;

#[cfg(feature = "delta")]
pub mod delta;
pub mod driver;
pub mod install;
pub mod source;
pub mod verify;

pub use cancel::CancelToken;
pub use driver::{DriverEvent, DriverEvents, UpdateDriver};
pub use engine::{
    Artifact, BundleVersion, CodeSigning, DEFAULT_MAX_DELTA_HOPS, DEFAULT_MAX_DOWNLOAD_SIZE,
    EngineConfig, UpdateEngine, Verification, VersionBinding,
};
pub use error::{Error, ErrorKind, Result};
pub use install::{Installed, current_install_root};
pub use release::{
    Asset, DELTA_FORMAT_FILES_BSDIFF_V1, DeltaAsset, Release, parse_tag, parse_version_lenient,
};
/// Re-exported so consumers can build an [`EngineConfig`] without depending on
/// `semver` directly (e.g. `Version::parse(env!("CARGO_PKG_VERSION"))`).
pub use semver::Version;
pub use source::{AppcastSource, AppcastVersion, GitHubSource, StaticManifestSource, UpdateSource};
pub use status::{UpdateError, UpdateStatus};
