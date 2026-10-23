//! Sparkle's own binary deltas, made with the `BinaryDelta` tool from the
//! pinned Sparkle release, so the format is always the one Sparkle applies.
//!
//! [`create`] makes the delta and, by default, applies it to the old app and
//! compares the result with the new app entry by entry before returning.
//! [`SparkleDelta::enclosure`] signs it and fills in the attributes Sparkle 2.3+
//! uses to skip a delta that cannot apply (`deltaFromSparkleExecutableSize`,
//! `deltaFromSparkleLocales`). `BinaryDelta` runs on macOS only; signing and
//! writing the appcast run anywhere.
//!
//! Sparkle's installer applies a download as a delta only when the saved
//! file's extension is exactly `delta` (`SUBinaryDeltaUnarchiver`), and it
//! saves the download under the name the server suggests: the last segment
//! of the URL path, unless the response sets `Content-Disposition`. Any
//! other name fails with "No valid unarchiver", and the client falls back to
//! the full download. [`delta_file_name`] therefore ends in
//! [`FILE_SUFFIX`], and [`create`] refuses an output name Sparkle would not
//! apply.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use super::SparkleDistribution;
use super::bundle::{framework, read_string};
use crate::appcast::DeltaEnclosure;
use crate::error::{Error, Result};
use crate::sign::{SparkleKey, sidecar};
use crate::tree::ensure_same_tree;
use crate::util::{file_name, run};

/// `generate_appcast` records at most this many locales.
pub const MAX_LOCALES: usize = 7;

/// The end of every name [`delta_file_name`] gives. Sparkle sees the
/// extension `delta`; the `.sparkle` before it keeps these files apart from
/// native `<asset>.from-<version>.delta` files in the same directory.
pub const FILE_SUFFIX: &str = ".sparkle.delta";

/// The suffix gpui-updater-release 0.4.3 and earlier used. Sparkle never
/// applies such a file; [`find_deltas`] reports one instead of attaching it.
const LEGACY_FILE_SUFFIX: &str = ".sparkle-delta";

/// What a delta needs to know about the app it applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OldApp {
    /// `CFBundleVersion`: the delta's `sparkle:deltaFrom`.
    pub bundle_version: String,
    /// `CFBundleShortVersionString`.
    pub short_version: Option<String>,
    /// Size of the embedded Sparkle executable, when Sparkle is embedded.
    pub sparkle_executable_size: Option<u64>,
    /// Up to [`MAX_LOCALES`] of Sparkle's localizations, comma-separated,
    /// without `en` and `Base` (the ones least likely to be stripped).
    pub sparkle_locales: Option<String>,
}

/// Read [`OldApp`] from an app bundle, the way `generate_appcast` does.
pub fn inspect(app: &Path) -> Result<OldApp> {
    let info = app.join("Contents/Info.plist");
    let bundle_version = read_string(&info, "CFBundleVersion")?
        .ok_or_else(|| Error::invalid(format!("`{}` has no CFBundleVersion", info.display())))?;
    let short_version = read_string(&info, "CFBundleShortVersionString")?;
    let framework = framework(app);
    let (mut sparkle_executable_size, mut sparkle_locales) = (None, None);
    if framework.exists() {
        // Both paths go through the framework's version links, which
        // `metadata` and `read_dir` follow.
        sparkle_executable_size = fs::metadata(framework.join("Sparkle"))
            .ok()
            .map(|m| m.len());
        if let Ok(entries) = fs::read_dir(framework.join("Resources")) {
            let mut locales: Vec<String> = entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter_map(|name| name.strip_suffix(".lproj").map(str::to_string))
                .filter(|locale| locale != "en" && locale != "Base")
                .collect();
            locales.sort();
            locales.truncate(MAX_LOCALES);
            if !locales.is_empty() {
                sparkle_locales = Some(locales.join(","));
            }
        }
    }
    Ok(OldApp {
        bundle_version,
        short_version,
        sparkle_executable_size,
        sparkle_locales,
    })
}

/// How [`create`] runs `BinaryDelta`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeltaOptions {
    /// Delta format major version (`--version`); `None` uses the tool's
    /// default (4 for Sparkle 2.7+). The *old* app's Sparkle must read it:
    /// 4 needs 2.7, 3 needs 2.1, 2 needs 1.10.
    pub format_version: Option<u32>,
    /// `--compression`: `lzma`, `lzfse`, `lz4`, `bzip2`, or `default`.
    pub compression: Option<String>,
    /// Apply the delta to the old app and compare with the new one before
    /// returning. Default `true`.
    pub verify: bool,
    /// Pass `--verbose`.
    pub verbose: bool,
}

impl Default for DeltaOptions {
    fn default() -> Self {
        Self {
            format_version: None,
            compression: None,
            verify: true,
            verbose: false,
        }
    }
}

/// A Sparkle delta on disk and what the appcast needs about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SparkleDelta {
    /// The delta file.
    pub path: PathBuf,
    /// Its size in bytes.
    pub length: u64,
    /// The app it applies to.
    pub from: OldApp,
    /// `CFBundleVersion` of the app it produces.
    pub to_bundle_version: String,
    /// `--version` it was made with, when given.
    pub format_version: Option<u32>,
}

impl SparkleDelta {
    /// The `<sparkle:deltas>` entry for this delta at `url`, signed with
    /// `key` when given.
    pub fn enclosure(
        &self,
        url: impl Into<String>,
        key: Option<&SparkleKey>,
    ) -> Result<DeltaEnclosure> {
        Ok(
            DeltaEnclosure::for_file(&self.path, url, self.from.bundle_version.clone(), key)?
                .old_sparkle(
                    self.from.sparkle_executable_size,
                    self.from.sparkle_locales.clone(),
                ),
        )
    }

    /// Write this description next to the delta as `<delta>.json`, so a
    /// later step (often on another machine) can write the appcast.
    pub fn write_metadata(&self) -> Result<PathBuf> {
        let out = sidecar(&self.path, "json");
        let mut stored = self.clone();
        stored.path = PathBuf::from(file_name(&self.path)?);
        fs::write(&out, serde_json::to_string_pretty(&stored)? + "\n")
            .map_err(Error::io("write", &out))?;
        Ok(out)
    }

    /// Read the description [`write_metadata`](Self::write_metadata) wrote
    /// for `delta`, and check it still matches the file.
    pub fn read_metadata(delta: &Path) -> Result<Self> {
        let json = sidecar(delta, "json");
        let text = fs::read_to_string(&json).map_err(Error::io("read", &json))?;
        let mut stored: Self = serde_json::from_str(&text)?;
        stored.path = delta.to_path_buf();
        let length = fs::metadata(delta).map_err(Error::io("stat", delta))?.len();
        if length != stored.length {
            return Err(Error::invalid(format!(
                "`{}` is {length} bytes, but its metadata says {}",
                delta.display(),
                stored.length
            )));
        }
        Ok(stored)
    }
}

/// The conventional Sparkle delta file name:
/// `<archive>.from-<bundle version>.sparkle.delta` (see [`FILE_SUFFIX`]).
pub fn delta_file_name(archive: &str, from_bundle_version: &str) -> String {
    format!("{archive}.from-{from_bundle_version}{FILE_SUFFIX}")
}

/// Whether `path` is named like a [`delta_file_name`].
pub fn is_delta_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.len() > FILE_SUFFIX.len() && name.ends_with(FILE_SUFFIX))
}

/// The Sparkle deltas in `dir` (not recursive), each read with
/// [`SparkleDelta::read_metadata`], sorted by path.
///
/// Fails on a file with the `.sparkle-delta` suffix of gpui-updater-release
/// 0.4.3 and earlier: Sparkle never applies it, so attaching it to an
/// appcast would only make every client download it before the full update.
pub fn find_deltas(dir: &Path) -> Result<Vec<SparkleDelta>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(Error::io("read directory", dir))? {
        let path = entry.map_err(Error::io("read directory", dir))?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.ends_with(LEGACY_FILE_SUFFIX) {
            return Err(Error::invalid(format!(
                "`{}` ends in `{LEGACY_FILE_SUFFIX}`, which Sparkle never applies as a delta; \
                 make it again, or rename it to end in `{FILE_SUFFIX}` along with its `.json`",
                path.display()
            )));
        }
        if is_delta_file(&path) {
            out.push(SparkleDelta::read_metadata(&path)?);
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Fail unless Sparkle would apply a download saved as `path`.
fn ensure_delta_name(path: &Path) -> Result<()> {
    if path.extension().is_some_and(|e| e == "delta") {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "`{}` must end in `.delta`: Sparkle applies a delta only under that extension \
             (use `delta_file_name`)",
            path.display()
        )))
    }
}

/// Make a delta from `old_app` to `new_app` at `out` with `BinaryDelta`.
///
/// `out` must end in `.delta`, the only extension Sparkle applies as a
/// delta; [`delta_file_name`] gives such a name. When `BinaryDelta` or the
/// verification fails, `out` is removed, so a directory that is uploaded
/// afterwards holds only deltas that work.
pub fn create(
    dist: &SparkleDistribution,
    old_app: &Path,
    new_app: &Path,
    out: &Path,
    options: &DeltaOptions,
) -> Result<SparkleDelta> {
    ensure_delta_name(out)?;
    let tool = tool(dist)?;
    let from = inspect(old_app)?;
    let to = inspect(new_app)?;
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(Error::io("create", parent))?;
    }
    if out.exists() {
        fs::remove_file(out).map_err(Error::io("remove", out))?;
    }
    let mut cmd = Command::new(&tool);
    cmd.arg("create");
    if let Some(version) = options.format_version {
        cmd.arg(format!("--version={version}"));
    }
    if let Some(compression) = &options.compression {
        cmd.arg(format!("--compression={compression}"));
    }
    if options.verbose {
        cmd.arg("--verbose");
    }
    if let Err(e) = run(cmd.arg(old_app).arg(new_app).arg(out)) {
        let _ = fs::remove_file(out);
        return Err(e);
    }
    if options.verify {
        if let Err(e) = verify(dist, old_app, new_app, out) {
            // Never leave a delta that does not rebuild the new app where a
            // later upload step would pick it up.
            let _ = fs::remove_file(out);
            return Err(e);
        }
    }
    Ok(SparkleDelta {
        path: out.to_path_buf(),
        length: fs::metadata(out).map_err(Error::io("stat", out))?.len(),
        from,
        to_bundle_version: to.bundle_version,
        format_version: options.format_version,
    })
}

/// Apply `delta` to `old_app`, writing the new app at `out_app`.
pub fn apply(
    dist: &SparkleDistribution,
    old_app: &Path,
    out_app: &Path,
    delta: &Path,
) -> Result<()> {
    let tool = tool(dist)?;
    if let Some(parent) = out_app.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(Error::io("create", parent))?;
    }
    run(Command::new(tool)
        .arg("apply")
        .arg(old_app)
        .arg(out_app)
        .arg(delta))
    .map(drop)
}

/// Fail unless applying `delta` to `old_app` gives exactly `new_app`.
pub fn verify(
    dist: &SparkleDistribution,
    old_app: &Path,
    new_app: &Path,
    delta: &Path,
) -> Result<()> {
    let scratch = tempfile::tempdir().map_err(Error::io(
        "create a scratch directory in",
        std::env::temp_dir(),
    ))?;
    let name = new_app.file_name().unwrap_or_else(|| "App.app".as_ref());
    let rebuilt = scratch.path().join(name);
    apply(dist, old_app, &rebuilt, delta)?;
    ensure_same_tree(&rebuilt, new_app)
}

fn tool(dist: &SparkleDistribution) -> Result<PathBuf> {
    let tool = dist.binary_delta();
    if tool.is_file() {
        Ok(tool)
    } else {
        Err(Error::invalid(format!(
            "`{}` is missing; Sparkle's release archive ships it in bin/",
            tool.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn app(root: &Path, version: &str, locales: &[&str]) {
        let contents = root.join("Contents");
        fs::create_dir_all(&contents).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert("CFBundleVersion".into(), version.into());
        dict.insert("CFBundleShortVersionString".into(), version.into());
        plist::Value::Dictionary(dict)
            .to_file_xml(contents.join("Info.plist"))
            .unwrap();
        if locales.is_empty() {
            return;
        }
        let b = contents.join("Frameworks/Sparkle.framework/Versions/B");
        fs::create_dir_all(b.join("Resources")).unwrap();
        fs::write(b.join("Sparkle"), vec![0u8; 1234]).unwrap();
        for l in locales {
            fs::create_dir_all(b.join("Resources").join(format!("{l}.lproj"))).unwrap();
        }
        #[cfg(unix)]
        {
            let framework = contents.join("Frameworks/Sparkle.framework");
            std::os::unix::fs::symlink("B", framework.join("Versions/Current")).unwrap();
            std::os::unix::fs::symlink("Versions/Current/Sparkle", framework.join("Sparkle"))
                .unwrap();
            std::os::unix::fs::symlink("Versions/Current/Resources", framework.join("Resources"))
                .unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn inspects_the_old_app_like_generate_appcast() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("Old.app");
        app(
            &old,
            "0.8.14",
            &[
                "Base", "en", "zh_CN", "de", "fr", "ar", "ja", "ko", "nl", "pl", "ru",
            ],
        );
        let info = inspect(&old).unwrap();
        assert_eq!(info.bundle_version, "0.8.14");
        assert_eq!(info.sparkle_executable_size, Some(1234));
        assert_eq!(
            info.sparkle_locales.as_deref(),
            Some("ar,de,fr,ja,ko,nl,pl")
        );
    }

    #[test]
    fn an_app_without_sparkle_has_no_sparkle_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("Old.app");
        app(&old, "0.8.13", &[]);
        let info = inspect(&old).unwrap();
        assert_eq!(
            (info.sparkle_executable_size, info.sparkle_locales),
            (None, None)
        );
    }

    #[test]
    fn metadata_round_trips_and_catches_a_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(delta_file_name("App.dmg", "0.8.14"));
        assert!(
            path.to_string_lossy()
                .ends_with("App.dmg.from-0.8.14.sparkle.delta")
        );
        // What Sparkle's `SUBinaryDeltaUnarchiver` checks.
        assert_eq!(path.extension().unwrap(), "delta");
        assert!(is_delta_file(&path));
        fs::write(&path, b"delta").unwrap();
        let delta = SparkleDelta {
            path: path.clone(),
            length: 5,
            from: OldApp {
                bundle_version: "0.8.14".into(),
                short_version: Some("0.8.14".into()),
                sparkle_executable_size: Some(976_240),
                sparkle_locales: Some("ar,ca".into()),
            },
            to_bundle_version: "0.8.15".into(),
            format_version: Some(4),
        };
        let json = delta.write_metadata().unwrap();
        assert!(
            !fs::read_to_string(&json)
                .unwrap()
                .contains(&dir.path().display().to_string())
        );
        assert_eq!(SparkleDelta::read_metadata(&path).unwrap(), delta);

        let key = SparkleKey::generate();
        assert!(delta.enclosure("https://e/d", Some(&key)).is_err());
        let enclosure = delta
            .enclosure("https://e/App.dmg.from-0.8.14.sparkle.delta", Some(&key))
            .unwrap();
        assert_eq!(enclosure.delta_from, "0.8.14");
        assert_eq!(enclosure.sparkle_executable_size, Some(976_240));
        assert!(enclosure.ed_signature.is_some());

        fs::write(&path, b"changed!").unwrap();
        assert!(SparkleDelta::read_metadata(&path).is_err());
    }

    /// A fake `BinaryDelta`: `create` writes the delta, `apply` rebuilds an
    /// app whose version differs from the new one.
    #[cfg(unix)]
    fn wrong_tool(root: &Path) -> SparkleDistribution {
        use std::os::unix::fs::PermissionsExt as _;
        fs::create_dir_all(root.join("Sparkle.framework")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        let tool = root.join("bin/BinaryDelta");
        fs::write(
            &tool,
            "#!/bin/sh\n\
             case \"$1\" in\n\
             create) echo delta > \"$4\" ;;\n\
             apply) mkdir -p \"$3/Contents\" && cp \"$2/Contents/Info.plist\" \"$3/Contents/\" ;;\n\
             esac\n",
        )
        .unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        SparkleDistribution::from_dir(root).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_delta_that_fails_verification_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let dist = wrong_tool(&dir.path().join("sparkle"));
        let old = dir.path().join("Old.app");
        let new = dir.path().join("New.app");
        app(&old, "0.8.14", &[]);
        app(&new, "0.8.15", &[]);
        let out = dir.path().join("out/App.dmg.from-0.8.14.sparkle.delta");
        create(&dist, &old, &new, &out, &DeltaOptions::default()).unwrap_err();
        assert!(!out.exists(), "an unverified delta was left behind");

        let unchecked = DeltaOptions {
            verify: false,
            ..DeltaOptions::default()
        };
        let delta = create(&dist, &old, &new, &out, &unchecked).unwrap();
        assert_eq!(delta.from.bundle_version, "0.8.14");
        assert_eq!(delta.to_bundle_version, "0.8.15");
        assert!(out.is_file());
    }

    #[test]
    fn a_distribution_without_the_tool_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("Sparkle.framework")).unwrap();
        let dist = SparkleDistribution::from_dir(dir.path()).unwrap();
        let err = verify(&dist, dir.path(), dir.path(), dir.path()).unwrap_err();
        assert!(err.to_string().contains("BinaryDelta"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn create_refuses_a_name_sparkle_would_not_apply() {
        let dir = tempfile::tempdir().unwrap();
        let dist = wrong_tool(&dir.path().join("sparkle"));
        let old = dir.path().join("Old.app");
        let new = dir.path().join("New.app");
        app(&old, "0.8.14", &[]);
        app(&new, "0.8.15", &[]);
        let unchecked = DeltaOptions {
            verify: false,
            ..DeltaOptions::default()
        };
        for name in [
            "App.dmg.from-0.8.14.sparkle-delta",
            "App.DELTA",
            "App.delta.zip",
        ] {
            let out = dir.path().join(name);
            let err = create(&dist, &old, &new, &out, &unchecked).unwrap_err();
            assert!(
                err.to_string().contains("must end in `.delta`"),
                "{name}: {err}"
            );
            assert!(!out.exists(), "{name} was written");
        }
    }

    fn stored_delta(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"delta").unwrap();
        SparkleDelta {
            path: path.clone(),
            length: 5,
            from: OldApp {
                bundle_version: "0.8.14".into(),
                short_version: None,
                sparkle_executable_size: None,
                sparkle_locales: None,
            },
            to_bundle_version: "0.8.15".into(),
            format_version: None,
        }
        .write_metadata()
        .unwrap();
        path
    }

    #[test]
    fn find_deltas_reads_sparkle_deltas_only() {
        let dir = tempfile::tempdir().unwrap();
        let b = stored_delta(dir.path(), &delta_file_name("App-arm64.dmg", "0.8.15"));
        let a = stored_delta(dir.path(), &delta_file_name("App-arm64.dmg", "0.8.14"));
        // A native delta, the full archive, and a bare suffix stay out.
        fs::write(dir.path().join("App-arm64.dmg.from-0.8.14.delta"), b"n").unwrap();
        fs::write(dir.path().join("App-arm64.dmg"), b"full").unwrap();
        fs::write(dir.path().join(FILE_SUFFIX), b"?").unwrap();
        let found: Vec<_> = find_deltas(dir.path())
            .unwrap()
            .into_iter()
            .map(|d| d.path)
            .collect();
        assert_eq!(found, [a, b]);
    }

    #[test]
    fn find_deltas_rejects_the_old_suffix() {
        let dir = tempfile::tempdir().unwrap();
        stored_delta(dir.path(), "App.dmg.from-0.8.14.sparkle-delta");
        let err = find_deltas(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("never applies") && err.contains(FILE_SUFFIX),
            "{err}"
        );
    }
}
