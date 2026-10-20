//! Windows install strategies: the bare-`.exe` rename trick (also for an
//! executable rebuilt from deltas), or a staged MSI handoff.
//!
//! **Bare exe** (artifact ends `.exe`): Windows refuses to *delete* a running
//! executable but allows *renaming* it. We move the current exe aside to
//! `*.old.exe`, drop the freshly downloaded exe into place, and leave the
//! stale copy for [`remove_leftovers`](super::remove_leftovers) to delete once
//! the new version runs.
//!
//! **Rebuilt exe** (artifact is a directory): with the `delta` feature, the
//! engine rebuilds the new executable from deltas into a directory, under the
//! install root's file name. That file goes into place exactly like a
//! downloaded bare exe. The engine only rebuilds when the install root and
//! the release asset are both `.exe` files (see `accepts_rebuilt`).
//!
//! **MSI** (artifact ends `.msi`): running msiexec from inside the app would
//! race ourselves — a well-authored per-user package closes the running app
//! (`WiX`'s `util:CloseApplication`), i.e. the very process driving this install,
//! killing the updater mid-`Installing`. So the MSI is *staged* instead: we
//! write a small `.cmd` beside the verified artifact that installs the package
//! (`/passive /norestart` by default — progress UI, no surprise reboot; see
//! [`MsiUi`](super::MsiUi)), logs to `<package>.log`, and relaunches the app,
//! and return that script as the restart path. GPUI's Windows
//! `restart` waits for this process to exit before spawning the restart path,
//! which is exactly the ordering msiexec needs: by the time the script runs,
//! the app's files are no longer in use (a still-running background helper is
//! the package's `CloseApplication` problem, as on a manual install).
//!
//! Dispatch is on the **artifact's** extension, not the install root's: the
//! artifact is what we're about to execute or copy, so it must prove its own
//! shape. (Dispatching on the root alone would let a mismatched payload — say
//! an `.msi` served under an `exe` manifest format — be copied over the
//! running binary byte-for-byte.) A directory artifact must hold a file with
//! the install root's name, and only that file is installed.

use std::fs;
use std::path::Path;

use super::{InstallOptions, Installed, MsiUi};
use crate::error::{Error, Result};

pub(crate) fn install(
    artifact: &Path,
    install_root: &Path,
    options: &InstallOptions,
) -> Result<Installed> {
    if artifact.is_dir() {
        return install_rebuilt_exe(artifact, install_root);
    }
    match artifact
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("exe") => install_bare_exe(artifact, install_root),
        Some("msi") => stage_msi(artifact, install_root, options.msi_ui),
        _ => Err(Error::UnsupportedPlatform(
            "windows update artifact (expected .exe or .msi)",
        )),
    }
}

/// Replace the running `.exe` in place via the rename trick.
fn install_bare_exe(new_exe: &Path, install_root: &Path) -> Result<Installed> {
    let root_is_exe = install_root
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    if !root_is_exe {
        return Err(Error::Install(format!(
            "bare-exe update needs an .exe install root, got {}",
            install_root.display()
        )));
    }

    let old = install_root.with_extension("old.exe");
    let _ = fs::remove_file(&old);
    if install_root.exists() {
        fs::rename(install_root, &old)
            .map_err(|e| Error::install_io("could not rename running exe", &e))?;
    }
    if let Err(e) = fs::copy(new_exe, install_root) {
        // Roll back so we never leave the app without its executable.
        if old.exists() {
            let _ = fs::rename(&old, install_root);
        }
        return Err(Error::install_io("could not place new exe", &e));
    }

    Ok(Installed {
        restart_path: Some(install_root.to_path_buf()),
    })
}

/// Install the executable that the engine rebuilt from deltas into `dir`,
/// where it has the install root's file name.
fn install_rebuilt_exe(dir: &Path, install_root: &Path) -> Result<Installed> {
    let exe = install_root
        .file_name()
        .map(|name| dir.join(name))
        .filter(|exe| fs::symlink_metadata(exe).is_ok_and(|m| m.is_file()))
        .ok_or_else(|| {
            Error::Install(format!(
                "{} holds no rebuilt {}",
                dir.display(),
                install_root.display()
            ))
        })?;
    install_bare_exe(&exe, install_root)
}

/// Whether an executable rebuilt from deltas may replace `install_root` when
/// the release's full artifact is `artifact_name`: both must be `.exe` files.
#[cfg(feature = "delta")]
pub(crate) fn accepts_rebuilt(install_root: &Path, artifact_name: &str) -> bool {
    let is_exe = |path: &Path| {
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    };
    is_exe(install_root) && !install_root.is_dir() && is_exe(Path::new(artifact_name))
}

/// Stage a verified `.msi`: write the apply script next to it and hand that
/// back as the restart path. Nothing is installed until the app exits.
fn stage_msi(msi: &Path, install_root: &Path, ui: MsiUi) -> Result<Installed> {
    // The relaunch target is the current exe path: an upgrade MSI installs to
    // the same per-user folder, so the path stays valid and holds the new
    // binary once msiexec finishes.
    let script_path = msi.with_extension("apply.cmd");
    fs::write(&script_path, apply_script(msi, install_root, ui)?)
        .map_err(|e| Error::install_io("could not stage MSI apply script", &e))?;
    Ok(Installed {
        restart_path: Some(script_path),
    })
}

/// The staged apply script: install the MSI, then relaunch the app — but only
/// on success, so a failed upgrade leaves the old (still installed) version
/// for the user to start rather than masking the failure with a relaunch.
/// Exit code 3010 means the install succeeded and a reboot will finish it,
/// which `/norestart` leaves to the user, so the app still relaunches.
/// msiexec writes a verbose log to `<package>.log` for failed upgrades.
fn apply_script(msi: &Path, relaunch: &Path, ui: MsiUi) -> Result<String> {
    let log = cmd_quoted(&msi.with_extension("log"))?;
    let msi = cmd_quoted(msi)?;
    let relaunch = cmd_quoted(relaunch)?;
    let ui = ui.flag();
    Ok(format!(
        "@echo off\r\n\
         rem Generated by gpui-updater: applies the staged MSI update and\r\n\
         rem relaunches the app. Run automatically after the app exits.\r\n\
         msiexec /i {msi} {ui} /norestart /l*v {log}\r\n\
         set code=%errorlevel%\r\n\
         if %code% equ 3010 set code=0\r\n\
         if %code% neq 0 exit /b %code%\r\n\
         start \"\" {relaunch}\r\n"
    ))
}

/// Delete the `.old.exe` a bare-exe install left beside `install_root`.
pub(crate) fn remove_leftovers(install_root: &Path) {
    let root_is_exe = install_root
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    let old = install_root.with_extension("old.exe");
    if root_is_exe && old.is_file() {
        if let Err(e) = fs::remove_file(&old) {
            tracing::debug!(path = %old.display(), error = %e, "could not remove the previous exe");
        }
    }
}

/// Quote a path for cmd.exe. Refuses the characters cmd cannot carry safely in
/// a quoted string rather than emitting a script that would mis-parse.
fn cmd_quoted(path: &Path) -> Result<String> {
    let s = path
        .to_str()
        .ok_or_else(|| Error::Install(format!("non-UTF-8 path: {}", path.display())))?;
    if s.contains('"') || s.contains('%') || s.contains('\r') || s.contains('\n') {
        return Err(Error::Install(format!(
            "path cannot be quoted for cmd.exe: {s}"
        )));
    }
    Ok(format!("\"{s}\""))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_rejects_unknown_artifact_kinds() {
        // Neither .exe nor .msi: never copied, never staged.
        let err = install(
            Path::new("C:\\dl\\app.zip"),
            Path::new("C:\\app\\app.exe"),
            &InstallOptions::new(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::UnsupportedPlatform(_)));
    }

    #[test]
    fn bare_exe_requires_an_exe_install_root() {
        // An .exe payload aimed at a non-exe root (e.g. a directory) is a
        // config error, not something to clobber.
        let err = install(
            Path::new("C:\\dl\\app.exe"),
            Path::new("C:\\app"),
            &InstallOptions::new(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Install(_)));
    }

    #[test]
    fn exe_rename_trick_swaps_and_keeps_the_old_copy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app.exe");
        let new = dir.path().join("download.exe");
        fs::write(&root, b"old").unwrap();
        fs::write(&new, b"new").unwrap();

        let installed = install(&new, &root, &InstallOptions::new()).unwrap();

        assert_eq!(installed.restart_path.as_deref(), Some(root.as_path()));
        assert_eq!(fs::read(&root).unwrap(), b"new");
        assert_eq!(fs::read(root.with_extension("old.exe")).unwrap(), b"old");

        // Once the new version runs, the old copy can go.
        super::super::remove_leftovers(&root);
        assert!(!root.with_extension("old.exe").exists());
        assert_eq!(fs::read(&root).unwrap(), b"new");
        super::super::remove_leftovers(&root);
    }

    #[test]
    fn a_rebuilt_directory_installs_the_exe_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("App.exe");
        let rebuilt = dir.path().join("delta-1.1.0");
        fs::create_dir(&rebuilt).unwrap();
        fs::write(&root, b"old").unwrap();
        fs::write(rebuilt.join("App.exe"), b"new").unwrap();

        let installed = install(&rebuilt, &root, &InstallOptions::new()).unwrap();

        assert_eq!(installed.restart_path.as_deref(), Some(root.as_path()));
        assert_eq!(fs::read(&root).unwrap(), b"new");
        assert_eq!(fs::read(root.with_extension("old.exe")).unwrap(), b"old");
    }

    #[test]
    fn a_rebuilt_directory_without_the_exe_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("App.exe");
        let rebuilt = dir.path().join("delta-1.1.0");
        fs::create_dir_all(rebuilt.join("App.exe.d")).unwrap();
        fs::write(&root, b"old").unwrap();
        fs::write(rebuilt.join("Other.exe"), b"new").unwrap();

        let err = install(&rebuilt, &root, &InstallOptions::new()).unwrap_err();

        assert!(matches!(err, Error::Install(_)), "{err}");
        assert_eq!(fs::read(&root).unwrap(), b"old");
        assert!(!root.with_extension("old.exe").exists());
    }

    #[cfg(feature = "delta")]
    #[test]
    fn only_a_bare_exe_accepts_a_rebuilt_update() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("App.exe");
        fs::write(&exe, b"app").unwrap();
        assert!(accepts_rebuilt(&exe, "App-1.1.0-x64.exe"));
        assert!(accepts_rebuilt(&exe, "APP.EXE"));
        assert!(!accepts_rebuilt(&exe, "App-1.1.0-x64.msi"));
        assert!(!accepts_rebuilt(&exe, "App-1.1.0-x64.zip"));
        assert!(!accepts_rebuilt(dir.path(), "App.exe"));
        let folder = dir.path().join("Folder.exe");
        fs::create_dir(&folder).unwrap();
        assert!(!accepts_rebuilt(&folder, "App.exe"));
    }

    #[test]
    fn msi_stages_an_apply_script_as_the_restart_path() {
        let dir = tempfile::tempdir().unwrap();
        let msi = dir.path().join("App-v2.msi");
        fs::write(&msi, b"msi").unwrap();
        let root = dir.path().join("App.exe");

        let installed = install(&msi, &root, &InstallOptions::new()).unwrap();

        let script_path = installed.restart_path.unwrap();
        assert_eq!(script_path, msi.with_extension("apply.cmd"));
        let script = fs::read_to_string(script_path).unwrap();
        assert!(script.contains(&format!(
            "msiexec /i \"{}\" /passive /norestart /l*v \"{}\"",
            msi.display(),
            msi.with_extension("log").display()
        )));
        assert!(script.contains(&format!("start \"\" \"{}\"", root.display())));
        // Relaunch is gated on msiexec succeeding; 3010 (reboot later) counts.
        assert!(script.contains("if %code% equ 3010 set code=0"));
        assert!(script.contains("if %code% neq 0 exit /b %code%"));
        let quiet = InstallOptions::new().msi_ui(MsiUi::Quiet);
        let installed = install(&msi, &root, &quiet).unwrap();
        let script = fs::read_to_string(installed.restart_path.unwrap()).unwrap();
        assert!(script.contains("/quiet /norestart"));
        // The MSI itself must not have been touched, let alone executed.
        assert_eq!(fs::read(&msi).unwrap(), b"msi");
    }

    #[test]
    fn quoting_refuses_paths_cmd_cannot_carry() {
        for bad in ["C:\\a\"b\\x.msi", "C:\\a%b\\x.msi"] {
            assert!(matches!(
                apply_script(
                    Path::new(bad),
                    Path::new("C:\\app\\App.exe"),
                    MsiUi::Passive
                ),
                Err(Error::Install(_))
            ));
        }
    }
}
