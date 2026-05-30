//! Windows install strategy: replace the running `.exe` via the rename trick.
//!
//! Windows refuses to *delete* a running executable but allows *renaming* it.
//! We move the current exe aside to `*.old.exe`, drop the freshly downloaded
//! exe into place, and leave the stale copy to be cleaned up on next launch.
//!
//! This covers the bare-executable case only. Apps shipped through an installer
//! (MSI / Inno / NSIS) or needing the Restart Manager to release locked sibling
//! DLLs should run their installer instead — that flow is future work and
//! currently returns [`Error::UnsupportedPlatform`].

use std::fs;
use std::path::Path;

use super::Installed;
use crate::error::{Error, Result};

pub(crate) fn install(new_exe: &Path, install_root: &Path) -> Result<Installed> {
    let is_exe = install_root
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    if !is_exe {
        return Err(Error::UnsupportedPlatform("windows installer-based update"));
    }

    let old = install_root.with_extension("old.exe");
    let _ = fs::remove_file(&old);
    if install_root.exists() {
        fs::rename(install_root, &old)
            .map_err(|e| Error::Install(format!("could not rename running exe: {e}")))?;
    }
    if let Err(e) = fs::copy(new_exe, install_root) {
        // Roll back so we never leave the app without its executable.
        if old.exists() {
            let _ = fs::rename(&old, install_root);
        }
        return Err(Error::Install(format!("could not place new exe: {e}")));
    }

    Ok(Installed {
        restart_binary: Some(install_root.to_path_buf()),
    })
}
