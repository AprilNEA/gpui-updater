//! The blocking update orchestrator that ties the pieces together.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use semver::Version;

use crate::error::{Error, Result};
use crate::install::{self, Installed};
#[cfg(feature = "delta")]
use crate::release::DeltaAsset;
use crate::release::Release;
use crate::source::UpdateSource;
use crate::{http, verify};

/// How strictly [`UpdateEngine`] enforces integrity and authenticity checks on
/// a release.
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
    /// Require a valid minisign signature: a public key must be configured and
    /// the release must publish a signature. A checksum is still verified when
    /// available, but is not itself required.
    Signature,
    /// Require both a valid minisign signature and a matching SHA-256.
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
pub struct EngineConfig {
    /// The currently running version, compared against the source's latest.
    pub current_version: Version,
    /// Base64 minisign public key. When set and the release publishes a
    /// `.minisig`, the download is signature-checked in addition to SHA-256.
    pub minisign_public_key: Option<String>,
    /// How strictly downloads are verified. See [`Verification`].
    pub verification: Verification,
    /// Where to install. Defaults to [`install::current_install_root`].
    pub install_root: Option<PathBuf>,
    /// Directory for downloads. Defaults to a fresh temp directory.
    pub download_dir: Option<PathBuf>,
    /// Whether to try a published binary delta before the full artifact.
    /// Defaults to `true`; only takes effect with the `delta` feature on macOS
    /// and Linux.
    pub deltas: bool,
}

impl EngineConfig {
    /// Start a config for the given running version.
    pub fn new(current_version: Version) -> Self {
        Self {
            current_version,
            minisign_public_key: None,
            verification: Verification::default(),
            install_root: None,
            download_dir: None,
            deltas: true,
        }
    }

    /// Set the base64 minisign public key used to verify signed downloads.
    #[must_use]
    pub fn minisign_public_key(mut self, key: impl Into<String>) -> Self {
        self.minisign_public_key = Some(key.into());
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
        self.precheck_integrity(Integrity::of_release(release))
    }

    /// [`precheck`](Self::precheck) for any downloadable file's metadata.
    fn precheck_integrity(&self, file: Integrity<'_>) -> Result<()> {
        let what = file.what;
        if self.verification.requires_signature() {
            if self.minisign_public_key.is_none() {
                return Err(Error::VerificationRequired(
                    "policy requires a minisign signature, but no public key is configured".into(),
                ));
            }
            if !file.has_signature() {
                return Err(Error::VerificationRequired(format!(
                    "policy requires a minisign signature, but the {what} publishes none"
                )));
            }
        }
        if self.verification.requires_checksum() && non_empty(file.sha256).is_none() {
            return Err(Error::VerificationRequired(format!(
                "policy requires a SHA-256 checksum, but the {what} publishes none"
            )));
        }
        Ok(())
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
}

/// The integrity inputs published for one downloadable file.
#[derive(Clone, Copy)]
struct Integrity<'a> {
    /// What the file is, for error messages (`release` / `delta`).
    what: &'static str,
    sha256: Option<&'a str>,
    signature: Option<&'a str>,
    signature_url: Option<&'a str>,
}

impl<'a> Integrity<'a> {
    fn of_release(release: &'a Release) -> Self {
        Self {
            what: "release",
            sha256: release.sha256.as_deref(),
            signature: release.signature.as_deref(),
            signature_url: release.signature_url.as_deref(),
        }
    }

    #[cfg(feature = "delta")]
    fn of_delta(delta: &'a DeltaAsset) -> Self {
        Self {
            what: "delta",
            sha256: delta.sha256.as_deref(),
            signature: delta.signature.as_deref(),
            signature_url: delta.signature_url.as_deref(),
        }
    }

    /// Whether a minisign signature is advertised, inline or by URL.
    fn has_signature(self) -> bool {
        non_empty(self.signature).is_some() || non_empty(self.signature_url).is_some()
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

    /// Download the update and verify it according to the configured
    /// [`Verification`] policy. Returns the path to hand to
    /// [`install`](Self::install).
    ///
    /// With the `delta` feature (macOS and Linux), a published delta from the
    /// current version is tried first: it is downloaded, verified under the same
    /// policy as a full artifact, and applied to the installed files, which are
    /// themselves checked against the hashes recorded in the delta. The result
    /// is a directory holding the rebuilt bundle or executable. If anything in
    /// that path fails — no matching delta, a modified installation, a bad
    /// checksum or signature, a network error — the full `release.asset` is
    /// downloaded instead, so a delta can never make an update fail.
    ///
    /// `progress` is called as `(downloaded_bytes, total_bytes)`. After a failed
    /// delta attempt it restarts from zero for the full download.
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
        let dir = self.download_dir()?;
        #[cfg(feature = "delta")]
        if let Some(rebuilt) = self.try_delta(release, &dir, &mut progress) {
            return Ok(rebuilt);
        }
        let artifact = dir.join(&release.asset.name);
        http::download(&release.asset.url, &[], &artifact, &mut progress)?;
        self.verify_file(&artifact, Integrity::of_release(release))?;
        Ok(artifact)
    }

    fn download_dir(&self) -> Result<PathBuf> {
        match &self.config.download_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                Ok(dir.clone())
            }
            None => default_download_dir(),
        }
    }

    fn install_root(&self) -> Result<PathBuf> {
        match &self.config.install_root {
            Some(root) => Ok(root.clone()),
            None => install::current_install_root(),
        }
    }

    /// Try to rebuild the update from a delta. `None` means "use the full
    /// download": there is no applicable delta, or applying it failed (logged).
    #[cfg(feature = "delta")]
    fn try_delta(
        &self,
        release: &Release,
        dir: &Path,
        progress: &mut impl FnMut(u64, Option<u64>),
    ) -> Option<PathBuf> {
        if !self.config.deltas || !cfg!(any(target_os = "macos", target_os = "linux")) {
            return None;
        }
        let current = &self.config.current_version;
        let delta = release
            .deltas
            .iter()
            .find(|d| &d.from == current && d.format == crate::delta::FORMAT)?;
        match self.apply_delta(release, delta, dir, progress) {
            Ok(rebuilt) => {
                tracing::info!(from = %current, to = %release.version, "rebuilt update from delta");
                Some(rebuilt)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    from = %current,
                    to = %release.version,
                    "delta update failed; falling back to the full download"
                );
                None
            }
        }
    }

    #[cfg(feature = "delta")]
    fn apply_delta(
        &self,
        release: &Release,
        delta: &DeltaAsset,
        dir: &Path,
        progress: &mut impl FnMut(u64, Option<u64>),
    ) -> Result<PathBuf> {
        // Skip the delta outright when the policy could not accept it (or the
        // release), rather than spending a download on it.
        if !self.config.verification.skips_all() {
            self.config.precheck(release)?;
            self.config.precheck_integrity(Integrity::of_delta(delta))?;
        }
        let base = self.install_root()?;
        let name = base
            .file_name()
            .ok_or_else(|| Error::Delta("install root has no file name".into()))?
            .to_os_string();

        let file = dir.join(safe_file_name(&delta.asset.name, "update.delta"));
        let result = (|| {
            http::download(&delta.asset.url, &[], &file, &mut *progress)?;
            self.verify_file(&file, Integrity::of_delta(delta))?;
            // The header must describe exactly this upgrade, so a validly signed
            // delta for another version pair cannot be replayed.
            let info = crate::delta::read_info(&file)?;
            if info.from != self.config.current_version || info.to != release.version {
                return Err(Error::Delta(format!(
                    "delta rebuilds {} -> {}, expected {} -> {}",
                    info.from, info.to, self.config.current_version, release.version
                )));
            }
            let out_dir = dir.join(format!("delta-{}", release.version));
            let _ = std::fs::remove_dir_all(&out_dir);
            std::fs::create_dir_all(&out_dir)?;
            if let Err(e) = crate::delta::apply(&file, &base, &out_dir.join(&name)) {
                let _ = std::fs::remove_dir_all(&out_dir);
                return Err(e);
            }
            Ok(out_dir)
        })();
        let _ = std::fs::remove_file(&file);
        result
    }

    /// Verify a downloaded file under the configured policy. Fails closed first
    /// (so a policy-required-but-missing input is rejected before the file is
    /// read), then runs whatever checks the metadata does provide.
    fn verify_file(&self, path: &Path, file: Integrity<'_>) -> Result<()> {
        let policy = self.config.verification;
        if policy.skips_all() {
            return Ok(());
        }
        // `download` is public and may be called without `check`, so re-run the
        // check-time gate here rather than trusting the caller went through it.
        self.config.precheck_integrity(file)?;

        if let Some(expected) = non_empty(file.sha256) {
            verify::verify_sha256(path, expected)?;
        }
        if let Some(key) = &self.config.minisign_public_key {
            match resolve_signature(file)? {
                Some(signature) => verify::verify_minisign(path, key, &signature)?,
                // `precheck` already accepted a non-empty `signature_url`, but the
                // fetched body turned out blank. Fail closed instead of relying on
                // the verifier to reject an empty signature string.
                None if policy.requires_signature() => {
                    return Err(Error::VerificationRequired(
                        "policy requires a minisign signature, but the published signature is empty"
                            .into(),
                    ));
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Install a downloaded artifact over the configured install root.
    ///
    /// # Errors
    /// Propagates install failures.
    pub fn install(&self, artifact: &Path) -> Result<Installed> {
        install::install(artifact, &self.install_root()?)
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

/// The last path component of a published file name, so a hostile name cannot
/// escape the download directory.
#[cfg(feature = "delta")]
fn safe_file_name<'a>(name: &'a str, fallback: &'a str) -> &'a str {
    match name.rsplit(['/', '\\']).next() {
        Some(base) if !base.is_empty() && base != "." && base != ".." => base,
        _ => fallback,
    }
}

/// The minisign signature for a file: the inline value if present, otherwise
/// fetched from `signature_url`. `None` when the release advertises neither, or
/// when a fetched signature body is blank.
fn resolve_signature(file: Integrity<'_>) -> Result<Option<String>> {
    if let Some(signature) = non_empty(file.signature) {
        return Ok(Some(signature.to_string()));
    }
    if let Some(url) = non_empty(file.signature_url) {
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
                .verify_file(
                    path,
                    Integrity::of_release(&release(Some("abc"), None, None))
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
                    .verify_file(
                        path,
                        Integrity::of_release(&release(Some(blank), None, None))
                    )
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

/// End-to-end delta downloads against a local HTTP server.
#[cfg(all(test, feature = "delta", any(target_os = "macos", target_os = "linux")))]
mod delta_tests {
    use super::*;
    use crate::release::{Asset, DELTA_FORMAT_FILES_BSDIFF_V1, DeltaAsset};
    use std::collections::HashMap;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    struct NoSource;

    impl UpdateSource for NoSource {
        fn fetch_latest(&self) -> Result<Release> {
            unreachable!("delta tests call download directly")
        }
    }

    /// Serves fixed bodies by path until the test process exits and records
    /// every requested path.
    struct Server {
        base: String,
        hits: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        fn start(files: HashMap<&'static str, Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let hits = Arc::new(Mutex::new(Vec::new()));
            let log = hits.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request = String::new();
                    let _ = reader.read_line(&mut request);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                    }
                    let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                    log.lock().unwrap().push(path.clone());
                    let response = match files.get(path.as_str()) {
                        Some(body) => {
                            let mut r = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            r.extend_from_slice(body);
                            r
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = stream.write_all(&response);
                }
            });
            Self { base, hits }
        }

        fn hits(&self) -> Vec<String> {
            self.hits.lock().unwrap().clone()
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        installed: PathBuf,
        new_app: PathBuf,
        server: Server,
        delta_sha: String,
        full_sha: String,
    }

    const FULL: &[u8] = b"pretend this is a disk image";

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("Applications/Demo.app");
        let new_app = dir.path().join("build/Demo.app");
        let binary: Vec<u8> = (0..50_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut changed = binary.clone();
        changed[1234] ^= 0xff;
        for (root, bin, plist) in [
            (&installed, &binary, "1.0.0"),
            (&new_app, &changed, "1.1.0"),
        ] {
            fs::create_dir_all(root.join("Contents/MacOS")).unwrap();
            fs::write(root.join("Contents/MacOS/demo"), bin).unwrap();
            fs::write(root.join("Contents/Info.plist"), plist).unwrap();
        }
        let delta_path = dir.path().join("made.delta");
        crate::delta::create(
            &installed,
            &new_app,
            &Version::new(1, 0, 0),
            &Version::new(1, 1, 0),
            &delta_path,
        )
        .unwrap();
        let delta = fs::read(&delta_path).unwrap();
        let delta_sha = verify::sha256_hex(&delta_path).unwrap();
        let full_path = dir.path().join("full");
        fs::write(&full_path, FULL).unwrap();
        let full_sha = verify::sha256_hex(&full_path).unwrap();
        let server = Server::start(HashMap::from([("/delta", delta), ("/full", FULL.to_vec())]));
        Fixture {
            dir,
            installed,
            new_app,
            server,
            delta_sha,
            full_sha,
        }
    }

    impl Fixture {
        fn release(&self, version: Version, delta_sha: Option<&str>) -> Release {
            Release {
                version,
                notes: None,
                asset: Asset {
                    name: "Demo.dmg".into(),
                    url: format!("{}/full", self.server.base),
                    size: FULL.len() as u64,
                },
                signature: None,
                signature_url: None,
                sha256: Some(self.full_sha.clone()),
                deltas: vec![DeltaAsset {
                    from: Version::new(1, 0, 0),
                    format: DELTA_FORMAT_FILES_BSDIFF_V1.into(),
                    asset: Asset {
                        name: "../Demo.dmg.from-1.0.0.delta".into(),
                        url: format!("{}/delta", self.server.base),
                        size: 0,
                    },
                    sha256: delta_sha.map(str::to_string),
                    signature: None,
                    signature_url: None,
                }],
            }
        }

        fn engine(&self, verification: Verification, deltas: bool) -> UpdateEngine<NoSource> {
            let config = EngineConfig::new(Version::new(1, 0, 0))
                .verification(verification)
                .install_root(&self.installed)
                .download_dir(self.dir.path().join("downloads"))
                .deltas(deltas);
            UpdateEngine::new(NoSource, config)
        }

        fn downloads(&self) -> PathBuf {
            self.dir.path().join("downloads")
        }
    }

    fn assert_same_tree(a: &Path, b: &Path) {
        for rel in ["Contents/MacOS/demo", "Contents/Info.plist"] {
            assert_eq!(
                fs::read(a.join(rel)).unwrap(),
                fs::read(b.join(rel)).unwrap(),
                "{rel}"
            );
        }
    }

    #[test]
    fn rebuilds_from_a_matching_delta() {
        let fx = fixture();
        let release = fx.release(Version::new(1, 1, 0), Some(&fx.delta_sha));
        let mut totals = Vec::new();
        let out = fx
            .engine(Verification::BestEffort, true)
            .download(&release, |_, total| totals.push(total))
            .unwrap();
        assert_eq!(out, fx.downloads().join("delta-1.1.0"));
        assert_same_tree(&out.join("Demo.app"), &fx.new_app);
        assert_eq!(fx.server.hits(), vec!["/delta"]);
        assert!(totals.iter().all(Option::is_some));
        // The downloaded delta is removed, and its hostile name stayed inside
        // the download directory.
        assert!(!fx.downloads().join("Demo.dmg.from-1.0.0.delta").exists());
        assert!(!fx.dir.path().join("Demo.dmg.from-1.0.0.delta").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn installs_the_rebuilt_bundle() {
        let fx = fixture();
        let engine = fx.engine(Verification::Checksum, true);
        let release = fx.release(Version::new(1, 1, 0), Some(&fx.delta_sha));
        let out = engine.download(&release, |_, _| {}).unwrap();
        let installed = engine.install(&out).unwrap();
        assert_eq!(installed.restart_path, Some(fx.installed.clone()));
        assert_same_tree(&fx.installed, &fx.new_app);
    }

    #[test]
    fn modified_installation_falls_back_to_the_full_download() {
        let fx = fixture();
        fs::write(
            fx.installed.join("Contents/Info.plist"),
            "edited by the user",
        )
        .unwrap();
        let release = fx.release(Version::new(1, 1, 0), Some(&fx.delta_sha));
        let out = fx
            .engine(Verification::BestEffort, true)
            .download(&release, |_, _| {})
            .unwrap();
        assert_eq!(out, fx.downloads().join("Demo.dmg"));
        assert_eq!(fs::read(&out).unwrap(), FULL);
        assert_eq!(fx.server.hits(), vec!["/delta", "/full"]);
        assert!(!fx.downloads().join("delta-1.1.0").exists());
    }

    #[test]
    fn checksum_policy_governs_the_delta_too() {
        // No published delta checksum: the delta is skipped without a download.
        let fx = fixture();
        let release = fx.release(Version::new(1, 1, 0), None);
        let out = fx
            .engine(Verification::Checksum, true)
            .download(&release, |_, _| {})
            .unwrap();
        assert_eq!(out, fx.downloads().join("Demo.dmg"));
        assert_eq!(fx.server.hits(), vec!["/full"]);

        // A wrong checksum: downloaded, rejected, then the full download.
        let fx = fixture();
        let release = fx.release(Version::new(1, 1, 0), Some(&"0".repeat(64)));
        let out = fx
            .engine(Verification::Checksum, true)
            .download(&release, |_, _| {})
            .unwrap();
        assert_eq!(out, fx.downloads().join("Demo.dmg"));
        assert_eq!(fx.server.hits(), vec!["/delta", "/full"]);

        // The right checksum: the delta is used.
        let fx = fixture();
        let release = fx.release(Version::new(1, 1, 0), Some(&fx.delta_sha));
        let out = fx
            .engine(Verification::Checksum, true)
            .download(&release, |_, _| {})
            .unwrap();
        assert_eq!(out, fx.downloads().join("delta-1.1.0"));
    }

    #[test]
    fn delta_for_another_version_pair_is_not_applied() {
        // The release is 1.2.0, but the listed delta's header says 1.0.0 -> 1.1.0.
        let fx = fixture();
        let release = fx.release(Version::new(1, 2, 0), Some(&fx.delta_sha));
        let out = fx
            .engine(Verification::BestEffort, true)
            .download(&release, |_, _| {})
            .unwrap();
        assert_eq!(out, fx.downloads().join("Demo.dmg"));
        assert_eq!(fx.server.hits(), vec!["/delta", "/full"]);
    }

    #[test]
    fn disabled_deltas_are_never_fetched() {
        let fx = fixture();
        let release = fx.release(Version::new(1, 1, 0), Some(&fx.delta_sha));
        let out = fx
            .engine(Verification::BestEffort, false)
            .download(&release, |_, _| {})
            .unwrap();
        assert_eq!(out, fx.downloads().join("Demo.dmg"));
        assert_eq!(fx.server.hits(), vec!["/full"]);
    }
}
