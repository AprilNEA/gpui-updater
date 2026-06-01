//! The blocking update orchestrator that ties the pieces together.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use semver::Version;

use crate::error::Result;
use crate::install::{self, Installed};
use crate::release::Release;
use crate::source::UpdateSource;
use crate::{http, verify};

/// Configuration for an [`UpdateEngine`].
pub struct EngineConfig {
    /// The currently running version, compared against the source's latest.
    pub current_version: Version,
    /// Base64 minisign public key. When set and the release publishes a
    /// `.minisig`, the download is signature-checked in addition to SHA-256.
    pub minisign_public_key: Option<String>,
    /// Where to install. Defaults to [`install::current_install_root`].
    pub install_root: Option<PathBuf>,
    /// Directory for downloads. Defaults to a fresh temp directory.
    pub download_dir: Option<PathBuf>,
}

impl EngineConfig {
    /// Start a config for the given running version.
    pub fn new(current_version: Version) -> Self {
        Self {
            current_version,
            minisign_public_key: None,
            install_root: None,
            download_dir: None,
        }
    }

    /// Set the base64 minisign public key used to verify signed downloads.
    #[must_use]
    pub fn minisign_public_key(mut self, key: impl Into<String>) -> Self {
        self.minisign_public_key = Some(key.into());
        self
    }

    /// Override the install location (defaults to the running app/bundle).
    #[must_use]
    pub fn install_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.install_root = Some(path.into());
        self
    }

    /// Override the download directory (defaults to a temp directory).
    #[must_use]
    pub fn download_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.download_dir = Some(path.into());
        self
    }
}

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
        Ok((latest.version > self.config.current_version).then_some(latest))
    }

    /// Download `release.asset`, then verify its SHA-256 (when the source
    /// resolved one) and minisign signature (when a public key is configured
    /// and a `.minisig` is published). Returns the downloaded artifact path.
    ///
    /// `progress` is called as `(downloaded_bytes, total_bytes)`.
    ///
    /// # Errors
    /// Returns an error on download failure or if verification fails.
    pub fn download(
        &self,
        release: &Release,
        mut progress: impl FnMut(u64, Option<u64>),
    ) -> Result<PathBuf> {
        let dir = match &self.config.download_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                dir.clone()
            }
            None => default_download_dir()?,
        };
        let artifact = dir.join(&release.asset.name);
        http::download(&release.asset.url, &[], &artifact, &mut progress)?;

        if let Some(expected) = &release.sha256 {
            verify::verify_sha256(&artifact, expected)?;
        }
        if let Some(key) = &self.config.minisign_public_key {
            if let Some(signature) = &release.signature {
                verify::verify_minisign(&artifact, key, signature)?;
            } else if let Some(sig_url) = &release.signature_url {
                let signature = http::get_string(sig_url, &[])?;
                verify::verify_minisign(&artifact, key, &signature)?;
            }
        }
        Ok(artifact)
    }

    /// Install a downloaded artifact over the configured install root.
    ///
    /// # Errors
    /// Propagates install failures.
    pub fn install(&self, artifact: &Path) -> Result<Installed> {
        let root = match &self.config.install_root {
            Some(root) => root.clone(),
            None => install::current_install_root()?,
        };
        install::install(artifact, &root)
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
