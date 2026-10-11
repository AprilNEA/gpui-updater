//! Filesystem helpers shared by delta creation and application.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// All entries under `root` as sorted `/`-separated relative paths, parents
/// before children. Symlinks are listed but not followed.
pub(super) fn walk(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    walk_into(root, "", &mut out)?;
    Ok(out)
}

fn walk_into(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            Error::Delta(format!("non-UTF-8 file name under `{}`", dir.display()))
        })?;
        names.push(name.to_string());
    }
    names.sort();
    for name in names {
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let path = dir.join(&name);
        let is_dir = fs::symlink_metadata(&path)?.is_dir();
        out.push(rel.clone());
        if is_dir {
            walk_into(&path, &rel, out)?;
        }
    }
    Ok(())
}

/// `root` joined with a `/`-separated relative path (`root` itself when empty).
pub(super) fn join_rel(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        rel.split('/')
            .fold(root.to_path_buf(), |p, part| p.join(part))
    }
}

/// A relative path for messages; the root of a file-rooted delta has none.
pub(super) fn display_rel(rel: &str) -> &str {
    if rel.is_empty() { "<root>" } else { rel }
}

/// Lowercase hex SHA-256 of `data`.
pub(super) fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(data) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(unix)]
pub(super) fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
pub(super) fn mode_of(meta: &fs::Metadata) -> u32 {
    if meta.is_dir() {
        0o755
    } else if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(unix)]
pub(super) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Never recreate setuid/setgid bits from a downloaded header.
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o1777))?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(mode & 0o200 == 0);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn make_symlink(target: &str, dest: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, dest)?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn make_symlink(_target: &str, dest: &Path) -> Result<()> {
    Err(Error::Delta(format!(
        "symlinks are not supported on this platform (`{}`)",
        dest.display()
    )))
}

/// Best effort: restore owner write permission on a partial output so it can
/// be removed.
pub(super) fn make_writable(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::symlink_metadata(dir) {
            if meta.is_dir() {
                let mode = meta.permissions().mode();
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700));
                for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                    make_writable(&entry.path());
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}
