//! Integrity and authenticity checks for downloaded artifacts.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Compute the lowercase hex SHA-256 of a file, streaming it in chunks.
///
/// # Errors
/// Returns [`Error::Io`] if the file cannot be read.
pub fn sha256_hex(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
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
    let data = std::fs::read(path)?;
    verify_minisign_bytes(&data, &[public_key], signature).map(drop)
}

/// Verify `data` against a detached minisign signature made by any of
/// `public_keys`, and return the signature's trusted comment.
///
/// Accepting several keys lets a publisher rotate keys: ship the new key next
/// to the old one, then start signing with it. The trusted comment is covered
/// by the signature, so it can carry authenticated metadata such as the
/// `version:` token read by [`signed_version`].
///
/// # Errors
/// Returns [`Error::Signature`] if no key is given, the signature cannot be
/// decoded, or no key verifies it.
pub fn verify_minisign_bytes(data: &[u8], public_keys: &[&str], signature: &str) -> Result<String> {
    let sig = minisign_verify::Signature::decode(signature)
        .map_err(|e| Error::Signature(format!("invalid signature: {e}")))?;
    let mut last = Error::Signature("no minisign public key configured".into());
    for key in public_keys {
        let verified = minisign_verify::PublicKey::from_base64(key.trim())
            .map_err(|e| Error::Signature(format!("invalid public key: {e}")))
            .and_then(|pk| {
                pk.verify(data, &sig, false)
                    .map_err(|e| Error::Signature(e.to_string()))
            });
        match verified {
            Ok(()) => return Ok(sig.trusted_comment().to_string()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The value of the `version:` token in a minisign trusted comment, if any.
///
/// Tokens are separated by whitespace, as in minisign's own
/// `timestamp:<n>\tfile:<name>` comments. Sign with, for example,
/// `minisign -S -t "version:1.2.3 file:App.dmg" -m App.dmg` to bind a file to
/// the version it claims to be, so an older signed file cannot be served in
/// place of a newer one.
pub fn signed_version(trusted_comment: &str) -> Option<&str> {
    trusted_comment
        .split_whitespace()
        .find_map(|token| token.strip_prefix("version:"))
        .filter(|v| !v.is_empty())
}

/// Find the SHA-256 for `asset_name` in the contents of a `shasum`-style file.
///
/// Lines look like `<hex>  <path>` (an optional `*` marks binary mode); the
/// `<path>` may carry a directory prefix, so matching is by basename.
pub(crate) fn sha256_from_sums(sums: &str, asset_name: &str) -> Option<String> {
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(path)) = (parts.next(), parts.next()) else {
            continue;
        };
        let path = path.strip_prefix('*').unwrap_or(path);
        let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
        if base == asset_name {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(contents: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("gpui-updater-test-{}.bin", std::process::id()));
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
    fn reads_the_signed_version_token() {
        assert_eq!(
            signed_version("timestamp:1700000000\tfile:App.dmg\tversion:1.2.3"),
            Some("1.2.3")
        );
        assert_eq!(
            signed_version("version:v2.0.0-rc.1 file:x"),
            Some("v2.0.0-rc.1")
        );
        assert_eq!(signed_version("timestamp:1 file:App.dmg"), None);
        assert_eq!(signed_version("version: 1.2.3"), None);
    }

    #[test]
    fn rejects_signatures_without_keys_or_with_garbage() {
        assert!(matches!(
            verify_minisign_bytes(b"x", &[], "not a signature"),
            Err(Error::Signature(_))
        ));
    }

    #[test]
    fn parses_checksum_by_basename_ignoring_dir_prefix() {
        let sums = "\
abc123  dist/app-macos.dmg
DEF456 *dist/app-linux.tar.gz
";
        assert_eq!(
            sha256_from_sums(sums, "app-macos.dmg").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            sha256_from_sums(sums, "app-linux.tar.gz").as_deref(),
            Some("def456")
        );
        assert!(sha256_from_sums(sums, "app-windows.exe").is_none());
    }
}
