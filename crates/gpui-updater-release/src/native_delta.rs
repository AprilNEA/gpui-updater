//! Native `files-bsdiff-v1` deltas, which the built-in engine applies on
//! every platform (feature `delta`).
//!
//! This wraps [`gpui_updater_core::delta`] with what release CI needs on top:
//! the file name the manifest builder recognizes, an entry-by-entry check that
//! the delta rebuilds the new build, and the base versions to build from.

use std::fs;
use std::path::{Path, PathBuf};

use gpui_updater_core::Version;
use gpui_updater_core::delta::{self, DeltaInfo, VersionPair};

use crate::error::{Error, Result};
use crate::manifest::{DeltaFile, delta_file_name};
use crate::tree::ensure_same_tree;
use crate::util::sha256_file;

pub use gpui_updater_core::delta::FORMAT;

/// How [`create`] makes a delta.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NativeDeltaOptions {
    /// Apply the delta to the old build and compare with the new one before
    /// returning. Default `true`.
    pub verify: bool,
}

impl Default for NativeDeltaOptions {
    fn default() -> Self {
        Self { verify: true }
    }
}

/// A native delta on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NativeDelta {
    /// The delta file.
    pub path: PathBuf,
    /// Its size in bytes.
    pub length: u64,
    /// What the delta's header says.
    pub info: DeltaInfo,
}

impl NativeDelta {
    /// The manifest entry for this delta under `asset`.
    pub fn manifest_entry(&self, asset: impl Into<String>) -> DeltaFile {
        let mut file = DeltaFile::new(asset, self.info.from.to_string(), &self.path);
        file.to = Some(self.info.to.to_string());
        file
    }
}

/// The path [`ManifestBuilder::scan_dir`](crate::ManifestBuilder::scan_dir)
/// recognizes for a delta of `asset` from `from`, inside `dir`.
pub fn delta_path(dir: &Path, asset: &str, from: &Version) -> PathBuf {
    dir.join(delta_file_name(asset, &from.to_string()))
}

/// Make a delta from `old` to `new` at `out`. Both are app bundle
/// directories (macOS) or both single executables (Linux): the unpacked
/// contents of two releases.
pub fn create(
    old: &Path,
    new: &Path,
    from: &Version,
    to: &Version,
    out: &Path,
    options: &NativeDeltaOptions,
) -> Result<NativeDelta> {
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(Error::io("create", parent))?;
    }
    let info = delta::create(old, new, &VersionPair::new(from.clone(), to.clone()), out)?;
    if options.verify {
        verify(old, new, out)?;
    }
    Ok(NativeDelta {
        path: out.to_path_buf(),
        length: fs::metadata(out).map_err(Error::io("stat", out))?.len(),
        info,
    })
}

/// Fail unless applying `delta` to `old` gives exactly `new`.
pub fn verify(old: &Path, new: &Path, delta: &Path) -> Result<()> {
    let scratch = tempfile::tempdir().map_err(Error::io(
        "create a scratch directory in",
        std::env::temp_dir(),
    ))?;
    let name = new.file_name().unwrap_or_else(|| "rebuilt".as_ref());
    let rebuilt = scratch.path().join(name);
    delta::apply(delta, old, &rebuilt)?;
    if new.is_dir() {
        return ensure_same_tree(&rebuilt, new);
    }
    if sha256_file(&rebuilt)? == sha256_file(new)? {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "`{}` does not rebuild `{}`",
            delta.display(),
            new.display()
        )))
    }
}

/// Which of the earlier releases (newest first, without the one being
/// published) to build deltas from: the `direct` newest plus a skip list, so
/// any user reaches the newest release in about log₂ steps. See
/// [`gpui_updater_core::delta::delta_bases`].
pub fn bases(previous: &[Version], direct: usize) -> Vec<Version> {
    delta::delta_bases(previous, direct)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn build(root: &Path, main: &[u8]) {
        fs::create_dir_all(root.join("Contents/MacOS")).unwrap();
        fs::write(root.join("Contents/MacOS/App"), main).unwrap();
        fs::write(root.join("Contents/Info.plist"), b"<plist/>").unwrap();
    }

    #[test]
    fn creates_and_verifies_a_bundle_delta() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (
            dir.path().join("old/App.app"),
            dir.path().join("new/App.app"),
        );
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut changed = body.clone();
        changed[1000..1010].copy_from_slice(b"0123456789");
        build(&old, &body);
        build(&new, &changed);
        let out = delta_path(
            &dir.path().join("dist"),
            "App-macos-arm64.dmg",
            &Version::new(1, 0, 0),
        );
        let delta = create(
            &old,
            &new,
            &Version::new(1, 0, 0),
            &Version::new(1, 1, 0),
            &out,
            &NativeDeltaOptions::default(),
        )
        .unwrap();
        assert!(out.ends_with("App-macos-arm64.dmg.from-1.0.0.delta"));
        assert!(delta.length < 20_000, "{}", delta.length);
        assert_eq!(delta.info.to, Version::new(1, 1, 0));
        let entry = delta.manifest_entry("App-macos-arm64.dmg");
        assert_eq!(
            (entry.from.as_str(), entry.to.as_deref()),
            ("1.0.0", Some("1.1.0"))
        );
        assert_eq!(DeltaFile::from_path(&out).unwrap().from, "1.0.0");

        // A delta that rebuilds something else fails verification.
        fs::write(new.join("Contents/extra"), b"x").unwrap();
        assert!(verify(&old, &new, &out).is_err());
    }

    #[test]
    fn bases_follow_the_skip_list() {
        let previous: Vec<Version> = (0..20).rev().map(|m| Version::new(1, m, 0)).collect();
        let picked: Vec<String> = bases(&previous, 3)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            picked,
            ["1.19.0", "1.18.0", "1.17.0", "1.16.0", "1.12.0", "1.4.0"]
        );
    }
}
