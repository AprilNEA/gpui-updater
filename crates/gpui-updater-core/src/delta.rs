//! Binary delta updates in the `files-bsdiff-v1` format.
//!
//! A delta rebuilds the *new* install tree from the *currently installed* one,
//! so an update only downloads what changed. On macOS the tree is the `.app`
//! bundle directory; on Linux it is the single executable. Everything works on
//! uncompressed content: patching a compressed `.dmg` or `.tar.gz` directly
//! yields deltas almost as large as the full artifact.
//!
//! # Format
//!
//! ```text
//! b"GPUIDLT1" | header length (u64 LE) | JSON header | blob section
//! ```
//!
//! The header lists every entry of the new tree in parent-before-child order:
//! directories, symlinks, and regular files. Each regular file is produced by
//! one of three operations:
//!
//! - `copy`: take a file from the installed tree unchanged (possibly from a
//!   different path, to follow renames);
//! - `patch`: apply a [bsdiff] patch to a file from the installed tree. The
//!   patch's control, diff, and extra streams are stored LZMA-compressed. With
//!   `"transform": "macho-code-slots"`, the page hashes of a signed Mach-O
//!   image are zeroed before patching and recomputed afterwards;
//! - `add`: decompress an LZMA blob. Creation picks this over `patch` whenever
//!   it is smaller.
//!
//! Every file records the SHA-256 of the output, and `copy`/`patch` also record
//! the SHA-256 of the installed file they start from. [`apply`] checks both, so
//! a modified installation is rejected instead of being patched into a broken
//! one, and a corrupt delta never produces an unverified file. Files absent
//! from the header are simply not recreated, which is how deletions work.
//!
//! The header carries no signature of its own. Sign the whole delta file with
//! minisign, exactly like a full artifact; the signature then authenticates the
//! per-file hashes as well.
//!
//! # Publishing
//!
//! Run [`create`] in release CI for each old version in [`delta_bases`]: the few
//! most recent releases plus a skip list further back. The engine chains
//! deltas when none starts at the installed version, picking the chain with
//! the fewest bytes, and falls back to the full artifact when that is smaller.
//!
//! [bsdiff]: https://www.daemonology.net/bsdiff/

mod apply;
mod bspatch;
mod codec;
mod create;
mod fsutil;
mod macho;
mod plan;
#[cfg(test)]
mod tests;

use semver::Version;
use serde::{Deserialize, Serialize};

pub use apply::{apply, read_info};
pub use create::{VersionPair, create};
pub use plan::delta_bases;
pub(crate) use plan::{Route, cheapest};

use crate::release::DELTA_FORMAT_FILES_BSDIFF_V1;

/// The delta format this module reads and writes.
pub const FORMAT: &str = DELTA_FORMAT_FILES_BSDIFF_V1;

const MAGIC: &[u8; 8] = b"GPUIDLT1";
/// Bytes before the JSON header: magic plus the u64 header length.
const PREAMBLE_LEN: u64 = 16;
/// Upper bound on the JSON header, so a corrupt length cannot trigger a huge
/// allocation.
const MAX_HEADER_LEN: u64 = 64 * 1024 * 1024;

/// Summary of a delta, read from its header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeltaInfo {
    /// The version the delta applies to.
    pub from: Version,
    /// The version the delta produces.
    pub to: Version,
    /// Number of regular files in the produced tree.
    pub files: usize,
    /// Files taken unchanged from the installed tree.
    pub copied: usize,
    /// Files produced by applying a bsdiff patch.
    pub patched: usize,
    /// Files stored in full (compressed), because they had no usable base or
    /// their patch would have been larger.
    pub added: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    format: String,
    from: String,
    to: String,
    root: RootKind,
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RootKind {
    Dir,
    File,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// `/`-separated path relative to the root. Empty only for a file root.
    path: String,
    #[serde(flatten)]
    kind: EntryKind,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum EntryKind {
    Dir {
        mode: u32,
    },
    Symlink {
        target: String,
    },
    File {
        mode: u32,
        size: u64,
        sha256: String,
        source: Source,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Source {
    Copy {
        base: String,
        base_sha256: String,
    },
    Patch {
        base: String,
        base_sha256: String,
        offset: u64,
        len: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transform: Option<Transform>,
    },
    Add {
        offset: u64,
        len: u64,
    },
}

/// A reversible rewrite applied to both sides of a patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Transform {
    /// Mach-O page hashes zeroed before diffing, recomputed after patching.
    MachoCodeSlots,
}
