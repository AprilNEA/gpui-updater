#![allow(clippy::unwrap_used, reason = "tests")]

use super::*;

fn pats(p: &[&str]) -> Vec<String> {
    p.iter().map(ToString::to_string).collect()
}

#[test]
fn asset_matching_requires_all_substrings_case_insensitively() {
    let asset = ManifestAsset {
        name: "OpenLogi-v0.2.1-macOS-arm64.DMG".to_string(),
        url: "https://dl.example.com/app.dmg".to_string(),
        size: None,
        sha256: None,
        signature: None,
        signature_url: None,
        ed25519_signature: None,
        os: Some("macos".to_string()),
        arch: Some("arm64".to_string()),
        format: Some("dmg".to_string()),
        deltas: Vec::new(),
    };
    assert!(asset_matches(
        &asset,
        AssetSelector {
            name_patterns: &pats(&["macos", "arm64", ".dmg"]),
            os: Some("macos"),
            arch: Some("ARM64"),
            format: Some("dmg"),
        }
    ));
    assert!(!asset_matches(
        &asset,
        AssetSelector {
            name_patterns: &pats(&["macos", "arm64", ".dmg"]),
            os: Some("windows"),
            arch: Some("arm64"),
            format: Some("dmg"),
        }
    ));
}

#[test]
fn deserializes_manifest_with_flat_assets_and_unknown_fields() {
    let json = r#"{
        "schema_version": 1,
        "app_id": "org.openlogi.OpenLogi",
        "version": "v0.2.1",
        "channel": "stable",
        "published_at": "2026-06-01T12:00:00Z",
        "notes": "notes here",
        "assets": [
            {
                "name": "OpenLogi-v0.2.1-macos-arm64.dmg",
                "url": "https://dl.example.com/OpenLogi-v0.2.1-macos-arm64.dmg",
                "os": "macos",
                "arch": "arm64",
                "format": "dmg",
                "size": 1234,
                "sha256": "ABCDEF",
                "signature": "trusted comment: sig",
                "ed25519_signature": "c2ln",
                "minimum_os_version": "13.0"
            }
        ]
    }"#;
    let manifest: Manifest = serde_json::from_str(json).unwrap();
    manifest.validate_schema_version().unwrap();
    assert_eq!(parse_tag(&manifest.version).unwrap().to_string(), "0.2.1");
    let picked = select_asset(
        &manifest.assets,
        AssetSelector {
            name_patterns: &pats(&["macos", "arm64"]),
            os: Some("macos"),
            arch: Some("arm64"),
            format: Some("dmg"),
        },
    )
    .unwrap();
    assert_eq!(picked.size, Some(1234));
    assert_eq!(picked.sha256.as_deref(), Some("ABCDEF"));
    assert_eq!(picked.ed25519_signature.as_deref(), Some("c2ln"));
    assert!(
        select_asset(
            &manifest.assets,
            AssetSelector {
                name_patterns: &pats(&[]),
                os: Some("windows"),
                arch: None,
                format: None,
            },
        )
        .is_none()
    );
}

#[test]
fn explicit_format_replaces_default_extension_matcher() {
    let source = StaticManifestSource::new("https://dl.example.com/latest.json").format("dmg");
    assert_eq!(source.asset_patterns, Vec::<String>::new());
    assert_eq!(source.format.as_deref(), Some("dmg"));
}

#[test]
fn maps_asset_deltas_and_skips_invalid_ones() {
    let json = r#"{
        "version": "0.8.13",
        "assets": [{
            "name": "OpenLogi-v0.8.13-macos-arm64.dmg",
            "url": "https://dl.example.com/OpenLogi-v0.8.13-macos-arm64.dmg",
            "deltas": [
                {
                    "from": "v0.8.12",
                    "url": "https://dl.example.com/v0.8.13/app.dmg.from-0.8.12.delta?x=1",
                    "size": 1850000,
                    "sha256": " ABC ",
                    "signature_url": "https://dl.example.com/v0.8.13/app.dmg.from-0.8.12.delta.minisig"
                },
                {
                    "from": "0.8.11",
                    "name": "custom.delta",
                    "format": "files-future-v2",
                    "url": "https://dl.example.com/custom"
                },
                { "from": "not-semver", "url": "https://dl.example.com/bad.delta" }
            ]
        }]
    }"#;
    let manifest: Manifest = serde_json::from_str(json).unwrap();
    let deltas: Vec<_> = manifest.assets[0]
        .deltas
        .iter()
        .filter_map(|d| delta_asset(d, &Version::new(0, 8, 13)))
        .collect();
    assert_eq!(deltas.len(), 2);
    assert_eq!(deltas[0].from.to_string(), "0.8.12");
    assert_eq!(deltas[0].format, DELTA_FORMAT_FILES_BSDIFF_V1);
    assert_eq!(deltas[0].asset.name, "app.dmg.from-0.8.12.delta");
    assert_eq!(deltas[0].asset.size, 1_850_000);
    assert_eq!(deltas[0].sha256.as_deref(), Some("abc"));
    assert!(deltas[0].signature_url.is_some());
    assert_eq!(deltas[1].asset.name, "custom.delta");
    assert_eq!(deltas[1].format, "files-future-v2");
}

#[test]
fn rejects_unknown_schema_version() {
    let manifest = Manifest {
        schema_version: Some(2),
        version: "1.0.0".to_string(),
        notes: None,
        notes_url: None,
        expires: None,
        assets: Vec::new(),
    };
    assert!(manifest.validate_schema_version().is_err());
}

/// Serve `files` (path, body) over HTTP until the test process exits.
fn serve(files: Vec<(&'static str, Vec<u8>)>) -> String {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
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
            let path = request.split_whitespace().nth(1).unwrap_or("");
            let response = match files.iter().find(|(p, _)| *p == path) {
                Some((_, body)) => {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(body);
                    r
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&response);
        }
    });
    base
}

fn sign(keys: &minisign::KeyPair, data: &[u8]) -> Vec<u8> {
    minisign::sign(None, &keys.sk, std::io::Cursor::new(data), None, None)
        .unwrap()
        .to_string()
        .into_bytes()
}

fn manifest_json(expires: Option<&str>) -> String {
    let expires = expires
        .map(|e| format!(r#""expires":"{e}","#))
        .unwrap_or_default();
    format!(
        r#"{{"version":"1.2.0",{expires}"assets":[{{"name":"App.dmg","url":"https://dl.example.com/App.dmg"}}]}}"#
    )
}

fn source(base: &str, keys: &[&minisign::KeyPair]) -> StaticManifestSource {
    let mut source =
        StaticManifestSource::new(format!("{base}/latest.json")).asset_patterns(pats(&["app"]));
    for key in keys {
        source = source.minisign_public_key(key.pk.to_base64());
    }
    source
}

#[test]
fn verifies_a_signed_manifest_before_parsing_it() {
    let (old, new) = (
        minisign::KeyPair::generate_unencrypted_keypair().unwrap(),
        minisign::KeyPair::generate_unencrypted_keypair().unwrap(),
    );
    let body = manifest_json(Some("2999-01-01T00:00:00Z"));
    let base = serve(vec![
        ("/latest.json", body.clone().into_bytes()),
        ("/latest.json.minisig", sign(&new, body.as_bytes())),
        ("/other.minisig", sign(&old, body.as_bytes())),
        (
            "/tampered.json",
            body.replace("1.2.0", "9.9.9").into_bytes(),
        ),
        ("/tampered.json.minisig", sign(&new, body.as_bytes())),
    ]);
    // Signed by either trusted key, at the default or a custom signature URL.
    let release = source(&base, &[&old, &new]).fetch_latest().unwrap();
    assert_eq!(release.version, Version::new(1, 2, 0));
    let custom = source(&base, &[&old]).signature_url(format!("{base}/other.minisig"));
    assert!(custom.fetch_latest().is_ok());
    // Signed by an untrusted key.
    assert!(matches!(
        source(&base, &[&old]).fetch_latest(),
        Err(Error::Signature(_))
    ));
    // A modified manifest.
    let tampered = StaticManifestSource::new(format!("{base}/tampered.json"))
        .asset_patterns(pats(&["app"]))
        .minisign_public_key(new.pk.to_base64());
    assert!(matches!(tampered.fetch_latest(), Err(Error::Signature(_))));
    // A missing signature.
    let unsigned = source(&base, &[&new]).signature_url(format!("{base}/missing.minisig"));
    assert!(matches!(unsigned.fetch_latest(), Err(Error::Http(_))));
}

#[test]
fn rejects_an_expired_manifest() {
    let keys = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let body = manifest_json(Some("2020-01-01T00:00:00Z"));
    let base = serve(vec![
        ("/latest.json", body.clone().into_bytes()),
        ("/latest.json.minisig", sign(&keys, body.as_bytes())),
    ]);
    for source in [source(&base, &[&keys]), source(&base, &[])] {
        assert!(matches!(
            source.fetch_latest(),
            Err(Error::ManifestExpired { .. })
        ));
    }
}
