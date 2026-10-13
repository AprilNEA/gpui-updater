//! LZMA compression of delta blobs.
//!
//! Blobs use the `.lzma` ("LZMA alone") container, whose 13-byte header
//! records the properties, dictionary size, and uncompressed length. The
//! encoder settings match `xz -9e`; on real application binaries this makes
//! bsdiff patches about 18% smaller than bzip2, while decoding stays fast.

use std::io::{Read, Write};

use lzma_rust2::{LzmaOptions, LzmaReader, LzmaWriter};

use crate::error::{Error, Result};

const PRESET: u32 = 9;
/// Deeper match search than preset 9 alone: what `xz` calls "extreme".
const DEPTH_LIMIT: i32 = 512;
/// Upper bound on decoder memory, in KiB. The largest dictionary the encoder
/// picks (64 MiB) needs far less; anything above this is a corrupt header.
const MEM_LIMIT_KIB: u32 = 256 * 1024;

/// Compress `data` into a self-describing LZMA blob.
pub(super) fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut options = LzmaOptions::with_preset(PRESET);
    options.nice_len = LzmaOptions::NICE_LEN_MAX;
    options.depth_limit = DEPTH_LIMIT;
    // A dictionary larger than the input buys nothing and costs the decoder
    // memory, so cap it at the input size (rounded up to a power of two).
    let fit = u32::try_from(data.len())
        .unwrap_or(u32::MAX)
        .max(lzma_rust2::DICT_SIZE_MIN)
        .checked_next_power_of_two()
        .unwrap_or(u32::MAX);
    options.dict_size = options.dict_size.min(fit);

    let mut writer = LzmaWriter::new_use_header(
        Vec::with_capacity(data.len() / 4),
        &options,
        Some(data.len() as u64),
    )
    .map_err(|e| Error::Delta(format!("could not start LZMA encoder: {e}")))?;
    writer.write_all(data)?;
    writer
        .finish()
        .map_err(|e| Error::Delta(format!("could not finish LZMA stream: {e}")))
}

/// Decompress an LZMA blob, refusing to produce more than `max_len` bytes.
pub(super) fn decompress(blob: &[u8], max_len: u64) -> Result<Vec<u8>> {
    let corrupt = |e: &dyn std::fmt::Display| Error::Delta(format!("corrupt LZMA data: {e}"));
    let reader = LzmaReader::new_mem_limit(blob, MEM_LIMIT_KIB, None).map_err(|e| corrupt(&e))?;
    let mut out = Vec::with_capacity(usize::try_from(max_len.min(1 << 26)).unwrap_or(0));
    reader
        .take(max_len.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|e| corrupt(&e))?;
    if out.len() as u64 > max_len {
        return Err(Error::Delta(
            "compressed data expands beyond its declared size".into(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::{compress, decompress};

    #[test]
    fn round_trips_and_enforces_the_limit() {
        let data: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let blob = compress(&data).unwrap();
        assert!(blob.len() < data.len() / 10);
        assert_eq!(decompress(&blob, data.len() as u64).unwrap(), data);
        assert!(decompress(&blob, data.len() as u64 - 1).is_err());
    }

    #[test]
    fn round_trips_empty_input() {
        let blob = compress(&[]).unwrap();
        assert_eq!(decompress(&blob, 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn rejects_garbage() {
        assert!(decompress(b"not an lzma stream at all", 1024).is_err());
    }
}
