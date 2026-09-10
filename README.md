# gpui-updater

Cross-platform self-update for [GPUI] desktop apps, hosted on GitHub Releases or
static JSON manifests.

GPUI ships no updater of its own, and Zed's `auto_update` crate is GPL-licensed
and wired to Zed's private update server. `gpui-updater` is an independent,
MIT/Apache implementation of the same idea: check a release source, download the
platform artifact, verify it, and swap it into place — on macOS (`.dmg`),
Linux (`.tar.gz`), and Windows (bare `.exe`, or `.msi` for installer-shipped
apps).

## What it does

- **Sources** — `GitHubSource` reads a repo's Releases (latest or pre-releases),
  picks the asset for the running platform, and resolves a `SHA256SUMS`
  checksum and an optional `.minisig` signature. `StaticManifestSource` reads a
  CDN/S3/R2-friendly `latest.json` with a flat asset list. Bring your own by
  implementing `UpdateSource`.
- **Verification** — SHA-256 against the published checksums, plus optional
  [minisign] (Ed25519) signature verification. Transport security alone is not
  trusted.
- **Install** — platform-native swaps:
  - **macOS**: mount the `.dmg`, `ditto` the new (already-notarized) `.app` onto
    the target volume, then atomically replace the bundle. Nothing is re-signed
    at runtime — the new bundle carries its own signature.
  - **Linux**: extract the `.tar.gz` and atomically replace the binary.
  - **Windows**: rename-in-place for a bare `.exe`, or a staged msiexec
    handoff for an `.msi` — the verified package is applied
    (`/passive /norestart`) after the app exits, via the restart path, then
    the app relaunches. (Restart-Manager integration for locked sibling DLLs
    remains future work.)
- **GPUI adapters** — an observable `Entity<Updater>` that
  runs the work on the background executor and sets `App::set_restart_path` when
  an update is staged. No background polling: trigger checks explicitly, which
  suits a privacy-conscious "Check for updates" button.

## Installation

The breaking **0.1.0** line separates the blocking engine from two independent
GPUI adapters. Pick the adapter matching your application's GPUI package:

| Package | Backend | Rust requirement |
| --- | --- | --- |
| `gpui-updater-core` | None (blocking I/O engine) | 1.85; tested on 1.85.0 |
| `gpui-updater` | Registry `gpui` `^0.2.2` | 1.98; tested on 1.98.1 |
| `gpui-updater-pre` | Registry `gpui-pre` `^0.3.4` | 1.98; tested on 1.98.1 |

Adapter requirements are conservative tested support floors, not a claim that
older compilers necessarily fail. They do not raise the core's requirement.
Registry dependencies for the 0.1 releases:

```toml
[dependencies]
# Core only (blocking engine, no GPUI):
gpui-updater-core = "0.1"

# OR official GPUI (Entity<Updater>):
gpui = "0.2.2"
gpui-updater = "0.1"

# OR gpui-pre (keep existing gpui_updater imports via an alias):
gpui = { package = "gpui-pre", version = "0.3.4" }
gpui-updater = { package = "gpui-updater-pre", version = "0.1" }
```

These are alternatives, not dependencies to combine. Both adapters re-export
the entire core API, including `EngineConfig`, `Verification`, `Version`,
`StaticManifestSource`, `Release`, `Installed`, `UpdateStatus`, and `UpdateEngine`.
Only the adapters export `Updater`. Without the alias, pre imports use
`gpui_updater_pre`. Before a release is published, use the corresponding
`crates/<package>` path from this checkout.

### Migrating from 0.0.x

- Blocking consumers: replace `gpui-updater` with `gpui-updater-core` and change
  imports to `gpui_updater_core` (or retain the old import via a Cargo alias).
- GPUI consumers: select official or pre above and remove `features = ["gpui"]`.
  `Updater::new`, `check`, `download_and_install`, `status`, `available`, and
  `restart` retain their behavior. No update policy or polling is added.
- The adapters depend only on core, never each other. Small GPUI bindings are
  intentionally duplicated; download, verification, and install code are not.

Published adapters contain no baked-in Zed Git dependency. A consumer using a
Git GPUI must patch **at its workspace root** so every dependency uses that
same source, for example:

```toml
[patch.crates-io]
gpui = { git = "https://github.com/zed-industries/zed", rev = "<your-verified-commit>", version = "=0.2.2" }
```

The explicit version disambiguates package selection: some Zed revisions also
contain a `gpui` 0.0.0 fixture, so a Git URL and revision alone are ambiguous.
The Git package version must satisfy `^0.2.2`, and the commit must actually be
API-compatible. No Git pin is claimed as verified by this workspace. An alias
does not make `gpui-pre` and `gpui` interchangeable: they have distinct Cargo
package/source identities and their `Entity`/`Context` types cannot be mixed.
Adapters disable GPUI default features; the application selects its platform
startup crate/features. No platform startup library is a mandatory adapter
runtime dependency.

## Usage

### Pick an update source

Use one of the built-in sources, or implement `UpdateSource` for your own
release service:

| Source | Best for | How it works |
| --- | --- | --- |
| `GitHubSource` | Existing GitHub Releases pipelines | Calls the GitHub Releases API, selects a matching asset, and can read `SHA256SUMS` / `.minisig` assets. |
| `StaticManifestSource` | Cloudflare R2, S3, MinIO, B2, CDNs, static hosting | Fetches a static `latest.json`, selects an asset from `assets[]`, then downloads the asset URL directly. |

### GitHub Releases

```rust
use gpui_updater_core::{EngineConfig, GitHubSource, UpdateEngine};
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

### Static manifests (S3/R2/CDN)

Use `StaticManifestSource` when your release metadata is hosted as a static JSON
file on Cloudflare R2, AWS S3, MinIO, Backblaze B2, GitHub Pages, or any normal
HTTPS file server. The updater does not speak the S3 API; it fetches JSON and
downloads direct artifact URLs. This keeps credentials and provider SDKs out of
the app: upload artifacts however you like, then publish a small JSON pointer.

```rust
use gpui_updater_core::{EngineConfig, StaticManifestSource, UpdateEngine};

let source = StaticManifestSource::new("https://dl.example.com/channels/stable/latest.json")
    .os("macos")
    .arch("arm64")
    .format("dmg");

let engine = UpdateEngine::new(source, EngineConfig::new(current_version));
```

Manifest v1 describes one latest release and a flat list of downloadable assets.
`schema_version`, `version`, `assets[].name`, and `assets[].url` are the only
required fields; all other fields are optional and unknown fields are ignored.
Put channel pointers at mutable URLs such as `/channels/stable/latest.json`, but
keep artifact URLs versioned and immutable.

Recommended layout:

```text
https://dl.example.com/
├── channels/
│   └── stable/
│       └── latest.json          # mutable pointer
└── releases/
    └── v1.2.3/
        ├── App-1.2.3-macos-arm64.dmg
        ├── App-1.2.3-macos-arm64.dmg.minisig
        └── SHA256SUMS
```

```json
{
  "schema_version": 1,
  "app_id": "org.example.App",
  "version": "1.2.3",
  "tag": "v1.2.3",
  "channel": "stable",
  "published_at": "2026-06-01T12:00:00Z",
  "release_url": "https://example.com/releases/v1.2.3",
  "notes": "Markdown release notes.",
  "assets": [
    {
      "name": "App-1.2.3-macos-arm64.dmg",
      "url": "https://dl.example.com/releases/v1.2.3/App-1.2.3-macos-arm64.dmg",
      "os": "macos",
      "arch": "arm64",
      "format": "dmg",
      "size": 12345678,
      "sha256": "0123456789abcdef...",
      "signature": null,
      "signature_url": null,
      "minimum_os_version": "13.0"
    }
  ]
}
```

`sha256` is checked after download when present. `signature` may contain an
inline minisign signature; `signature_url` may point to a detached signature file.
If both are present and a minisign public key is configured, the inline signature
is used.

By default (`Verification::BestEffort`) these checks are skipped when their input
is absent, so an unsigned release still installs. To **fail closed**, set a
stricter policy:

```rust
use gpui_updater_core::Verification;

EngineConfig::new(current_version)
    .minisign_public_key("RWQ…")
    .verification(Verification::Strict);
```

| Policy | Behaviour |
| ------ | --------- |
| `BestEffort` (default) | Verify what's available; skip missing checks (fails open). |
| `Off` | Skip all checks (tests/local dev only). |
| `Checksum` | Require a matching SHA-256. |
| `Signature` | Require a public key **and** an advertised minisign signature. |
| `Strict` | Require both a valid signature and a matching SHA-256. |

Under `Signature`/`Strict`, a release that cannot be verified is rejected at
`check()` time — before it is ever surfaced as available — and again before a
download is verified.

Asset selection is explicit when you use `os`, `arch`, and `format`:

```rust
let source = StaticManifestSource::new("https://dl.example.com/channels/stable/latest.json")
    .os("macos")
    .arch("arm64")
    .format("dmg");
```

You can also add filename substring filters with `asset_contains(...)`, or
replace them completely with `asset_patterns(...)`. Matching is
case-insensitive. If no selector is configured, the source falls back to a
platform extension guess: `.dmg` on macOS, `.exe` on Windows, and `.tar.gz` on
Linux/other platforms. Installer-shipped Windows apps should select their MSI
explicitly with `.format("msi")` — the install step stages it and applies via
msiexec on restart.

For R2/S3-style hosting, the recommended release flow is:

1. Build and sign/notarize platform artifacts.
2. Upload artifacts to an immutable, versioned prefix such as
   `/releases/v1.2.3/`.
3. Generate SHA-256 hashes and, optionally, minisign signatures.
4. Upload `latest.json` to a mutable channel prefix such as
   `/channels/stable/latest.json` after all artifacts are in place.

### GPUI entity

Choose an adapter (see [Installation](#installation)); with the pre alias above
the same imports work:

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

`UpdateStatus`: `Idle → Checking → {UpToDate | Available(v)} →
Downloading { downloaded, total } → Installing → Staged(v) | Errored(msg)`.
`Downloading` carries live byte counts (`total` is `None` when the server omits
`Content-Length`).

Keep the entity alive while updating. Dropping it cancels its foreground task,
but does not interrupt an already-running blocking download/install or roll it
back. A completed download may remain on disk. A check that finds no newer
release or fails retains the previous `available()` release, as in 0.0.x.

## Platform notes

- **macOS** replacing an app in `/Applications` needs write permission to it.
  An admin who drag-installed the app can replace it without a prompt; a
  standard user cannot. A privilege-escalation fallback (an `osascript … with
  administrator privileges` prompt, as Velopack does) is not yet implemented —
  a permission error just surfaces as a failed update.
- Adapters need their GPUI backend's native toolchain (including Xcode/Metal
  where required on macOS). Core does not need GPUI or a graphics toolchain.

## Development notes

The workspace defaults to core; `cargo test` is not adapter coverage. Run:

```bash
cargo +1.85.0 test -p gpui-updater-core --all-targets --locked
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
bash scripts/check-packages.sh
```

CI tests core/platform installers on Linux, macOS and Windows, and builds each
registry adapter independently on Linux. Adapter tests use GPUI's seeded test
executor, local one-shot HTTP fixtures, and temporary install roots. They
exercise observer notifications, busy guards, progress timing, errors, and
entity/task lifetime. `gpui-pre` captures the fake platform restart request;
official 0.2.2 discards its path, so exact-path coverage uses a small isolated
handoff boundary. No test restarts a real app or installs over the test binary.

`scripts/check-packages.sh` packages all three crates without publishing,
checks archive contents, builds fresh consumers from extracted packages, and
checks the selected normal/build dependency closures (core: no GPUI; official:
no pre; pre: no official). Package locks may list unrelated platform/optional
dependencies; they are not evidence that those dependencies are compiled.

Release order: publish `gpui-updater-core` 0.1.0 first; after it is available in
the registry, publish `gpui-updater` and `gpui-updater-pre` 0.1.0. Run normal
package verification again before publishing. A repository tag is not needed
for registry resolution; if a coordinated release tag is created, `v0.1.0`
should identify the reviewed workspace commit, not the old prototype. Never
rewrite an existing release tag or embed a consumer's GPUI Git pin here.

On macOS inside some Nix shells, Cargo may pick Nix's clang wrapper as `cc` and
fail to link build scripts with missing system symbols such as `__Unwind_*` or
`_pthread_*`. Point Cargo at Apple's linker for the host target:

```bash
CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/cc cargo test
```

The same prefix works for `cargo check` and `cargo clippy`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

[GPUI]: https://www.gpui.rs/
[minisign]: https://jedisct1.github.io/minisign/
