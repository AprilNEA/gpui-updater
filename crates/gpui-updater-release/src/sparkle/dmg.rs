//! Copy the app out of a disk image, for example an older release's DMG to
//! build deltas from. macOS only (`hdiutil`, `ditto`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::util::run;

/// Mount `dmg` read-only, copy its app (the one named `app_name`, or the only
/// `.app` at the image's root) into `dest_dir`, and unmount. Returns the
/// copied app's path.
pub fn extract_app(dmg: &Path, dest_dir: &Path, app_name: Option<&str>) -> Result<PathBuf> {
    let mount = tempfile::Builder::new()
        .prefix("gpui-updater-dmg")
        .tempdir()
        .map_err(Error::io("create a mount point in", std::env::temp_dir()))?;
    run(Command::new("hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noautoopen",
            "-mountpoint",
        ])
        .arg(mount.path())
        .arg(dmg))?;
    let attached = Attached(mount.path().to_path_buf());
    let source = find_app(&attached.0, app_name)?;
    fs::create_dir_all(dest_dir).map_err(Error::io("create", dest_dir))?;
    let name = source
        .file_name()
        .ok_or_else(|| Error::invalid("the app has no name"))?;
    let target = dest_dir.join(name);
    if target.exists() {
        fs::remove_dir_all(&target).map_err(Error::io("remove", &target))?;
    }
    run(Command::new("ditto").arg(&source).arg(&target))?;
    attached.detach()?;
    Ok(target)
}

fn find_app(root: &Path, app_name: Option<&str>) -> Result<PathBuf> {
    if let Some(name) = app_name {
        let path = root.join(name);
        return if path.is_dir() {
            Ok(path)
        } else {
            Err(Error::invalid(format!("the disk image has no `{name}`")))
        };
    }
    let apps: Vec<PathBuf> = fs::read_dir(root)
        .map_err(Error::io("read directory", root))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "app") && p.is_dir())
        .collect();
    match apps.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(Error::invalid("the disk image has no .app at its root")),
        _ => Err(Error::invalid(
            "the disk image has several apps; name the one to copy",
        )),
    }
}

/// A mounted image, unmounted on drop if [`detach`](Self::detach) was not
/// reached.
struct Attached(PathBuf);

impl Attached {
    fn detach(self) -> Result<()> {
        let result = detach(&self.0);
        std::mem::forget(self);
        result
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = detach(&self.0);
    }
}

fn detach(mount: &Path) -> Result<()> {
    if run(Command::new("hdiutil").arg("detach").arg(mount)).is_ok() {
        return Ok(());
    }
    run(Command::new("hdiutil")
        .args(["detach", "-force"])
        .arg(mount))
    .map(drop)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn finds_the_only_app_or_the_named_one() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_app(dir.path(), None).is_err());
        fs::create_dir(dir.path().join("A.app")).unwrap();
        fs::write(dir.path().join("Applications"), "").unwrap();
        assert_eq!(
            find_app(dir.path(), None).unwrap(),
            dir.path().join("A.app")
        );
        fs::create_dir(dir.path().join("B.app")).unwrap();
        assert!(find_app(dir.path(), None).is_err());
        assert_eq!(
            find_app(dir.path(), Some("B.app")).unwrap(),
            dir.path().join("B.app")
        );
        assert!(find_app(dir.path(), Some("C.app")).is_err());
    }
}
