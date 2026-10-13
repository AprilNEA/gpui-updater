#![allow(clippy::unwrap_used, reason = "tests")]

use std::fs;
use std::path::{Path, PathBuf};

use semver::Version;

use super::fsutil::{join_rel, mode_of, sha256_hex, walk};
use super::macho::tests::signed;
use super::{FORMAT, MAGIC, VersionPair, apply, create, read_info};
use crate::error::Error;

fn pair(from: &str, to: &str) -> VersionPair {
    VersionPair::new(Version::parse(from).unwrap(), Version::parse(to).unwrap())
}

fn write(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

/// Pseudo-random but deterministic bytes, so patches are non-trivial.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[3]
        })
        .collect()
}

/// Scattered edits plus a tail, like a recompiled binary.
fn recompiled(data: &[u8], seed: u64) -> Vec<u8> {
    let mut out = data.to_vec();
    for i in (0..out.len()).step_by(997) {
        out[i] = out[i].wrapping_add(3);
    }
    out.extend_from_slice(&noise(seed, 5_000));
    out
}

fn snapshot(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for rel in walk(root).unwrap() {
        let path = join_rel(root, &rel);
        let meta = fs::symlink_metadata(&path).unwrap();
        let desc = if meta.file_type().is_symlink() {
            format!("link {}", fs::read_link(&path).unwrap().display())
        } else if meta.is_dir() {
            format!("dir {:o}", mode_of(&meta))
        } else {
            format!(
                "file {:o} {}",
                mode_of(&meta),
                sha256_hex(&fs::read(&path).unwrap())
            )
        };
        out.push((rel, desc));
    }
    out
}

fn fixture(dir: &Path) -> (PathBuf, PathBuf) {
    let old = dir.join("old/Demo.app");
    let new = dir.join("new/Demo.app");
    let exe = noise(1, 200_000);
    write(&old.join("Contents/MacOS/demo"), &exe);
    write(&new.join("Contents/MacOS/demo"), &recompiled(&exe, 2));
    write(&old.join("Contents/Info.plist"), b"<plist>1.0.0</plist>");
    write(&new.join("Contents/Info.plist"), b"<plist>1.1.0</plist>");
    write(&old.join("Contents/Resources/same.txt"), b"unchanged");
    write(&new.join("Contents/Resources/same.txt"), b"unchanged");
    write(
        &old.join("Contents/Resources/old-name.bin"),
        &noise(3, 4096),
    );
    write(&new.join("Contents/Resources/renamed.bin"), &noise(3, 4096));
    write(&old.join("Contents/Resources/removed.txt"), b"gone");
    write(&new.join("Contents/Resources/added.txt"), b"brand new file");
    write(&old.join("Contents/Resources/empty-before"), b"");
    write(
        &new.join("Contents/Resources/empty-before"),
        b"now has content",
    );
    write(&new.join("Contents/Resources/empty"), b"");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for root in [&old, &new] {
            fs::set_permissions(
                root.join("Contents/MacOS/demo"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        std::os::unix::fs::symlink("Resources/added.txt", new.join("Contents/link")).unwrap();
    }
    (old, new)
}

/// The JSON header of a delta file.
fn header_json(delta: &Path) -> serde_json::Value {
    let bytes = fs::read(delta).unwrap();
    let len = usize::try_from(u64::from_le_bytes(bytes[8..16].try_into().unwrap())).unwrap();
    serde_json::from_slice(&bytes[16..16 + len]).unwrap()
}

#[test]
fn round_trips_a_directory_tree() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = fixture(dir.path());
    let delta = dir.path().join("demo.delta");
    let made = create(&old, &new, &pair("1.0.0", "1.1.0"), &delta).unwrap();
    assert_eq!(made.files, 7);
    // The new empty file is copied from the old empty file by content.
    assert_eq!(made.copied, 3, "unchanged + renamed + empty");
    assert_eq!(made.patched, 1, "binary");
    // The plist patch would be larger than the compressed plist itself.
    assert_eq!(made.added, 3, "new file + previously empty file + plist");

    let out = dir.path().join("out/Demo.app");
    fs::create_dir_all(out.parent().unwrap()).unwrap();
    let applied = apply(&delta, &old, &out).unwrap();
    assert_eq!(applied, made);
    assert_eq!(read_info(&delta).unwrap(), made);
    assert_eq!(snapshot(&out), snapshot(&new));
    // The base is only read.
    assert!(old.join("Contents/Resources/removed.txt").exists());
    // The patch is much smaller than the binary it rebuilds.
    assert!(fs::metadata(&delta).unwrap().len() < 40_000);
}

#[test]
fn output_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = fixture(dir.path());
    let (a, b) = (dir.path().join("a.delta"), dir.path().join("b.delta"));
    create(&old, &new, &pair("1.0.0", "1.1.0"), &a).unwrap();
    create(&old, &new, &pair("1.0.0", "1.1.0"), &b).unwrap();
    assert_eq!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
}

#[test]
fn round_trips_a_single_file() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old-bin");
    let new = dir.path().join("new-bin");
    let data = noise(7, 100_000);
    fs::write(&old, &data).unwrap();
    let mut changed = data.clone();
    changed[500] ^= 0xff;
    fs::write(&new, &changed).unwrap();
    let delta = dir.path().join("bin.delta");
    let info = create(&old, &new, &pair("1.0.0", "2.0.0"), &delta).unwrap();
    assert_eq!((info.files, info.patched), (1, 1));

    let out = dir.path().join("rebuilt");
    apply(&delta, &old, &out).unwrap();
    assert_eq!(fs::read(&out).unwrap(), changed);
}

#[test]
fn stores_unrelated_content_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (dir.path().join("old"), dir.path().join("new"));
    // Same path, but nothing in common: compressible text replaces noise.
    write(&old.join("data"), &noise(9, 50_000));
    write(
        &new.join("data"),
        "all new, highly repetitive text ".repeat(2000).as_bytes(),
    );
    let delta = dir.path().join("d.delta");
    let info = create(&old, &new, &pair("1.0.0", "1.0.1"), &delta).unwrap();
    assert_eq!((info.patched, info.added), (0, 1));
    let out = dir.path().join("out");
    apply(&delta, &old, &out).unwrap();
    assert_eq!(snapshot(&out), snapshot(&new));
}

#[test]
fn patches_signed_mach_o_images_without_their_page_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (dir.path().join("old"), dir.path().join("new"));
    let code = noise(11, 120_000);
    let (old_bin, new_bin) = (signed(&code), signed(&recompiled(&code, 12)));
    write(&old.join("Contents/MacOS/demo"), &old_bin);
    write(&new.join("Contents/MacOS/demo"), &new_bin);
    let delta = dir.path().join("d.delta");
    let info = create(&old, &new, &pair("1.0.0", "1.1.0"), &delta).unwrap();
    assert_eq!(info.patched, 1);
    let header = header_json(&delta);
    let file = header["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "Contents/MacOS/demo")
        .unwrap();
    assert_eq!(file["source"]["transform"], "macho-code-slots");

    let out = dir.path().join("out");
    apply(&delta, &old, &out).unwrap();
    assert_eq!(fs::read(out.join("Contents/MacOS/demo")).unwrap(), new_bin);
}

#[test]
fn rejects_a_modified_installation_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = fixture(dir.path());
    let delta = dir.path().join("demo.delta");
    create(&old, &new, &pair("1.0.0", "1.1.0"), &delta).unwrap();

    write(&old.join("Contents/MacOS/demo"), b"tampered");
    let out = dir.path().join("Demo.app");
    let err = apply(&delta, &old, &out).unwrap_err();
    assert!(
        matches!(err, Error::Delta(ref m) if m.contains("Contents/MacOS/demo")),
        "{err}"
    );
    assert!(!out.exists(), "partial output must be removed");
}

#[test]
fn rejects_a_corrupt_patch() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = fixture(dir.path());
    let delta = dir.path().join("demo.delta");
    create(&old, &new, &pair("1.0.0", "1.1.0"), &delta).unwrap();

    let mut bytes = fs::read(&delta).unwrap();
    let last = bytes.len() - 10;
    bytes[last] ^= 0x55;
    fs::write(&delta, &bytes).unwrap();
    let out = dir.path().join("Demo.app");
    assert!(matches!(apply(&delta, &old, &out), Err(Error::Delta(_))));
    assert!(!out.exists());
}

fn handcrafted(dir: &Path, header: &serde_json::Value) -> PathBuf {
    let json = serde_json::to_vec(header).unwrap();
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&(json.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&json);
    let path = dir.join("crafted.delta");
    fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn rejects_paths_that_escape_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    fs::create_dir(&base).unwrap();
    for path in ["../escape", "/etc/passwd", "a/../../b", "./x", ""] {
        let delta = handcrafted(
            dir.path(),
            &serde_json::json!({
                "format": FORMAT, "from": "1.0.0", "to": "1.1.0", "root": "dir",
                "entries": [{"path": path, "kind": "dir", "mode": 0o755}]
            }),
        );
        let out = dir.path().join("out");
        assert!(
            matches!(apply(&delta, &base, &out), Err(Error::Delta(_))),
            "{path}"
        );
        assert!(!out.exists());
    }
}

#[test]
fn rejects_writing_through_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    fs::create_dir(&base).unwrap();
    let delta = handcrafted(
        dir.path(),
        &serde_json::json!({
            "format": FORMAT, "from": "1.0.0", "to": "1.1.0", "root": "dir",
            "entries": [
                {"path": "link", "kind": "symlink", "target": "../../outside"},
                {"path": "link/file", "kind": "dir", "mode": 0o755}
            ]
        }),
    );
    assert!(matches!(
        apply(&delta, &base, &dir.path().join("out")),
        Err(Error::Delta(m)) if m.contains("before its parent")
    ));
}

#[test]
fn rejects_unknown_format_transform_and_bad_magic() {
    let dir = tempfile::tempdir().unwrap();
    let delta = handcrafted(
        dir.path(),
        &serde_json::json!({
            "format": "files-zstd-v9", "from": "1.0.0", "to": "1.1.0", "root": "dir",
            "entries": []
        }),
    );
    assert!(matches!(read_info(&delta), Err(Error::Delta(m)) if m.contains("unsupported")));

    let delta = handcrafted(
        dir.path(),
        &serde_json::json!({
            "format": FORMAT, "from": "1.0.0", "to": "1.1.0", "root": "file",
            "entries": [{"path": "", "kind": "file", "mode": 0o755, "size": 1, "sha256": "00",
                "source": {"op": "patch", "base": "", "base_sha256": "00", "offset": 0,
                    "len": 0, "transform": "elf-relocations"}}]
        }),
    );
    assert!(matches!(read_info(&delta), Err(Error::Delta(m)) if m.contains("invalid header")));

    fs::write(&delta, b"definitely not a delta file").unwrap();
    assert!(matches!(read_info(&delta), Err(Error::Delta(m)) if m.contains("magic")));
}
