//! Engine configuration and the verification of one downloaded file.

use std::path::{Path, PathBuf};

use semver::Version;

use crate::error::{Error, Result};
use crate::release::{DeltaAsset, Release};
use crate::{http, verify};

/// Longest chain of deltas the engine downloads by default.
pub const DEFAULT_MAX_DELTA_HOPS: usize = 4;

/// How strictly [`UpdateEngine`](crate::UpdateEngine) enforces integrity and
/// authenticity checks on a release.
///
/// A "signature" is either a minisign signature (checked against
/// [`EngineConfig::minisign_public_key`]) or a Sparkle-style Ed25519
/// signature (checked against [`EngineConfig::ed25519_public_key`]). Every
/// signature the release publishes *and* a configured key can check is
/// verified, and any mismatch fails the download. The same applies to deltas.
///
/// The default, [`BestEffort`](Verification::BestEffort), verifies whatever the
/// release provides and *skips* a check when its input is absent — so an
/// unsigned release installs with only a checksum, or with nothing. The
/// `Signature`/`Strict` modes instead fail closed: a release that cannot be
/// verified is rejected at [`check`](crate::UpdateEngine::check) time (before
/// it is surfaced as available) and again at
/// [`download`](crate::UpdateEngine::download) time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Verification {
    /// Verify whatever the release and config provide; skip a check when its
    /// input is missing (no checksum, no signature, or no public key). The
    /// backwards-compatible default — note it fails *open*.
    #[default]
    BestEffort,
    /// Skip all integrity, authenticity, and payload checks. For tests and
    /// local development only.
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
    pub(super) fn skips_all(self) -> bool {
        matches!(self, Self::Off)
    }

    fn requires_signature(self) -> bool {
        matches!(self, Self::Signature | Self::Strict)
    }

    fn requires_checksum(self) -> bool {
        matches!(self, Self::Checksum | Self::Strict)
    }
}

/// Whether a signature must name the version it was made for.
///
/// minisign signs a *trusted comment* along with the file. When it carries a
/// `version:<semver>` token (see [`verify::signed_version`]), the engine
/// checks it against the version the file is supposed to be: the release
/// version for a full artifact, the target version for a delta. That stops an
/// older, validly signed artifact from being served as a newer one.
///
/// Sparkle-style Ed25519 signatures sign the file alone and cannot name a
/// version; on macOS, [`BundleVersion`] covers them instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum VersionBinding {
    /// Check the `version:` token when a verified signature carries one.
    #[default]
    IfPresent,
    /// Reject a verified signature without a `version:` token. Use once every
    /// artifact you publish is signed with one.
    Required,
}

/// Which code signature the app bundle of an update must carry (macOS).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum CodeSigning {
    /// When the installed app is signed by a team, require the update to pass
    /// `codesign --verify --deep --strict` and be signed by the same team.
    /// Unsigned and ad-hoc signed installations are not checked.
    #[default]
    MatchInstalled,
    /// Require a valid signature by this Team ID, whatever the installed app
    /// carries.
    TeamId(String),
    /// Do not check code signatures.
    Off,
}

/// Whether the app bundle of an update must declare the release's version
/// (macOS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum BundleVersion {
    /// Require `CFBundleShortVersionString` to equal the release version
    /// (missing components count as zero, so `1.2` matches `1.2.0`).
    #[default]
    MustMatch,
    /// Do not compare bundle versions.
    Ignore,
}

/// Configuration for an [`UpdateEngine`](crate::UpdateEngine).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EngineConfig {
    /// The currently running version, compared against the source's latest.
    pub current_version: Version,
    /// Base64 minisign public keys. A signature made by any of them is
    /// accepted, so a new key can ship next to the old one before releases
    /// switch to it.
    pub minisign_public_keys: Vec<String>,
    /// Base64 Ed25519 public key in Sparkle's format (the `SUPublicEDKey`
    /// value printed by Sparkle's `generate_keys`). When set and the release
    /// publishes an [`ed25519_signature`](Release::ed25519_signature), the
    /// download is signature-checked.
    pub ed25519_public_key: Option<String>,
    /// How strictly downloads are verified. See [`Verification`].
    pub verification: Verification,
    /// Whether signatures must name the version they were made for.
    pub version_binding: VersionBinding,
    /// Which code signature the update must carry (macOS).
    pub code_signing: CodeSigning,
    /// Whether the update's bundle version must equal the release (macOS).
    pub bundle_version: BundleVersion,
    /// Where to install. Defaults to [`crate::install::current_install_root`].
    pub install_root: Option<PathBuf>,
    /// Directory for downloads. Defaults to a fresh temp directory.
    pub download_dir: Option<PathBuf>,
    /// Whether to try published binary deltas before the full artifact.
    /// Defaults to `true`; only takes effect with the `delta` feature on macOS
    /// and Linux.
    pub deltas: bool,
    /// Longest chain of deltas to download when none starts at the installed
    /// version. Defaults to [`DEFAULT_MAX_DELTA_HOPS`].
    pub max_delta_hops: usize,
}

impl EngineConfig {
    /// Start a config for the given running version.
    pub fn new(current_version: Version) -> Self {
        Self {
            current_version,
            minisign_public_keys: Vec::new(),
            ed25519_public_key: None,
            verification: Verification::default(),
            version_binding: VersionBinding::default(),
            code_signing: CodeSigning::default(),
            bundle_version: BundleVersion::default(),
            install_root: None,
            download_dir: None,
            deltas: true,
            max_delta_hops: DEFAULT_MAX_DELTA_HOPS,
        }
    }

    /// Trust a base64 minisign public key. Call again to trust several, for
    /// example while rotating keys.
    #[must_use]
    pub fn minisign_public_key(mut self, key: impl Into<String>) -> Self {
        self.minisign_public_keys.push(key.into());
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

    /// Set whether signatures must name their version. Defaults to
    /// [`VersionBinding::IfPresent`].
    #[must_use]
    pub fn version_binding(mut self, binding: VersionBinding) -> Self {
        self.version_binding = binding;
        self
    }

    /// Set which code signature updates must carry. Defaults to
    /// [`CodeSigning::MatchInstalled`].
    #[must_use]
    pub fn code_signing(mut self, code_signing: CodeSigning) -> Self {
        self.code_signing = code_signing;
        self
    }

    /// Set whether bundle versions are compared. Defaults to
    /// [`BundleVersion::MustMatch`].
    #[must_use]
    pub fn bundle_version(mut self, bundle_version: BundleVersion) -> Self {
        self.bundle_version = bundle_version;
        self
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

    /// Enable or disable trying binary deltas before the full artifact.
    /// Defaults to `true`. Has no effect without the `delta` feature.
    #[must_use]
    pub fn deltas(mut self, enabled: bool) -> Self {
        self.deltas = enabled;
        self
    }

    /// Set the longest chain of deltas to download. `1` only uses a delta
    /// from the installed version itself.
    #[must_use]
    pub fn max_delta_hops(mut self, hops: usize) -> Self {
        self.max_delta_hops = hops;
        self
    }

    /// Fail closed when the policy requires a check the release *metadata*
    /// cannot satisfy (no public key, no advertised signature, no checksum).
    ///
    /// Run at check time so an obviously unverifiable release is not surfaced
    /// as available, and again before a download. This validates presence
    /// only: a release advertising a `signature_url` passes here even if that
    /// URL later fails to fetch or yields an invalid signature — those are
    /// caught when the download is verified.
    pub(super) fn precheck(&self, release: &Release) -> Result<()> {
        self.precheck_integrity(Integrity::of_release(release))
    }

    /// [`precheck`](Self::precheck) for any downloadable file's metadata.
    pub(super) fn precheck_integrity(&self, file: Integrity<'_>) -> Result<()> {
        let what = file.what;
        if self.verification.requires_signature() && !self.can_verify_signature_of(file) {
            if self.minisign_public_keys.is_empty() && self.ed25519_public_key.is_none() {
                return Err(Error::VerificationRequired(
                    "policy requires a signature, but no minisign or Ed25519 public key is configured"
                        .into(),
                ));
            }
            return Err(Error::VerificationRequired(format!(
                "policy requires a signature, but the {what} publishes none that a configured key can verify"
            )));
        }
        if self.verification.requires_checksum() && !file.has_checksum() {
            return Err(Error::VerificationRequired(format!(
                "policy requires a SHA-256 checksum, but the {what} publishes none"
            )));
        }
        Ok(())
    }

    /// Whether at least one signature the file advertises has a matching
    /// configured key.
    fn can_verify_signature_of(&self, file: Integrity<'_>) -> bool {
        let minisign = !self.minisign_public_keys.is_empty() && file.has_minisign();
        let ed25519 =
            self.ed25519_public_key.is_some() && non_empty(file.ed25519_signature).is_some();
        minisign || ed25519
    }

    /// Verify a downloaded file under the configured policy. `version` is
    /// the version the file must be signed for. Fails closed first (so a
    /// policy-required-but-missing input is rejected before the file is read),
    /// then runs whatever checks the metadata does provide.
    pub(super) fn verify_file(
        &self,
        path: &Path,
        file: Integrity<'_>,
        version: &Version,
    ) -> Result<()> {
        let policy = self.verification;
        if policy.skips_all() {
            return Ok(());
        }
        // `download` is public and may be called without `check`, so re-run the
        // check-time gate here rather than trusting the caller went through it.
        self.precheck_integrity(file)?;

        match resolve_checksum(file)? {
            Some(expected) => verify::verify_sha256(path, &expected)?,
            None if policy.requires_checksum() => {
                return Err(Error::VerificationRequired(format!(
                    "policy requires a SHA-256 checksum, but the published checksums list no `{}`",
                    file.name
                )));
            }
            None => {}
        }
        let mut verified_signature = false;
        if let (Some(key), Some(signature)) =
            (&self.ed25519_public_key, non_empty(file.ed25519_signature))
        {
            verify::verify_ed25519(path, key, signature)?;
            verified_signature = true;
        }
        if !self.minisign_public_keys.is_empty() {
            if let Some(signature) = resolve_minisign(file)? {
                let keys: Vec<&str> = self
                    .minisign_public_keys
                    .iter()
                    .map(String::as_str)
                    .collect();
                let comment =
                    verify::verify_minisign_bytes(&std::fs::read(path)?, &keys, &signature)?;
                self.check_signed_version(&comment, version)?;
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

    fn check_signed_version(&self, trusted_comment: &str, expected: &Version) -> Result<()> {
        match verify::signed_version(trusted_comment) {
            Some(signed) => {
                let matches = crate::release::parse_tag(signed)
                    .is_ok_and(|v| v.cmp_precedence(expected).is_eq());
                if matches {
                    Ok(())
                } else {
                    Err(Error::Signature(format!(
                        "file is signed for version {signed}, expected {expected}"
                    )))
                }
            }
            None if self.version_binding == VersionBinding::Required => Err(Error::Signature(
                "the signature's trusted comment names no `version:`".into(),
            )),
            None => Ok(()),
        }
    }
}

/// The integrity inputs published for one downloadable file.
#[derive(Clone, Copy)]
pub(super) struct Integrity<'a> {
    /// What the file is, for error messages (`release` / `delta`).
    what: &'static str,
    /// The file's published name, looked up in a checksums file.
    name: &'a str,
    sha256: Option<&'a str>,
    checksums_url: Option<&'a str>,
    signature: Option<&'a str>,
    signature_url: Option<&'a str>,
    ed25519_signature: Option<&'a str>,
}

impl<'a> Integrity<'a> {
    pub(super) fn of_release(release: &'a Release) -> Self {
        Self {
            what: "release",
            name: &release.asset.name,
            sha256: release.sha256.as_deref(),
            checksums_url: None,
            signature: release.signature.as_deref(),
            signature_url: release.signature_url.as_deref(),
            ed25519_signature: release.ed25519_signature.as_deref(),
        }
    }

    #[cfg_attr(
        all(not(feature = "delta"), not(test)),
        expect(dead_code, reason = "only deltas are verified this way")
    )]
    pub(super) fn of_delta(delta: &'a DeltaAsset) -> Self {
        Self {
            what: "delta",
            name: &delta.asset.name,
            sha256: delta.sha256.as_deref(),
            checksums_url: delta.checksums_url.as_deref(),
            signature: delta.signature.as_deref(),
            signature_url: delta.signature_url.as_deref(),
            ed25519_signature: delta.ed25519_signature.as_deref(),
        }
    }

    /// Whether a minisign signature is advertised, inline or by URL.
    fn has_minisign(self) -> bool {
        non_empty(self.signature).is_some() || non_empty(self.signature_url).is_some()
    }

    /// Whether a checksum is advertised, inline or in a checksums file.
    fn has_checksum(self) -> bool {
        non_empty(self.sha256).is_some() || non_empty(self.checksums_url).is_some()
    }
}

/// Trim `s` and treat a blank string as absent.
fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// The expected SHA-256 of a file: the inline value if present, otherwise
/// looked up in the checksums file at `checksums_url`.
fn resolve_checksum(file: Integrity<'_>) -> Result<Option<String>> {
    if let Some(sha256) = non_empty(file.sha256) {
        return Ok(Some(sha256.to_string()));
    }
    match non_empty(file.checksums_url) {
        Some(url) => Ok(verify::sha256_from_sums(
            &http::get_string(url, &[])?,
            file.name,
        )),
        None => Ok(None),
    }
}

/// The minisign signature for a file: the inline value if present, otherwise
/// fetched from `signature_url`. `None` when the release advertises neither,
/// or when a fetched signature body is blank.
fn resolve_minisign(file: Integrity<'_>) -> Result<Option<String>> {
    if let Some(signature) = non_empty(file.signature) {
        return Ok(Some(signature.to_string()));
    }
    if let Some(url) = non_empty(file.signature_url) {
        let body = http::get_string(url, &[])?;
        return Ok(non_empty(Some(&body)).map(str::to_string));
    }
    Ok(None)
}
