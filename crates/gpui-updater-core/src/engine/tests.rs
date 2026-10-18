#![allow(clippy::unwrap_used, reason = "tests")]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::error::Error;
use crate::release::Asset;

fn release(sha256: Option<&str>, signature: Option<&str>, signature_url: Option<&str>) -> Release {
    let mut release = Release::new(
        Version::new(1, 0, 0),
        Asset::new("app.dmg", "https://example.test/app.dmg", 0),
    );
    release.signature = signature.map(str::to_string);
    release.signature_url = signature_url.map(str::to_string);
    release.sha256 = sha256.map(str::to_string);
    release
}

fn ed25519_release(signature: &str) -> Release {
    release(None, None, None).with_ed25519_signature(signature)
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
    .map(|checked| assert!(checked, "no signature was checked"))
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

#[test]
fn signature_policy_accepts_an_ed25519_signature_with_its_key() {
    let cfg = config(Verification::Signature, None).ed25519_public_key("edkey");
    assert!(cfg.precheck(&ed25519_release("edsig")).is_ok());
    // A blank Ed25519 signature counts as absent.
    assert!(matches!(
        cfg.precheck(&ed25519_release("  ")),
        Err(Error::VerificationRequired(_))
    ));
}

#[test]
fn signature_policy_needs_a_key_for_the_published_signature_kind() {
    // Only a minisign key, but the release is signed with Ed25519 alone.
    let cfg = config(Verification::Signature, Some("pubkey"));
    assert!(matches!(
        cfg.precheck(&ed25519_release("edsig")),
        Err(Error::VerificationRequired(_))
    ));
    // Only an Ed25519 key, but the release is signed with minisign alone.
    let cfg = config(Verification::Signature, None).ed25519_public_key("edkey");
    assert!(matches!(
        cfg.precheck(&release(None, Some("sig"), None)),
        Err(Error::VerificationRequired(_))
    ));
}

#[test]
fn verifies_ed25519_signed_artifacts_and_rejects_tampering() {
    let fixture = crate::verify::test_support::Ed25519Fixture::new(b"update payload");
    let cfg = config(Verification::Signature, None).ed25519_public_key(&fixture.public_key);
    let signed = ed25519_release(&fixture.signature);
    let verify = |cfg: &EngineConfig| {
        cfg.verify_file(
            &fixture.path,
            Integrity::of_release(&signed),
            &signed.version,
        )
    };
    assert!(verify(&cfg).is_ok());

    std::fs::write(&fixture.path, b"update payloaD").unwrap();
    assert!(matches!(verify(&cfg), Err(Error::Signature(_))));
}

#[test]
fn ed25519_satisfies_strict_deltas_too() {
    // A delta signed only with Ed25519 passes the signature gate when the
    // Ed25519 key is configured, and fails it when only minisign is.
    let mut delta = crate::release::DeltaAsset::new(
        Version::new(0, 1, 0),
        Version::new(1, 0, 0),
        Asset::new("app.dmg.from-0.1.0.delta", "https://example.test/d", 0),
    );
    delta.sha256 = Some("ab".into());
    delta.ed25519_signature = Some("edsig".into());
    let ed = config(Verification::Strict, None).ed25519_public_key("edkey");
    assert!(ed.precheck_integrity(Integrity::of_delta(&delta)).is_ok());
    let minisign_only = config(Verification::Strict, Some("pubkey"));
    assert!(matches!(
        minisign_only.precheck_integrity(Integrity::of_delta(&delta)),
        Err(Error::VerificationRequired(_))
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

/// A source that always offers the same release.
struct FixedSource(Release);

impl UpdateSource for FixedSource {
    fn fetch_latest(&self) -> Result<Release> {
        Ok(self.0.clone())
    }
}

/// Serve `body` to every request on a loopback port, returning its URL.
fn serve(body: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.read(&mut [0u8; 1024]);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(body);
        }
    });
    format!("http://{addr}/download")
}

#[test]
fn check_refuses_releases_fetched_without_https() {
    let check = |release: Release| {
        UpdateEngine::new(FixedSource(release), config(Verification::BestEffort, None)).check()
    };
    let mut plain = release(None, None, None);
    plain.asset.url = "http://example.com/app.dmg".into();
    assert!(matches!(check(plain), Err(Error::InsecureUrl(_))));
    let plain_signature = release(None, None, Some("http://example.com/app.dmg.minisig"));
    assert!(matches!(check(plain_signature), Err(Error::InsecureUrl(_))));
    assert!(check(release(None, None, None)).unwrap().is_some());
}

#[test]
fn keeps_hostile_asset_names_inside_the_download_dir() {
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("downloads");
    let outside = dir.path().join("escaped.dmg");
    let url = serve(b"artifact");
    let engine = UpdateEngine::new(
        PanicSource,
        config(Verification::Off, None).download_dir(&downloads),
    );
    for name in ["../escaped.dmg".to_string(), outside.display().to_string()] {
        let mut hostile = release(None, None, None);
        hostile.asset = Asset::new(name.as_str(), url.as_str(), 0);
        let artifact = engine.download(&hostile, |_, _| {}).unwrap();
        assert_eq!(artifact.path(), downloads.join("escaped.dmg"), "{name}");
        assert!(!outside.exists(), "{name} escaped the download directory");
    }
}

#[cfg(unix)]
#[test]
fn download_dirs_are_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = |dir: &Path| std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;

    let engine = UpdateEngine::new(PanicSource, config(Verification::Off, None));
    let (first, second) = (
        engine.download_dir().unwrap(),
        engine.download_dir().unwrap(),
    );
    assert_ne!(first, second, "each download gets a fresh directory");
    assert_eq!(mode(&first), 0o700);
    std::fs::remove_dir(first).unwrap();
    std::fs::remove_dir(second).unwrap();

    let parent = tempfile::tempdir().unwrap();
    let custom = parent.path().join("updates/cache");
    let engine = UpdateEngine::new(
        PanicSource,
        config(Verification::Off, None).download_dir(&custom),
    );
    assert_eq!(engine.download_dir().unwrap(), custom);
    assert_eq!(mode(&custom), 0o700);
    assert_eq!(mode(&parent.path().join("updates")), 0o700);
}

#[test]
fn a_cancelled_download_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("downloads");
    let url = serve(b"artifact");
    let engine = UpdateEngine::new(
        PanicSource,
        config(Verification::Off, None).download_dir(&downloads),
    );
    let mut wanted = release(None, None, None);
    wanted.asset = Asset::new("app.dmg", url.as_str(), 0);
    let cancelled = CancelToken::new();
    cancelled.cancel();
    assert!(matches!(
        engine.download_cancellable(&wanted, &cancelled, |_, _| {}),
        Err(Error::Cancelled)
    ));
    assert!(!downloads.join("app.dmg").exists());
    // A fresh token downloads as before.
    let artifact = engine
        .download_cancellable(&wanted, &CancelToken::new(), |_, _| {})
        .unwrap();
    assert_eq!(std::fs::read(artifact.path()).unwrap(), b"artifact");
}

/// Serve `body` to every request, counting the requests.
fn serve_counting(body: &'static [u8]) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.read(&mut [0u8; 1024]);
            seen.fetch_add(1, Ordering::SeqCst);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(body);
        }
    });
    (format!("http://{addr}/download"), count)
}

/// An engine that keeps downloads in `downloads` and skips the macOS
/// payload check, which these fixtures cannot pass.
fn reusing_engine(verification: Verification, downloads: &Path) -> UpdateEngine<PanicSource> {
    UpdateEngine::new(
        PanicSource,
        config(verification, None)
            .download_dir(downloads)
            .install_root(downloads.join("installed"))
            .bundle_version(BundleVersion::Ignore)
            .code_signing(CodeSigning::Off),
    )
}

const BODY: &[u8] = b"artifact";

fn reusable_release(url: &str, sha256: Option<&str>) -> Release {
    let mut wanted = release(sha256, None, None);
    wanted.asset = Asset::new("app.bin", url, BODY.len() as u64);
    wanted
}

fn body_sha256() -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(BODY))
}

#[test]
fn reuses_a_verified_earlier_download() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = serve_counting(BODY);
    let engine = reusing_engine(Verification::Checksum, dir.path());
    let wanted = reusable_release(&url, Some(&body_sha256()));
    engine.download(&wanted, |_, _| {}).unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    let mut reported = Vec::new();
    let artifact = engine
        .download(&wanted, |got, total| reported.push((got, total)))
        .unwrap();
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the file was downloaded again"
    );
    assert_eq!(std::fs::read(artifact.path()).unwrap(), BODY);
    assert_eq!(reported, [(8, Some(8))]);
}

#[test]
fn downloads_again_over_a_file_that_fails_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = serve_counting(BODY);
    let engine = reusing_engine(Verification::Checksum, dir.path());
    // Same size, other bytes.
    std::fs::write(dir.path().join("app.bin"), b"tampered").unwrap();
    let wanted = reusable_release(&url, Some(&body_sha256()));
    let artifact = engine.download(&wanted, |_, _| {}).unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(artifact.path()).unwrap(), BODY);
}

#[test]
fn does_not_reuse_a_file_nothing_can_check() {
    let dir = tempfile::tempdir().unwrap();
    let (url, requests) = serve_counting(BODY);
    for verification in [Verification::BestEffort, Verification::Off] {
        let engine = reusing_engine(verification, dir.path());
        let wanted = reusable_release(&url, None);
        engine.download(&wanted, |_, _| {}).unwrap();
        engine.download(&wanted, |_, _| {}).unwrap();
    }
    assert_eq!(requests.load(Ordering::SeqCst), 4);
}

#[cfg(unix)]
#[test]
fn an_install_location_the_user_may_not_write_fails_before_downloading() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let (url, requests) = serve_counting(b"artifact");
    let engine = UpdateEngine::new(
        PanicSource,
        config(Verification::Off, None)
            .download_dir(dir.path().join("downloads"))
            .install_root(locked.join("App.app")),
    );
    let mut wanted = release(None, None, None);
    wanted.asset = Asset::new("app.dmg", url.as_str(), 0);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    // A privileged user may write anyway, so the check passes there.
    let privileged = std::fs::write(locked.join("probe"), b"").is_ok();
    let checked = engine.check_location(&wanted);
    let downloaded = engine.download(&wanted, |_, _| {});
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if privileged {
        return;
    }
    assert!(matches!(checked, Err(Error::Permission(_))), "{checked:?}");
    assert!(matches!(downloaded, Err(Error::Permission(_))));
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert!(!dir.path().join("downloads").exists());
}
