//! Replacing the installed application with a freshly downloaded artifact.
//!
//! The strategy is platform-specific:
//! - **macOS**: mount the `.dmg`, copy the new `.app` onto the same volume as
//!   the current bundle with `ditto` (preserving its existing notarized
//!   signature), then swap it into place. The new bundle carries its own
//!   signature, so nothing is re-signed at runtime.
//! - **Linux**: extract the `.tar.gz` and atomically replace the binary.
//! - **Windows**: rename-in-place for a bare `.exe`, or a
//!   staged msiexec handoff for an `.msi` (applied after the app exits, via
//!   the restart path).

use std::path::{Path, PathBuf};

use crate::error::Result;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "windows")]
mod windows;

mod ownership;
pub use ownership::{Ownership, OwnershipHints, PackageManager, ownership};

/// Outcome of installing an update.
#[derive(Debug, Clone)]
pub struct Installed {
    /// Path to relaunch into the new version, fed to `App::set_restart_path` by
    /// the GPUI integration. This is what GPUI's platform `restart` consumes, so
    /// it is platform-shaped: on macOS the `.app` bundle (relaunched via `open`,
    /// which needs the bundle, not the inner Mach-O — that would open in
    /// Terminal); on Linux the executable itself; on Windows the executable —
    /// or, for a staged MSI, the generated apply script that installs the
    /// package and relaunches the app (GPUI's Windows `restart` spawns the path
    /// only after this process exits, which is the ordering msiexec needs).
    /// `None` means the running location was updated in place with no new path
    /// to restart into.
    pub restart_path: Option<PathBuf>,
}

/// How much of the Windows Installer UI an `.msi` update shows. The package
/// installs after the app exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum MsiUi {
    /// A progress bar and no questions (`/passive`).
    #[default]
    Passive,
    /// The basic UI: progress, a Cancel button, and error dialogs (`/qb`).
    Basic,
    /// No UI at all (`/quiet`). A per-machine package cannot ask for
    /// elevation then, so use it for per-user packages or an elevated app.
    Quiet,
}

impl MsiUi {
    /// The msiexec option for this UI level.
    #[cfg_attr(
        not(any(target_os = "windows", test)),
        expect(dead_code, reason = "msiexec runs on Windows only")
    )]
    pub(crate) fn flag(self) -> &'static str {
        match self {
            Self::Passive => "/passive",
            Self::Basic => "/qb",
            Self::Quiet => "/quiet",
        }
    }
}

/// How [`install_with`] installs an update.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct InstallOptions {
    /// The Windows Installer UI for an `.msi` update.
    pub msi_ui: MsiUi,
}

impl InstallOptions {
    /// The defaults: [`MsiUi::Passive`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the Windows Installer UI for an `.msi` update.
    #[must_use]
    pub fn msi_ui(mut self, ui: MsiUi) -> Self {
        self.msi_ui = ui;
        self
    }
}

/// Install a downloaded update artifact over `install_root`.
///
/// - `artifact`: the downloaded file (`.dmg` on macOS, `.tar.gz` or
///   `.AppImage` on Linux, `.exe` or `.msi` on Windows). On macOS and Linux
///   it may also be a directory holding an already-unpacked payload (the
///   `.app` bundle or the executable), which is what
///   [`UpdateEngine::download`](crate::UpdateEngine::download) returns after
///   rebuilding an update from a delta.
/// - `install_root`: what to replace — on macOS the `.app` bundle directory;
///   on Linux the executable file, the `AppImage` file, or a directory that
///   holds the whole app (replaced as a whole from the `.tar.gz`). Use
///   [`current_install_root`] to derive it, or set
///   [`EngineConfig::install_root`](crate::EngineConfig::install_root) to an
///   app directory.
///
/// # Errors
/// Returns [`Error::Install`](crate::Error::Install) on any failure during
/// mounting, copying, or swapping, and
/// [`Error::UnsupportedPlatform`](crate::Error::UnsupportedPlatform) on
/// platforms without an install strategy.
pub fn install(artifact: &Path, install_root: &Path) -> Result<Installed> {
    install_with(artifact, install_root, &InstallOptions::new())
}

/// [`install`] with `options`, such as the Windows Installer UI for an
/// `.msi`.
///
/// # Errors
/// As [`install`].
pub fn install_with(
    artifact: &Path,
    install_root: &Path,
    options: &InstallOptions,
) -> Result<Installed> {
    #[cfg(target_os = "macos")]
    {
        let _ = options;
        macos::install(artifact, install_root)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = options;
        linux::install(artifact, install_root)
    }
    #[cfg(target_os = "windows")]
    {
        windows::install(artifact, install_root, options)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (artifact, install_root, options);
        Err(crate::Error::UnsupportedPlatform(std::env::consts::OS))
    }
}

/// Remove what an earlier install left next to `install_root` because the
/// old version was still running: on Windows, the previous executable
/// (`<app>.old.exe`) that a bare-`.exe` install renames aside. Call it at
/// startup. It does nothing when there is nothing to remove, and it ignores
/// failures, because the next call tries again.
pub fn remove_leftovers(install_root: &Path) {
    #[cfg(target_os = "windows")]
    windows::remove_leftovers(install_root);
    #[cfg(not(target_os = "windows"))]
    let _ = install_root;
}

/// Best-effort path of the currently installed application to replace.
///
/// On macOS this walks up from the running executable to the enclosing `.app`
/// bundle. On Linux it is the `AppImage` file (`$APPIMAGE`) when the app runs
/// from inside that `AppImage`'s mount (`$APPDIR`), and otherwise the running
/// executable. On Windows it is the running executable.
///
/// # Errors
/// Returns [`Error::Io`](crate::Error::Io) if the current executable path
/// cannot be determined.
pub fn current_install_root() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    #[cfg(target_os = "macos")]
    {
        Ok(macos::bundle_root(&exe).unwrap_or(exe))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(linux::appimage_root(&exe).unwrap_or(exe))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(exe)
    }
}

/// Check, before anything is downloaded, that an update can be installed over
/// `install_root`. `artifact_name` is the release asset's file name, which
/// decides how it installs.
///
/// Installing replaces `install_root` from inside its parent directory, so
/// that directory must be writable — except for a Windows `.msi`, which
/// msiexec installs with its own elevation. On macOS an app that runs from
/// an App Translocation path is rejected too: macOS runs a quarantined app
/// from a randomized read-only copy until the user moves it.
///
/// # Errors
/// [`Error::Location`](crate::Error::Location) for an App Translocation path
/// or a read-only volume (such as a mounted disk image), and
/// [`Error::Permission`](crate::Error::Permission) when the user may not
/// write to the parent directory. Any other failure to probe the directory
/// is left for the install to report.
pub fn check_location(install_root: &Path, artifact_name: &str) -> Result<()> {
    if cfg!(target_os = "macos")
        && install_root
            .components()
            .any(|c| c.as_os_str() == "AppTranslocation")
    {
        return Err(crate::Error::Location(
            "the app runs from a read-only App Translocation copy; move it to /Applications and open it again"
                .into(),
        ));
    }
    let installer_elevates =
        cfg!(target_os = "windows") && artifact_name.to_ascii_lowercase().ends_with(".msi");
    if installer_elevates {
        return Ok(());
    }
    match install_root.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => probe_write(dir),
        _ => Ok(()),
    }
}

/// Create and remove a file in `dir` to learn whether the user may write
/// there.
fn probe_write(dir: &Path) -> Result<()> {
    use std::io::ErrorKind;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let probe = dir.join(format!(
        ".gpui-updater-probe-{}-{nanos}",
        std::process::id()
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::ReadOnlyFilesystem => {
            Err(crate::Error::Location(format!(
                "{} is on a read-only volume, such as a mounted disk image; move the app to a writable folder",
                dir.display()
            )))
        }
        Err(e) if e.kind() == ErrorKind::PermissionDenied => Err(crate::Error::Permission(
            format!("no permission to write to {}", dir.display()),
        )),
        Err(e) => {
            tracing::debug!(dir = %dir.display(), error = %e, "could not probe the install directory");
            Ok(())
        }
    }
}

#[cfg(test)]
mod location_tests {
    use super::*;
    #[cfg(unix)]
    use crate::error::{Error, ErrorKind};

    #[test]
    fn accepts_a_writable_folder_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        check_location(&dir.path().join("App.app"), "App.dmg").unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn leaves_a_missing_folder_for_the_install_to_report() {
        let dir = tempfile::tempdir().unwrap();
        check_location(&dir.path().join("gone/App.app"), "App.dmg").unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rejects_app_translocation() {
        let root = Path::new("/private/var/folders/x/T/AppTranslocation/1A2B/d/App.app");
        let err = check_location(root, "App.dmg").unwrap_err();
        assert!(matches!(err, Error::Location(_)), "{err:?}");
        assert_eq!(err.kind(), ErrorKind::Location);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_folder_the_user_may_not_write() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        // A privileged user may write anyway, so the check passes there.
        let privileged = std::fs::write(locked.join("probe"), b"").is_ok();
        let result = check_location(&locked.join("App.app"), "App.dmg");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if privileged {
            return;
        }
        let err = result.unwrap_err();
        assert!(matches!(err, Error::Permission(_)), "{err:?}");
        assert_eq!(err.kind(), ErrorKind::Permission);
    }
}

#[cfg(test)]
mod options_tests {
    use super::*;

    #[test]
    fn msi_ui_levels_map_to_msiexec_options() {
        assert_eq!(InstallOptions::new().msi_ui, MsiUi::Passive);
        assert_eq!(
            InstallOptions::new().msi_ui(MsiUi::Basic).msi_ui,
            MsiUi::Basic
        );
        assert_eq!(MsiUi::Passive.flag(), "/passive");
        assert_eq!(MsiUi::Basic.flag(), "/qb");
        assert_eq!(MsiUi::Quiet.flag(), "/quiet");
    }
}
