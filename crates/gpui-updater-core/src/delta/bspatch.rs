//! bsdiff patches stored as three LZMA streams.
//!
//! [`qbsdiff`] produces classic `BSDIFF40` patches, whose control, diff, and
//! extra streams are each bzip2-compressed. Creation decodes those streams and
//! recompresses them with LZMA (see [`codec`](super::codec)); application runs
//! the bspatch algorithm directly on the decoded streams. Blob layout:
//!
//! ```text
//! control len (u64 LE) | diff len (u64 LE) | extra len (u64 LE) | control | diff | extra
//! ```
//!
//! where each length is the compressed size of the stream that follows.

use std::io::Read;

use super::codec;
use crate::error::{Error, Result};

const BSDIFF40_MAGIC: &[u8; 8] = b"BSDIFF40";
const BSDIFF40_HEADER_LEN: usize = 32;
const BLOB_HEADER_LEN: usize = 24;
/// One control record: add length, copy length, seek offset.
const CONTROL_RECORD_LEN: usize = 24;

/// The decoded streams of a bsdiff patch.
pub(super) struct RawPatch {
    control: Vec<u8>,
    diff: Vec<u8>,
    extra: Vec<u8>,
}

impl RawPatch {
    /// Run bsdiff on `old` → `new` and keep the decoded streams.
    pub(super) fn diff(old: &[u8], new: &[u8]) -> Result<Self> {
        let mut patch = Vec::new();
        // The bzip2 level only matters for the throwaway BSDIFF40 encoding.
        qbsdiff::Bsdiff::new(old, new)
            .compression_level(1)
            .compare(std::io::Cursor::new(&mut patch))?;
        Self::from_bsdiff40(&patch)
    }

    /// Decode a `BSDIFF40` patch into its three raw streams.
    fn from_bsdiff40(patch: &[u8]) -> Result<Self> {
        let bad = || Error::Delta("bsdiff produced a malformed patch".into());
        if patch.len() < BSDIFF40_HEADER_LEN || &patch[..8] != BSDIFF40_MAGIC {
            return Err(bad());
        }
        let control_len = usize::try_from(read_offset(&patch[8..16])).map_err(|_| bad())?;
        let diff_len = usize::try_from(read_offset(&patch[16..24])).map_err(|_| bad())?;
        let body = &patch[BSDIFF40_HEADER_LEN..];
        let diff_end = control_len.checked_add(diff_len).ok_or_else(bad)?;
        if diff_end > body.len() {
            return Err(bad());
        }
        Ok(Self {
            control: bunzip(&body[..control_len])?,
            diff: bunzip(&body[control_len..diff_end])?,
            extra: bunzip(&body[diff_end..])?,
        })
    }

    /// Encode the streams as a delta blob.
    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        let streams = [
            codec::compress(&self.control)?,
            codec::compress(&self.diff)?,
            codec::compress(&self.extra)?,
        ];
        let total = BLOB_HEADER_LEN + streams.iter().map(Vec::len).sum::<usize>();
        let mut blob = Vec::with_capacity(total);
        for stream in &streams {
            blob.extend_from_slice(&(stream.len() as u64).to_le_bytes());
        }
        for stream in &streams {
            blob.extend_from_slice(stream);
        }
        Ok(blob)
    }

    /// Decode a delta blob for an output of exactly `new_size` bytes.
    pub(super) fn decode(blob: &[u8], new_size: u64) -> Result<Self> {
        let bad = || Error::Delta("patch blob is malformed".into());
        if blob.len() < BLOB_HEADER_LEN {
            return Err(bad());
        }
        let mut lens = [0usize; 3];
        for (i, len) in lens.iter_mut().enumerate() {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&blob[i * 8..i * 8 + 8]);
            *len = usize::try_from(u64::from_le_bytes(bytes)).map_err(|_| bad())?;
        }
        let mut rest = &blob[BLOB_HEADER_LEN..];
        let mut take = |len: usize| -> Result<&[u8]> {
            if len > rest.len() {
                return Err(bad());
            }
            let (head, tail) = rest.split_at(len);
            rest = tail;
            Ok(head)
        };
        let (control, diff, extra) = (take(lens[0])?, take(lens[1])?, take(lens[2])?);
        if !rest.is_empty() {
            return Err(bad());
        }
        let records = new_size.saturating_add(1);
        Ok(Self {
            control: codec::decompress(control, records.saturating_mul(CONTROL_RECORD_LEN as u64))?,
            diff: codec::decompress(diff, new_size)?,
            extra: codec::decompress(extra, new_size)?,
        })
    }

    /// Rebuild the new file from `old`. Every length and offset is checked, so
    /// a malformed patch yields an error rather than a panic or an
    /// out-of-bounds read; the result is exactly `new_size` bytes.
    pub(super) fn apply(&self, old: &[u8], new_size: u64) -> Result<Vec<u8>> {
        let bad = |what: &str| Error::Delta(format!("corrupt patch: {what}"));
        let size = usize::try_from(new_size).map_err(|_| bad("output too large"))?;
        if self.control.len() % CONTROL_RECORD_LEN != 0 {
            return Err(bad("truncated control record"));
        }
        let old_len = i64::try_from(old.len()).map_err(|_| bad("base too large"))?;
        let mut out = Vec::with_capacity(size);
        let (mut diff_pos, mut extra_pos) = (0usize, 0usize);
        let mut old_pos: i64 = 0;
        for record in self.control.chunks_exact(CONTROL_RECORD_LEN) {
            let add =
                usize::try_from(read_offset(&record[..8])).map_err(|_| bad("negative add"))?;
            let copy =
                usize::try_from(read_offset(&record[8..16])).map_err(|_| bad("negative copy"))?;
            let seek = read_offset(&record[16..24]);

            let diff_end = diff_pos
                .checked_add(add)
                .filter(|&end| end <= self.diff.len())
                .ok_or_else(|| bad("diff stream overrun"))?;
            if out.len().saturating_add(add) > size {
                return Err(bad("output overrun"));
            }
            // Checked before iterating so `start..` below cannot overflow.
            let next_old = i64::try_from(add)
                .ok()
                .and_then(|add| old_pos.checked_add(add))
                .ok_or_else(|| bad("seek overflow"))?;
            let start = old_pos;
            out.extend(
                self.diff[diff_pos..diff_end]
                    .iter()
                    .zip(start..)
                    .map(|(&d, at)| {
                        if (0..old_len).contains(&at) {
                            // `at` is in 0..old.len(), so the index is valid.
                            usize::try_from(at).map_or(d, |i| d.wrapping_add(old[i]))
                        } else {
                            d
                        }
                    }),
            );
            diff_pos = diff_end;
            old_pos = next_old;

            let extra_end = extra_pos
                .checked_add(copy)
                .filter(|&end| end <= self.extra.len())
                .ok_or_else(|| bad("extra stream overrun"))?;
            if out.len().saturating_add(copy) > size {
                return Err(bad("output overrun"));
            }
            out.extend_from_slice(&self.extra[extra_pos..extra_end]);
            extra_pos = extra_end;
            old_pos = old_pos
                .checked_add(seek)
                .ok_or_else(|| bad("seek overflow"))?;
        }
        if out.len() != size || diff_pos != self.diff.len() || extra_pos != self.extra.len() {
            return Err(bad("streams do not add up to the output size"));
        }
        Ok(out)
    }
}

/// bsdiff's sign-magnitude 64-bit integer.
fn read_offset(bytes: &[u8]) -> i64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    let raw = u64::from_le_bytes(buf);
    let magnitude = i64::try_from(raw & !(1 << 63)).unwrap_or(i64::MAX);
    if raw >> 63 == 1 {
        -magnitude
    } else {
        magnitude
    }
}

fn bunzip(stream: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if !stream.is_empty() {
        bzip2::read::BzDecoder::new(stream).read_to_end(&mut out)?;
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::{RawPatch, read_offset};

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

    #[test]
    fn decodes_sign_magnitude_offsets() {
        assert_eq!(read_offset(&5u64.to_le_bytes()), 5);
        assert_eq!(read_offset(&((1u64 << 63) | 7).to_le_bytes()), -7);
    }

    #[test]
    fn round_trips_through_a_blob() {
        let old = noise(1, 64 * 1024);
        let mut new = old.clone();
        for i in (0..new.len()).step_by(500) {
            new[i] = new[i].wrapping_add(1);
        }
        new.splice(1000..1000, noise(2, 3000));
        new.truncate(60_000);
        let blob = RawPatch::diff(&old, &new).unwrap().encode().unwrap();
        let size = new.len() as u64;
        let rebuilt = RawPatch::decode(&blob, size)
            .unwrap()
            .apply(&old, size)
            .unwrap();
        assert_eq!(rebuilt, new);
    }

    #[test]
    fn rejects_a_wrong_size_or_truncated_blob() {
        let old = noise(3, 10_000);
        let mut new = old.clone();
        new[10] ^= 1;
        let blob = RawPatch::diff(&old, &new).unwrap().encode().unwrap();
        let size = new.len() as u64;
        let raw = RawPatch::decode(&blob, size).unwrap();
        assert!(raw.apply(&old, size - 1).is_err());
        assert!(raw.apply(&old, size + 1).is_err());
        assert!(RawPatch::decode(&blob[..blob.len() - 1], size).is_err());
        assert!(RawPatch::decode(&blob[..10], size).is_err());
    }

    #[test]
    fn rejects_records_that_overrun_the_streams() {
        let mut control = Vec::new();
        for value in [10u64, 0, 0] {
            control.extend_from_slice(&value.to_le_bytes());
        }
        let raw = RawPatch {
            control,
            diff: vec![0; 4],
            extra: Vec::new(),
        };
        assert!(raw.apply(&[1, 2, 3], 10).is_err());
    }
}
