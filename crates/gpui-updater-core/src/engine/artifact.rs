//! The result of a download, which is all [`UpdateEngine::install`] accepts.
//!
//! [`UpdateEngine::install`]: crate::UpdateEngine::install

use std::path::{Path, PathBuf};

/// A downloaded update that passed the engine's checks, ready for
/// [`UpdateEngine::install`](crate::UpdateEngine::install).
///
/// Only [`UpdateEngine::download`](crate::UpdateEngine::download) creates one,
/// after verifying the file under the configured [`Verification`] policy and
/// the macOS bundle checks, so `install` cannot be handed a file that skipped
/// them. (With [`Verification::Off`] nothing was checked, as configured.) To
/// install an arbitrary file on purpose, call [`crate::install::install`]
/// directly.
///
/// The path is the downloaded `.dmg`, archive, or installer, or — after a
/// delta update — the directory holding the rebuilt app.
///
/// A bare path does not compile:
///
/// ```compile_fail,E0308
/// # use gpui_updater_core::{EngineConfig, GitHubSource, UpdateEngine, Version};
/// # let engine = UpdateEngine::new(GitHubSource::new("o", "r"), EngineConfig::new(Version::new(1, 0, 0)));
/// engine.install(std::path::Path::new("Unverified.dmg"))?;
/// # Ok::<_, gpui_updater_core::Error>(())
/// ```
///
/// [`Verification`]: crate::Verification
/// [`Verification::Off`]: crate::Verification::Off
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    path: PathBuf,
}

impl Artifact {
    /// Wrap a path the engine has verified.
    pub(super) fn verified(path: PathBuf) -> Self {
        Self { path }
    }

    /// Where the artifact is on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The artifact's path, giving up the guarantee that it was verified.
    pub fn into_path(self) -> PathBuf {
        self.path
    }
}

impl AsRef<Path> for Artifact {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}
