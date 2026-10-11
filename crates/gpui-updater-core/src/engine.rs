//! The blocking update orchestrator that ties the pieces together.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use semver::Version;

use crate::error::{Error, Result};
use crate::install::{self, Installed};
use crate::release::Release;
use crate::source::UpdateSource;
use crate::{http, verify};

/// How strictly [`UpdateEngine`] enforces integrity and authenticity checks on
/// a release.
///
/// A "signature" is either a minisign signature (checked against
/// [`EngineConfig::minisign_public_key`]) or a Sparkle-style Ed25519
/// signature (checked against [`EngineConfig::ed25519_public_key`]). Every
/// signature the release publishes *and* a configured key can check is
/// verified, and any mismatch fails the download.
///
/// The default, [`BestEffort`](Verification::BestEffort), verifies whatever the
/// release provides and *skips* a check when its input is absent — so an
/// unsigned release installs with only a checksum, or with nothing. The
/// `Signature`/`Strict` modes instead fail closed: a release that cannot be
/// verified is rejected at [`check`](UpdateEngine::check) time (before it is
/// surfaced as available) and again at [`download`](UpdateEngine::download)
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Verification {
    /// Verify whatever the release and config provide; skip a check when its
    /// input is missing (no checksum, no signature, or no public key). The
    /// backwards-compatible default — note it fails *open*.
    #[default]
    BestEffort,
    /// Skip all integrity and authenticity checks. For tests and local
    /// development only.
    Off,
    /// Require a matching SHA-256. Fails closed when the release publishes no
    /// checksum. A signature is still verified when one is available.
    Checksum,
    /// Require a valid signature: the release must publish a minisign or
    /// Ed25519 signature for which a public key is configured. A checksum is
    /// still verified when available, but is not itself required.
    Signature,
    /// Require both a valid signature and a matching SHA-256.
    Strict,
}

impl Verification {
    fn skips_all(self) -> bool {
        matches!(self, Self::Off)
    }

    fn requires_signature(self) -> bool {
        matches!(self, Self::Signature | Self::Strict)
    }

    fn requires_checksum(self) -> bool {
        matches!(self, Self::Checksum | Self::Strict)
    }
}

/// Configuration for an [`UpdateEngine`].
///
/// Start from [`EngineConfig::new`] and chain the builder methods. The fields
/// stay public for reading and adjusting; the struct is `#[non_exhaustive]`
/// so new options can be added without a breaking release.
#[non_exhaustive]
pub struct EngineConfig {
    /// The currently running version, compared against the source's latest.
    pub current_version: Version,
    /// Base64 minisign public key. When set and the release publishes a
    /// `.minisig`, the download is signature-checked in addition to SHA-256.
    pub minisign_public_key: Option<String>,
    /// Base64 Ed25519 public key in Sparkle's format (the `SUPublicEDKey`
    /// value printed by Sparkle's `generate_keys`). When set and the release
    /// publishes an [`ed25519_signature`](Release::ed25519_signature), the
    /// download is signature-checked.
    pub ed25519_public_key: Option<String>,
    /// How strictly downloads are verified. See [`Verification`].
    pub verification: Verification,
    /// Where to install. Defaults to [`install::current_install_root`].
    pub install_root: Option<PathBuf>,
    /// Directory for downloads. Defaults to a fresh temp directory.
    pub download_dir: Option<PathBuf>,
}

impl EngineConfig {
    /// Start a config for the given running version.
    pub fn new(current_version: Version) -> Self {
        Self {
            current_version,
            minisign_public_key: None,
            ed25519_public_key: None,
            verification: Verification::default(),
            install_root: None,
            download_dir: None,
        }
    }

    /// Set the base64 minisign public key used to verify signed downloads.
    #[must_use]
    pub fn minisign_public_key(mut self, key: impl Into<String>) -> Self {
        self.minisign_public_key = Some(key.into());
        self
    }

    /// Set the base64 Ed25519 public key (Sparkle's `SUPublicEDKey`) used to
    /// verify `sparkle:edSignature` signatures.
    #[must_use]
    pub fn ed25519_public_key(mut self, key: impl Into<String>) -> Self {
        self.ed25519_public_key = Some(key.into());
        self
    }

    /// Set how strictly downloads are verified. Defaults to
    /// [`Verification::BestEffort`].
    #[must_use]
    pub fn verification(mut self, verification: Verification) -> Self {
        self.verification = verification;
        self
    }

    /// Fail closed when the policy requires a check the release *metadata*
    /// cannot satisfy (no public key, no advertised signature, no checksum).
    ///
    /// Run at check time so an obviously unverifiable release is not surfaced as
    /// available, and again before a download. This validates presence only: a
    /// release advertising a `signature_url` passes here even if that URL later
    /// fails to fetch or yields an invalid signature — those are caught when the
    /// download is verified.
    fn precheck(&self, release: &Release) -> Result<()> {
        if self.verification.requires_signature() && !self.can_verify_signature_of(release) {
            let reason = if self.minisign_public_key.is_none() && self.ed25519_public_key.is_none()
            {
                "policy requires a signature, but no minisign or Ed25519 public key is configured"
            } else {
                "policy requires a signature, but the release publishes none that a configured key can verify"
            };
            return Err(Error::VerificationRequired(reason.into()));
        }
        if self.verification.requires_checksum() && non_empty(release.sha256.as_deref()).is_none() {
            return Err(Error::VerificationRequired(
                "policy requires a SHA-256 checksum, but the release publishes none".into(),
            ));
        }
        Ok(())
    }

    /// Whether at least one signature the release advertises has a matching
    /// configured key.
    fn can_verify_signature_of(&self, release: &Release) -> bool {
        let minisign = self.minisign_public_key.is_some() && release_has_minisign(release);
        let ed25519 = self.ed25519_public_key.is_some()
            && non_empty(release.ed25519_signature.as_deref()).is_some();
        minisign || ed25519
    }

    /// Override the install location (defaults to the running app/bundle).
    #[must_use]
    pub fn install_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.install_root = Some(path.into());
        self
    }

    /// Override the download directory (defaults to a temp directory).
    #[must_use]
    pub fn download_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.download_dir = Some(path.into());
        self
    }
}

/// Blocking update orchestrator: check → download → verify → install.
///
/// All methods block; the GPUI integration runs them on a background executor.
pub struct UpdateEngine<S: UpdateSource> {
    source: S,
    config: EngineConfig,
}

impl<S: UpdateSource> UpdateEngine<S> {
    /// Build an engine from a source and configuration.
    pub fn new(source: S, config: EngineConfig) -> Self {
        Self { source, config }
    }

    /// The configured current version.
    pub fn current_version(&self) -> &Version {
        &self.config.current_version
    }

    /// Fetch the latest release, returning it only if it is newer than the
    /// current version (otherwise `Ok(None)`).
    ///
    /// # Errors
    /// Propagates source/network/parse errors.
    pub fn check(&self) -> Result<Option<Release>> {
        let latest = self.source.fetch_latest()?;
        if latest.version > self.config.current_version {
            self.config.precheck(&latest)?;
            Ok(Some(latest))
        } else {
            Ok(None)
        }
    }

    /// Download `release.asset`, then verify it according to the configured
    /// [`Verification`] policy. Returns the downloaded artifact path.
    ///
    /// `progress` is called as `(downloaded_bytes, total_bytes)`.
    ///
    /// # Errors
    /// Returns an error on download failure, if verification fails, or if the
    /// policy requires a check the release cannot satisfy
    /// ([`Error::VerificationRequired`]).
    pub fn download(
        &self,
        release: &Release,
        mut progress: impl FnMut(u64, Option<u64>),
    ) -> Result<PathBuf> {
        let dir = match &self.config.download_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                dir.clone()
            }
            None => default_download_dir()?,
        };
        let artifact = dir.join(&release.asset.name);
        http::download(&release.asset.url, &[], &artifact, &mut progress)?;
        self.verify_artifact(&artifact, release)?;
        Ok(artifact)
    }

    /// Verify a downloaded `artifact` against `release` under the configured
    /// policy. Fails closed first (so a policy-required-but-missing input is
    /// rejected before the artifact is read), then runs whatever checks the
    /// release does provide.
    fn verify_artifact(&self, artifact: &Path, release: &Release) -> Result<()> {
        let policy = self.config.verification;
        if policy.skips_all() {
            return Ok(());
        }
        // `download` is public and may be called without `check`, so re-run the
        // check-time gate here rather than trusting the caller went through it.
        self.config.precheck(release)?;

        if let Some(expected) = non_empty(release.sha256.as_deref()) {
            verify::verify_sha256(artifact, expected)?;
        }
        let mut verified_signature = false;
        if let (Some(key), Some(signature)) = (
            &self.config.ed25519_public_key,
            non_empty(release.ed25519_signature.as_deref()),
        ) {
            verify::verify_ed25519(artifact, key, signature)?;
            verified_signature = true;
        }
        if let Some(key) = &self.config.minisign_public_key {
            if let Some(signature) = resolve_minisign(release)? {
                verify::verify_minisign(artifact, key, &signature)?;
                verified_signature = true;
            }
        }
        // `precheck` may have accepted a non-empty `signature_url` whose fetched
        // body turned out blank. Fail closed instead of relying on a verifier to
        // reject an empty signature string.
        if policy.requires_signature() && !verified_signature {
            return Err(Error::VerificationRequired(
                "policy requires a signature, but the published signature is empty".into(),
            ));
        }
        Ok(())
    }

    /// Install a downloaded artifact over the configured install root.
    ///
    /// # Errors
    /// Propagates install failures.
    pub fn install(&self, artifact: &Path) -> Result<Installed> {
        let root = match &self.config.install_root {
            Some(root) => root.clone(),
            None => install::current_install_root()?,
        };
        install::install(artifact, &root)
    }

    /// Run the full pipeline. Returns `Ok(None)` when already up to date.
    ///
    /// # Errors
    /// Propagates any check/download/verify/install error.
    pub fn update_now(&self, progress: impl FnMut(u64, Option<u64>)) -> Result<Option<Installed>> {
        let Some(release) = self.check()? else {
            return Ok(None);
        };
        let artifact = self.download(&release, progress)?;
        Ok(Some(self.install(&artifact)?))
    }
}

fn default_download_dir() -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("gpui-updater-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Trim `s` and treat a blank string as absent.
fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Whether the release advertises a minisign signature, inline or by URL.
fn release_has_minisign(release: &Release) -> bool {
    non_empty(release.signature.as_deref()).is_some()
        || non_empty(release.signature_url.as_deref()).is_some()
}

/// The minisign signature for a release: the inline value if present, otherwise
/// fetched from `signature_url`. `None` when the release advertises neither, or
/// when a fetched signature body is blank.
fn resolve_minisign(release: &Release) -> Result<Option<String>> {
    if let Some(signature) = non_empty(release.signature.as_deref()) {
        return Ok(Some(signature.to_string()));
    }
    if let Some(url) = non_empty(release.signature_url.as_deref()) {
        let body = http::get_string(url, &[])?;
        return Ok(non_empty(Some(&body)).map(str::to_string));
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::release::Asset;

    fn release(
        sha256: Option<&str>,
        signature: Option<&str>,
        signature_url: Option<&str>,
    ) -> Release {
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
        let engine = UpdateEngine::new(
            PanicSource,
            config(Verification::Signature, None).ed25519_public_key(&fixture.public_key),
        );
        let signed = ed25519_release(&fixture.signature);
        assert!(engine.verify_artifact(&fixture.path, &signed).is_ok());

        std::fs::write(&fixture.path, b"update payloaD").unwrap();
        assert!(matches!(
            engine.verify_artifact(&fixture.path, &signed),
            Err(Error::Signature(_))
        ));
    }

    #[test]
    fn off_policy_skips_verification_entirely() {
        // `verify_artifact` returns Ok without touching the (nonexistent) file
        // even though the release claims a checksum.
        let engine = UpdateEngine::new(PanicSource, config(Verification::Off, None));
        let path = Path::new("/nonexistent/app.dmg");
        assert!(
            engine
                .verify_artifact(path, &release(Some("abc"), None, None))
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
                    .verify_artifact(path, &release(Some(blank), None, None))
                    .is_ok(),
                "blank sha256 {blank:?} should be treated as absent"
            );
        }
    }

    /// A source whose `fetch_latest` must never be called: the policy tests
    /// exercise verification, not fetching.
    struct PanicSource;

    impl UpdateSource for PanicSource {
        fn fetch_latest(&self) -> Result<Release> {
            unreachable!("policy tests do not fetch")
        }
    }
}
