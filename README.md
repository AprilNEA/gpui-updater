# gpui-updater

Cross-platform self-update for [GPUI] desktop apps, hosted on GitHub Releases,
static JSON manifests, or [Sparkle] appcasts — and, on macOS, optionally driven
by Sparkle itself.

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
  CDN/S3/R2-friendly `latest.json` with a flat asset list. `AppcastSource`
  reads a Sparkle / WinSparkle appcast. Bring your own by implementing
  `UpdateSource`.
- **Verification** — the published size, SHA-256, and optional [minisign] or
  Sparkle-compatible Ed25519 (`sparkle:edSignature` / `SUPublicEDKey`)
  signatures, with minisign key rotation and signatures bound to a version. On
  macOS the new `.app` must declare the release version and carry the
  installed app's code-signing team. Static manifests can be signed and expire,
  so a stale one cannot be replayed. Transport security alone is not trusted.
  See [Verification and trust](#verification-and-trust).
- **Delta updates** (optional `delta` feature) — download small per-file
  patches instead of the full artifact (chained across several releases when
  needed) and rebuild the new `.app` bundle or executable from the installed
  one. Any problem falls back to the full download. See
  [Delta updates](#delta-updates).
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
- **Sparkle driver** (`gpui-updater-sparkle`) — hands the whole flow on macOS
  to Sparkle 2 (installs for standard users, installs on quit, delta updates,
  Sparkle's UI) behind the same `Entity<Updater>` and `UpdateStatus`.

## Installation

Since **0.1.0** the blocking engine is separate from two independent GPUI
adapters. **0.2.0** adds Sparkle appcasts, Ed25519 verification, and the
Sparkle driver, with a few breaking changes (see
[Migrating from 0.1](#migrating-from-01)). Pick the adapter matching your
application's GPUI package:

| Package | Backend | Rust requirement |
| --- | --- | --- |
| `gpui-updater-core` | None (blocking I/O engine) | 1.85; tested on 1.85.0 |
| `gpui-updater` | Registry `gpui` `^0.2.2` | 1.98; tested on 1.98.1 |
| `gpui-updater-pre` | Registry `gpui-pre` `^0.3.4` | 1.98; tested on 1.98.1 |
| `gpui-updater-sparkle` | Sparkle 2 on macOS (loaded at run time); add it next to an adapter | 1.85; tested on 1.85.0 |

Adapter requirements are conservative tested support floors, not a claim that
older compilers necessarily fail. They do not raise the core's requirement.
Registry dependencies for the 0.2 releases:

```toml
[dependencies]
# Core only (blocking engine, no GPUI):
gpui-updater-core = "0.2"

# OR official GPUI (Entity<Updater>):
gpui = "0.2.2"
gpui-updater = "0.2"

# OR gpui-pre (keep existing gpui_updater imports via an alias):
gpui = { package = "gpui-pre", version = "0.3.4" }
gpui-updater = { package = "gpui-updater-pre", version = "0.2" }

# Optional, next to either adapter: hand the macOS flow to Sparkle 2.
gpui-updater-sparkle = "0.2"
```

The core and the two adapters are alternatives, not dependencies to combine;
`gpui-updater-sparkle` is additive. Both adapters re-export
the entire core API, including `EngineConfig`, `Verification`, `Version`,
`StaticManifestSource`, `Release`, `Installed`, `UpdateStatus`, and `UpdateEngine`.
Only the adapters export `Updater`. Without the alias, pre imports use
`gpui_updater_pre`. Before a release is published, use the corresponding
`crates/<package>` path from this checkout.

### Migrating from 0.2

- `EngineConfig::minisign_public_key` (the field) became
  `minisign_public_keys: Vec<String>`. The builder method of the same name is
  unchanged and may be called more than once to trust several keys.
- On macOS, downloads are now checked before install: the `.app` must declare
  the release version, and when the installed app is signed by a team, the
  update must be validly signed by the same team. Opt out with
  `.bundle_version(BundleVersion::Ignore)` or `.code_signing(CodeSigning::Off)`.
- A download is cut off once it exceeds the size the release publishes
  (GitHub always publishes one).
- `Release` gains `deltas` (build it with `Release::new` / `with_deltas`), and
  `Error` gains `Payload`, `ManifestExpired`, and `Delta`.

### Migrating from 0.1

- `Release`, `Asset`, and `EngineConfig` are `#[non_exhaustive]`. Build them
  with `Release::new` / `Asset::new` / `EngineConfig::new` and the `with_*` or
  builder methods; the fields stay public for reading and adjusting.
  `EngineConfig` also gains `ed25519_public_key`.
- Adapters: `Updater::restart` takes `&mut self`. It needs `Context<Self>`, so
  it is called from `update` closures and call sites are unchanged.
- Adapters: `Updater` is no longer `Send` or `Sync`, because it may hold a
  main-thread-only driver such as `SparkleDriver`. `Entity<Updater>` is still
  `Send + Sync`; only code that moves an `Updater` value itself across threads
  is affected.
- `Verification::Signature` and `Strict` also accept a valid Sparkle Ed25519
  signature once `ed25519_public_key` is set. With only a minisign key
  configured, verification behaves as before.
- Then apply [Migrating from 0.2](#migrating-from-02).

### Migrating from 0.0.x

- Blocking consumers: replace `gpui-updater` with `gpui-updater-core` and change
  imports to `gpui_updater_core` (or retain the old import via a Cargo alias).
- GPUI consumers: select official or pre above and remove `features = ["gpui"]`.
  `Updater::new`, `check`, `download_and_install`, `status`, `available`, and
  `restart` retain their behavior. No update policy or polling is added.
- The adapters depend only on core, never each other. Small GPUI bindings are
  intentionally duplicated; download, verification, and install code are not.
- Then apply [Migrating from 0.1](#migrating-from-01).

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
| `AppcastSource` | Apps that publish (or want to publish) a Sparkle appcast | Fetches an appcast, picks the newest applicable `<item>` and the enclosure for this platform, and carries its `sparkle:edSignature`. |

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
  "expires": "2026-06-15T12:00:00Z",
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

`size`, `sha256`, `signature` (inline minisign signature), `signature_url`
(detached signature file), and `ed25519_signature` (base64, as Sparkle's
`sign_update` prints it) are checked after download as described in
[Verification and trust](#verification-and-trust). If both `signature` and
`signature_url` are present, the inline signature is used. `expires` only
matters for a signed manifest; see there too.

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
   `/channels/stable/latest.json` after all artifacts are in place. If the
   app verifies the manifest, upload `latest.json.minisig` with it, and
   re-sign it before its `expires` time.

### Verification and trust

Every downloaded file, the full artifact or a delta, goes through the same
checks, in this order:

1. **Size.** When the release publishes one (`size` in a manifest; GitHub
   always does), a response announcing another size is rejected and a body
   growing past it is cut off, so a hostile mirror cannot fill the disk.
2. **SHA-256** against the inline `sha256` or the release's checksums file.
3. **Signatures.** A minisign signature (inline `signature`, else
   `signature_url`) made by any configured public key, and a Sparkle-style
   Ed25519 signature (`ed25519_signature`) checked against
   `ed25519_public_key`. Every signature that is published *and* has a
   configured key is verified; any mismatch fails the download.
4. **Signed version.** If the signature's trusted comment contains
   `version:<semver>`, it must equal the version the file is supposed to be:
   the release version, or for a delta the version it produces. Sign with
   `minisign -S -m App.dmg -t "version:1.2.3"`. This stops an older, validly
   signed artifact from being served as a newer release.
   `VersionBinding::Required` also rejects signatures without the token, once
   all your releases carry it. Ed25519 signatures cover the file alone and
   cannot name a version; on macOS check 5 covers them.
5. **App bundle (macOS).** The `.app` in the update must declare the release
   version as `CFBundleShortVersionString` (`BundleVersion::MustMatch`, where
   `1.2` matches `1.2.0`). When the installed app is signed by a team, the
   update must pass `codesign --verify --deep --strict` and be signed by the
   same Team ID (`CodeSigning::MatchInstalled`). Pin a team with
   `CodeSigning::TeamId("…")`, or turn either check off. A delta-rebuilt
   bundle failing these checks falls back to the full download. Like
   Sparkle, this keeps a stolen minisign key alone from shipping someone
   else's app.

By default (`Verification::BestEffort`) checks 2 and 3 are skipped when their
input is absent, so an unsigned release still installs. To **fail closed**,
set a stricter policy:

```rust
use gpui_updater_core::Verification;

EngineConfig::new(current_version)
    .minisign_public_key("RWQ…")
    .verification(Verification::Strict);
```

| Policy | Behaviour |
| ------ | --------- |
| `BestEffort` (default) | Verify what's available; skip missing checks (fails open). |
| `Off` | Skip all checks, including the bundle checks (tests/local dev only). |
| `Checksum` | Require a matching SHA-256. |
| `Signature` | Require a public key **and** an advertised signature it can check (minisign or Ed25519). |
| `Strict` | Require both a valid signature and a matching SHA-256. |

Under `Signature`/`Strict`, a release that cannot be verified is rejected at
`check()` time — before it is ever surfaced as available — and again before a
download is verified.

**Key rotation.** Call `minisign_public_key` more than once: a signature by
any configured key is accepted. Ship a release that trusts both the old and
the new key, then start signing with the new one.

**Signed manifests.** Signed artifacts prove what a file is, but not that the
manifest listing them is current. Whoever controls the host could keep
serving an old manifest, so users never see a security fix (a *freeze*
attack). `StaticManifestSource` can require the manifest itself to be signed:

```rust
let source = StaticManifestSource::new("https://dl.example.com/channels/stable/latest.json")
    .minisign_public_key("RWQ…"); // verifies latest.json against latest.json.minisig
```

The bytes are verified before they are parsed; `.signature_url(…)` overrides
the default `<url>.minisig`. With a top-level `"expires"` (RFC 3339) in the
signed manifest, `check()` fails with `Error::ManifestExpired` after that time
instead of reporting "up to date", which bounds how long a replay works.
Re-sign the manifest on a schedule shorter than its lifetime, e.g. weekly with
a two-week `expires`. Rollback is never possible: `check()` only offers
versions newer than the running one.

### Delta updates

Enable the `delta` feature on whichever crate you depend on
(`gpui-updater-core`, `gpui-updater`, or `gpui-updater-pre`). Nothing else
changes in your app: `download` tries deltas from the running version first
and otherwise behaves exactly as before.

```toml
gpui-updater = { version = "0.1", features = ["delta"] }
```

A delta in the `files-bsdiff-v1` format rebuilds the new install tree (the
`.app` bundle on macOS, the executable on Linux) from the installed one. It
works on unpacked content, because diffing the compressed `.dmg` or `.tar.gz`
gains almost nothing. Each file is stored the cheapest way: copied from the
installed tree (also across renames), patched with bsdiff, or stored whole.
Patches and whole files are LZMA-compressed at the `xz -9e` level. Signed
Mach-O images have the page hashes in their code signature zeroed before
diffing and recomputed when the delta is applied, because those hashes change
on every build and do not diff well. Every file records SHA-256 hashes of both
the installed file it starts from and the exact file it must produce, so the
rebuilt bundle is still byte-identical to the notarized one.

**Chains.** When no delta starts at the running version, the engine chains
deltas (`1.2.0 → 1.2.2 → 1.2.3`). It picks the chain with the fewest bytes, of
at most `max_delta_hops` deltas (default 4), and only if that total is smaller
than the full download.

When an update is applied:

1. Each delta is downloaded and verified under the same
   [checks](#verification-and-trust) as the full artifact: size, `sha256`,
   signature, and the signed version (the version the delta produces). Under
   `Strict`, an unsigned delta is never downloaded.
2. Its header must say exactly the `from -> to` it was listed with, so a
   validly signed delta cannot be replayed for another upgrade.
3. Each installed file is hashed before use. A user-modified or partially
   updated install is rejected instead of being patched into a broken one.
4. Each rebuilt file is hashed after writing. The next delta of a chain starts
   from the result. The final directory holds the rebuilt bundle, which goes
   through the macOS bundle checks and then `install` swaps it in exactly like
   a mounted `.dmg`. Nothing is re-signed.

If any step fails (no usable chain, a network error, a bad checksum or
signature, a modified install), the failure is logged with `tracing::warn!`
and the full artifact is downloaded instead. The progress callback then
restarts from zero. Deltas never make an update fail. Turn them off at runtime
with `EngineConfig::deltas(false)`. Windows still uses full downloads.

**Generating deltas** (release CI). Pick the earlier releases to build deltas
from, unpack each of them and the new build, then create one delta per base:

```bash
MAKE="cargo run -p gpui-updater-core --features delta --release --example make_delta --"
$MAKE bases 1.2.2 1.2.1 1.2.0 1.1.9 1.1.8   # earlier releases, newest first
$MAKE old/MyApp.app new/MyApp.app 1.2.2 1.2.3 MyApp-macos-arm64.dmg.from-1.2.2.delta --verify
minisign -S -m MyApp-macos-arm64.dmg.from-1.2.2.delta -t "version:1.2.3"
```

`bases` prints the 3 newest earlier releases plus those 2, 4, 8, … releases
further back (`--direct N` changes the 3). When every release is published
this way, a user `d` releases behind reaches the newest one in at most
⌊log₂ d⌋ + 1 deltas, while each release ships only a handful of them. Older
installs just take the full download. `--verify` rebuilds the new build from
the old one with the fresh delta before you publish it. Library users can call
`gpui_updater_core::delta::create` and `delta::delta_bases` directly.
Generation keeps whole files in memory and is CPU-heavy, so it belongs in CI,
not in the app.

**Publishing.** On GitHub Releases, name the delta
`<asset>.from-<version>.delta`, e.g. `MyApp-macos-arm64.dmg.from-1.2.2.delta`.
Its checksum is read from the same checksums file and its signature from
`<delta name>.minisig`. Delta files are never picked as the main asset. To
chain deltas attached to earlier releases, let the source see them:

```rust
let source = GitHubSource::new("AprilNEA", "OpenLogi")
    .asset_contains("macos").asset_contains(".dmg")
    .with_checksums("SHA256SUMS")
    .with_minisig()
    .delta_history(20); // still one API request
```

In a static manifest, list deltas on the asset they rebuild:

```json
{
  "name": "MyApp-v1.2.3-macos-arm64.dmg",
  "url": "https://dl.example.com/releases/v1.2.3/MyApp-v1.2.3-macos-arm64.dmg",
  "size": 19670263,
  "sha256": "…",
  "signature_url": "https://dl.example.com/releases/v1.2.3/MyApp-v1.2.3-macos-arm64.dmg.minisig",
  "deltas": [
    {
      "from": "1.2.2",
      "url": "https://dl.example.com/releases/v1.2.3/MyApp-v1.2.3-macos-arm64.dmg.from-1.2.2.delta",
      "size": 1418292,
      "sha256": "…",
      "signature_url": "https://dl.example.com/releases/v1.2.3/MyApp-v1.2.3-macos-arm64.dmg.from-1.2.2.delta.minisig"
    },
    {
      "from": "1.2.0",
      "to": "1.2.2",
      "url": "https://dl.example.com/releases/v1.2.2/MyApp-v1.2.2-macos-arm64.dmg.from-1.2.0.delta",
      "size": 2079621,
      "sha256": "…",
      "signature_url": "https://dl.example.com/releases/v1.2.2/MyApp-v1.2.2-macos-arm64.dmg.from-1.2.0.delta.minisig"
    }
  ]
}
```

`from` and `url` are required. `to` defaults to the manifest's `version`; set
it to list deltas between earlier releases for chaining. `format` defaults to
`files-bsdiff-v1`. A delta in an unknown format is ignored, so newer formats
can be published next to older ones. Deltas take the same `sha256`,
`signature`, `signature_url`, and `ed25519_signature` fields as assets, and
the same policy applies: under `Strict`, a delta without a checksum and a
signature you can check is skipped in favour of the full download.
`AppcastSource` lists no deltas, so appcast feeds always get the full archive.

Measured on [OpenLogi] macOS arm64 releases (19.7 MB `.dmg`, 51.4 MB unpacked):

| Upgrade | Delta | Share of full download |
| --- | --- | --- |
| 0.8.12 → 0.8.13 | 1.42 MB | 7.2% |
| 0.8.11 → 0.8.13 | 2.36 MB | 12.0% |
| 0.8.11 → 0.8.12 → 0.8.13 (chain, not chosen) | 3.50 MB | 17.8% |

Compared with plain bsdiff patches (bzip2-compressed), LZMA saved 18% and
normalizing Mach-O code signatures another 6%.
Generating a delta took about 18 s with 4 worker threads and a peak of about
1 GB of memory, and the output is identical on every run. Downloading and applying it locally took under a second. The
installed result was identical to the full 0.8.13 bundle, and
`codesign --verify --deep --strict` and Gatekeeper (`spctl`, "Notarized
Developer ID") both accepted it.

### Sparkle appcasts

`AppcastSource` reads the RSS feed that [Sparkle] (macOS) and WinSparkle
(Windows) consume, so one feed can serve a Sparkle-driven macOS build and the
built-in engine everywhere else:

```rust
use gpui_updater_core::{AppcastSource, EngineConfig, UpdateEngine, Verification};

let source = AppcastSource::new("https://dl.example.com/appcast.xml")
    .channel("beta"); // optional: also offer `sparkle:channel` "beta"

let engine = UpdateEngine::new(
    source,
    EngineConfig::new(current_version)
        .ed25519_public_key("pfIShU4dEXqPd5ObYNfDBiQWcXozk7estwzTnF9BamQ=") // SUPublicEDKey
        .verification(Verification::Signature),
);
```

- **Signatures** — the enclosure's `sparkle:edSignature` is verified against
  `ed25519_public_key`, the same base64 key Sparkle's `generate_keys` prints
  for `SUPublicEDKey`. Archives signed with Sparkle's `sign_update` or
  `generate_appcast` verify unchanged (pinned by golden tests).
- **Versions** — the crate orders by semver, so it compares
  `sparkle:shortVersionString` (falling back to `sparkle:version`). Pick
  `.version_field(AppcastVersion::BundleVersion)` if your `CFBundleVersion` is
  itself a semantic version.
- **Filters** — `sparkle:channel`, `sparkle:minimumSystemVersion` /
  `maximumSystemVersion` (macOS), `sparkle:hardwareRequirements` (`arm64`), and
  `sparkle:informationalUpdate` are honoured. Enclosures are matched on
  `sparkle:os` (`macos` when absent; WinSparkle's `windows-x64` etc. on
  Windows; `linux-x64` / `linux-arm64` / `linux` on Linux).
- **Not supported** — Sparkle's own delta updates (`sparkle:deltas`), phased rollouts, critical updates, and
  verifying Sparkle 2.10's signed-feed comment; the engine always downloads the
  full archive. On macOS the engine installs only `.dmg` enclosures (a `.zip`
  or `.tar.xz` fails with a clear error); Sparkle itself handles all formats.

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

### Sparkle on macOS

`gpui-updater-sparkle` provides `SparkleDriver`, an `UpdateDriver` that hands
checking, downloading, installing, and relaunching to Sparkle 2. Build the
entity with `Updater::with_driver` instead of `Updater::new` (both adapters
have it); views keep using `UpdateStatus` and the same `check` /
`download_and_install` / `restart` calls.

```rust
use gpui_updater::{AppcastSource, EngineConfig, Updater};
use gpui_updater_sparkle::{SparkleDriver, SparkleOptions};

let updater = cx.new(|cx| match SparkleDriver::load(SparkleOptions::new()) {
    // Running from a bundle that embeds Sparkle.framework.
    Ok(Some(sparkle)) => Updater::with_driver(sparkle, cx),
    // `cargo run`, other platforms, or a misconfigured bundle: same feed,
    // built-in engine.
    _ => Updater::new(
        AppcastSource::new(FEED_URL),
        EngineConfig::new(current_version).ed25519_public_key(SU_PUBLIC_ED_KEY),
        cx,
    ),
});
```

Sparkle is loaded at run time from `Contents/Frameworks/Sparkle.framework`, so
`cargo build` and `cargo test` never need it. The bundle must embed and sign
the framework and set `SUPublicEDKey` and `SUFeedURL` in `Info.plist`; see the
[crate README](crates/gpui-updater-sparkle/README.md) for the checklist, the
call mapping, and the end-to-end smoke test. `Updater::driver_mut::<SparkleDriver>()`
reaches Sparkle's own settings (automatic checks and downloads).

## Platform notes

- **macOS** (built-in engine) replacing an app in `/Applications` needs write
  permission to it.
  An admin who drag-installed the app can replace it without a prompt; a
  standard user cannot. A privilege-escalation fallback (an `osascript … with
  administrator privileges` prompt, as Velopack does) is not yet implemented —
  a permission error just surfaces as a failed update. Sparkle handles this
  case with its installer, which is one reason to use `gpui-updater-sparkle`.
- **Linux and Windows** have no bundle metadata to check, so
  `BundleVersion` and `CodeSigning` only apply on macOS. Sign your artifacts
  and bind signatures to versions there instead.
- Adapters need their GPUI backend's native toolchain (including Xcode/Metal
  where required on macOS). Core does not need GPUI or a graphics toolchain.
- **Linux with registry `gpui` 0.2.2**: libc 0.2.190 removed `ENOATTR`, which
  breaks `xattr` 0.2.3 (pulled in by `gpui_http_client` → `zed-async-tar`). If
  a fresh lockfile picks that libc, hold it back with
  `cargo update -p libc --precise 0.2.189`. `gpui-pre` is not affected.

## Development notes

The workspace defaults to core; `cargo test` is not adapter coverage. Run:

```bash
cargo +1.85.0 test -p gpui-updater-core --all-targets --locked
cargo +1.85.0 test -p gpui-updater-core --features delta --all-targets --locked
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p gpui-updater-core --features delta --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo xtask check-packages
```

The `delta` feature adds `qbsdiff` and a pure-Rust bzip2 (both only used to
create deltas; applying them uses this crate's own bounds-checked bspatch) and
the pure-Rust `lzma-rust2`.
qbsdiff's suffix sorting builds a small C library (`cdivsufsort`), so the
feature needs a C compiler, as `ring` already does.

CI tests core/platform installers on Linux, macOS and Windows, and builds each
registry adapter independently on Linux. Adapter tests use GPUI's seeded test
executor, local one-shot HTTP fixtures, and temporary install roots. They
exercise observer notifications, busy guards, progress timing, errors, and
entity/task lifetime. `gpui-pre` captures the fake platform restart request;
official 0.2.2 discards its path, so exact-path coverage uses a small isolated
handoff boundary. No test restarts a real app or installs over the test binary.

`cargo xtask check-packages` packages all four crates without publishing,
checks archive contents, builds fresh consumers from extracted packages, and
checks the selected normal/build dependency closures (core and Sparkle: no
GPUI; official: no pre; pre: no official). Package locks may list unrelated
platform/optional dependencies; they are not evidence that those dependencies
are compiled. Pass package names to check only those, for example
`cargo xtask check-packages gpui-updater-core gpui-updater-pre` on a Mac
without the Metal toolchain.

Release order: publish `gpui-updater-core` 0.2.0 first; after it is available in
the registry, publish `gpui-updater`, `gpui-updater-pre` and
`gpui-updater-sparkle` 0.2.0. Run normal package verification again before
publishing. A repository tag is not needed for registry resolution; if a
coordinated release tag is created, `v0.2.0` should identify the published
workspace commit. Never rewrite an existing release tag or embed a consumer's
GPUI Git pin here.

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
[OpenLogi]: https://github.com/AprilNEA/OpenLogi
[Sparkle]: https://sparkle-project.org
