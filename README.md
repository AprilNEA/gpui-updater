# gpui-updater

Cross-platform self-update for [GPUI] desktop apps, hosted on GitHub Releases.

GPUI ships no updater of its own, and Zed's `auto_update` crate is GPL-licensed
and wired to Zed's private update server. `gpui-updater` is an independent,
MIT/Apache implementation of the same idea: check a release source, download the
platform artifact, verify it, and swap it into place.

## What it does

- **Sources** — `GitHubSource` reads a repo's Releases (latest or pre-releases),
  picks the asset for the running platform, and resolves a `SHA256SUMS`
  checksum and an optional `.minisig` signature. Bring your own by implementing
  `UpdateSource`.
- **Verification** — SHA-256 against the published checksums, plus optional
  [minisign] (Ed25519) signature verification. Transport security alone is not
  trusted.
- **Install** — platform-native swaps:
  - **macOS**: mount the `.dmg`, `ditto` the new (already-notarized) `.app` onto
    the target volume, then atomically replace the bundle. Nothing is re-signed
    at runtime — the new bundle carries its own signature.
  - **Linux**: extract the `.tar.gz` and atomically replace the binary.
  - **Windows**: rename-in-place for a bare `.exe` (installer/Restart-Manager
    flow is future work).
- **GPUI integration** (`gpui` feature) — an observable `Entity<Updater>` that
  runs the work on the background executor and sets `App::set_restart_path` when
  an update is staged. No background polling: trigger checks explicitly, which
  suits a privacy-conscious "Check for updates" button.

## Installation

Not published on crates.io: the `gpui` feature depends on `gpui` from the zed
git repo, and a crate with a git dependency can't be published to the registry.
Install from git instead:

```toml
[dependencies]
# Core only (blocking engine, no GPUI):
gpui-updater = { git = "https://github.com/AprilNEA/gpui-updater", tag = "v0.0.1" }

# With the GPUI integration (Entity<Updater>):
gpui-updater = { git = "https://github.com/AprilNEA/gpui-updater", tag = "v0.0.1", features = ["gpui"] }
```

When your app already depends on `gpui` from the same zed git source, Cargo
unifies the two onto your pinned commit — `gpui-updater` does not impose a gpui
version.

## Usage

### Blocking engine (any app, CLI included)

```rust
use gpui_updater::{EngineConfig, GitHubSource, UpdateEngine};
use semver::Version;

let source = GitHubSource::new("AprilNEA", "OpenLogi")
    .asset_contains("macos")
    .asset_contains(".dmg")
    .with_checksums("SHA256SUMS")
    .with_minisig();

let engine = UpdateEngine::new(
    source,
    EngineConfig::new(Version::parse(env!("CARGO_PKG_VERSION"))?)
        .minisign_public_key("RWQ…"), // optional
);

if let Some(release) = engine.check()? {
    let artifact = engine.download(&release, |done, total| { /* progress */ })?;
    engine.install(&artifact)?;
}
```

### GPUI entity

Enable the `gpui` feature (see [Installation](#installation)):

```rust
use gpui_updater::{EngineConfig, GitHubSource, UpdateStatus, Updater};

let updater = cx.new(|cx| Updater::new(
    GitHubSource::new("AprilNEA", "OpenLogi")
        .asset_contains("macos").asset_contains(".dmg")
        .with_checksums("SHA256SUMS"),
    EngineConfig::new(current_version),
    cx,
));

// Re-render on status changes:
cx.observe(&updater, |_, _, cx| cx.notify()).detach();

// Drive it from buttons:
updater.update(cx, |u, cx| u.check(cx));
// when status is Available → u.download_and_install(cx)
// when status is Staged → u.restart(cx)
```

`UpdateStatus`: `Idle → Checking → {UpToDate | Available(v)} → Downloading →
Installing → Staged(v) | Errored(msg)`.

## Platform notes

- **macOS** replacing an app in `/Applications` needs write permission to it;
  privilege escalation (Zed's `osascript` admin fallback) is not yet
  implemented.
- The `gpui` feature pulls `gpui` from the zed git repo (no registry release),
  so it needs the same native toolchain GPUI itself requires (a real Xcode +
  Metal on macOS). The core (default features) builds with stable Rust alone.

## License

MIT OR Apache-2.0.

[GPUI]: https://www.gpui.rs/
[minisign]: https://jedisct1.github.io/minisign/
