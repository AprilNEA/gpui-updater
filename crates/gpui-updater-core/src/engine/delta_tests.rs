//! End-to-end delta downloads against a local HTTP server.

#![allow(clippy::unwrap_used, reason = "tests")]

use std::collections::HashMap;
use std::fs;

use super::test_server::Server;
use super::*;
use crate::delta::VersionPair;
use crate::release::{Asset, DELTA_FORMAT_FILES_BSDIFF_V1, DeltaAsset};
use crate::verify;

struct NoSource;

impl UpdateSource for NoSource {
    fn fetch_latest(&self) -> Result<Release> {
        unreachable!("delta tests call download directly")
    }
}

const VERSIONS: [&str; 3] = ["1.0.0", "1.1.0", "1.2.0"];
/// Delta pairs published by the fixture.
const PAIRS: [(&str, &str); 3] = [("1.0.0", "1.1.0"), ("1.1.0", "1.2.0"), ("1.0.0", "1.2.0")];

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// Pretend disk image; large enough that deltas beat it.
fn full() -> Vec<u8> {
    (0..300_000u32).map(|i| (i * 13 % 241) as u8).collect()
}

fn plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleShortVersionString</key><string>{version}</string></dict></plist>
"#
    )
}

/// A bundle for `version` (index into [`VERSIONS`]) declaring `declared`.
fn write_bundle(root: &Path, index: usize, declared: &str) {
    let mut binary: Vec<u8> = (0..50_000u32).map(|i| (i * 7 % 251) as u8).collect();
    for k in 0..index {
        binary[1234 + k * 9000] ^= 0xff;
    }
    fs::create_dir_all(root.join("Contents/MacOS")).unwrap();
    fs::write(root.join("Contents/MacOS/demo"), binary).unwrap();
    fs::write(root.join("Contents/Info.plist"), plist(declared)).unwrap();
}

struct Fixture {
    dir: tempfile::TempDir,
    installed: PathBuf,
    builds: HashMap<String, PathBuf>,
    server: Server,
    /// `(from, to)` -> (sha256, size) of the published delta.
    deltas: HashMap<(String, String), (String, u64)>,
    full_sha: String,
}

fn fixture() -> Fixture {
    fixture_declaring(str::to_string)
}

/// A fixture whose builds declare `declared(version)` as their bundle version.
fn fixture_declaring(declared: impl Fn(&str) -> String) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let installed = dir.path().join("Applications/Demo.app");
    write_bundle(&installed, 0, VERSIONS[0]);
    let mut builds = HashMap::new();
    for (i, version) in VERSIONS.iter().enumerate() {
        let root = dir.path().join(format!("build/{version}/Demo.app"));
        write_bundle(&root, i, &declared(version));
        builds.insert((*version).to_string(), root);
    }
    let mut files = HashMap::from([("/full".to_string(), full())]);
    let mut deltas = HashMap::new();
    for (from, to) in PAIRS {
        let path = dir.path().join(format!("{from}-{to}.delta"));
        crate::delta::create(
            &builds[from],
            &builds[to],
            &VersionPair::new(v(from), v(to)),
            &path,
        )
        .unwrap();
        let bytes = fs::read(&path).unwrap();
        let meta = (verify::sha256_hex(&path).unwrap(), bytes.len() as u64);
        files.insert(format!("/d/{from}-{to}"), bytes);
        deltas.insert((from.to_string(), to.to_string()), meta);
    }
    let full_path = dir.path().join("full");
    fs::write(&full_path, full()).unwrap();
    let full_sha = verify::sha256_hex(&full_path).unwrap();
    Fixture {
        installed,
        builds,
        server: Server::start(files),
        deltas,
        full_sha,
        dir,
    }
}

impl Fixture {
    /// The published delta `from -> to`, with its real checksum and size.
    fn delta(&self, from: &str, to: &str) -> DeltaAsset {
        let (sha256, size) = self.deltas[&(from.to_string(), to.to_string())].clone();
        DeltaAsset {
            from: v(from),
            to: v(to),
            format: DELTA_FORMAT_FILES_BSDIFF_V1.into(),
            asset: Asset {
                name: format!("../Demo.dmg.from-{from}.delta"),
                url: format!("{}/d/{from}-{to}", self.server.base),
                size,
            },
            sha256: Some(sha256),
            checksums_url: None,
            signature: None,
            signature_url: None,
            ed25519_signature: None,
        }
    }

    fn release(&self, version: &str, deltas: Vec<DeltaAsset>) -> Release {
        Release::new(
            v(version),
            Asset::new(
                "Demo.dmg",
                format!("{}/full", self.server.base),
                full().len() as u64,
            ),
        )
        .with_sha256(self.full_sha.clone())
        .with_deltas(deltas)
    }

    /// A config without payload checks: the full artifact is not a real
    /// disk image.
    fn config(&self, verification: Verification) -> EngineConfig {
        EngineConfig::new(v("1.0.0"))
            .verification(verification)
            .bundle_version(BundleVersion::Ignore)
            .code_signing(CodeSigning::Off)
            .install_root(&self.installed)
            .download_dir(self.downloads())
    }

    fn engine(&self, verification: Verification) -> UpdateEngine<NoSource> {
        UpdateEngine::new(NoSource, self.config(verification))
    }

    fn downloads(&self) -> PathBuf {
        self.dir.path().join("downloads")
    }

    fn assert_rebuilt(&self, out: impl AsRef<Path>, version: &str) {
        let out = out.as_ref();
        for rel in ["Contents/MacOS/demo", "Contents/Info.plist"] {
            assert_eq!(
                fs::read(out.join("Demo.app").join(rel)).unwrap(),
                fs::read(self.builds[version].join(rel)).unwrap(),
                "{rel}"
            );
        }
    }

    fn assert_full(&self, out: impl AsRef<Path>) {
        let out = out.as_ref();
        assert_eq!(out, self.downloads().join("Demo.dmg"));
        assert_eq!(fs::read(out).unwrap(), full());
        let leftovers: Vec<_> = fs::read_dir(self.downloads())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "Demo.dmg")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[test]
fn rebuilds_from_a_matching_delta() {
    let fx = fixture();
    let release = fx.release("1.1.0", vec![fx.delta("1.0.0", "1.1.0")]);
    let mut totals = Vec::new();
    let out = fx
        .engine(Verification::BestEffort)
        .download(&release, |_, total| totals.push(total))
        .unwrap();
    assert_eq!(out.path(), fx.downloads().join("delta-1.1.0"));
    fx.assert_rebuilt(&out, "1.1.0");
    assert_eq!(fx.server.hits(), ["/d/1.0.0-1.1.0"]);
    assert!(totals.iter().all(Option::is_some));
    // The downloaded delta is removed, and its hostile name stayed inside
    // the download directory.
    assert!(!fx.downloads().join("Demo.dmg.from-1.0.0.delta").exists());
    assert!(!fx.dir.path().join("Demo.dmg.from-1.0.0.delta").exists());
}

#[test]
fn chains_deltas_when_none_starts_at_the_installed_version() {
    let fx = fixture();
    let release = fx.release(
        "1.2.0",
        vec![fx.delta("1.1.0", "1.2.0"), fx.delta("1.0.0", "1.1.0")],
    );
    let mut seen = Vec::new();
    let out = fx
        .engine(Verification::Checksum)
        .download(&release, |got, total| seen.push((got, total)))
        .unwrap();
    assert_eq!(out.path(), fx.downloads().join("delta-1.2.0"));
    fx.assert_rebuilt(&out, "1.2.0");
    assert_eq!(fx.server.hits(), ["/d/1.0.0-1.1.0", "/d/1.1.0-1.2.0"]);
    // Progress spans the whole chain, and the intermediate tree is gone.
    let total = fx.deltas.values().map(|(_, s)| s).sum::<u64>()
        - fx.deltas[&("1.0.0".into(), "1.2.0".into())].1;
    assert!(seen.iter().all(|&(_, t)| t == Some(total)));
    assert_eq!(seen.last().unwrap().0, total);
    assert!(!fx.downloads().join("delta-1.1.0").exists());
}

#[test]
fn prefers_the_cheapest_chain() {
    let fx = fixture();
    let direct = fx.delta("1.0.0", "1.2.0");
    let hops = fx.delta("1.0.0", "1.1.0").asset.size + fx.delta("1.1.0", "1.2.0").asset.size;
    assert!(direct.asset.size < hops);
    let release = fx.release(
        "1.2.0",
        vec![
            fx.delta("1.1.0", "1.2.0"),
            fx.delta("1.0.0", "1.1.0"),
            direct,
        ],
    );
    let out = fx
        .engine(Verification::Checksum)
        .download(&release, |_, _| {})
        .unwrap();
    fx.assert_rebuilt(&out, "1.2.0");
    assert_eq!(fx.server.hits(), ["/d/1.0.0-1.2.0"]);
}

#[test]
fn respects_the_hop_limit() {
    let fx = fixture();
    let release = fx.release(
        "1.2.0",
        vec![fx.delta("1.1.0", "1.2.0"), fx.delta("1.0.0", "1.1.0")],
    );
    let engine = UpdateEngine::new(
        NoSource,
        fx.config(Verification::Checksum).max_delta_hops(1),
    );
    let out = engine.download(&release, |_, _| {}).unwrap();
    fx.assert_full(&out);
    assert_eq!(fx.server.hits(), ["/full"]);
}

#[test]
fn a_failing_hop_falls_back_and_cleans_up() {
    let fx = fixture();
    let mut second = fx.delta("1.1.0", "1.2.0");
    second.sha256 = Some("0".repeat(64));
    let release = fx.release("1.2.0", vec![second, fx.delta("1.0.0", "1.1.0")]);
    let out = fx
        .engine(Verification::Checksum)
        .download(&release, |_, _| {})
        .unwrap();
    fx.assert_full(&out);
    assert_eq!(
        fx.server.hits(),
        ["/d/1.0.0-1.1.0", "/d/1.1.0-1.2.0", "/full"]
    );
}

#[test]
fn an_oversized_delta_is_cut_off() {
    let fx = fixture();
    let mut delta = fx.delta("1.0.0", "1.1.0");
    delta.asset.size -= 1;
    let release = fx.release("1.1.0", vec![delta]);
    let out = fx
        .engine(Verification::BestEffort)
        .download(&release, |_, _| {})
        .unwrap();
    fx.assert_full(&out);
    assert_eq!(fx.server.hits(), ["/d/1.0.0-1.1.0", "/full"]);
}

#[cfg(target_os = "macos")]
#[test]
fn installs_the_rebuilt_bundle_after_checking_its_version() {
    let fx = fixture();
    let config = fx
        .config(Verification::Checksum)
        .bundle_version(BundleVersion::MustMatch)
        .code_signing(CodeSigning::MatchInstalled);
    let engine = UpdateEngine::new(NoSource, config);
    let release = fx.release("1.1.0", vec![fx.delta("1.0.0", "1.1.0")]);
    let out = engine.download(&release, |_, _| {}).unwrap();
    let installed = engine.install(&out).unwrap();
    assert_eq!(installed.restart_path, Some(fx.installed.clone()));
    fx.assert_rebuilt(fx.installed.parent().unwrap(), "1.1.0");
}

#[cfg(target_os = "macos")]
#[test]
fn a_bundle_declaring_another_version_is_rejected() {
    // The publisher's build of 1.1.0 claims to be 1.0.5.
    let fx = fixture_declaring(|version| {
        if version == "1.1.0" {
            "1.0.5".into()
        } else {
            version.into()
        }
    });
    let config = fx
        .config(Verification::Checksum)
        .bundle_version(BundleVersion::MustMatch);
    let engine = UpdateEngine::new(NoSource, config);
    let release = fx.release("1.1.0", vec![fx.delta("1.0.0", "1.1.0")]);
    // The rebuilt bundle is rejected, then the (fake) full image fails too.
    assert!(engine.download(&release, |_, _| {}).is_err());
    assert_eq!(fx.server.hits(), ["/d/1.0.0-1.1.0", "/full"]);
    assert!(!fx.downloads().join("delta-1.1.0").exists());
}

#[test]
fn modified_installation_falls_back_to_the_full_download() {
    let fx = fixture();
    fs::write(
        fx.installed.join("Contents/MacOS/demo"),
        "edited by the user",
    )
    .unwrap();
    let release = fx.release("1.1.0", vec![fx.delta("1.0.0", "1.1.0")]);
    let out = fx
        .engine(Verification::BestEffort)
        .download(&release, |_, _| {})
        .unwrap();
    fx.assert_full(&out);
    assert_eq!(fx.server.hits(), ["/d/1.0.0-1.1.0", "/full"]);
}

#[test]
fn checksum_policy_governs_the_delta_too() {
    // No published delta checksum: the delta is skipped without a download.
    let fx = fixture();
    let mut delta = fx.delta("1.0.0", "1.1.0");
    delta.sha256 = None;
    let release = fx.release("1.1.0", vec![delta]);
    let out = fx
        .engine(Verification::Checksum)
        .download(&release, |_, _| {})
        .unwrap();
    fx.assert_full(&out);
    assert_eq!(fx.server.hits(), ["/full"]);

    // A wrong checksum: downloaded, rejected, then the full download.
    let fx = fixture();
    let mut delta = fx.delta("1.0.0", "1.1.0");
    delta.sha256 = Some("0".repeat(64));
    let release = fx.release("1.1.0", vec![delta]);
    let out = fx
        .engine(Verification::Checksum)
        .download(&release, |_, _| {})
        .unwrap();
    fx.assert_full(&out);
    assert_eq!(fx.server.hits(), ["/d/1.0.0-1.1.0", "/full"]);
}

#[test]
fn checksums_can_come_from_a_checksums_file() {
    let fx = fixture();
    let mut delta = fx.delta("1.0.0", "1.1.0");
    let sums = format!(
        "{}  {}\n",
        delta.sha256.take().unwrap(),
        "Demo.dmg.from-1.0.0.delta"
    );
    let sums_server = Server::start(HashMap::from([(
        "/SHA256SUMS".to_string(),
        sums.into_bytes(),
    )]));
    delta.asset.name = "Demo.dmg.from-1.0.0.delta".into();
    delta.checksums_url = Some(format!("{}/SHA256SUMS", sums_server.base));
    let release = fx.release("1.1.0", vec![delta]);
    let out = fx
        .engine(Verification::Checksum)
        .download(&release, |_, _| {})
        .unwrap();
    fx.assert_rebuilt(&out, "1.1.0");
    assert_eq!(sums_server.hits(), ["/SHA256SUMS"]);
}

#[test]
fn delta_for_another_version_pair_is_not_applied() {
    // Listed as 1.0.0 -> 1.2.0, but the file's header says 1.0.0 -> 1.1.0.
    let fx = fixture();
    let mut delta = fx.delta("1.0.0", "1.1.0");
    delta.to = v("1.2.0");
    let release = fx.release("1.2.0", vec![delta]);
    let out = fx
        .engine(Verification::BestEffort)
        .download(&release, |_, _| {})
        .unwrap();
    fx.assert_full(&out);
    assert_eq!(fx.server.hits(), ["/d/1.0.0-1.1.0", "/full"]);
}

#[test]
fn disabled_deltas_are_never_fetched() {
    let fx = fixture();
    let release = fx.release("1.1.0", vec![fx.delta("1.0.0", "1.1.0")]);
    let engine = UpdateEngine::new(NoSource, fx.config(Verification::BestEffort).deltas(false));
    let out = engine.download(&release, |_, _| {}).unwrap();
    fx.assert_full(&out);
    assert_eq!(fx.server.hits(), ["/full"]);
}
