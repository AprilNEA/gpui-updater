//! Linux install strategies, chosen by what the install root is:
//!
//! - **An executable file**: extract the `.tar.gz` and replace that file.
//! - **An `AppImage`** (the artifact ends `.AppImage`): replace the file with
//!   the new `AppImage`. [`current_install_root`](super::current_install_root)
//!   returns the `AppImage` file, not the executable inside its read-only
//!   mount.
//! - **A directory**, such as an `app.app/` that holds `bin/` and `lib/`:
//!   extract the `.tar.gz` and replace the whole directory.
//!
//! Each strategy stages next to the install root, on the same file system, so
//! the final rename is atomic. Linux lets a running process's executable and
//! libraries be renamed or removed: the process keeps the old inodes.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::Installed;
use crate::error::{Error, Result};

pub(crate) fn install(artifact: &Path, install_root: &Path) -> Result<Installed> {
    if install_root.is_dir() {
        let running = std::env::current_exe().ok();
        return install_directory(artifact, install_root, running.as_deref());
    }
    let (parent, name) = split(install_root)?;

    if is_appimage(artifact) {
        replace_binary(artifact, parent, name, install_root)?;
        return Ok(Installed {
            restart_path: Some(install_root.to_path_buf()),
        });
    }

    // A directory is a payload that is already unpacked, such as a binary the
    // engine rebuilt from a delta. Treat it like an extracted archive.
    if artifact.is_dir() {
        let new_bin = find_binary(artifact, name)?;
        replace_binary(&new_bin, parent, name, install_root)?;
        return Ok(Installed {
            restart_path: Some(install_root.to_path_buf()),
        });
    }

    // Stage on the same volume as the target so the final rename is atomic.
    let staging = unique_dir(parent, "gpui-updater-stage")?;
    let extract = extract_tar_gz(artifact, &staging);
    let new_bin = extract.and_then(|()| find_binary(&staging, name));
    let new_bin = match new_bin {
        Ok(p) => p,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    let result = replace_binary(&new_bin, parent, name, install_root);
    let _ = fs::remove_dir_all(&staging);
    result?;

    Ok(Installed {
        restart_path: Some(install_root.to_path_buf()),
    })
}

/// The parent directory and file name of `install_root`.
fn split(install_root: &Path) -> Result<(&Path, &str)> {
    let parent = install_root
        .parent()
        .ok_or_else(|| Error::Install("install root has no parent directory".to_string()))?;
    let name = install_root
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| Error::Install("install root has no file name".to_string()))?;
    Ok((parent, name))
}

fn is_appimage(artifact: &Path) -> bool {
    artifact.is_file()
        && artifact
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("appimage"))
}

/// The `AppImage` file this process runs from: `$APPIMAGE`, when the running
/// executable `exe` is inside `$APPDIR`, the `AppImage`'s mount point. A
/// process started from inside another `AppImage` (a terminal, say) inherits
/// both variables but runs from elsewhere, so it never replaces that file.
pub(crate) fn appimage_root(exe: &Path) -> Option<PathBuf> {
    appimage_root_from(
        std::env::var_os("APPIMAGE"),
        std::env::var_os("APPDIR"),
        exe,
    )
}

fn appimage_root_from(
    appimage: Option<OsString>,
    appdir: Option<OsString>,
    exe: &Path,
) -> Option<PathBuf> {
    let appimage = PathBuf::from(appimage?);
    let appdir = PathBuf::from(appdir?);
    let mounted = appdir.is_absolute() && appdir.parent().is_some() && exe.starts_with(&appdir);
    (mounted && appimage.is_absolute() && appimage.is_file()).then_some(appimage)
}

/// Replace the directory `install_root` with the tree in `artifact`: a
/// `.tar.gz`, or a directory holding an unpacked payload.
///
/// When `running` (the running executable) is inside the install root, the
/// new tree must hold an executable at the same relative path, and that path
/// is the restart path. Otherwise there is no restart path.
fn install_directory(
    artifact: &Path,
    install_root: &Path,
    running: Option<&Path>,
) -> Result<Installed> {
    // Replace the real directory, keeping any symlink that points to it.
    let root = fs::canonicalize(install_root)?;
    let (parent, name) = split(&root)?;
    let relaunch = running
        .and_then(|exe| fs::canonicalize(exe).ok())
        .and_then(|exe| exe.strip_prefix(&root).ok().map(Path::to_path_buf));

    let staging = unique_dir(parent, "gpui-updater-stage")?;
    let result = (|| {
        if artifact.is_dir() {
            copy_tree(artifact, &staging)?;
        } else {
            extract_tar_gz(artifact, &staging)?;
        }
        let tree = new_tree_root(&staging, name, &root)?;
        if let Some(rel) = &relaunch {
            let is_exec = fs::metadata(tree.join(rel))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
            if !is_exec {
                return Err(Error::Install(format!(
                    "the update has no executable {}",
                    rel.display()
                )));
            }
        }
        fs::set_permissions(&tree, fs::metadata(&root)?.permissions())?;
        swap_directory(&tree, &root, parent, name)
    })();
    let _ = fs::remove_dir_all(&staging);
    result?;

    Ok(Installed {
        restart_path: relaunch.map(|rel| root.join(rel)),
    })
}

/// The directory to install from an extraction: its only top-level entry when
/// that is a directory named like the install root, or a wrapper such as
/// `app-1.2.0/` whose name the current tree does not use. Otherwise the
/// extraction itself.
fn new_tree_root(staging: &Path, name: &str, current: &Path) -> Result<PathBuf> {
    let entries = fs::read_dir(staging)?.collect::<io::Result<Vec<_>>>()?;
    match entries.as_slice() {
        [] => Err(Error::Install("the update archive is empty".to_string())),
        [only] if only.file_type()?.is_dir() => {
            let only_name = only.file_name();
            let wrapper = only_name == name || !current.join(&only_name).exists();
            Ok(if wrapper {
                only.path()
            } else {
                staging.to_path_buf()
            })
        }
        _ => Ok(staging.to_path_buf()),
    }
}

/// Move `root` aside, move `tree` into its place, then remove the old tree.
/// When the second move fails, the old tree is moved back.
fn swap_directory(tree: &Path, root: &Path, parent: &Path, name: &str) -> Result<()> {
    let old = parent.join(format!(".{name}.old-{}-{}", std::process::id(), nanos()));
    fs::rename(root, &old)
        .map_err(|e| Error::install_io("could not move the current app aside", &e))?;
    if let Err(e) = fs::rename(tree, root) {
        let _ = fs::rename(&old, root);
        return Err(Error::install_io(
            "could not move the new app into place",
            &e,
        ));
    }
    if let Err(e) = fs::remove_dir_all(&old) {
        tracing::warn!(path = %old.display(), error = %e, "could not remove the previous app");
    }
    Ok(())
}

/// Copy the contents of `src` into the existing directory `dest`, keeping
/// modes and symlinks.
fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    let out = Command::new("cp")
        .arg("-a")
        .arg("--")
        .arg(src.join("."))
        .arg(dest)
        .output()?;
    if !out.status.success() {
        return Err(Error::Install(format!(
            "copying the update failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

fn replace_binary(new_bin: &Path, parent: &Path, name: &str, install_root: &Path) -> Result<()> {
    // Replacing a running executable by rename is permitted on Linux (the old
    // inode stays live for the running process).
    let tmp = parent.join(format!(".{name}.new"));
    let result = (|| {
        fs::copy(new_bin, &tmp)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        fs::rename(&tmp, install_root)
            .map_err(|e| Error::install_io("could not replace binary", &e))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn extract_tar_gz(tarball: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    let out = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(dest)
        .output()?;
    if !out.status.success() {
        return Err(Error::Install(format!(
            "tar extract failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Find the new executable: prefer one named `wanted`, else the first regular
/// file with an executable bit set (searched recursively).
fn find_binary(dir: &Path, wanted: &str) -> Result<PathBuf> {
    let mut fallback = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !path.is_file() {
                continue;
            }
            if path.file_name().and_then(OsStr::to_str) == Some(wanted) {
                return Ok(path);
            }
            let is_exec = fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
            if is_exec && fallback.is_none() {
                fallback = Some(path);
            }
        }
    }
    fallback.ok_or_else(|| Error::Install("no executable found in archive".to_string()))
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

fn unique_dir(parent: &Path, prefix: &str) -> Result<PathBuf> {
    let dir = parent.join(format!(".{prefix}-{}-{}", std::process::id(), nanos()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar(dir: &Path, payload: &Path, entries: &[&str]) -> PathBuf {
        let archive = dir.join(format!("update-{}.tar.gz", nanos()));
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(payload)
                .args(entries)
                .status()
                .unwrap()
                .success()
        );
        archive
    }

    fn write_exec(path: &Path, body: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// An app directory `demo.app` with `bin/demo` and `lib/old.so`.
    fn app_dir(dir: &Path) -> PathBuf {
        let root = dir.join("demo.app");
        write_exec(&root.join("bin/demo"), b"old binary");
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(root.join("lib/old.so"), b"old lib").unwrap();
        root
    }

    /// A new tree for `demo.app` at `payload/<top>`.
    fn new_tree(payload: &Path, top: &str) {
        let tree = payload.join(top);
        write_exec(&tree.join("bin/demo"), b"new binary");
        fs::create_dir_all(tree.join("lib")).unwrap();
        fs::write(tree.join("lib/new.so"), b"new lib").unwrap();
    }

    fn assert_new_tree(root: &Path) {
        assert_eq!(fs::read(root.join("bin/demo")).unwrap(), b"new binary");
        assert_eq!(fs::read(root.join("lib/new.so")).unwrap(), b"new lib");
        assert!(!root.join("lib/old.so").exists());
    }

    fn assert_no_leftovers(dir: &Path) {
        let names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| !n.starts_with(".gpui-updater-stage") && !n.contains(".old-")),
            "{names:?}"
        );
    }

    #[test]
    fn installs_to_temporary_root_and_preserves_it_on_bad_archive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("demo");
        let payload = dir.path().join("payload");
        fs::create_dir(&payload).unwrap();
        fs::write(&root, b"old binary").unwrap();
        fs::write(payload.join("demo"), b"new binary").unwrap();
        // A differently named executable must not override the exact-name match.
        write_exec(&payload.join("helper"), b"wrong binary");
        let archive = tar(dir.path(), &payload, &["."]);

        let installed = install(&archive, &root).unwrap();
        assert_eq!(installed.restart_path, Some(root.clone()));
        assert_eq!(fs::read(&root).unwrap(), b"new binary");
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );

        fs::write(&archive, b"not a tarball").unwrap();
        assert!(matches!(install(&archive, &root), Err(Error::Install(_))));
        assert_eq!(fs::read(&root).unwrap(), b"new binary");
        assert!(!dir.path().join(".demo.new").exists());
        assert_no_leftovers(dir.path());
    }

    #[test]
    fn installs_an_unpacked_payload_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("demo");
        fs::write(&root, b"old binary").unwrap();
        let payload = dir.path().join("rebuilt");
        fs::create_dir(&payload).unwrap();
        fs::write(payload.join("demo"), b"rebuilt binary").unwrap();

        let installed = install(&payload, &root).unwrap();
        assert_eq!(installed.restart_path, Some(root.clone()));
        assert_eq!(fs::read(&root).unwrap(), b"rebuilt binary");
        assert!(!dir.path().join(".demo.new").exists());
    }

    #[test]
    fn replaces_an_appimage_with_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Demo-x86_64.AppImage");
        write_exec(&root, b"old appimage");
        let artifact = dir.path().join("downloads/Demo-2.0-x86_64.AppImage");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, b"new appimage").unwrap();

        let installed = install(&artifact, &root).unwrap();
        assert_eq!(installed.restart_path, Some(root.clone()));
        assert_eq!(fs::read(&root).unwrap(), b"new appimage");
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn finds_the_appimage_only_for_its_own_mount() {
        let dir = tempfile::tempdir().unwrap();
        let appimage = dir.path().join("Demo.AppImage");
        fs::write(&appimage, b"appimage").unwrap();
        let mount = OsString::from("/tmp/.mount_DemoAb12");
        let inside = Path::new("/tmp/.mount_DemoAb12/usr/bin/demo");
        let found = |appimage: Option<OsString>, appdir: Option<OsString>, exe: &Path| {
            appimage_root_from(appimage, appdir, exe)
        };
        assert_eq!(
            found(Some(appimage.clone().into()), Some(mount.clone()), inside),
            Some(appimage.clone())
        );
        // Inherited from another AppImage: this process runs from elsewhere.
        assert_eq!(
            found(
                Some(appimage.clone().into()),
                Some(mount.clone()),
                Path::new("/usr/bin/demo")
            ),
            None
        );
        assert_eq!(found(None, Some(mount.clone()), inside), None);
        assert_eq!(found(Some(appimage.clone().into()), None, inside), None);
        assert_eq!(
            found(Some("Demo.AppImage".into()), Some(mount.clone()), inside),
            None
        );
        assert_eq!(found(Some(appimage.into()), Some("/".into()), inside), None);
        let missing = dir.path().join("gone.AppImage");
        assert_eq!(found(Some(missing.into()), Some(mount), inside), None);
    }

    #[test]
    fn replaces_a_directory_from_a_wrapped_archive() {
        let dir = tempfile::tempdir().unwrap();
        let root = app_dir(dir.path());
        let payload = dir.path().join("payload");
        new_tree(&payload, "demo-2.0.0-linux-x86_64");
        let archive = tar(dir.path(), &payload, &["demo-2.0.0-linux-x86_64"]);

        let installed = install_directory(&archive, &root, Some(&root.join("bin/demo"))).unwrap();
        assert_eq!(
            installed.restart_path,
            Some(fs::canonicalize(&root).unwrap().join("bin/demo"))
        );
        assert_new_tree(&root);
        assert_no_leftovers(dir.path());
    }

    #[test]
    fn replaces_a_directory_from_an_archive_of_the_same_name_or_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let root = app_dir(dir.path());
        let named = dir.path().join("named");
        new_tree(&named, "demo.app");
        install(&tar(dir.path(), &named, &["demo.app"]), &root).unwrap();
        assert_new_tree(&root);

        let root = app_dir(&dir.path().join("second"));
        let contents = dir.path().join("contents");
        new_tree(&contents, "");
        let installed = install(&tar(dir.path(), &contents, &["bin", "lib"]), &root).unwrap();
        // The test binary does not run from the install root.
        assert_eq!(installed.restart_path, None);
        assert_new_tree(&root);
        assert_no_leftovers(dir.path());
    }

    #[test]
    fn a_lone_top_level_folder_the_app_already_has_is_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = app_dir(dir.path());
        let payload = dir.path().join("payload");
        write_exec(&payload.join("bin/demo"), b"new binary");
        install(&tar(dir.path(), &payload, &["bin"]), &root).unwrap();
        assert_eq!(fs::read(root.join("bin/demo")).unwrap(), b"new binary");
        assert!(!root.join("bin/bin").exists());
    }

    #[test]
    fn keeps_the_directory_when_the_update_lacks_the_running_executable() {
        let dir = tempfile::tempdir().unwrap();
        let root = app_dir(dir.path());
        let payload = dir.path().join("payload");
        fs::create_dir_all(payload.join("demo.app/lib")).unwrap();
        fs::write(payload.join("demo.app/lib/new.so"), b"new lib").unwrap();
        let archive = tar(dir.path(), &payload, &["demo.app"]);

        let err = install_directory(&archive, &root, Some(&root.join("bin/demo"))).unwrap_err();
        assert!(matches!(err, Error::Install(_)), "{err:?}");
        assert_eq!(fs::read(root.join("bin/demo")).unwrap(), b"old binary");
        assert!(root.join("lib/old.so").exists());
        assert_no_leftovers(dir.path());
    }

    #[test]
    fn replaces_a_directory_from_an_unpacked_payload_and_keeps_a_symlink_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = app_dir(dir.path());
        let link = dir.path().join("current");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        let payload = dir.path().join("rebuilt");
        new_tree(&payload, "");

        install(&payload, &link).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_new_tree(&root);
        assert!(
            payload.join("bin/demo").exists(),
            "the payload is copied, not moved"
        );
    }
}
