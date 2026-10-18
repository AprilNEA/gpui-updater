//! The blocking update orchestrator that ties the pieces together.

mod artifact;
mod config;
#[cfg(all(test, feature = "delta", any(target_os = "macos", target_os = "linux")))]
mod delta_tests;
#[cfg(feature = "delta")]
mod hops;
mod payload;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use semver::Version;

pub use artifact::Artifact;
use config::Integrity;
pub use config::{
    BundleVersion, CodeSigning, DEFAULT_MAX_DELTA_HOPS, DEFAULT_MAX_DOWNLOAD_SIZE, EngineConfig,
    Verification, VersionBinding,
};

use crate::cancel::CancelToken;
use crate::error::Result;
use crate::http::{self, Fetch};
use crate::install::{self, Installed};
use crate::release::Release;
use crate::source::UpdateSource;
use crate::staging;

/// Blocking update orchestrator: check → download → verify → install.
///
/// All methods block; the GPUI integration runs them on a background executor.
pub struct UpdateEngine<S: UpdateSource> {
    source: S,
    config: EngineConfig,
}

impl<S: UpdateSource> UpdateEngine<S> {
    /// Build an engine from a source and configuration.
    pub fn new(source: S, config: EngineConfig) -> Self {
        Self { source, config }
    }

    /// The configured current version.
    pub fn current_version(&self) -> &Version {
        &self.config.current_version
    }

    /// Fetch the latest release, returning it only if it is newer than the
    /// current version (otherwise `Ok(None)`).
    ///
    /// # Errors
    /// Propagates source/network/parse errors. A newer release is rejected
    /// instead of returned when the [`Verification`] policy cannot be met
    /// ([`Error::VerificationRequired`](crate::Error::VerificationRequired)),
    /// or when its artifact or signature would not be fetched over HTTPS
    /// ([`Error::InsecureUrl`](crate::Error::InsecureUrl)).
    pub fn check(&self) -> Result<Option<Release>> {
        let latest = self.source.fetch_latest()?;
        if latest.version > self.config.current_version {
            self.config.precheck(&latest)?;
            ensure_fetchable(&latest)?;
            Ok(Some(latest))
        } else {
            Ok(None)
        }
    }

    /// Download the update and verify it according to the configured
    /// policies. Returns the [`Artifact`] to hand to [`install`](Self::install).
    ///
    /// Files are fetched over HTTPS only, into a fresh directory that only the
    /// current user can access (or [`EngineConfig::download_dir`], where an
    /// earlier download of the same artifact that still verifies is reused
    /// instead of fetched again). A download
    /// larger than the size the release publishes, or than
    /// [`EngineConfig::max_download_size`], is cut off. The file is then
    /// checked against its SHA-256 and minisign signature (see
    /// [`Verification`] and [`VersionBinding`]), and on macOS the app inside
    /// must declare the release version and carry the expected code signature
    /// (see [`BundleVersion`] and [`CodeSigning`]).
    ///
    /// With the `delta` feature (macOS and Linux), published deltas are tried
    /// first: the cheapest chain from the current version, if it is smaller
    /// than the full artifact. Each delta is downloaded, verified under the
    /// same policies, and applied to the files it expects, which are checked
    /// against the hashes recorded in the delta. The result is a directory
    /// holding the rebuilt bundle or executable. If anything in that path
    /// fails — no usable delta, a modified installation, a bad checksum or
    /// signature, a network error — the full `release.asset` is downloaded
    /// instead, so deltas can never make an update fail.
    ///
    /// `progress` is called as `(downloaded_bytes, total_bytes)`. After a failed
    /// delta attempt it restarts from zero for the full download.
    ///
    /// To stop a download from another thread, use
    /// [`download_cancellable`](Self::download_cancellable).
    ///
    /// # Errors
    /// Returns an error on download failure, if verification fails, if the
    /// policy requires a check the release cannot satisfy
    /// ([`Error::VerificationRequired`](crate::Error::VerificationRequired)),
    /// or, before anything is downloaded, if the update could not be
    /// installed where the app is ([`check_location`](Self::check_location)).
    pub fn download(
        &self,
        release: &Release,
        progress: impl FnMut(u64, Option<u64>),
    ) -> Result<Artifact> {
        self.download_cancellable(release, &CancelToken::new(), progress)
    }

    /// [`download`](Self::download), stopping with
    /// [`Error::Cancelled`](crate::Error::Cancelled) once `cancel` is
    /// cancelled. See [`CancelToken`] for when a cancel takes effect. A
    /// cancelled download removes its partial file; a delta attempt that is
    /// cancelled does not fall back to the full download.
    ///
    /// # Errors
    /// As [`download`](Self::download), plus
    /// [`Error::Cancelled`](crate::Error::Cancelled).
    pub fn download_cancellable(
        &self,
        release: &Release,
        cancel: &CancelToken,
        mut progress: impl FnMut(u64, Option<u64>),
    ) -> Result<Artifact> {
        cancel.check()?;
        self.check_location(release)?;
        let dir = self.download_dir()?;
        // The name comes from release metadata; keep the file inside `dir`.
        let artifact = dir.join(staging::safe_file_name(&release.asset.name, "update"));
        if self.config.download_dir.is_some() {
            if let Some(len) = self.reusable(&artifact, release) {
                tracing::info!(path = %artifact.display(), "reusing the verified earlier download");
                progress(len, Some(len));
                return Ok(Artifact::verified(artifact));
            }
        }
        #[cfg(feature = "delta")]
        if let Some(rebuilt) = self.try_deltas(release, &dir, cancel, &mut progress) {
            match self.check_payload(&rebuilt, release) {
                Ok(()) => return Ok(Artifact::verified(rebuilt)),
                Err(e) => {
                    tracing::warn!(error = %e, "rebuilt update rejected; falling back to the full download");
                    let _ = std::fs::remove_dir_all(&rebuilt);
                }
            }
        }
        // A delta attempt that was cancelled must not turn into a full download.
        cancel.check()?;
        let fetch = Fetch::new(&release.asset.url)
            .expect_len(release.asset.size)
            .max_len(self.config.max_download_size)
            .cancel(cancel)
            .http(&self.config.http);
        http::download(&fetch, &artifact, &mut progress)?;
        cancel.check()?;
        self.config
            .verify_file(&artifact, Integrity::of_release(release), &release.version)?;
        self.check_payload(&artifact, release)?;
        Ok(Artifact::verified(artifact))
    }

    /// The size of `path`, if an earlier download left this release's
    /// artifact there: a regular file of the published size that passes a
    /// published checksum or signature and the payload check. A file that
    /// passes only because nothing was published to check it against is not
    /// reused.
    fn reusable(&self, path: &Path, release: &Release) -> Option<u64> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        let size = release.asset.size;
        if !meta.is_file() || (size > 0 && meta.len() != size) {
            return None;
        }
        match self
            .config
            .verify_file(path, Integrity::of_release(release), &release.version)
        {
            Ok(true) => {}
            Ok(false) => return None,
            Err(e) => {
                tracing::debug!(error = %e, "not reusing the earlier download");
                return None;
            }
        }
        self.check_payload(path, release).ok()?;
        Some(meta.len())
    }

    /// Check the app inside a verified artifact before it is offered for
    /// install (macOS).
    fn check_payload(&self, artifact: &Path, release: &Release) -> Result<()> {
        if self.config.verification.skips_all() {
            return Ok(());
        }
        let installed = self.install_root()?;
        payload::check(
            artifact,
            &payload::Expectations {
                version: &release.version,
                installed: &installed,
                bundle_version: self.config.bundle_version,
                code_signing: &self.config.code_signing,
            },
        )
    }

    fn download_dir(&self) -> Result<PathBuf> {
        match &self.config.download_dir {
            Some(dir) => {
                staging::ensure_private_dir(dir)?;
                Ok(dir.clone())
            }
            None => Ok(staging::private_temp_dir("gpui-updater")?),
        }
    }

    /// Check that `release` could be installed where the app is, without
    /// downloading anything: see [`install::check_location`].
    /// [`download`](Self::download) runs this first; call it yourself to tell
    /// the user before offering the download.
    ///
    /// # Errors
    /// [`Error::Location`](crate::Error::Location) when the app runs from a
    /// read-only place, such as a disk image or an App Translocation copy, and
    /// [`Error::Permission`](crate::Error::Permission) when the user may not
    /// write next to it.
    pub fn check_location(&self, release: &Release) -> Result<()> {
        install::check_location(&self.install_root()?, &release.asset.name)
    }

    fn install_root(&self) -> Result<PathBuf> {
        match &self.config.install_root {
            Some(root) => Ok(root.clone()),
            None => install::current_install_root(),
        }
    }

    /// Install a downloaded artifact over the configured install root.
    ///
    /// Only an [`Artifact`] returned by [`download`](Self::download) is
    /// accepted, so nothing reaches the installer without passing the
    /// engine's checks.
    ///
    /// # Errors
    /// Propagates install failures.
    pub fn install(&self, artifact: &Artifact) -> Result<Installed> {
        install::install_with(
            artifact.path(),
            &self.install_root()?,
            &self.config.install_options,
        )
    }

    /// Remove what an earlier install left beside the install root because
    /// the old version was still running, such as the previous `.exe` on
    /// Windows: see [`install::remove_leftovers`]. Call it at startup.
    pub fn remove_leftovers(&self) {
        if let Ok(root) = self.install_root() {
            install::remove_leftovers(&root);
        }
    }

    /// Run the full pipeline. Returns `Ok(None)` when already up to date.
    ///
    /// # Errors
    /// Propagates any check/download/verify/install error.
    pub fn update_now(&self, progress: impl FnMut(u64, Option<u64>)) -> Result<Option<Installed>> {
        let Some(release) = self.check()? else {
            return Ok(None);
        };
        let artifact = self.download(&release, progress)?;
        Ok(Some(self.install(&artifact)?))
    }
}

/// Fail when the release's artifact or detached signature would not be
/// fetched over HTTPS, so such a release is never offered as available.
fn ensure_fetchable(release: &Release) -> Result<()> {
    http::ensure_secure(&release.asset.url)?;
    match release
        .signature_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        Some(url) => http::ensure_secure(url),
        None => Ok(()),
    }
}
