//! Mach-O code-signature normalization.
//!
//! A signed Mach-O image embeds a code directory holding one hash per page of
//! the image. Recompiling shifts most pages, so nearly every hash changes, and
//! to bsdiff the new hashes are random bytes: on real application binaries
//! they are a sizeable share of the patch. They are also redundant, because
//! each is a function of the page it covers. Delta creation therefore zeroes
//! them in both images before diffing, and application recomputes them after
//! patching.
//!
//! The rebuilt file is still checked against the SHA-256 recorded in the delta,
//! like every other file, so a miscomputed hash can only make the delta fail
//! (and the engine fall back to the full download), never yield a broken
//! binary. Only page hashes are touched: the special slots (hashes of
//! `Info.plist`, resources, entitlements) and the CMS signature stay with
//! bsdiff, since they cannot be derived from the image alone.

use sha2::{Digest, Sha256, Sha384};

const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;
const MH_MAGIC: u32 = 0xfeed_face;
const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
const CSMAGIC_CODEDIRECTORY: u32 = 0xfade_0c02;
const CSSLOT_CODEDIRECTORY: u32 = 0;
const CSSLOT_ALTERNATE_CODEDIRECTORIES: u32 = 0x1000;
const ALTERNATE_CODEDIRECTORY_COUNT: u32 = 5;
/// Code directory versions that carry `scatterOffset` / `codeLimit64`.
const CS_SUPPORTS_SCATTER: u32 = 0x20100;
const CS_SUPPORTS_CODELIMIT64: u32 = 0x20300;
/// Sanity bounds for counts read from the file.
const MAX_FAT_ARCHS: u32 = 32;
const MAX_BLOBS: u32 = 64;

/// `data` with every recomputable page hash zeroed, or `None` when it is not
/// a signed Mach-O image (thin or universal) with a supported code directory.
pub(super) fn normalize(data: &[u8]) -> Option<Vec<u8>> {
    let slots = page_slots(data);
    if slots.is_empty() {
        return None;
    }
    let mut out = data.to_vec();
    for s in &slots {
        out[s.slots..s.slots + s.count * s.hash.len()].fill(0);
    }
    Some(out)
}

/// Recompute every page hash in place: the inverse of [`normalize`].
pub(super) fn restore(data: &mut [u8]) {
    for s in page_slots(data) {
        let page = 1usize << s.page_shift;
        let limit = s.image + s.code_limit;
        let mut digest = [0u8; 48];
        for i in 0..s.count {
            let start = s.image + i * page;
            let end = (start + page).min(limit);
            let len = s.hash.len();
            s.hash.digest(&data[start..end], &mut digest[..len]);
            let at = s.slots + i * len;
            data[at..at + len].copy_from_slice(&digest[..len]);
        }
    }
}

#[derive(Clone, Copy)]
enum PageHash {
    Sha256,
    Sha256Truncated,
    Sha384,
}

impl PageHash {
    fn from_type(hash_type: u8) -> Option<Self> {
        match hash_type {
            2 => Some(Self::Sha256),
            3 => Some(Self::Sha256Truncated),
            4 => Some(Self::Sha384),
            // SHA-1 (1) and anything newer are left untouched.
            _ => None,
        }
    }

    fn len(self) -> usize {
        match self {
            Self::Sha256 => 32,
            Self::Sha256Truncated => 20,
            Self::Sha384 => 48,
        }
    }

    fn digest(self, page: &[u8], out: &mut [u8]) {
        match self {
            Self::Sha256 | Self::Sha256Truncated => {
                out.copy_from_slice(&Sha256::digest(page)[..out.len()]);
            }
            Self::Sha384 => out.copy_from_slice(&Sha384::digest(page)),
        }
    }
}

/// The page hashes of one code directory, located in the whole file.
struct PageSlots {
    /// Start of the Mach-O image (its slice, in a universal file).
    image: usize,
    /// Absolute offset of the first page hash.
    slots: usize,
    count: usize,
    hash: PageHash,
    page_shift: u32,
    /// End of the hashed bytes, relative to `image`.
    code_limit: usize,
}

/// Every supported code directory's page hashes in `data`.
fn page_slots(data: &[u8]) -> Vec<PageSlots> {
    let mut out = Vec::new();
    let magic = be32(data, 0);
    if !matches!(magic, Some(FAT_MAGIC | FAT_MAGIC_64)) {
        image_slots(data, 0, &mut out);
        return out;
    }
    let wide = magic == Some(FAT_MAGIC_64);
    let Some(count) = be32(data, 4).filter(|&n| n > 0 && n <= MAX_FAT_ARCHS) else {
        return out;
    };
    let entry_len = if wide { 32 } else { 20 };
    for i in 0..count as usize {
        let at = 8 + i * entry_len;
        let range = if wide {
            be64(data, at + 8).zip(be64(data, at + 16))
        } else {
            be32(data, at + 8)
                .map(u64::from)
                .zip(be32(data, at + 12).map(u64::from))
        };
        let slice = range.and_then(|(offset, size)| {
            let offset = usize::try_from(offset).ok()?;
            let end = offset.checked_add(usize::try_from(size).ok()?)?;
            (end <= data.len()).then_some((offset, end))
        });
        let Some((start, end)) = slice else {
            // A malformed universal header: leave the whole file alone.
            return Vec::new();
        };
        image_slots(&data[..end], start, &mut out);
    }
    out
}

/// Code directories of the thin image starting at `start` (and ending at the
/// end of `data`).
fn image_slots(data: &[u8], start: usize, out: &mut Vec<PageSlots>) {
    let image = &data[start..];
    let header_len: usize = match le32(image, 0) {
        Some(MH_MAGIC_64) => 32,
        Some(MH_MAGIC) => 28,
        _ => return,
    };
    let (Some(ncmds), Some(sizeofcmds)) = (le32(image, 16), le32(image, 20)) else {
        return;
    };
    let Some(cmds_end) = header_len
        .checked_add(sizeofcmds as usize)
        .filter(|&end| end <= image.len())
    else {
        return;
    };
    let mut at = header_len;
    for _ in 0..ncmds {
        let (Some(cmd), Some(size)) = (le32(image, at), le32(image, at + 4)) else {
            return;
        };
        let size = size as usize;
        if size < 8 || at + size > cmds_end {
            return;
        }
        if cmd == LC_CODE_SIGNATURE {
            if let (Some(offset), Some(len)) = (le32(image, at + 8), le32(image, at + 12)) {
                signature_slots(image, start, (offset as usize, len as usize), out);
            }
            return;
        }
        at += size;
    }
}

/// Code directories in the embedded signature at `sig` (offset, length).
fn signature_slots(image: &[u8], start: usize, sig: (usize, usize), out: &mut Vec<PageSlots>) {
    let (offset, len) = sig;
    let Some(blob) = offset
        .checked_add(len)
        .and_then(|end| image.get(offset..end))
    else {
        return;
    };
    if be32(blob, 0) != Some(CSMAGIC_EMBEDDED_SIGNATURE) {
        return;
    }
    let Some(count) = be32(blob, 8).filter(|&n| n <= MAX_BLOBS) else {
        return;
    };
    let alternates = CSSLOT_ALTERNATE_CODEDIRECTORIES
        ..CSSLOT_ALTERNATE_CODEDIRECTORIES + ALTERNATE_CODEDIRECTORY_COUNT;
    for i in 0..count as usize {
        let (Some(kind), Some(at)) = (be32(blob, 12 + i * 8), be32(blob, 16 + i * 8)) else {
            return;
        };
        if kind != CSSLOT_CODEDIRECTORY && !alternates.contains(&kind) {
            continue;
        }
        if let Some(mut slots) = code_directory(image, offset + at as usize) {
            slots.image = start;
            slots.slots += start;
            out.push(slots);
        }
    }
}

/// The page hashes of the code directory at `cd` within `image`, when they can
/// be recomputed exactly.
fn code_directory(image: &[u8], cd: usize) -> Option<PageSlots> {
    if be32(image, cd)? != CSMAGIC_CODEDIRECTORY {
        return None;
    }
    let cd_end = cd.checked_add(be32(image, cd + 4)? as usize)?;
    if cd_end > image.len() {
        return None;
    }
    let version = be32(image, cd + 8)?;
    let hash_offset = be32(image, cd + 16)? as usize;
    let count = be32(image, cd + 28)? as usize;
    let mut code_limit = u64::from(be32(image, cd + 32)?);
    let hash_size = *image.get(cd + 36)?;
    let hash = PageHash::from_type(*image.get(cd + 37)?)?;
    let page_shift = u32::from(*image.get(cd + 39)?);
    if version >= CS_SUPPORTS_SCATTER && be32(image, cd + 44)? != 0 {
        return None;
    }
    if version >= CS_SUPPORTS_CODELIMIT64 {
        let wide = be64(image, cd + 56)?;
        if wide != 0 {
            code_limit = wide;
        }
    }
    let code_limit = usize::try_from(code_limit).ok()?;
    if usize::from(hash_size) != hash.len()
        || !(9..=24).contains(&page_shift)
        || code_limit > image.len()
        || count != code_limit.div_ceil(1 << page_shift)
    {
        return None;
    }
    let slots = cd.checked_add(hash_offset)?;
    let slots_end = slots.checked_add(count.checked_mul(hash.len())?)?;
    // The hashes must lie outside the bytes they cover; otherwise recomputing
    // them would change their own input.
    if slots_end > cd_end || slots < code_limit {
        return None;
    }
    Some(PageSlots {
        image: 0,
        slots,
        count,
        hash,
        page_shift,
        code_limit,
    })
}

fn bytes<const N: usize>(data: &[u8], at: usize) -> Option<[u8; N]> {
    data.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    bytes(data, at).map(u32::from_be_bytes)
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    bytes(data, at).map(u64::from_be_bytes)
}

fn le32(data: &[u8], at: usize) -> Option<u32> {
    bytes(data, at).map(u32::from_le_bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
pub(super) mod tests {
    use super::{normalize, restore};
    use sha2::{Digest, Sha256};

    const PAGE: usize = 4096;
    const HEADER_LEN: usize = 48;

    /// A minimal signed thin Mach-O built around `code`: a header and one
    /// `LC_CODE_SIGNATURE` overwrite its first 48 bytes, and a superblob with
    /// one SHA-256 code directory follows it.
    pub(in crate::delta) fn signed(code: &[u8]) -> Vec<u8> {
        let code_len = code.len();
        assert!(code_len > HEADER_LEN);
        let mut image = Vec::with_capacity(code_len + 4096);
        image.extend_from_slice(&0xfeed_facf_u32.to_le_bytes());
        image.extend_from_slice(&[0; 12]);
        image.extend_from_slice(&1u32.to_le_bytes()); // ncmds
        image.extend_from_slice(&16u32.to_le_bytes()); // sizeofcmds
        image.extend_from_slice(&[0; 8]);
        let count = code_len.div_ceil(PAGE);
        let cd_len = 88 + count * 32;
        let sig_len = 20 + cd_len;
        for value in [0x1d, 16, code_len, sig_len] {
            image.extend_from_slice(&u32::try_from(value).unwrap().to_le_bytes());
        }
        image.extend_from_slice(&code[HEADER_LEN..]);
        let mut sig = Vec::new();
        for value in [0xfade_0cc0, u32::try_from(sig_len).unwrap(), 1, 0, 20] {
            sig.extend_from_slice(&u32::to_be_bytes(value));
        }
        let header = [
            0xfade_0c02,
            u32::try_from(cd_len).unwrap(),
            0x20400,
            0,
            88,
            0,
            0,
            u32::try_from(count).unwrap(),
            u32::try_from(code_len).unwrap(),
        ];
        for value in header {
            sig.extend_from_slice(&value.to_be_bytes());
        }
        sig.extend_from_slice(&[32, 2, 0, 12]);
        sig.extend_from_slice(&[0; 88 - 40]);
        for page in image.chunks(PAGE) {
            sig.extend_from_slice(&Sha256::digest(page));
        }
        image.extend_from_slice(&sig);
        image
    }

    fn signed_image(seed: u8, code_len: usize) -> Vec<u8> {
        let mut x = u32::from(seed).wrapping_mul(2_654_435_761) | 1;
        let code: Vec<u8> = (0..code_len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x.to_le_bytes()[0]
            })
            .collect();
        signed(&code)
    }

    #[test]
    fn zeroes_and_recomputes_page_hashes() {
        let image = signed_image(1, 3 * PAGE + 100);
        let normalized = normalize(&image).unwrap();
        assert_eq!(normalized.len(), image.len());
        let hashes = image.len() - 4 * 32;
        assert!(normalized[hashes..].iter().all(|&b| b == 0));
        assert_eq!(normalized[..hashes], image[..hashes]);
        let mut restored = normalized;
        restore(&mut restored);
        assert_eq!(restored, image);
    }

    #[test]
    fn handles_universal_files() {
        let (a, b) = (signed_image(2, 2 * PAGE), signed_image(3, PAGE + 1));
        let mut fat = Vec::new();
        for value in [0xcafe_babe_u32, 2] {
            fat.extend_from_slice(&value.to_be_bytes());
        }
        let first = PAGE;
        let second = first + a.len().next_multiple_of(PAGE);
        for (offset, len) in [(first, a.len()), (second, b.len())] {
            for value in [0, 0, offset, len, 12] {
                fat.extend_from_slice(&u32::try_from(value).unwrap().to_be_bytes());
            }
        }
        fat.resize(first, 0);
        fat.extend_from_slice(&a);
        fat.resize(second, 0);
        fat.extend_from_slice(&b);

        let mut restored = normalize(&fat).unwrap();
        assert_ne!(restored, fat);
        restore(&mut restored);
        assert_eq!(restored, fat);
    }

    #[test]
    fn leaves_other_files_alone() {
        assert!(normalize(b"just some text").is_none());
        assert!(normalize(&[]).is_none());
        // A code directory whose slot count disagrees with its code limit.
        let mut image = signed_image(4, 2 * PAGE);
        let count_at = 2 * PAGE + 20 + 28;
        image[count_at + 3] = 9;
        assert!(normalize(&image).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn round_trips_a_system_binary() {
        let ls = std::fs::read("/bin/ls").unwrap();
        let mut restored = normalize(&ls).expect("/bin/ls carries a SHA-256 code directory");
        restore(&mut restored);
        assert!(restored == ls, "restored /bin/ls differs from the original");
    }
}
