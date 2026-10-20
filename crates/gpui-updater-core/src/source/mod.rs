//! Where to look for releases.
//!
//! A [`UpdateSource`] knows how to fetch the latest release metadata. The
//! crate ships [`GitHubSource`], [`StaticManifestSource`], and
//! [`AppcastSource`] (Sparkle / WinSparkle appcasts), plus
//! [`FallbackSource`], which asks several of them in order; implement the
//! trait yourself to read from a self-hosted endpoint, GitLab, etc.
#![doc = include_str!("../../docs/sources.md")]

mod appcast;
mod dates;
mod fallback;
mod github;
mod manifest;
mod trust;

pub use appcast::{AppcastSource, AppcastVersion};
pub use fallback::FallbackSource;
pub use github::GitHubSource;
pub use manifest::StaticManifestSource;

use crate::error::Result;
use crate::release::Release;

/// A source of release metadata.
///
/// Implementations perform blocking I/O; callers must choose an appropriate
/// thread or executor.
pub trait UpdateSource: Send + Sync + 'static {
    /// Fetch the latest release applicable to the running platform.
    ///
    /// This returns the newest release the source knows about — comparing it
    /// against the currently running version is the caller's job (see
    /// [`crate::UpdateEngine::check`]).
    ///
    /// # Errors
    /// Returns an error if the network request fails, the metadata cannot be
    /// parsed, or no asset matches the running platform.
    fn fetch_latest(&self) -> Result<Release>;
}

/// Forwarding impl so a boxed, type-erased source can drive the engine.
impl UpdateSource for Box<dyn UpdateSource> {
    fn fetch_latest(&self) -> Result<Release> {
        (**self).fetch_latest()
    }
}
