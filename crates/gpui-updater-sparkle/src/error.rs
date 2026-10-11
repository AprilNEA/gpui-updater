//! Errors from loading Sparkle.

use std::path::PathBuf;

/// Why [`SparkleDriver::load`](crate::SparkleDriver::load) could not provide a
/// driver for a bundle that embeds Sparkle.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Sparkle may only be loaded and driven from the main thread.
    #[error("Sparkle must be loaded on the main thread")]
    NotMainThread,
    /// The app's `Info.plist` lacks a key Sparkle needs.
    #[error("Info.plist has no `{key}`, which Sparkle requires")]
    MissingInfoKey {
        /// The missing key, e.g. `SUPublicEDKey`.
        key: &'static str,
    },
    /// The framework could not be loaded, or is not Sparkle 2.
    #[error("could not load {}: {message}", path.display())]
    Load {
        /// Path of the framework that failed to load.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },
}
