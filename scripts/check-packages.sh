#!/usr/bin/env bash
# Run from the workspace root with Rust 1.98.1+. No registry writes.
set -euo pipefail

root=$(pwd)
version=0.1.0
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
export CARGO_PROFILE_DEV_DEBUG=0
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# Cargo stages unpublished workspace dependencies together. Verification below
# uses only the extracted archives, never the original workspace source paths.
cargo package --workspace --allow-dirty --no-verify
for package in gpui-updater-core gpui-updater gpui-updater-pre; do
    archive="$CARGO_TARGET_DIR/package/$package-$version.crate"
    tar -xzf "$archive" -C "$scratch"
    extracted="$scratch/$package-$version"
    test -f "$extracted/src/lib.rs"
    test -f "$extracted/README.md"
    test -f "$extracted/LICENSE-MIT"
    test -f "$extracted/LICENSE-APACHE"
    python3 - "$extracted/Cargo.toml" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as file:
    manifest = tomllib.load(file)
assert "workspace" not in manifest
for section in [manifest, *manifest.get("target", {}).values()]:
    for kind in ["dependencies", "dev-dependencies", "build-dependencies"]:
        for dependency in section.get(kind, {}).values():
            assert "path" not in dependency and "git" not in dependency, dependency
PY
done

for package in gpui-updater-core gpui-updater gpui-updater-pre; do
    consumer="$scratch/consumer-$package"
    mkdir -p "$consumer/src"
    cat > "$consumer/Cargo.toml" <<EOF
[package]
name = "consumer-$package"
version = "0.0.0"
edition = "2024"
[dependencies]
updater = { package = "$package", path = "$scratch/$package-$version" }
EOF
    if [ "$package" = gpui-updater ]; then
        echo 'gpui = { version = "=0.2.2", default-features = false }' >> "$consumer/Cargo.toml"
    elif [ "$package" = gpui-updater-pre ]; then
        echo 'gpui = { package = "gpui-pre", version = "=0.3.4", default-features = false }' >> "$consumer/Cargo.toml"
    fi
    cat >> "$consumer/Cargo.toml" <<EOF
[patch.crates-io]
gpui-updater-core = { path = "$scratch/gpui-updater-core-$version" }
EOF
    cat > "$consumer/src/main.rs" <<'RS'
use updater::{EngineConfig, StaticManifestSource, UpdateEngine, UpdateStatus, Verification, Version};

fn main() {
    let config = EngineConfig::new(Version::new(1, 2, 3)).verification(Verification::Strict);
    let source = StaticManifestSource::new("https://example.invalid/latest.json");
    let engine = UpdateEngine::new(source, config);
    assert_eq!(engine.current_version(), &Version::new(1, 2, 3));
    assert!(!UpdateStatus::Idle.is_busy());
}
RS
    if [ "$package" != gpui-updater-core ]; then
        cat >> "$consumer/src/main.rs" <<'RS'

use gpui::AppContext as _;

// Type-check the adapter against the consumer's own GPUI dependency identity.
pub fn create(cx: &mut gpui::App) -> gpui::Entity<updater::Updater> {
    cx.new(|cx| updater::Updater::new(
        StaticManifestSource::new("https://example.invalid/latest.json"),
        EngineConfig::new(Version::new(1, 0, 0)), cx))
}
RS
    fi
    cargo build --manifest-path "$consumer/Cargo.toml"
    cargo tree --manifest-path "$consumer/Cargo.toml" -e normal,build --prefix none --format '{p}' > "$consumer/tree"
    python3 - "$package" "$consumer/tree" <<'PY'
import sys
package = sys.argv[1]
names = {line.split()[0] for line in open(sys.argv[2])}
if package == "gpui-updater-core":
    assert not any(name.startswith(("gpui", "gpui_")) and name != package for name in names), names
elif package == "gpui-updater":
    assert "gpui" in names and "gpui-updater-core" in names
    assert not any(name.startswith("gpui-pre") or name == "gpui-updater-pre" for name in names), names
else:
    assert "gpui-pre" in names and "gpui-updater-core" in names
    assert "gpui" not in names and "gpui-updater" not in names
    assert not any(name.startswith(("gpui_", "gpui-macros")) for name in names), names
print(f"PASS {package}: extracted consumer build and isolated normal/build closure")
PY
done
