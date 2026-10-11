//! Integrity and authenticity checks for downloaded artifacts.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Read buffer for streaming hashes and signature checks over large artifacts.
const CHUNK: usize = 64 * 1024;

/// Compute the lowercase hex SHA-256 of a file, streaming it in chunks.
///
/// # Errors
/// Returns [`Error::Io`] if the file cannot be read.
pub fn sha256_hex(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// Verify a file against an expected hex SHA-256 (case-insensitive).
///
/// # Errors
/// Returns [`Error::ChecksumMismatch`] if the digests differ.
pub fn verify_sha256(path: &Path, expected_hex: &str) -> Result<()> {
    let actual = sha256_hex(path)?;
    if actual.eq_ignore_ascii_case(expected_hex.trim()) {
        Ok(())
    } else {
        Err(Error::ChecksumMismatch {
            expected: expected_hex.trim().to_ascii_lowercase(),
            actual,
        })
    }
}

/// Verify a file against a detached [minisign] signature.
///
/// - `public_key`: the base64 minisign public key (the second line of a
///   `minisign.pub`, or the whole single-line key).
/// - `signature`: the contents of the `.minisig` file.
///
/// [minisign]: https://jedisct1.github.io/minisign/
///
/// # Errors
/// Returns [`Error::Signature`] if the key/signature cannot be decoded or the
/// signature does not match the file's contents.
pub fn verify_minisign(path: &Path, public_key: &str, signature: &str) -> Result<()> {
    let pk = minisign_verify::PublicKey::from_base64(public_key.trim())
        .map_err(|e| Error::Signature(format!("invalid public key: {e}")))?;
    let sig = minisign_verify::Signature::decode(signature)
        .map_err(|e| Error::Signature(format!("invalid signature: {e}")))?;
    let data = std::fs::read(path)?;
    pk.verify(&data, &sig, false)
        .map_err(|e| Error::Signature(e.to_string()))
}

/// Verify a file against a Sparkle-style Ed25519 signature.
///
/// This is the scheme behind a Sparkle appcast's `sparkle:edSignature`: a plain
/// (non-prehashed) Ed25519 signature over the file's raw bytes, checked against
/// the app's `SUPublicEDKey`. Signatures printed by Sparkle's `sign_update`
/// verify here unchanged.
///
/// - `public_key`: base64 of the 32-byte public key (`SUPublicEDKey`).
/// - `signature`: base64 of the 64-byte signature (`sparkle:edSignature`).
///
/// The file is streamed, so large disk images are not read into memory.
///
/// # Errors
/// Returns [`Error::Signature`] if the key or signature cannot be decoded or
/// the signature does not match the file's contents, and [`Error::Io`] if the
/// file cannot be read.
pub fn verify_ed25519(path: &Path, public_key: &str, signature: &str) -> Result<()> {
    let pk = decode_base64("public key", public_key).and_then(|bytes| {
        ed25519_compact::PublicKey::from_slice(&bytes)
            .map_err(|e| Error::Signature(format!("invalid Ed25519 public key: {e}")))
    })?;
    let sig = decode_base64("signature", signature).and_then(|bytes| {
        ed25519_compact::Signature::from_slice(&bytes)
            .map_err(|e| Error::Signature(format!("invalid Ed25519 signature: {e}")))
    })?;
    let mut state = pk
        .verify_incremental(&sig)
        .map_err(|e| Error::Signature(format!("invalid Ed25519 signature: {e}")))?;

    let mut file = File::open(path)?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        state.absorb(&buf[..n]);
    }
    state
        .verify()
        .map_err(|_| Error::Signature("Ed25519 signature does not match the file".into()))
}

fn decode_base64(what: &str, value: &str) -> Result<Vec<u8>> {
    BASE64
        .decode(value.trim())
        .map_err(|e| Error::Signature(format!("Ed25519 {what} is not valid base64: {e}")))
}

/// Helpers shared by tests across the crate.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::BASE64;
    use base64::Engine as _;

    /// A temp file signed with a fixed Ed25519 test key. Deleted on drop.
    pub(crate) struct Ed25519Fixture {
        pub(crate) path: PathBuf,
        pub(crate) public_key: String,
        pub(crate) signature: String,
    }

    impl Ed25519Fixture {
        pub(crate) fn new(contents: &[u8]) -> Self {
            let seed = ed25519_compact::Seed::new([7; ed25519_compact::Seed::BYTES]);
            let pair = ed25519_compact::KeyPair::from_seed(seed);
            let signature = pair.sk.sign(contents, None);
            let path = unique_temp_path("ed25519");
            std::fs::write(&path, contents).unwrap();
            Self {
                path,
                public_key: BASE64.encode(*pair.pk),
                signature: BASE64.encode(*signature),
            }
        }
    }

    impl Drop for Ed25519Fixture {
        fn drop(&mut self) {
            // Best-effort cleanup of a temp file; a leftover is harmless.
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// A temp-file path no other test in this process will use.
    pub(crate) fn unique_temp_path(tag: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gpui-updater-test-{}-{tag}-{n}.bin",
            std::process::id()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(contents: &[u8]) -> std::path::PathBuf {
        let path = test_support::unique_temp_path("sha256");
        let mut f = File::create(&path).unwrap();
        f.write_all(contents).unwrap();
        path
    }

    #[test]
    fn sha256_matches_known_vector() {
        // SHA-256("abc")
        let path = temp_file(b"abc");
        let got = sha256_hex(&path).unwrap();
        assert_eq!(
            got,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(verify_sha256(&path, &got.to_uppercase()).is_ok());
        assert!(verify_sha256(&path, "deadbeef").is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ed25519_accepts_valid_and_rejects_tampered_files() {
        let fixture = test_support::Ed25519Fixture::new(b"abc");
        assert!(verify_ed25519(&fixture.path, &fixture.public_key, &fixture.signature).is_ok());

        std::fs::write(&fixture.path, b"abd").unwrap();
        assert!(matches!(
            verify_ed25519(&fixture.path, &fixture.public_key, &fixture.signature),
            Err(Error::Signature(_))
        ));
    }

    #[test]
    fn ed25519_rejects_malformed_keys_and_signatures() {
        let fixture = test_support::Ed25519Fixture::new(b"abc");
        for (key, sig) in [
            ("not base64!", fixture.signature.as_str()),
            ("AAAA", fixture.signature.as_str()),
            (fixture.public_key.as_str(), "AAAA"),
        ] {
            assert!(matches!(
                verify_ed25519(&fixture.path, key, sig),
                Err(Error::Signature(_))
            ));
        }
    }

    /// Golden values produced by Sparkle 2.10.0's own tools, so this pins
    /// compatibility with `sparkle:edSignature` rather than with ourselves.
    ///
    /// - Private key: the 32-byte seed `[7; 32]`, passed to Sparkle as
    ///   `sign_update --ed-key-file` (base64 `BwcH…Bwc=`).
    /// - Public key: derived from that seed independently with
    ///   `openssl pkey -pubout` (what `SUPublicEDKey` would hold).
    /// - Signature: `sign_update -p` over the payload below.
    #[test]
    fn ed25519_verifies_signatures_made_by_sparkle_sign_update() {
        const SPARKLE_PUBLIC_KEY: &str = "6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=";
        const SPARKLE_SIGNATURE: &str = "fkZSsqZWXCJYAnlGh01EaGCGfLf0DLUpVDF6d7qpDPyexRhMbj85AAF61DR7ksXSCa/Yw1lZk6kDSYJOvioFAQ==";

        // 70 KiB, so the streaming verifier crosses a chunk boundary.
        let payload: Vec<u8> = (0..70 * 1024u32)
            .map(|i| u8::try_from((i * 31 + 7) % 251).unwrap())
            .collect();
        let path = temp_file(&payload);
        assert!(verify_ed25519(&path, SPARKLE_PUBLIC_KEY, SPARKLE_SIGNATURE).is_ok());

        let mut tampered = payload;
        tampered[CHUNK] ^= 1;
        std::fs::write(&path, &tampered).unwrap();
        assert!(verify_ed25519(&path, SPARKLE_PUBLIC_KEY, SPARKLE_SIGNATURE).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
