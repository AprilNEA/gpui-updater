//! Delta creation: the release-CI side.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use semver::Version;

use super::bspatch::RawPatch;
use super::fsutil::{join_rel, mode_of, sha256_hex, walk};
use super::{
    DeltaInfo, Entry, EntryKind, FORMAT, Header, MAGIC, RootKind, Source, Transform, codec, macho,
};
use crate::error::{Error, Result};

/// bsdiff holds both files and a suffix array of the old one in memory (about
/// ten times the file size), so cap how many changed files are diffed at once.
const MAX_WORKERS: usize = 4;
/// A changed file is always compressed in full as well when it is this small,
/// so the cheaper encoding wins.
const ALWAYS_COMPARE_LEN: usize = 1 << 20;
/// Larger files are compressed in full only when their patch is at least this
/// fraction of their size. Binaries compress to a third at best, so a smaller
/// patch always wins, and compressing them is the slowest step of creation.
const FULL_COPY_RATIO: usize = 10;

/// The version pair a delta upgrades between.
///
/// Marked `#[non_exhaustive]`: build one with [`VersionPair::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VersionPair {
    /// The installed version the delta applies to.
    pub from: Version,
    /// The version the delta produces.
    pub to: Version,
}

impl VersionPair {
    /// The pair `from` → `to`.
    pub fn new(from: Version, to: Version) -> Self {
        Self { from, to }
    }
}

/// Create a delta that rebuilds `new` from `old` and write it to `out`.
///
/// `old` and `new` must both be directories (e.g. two `.app` bundles) or both
/// be regular files (e.g. two Linux executables). `versions` is recorded in
/// the header; [`apply`](super::apply) callers check it against the installed
/// and offered versions.
///
/// Each regular file is stored the cheapest way: copied from the old tree
/// (also across renames), patched with bsdiff, or compressed in full when that
/// is smaller than its patch. Signed Mach-O images have their page hashes
/// stripped before diffing and recomputed on apply. Changed files are
/// processed in parallel, and the output is identical for identical inputs.
///
/// Generation reads whole files into memory and is CPU-heavy (LZMA at the
/// `xz -9e` level), so it belongs in release CI, not in the shipped app.
///
/// # Errors
/// Returns [`Error::Delta`] for unsupported inputs (mismatched root kinds,
/// non-UTF-8 paths, special files) and [`Error::Io`] on I/O failure.
pub fn create(old: &Path, new: &Path, versions: &VersionPair, out: &Path) -> Result<DeltaInfo> {
    let root = root_kind(old, new)?;
    let old_by_hash = match root {
        RootKind::Dir => index_by_hash(old)?,
        RootKind::File => HashMap::new(),
    };
    let rels = match root {
        RootKind::Dir => walk(new)?,
        RootKind::File => vec![String::new()],
    };
    let (slots, jobs) = describe(new, rels)?;
    let trees = Trees {
        old,
        new,
        old_by_hash: &old_by_hash,
    };
    let mut prepared: Vec<Option<Prepared>> =
        run_parallel(jobs.len(), |i| trees.prepare(&jobs[i]))?
            .into_iter()
            .map(Some)
            .collect();

    let mut info = DeltaInfo {
        from: versions.from.clone(),
        to: versions.to.clone(),
        files: 0,
        copied: 0,
        patched: 0,
        added: 0,
    };
    let mut blobs = Blobs::default();
    let mut entries = Vec::with_capacity(slots.len());
    for (path, slot) in slots {
        let kind = match slot {
            Slot::Ready(kind) => kind,
            Slot::File(i) => {
                let file = prepared[i]
                    .take()
                    .ok_or_else(|| Error::Delta("internal: file prepared twice".into()))?;
                file_entry(file, &mut blobs, &mut info)
            }
        };
        entries.push(Entry { path, kind });
    }

    let header = Header {
        format: FORMAT.to_string(),
        from: versions.from.to_string(),
        to: versions.to.to_string(),
        root,
        entries,
    };
    let written = write_delta(out, &header, &blobs.data);
    if written.is_err() {
        let _ = fs::remove_file(out);
    }
    written.map(|()| info)
}

/// Each path of the new tree with its slot, and the regular files to prepare.
type Described = (Vec<(String, Slot)>, Vec<String>);

/// Describe each path of the new tree: directories and symlinks directly,
/// regular files as jobs to prepare (returned alongside, by index).
fn describe(new: &Path, rels: Vec<String>) -> Result<Described> {
    let mut slots = Vec::with_capacity(rels.len());
    let mut jobs = Vec::new();
    for rel in rels {
        let path = join_rel(new, &rel);
        let meta = fs::symlink_metadata(&path)?;
        let slot = if meta.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            let target = target
                .to_str()
                .ok_or_else(|| Error::Delta(format!("symlink target of `{rel}` is not UTF-8")))?;
            Slot::Ready(EntryKind::Symlink {
                target: target.to_string(),
            })
        } else if meta.is_dir() {
            Slot::Ready(EntryKind::Dir {
                mode: mode_of(&meta),
            })
        } else if meta.is_file() {
            jobs.push(rel.clone());
            Slot::File(jobs.len() - 1)
        } else {
            return Err(Error::Delta(format!(
                "`{rel}` is not a file, directory, or symlink"
            )));
        };
        slots.push((rel, slot));
    }
    Ok((slots, jobs))
}

/// The entry for a prepared file: its blob is appended to `blobs`, and it is
/// counted in `info`.
fn file_entry(file: Prepared, blobs: &mut Blobs, info: &mut DeltaInfo) -> EntryKind {
    info.files += 1;
    let source = match file.choice {
        Choice::Copy { base, base_sha256 } => {
            info.copied += 1;
            Source::Copy { base, base_sha256 }
        }
        Choice::Patch {
            base,
            base_sha256,
            blob,
            transform,
        } => {
            info.patched += 1;
            let (offset, len) = blobs.push(blob);
            Source::Patch {
                base,
                base_sha256,
                offset,
                len,
                transform,
            }
        }
        Choice::Add { blob } => {
            info.added += 1;
            let (offset, len) = blobs.push(blob);
            Source::Add { offset, len }
        }
    };
    EntryKind::File {
        mode: file.mode,
        size: file.size,
        sha256: file.sha256,
        source,
    }
}

fn root_kind(old: &Path, new: &Path) -> Result<RootKind> {
    let old_meta = fs::metadata(old)?;
    let new_meta = fs::metadata(new)?;
    match (old_meta.is_dir(), new_meta.is_dir()) {
        (true, true) => Ok(RootKind::Dir),
        (false, false) if old_meta.is_file() && new_meta.is_file() => Ok(RootKind::File),
        _ => Err(Error::Delta(
            "old and new must both be directories or both be regular files".into(),
        )),
    }
}

/// Old files by content, so renamed or duplicated files are copied instead of
/// re-sent.
fn index_by_hash(old: &Path) -> Result<HashMap<String, String>> {
    let mut index = HashMap::new();
    for rel in walk(old)? {
        let path = join_rel(old, &rel);
        if fs::symlink_metadata(&path)?.is_file() {
            index.entry(sha256_hex(&fs::read(&path)?)).or_insert(rel);
        }
    }
    Ok(index)
}

fn write_delta(out: &Path, header: &Header, blobs: &[Vec<u8>]) -> Result<()> {
    let json = serde_json::to_vec(header)
        .map_err(|e| Error::Delta(format!("could not encode header: {e}")))?;
    let mut file = BufWriter::new(File::create(out)?);
    file.write_all(MAGIC)?;
    file.write_all(&(json.len() as u64).to_le_bytes())?;
    file.write_all(&json)?;
    for blob in blobs {
        file.write_all(blob)?;
    }
    file.into_inner()
        .map_err(std::io::IntoInnerError::into_error)?
        .sync_all()?;
    Ok(())
}

/// One entry of the new tree, in header order.
enum Slot {
    Ready(EntryKind),
    /// A regular file: index into the prepared jobs.
    File(usize),
}

/// The blob section, accumulated in header order.
#[derive(Default)]
struct Blobs {
    data: Vec<Vec<u8>>,
    len: u64,
}

impl Blobs {
    fn push(&mut self, blob: Vec<u8>) -> (u64, u64) {
        let (offset, len) = (self.len, blob.len() as u64);
        self.len += len;
        self.data.push(blob);
        (offset, len)
    }
}

/// A regular file of the new tree, ready to be described in the header.
struct Prepared {
    mode: u32,
    size: u64,
    sha256: String,
    choice: Choice,
}

enum Choice {
    Copy {
        base: String,
        base_sha256: String,
    },
    Patch {
        base: String,
        base_sha256: String,
        blob: Vec<u8>,
        transform: Option<Transform>,
    },
    Add {
        blob: Vec<u8>,
    },
}

struct Trees<'a> {
    old: &'a Path,
    new: &'a Path,
    old_by_hash: &'a HashMap<String, String>,
}

impl Trees<'_> {
    fn prepare(&self, rel: &str) -> Result<Prepared> {
        let path = join_rel(self.new, rel);
        let meta = fs::symlink_metadata(&path)?;
        let data = fs::read(&path)?;
        let sha256 = sha256_hex(&data);
        let choice = self.choose(rel, &data, &sha256)?;
        Ok(Prepared {
            mode: mode_of(&meta),
            size: data.len() as u64,
            sha256,
            choice,
        })
    }

    /// The cheapest way to produce `data`: copy, patch, or store it in full.
    fn choose(&self, rel: &str, data: &[u8], sha256: &str) -> Result<Choice> {
        let old_path = join_rel(self.old, rel);
        let old_data = match fs::symlink_metadata(&old_path) {
            Ok(meta) if meta.is_file() => Some(fs::read(&old_path)?),
            _ => None,
        };
        if old_data.as_deref() == Some(data) {
            return Ok(Choice::Copy {
                base: rel.to_string(),
                base_sha256: sha256.to_string(),
            });
        }
        if let Some(base) = self.old_by_hash.get(sha256) {
            return Ok(Choice::Copy {
                base: base.clone(),
                base_sha256: sha256.to_string(),
            });
        }
        let Some(old) = old_data.filter(|old| !old.is_empty() && !data.is_empty()) else {
            return Ok(Choice::Add {
                blob: codec::compress(data)?,
            });
        };
        let (blob, transform) = patch_blob(&old, data)?;
        if data.len() <= ALWAYS_COMPARE_LEN
            || blob.len().saturating_mul(FULL_COPY_RATIO) > data.len()
        {
            let full = codec::compress(data)?;
            if full.len() < blob.len() {
                return Ok(Choice::Add { blob: full });
            }
        }
        Ok(Choice::Patch {
            base: rel.to_string(),
            base_sha256: sha256_hex(&old),
            blob,
            transform,
        })
    }
}

/// A patch blob for `old` → `new`, diffing normalized Mach-O images when `new`
/// is one whose page hashes can be recomputed exactly.
fn patch_blob(old: &[u8], new: &[u8]) -> Result<(Vec<u8>, Option<Transform>)> {
    if let Some(normalized) = macho::normalize(new) {
        let mut check = normalized.clone();
        macho::restore(&mut check);
        if check == new {
            let old_normalized = macho::normalize(old);
            let base = old_normalized.as_deref().unwrap_or(old);
            let blob = RawPatch::diff(base, &normalized)?.encode()?;
            return Ok((blob, Some(Transform::MachoCodeSlots)));
        }
    }
    Ok((RawPatch::diff(old, new)?.encode()?, None))
}

/// Run `work(0..count)` on a few threads and return the results in order.
fn run_parallel<T: Send>(count: usize, work: impl Fn(usize) -> Result<T> + Sync) -> Result<Vec<T>> {
    let workers = std::thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .clamp(1, MAX_WORKERS)
        .min(count.max(1));
    let next = AtomicUsize::new(0);
    let done: Vec<Vec<(usize, Result<T>)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= count {
                            break done;
                        }
                        done.push((i, work(i)));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    let mut results: Vec<Option<T>> = (0..count).map(|_| None).collect();
    for (i, result) in done.into_iter().flatten() {
        results[i] = Some(result?);
    }
    results
        .into_iter()
        .map(|r| r.ok_or_else(|| Error::Delta("internal: a file was not processed".into())))
        .collect()
}
