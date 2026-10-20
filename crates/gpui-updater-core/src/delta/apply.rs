//! Delta application: the shipped-app side.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path};

use semver::Version;

use super::bspatch::RawPatch;
use super::fsutil::{display_rel, join_rel, make_symlink, make_writable, set_mode, sha256_hex};
use super::{
    DeltaInfo, Entry, EntryKind, FORMAT, Header, MAGIC, MAX_HEADER_LEN, PREAMBLE_LEN, RootKind,
    Source, Transform, codec, macho,
};
use crate::error::{Error, Result};

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

    let mut tree = Tree {
        header: &header,
        delta: file,
        blob_start,
        base,
    };
    let result = tree.write(out);
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
/// On Windows, each component must also be a name Windows stores as written
/// (see [`windows_safe_name`]).
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
                && (!cfg!(windows) || windows_safe_name(part))
        });
    if ok {
        Ok(())
    } else {
        Err(Error::Delta(format!("unsafe path `{rel}` in delta")))
    }
}

/// Whether Windows stores `name` as written. `a:b` names an alternate data
/// stream of `a`, Win32 drops a trailing `.` or space (so `a.` is `a`), and
/// `CON`, `NUL`, `COM1`, or `aux.txt` open a device instead of a file.
pub(super) fn windows_safe_name(name: &str) -> bool {
    if name.contains(':') || name.ends_with('.') || name.ends_with(' ') {
        return false;
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let numbered = |prefix: &str| {
        stem.get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
            && stem.get(prefix.len()..).is_some_and(|n| {
                // A digit, or the superscript digits Windows also reserves.
                matches!(n.as_bytes(), [b'0'..=b'9']) || matches!(n, "\u{b9}" | "\u{b2}" | "\u{b3}")
            })
    };
    let device = ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .any(|d| stem.eq_ignore_ascii_case(d));
    !device && !numbered("COM") && !numbered("LPT")
}

/// A validated delta being written out over an installed base.
struct Tree<'a> {
    header: &'a Header,
    delta: File,
    blob_start: u64,
    base: &'a Path,
}

impl Tree<'_> {
    fn write(&mut self, out: &Path) -> Result<()> {
        if self.header.root == RootKind::Dir {
            fs::create_dir(out)?;
        }
        let mut dir_modes = Vec::new();
        for entry in &self.header.entries {
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
                    let rel = display_rel(&entry.path);
                    let data = self.produce(source, *size).map_err(|e| match e {
                        Error::Delta(m) => Error::Delta(format!("`{rel}`: {m}")),
                        other => other,
                    })?;
                    if data.len() as u64 != *size || !sha256_hex(&data).eq_ignore_ascii_case(sha256)
                    {
                        return Err(Error::Delta(format!(
                            "rebuilt `{rel}` does not match the expected hash"
                        )));
                    }
                    let mut file = File::create_new(&dest)?;
                    file.write_all(&data)?;
                    file.sync_all()?;
                    set_mode(&dest, *mode)?;
                }
            }
        }
        // Apply directory modes last, deepest first, so a read-only directory
        // does not block writing its children.
        for (dir, mode) in dir_modes.iter().rev() {
            set_mode(dir, *mode)?;
        }
        Ok(())
    }

    /// The content of one output file of `size` bytes.
    fn produce(&mut self, source: &Source, size: u64) -> Result<Vec<u8>> {
        match source {
            Source::Copy { base, base_sha256 } => read_base(self.base, base, base_sha256),
            Source::Patch {
                base,
                base_sha256,
                offset,
                len,
                transform,
            } => {
                let old = read_base(self.base, base, base_sha256)?;
                let old = match transform {
                    Some(Transform::MachoCodeSlots) => macho::normalize(&old).unwrap_or(old),
                    None => old,
                };
                let blob = self.blob(*offset, *len)?;
                let mut data = RawPatch::decode(&blob, size)?.apply(&old, size)?;
                if transform.is_some() {
                    macho::restore(&mut data);
                }
                Ok(data)
            }
            Source::Add { offset, len } => {
                let blob = self.blob(*offset, *len)?;
                codec::decompress(&blob, size)
            }
        }
    }

    fn blob(&mut self, offset: u64, len: u64) -> Result<Vec<u8>> {
        let len = usize::try_from(len).map_err(|_| Error::Delta("blob too large".into()))?;
        let mut buf = vec![0u8; len];
        self.delta.seek(SeekFrom::Start(self.blob_start + offset))?;
        BufReader::new(&mut self.delta).read_exact(&mut buf)?;
        Ok(buf)
    }
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
