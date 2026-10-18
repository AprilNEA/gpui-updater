//! Get Sparkle's release archive: the framework to embed and the `bin/` tools
//! (`BinaryDelta`, `sign_update`, `generate_keys`, `generate_appcast`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::util::{run, sha256_file};

/// A Sparkle release: version, archive URL, and the archive's SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SparkleRelease {
    /// Version, for example `2.10.0`.
    pub version: String,
    /// Download URL of the `.tar.xz` archive.
    pub url: String,
    /// Lowercase hex SHA-256 of the archive.
    pub sha256: String,
}

impl SparkleRelease {
    /// The release this crate is tested with: Sparkle 2.10.0.
    pub fn pinned() -> Self {
        Self::new(
            "2.10.0",
            "c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c",
        )
    }

    /// A release from GitHub, by version and archive SHA-256.
    pub fn new(version: impl Into<String>, sha256: impl Into<String>) -> Self {
        let version = version.into();
        Self {
            url: format!(
                "https://github.com/sparkle-project/Sparkle/releases/download/{version}/Sparkle-{version}.tar.xz"
            ),
            version,
            sha256: sha256.into().to_ascii_lowercase(),
        }
    }

    /// Download from `url` instead, for example a mirror.
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Download and check the archive into `cache`, then unpack it afresh
    /// into `cache/Sparkle-<version>`. A cached archive with the pinned hash
    /// is not downloaded again; the unpacked tree is always replaced, so
    /// nothing left in it can reach a bundle. Needs `curl` and `tar`.
    pub fn fetch(&self, cache: &Path) -> Result<SparkleDistribution> {
        fs::create_dir_all(cache).map_err(Error::io("create", cache))?;
        let archive = cache.join(format!("Sparkle-{}.tar.xz", self.version));
        if !archive.is_file() || sha256_file(&archive)? != self.sha256 {
            run(Command::new("curl")
                .args([
                    "--fail",
                    "--silent",
                    "--show-error",
                    "--location",
                    "--retry",
                    "3",
                    "--output",
                ])
                .arg(&archive)
                .arg(&self.url))?;
        }
        let root = cache.join(format!("Sparkle-{}", self.version));
        unpack(&archive, &self.sha256, &root)?;
        SparkleDistribution::from_dir(&root)
    }
}

/// Check `archive` against `sha256` and unpack it into `into` (replaced).
pub fn unpack(archive: &Path, sha256: &str, into: &Path) -> Result<()> {
    let actual = sha256_file(archive)?;
    if actual != sha256.to_ascii_lowercase() {
        return Err(Error::invalid(format!(
            "`{}` has SHA-256 {actual}, not the pinned {sha256}",
            archive.display()
        )));
    }
    if into.exists() {
        fs::remove_dir_all(into).map_err(Error::io("remove", into))?;
    }
    fs::create_dir_all(into).map_err(Error::io("create", into))?;
    run(Command::new("tar")
        .arg("-xJf")
        .arg(archive)
        .arg("-C")
        .arg(into))?;
    Ok(())
}

/// An unpacked Sparkle release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparkleDistribution {
    root: PathBuf,
}

impl SparkleDistribution {
    /// Use an already unpacked release.
    pub fn from_dir(root: &Path) -> Result<Self> {
        let dist = Self {
            root: root.to_path_buf(),
        };
        if !dist.framework().is_dir() {
            return Err(Error::invalid(format!(
                "`{}` has no Sparkle.framework; is it an unpacked Sparkle release?",
                root.display()
            )));
        }
        Ok(dist)
    }

    /// The release's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `Sparkle.framework`.
    pub fn framework(&self) -> PathBuf {
        self.root.join("Sparkle.framework")
    }

    /// The `LICENSE` file, which Sparkle's license asks you to ship.
    pub fn license(&self) -> PathBuf {
        self.root.join("LICENSE")
    }

    /// A tool in `bin/`, for example `BinaryDelta`.
    pub fn tool(&self, name: &str) -> PathBuf {
        self.root.join("bin").join(name)
    }

    /// `bin/BinaryDelta`.
    pub fn binary_delta(&self) -> PathBuf {
        self.tool("BinaryDelta")
    }

    /// The framework's `CFBundleShortVersionString`.
    pub fn version(&self) -> Result<String> {
        let plist = self.framework().join("Versions/B/Resources/Info.plist");
        crate::sparkle::bundle::read_string(&plist, "CFBundleShortVersionString")?
            .ok_or_else(|| Error::invalid(format!("`{}` has no version", plist.display())))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_pinned_release_points_at_github() {
        let r = SparkleRelease::pinned();
        assert_eq!(
            r.url,
            "https://github.com/sparkle-project/Sparkle/releases/download/2.10.0/Sparkle-2.10.0.tar.xz"
        );
        assert_eq!(r.sha256.len(), 64);
    }

    #[test]
    fn unpack_refuses_a_wrong_hash_and_from_dir_needs_the_framework() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.xz");
        fs::write(&archive, b"not it").unwrap();
        let err = unpack(&archive, &"0".repeat(64), &dir.path().join("out")).unwrap_err();
        assert!(err.to_string().contains("pinned"), "{err}");
        assert!(SparkleDistribution::from_dir(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_cached_archive_is_unpacked_afresh_without_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("Sparkle.framework")).unwrap();
        fs::write(src.join("LICENSE"), "MIT").unwrap();
        let cache = dir.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let archive = cache.join("Sparkle-9.9.9.tar.xz");
        run(Command::new("tar")
            .arg("-cJf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg("."))
        .unwrap();
        let release = SparkleRelease::new("9.9.9", sha256_file(&archive).unwrap())
            .url("https://invalid.example/never");
        let stale = cache.join("Sparkle-9.9.9/stale");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, "x").unwrap();
        let dist = release.fetch(&cache).unwrap();
        assert!(dist.license().is_file());
        assert!(!stale.exists());
        assert_eq!(
            dist.binary_delta(),
            cache.join("Sparkle-9.9.9/bin/BinaryDelta")
        );
    }
}
