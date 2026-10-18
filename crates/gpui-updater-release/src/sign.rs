//! Signing: Sparkle's Ed25519 (`sparkle:edSignature`) and minisign.
//!
//! Every signature is checked with `gpui-updater-core`'s own verifier right
//! after it is made, so a release cannot publish a signature its clients would
//! reject.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_compact::{KeyPair, Seed};
use gpui_updater_core::verify;

use crate::error::{Error, Result};
use crate::util::file_name;

/// A Sparkle Ed25519 private key.
///
/// Sparkle signs the raw bytes of each archive and delta with plain Ed25519.
/// The key is the base64 32-byte seed that `generate_keys -x <file>` exports.
/// A 64-byte value (seed followed by public key) is also accepted, after its
/// two halves are checked to belong together.
pub struct SparkleKey {
    pair: KeyPair,
}

impl std::fmt::Debug for SparkleKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SparkleKey")
            .field("public_key", &self.public_key_base64())
            .finish_non_exhaustive()
    }
}

impl SparkleKey {
    /// Read a key from its base64 export. Surrounding whitespace is ignored.
    pub fn from_base64(secret: &str) -> Result<Self> {
        let bytes = STANDARD
            .decode(secret.trim())
            .map_err(|e| Error::Key(format!("the Sparkle private key is not base64: {e}")))?;
        match bytes.len() {
            32 => {
                let mut seed = [0u8; 32];
                seed.copy_from_slice(&bytes);
                Ok(Self {
                    pair: KeyPair::from_seed(Seed::new(seed)),
                })
            }
            64 => {
                let pair = KeyPair::from_slice(&bytes)
                    .map_err(|e| Error::Key(format!("invalid 64-byte Sparkle key: {e}")))?;
                let derived = KeyPair::from_seed(pair.sk.seed());
                if derived.pk != pair.pk {
                    return Err(Error::Key(
                        "the 64-byte Sparkle key's public half does not match its seed".into(),
                    ));
                }
                Ok(Self { pair })
            }
            96 => Err(Error::Key(
                "this is a legacy 96-byte Sparkle key; re-export it with Sparkle 2's \
                 `generate_keys -x <file>`, which writes the 32-byte seed"
                    .into(),
            )),
            n => Err(Error::Key(format!(
                "a Sparkle private key decodes to 32 bytes, not {n}"
            ))),
        }
    }

    /// Generate a new random key, for tests and first-time setup.
    pub fn generate() -> Self {
        Self {
            pair: KeyPair::from_seed(Seed::generate()),
        }
    }

    /// The private key as base64, in the format [`from_base64`](Self::from_base64)
    /// reads.
    pub fn to_base64(&self) -> String {
        STANDARD.encode(self.pair.sk.seed().as_ref())
    }

    /// The public key as base64, the value of `SUPublicEDKey`.
    pub fn public_key_base64(&self) -> String {
        STANDARD.encode(self.pair.pk.as_ref())
    }

    /// Fail unless this key's public half is `expected` (base64), for example
    /// the `SUPublicEDKey` that shipped apps trust.
    pub fn expect_public_key(&self, expected: &str) -> Result<()> {
        let actual = self.public_key_base64();
        if actual == expected.trim() {
            Ok(())
        } else {
            Err(Error::Key(format!(
                "the Sparkle private key's public key is {actual}, but {} is expected; \
                 shipped apps would reject every signature",
                expected.trim()
            )))
        }
    }

    /// Sign bytes; returns the base64 signature.
    pub fn sign_bytes(&self, data: &[u8]) -> String {
        STANDARD.encode(self.pair.sk.sign(data, None).as_ref())
    }

    /// Sign a file and check the result with the client-side verifier;
    /// returns the base64 signature for `sparkle:edSignature`.
    pub fn sign_file(&self, path: &Path) -> Result<String> {
        let data = fs::read(path).map_err(Error::io("read", path))?;
        let signature = self.sign_bytes(&data);
        verify::verify_ed25519(path, &self.public_key_base64(), &signature)
            .map_err(|e| Error::Signature(format!("self-check of `{}`: {e}", path.display())))?;
        Ok(signature)
    }
}

/// What goes into a minisign signature's comments.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct MinisignOptions {
    /// Version the file is, or for a delta the version it produces. When set,
    /// the trusted comment carries `version:<v>`, which clients compare with
    /// the release they expect (see `VersionBinding` in `gpui-updater-core`).
    pub version: Option<String>,
    /// Replace the whole trusted comment. Takes precedence over `version`.
    pub trusted_comment: Option<String>,
    /// The untrusted comment line; minisign's default when `None`.
    pub untrusted_comment: Option<String>,
    /// Leave out the `file:<name>` token that minisign's tool adds.
    pub omit_file_name: bool,
}

impl MinisignOptions {
    /// Options that bind the signature to `version`.
    pub fn version(version: impl Into<String>) -> Self {
        Self {
            version: Some(version.into()),
            ..Self::default()
        }
    }

    fn trusted_comment(&self, path: &Path) -> Result<String> {
        if let Some(comment) = &self.trusted_comment {
            return Ok(comment.clone());
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut tokens = vec![format!("timestamp:{timestamp}")];
        if !self.omit_file_name {
            tokens.push(format!("file:{}", file_name(path)?));
        }
        if let Some(version) = &self.version {
            let version = version.trim().trim_start_matches('v');
            if version.is_empty() || version.contains(char::is_whitespace) {
                return Err(Error::invalid(format!(
                    "`{version}` cannot be a signed version"
                )));
            }
            tokens.push(format!("version:{version}"));
        }
        tokens.push("hashed".into());
        Ok(tokens.join("\t"))
    }
}

/// A minisign secret key.
pub struct MinisignKey {
    secret: minisign::SecretKey,
    public: minisign::PublicKey,
}

impl std::fmt::Debug for MinisignKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MinisignKey")
            .field("public_key", &self.public_key_base64())
            .finish_non_exhaustive()
    }
}

impl MinisignKey {
    /// Read a key from the text of a minisign secret-key file
    /// (`untrusted comment: …` plus the base64 box). Pass `password` for an
    /// encrypted key and `None` for one made with `minisign -G -W`.
    ///
    /// This never prompts: `minisign`'s own `None` reads a password from the
    /// terminal, which would hang an interactive shell and read nothing in
    /// CI. Here `None` means "unencrypted", and an encrypted key without its
    /// password is a "wrong password" error.
    pub fn from_box(contents: &str, password: Option<String>) -> Result<Self> {
        let boxed = minisign::SecretKeyBox::from_string(contents.trim())
            .map_err(|e| Error::Key(format!("minisign secret key: {e}")))?;
        let secret = boxed
            .into_secret_key(Some(password.unwrap_or_default()))
            .map_err(|e| Error::Key(format!("minisign secret key: {e}")))?;
        let public = minisign::PublicKey::from_secret_key(&secret)
            .map_err(|e| Error::Key(format!("minisign public key: {e}")))?;
        Ok(Self { secret, public })
    }

    /// Read a key from a minisign secret-key file.
    pub fn from_file(path: &Path, password: Option<String>) -> Result<Self> {
        let contents = fs::read_to_string(path).map_err(Error::io("read", path))?;
        Self::from_box(&contents, password)
    }

    /// Generate a new unencrypted key pair, for tests and first-time setup.
    pub fn generate() -> Result<Self> {
        // An empty password skips encryption but, unlike
        // `generate_unencrypted_keypair`, writes the checksum, so the key
        // can be saved with `to_box` and read back.
        let pair = minisign::KeyPair::generate_encrypted_keypair(Some(String::new()))
            .map_err(|e| Error::Key(format!("generate minisign key: {e}")))?;
        Ok(Self {
            secret: pair.sk,
            public: pair.pk,
        })
    }

    /// The secret key as minisign's file text, unencrypted.
    pub fn to_box(&self) -> Result<String> {
        self.secret
            .to_box(None)
            .map(minisign::SecretKeyBox::into_string)
            .map_err(|e| Error::Key(format!("minisign secret key: {e}")))
    }

    /// The public key as base64, the value clients configure.
    pub fn public_key_base64(&self) -> String {
        self.public.to_base64()
    }

    /// Fail unless this key's public half is `expected` (base64).
    pub fn expect_public_key(&self, expected: &str) -> Result<()> {
        let actual = self.public_key_base64();
        if actual == expected.trim() {
            Ok(())
        } else {
            Err(Error::Key(format!(
                "the minisign secret key's public key is {actual}, but {} is expected",
                expected.trim()
            )))
        }
    }

    /// Sign a file and check the result with the client-side verifier,
    /// including the signed version; returns the `.minisig` contents.
    pub fn sign_file(&self, path: &Path, options: &MinisignOptions) -> Result<String> {
        let trusted = options.trusted_comment(path)?;
        let file = fs::File::open(path).map_err(Error::io("open", path))?;
        let signature = minisign::sign(
            Some(&self.public),
            &self.secret,
            std::io::BufReader::new(file),
            Some(&trusted),
            options.untrusted_comment.as_deref(),
        )
        .map_err(|e| Error::Signature(format!("sign `{}`: {e}", path.display())))?
        .to_string();
        let public = self.public_key_base64();
        let comment = verify::verify_minisign_file(path, &[public.as_str()], &signature)
            .map_err(|e| Error::Signature(format!("self-check of `{}`: {e}", path.display())))?;
        if let Some(expected) = &options.version {
            let expected = expected.trim().trim_start_matches('v');
            if options.trusted_comment.is_none()
                && verify::signed_version(&comment) != Some(expected)
            {
                return Err(Error::Signature(format!(
                    "the signature of `{}` does not carry version {expected}",
                    path.display()
                )));
            }
        }
        Ok(signature)
    }

    /// Sign a file and write the signature next to it as `<file>.minisig`;
    /// returns that path.
    pub fn sign_file_to_sidecar(&self, path: &Path, options: &MinisignOptions) -> Result<PathBuf> {
        let signature = self.sign_file(path, options)?;
        let out = sidecar(path, "minisig");
        fs::write(&out, signature).map_err(Error::io("write", &out))?;
        Ok(out)
    }
}

/// `<path>.<extension>`, keeping every existing extension.
pub(crate) fn sidecar(path: &Path, extension: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(extension);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn sparkle_key_round_trips_and_matches_the_core_verifier() {
        let key = SparkleKey::generate();
        let again = SparkleKey::from_base64(&format!("  {}\n", key.to_base64())).unwrap();
        assert_eq!(key.public_key_base64(), again.public_key_base64());
        key.expect_public_key(&again.public_key_base64()).unwrap();
        assert!(
            key.expect_public_key(&SparkleKey::generate().public_key_base64())
                .is_err()
        );

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("App.dmg");
        fs::write(&file, b"payload").unwrap();
        let sig = key.sign_file(&file).unwrap();
        verify::verify_ed25519(&file, &key.public_key_base64(), &sig).unwrap();
    }

    #[test]
    fn sparkle_key_matches_the_shared_test_vector() {
        // Same seed as gpui-updater-core's Ed25519 verifier tests.
        let key = SparkleKey::from_base64(&STANDARD.encode([7u8; 32])).unwrap();
        let mut sixty_four = [7u8; 32].to_vec();
        sixty_four.extend(STANDARD.decode(key.public_key_base64()).unwrap());
        let long = SparkleKey::from_base64(&STANDARD.encode(&sixty_four)).unwrap();
        assert_eq!(long.public_key_base64(), key.public_key_base64());
        sixty_four[40] ^= 1;
        assert!(SparkleKey::from_base64(&STANDARD.encode(&sixty_four)).is_err());
    }

    #[test]
    fn sparkle_key_rejects_legacy_and_garbage() {
        let legacy = SparkleKey::from_base64(&STANDARD.encode([1u8; 96])).unwrap_err();
        assert!(legacy.to_string().contains("generate_keys -x"), "{legacy}");
        assert!(SparkleKey::from_base64("not base64!").is_err());
        assert!(SparkleKey::from_base64(&STANDARD.encode([1u8; 31])).is_err());
    }

    #[test]
    fn minisign_signs_with_a_bound_version() {
        let key = MinisignKey::generate().unwrap();
        let reread = MinisignKey::from_box(&key.to_box().unwrap(), None).unwrap();
        assert_eq!(reread.public_key_base64(), key.public_key_base64());

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("App.msi");
        fs::write(&file, b"payload").unwrap();
        let out = key
            .sign_file_to_sidecar(&file, &MinisignOptions::version("v1.2.3"))
            .unwrap();
        assert_eq!(out, dir.path().join("App.msi.minisig"));
        let sig = fs::read_to_string(&out).unwrap();
        let comment =
            verify::verify_minisign_file(&file, &[key.public_key_base64().as_str()], &sig).unwrap();
        assert_eq!(verify::signed_version(&comment), Some("1.2.3"));
        assert!(comment.contains("file:App.msi"), "{comment}");
    }

    #[test]
    fn minisign_rejects_an_unusable_version() {
        let key = MinisignKey::generate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        fs::write(&file, b"x").unwrap();
        assert!(
            key.sign_file(&file, &MinisignOptions::version("1 2"))
                .is_err()
        );
        let custom = MinisignOptions {
            trusted_comment: Some("release:1".into()),
            ..MinisignOptions::default()
        };
        let sig = key.sign_file(&file, &custom).unwrap();
        assert!(sig.contains("trusted comment: release:1"), "{sig}");
    }
}
