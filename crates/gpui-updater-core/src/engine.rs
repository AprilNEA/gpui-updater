//! The blocking update orchestrator that ties the pieces together.

mod config;
#[cfg(all(test, feature = "delta", any(target_os = "macos", target_os = "linux")))]
mod delta_tests;
#[cfg(feature = "delta")]
mod hops;
mod payload;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use semver::Version;

use config::Integrity;
pub use config::{
    BundleVersion, CodeSigning, DEFAULT_MAX_DELTA_HOPS, EngineConfig, Verification, VersionBinding,
};

use crate::error::Result;
use crate::http::{self, Fetch};
use crate::install::{self, Installed};
use crate::release::Release;
use crate::source::UpdateSource;

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
    /// Propagates source/network/parse errors.
    pub fn check(&self) -> Result<Option<Release>> {
        let latest = self.source.fetch_latest()?;
        if latest.version > self.config.current_version {
            self.config.precheck(&latest)?;
            Ok(Some(latest))
        } else {
            Ok(None)
        }
    }

    /// Download the update and verify it according to the configured
    /// policies. Returns the path to hand to [`install`](Self::install).
    ///
    /// A download larger than the size the release publishes is cut off. The
    /// file is then checked against its SHA-256 and minisign signature (see
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
    /// # Errors
    /// Returns an error on download failure, if verification fails, or if the
    /// policy requires a check the release cannot satisfy
    /// ([`Error::VerificationRequired`](crate::Error::VerificationRequired)).
    pub fn download(
        &self,
        release: &Release,
        mut progress: impl FnMut(u64, Option<u64>),
    ) -> Result<PathBuf> {
        let dir = self.download_dir()?;
        #[cfg(feature = "delta")]
        if let Some(rebuilt) = self.try_deltas(release, &dir, &mut progress) {
            match self.check_payload(&rebuilt, release) {
                Ok(()) => return Ok(rebuilt),
                Err(e) => {
                    tracing::warn!(error = %e, "rebuilt update rejected; falling back to the full download");
                    let _ = std::fs::remove_dir_all(&rebuilt);
                }
            }
        }
        let artifact = dir.join(&release.asset.name);
        let fetch = Fetch::new(&release.asset.url).expect_len(release.asset.size);
        http::download(&fetch, &artifact, &mut progress)?;
        self.config
            .verify_file(&artifact, Integrity::of_release(release), &release.version)?;
        self.check_payload(&artifact, release)?;
        Ok(artifact)
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
                std::fs::create_dir_all(dir)?;
                Ok(dir.clone())
            }
            None => default_download_dir(),
        }
    }

    fn install_root(&self) -> Result<PathBuf> {
        match &self.config.install_root {
            Some(root) => Ok(root.clone()),
            None => install::current_install_root(),
        }
    }

    /// Install a downloaded artifact over the configured install root.
    ///
    /// # Errors
    /// Propagates install failures.
    pub fn install(&self, artifact: &Path) -> Result<Installed> {
        install::install(artifact, &self.install_root()?)
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

fn default_download_dir() -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("gpui-updater-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
