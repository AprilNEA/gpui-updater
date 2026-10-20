//! Compare two directory trees exactly, without following symbolic links:
//! every entry's kind, permissions, contents (SHA-256), and link target.
//!
//! Delta verification uses this to prove that applying a delta to the old
//! build gives back the new build, before the delta is published.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::error::{Error, Result};
use crate::util::sha256_file;

/// One entry of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Entry {
    /// A directory and its permission bits.
    Dir {
        /// Permission bits (`0` on platforms without them).
        mode: u32,
    },
    /// A regular file.
    File {
        /// Permission bits (`0` on platforms without them).
        mode: u32,
        /// Lowercase hex SHA-256.
        sha256: String,
    },
    /// A symbolic link and its target, as stored.
    Symlink {
        /// The link's target.
        target: String,
    },
}

/// Every entry of a tree, keyed by `/`-separated path relative to its root.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreeDigest {
    entries: BTreeMap<String, Entry>,
}

impl TreeDigest {
    /// Digest the tree at `root`.
    pub fn of(root: &Path) -> Result<Self> {
        let mut digest = Self::default();
        digest.walk(root, root)?;
        Ok(digest)
    }

    /// The entries, in path order.
    pub fn entries(&self) -> &BTreeMap<String, Entry> {
        &self.entries
    }

    /// Describe every difference from `other`; empty when the trees match.
    pub fn differences(&self, other: &Self) -> Vec<String> {
        let mut out = Vec::new();
        for (path, entry) in &self.entries {
            match other.entries.get(path) {
                None => out.push(format!("only in the first tree: {path}")),
                Some(theirs) if theirs != entry => {
                    out.push(format!("differs: {path} ({entry:?} vs {theirs:?})"));
                }
                Some(_) => {}
            }
        }
        for path in other.entries.keys() {
            if !self.entries.contains_key(path) {
                out.push(format!("only in the second tree: {path}"));
            }
        }
        out
    }

    fn walk(&mut self, root: &Path, dir: &Path) -> Result<()> {
        let mut children: Vec<_> = fs::read_dir(dir)
            .map_err(Error::io("read directory", dir))?
            .collect::<std::io::Result<_>>()
            .map_err(Error::io("read directory", dir))?;
        children.sort_by_key(fs::DirEntry::file_name);
        for child in children {
            let path = child.path();
            let meta = fs::symlink_metadata(&path).map_err(Error::io("stat", &path))?;
            let relative = path
                .strip_prefix(root)
                .map_err(|_| Error::invalid(format!("`{}` escaped its tree", path.display())))?
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let entry = if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).map_err(Error::io("read link", &path))?;
                Entry::Symlink {
                    target: target.to_string_lossy().into_owned(),
                }
            } else if meta.is_dir() {
                self.walk(root, &path)?;
                Entry::Dir { mode: mode(&meta) }
            } else {
                Entry::File {
                    mode: mode(&meta),
                    sha256: sha256_file(&path)?,
                }
            };
            self.entries.insert(relative, entry);
        }
        Ok(())
    }
}

/// Fail with every difference unless the trees at `a` and `b` match.
pub fn ensure_same_tree(a: &Path, b: &Path) -> Result<()> {
    let differences = TreeDigest::of(a)?.differences(&TreeDigest::of(b)?);
    if differences.is_empty() {
        return Ok(());
    }
    let shown: Vec<_> = differences.iter().take(20).cloned().collect();
    Err(Error::invalid(format!(
        "`{}` and `{}` differ in {} entries:\n{}",
        a.display(),
        b.display(),
        differences.len(),
        shown.join("\n")
    )))
}

#[cfg(unix)]
fn mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode(_: &fs::Metadata) -> u32 {
    0
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn tree(root: &Path, body: &str) {
        fs::create_dir_all(root.join("Contents/MacOS")).unwrap();
        fs::write(root.join("Contents/MacOS/app"), body).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("MacOS/app", root.join("Contents/link")).unwrap();
    }

    #[test]
    fn equal_trees_match_and_changes_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        tree(&a, "one");
        tree(&b, "one");
        ensure_same_tree(&a, &b).unwrap();
        fs::write(b.join("Contents/MacOS/app"), "two").unwrap();
        fs::write(b.join("Contents/extra"), "x").unwrap();
        let diff = TreeDigest::of(&a)
            .unwrap()
            .differences(&TreeDigest::of(&b).unwrap());
        assert_eq!(diff.len(), 2, "{diff:?}");
        assert!(ensure_same_tree(&a, &b).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_and_modes_count() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        tree(&a, "one");
        tree(&b, "one");
        fs::remove_file(b.join("Contents/link")).unwrap();
        std::os::unix::fs::symlink("elsewhere", b.join("Contents/link")).unwrap();
        assert!(ensure_same_tree(&a, &b).is_err());
        fs::remove_file(b.join("Contents/link")).unwrap();
        std::os::unix::fs::symlink("MacOS/app", b.join("Contents/link")).unwrap();
        ensure_same_tree(&a, &b).unwrap();
        fs::set_permissions(
            b.join("Contents/MacOS/app"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(ensure_same_tree(&a, &b).is_err());
    }
}
