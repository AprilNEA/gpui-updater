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
//! - `patch`: apply a [bsdiff] patch (BSDIFF40, via [`qbsdiff`]) to a file from
//!   the installed tree;
//! - `add`: decompress a bzip2 blob.
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
//! [bsdiff]: https://www.daemonology.net/bsdiff/

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::release::DELTA_FORMAT_FILES_BSDIFF_V1;

/// The delta format this module reads and writes.
pub const FORMAT: &str = DELTA_FORMAT_FILES_BSDIFF_V1;

const MAGIC: &[u8; 8] = b"GPUIDLT1";
/// Bytes before the JSON header: magic plus the u64 header length.
const PREAMBLE_LEN: u64 = 16;
/// Upper bound on the JSON header, so a corrupt length cannot trigger a huge
/// allocation.
const MAX_HEADER_LEN: u64 = 64 * 1024 * 1024;
const BZIP2_LEVEL: u32 = 9;

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
    /// Files stored in full (compressed) because they had no usable base.
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
    },
    Add {
        offset: u64,
        len: u64,
    },
}

/// Create a delta that rebuilds `new` from `old` and write it to `out`.
///
/// `old` and `new` must both be directories (e.g. two `.app` bundles) or both
/// be regular files (e.g. two Linux executables). `from` and `to` are recorded
/// in the header; [`apply`] callers should check them against the installed and
/// offered versions.
///
/// Generation reads whole files into memory and runs bsdiff on every changed
/// file, so it belongs in release CI, not in the shipped app.
///
/// # Errors
/// Returns [`Error::Delta`] for unsupported inputs (mismatched root kinds,
/// non-UTF-8 paths, special files) and [`Error::Io`] on I/O failure.
pub fn create(
    old: &Path,
    new: &Path,
    from: &Version,
    to: &Version,
    out: &Path,
) -> Result<DeltaInfo> {
    let old_meta = fs::metadata(old)?;
    let new_meta = fs::metadata(new)?;
    let root = match (old_meta.is_dir(), new_meta.is_dir()) {
        (true, true) => RootKind::Dir,
        (false, false) if old_meta.is_file() && new_meta.is_file() => RootKind::File,
        _ => {
            return Err(Error::Delta(
                "old and new must both be directories or both be regular files".into(),
            ));
        }
    };

    let blobs_path = sibling_temp(out, "blobs");
    let mut builder = Builder {
        old,
        old_by_hash: HashMap::new(),
        blobs: BufWriter::new(File::create(&blobs_path)?),
        blob_len: 0,
        info: DeltaInfo {
            from: from.clone(),
            to: to.clone(),
            files: 0,
            copied: 0,
            patched: 0,
            added: 0,
        },
    };
    let entries = builder.entries(root, new);
    let Builder { blobs, info, .. } = builder;
    let entries = entries.and_then(|entries| {
        blobs.into_inner().map_err(io::IntoInnerError::into_error)?;
        Ok(entries)
    });
    let entries = match entries {
        Ok(entries) => entries,
        Err(e) => {
            let _ = fs::remove_file(&blobs_path);
            return Err(e);
        }
    };

    let header = Header {
        format: FORMAT.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        root,
        entries,
    };
    let write = (|| -> Result<()> {
        let json = serde_json::to_vec(&header)
            .map_err(|e| Error::Delta(format!("could not encode header: {e}")))?;
        let mut file = BufWriter::new(File::create(out)?);
        file.write_all(MAGIC)?;
        file.write_all(&(json.len() as u64).to_le_bytes())?;
        file.write_all(&json)?;
        io::copy(&mut File::open(&blobs_path)?, &mut file)?;
        file.flush()?;
        Ok(())
    })();
    let _ = fs::remove_file(&blobs_path);
    write?;
    Ok(info)
}

/// Accumulates header entries and blobs while [`create`] walks the new tree.
struct Builder<'a> {
    old: &'a Path,
    /// Old files by content, so renamed or duplicated files are copied
    /// instead of re-sent.
    old_by_hash: HashMap<String, String>,
    blobs: BufWriter<File>,
    blob_len: u64,
    info: DeltaInfo,
}

impl Builder<'_> {
    fn entries(&mut self, root: RootKind, new: &Path) -> Result<Vec<Entry>> {
        if root == RootKind::Dir {
            for rel in walk(self.old)? {
                let path = join_rel(self.old, &rel);
                if fs::symlink_metadata(&path)?.is_file() {
                    self.old_by_hash
                        .entry(sha256_hex(&fs::read(&path)?))
                        .or_insert(rel);
                }
            }
        }
        let rels = match root {
            RootKind::Dir => walk(new)?,
            RootKind::File => vec![String::new()],
        };
        let mut entries = Vec::with_capacity(rels.len());
        for rel in rels {
            let kind = self.entry(new, &rel)?;
            entries.push(Entry { path: rel, kind });
        }
        Ok(entries)
    }

    fn entry(&mut self, new: &Path, rel: &str) -> Result<EntryKind> {
        let new_path = join_rel(new, rel);
        let meta = fs::symlink_metadata(&new_path)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&new_path)?;
            let target = target
                .to_str()
                .ok_or_else(|| Error::Delta(format!("symlink target of `{rel}` is not UTF-8")))?;
            Ok(EntryKind::Symlink {
                target: target.to_string(),
            })
        } else if meta.is_dir() {
            Ok(EntryKind::Dir {
                mode: mode_of(&meta),
            })
        } else if meta.is_file() {
            let data = fs::read(&new_path)?;
            let sha256 = sha256_hex(&data);
            let source = self.file_source(rel, &data, &sha256)?;
            self.info.files += 1;
            Ok(EntryKind::File {
                mode: mode_of(&meta),
                size: data.len() as u64,
                sha256,
                source,
            })
        } else {
            Err(Error::Delta(format!(
                "`{rel}` is not a file, directory, or symlink"
            )))
        }
    }

    /// Pick the cheapest way to produce `data`: copy, bsdiff, or store it.
    fn file_source(&mut self, rel: &str, data: &[u8], sha256: &str) -> Result<Source> {
        let old_path = join_rel(self.old, rel);
        let old_data = match fs::symlink_metadata(&old_path) {
            Ok(m) if m.is_file() => Some(fs::read(&old_path)?),
            _ => None,
        };
        if old_data.as_deref() == Some(data) {
            self.info.copied += 1;
            return Ok(Source::Copy {
                base: rel.to_string(),
                base_sha256: sha256.to_string(),
            });
        }
        if let Some(base) = self.old_by_hash.get(sha256) {
            self.info.copied += 1;
            return Ok(Source::Copy {
                base: base.clone(),
                base_sha256: sha256.to_string(),
            });
        }
        if let Some(old_data) = old_data.filter(|o| !o.is_empty() && !data.is_empty()) {
            let mut patch = Vec::new();
            qbsdiff::Bsdiff::new(&old_data, data)
                .compression_level(BZIP2_LEVEL)
                .compare(io::Cursor::new(&mut patch))?;
            self.info.patched += 1;
            let (offset, len) = self.push_blob(&patch)?;
            return Ok(Source::Patch {
                base: rel.to_string(),
                base_sha256: sha256_hex(&old_data),
                offset,
                len,
            });
        }
        let mut compressed = Vec::new();
        let mut encoder =
            bzip2::write::BzEncoder::new(&mut compressed, bzip2::Compression::new(BZIP2_LEVEL));
        encoder.write_all(data)?;
        encoder.finish()?;
        self.info.added += 1;
        let (offset, len) = self.push_blob(&compressed)?;
        Ok(Source::Add { offset, len })
    }

    fn push_blob(&mut self, data: &[u8]) -> Result<(u64, u64)> {
        self.blobs.write_all(data)?;
        let offset = self.blob_len;
        self.blob_len += data.len() as u64;
        Ok((offset, data.len() as u64))
    }
}

/// Read the header of a delta without applying it.
///
/// # Errors
/// Returns [`Error::Delta`] if the file is not a well-formed delta in a
/// supported format.
pub fn read_info(delta: &Path) -> Result<DeltaInfo> {
    let mut file = File::open(delta)?;
    let (header, _) = read_header(&mut file)?;
    info_of(&header)
}

/// Rebuild the new tree described by `delta` from the installed `base`, writing
/// it to `out`, which must not exist yet.
///
/// `base` is the installed root (the `.app` directory, or the executable for a
/// file-rooted delta). It is only read. Every base file is checked against the
/// hash recorded in the delta before use, and every output file is checked
/// after it is written. On any error the partial output is removed.
///
/// # Errors
/// Returns [`Error::Delta`] when the delta is malformed, the installed tree does
/// not match what the delta expects, or an output hash does not match.
pub fn apply(delta: &Path, base: &Path, out: &Path) -> Result<DeltaInfo> {
    if fs::symlink_metadata(out).is_ok() {
        return Err(Error::Delta(format!(
            "output path `{}` already exists",
            out.display()
        )));
    }
    let mut file = File::open(delta)?;
    let (header, blob_start) = read_header(&mut file)?;
    let info = info_of(&header)?;
    let blob_section = file.metadata()?.len().saturating_sub(blob_start);
    validate(&header, blob_section)?;

    let result = write_tree(&header, &mut file, blob_start, base, out);
    if result.is_err() {
        if header.root == RootKind::Dir {
            make_writable(out);
            let _ = fs::remove_dir_all(out);
        } else {
            let _ = fs::remove_file(out);
        }
    }
    result.map(|()| info)
}

fn info_of(header: &Header) -> Result<DeltaInfo> {
    let parse = |v: &str| {
        Version::parse(v).map_err(|e| Error::Delta(format!("invalid version `{v}` in header: {e}")))
    };
    let mut info = DeltaInfo {
        from: parse(&header.from)?,
        to: parse(&header.to)?,
        files: 0,
        copied: 0,
        patched: 0,
        added: 0,
    };
    for entry in &header.entries {
        if let EntryKind::File { source, .. } = &entry.kind {
            info.files += 1;
            match source {
                Source::Copy { .. } => info.copied += 1,
                Source::Patch { .. } => info.patched += 1,
                Source::Add { .. } => info.added += 1,
            }
        }
    }
    Ok(info)
}

fn read_header(file: &mut File) -> Result<(Header, u64)> {
    let len = file.metadata()?.len();
    let mut preamble = [0u8; 16];
    file.read_exact(&mut preamble)
        .map_err(|_| Error::Delta("file is too short to be a delta".into()))?;
    if &preamble[..8] != MAGIC {
        return Err(Error::Delta("not a gpui-updater delta (bad magic)".into()));
    }
    let mut len_bytes = [0u8; 8];
    len_bytes.copy_from_slice(&preamble[8..]);
    let header_len = u64::from_le_bytes(len_bytes);
    if header_len > MAX_HEADER_LEN || header_len > len.saturating_sub(PREAMBLE_LEN) {
        return Err(Error::Delta("header length is out of bounds".into()));
    }
    let mut json = vec![
        0u8;
        usize::try_from(header_len)
            .map_err(|_| Error::Delta("header too large".into()))?
    ];
    file.read_exact(&mut json)?;
    let header: Header =
        serde_json::from_slice(&json).map_err(|e| Error::Delta(format!("invalid header: {e}")))?;
    if header.format != FORMAT {
        return Err(Error::Delta(format!(
            "unsupported delta format `{}`",
            header.format
        )));
    }
    Ok((header, PREAMBLE_LEN + header_len))
}

/// Reject headers that could write outside `out`, reference missing blobs, or
/// describe an inconsistent tree. Runs before anything is written.
fn validate(header: &Header, blob_section: u64) -> Result<()> {
    let check_blob = |offset: u64, len: u64| {
        if offset.checked_add(len).is_none_or(|end| end > blob_section) {
            Err(Error::Delta("blob range is out of bounds".into()))
        } else {
            Ok(())
        }
    };
    let check_source = |source: &Source| match source {
        Source::Copy { base, .. } => check_rel(header.root, base),
        Source::Patch {
            base, offset, len, ..
        } => {
            check_rel(header.root, base)?;
            check_blob(*offset, *len)
        }
        Source::Add { offset, len } => check_blob(*offset, *len),
    };

    if header.root == RootKind::File {
        return match header.entries.as_slice() {
            [
                Entry {
                    path,
                    kind: EntryKind::File { source, .. },
                },
            ] if path.is_empty() => check_source(source),
            _ => Err(Error::Delta(
                "a file-rooted delta must contain exactly one file entry".into(),
            )),
        };
    }

    // Directory root: each entry's parent must be the root or a directory
    // listed earlier. This also rules out writing through a symlink.
    let mut dirs: HashSet<&str> = HashSet::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for entry in &header.entries {
        check_rel(header.root, &entry.path)?;
        if !seen.insert(&entry.path) {
            return Err(Error::Delta(format!("duplicate entry `{}`", entry.path)));
        }
        if let Some((parent, _)) = entry.path.rsplit_once('/') {
            if !dirs.contains(parent) {
                return Err(Error::Delta(format!(
                    "entry `{}` appears before its parent directory",
                    entry.path
                )));
            }
        }
        match &entry.kind {
            EntryKind::Dir { .. } => {
                dirs.insert(&entry.path);
            }
            EntryKind::Symlink { target } => {
                if target.is_empty() || Path::new(target).is_absolute() {
                    return Err(Error::Delta(format!(
                        "symlink `{}` must have a relative target",
                        entry.path
                    )));
                }
            }
            EntryKind::File { source, .. } => check_source(source)?,
        }
    }
    Ok(())
}

/// A relative path made only of normal components (no `..`, `.`, or root).
fn check_rel(root: RootKind, rel: &str) -> Result<()> {
    if root == RootKind::File {
        return if rel.is_empty() {
            Ok(())
        } else {
            Err(Error::Delta(format!(
                "file-rooted delta references path `{rel}`"
            )))
        };
    }
    let ok = !rel.is_empty()
        && !rel.contains('\\')
        && rel.split('/').all(|part| {
            let mut c = Path::new(part).components();
            matches!((c.next(), c.next()), (Some(Component::Normal(_)), None))
        });
    if ok {
        Ok(())
    } else {
        Err(Error::Delta(format!("unsafe path `{rel}` in delta")))
    }
}

fn write_tree(
    header: &Header,
    delta: &mut File,
    blob_start: u64,
    base: &Path,
    out: &Path,
) -> Result<()> {
    if header.root == RootKind::Dir {
        fs::create_dir(out)?;
    }
    let mut dir_modes = Vec::new();
    for entry in &header.entries {
        let dest = join_rel(out, &entry.path);
        match &entry.kind {
            EntryKind::Dir { mode } => {
                fs::create_dir(&dest)?;
                dir_modes.push((dest, *mode));
            }
            EntryKind::Symlink { target } => make_symlink(target, &dest)?,
            EntryKind::File {
                mode,
                size,
                sha256,
                source,
            } => {
                let data = match source {
                    Source::Copy {
                        base: rel,
                        base_sha256,
                    } => read_base(base, rel, base_sha256)?,
                    Source::Patch {
                        base: rel,
                        base_sha256,
                        offset,
                        len,
                    } => {
                        let old = read_base(base, rel, base_sha256)?;
                        let patch = read_blob(delta, blob_start, *offset, *len)?;
                        let patcher = qbsdiff::Bspatch::new(&patch).map_err(|e| {
                            Error::Delta(format!("corrupt patch for `{}`: {e}", entry.path))
                        })?;
                        let mut data = Vec::with_capacity(usize::try_from(*size).unwrap_or(0));
                        patcher
                            .apply(&old, io::Cursor::new(&mut data))
                            .map_err(|e| {
                                Error::Delta(format!("could not patch `{}`: {e}", entry.path))
                            })?;
                        data
                    }
                    Source::Add { offset, len } => {
                        let blob = read_blob(delta, blob_start, *offset, *len)?;
                        let mut data = Vec::with_capacity(usize::try_from(*size).unwrap_or(0));
                        bzip2::read::BzDecoder::new(blob.as_slice())
                            .take(size.saturating_add(1))
                            .read_to_end(&mut data)
                            .map_err(|e| {
                                Error::Delta(format!("corrupt data for `{}`: {e}", entry.path))
                            })?;
                        data
                    }
                };
                if data.len() as u64 != *size || !sha256_hex(&data).eq_ignore_ascii_case(sha256) {
                    return Err(Error::Delta(format!(
                        "rebuilt `{}` does not match the expected hash",
                        display_rel(&entry.path)
                    )));
                }
                let mut file = File::create_new(&dest)?;
                file.write_all(&data)?;
                file.sync_all()?;
                set_mode(&dest, *mode)?;
            }
        }
    }
    // Apply directory modes last, deepest first, so a read-only directory does
    // not block writing its children.
    for (dir, mode) in dir_modes.iter().rev() {
        set_mode(dir, *mode)?;
    }
    Ok(())
}

fn read_base(base: &Path, rel: &str, expected: &str) -> Result<Vec<u8>> {
    let path = join_rel(base, rel);
    let data = fs::read(&path).map_err(|e| {
        Error::Delta(format!(
            "installed file `{}` is unavailable: {e}",
            display_rel(rel)
        ))
    })?;
    if sha256_hex(&data).eq_ignore_ascii_case(expected) {
        Ok(data)
    } else {
        Err(Error::Delta(format!(
            "installed file `{}` does not match the version this delta expects",
            display_rel(rel)
        )))
    }
}

fn read_blob(delta: &mut File, blob_start: u64, offset: u64, len: u64) -> Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| Error::Delta("blob too large".into()))?;
    let mut buf = vec![0u8; len];
    delta.seek(SeekFrom::Start(blob_start + offset))?;
    BufReader::new(&mut *delta).read_exact(&mut buf)?;
    Ok(buf)
}

/// All entries under `root` as sorted `/`-separated relative paths, parents
/// before children. Symlinks are listed but not followed.
fn walk(root: &Path) -> Result<Vec<String>> {
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

fn join_rel(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        rel.split('/')
            .fold(root.to_path_buf(), |p, part| p.join(part))
    }
}

fn display_rel(rel: &str) -> &str {
    if rel.is_empty() { "<root>" } else { rel }
}

fn sibling_temp(path: &Path, tag: &str) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "delta".into(), |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.{tag}.{}.tmp", std::process::id()))
}

fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(data) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(meta: &fs::Metadata) -> u32 {
    if meta.is_dir() {
        0o755
    } else if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Never recreate setuid/setgid bits from a downloaded header.
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o1777))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(mode & 0o200 == 0);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(unix)]
fn make_symlink(target: &str, dest: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, dest)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_symlink(_target: &str, dest: &Path) -> Result<()> {
    Err(Error::Delta(format!(
        "symlinks are not supported on this platform (`{}`)",
        dest.display()
    )))
}

/// Best effort: restore owner write permission on a partial output so it can
/// be removed.
fn make_writable(dir: &Path) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn write(path: &Path, data: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    /// Pseudo-random but deterministic bytes, so patches are non-trivial.
    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x.to_le_bytes()[3]
            })
            .collect()
    }

    fn snapshot(root: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for rel in walk(root).unwrap() {
            let path = join_rel(root, &rel);
            let meta = fs::symlink_metadata(&path).unwrap();
            let desc = if meta.file_type().is_symlink() {
                format!("link {}", fs::read_link(&path).unwrap().display())
            } else if meta.is_dir() {
                format!("dir {:o}", mode_of(&meta))
            } else {
                format!(
                    "file {:o} {}",
                    mode_of(&meta),
                    sha256_hex(&fs::read(&path).unwrap())
                )
            };
            out.push((rel, desc));
        }
        out
    }

    fn fixture(dir: &Path) -> (PathBuf, PathBuf) {
        let old = dir.join("old/Demo.app");
        let new = dir.join("new/Demo.app");
        let exe = noise(1, 200_000);
        let mut exe2 = exe.clone();
        // Scattered edits, like a recompiled binary.
        for i in (0..exe2.len()).step_by(997) {
            exe2[i] = exe2[i].wrapping_add(3);
        }
        exe2.extend_from_slice(&noise(2, 5_000));

        write(&old.join("Contents/MacOS/demo"), &exe);
        write(&new.join("Contents/MacOS/demo"), &exe2);
        write(&old.join("Contents/Info.plist"), b"<plist>1.0.0</plist>");
        write(&new.join("Contents/Info.plist"), b"<plist>1.1.0</plist>");
        write(&old.join("Contents/Resources/same.txt"), b"unchanged");
        write(&new.join("Contents/Resources/same.txt"), b"unchanged");
        write(
            &old.join("Contents/Resources/old-name.bin"),
            &noise(3, 4096),
        );
        write(&new.join("Contents/Resources/renamed.bin"), &noise(3, 4096));
        write(&old.join("Contents/Resources/removed.txt"), b"gone");
        write(&new.join("Contents/Resources/added.txt"), b"brand new file");
        write(&old.join("Contents/Resources/empty-before"), b"");
        write(
            &new.join("Contents/Resources/empty-before"),
            b"now has content",
        );
        write(&new.join("Contents/Resources/empty"), b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for root in [&old, &new] {
                fs::set_permissions(
                    root.join("Contents/MacOS/demo"),
                    fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
            std::os::unix::fs::symlink("Resources/added.txt", new.join("Contents/link")).unwrap();
        }
        (old, new)
    }

    #[test]
    fn round_trips_a_directory_tree() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = fixture(dir.path());
        let delta = dir.path().join("demo.delta");
        let made = create(&old, &new, &v("1.0.0"), &v("1.1.0"), &delta).unwrap();
        assert_eq!(made.files, 7);
        // The new empty file is copied from the old empty file by content.
        assert_eq!(made.copied, 3, "unchanged + renamed + empty");
        assert_eq!(made.patched, 2, "binary + plist");
        assert_eq!(made.added, 2, "new file + previously empty file");

        let out = dir.path().join("out/Demo.app");
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        let applied = apply(&delta, &old, &out).unwrap();
        assert_eq!(applied, made);
        assert_eq!(read_info(&delta).unwrap(), made);
        assert_eq!(snapshot(&out), snapshot(&new));
        // The base is only read.
        assert!(old.join("Contents/Resources/removed.txt").exists());
        // The patch is much smaller than the binary it rebuilds.
        assert!(fs::metadata(&delta).unwrap().len() < 100_000);
    }

    #[test]
    fn round_trips_a_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old-bin");
        let new = dir.path().join("new-bin");
        let data = noise(7, 100_000);
        fs::write(&old, &data).unwrap();
        let mut changed = data.clone();
        changed[500] ^= 0xff;
        fs::write(&new, &changed).unwrap();
        let delta = dir.path().join("bin.delta");
        let info = create(&old, &new, &v("1.0.0"), &v("2.0.0"), &delta).unwrap();
        assert_eq!((info.files, info.patched), (1, 1));

        let out = dir.path().join("rebuilt");
        apply(&delta, &old, &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), changed);
    }

    #[test]
    fn rejects_a_modified_installation_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = fixture(dir.path());
        let delta = dir.path().join("demo.delta");
        create(&old, &new, &v("1.0.0"), &v("1.1.0"), &delta).unwrap();

        write(&old.join("Contents/Info.plist"), b"<plist>tampered</plist>");
        let out = dir.path().join("Demo.app");
        let err = apply(&delta, &old, &out).unwrap_err();
        assert!(
            matches!(err, Error::Delta(ref m) if m.contains("Info.plist")),
            "{err}"
        );
        assert!(!out.exists(), "partial output must be removed");
    }

    #[test]
    fn rejects_a_corrupt_patch() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = fixture(dir.path());
        let delta = dir.path().join("demo.delta");
        create(&old, &new, &v("1.0.0"), &v("1.1.0"), &delta).unwrap();

        let mut bytes = fs::read(&delta).unwrap();
        let last = bytes.len() - 10;
        bytes[last] ^= 0x55;
        fs::write(&delta, &bytes).unwrap();
        let out = dir.path().join("Demo.app");
        assert!(matches!(apply(&delta, &old, &out), Err(Error::Delta(_))));
        assert!(!out.exists());
    }

    fn handcrafted(dir: &Path, header: &serde_json::Value) -> PathBuf {
        let json = serde_json::to_vec(header).unwrap();
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(json.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&json);
        let path = dir.join("crafted.delta");
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn rejects_paths_that_escape_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        fs::create_dir(&base).unwrap();
        for path in ["../escape", "/etc/passwd", "a/../../b", "./x", ""] {
            let delta = handcrafted(
                dir.path(),
                &serde_json::json!({
                    "format": FORMAT, "from": "1.0.0", "to": "1.1.0", "root": "dir",
                    "entries": [{"path": path, "kind": "dir", "mode": 0o755}]
                }),
            );
            let out = dir.path().join("out");
            assert!(
                matches!(apply(&delta, &base, &out), Err(Error::Delta(_))),
                "{path}"
            );
            assert!(!out.exists());
        }
    }

    #[test]
    fn rejects_writing_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        fs::create_dir(&base).unwrap();
        let delta = handcrafted(
            dir.path(),
            &serde_json::json!({
                "format": FORMAT, "from": "1.0.0", "to": "1.1.0", "root": "dir",
                "entries": [
                    {"path": "link", "kind": "symlink", "target": "../../outside"},
                    {"path": "link/file", "kind": "dir", "mode": 0o755}
                ]
            }),
        );
        assert!(matches!(
            apply(&delta, &base, &dir.path().join("out")),
            Err(Error::Delta(m)) if m.contains("before its parent")
        ));
    }

    #[test]
    fn rejects_unknown_format_and_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let delta = handcrafted(
            dir.path(),
            &serde_json::json!({
                "format": "files-zstd-v9", "from": "1.0.0", "to": "1.1.0", "root": "dir",
                "entries": []
            }),
        );
        assert!(matches!(read_info(&delta), Err(Error::Delta(m)) if m.contains("unsupported")));
        fs::write(&delta, b"definitely not a delta file").unwrap();
        assert!(matches!(read_info(&delta), Err(Error::Delta(m)) if m.contains("magic")));
    }
}
