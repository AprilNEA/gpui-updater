//! Private scratch space for downloads and installs.
//!
//! A downloaded update is verified first and installed later, so whoever can
//! write to the directory holding it between those two steps can swap it for
//! something else. Every directory the updater creates is therefore private
//! to the current user (`0700` on Unix) and created exclusively under a
//! name other users cannot predict, so a directory planted in advance is never
//! reused. File names taken from release metadata are reduced to a single
//! path component, so a hostile name cannot point outside the directory.

use std::fs::DirBuilder;
use std::hash::{BuildHasher as _, RandomState};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// How many fresh names to try when a candidate directory already exists.
const ATTEMPTS: u32 = 16;

/// Create a new directory under the system temp directory that only the
/// current user can access, named `<prefix>-<pid>-<random>`.
///
/// # Errors
/// Returns the I/O error from creating the directory.
pub(crate) fn private_temp_dir(prefix: &str) -> io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let mut attempt = 0;
    loop {
        let name = format!("{prefix}-{}-{:016x}", std::process::id(), unpredictable());
        let dir = base.join(name);
        match private_dir_builder().create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < ATTEMPTS => {
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Create `dir` and any missing parents. Directories created here are
/// private to the current user; existing ones are left as they are.
///
/// # Errors
/// Returns the I/O error from creating a directory.
pub(crate) fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    private_dir_builder().recursive(true).create(dir)
}

/// The last path component of a file name published in release metadata, or
/// `fallback` when that is not a plain file name (empty, `.`, `..`, or a
/// Windows drive prefix). Joining the result onto a directory therefore never
/// leaves it.
pub(crate) fn safe_file_name<'a>(name: &'a str, fallback: &'a str) -> &'a str {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut components = Path::new(base).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => base,
        _ => fallback,
    }
}

#[cfg(unix)]
fn private_dir_builder() -> DirBuilder {
    use std::os::unix::fs::DirBuilderExt as _;
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
}

/// Windows temp directories are per user and inherit an ACL that already
/// keeps other users out.
#[cfg(not(unix))]
fn private_dir_builder() -> DirBuilder {
    DirBuilder::new()
}

/// A number other local users cannot guess: std seeds `RandomState` from the
/// operating system's random number generator.
fn unpredictable() -> u64 {
    RandomState::new().hash_one(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_fresh_private_directories() {
        let first = private_temp_dir("gpui-updater-test-staging").unwrap();
        let second = private_temp_dir("gpui-updater-test-staging").unwrap();
        assert_ne!(first, second);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&first).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "group/other bits set: {mode:o}");
        }
        std::fs::remove_dir(first).unwrap();
        std::fs::remove_dir(second).unwrap();
    }

    #[test]
    fn creates_missing_parents_privately() {
        let root = private_temp_dir("gpui-updater-test-ensure").unwrap();
        let nested = root.join("a/b");
        ensure_private_dir(&nested).unwrap();
        // An existing directory is fine.
        ensure_private_dir(&nested).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(root.join("a"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "group/other bits set: {mode:o}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keeps_published_names_inside_the_directory() {
        let safe = |name| safe_file_name(name, "update");
        assert_eq!(safe("App-1.2.3.dmg"), "App-1.2.3.dmg");
        assert_eq!(safe("dist/App.dmg"), "App.dmg");
        assert_eq!(safe(r"dist\App.msi"), "App.msi");
        assert_eq!(safe("../../../.zshrc"), ".zshrc");
        assert_eq!(safe("/etc/passwd"), "passwd");
        for hostile in ["", ".", "..", "a/..", "a/", r"C:\"] {
            assert_eq!(safe(hostile), "update", "{hostile:?}");
        }
        #[cfg(windows)]
        assert_eq!(safe("C:evil.exe"), "update");
    }
}
