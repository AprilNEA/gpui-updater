#![allow(clippy::unwrap_used)]

use std::fs;

use gpui_updater_core::{StaticManifestSource, UpdateSource as _, Version};

use super::*;
use crate::sign::{MinisignKey, MinisignOptions};

/// An OpenLogi-shaped `dist/`: two DMGs, an MSI and a ZIP, side files, and a
/// Linux archive.
fn dist(signed: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let key = MinisignKey::generate().unwrap();
    for name in [
        "OpenLogi-v0.8.14-macos-arm64.dmg",
        "OpenLogi-v0.8.14-macos-x86_64.dmg",
        "OpenLogi-v0.8.14-windows-x86_64.msi",
        "OpenLogi-v0.8.14-windows-x86_64.zip",
        "OpenLogi-v0.8.14-linux-x86_64.tar.gz",
    ] {
        let path = dir.path().join(name);
        fs::write(&path, name.as_bytes()).unwrap();
        if signed {
            key.sign_file_to_sidecar(&path, &MinisignOptions::version("0.8.14"))
                .unwrap();
        }
    }
    fs::write(dir.path().join("SHA256SUMS"), "x").unwrap();
    dir
}

fn base() -> ManifestBuilder {
    ManifestBuilder::new("v0.8.14", "https://updates.example.com/releases/v0.8.14")
        .published_at(PublishedAt::At("2026-10-18T12:00:00Z".into()))
}

#[test]
fn lists_classified_files_with_hashes_and_signature_urls() {
    let dir = dist(true);
    let manifest = base()
        .app_id("org.openlogi.OpenLogi")
        .tag("v0.8.14")
        .channel("stable")
        .classifier(Classifier::default().minimum_os_version("macos", "13.0"))
        .scan_dir(dir.path())
        .unwrap()
        .exclude_os("linux")
        .require_os("macos")
        .require_target("windows", "x86_64")
        .build()
        .unwrap();
    assert_eq!(manifest.version, "0.8.14");
    assert_eq!(
        manifest.published_at.as_deref(),
        Some("2026-10-18T12:00:00Z")
    );
    let names: Vec<_> = manifest.assets.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "OpenLogi-v0.8.14-macos-arm64.dmg",
            "OpenLogi-v0.8.14-macos-x86_64.dmg",
            "OpenLogi-v0.8.14-windows-x86_64.msi",
            "OpenLogi-v0.8.14-windows-x86_64.zip",
        ]
    );
    let dmg = &manifest.assets[0];
    assert_eq!(
        dmg.url,
        "https://updates.example.com/releases/v0.8.14/OpenLogi-v0.8.14-macos-arm64.dmg"
    );
    assert_eq!(
        dmg.signature_url.as_deref(),
        Some(
            "https://updates.example.com/releases/v0.8.14/OpenLogi-v0.8.14-macos-arm64.dmg.minisig"
        )
    );
    assert_eq!(dmg.size, Some(32));
    assert_eq!(
        dmg.sha256.as_deref(),
        Some(sha256_file(&dir.path().join(&dmg.name)).unwrap().as_str())
    );
    assert_eq!(dmg.minimum_os_version.as_deref(), Some("13.0"));
    assert_eq!(manifest.assets[2].minimum_os_version, None);
}

#[test]
fn signature_policy_and_style() {
    let unsigned = dist(false);
    let err = base()
        .scan_dir(unsigned.path())
        .unwrap()
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("no minisign signature"), "{err}");
    let manifest = base()
        .signatures(SignaturePolicy::IfPresent)
        .scan_dir(unsigned.path())
        .unwrap()
        .build()
        .unwrap();
    assert!(manifest.assets.iter().all(|a| a.signature_url.is_none()));

    let signed = dist(true);
    let manifest = base()
        .signature_style(SignatureStyle::Inline)
        .include_os(["windows"])
        .scan_dir(signed.path())
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(manifest.assets.len(), 2);
    assert!(
        manifest.assets[0]
            .signature
            .as_deref()
            .unwrap()
            .starts_with("untrusted comment:")
    );
    assert!(manifest.assets[0].signature_url.is_none());
}

#[test]
fn requirements_and_unmatched_files_fail_the_build() {
    let dir = dist(true);
    let err = base()
        .scan_dir(dir.path())
        .unwrap()
        .require_target("macos", "universal")
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("macos/universal"), "{err}");
    fs::write(dir.path().join("notes.txt"), "x").unwrap();
    let err = base()
        .unmatched(Unmatched::Error)
        .scan_dir(dir.path())
        .unwrap()
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("notes.txt"), "{err}");
}

#[test]
fn attaches_deltas_newest_first_and_sets_expiry() {
    let dir = dist(true);
    let key = MinisignKey::generate().unwrap();
    for from in ["0.8.9", "0.8.13", "0.8.12"] {
        let path = dir
            .path()
            .join(delta_file_name("OpenLogi-v0.8.14-macos-arm64.dmg", from));
        fs::write(&path, from).unwrap();
        key.sign_file_to_sidecar(&path, &MinisignOptions::version("0.8.14"))
            .unwrap();
    }
    let sparkle = SparkleKey::generate();
    let public = sparkle.public_key_base64();
    let manifest = base()
        .ed25519_key(sparkle)
        .expires_after(time::Duration::days(30))
        .scan_dir(dir.path())
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(manifest.expires.as_deref(), Some("2026-11-17T12:00:00Z"));
    let by_name = |name: &str| manifest.assets.iter().find(|a| a.name == name).unwrap();
    let dmg = by_name("OpenLogi-v0.8.14-macos-arm64.dmg");
    let froms: Vec<_> = dmg.deltas.iter().map(|d| d.from.as_str()).collect();
    assert_eq!(froms, ["0.8.13", "0.8.12", "0.8.9"]);
    assert!(
        dmg.deltas[0]
            .url
            .ends_with("/OpenLogi-v0.8.14-macos-arm64.dmg.from-0.8.13.delta")
    );
    assert!(
        dmg.deltas[0]
            .signature_url
            .as_deref()
            .unwrap()
            .ends_with(".delta.minisig")
    );
    let path = dir.path().join(&dmg.name);
    gpui_updater_core::verify::verify_ed25519(
        &path,
        &public,
        dmg.ed25519_signature.as_deref().unwrap(),
    )
    .unwrap();
    assert_eq!(by_name("OpenLogi-v0.8.14-macos-x86_64.dmg").deltas, []);
}

#[test]
fn a_delta_for_a_missing_asset_is_an_error() {
    let dir = dist(false);
    let err = base()
        .signatures(SignaturePolicy::Ignore)
        .scan_dir(dir.path())
        .unwrap()
        .delta(DeltaFile::new(
            "OpenLogi-v0.8.14-macos-universal.dmg",
            "0.8.13",
            dir.path().join("SHA256SUMS"),
        ))
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("not an asset"), "{err}");
}

#[test]
fn the_core_source_reads_what_the_builder_writes() {
    let dir = dist(true);
    for from in ["0.8.13", "not-a-version"] {
        let path = dir
            .path()
            .join(delta_file_name("OpenLogi-v0.8.14-macos-arm64.dmg", from));
        fs::write(&path, from).unwrap();
    }
    let manifest = base()
        .signatures(SignaturePolicy::IfPresent)
        .notes("Fixes")
        .extra("x_build", serde_json::json!(42))
        .scan_dir(dir.path())
        .unwrap()
        .build()
        .unwrap();
    let out = dir.path().join("out/latest.json");
    write_manifest(&manifest, &out).unwrap();
    let text = fs::read_to_string(&out).unwrap();
    assert!(text.ends_with("}\n"));
    assert_eq!(read_manifest(&out).unwrap(), manifest);

    let base_url = crate::test_server::serve(vec![("/latest.json".into(), text.into_bytes())]);
    let release = StaticManifestSource::new(format!("{base_url}/latest.json"))
        .os("macos")
        .arch("arm64")
        .format("dmg")
        .fetch_latest()
        .unwrap();
    assert_eq!(release.version, Version::new(0, 8, 14));
    assert_eq!(release.notes.as_deref(), Some("Fixes"));
    assert_eq!(release.asset.name, "OpenLogi-v0.8.14-macos-arm64.dmg");
    assert!(release.signature_url.is_some());
    // The core skips the delta whose `from` is not a version.
    assert_eq!(release.deltas.len(), 1);
    assert_eq!(release.deltas[0].from, Version::new(0, 8, 13));
    assert_eq!(release.deltas[0].to, Version::new(0, 8, 14));
}

#[test]
fn sparkle_deltas_are_neither_native_deltas_nor_unmatched() {
    let dir = dist(true);
    let name = crate::sparkle::delta::delta_file_name("OpenLogi-v0.8.14-macos-arm64.dmg", "0.8.13");
    assert!(DeltaFile::from_path(&dir.path().join(&name)).is_none());
    fs::write(dir.path().join(&name), "sparkle").unwrap();
    fs::write(dir.path().join(format!("{name}.json")), "{}").unwrap();
    let manifest = base()
        .unmatched(Unmatched::Error)
        .scan_dir(dir.path())
        .unwrap()
        .build()
        .unwrap();
    assert!(manifest.assets.iter().all(|a| a.deltas.is_empty()));
    assert!(manifest.assets.iter().all(|a| !a.name.contains("sparkle")));
}
