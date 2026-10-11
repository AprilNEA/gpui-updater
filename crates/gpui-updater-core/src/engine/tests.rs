#![allow(clippy::unwrap_used, reason = "tests")]

use super::*;
use crate::error::Error;
use crate::release::Asset;

fn release(sha256: Option<&str>, signature: Option<&str>, signature_url: Option<&str>) -> Release {
    Release {
        version: Version::new(1, 0, 0),
        notes: None,
        asset: Asset {
            name: "app.dmg".to_string(),
            url: "https://example.test/app.dmg".to_string(),
            size: 0,
        },
        signature: signature.map(str::to_string),
        signature_url: signature_url.map(str::to_string),
        sha256: sha256.map(str::to_string),
        deltas: Vec::new(),
    }
}

fn config(verification: Verification, key: Option<&str>) -> EngineConfig {
    let mut config = EngineConfig::new(Version::new(0, 1, 0)).verification(verification);
    if let Some(key) = key {
        config = config.minisign_public_key(key);
    }
    config
}

#[test]
fn best_effort_accepts_unverifiable_release() {
    // No checksum, no signature, no key: the default fails open.
    assert!(
        config(Verification::BestEffort, None)
            .precheck(&release(None, None, None))
            .is_ok()
    );
}

#[test]
fn checksum_policy_requires_published_checksum() {
    let cfg = config(Verification::Checksum, None);
    assert!(matches!(
        cfg.precheck(&release(None, None, None)),
        Err(Error::VerificationRequired(_))
    ));
    assert!(cfg.precheck(&release(Some("abc"), None, None)).is_ok());
}

#[test]
fn signature_policy_requires_public_key() {
    // Key missing, even though the release advertises a signature.
    let cfg = config(Verification::Signature, None);
    assert!(matches!(
        cfg.precheck(&release(
            None,
            None,
            Some("https://example.test/app.dmg.minisig")
        )),
        Err(Error::VerificationRequired(_))
    ));
}

#[test]
fn signature_policy_requires_advertised_signature() {
    let cfg = config(Verification::Signature, Some("pubkey"));
    assert!(matches!(
        cfg.precheck(&release(Some("abc"), None, None)),
        Err(Error::VerificationRequired(_))
    ));
    // A blank signature reference is treated as absent.
    assert!(matches!(
        cfg.precheck(&release(None, Some("  "), Some(""))),
        Err(Error::VerificationRequired(_))
    ));
    assert!(
        cfg.precheck(&release(None, Some("trusted comment: …"), None))
            .is_ok()
    );
    assert!(
        cfg.precheck(&release(
            None,
            None,
            Some("https://example.test/app.dmg.minisig")
        ))
        .is_ok()
    );
}

#[test]
fn strict_policy_requires_both_signature_and_checksum() {
    let cfg = config(Verification::Strict, Some("pubkey"));
    // Signature present but checksum missing.
    assert!(matches!(
        cfg.precheck(&release(None, Some("sig"), None)),
        Err(Error::VerificationRequired(_))
    ));
    assert!(
        cfg.precheck(&release(Some("abc"), Some("sig"), None))
            .is_ok()
    );
}

#[test]
fn off_policy_skips_verification_entirely() {
    // `verify_artifact` returns Ok without touching the (nonexistent) file
    // even though the release claims a checksum.
    let engine = UpdateEngine::new(PanicSource, config(Verification::Off, None));
    let path = Path::new("/nonexistent/app.dmg");
    assert!(
        engine
            .config
            .verify_file(
                path,
                Integrity::of_release(&release(Some("abc"), None, None)),
                &Version::new(1, 0, 0)
            )
            .is_ok()
    );
}

#[test]
fn blank_checksum_is_treated_as_absent_not_verified() {
    // A `"sha256": ""` (or whitespace) field reaches us as Some("") from the
    // manifest source. It must be skipped like a missing checksum, not run
    // through verify_sha256 against a blank expected hash — which would read
    // the (here nonexistent) artifact and spuriously fail every download.
    let engine = UpdateEngine::new(PanicSource, config(Verification::BestEffort, None));
    let path = Path::new("/nonexistent/app.dmg");
    for blank in ["", "   "] {
        assert!(
            engine
                .config
                .verify_file(
                    path,
                    Integrity::of_release(&release(Some(blank), None, None)),
                    &Version::new(1, 0, 0)
                )
                .is_ok(),
            "blank sha256 {blank:?} should be treated as absent"
        );
    }
}

/// A throwaway minisign key pair.
struct Signer {
    keys: minisign::KeyPair,
}

impl Signer {
    fn new() -> Self {
        Self {
            keys: minisign::KeyPair::generate_unencrypted_keypair().unwrap(),
        }
    }

    fn public_key(&self) -> String {
        self.keys.pk.to_base64()
    }

    fn sign(&self, data: &[u8], trusted_comment: &str) -> String {
        minisign::sign(
            None,
            &self.keys.sk,
            std::io::Cursor::new(data),
            Some(trusted_comment),
            None,
        )
        .unwrap()
        .to_string()
    }
}

/// Verify a file signed with `signature` as the artifact of release `version`.
fn verify_signed(cfg: &EngineConfig, signature: &str, version: &str) -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.dmg");
    std::fs::write(&path, b"artifact").unwrap();
    let rel = release(None, Some(signature), None);
    cfg.verify_file(
        &path,
        Integrity::of_release(&rel),
        &Version::parse(version).unwrap(),
    )
}

#[test]
fn accepts_a_signature_by_any_trusted_key() {
    let (old, new, other) = (Signer::new(), Signer::new(), Signer::new());
    let cfg = config(Verification::Signature, Some(&old.public_key()))
        .minisign_public_key(new.public_key());
    for signer in [&old, &new] {
        let signature = signer.sign(b"artifact", "file:app.dmg");
        assert!(verify_signed(&cfg, &signature, "1.0.0").is_ok());
    }
    let signature = other.sign(b"artifact", "file:app.dmg");
    assert!(matches!(
        verify_signed(&cfg, &signature, "1.0.0"),
        Err(Error::Signature(_))
    ));
}

#[test]
fn binds_a_signature_to_the_version_it_names() {
    let signer = Signer::new();
    let cfg = config(Verification::Signature, Some(&signer.public_key()));
    let bound = signer.sign(b"artifact", "timestamp:1\tfile:app.dmg\tversion:1.2.0");
    assert!(verify_signed(&cfg, &bound, "1.2.0").is_ok());
    // An older signed artifact served as a newer release is rejected.
    let err = verify_signed(&cfg, &bound, "1.3.0").unwrap_err();
    assert!(
        err.to_string().contains("signed for version 1.2.0"),
        "{err}"
    );

    // Without a `version:` token, only `Required` rejects the signature.
    let unbound = signer.sign(b"artifact", "file:app.dmg");
    assert!(verify_signed(&cfg, &unbound, "1.3.0").is_ok());
    let strict = cfg.version_binding(VersionBinding::Required);
    assert!(matches!(
        verify_signed(&strict, &unbound, "1.3.0"),
        Err(Error::Signature(_))
    ));
    assert!(verify_signed(&strict, &bound, "1.2.0").is_ok());
}

#[test]
fn rejects_a_tampered_trusted_comment() {
    let signer = Signer::new();
    let cfg = config(Verification::Signature, Some(&signer.public_key()));
    let genuine = signer.sign(b"artifact", "version:1.2.0");
    let forged = genuine.replace("version:1.2.0", "version:1.3.0");
    assert!(matches!(
        verify_signed(&cfg, &forged, "1.3.0"),
        Err(Error::Signature(_))
    ));
}

/// A source whose `fetch_latest` must never be called: the policy tests
/// exercise verification, not fetching.
struct PanicSource;

impl UpdateSource for PanicSource {
    fn fetch_latest(&self) -> Result<Release> {
        unreachable!("policy tests do not fetch")
    }
}
