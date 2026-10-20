//! End-to-end delta updates of a bare `.exe` on Windows against a local HTTP
//! server: the rebuilt executable is checked and installed like the full one.

#![allow(clippy::unwrap_used, reason = "tests")]

use std::collections::HashMap;
use std::fs;

use super::test_server::Server;
use super::*;
use crate::delta::VersionPair;
use crate::error::Error;
use crate::release::{Asset, DELTA_FORMAT_FILES_BSDIFF_V1, DeltaAsset};
use crate::verify;

struct NoSource;

impl UpdateSource for NoSource {
    fn fetch_latest(&self) -> Result<Release> {
        unreachable!("delta tests call download directly")
    }
}

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// Stand-in executable bytes. They are not a real program, so a test that
/// checks signatures installs a signed system program instead.
fn program(edits: usize) -> Vec<u8> {
    let mut bytes: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    for k in 0..edits {
        bytes[1234 + k * 9000] ^= 0xff;
    }
    bytes
}

/// A program Windows ships signed (by catalog). A copy stays signed.
fn system_program(name: &str) -> Vec<u8> {
    let root =
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    fs::read(root.join("System32").join(name)).unwrap()
}

struct Fixture {
    dir: tempfile::TempDir,
    installed: PathBuf,
    old: Vec<u8>,
    new: Vec<u8>,
    server: Server,
    /// (sha256, size) of the published delta.
    delta: (String, u64),
    /// (sha256, size) of the full `.exe`.
    full: (String, u64),
}

/// `old` installed as `App.exe`, and a published `1.0.0 -> 1.1.0` delta that
/// rebuilds `new` from it.
fn fixture(old: Vec<u8>, new: Vec<u8>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let installed = dir.path().join(r"Programs\App\App.exe");
    fs::create_dir_all(installed.parent().unwrap()).unwrap();
    fs::write(&installed, &old).unwrap();
    let build = dir.path().join(r"build\App.exe");
    fs::create_dir_all(build.parent().unwrap()).unwrap();
    fs::write(&build, &new).unwrap();
    let path = dir.path().join("App.exe.from-1.0.0.delta");
    crate::delta::create(
        &installed,
        &build,
        &VersionPair::new(v("1.0.0"), v("1.1.0")),
        &path,
    )
    .unwrap();
    let meta = |p: &Path| {
        (
            verify::sha256_hex(p).unwrap(),
            fs::metadata(p).unwrap().len(),
        )
    };
    let (delta, full) = (meta(&path), meta(&build));
    let server = Server::start(HashMap::from([
        ("/full".to_string(), new.clone()),
        ("/delta".to_string(), fs::read(&path).unwrap()),
    ]));
    Fixture {
        dir,
        installed,
        old,
        new,
        server,
        delta,
        full,
    }
}

impl Fixture {
    /// Release 1.1.0 with its full asset named `asset_name`, and the delta.
    fn release(&self, asset_name: &str) -> Release {
        let (sha256, size) = self.delta.clone();
        Release::new(
            v("1.1.0"),
            Asset::new(
                asset_name,
                format!("{}/full", self.server.base),
                self.full.1,
            ),
        )
        .with_sha256(self.full.0.clone())
        .with_deltas(vec![DeltaAsset {
            from: v("1.0.0"),
            to: v("1.1.0"),
            format: DELTA_FORMAT_FILES_BSDIFF_V1.into(),
            asset: Asset {
                name: format!("{asset_name}.from-1.0.0.delta"),
                url: format!("{}/delta", self.server.base),
                size,
            },
            sha256: Some(sha256),
            checksums_url: None,
            signature: None,
            signature_url: None,
            ed25519_signature: None,
        }])
    }

    fn engine(&self, code_signing: CodeSigning) -> UpdateEngine<NoSource> {
        self.engine_at(&self.installed, code_signing)
    }

    fn engine_at(&self, root: &Path, code_signing: CodeSigning) -> UpdateEngine<NoSource> {
        UpdateEngine::new(
            NoSource,
            EngineConfig::new(v("1.0.0"))
                .verification(Verification::Checksum)
                .code_signing(code_signing)
                .install_root(root)
                .download_dir(self.downloads()),
        )
    }

    fn downloads(&self) -> PathBuf {
        self.dir.path().join("downloads")
    }
}

#[test]
fn rebuilds_and_installs_a_bare_exe() {
    let fx = fixture(program(0), program(1));
    let engine = fx.engine(CodeSigning::Off);
    let artifact = engine.download(&fx.release("App.exe"), |_, _| {}).unwrap();
    assert_eq!(artifact.path(), fx.downloads().join("delta-1.1.0"));
    assert_eq!(fx.server.hits(), ["/delta"]);

    let installed = engine.install(&artifact).unwrap();
    assert_eq!(
        installed.restart_path.as_deref(),
        Some(fx.installed.as_path())
    );
    assert_eq!(fs::read(&fx.installed).unwrap(), fx.new);
    // As with the full `.exe`, the old version is renamed aside.
    let old = fx.installed.with_extension("old.exe");
    assert_eq!(fs::read(&old).unwrap(), fx.old);
    engine.remove_leftovers();
    assert!(!old.exists());
}

#[test]
fn a_rebuilt_exe_keeps_the_publisher_signature() {
    // Both builds are signed by the same publisher: Microsoft, by catalog.
    let fx = fixture(system_program("hostname.exe"), system_program("find.exe"));
    let engine = fx.engine(CodeSigning::MatchInstalled);
    let artifact = engine.download(&fx.release("App.exe"), |_, _| {}).unwrap();
    assert_eq!(fx.server.hits(), ["/delta"]);
    engine.install(&artifact).unwrap();
    assert_eq!(fs::read(&fx.installed).unwrap(), fx.new);
}

#[test]
fn a_rebuilt_exe_without_the_publisher_signature_is_rejected() {
    // The installed app is signed, but the build the delta produces is not.
    let fx = fixture(system_program("hostname.exe"), program(1));
    let engine = fx.engine(CodeSigning::MatchInstalled);
    // The rebuilt exe is rejected, and then the unsigned full exe too.
    assert!(matches!(
        engine.download(&fx.release("App.exe"), |_, _| {}),
        Err(Error::Payload(_))
    ));
    assert_eq!(fx.server.hits(), ["/delta", "/full"]);
    assert!(!fx.downloads().join("delta-1.1.0").exists());
    assert_eq!(fs::read(&fx.installed).unwrap(), fx.old);
}

#[test]
fn an_msi_release_takes_the_full_download() {
    // Windows Installer owns the files an `.msi` installed, so a delta must
    // not replace them.
    let fx = fixture(program(0), program(1));
    let artifact = fx
        .engine(CodeSigning::Off)
        .download(&fx.release("App.msi"), |_, _| {})
        .unwrap();
    assert_eq!(artifact.path(), fx.downloads().join("App.msi"));
    assert_eq!(fx.server.hits(), ["/full"]);
}

#[test]
fn deltas_need_an_exe_install_root() {
    let fx = fixture(program(0), program(1));
    let root = fx.installed.parent().unwrap();
    let artifact = fx
        .engine_at(root, CodeSigning::Off)
        .download(&fx.release("App.exe"), |_, _| {})
        .unwrap();
    assert_eq!(artifact.path(), fx.downloads().join("App.exe"));
    assert_eq!(fx.server.hits(), ["/full"]);
}
