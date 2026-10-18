
# Choosing a source

Use one of the built-in sources, or implement [`UpdateSource`] for your own
release service:

| Source | Best for | How it works |
| --- | --- | --- |
| [`GitHubSource`] | Existing GitHub Releases pipelines | Calls the GitHub Releases API, selects a matching asset, and can read `SHA256SUMS` and `.minisig` assets. |
| [`StaticManifestSource`] | Cloudflare R2, S3, MinIO, B2, CDNs, static hosting | Fetches a static `latest.json`, selects an asset from `assets[]`, then downloads the asset URL directly. |
| [`AppcastSource`] | Apps that publish (or want to publish) a Sparkle appcast | Fetches an appcast, picks the newest applicable `<item>` and the enclosure for this platform, and carries its `sparkle:edSignature`. |
| [`FallbackSource`] | A mirror or second host for when the primary is down | Asks its sources in order and returns the release from the first one that answers. When all fail, returns the primary's error. |

`FallbackSource::new(primary).or(mirror)` does not compare its sources: a
primary that answers wins, even if the mirror knows a newer release. Each
source verifies what it fetches with its own settings, such as a manifest's
signing keys. The engine verifies the artifact the same way whichever source
named it.

Every source fills [`Release::published_at`] and [`Release::release_url`]
when its metadata has them, for a "published on" line and a "View release"
link. Both are for display only. A date that does not parse, or a URL that is
not `http(s)`, is dropped; the check does not fail.

# GitHub Releases

[`GitHubSource`] reads the latest release of a repository, or the newest
pre-release too with
[`include_prereleases(true)`](GitHubSource::include_prereleases). It picks
the asset whose name contains every [`asset_contains`] substring (matching is
case-insensitive). With [`with_checksums`], it reads the asset's SHA-256 from
a checksums file in the same release; with [`with_minisig`], it reads the
detached signature `<asset>.minisig` (another suffix with
[`signature_suffix`]). [`token`] authenticates against private repositories
and higher rate limits. The release's `published_at` and `html_url` fill the
display fields.

```no_run
use gpui_updater_core::{EngineConfig, GitHubSource, UpdateEngine, Version};

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
    let artifact = engine.download(&release, |_done, _total| { /* progress */ })?;
    engine.install(&artifact)?;
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

With the `delta` feature, [`delta_history`] also lists the deltas attached to
earlier releases, so the engine can chain them; see `crate::delta`.

[`asset_contains`]: GitHubSource::asset_contains
[`with_checksums`]: GitHubSource::with_checksums
[`with_minisig`]: GitHubSource::with_minisig
[`signature_suffix`]: GitHubSource::signature_suffix
[`token`]: GitHubSource::token
[`delta_history`]: GitHubSource::delta_history

# Static manifests

Use [`StaticManifestSource`] when your release metadata is hosted as a static
JSON file on Cloudflare R2, AWS S3, MinIO, Backblaze B2, GitHub Pages, or any
normal HTTPS file server. The updater does not speak the S3 API: it fetches
JSON and downloads direct artifact URLs. This keeps credentials and provider
SDKs out of the app. Upload artifacts however you like, then publish a small
JSON pointer.

```
use gpui_updater_core::{EngineConfig, StaticManifestSource, UpdateEngine};
# let current_version = gpui_updater_core::Version::new(1, 0, 0);

let source = StaticManifestSource::new("https://dl.example.com/channels/stable/latest.json")
    .os("macos")
    .arch("arm64")
    .format("dmg");

let engine = UpdateEngine::new(source, EngineConfig::new(current_version));
```

## Directory layout

Manifest v1 describes one latest release and a flat list of downloadable
assets. Put channel pointers at mutable URLs such as
`/channels/stable/latest.json`, but keep artifact URLs versioned and
immutable:

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

## Format

`schema_version`, `version`, `assets[].name`, and `assets[].url` are the only
required fields. All other fields are optional, and unknown fields are
ignored.

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

| Field | Meaning |
| --- | --- |
| `size`, `sha256` | Checked after download. |
| `signature` | An inline minisign signature. Used instead of `signature_url` when both are present. |
| `signature_url` | A detached minisign signature file. |
| `ed25519_signature` | A base64 Ed25519 signature, as Sparkle's `sign_update` prints it. |
| `expires` | Matters only for a signed manifest: after this time, the check fails. |
| `published_at` (RFC 3339), `release_url` | Fill [`Release::published_at`] and [`Release::release_url`]. |
| `deltas` | Binary deltas from older versions; see [`StaticManifestSource`]. |

The [`verify`](crate::verify) module describes every check, including signed
manifests.

## Asset selection

Asset selection is explicit when you use [`os`](StaticManifestSource::os),
[`arch`](StaticManifestSource::arch), and
[`format`](StaticManifestSource::format). You can also add filename substring
filters with [`asset_contains`](StaticManifestSource::asset_contains), or
replace them completely with
[`asset_patterns`](StaticManifestSource::asset_patterns). Matching is
case-insensitive.

If no selector is configured, the source falls back to a platform extension
guess: `.dmg` on macOS, `.exe` on Windows, and `.tar.gz` on Linux and other
platforms. An AppImage app selects its asset with
`asset_patterns(vec![".AppImage".into()])`. An installer-shipped Windows app
selects its MSI with `.format("msi")`; the install step stages it and applies
it with msiexec on restart.

## Release flow

For R2- or S3-style hosting:

1. Build and sign or notarize the platform artifacts.
2. Upload the artifacts to an immutable, versioned prefix such as
   `/releases/v1.2.3/`.
3. Generate SHA-256 hashes and, optionally, minisign signatures.
4. Upload `latest.json` to a mutable channel prefix such as
   `/channels/stable/latest.json` after all artifacts are in place. If the app
   verifies the manifest, upload `latest.json.minisig` with it, and re-sign it
   before its `expires` time.

[`gpui-updater-release`](https://github.com/AprilNEA/gpui-updater/tree/master/crates/gpui-updater-release)
does steps 3 and 4. `sign minisign --version v1.2.3` writes version-bound
`.minisig` files. `manifest` writes `latest.json` from the release directory
with sizes, hashes, signature URLs, deltas, OS minimums, and an optional
`expires`, and fails when a required target is missing.

# Sparkle appcasts

[`AppcastSource`] reads the RSS feed that [Sparkle] (macOS) and WinSparkle
(Windows) consume. So one feed can serve a Sparkle-driven macOS build and the
built-in engine everywhere else:

```
use gpui_updater_core::{AppcastSource, EngineConfig, UpdateEngine, Verification};
# let current_version = gpui_updater_core::Version::new(1, 0, 0);

let source = AppcastSource::new("https://dl.example.com/appcast.xml")
    .channel("beta"); // optional: also offer `sparkle:channel` "beta"

let engine = UpdateEngine::new(
    source,
    EngineConfig::new(current_version)
        .ed25519_public_key("pfIShU4dEXqPd5ObYNfDBiQWcXozk7estwzTnF9BamQ=") // SUPublicEDKey
        .verification(Verification::Signature),
);
```

- **Signatures**: the enclosure's `sparkle:edSignature` is verified against
  [`ed25519_public_key`](crate::EngineConfig::ed25519_public_key), the same
  base64 key that Sparkle's `generate_keys` prints for `SUPublicEDKey`.
  Archives signed with Sparkle's `sign_update` or `generate_appcast` verify
  unchanged.
- **Versions**: the crate orders releases by semver, so it compares
  `sparkle:shortVersionString`, or `sparkle:version` when that is missing.
  Pick `.version_field(AppcastVersion::BundleVersion)` if your
  `CFBundleVersion` is itself a semantic version.
- **Filters**: `sparkle:channel`, `sparkle:minimumSystemVersion` and
  `maximumSystemVersion` (macOS), `sparkle:hardwareRequirements` (`arm64`),
  and `sparkle:informationalUpdate` are honoured. Enclosures are matched on
  `sparkle:os`: `macos` when absent, WinSparkle's `windows-x64` and others on
  Windows, and `linux-x64`, `linux-arm64`, or `linux` on Linux.
- **Display fields**: the item's `<pubDate>` (RFC 2822) becomes
  [`Release::published_at`], and its `<link>` becomes
  [`Release::release_url`] when it is an `http(s)` URL.
- **Not supported**: Sparkle's own delta updates (`sparkle:deltas`), phased
  rollouts, critical updates, and verifying Sparkle 2.10's signed-feed
  comment. The engine always downloads the full archive. On macOS the engine
  installs only `.dmg` enclosures; a `.zip` or `.tar.xz` fails with a clear
  error. Sparkle itself handles all formats, and applies the deltas.

To write the feed without Xcode or Sparkle's `generate_appcast`, use
`gpui-updater-release appcast`. It writes one feed per architecture with
`sparkle:edSignature` on every enclosure. `gpui-updater-release sparkle delta`
runs Sparkle's `BinaryDelta`, applies the result to check it, and names the
file after the old `CFBundleVersion`, which Sparkle sends as
`sparkle:deltaFrom`; `appcast` then lists it under `<sparkle:deltas>`. Sparkle
uses a delta only when the installed `CFBundleVersion` equals `deltaFrom`, so
keep `CFBundleVersion` stable and increasing.

[Sparkle]: https://sparkle-project.org/

# Proxies and request headers

By default every request uses the proxy from the environment (`ALL_PROXY`,
`HTTPS_PROXY`, `HTTP_PROXY`, or their lowercase forms) and adds no headers.
[`HttpOptions`] changes this. Give it to the source for metadata requests,
and to [`EngineConfig::http`] for the artifact, deltas, and their checksum
and signature files:

```
use gpui_updater_core::{EngineConfig, HttpOptions, StaticManifestSource};
# let current_version = gpui_updater_core::Version::new(1, 0, 0);

let http = HttpOptions::new()
    .header("authorization", "Bearer s3cr3t")
    .proxy("http://proxy.internal:3128"); // or .no_proxy()
let source = StaticManifestSource::new("https://updates.example.com/latest.json")
    .http(http.clone());
let config = EngineConfig::new(current_version).http(http);
```

Put a credential in `authorization`. A redirect drops it (and `cookie`), so
the storage host behind a release asset never sees it. Every other header
also goes to redirect targets. A header that the source sets itself, such as
the `authorization` from [`GitHubSource::token`], takes precedence. HTTPS
requests are tunnelled through the proxy, so TLS still ends at the update
server. The `Debug` output of [`HttpOptions`] shows header names only, never
values or the proxy URL.

[`HttpOptions`]: crate::HttpOptions
[`EngineConfig::http`]: crate::EngineConfig::http
[`Release::published_at`]: crate::Release::published_at
[`Release::release_url`]: crate::Release::release_url
