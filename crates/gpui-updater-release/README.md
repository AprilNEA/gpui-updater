# gpui-updater-release

Release-side tooling for [gpui-updater](https://github.com/AprilNEA/gpui-updater).
The client crates read update metadata. This crate writes it: the static JSON
manifest, Sparkle appcasts with `<sparkle:deltas>`, minisign and Sparkle
Ed25519 signatures, and both delta formats. It also embeds, configures, signs,
and checks `Sparkle.framework` in a macOS bundle.

Every writer is tested against the client's own parsers, so a feed that this
crate writes is a feed that `gpui-updater-core` reads. Every signature is
checked with the client-side verifier before the command returns. Every delta
is applied to the old build and compared, file by file, with the new build
before it is kept.

The crate is a library first. The `gpui-updater-release` binary (feature
`cli`) exposes the same steps to shell scripts and CI workflows. Neither pulls
in GPUI.

| Step | Library | Command | Runs on |
| --- | --- | --- | --- |
| Write the static JSON manifest | `ManifestBuilder` | `manifest` | any |
| Write Sparkle appcasts, with deltas | `Appcast`, `AppcastItem` | `appcast` | any |
| Sign with minisign (version-bound) | `MinisignKey` | `sign minisign` | any |
| Sign with Sparkle Ed25519 | `SparkleKey` | `sign sparkle` | any |
| Create keys | `SparkleKey::generate`, `MinisignKey::generate` | `keys …` | any |
| Fetch a pinned, checksummed Sparkle release | `sparkle::SparkleRelease` | `sparkle fetch` | any |
| Embed `Sparkle.framework` and write `Info.plist` | `sparkle::embed`, `SparkleSettings` | `sparkle embed`, `sparkle configure` | macOS |
| Sign Sparkle's code inside out | `sparkle::codesign_sparkle` | `sparkle codesign` | macOS |
| Check an embedded bundle | `sparkle::verify_bundle` | `sparkle verify` | macOS |
| Make a Sparkle delta (`BinaryDelta`), verified | `sparkle::delta::create` | `sparkle delta` | macOS |
| Make a native `files-bsdiff-v1` delta, verified | `native_delta::create` | `delta create` | any (feature `delta`) |
| Choose delta bases | `native_delta::bases` | `delta bases` | any (feature `delta`) |

## Installation

```toml
[dependencies]
gpui-updater-release = "0.4"
# Native deltas as well:
gpui-updater-release = { version = "0.4", features = ["delta"] }
```

The command-line tool:

```bash
cargo install gpui-updater-release --features cli,delta --locked
```

In a Cargo workspace you can also call the library from your own `xtask`.
The library needs Rust 1.85. Sparkle steps call Apple's `codesign`, `hdiutil`,
`ditto`, and Sparkle's `BinaryDelta`, so they run only on macOS. Everything
else runs on any OS.

## Conventions

The defaults match the layout that the main README recommends. Each default is
an option.

| What | Default | Option |
| --- | --- | --- |
| Channel directory | `channels/{channel}` | `--channel-dir`, `FeedLayout::channel_dir` |
| Manifest name | `latest.json` | `--manifest-name` |
| Appcast name | `appcast-{arch}.xml` (one feed per architecture) | `--appcast-name` (drop `{arch}` for one feed) |
| Release directory | `releases/{tag}` (also `{version}`) | `--release-dir` |
| Asset names | `<app>-v<version>-<os>-<arch>.<format>` | `Classifier` rules, `--arch` |
| minisign signature | `<file>.minisig` next to the file | — |
| Native delta | `<asset>.from-<version>.delta` | `native_delta::delta_path` |
| Sparkle delta | `<archive>.from-<CFBundleVersion>.sparkle.delta` | `--out <file>` (must end in `.delta`) |
| Sparkle delta metadata | `<delta>.json` next to the delta | — |
| Sparkle release | 2.10.0, SHA-256 pinned | `--sparkle-version` and `--sparkle-sha256`, `--sparkle-url` |

The default classifier has one rule per format that the engine installs:
`.dmg` and `.zip` for macOS, `.msi`, `.exe`, and `.zip` for Windows, and
`.tar.gz` and `.AppImage` for Linux. Each rule reads the architecture after the
`-<os>-` marker and accepts `arm64`, `aarch64`, `x86_64`, `x64`, and
`universal` (`--arch` replaces the list). Suffixes compare case-insensitively.
Files that no rule matches are skipped, or rejected with `--strict`. Add rules
for other names or formats with `Classifier::rule` and `ArtifactRule`, for
example a `.pkg` with a fixed architecture.

## A release pipeline

The steps below replace a hand-written release script. The commands and
options are the same in GitHub Actions, GitLab CI, or a local shell. Secrets
come from environment variables, and the tool never reads a terminal.

| Variable | Used by | Contents |
| --- | --- | --- |
| `GPUI_UPDATER_BASE_URL` | `manifest`, `appcast` | Public base URL of the update host |
| `SPARKLE_PRIVATE_KEY` | `appcast`, `sign sparkle` | The output of Sparkle's `generate_keys -x` (32-byte seed, base64) |
| `SPARKLE_PUBLIC_KEY` | `sparkle embed`, `appcast` | `SUPublicEDKey`; `appcast` checks that the private key matches it |
| `MINISIGN_SECRET_KEY` | `sign minisign` | The secret key file's contents |
| `MINISIGN_PUBLIC_KEY` | `sign minisign` | The public key clients trust; checked against the secret key |
| `APPLE_SIGNING_IDENTITY` | `sparkle codesign` | Developer ID identity, or `-` for ad-hoc |

### 1. Build the macOS bundle with Sparkle

Run this after your bundler writes `MyApp.app` and before you sign the app.

```bash
gpui-updater-release sparkle embed --app dist/MyApp.app \
  --feed-url "$GPUI_UPDATER_BASE_URL/channels/stable/appcast-arm64.xml" \
  --automatic-checks false --allows-automatic-updates true
gpui-updater-release sparkle codesign --app dist/MyApp.app
codesign --force --options runtime --timestamp \
  --sign "$APPLE_SIGNING_IDENTITY" --entitlements MyApp.entitlements dist/MyApp.app
gpui-updater-release sparkle verify --app dist/MyApp.app \
  --feed-url "$GPUI_UPDATER_BASE_URL/channels/stable/appcast-arm64.xml" \
  --public-key "$SPARKLE_PUBLIC_KEY"
```

`embed` copies the framework from the pinned Sparkle release, removes the XPC
services (keep them with `--keep-xpc-services` in a sandboxed app), adds the
runpath, and writes the `SU*` keys. It refuses a bundle whose
`CFBundleVersion` differs from `CFBundleShortVersionString`, because Sparkle
compares `CFBundleVersion` and a build stamp there breaks ordering and deltas
(allow it with `--allow-any-bundle-version`). `codesign` signs Autoupdate,
Updater.app, and the framework in the order Sparkle documents; you sign the
app last. `verify` checks the framework layout, the `Info.plist` values, the
signatures, and that no XPC service is left.

### 2. Make deltas

Sparkle deltas, from the previous releases' DMGs (or `.app` directories):

```bash
for old in v1.2.2 v1.2.1; do
  gpui-updater-release sparkle delta \
    --old "previous/MyApp-$old-macos-arm64.dmg" \
    --new dist/MyApp.app --archive MyApp-v1.2.3-macos-arm64.dmg --out dist/
done
```

Each delta is named after the old build's `CFBundleVersion`, which is the
value Sparkle sends as `sparkle:deltaFrom`. The tool applies the delta and
compares the result with the new bundle before it keeps the delta. It also
writes `<delta>.json`, which `appcast` reads. Use `--format-version` when old
installs run an older Sparkle, and `--compression` to choose the codec.

Native deltas, for the built-in engine on any OS:

```bash
gpui-updater-release delta bases v1.2.2 v1.2.1 v1.2.0 v1.1.0 v1.0.0
gpui-updater-release delta create --old previous/1.2.2/MyApp.app --new dist/MyApp.app \
  --from 1.2.2 --to 1.2.3 --asset MyApp-v1.2.3-macos-arm64.dmg --out dist/
```

`delta bases` prints the earlier releases to build deltas from: the three
newest (`--direct N` changes the three) plus a skip list, so any install
reaches the newest release in about log₂ steps. This is the policy of
`gpui_updater_core::delta::delta_bases`.

### 3. Sign

```bash
gpui-updater-release sign minisign --version v1.2.3 dist/*.dmg dist/*.msi dist/*.tar.gz dist/*.delta
```

The trusted comment carries `version:1.2.3`, so the client rejects an older,
validly signed file that is served as a newer release (see "Signed version" in
the main README; `VersionBinding::Required` also rejects signatures without
the token). Use `--trusted-comment` for your own comment, and `--password-env` for an
encrypted key.

### 4. Write the feeds

```bash
gpui-updater-release appcast --tag v1.2.3 --dist dist --out-dir feeds \
  --title MyApp --description 'MyApp updates ({channel} channel)' \
  --minimum-system-version 13.0 \
  --release-url https://github.com/me/myapp/releases/tag/v1.2.3 \
  --require-arch arm64 --require-arch x86_64
gpui-updater-release manifest --tag v1.2.3 --dist dist --out feeds/latest.json \
  --app-id com.example.MyApp --min-os macos=13.0 --min-os windows=10.0 \
  --require macos/arm64 --require windows --expires-days 30
```

`appcast` writes one feed per architecture, signs every enclosure and delta
with the Sparkle key, and attaches the `.sparkle.delta` files whose name
matches the archive. It fails when a delta does not produce this release's
`CFBundleVersion`, when `sparkle:version` is not one Sparkle orders
correctly (`--version-parts 3` by default; `--any-version` skips the check),
and when a delta URL does not end in `.delta`.

Sparkle saves a download under the last segment of its URL and applies it
as a delta only when that name ends in `.delta`. Any other name fails with
"No valid unarchiver", and the client downloads the full update after the
delta, so the delta saves nothing. `Appcast::write` and
`DeltaEnclosure::for_file` refuse such a URL (`ensure_sparkle_delta_url`).
gpui-updater-release 0.4.3 and earlier named deltas `.sparkle-delta`;
`appcast` now refuses those files instead of attaching them, so make them
again with 0.4.4 or rename each delta and its `.json` to end in
`.sparkle.delta`.

`manifest` lists every classified file, attaches native deltas to their asset
newest first, and adds `sha256`. It fails when a file has no `.minisig`
(`--signatures if-present` or `ignore` relaxes this) or when a `--require`
target is missing. `--signature-style inline` embeds signatures instead of
linking them. `--ed25519-key-env` adds Sparkle-compatible `ed25519_signature`
values as well.

### 5. Upload

Upload `dist/` to the release directory first and the feeds last. A client
that reads a new feed then always finds its files. The tool prints the public
URL of each feed it writes; use your own uploader (`aws s3 cp`, `rclone`,
`wrangler r2 object put`, GitHub Releases).

## Command reference

Run `gpui-updater-release <command> --help` for the full list. The main
options:

| Command | Options |
| --- | --- |
| `manifest` | `--tag`, `--dist` (repeatable), `--out`, `--download-base`, `--app-id`, `--release-url`, `--notes-file`, `--notes-url`, `--published-at`, `--no-published-at`, `--expires-days`, `--min-os os=version`, `--arch`, `--include-os`, `--exclude-os`, `--require os[/arch]`, `--signatures required\|if-present\|ignore`, `--signature-style url\|inline\|both`, `--ed25519-key-env`, `--strict`, `--no-sha256` |
| `appcast` | `--tag`, `--dist`, `--out-dir`, `--format dmg\|zip`, `--require-arch`, `--key-env`, `--public-key`, `--title`, `--description`, `--language`, `--item-title`, `--bundle-version`, `--short-version`, `--version-parts N\|MIN..MAX`, `--any-version`, `--minimum-system-version`, `--release-url`, `--release-notes-link`, `--notes-file`, `--notes-format markdown\|html\|plain-text`, `--sparkle-channel`, `--critical`, `--critical-below`, `--minimum-autoupdate-version`, `--phased-rollout-interval`, `--no-deltas` |
| layout (both) | `--base-url`, `--channel`, `--channel-dir`, `--manifest-name`, `--appcast-name`, `--release-dir` |
| `sign minisign` | `--key-env`, `--password-env`, `--public-key`, `--version`, `--trusted-comment`, files |
| `sign sparkle` | `--key-env`, files; prints `<signature>  <file>` |
| `keys` | `generate-sparkle`, `generate-minisign --out`, `sparkle-public` |
| `sparkle fetch` | `--cache`, `--sparkle-version`, `--sparkle-sha256`, `--sparkle-url` |
| `sparkle embed` | `--app`, fetch options, settings options, `--keep-xpc-services`, `--no-license` |
| `sparkle configure` | `--app`, settings options |
| settings | `--feed-url`, `--public-key`, `--automatic-checks`, `--allows-automatic-updates`, `--automatically-update`, `--scheduled-check-interval`, `--system-profiling`, `--verify-before-extraction`, `--require-signed-feed`, `--show-release-notes`, `--set KEY=VALUE`, `--allow-any-bundle-version` |
| `sparkle codesign` | `--app`, `--identity`, `--keychain`, `--no-timestamp`, `--no-runtime` |
| `sparkle code-paths` | `--app`; prints the signing order |
| `sparkle verify` | `--app`, `--feed-url`, `--public-key`, `--allow-xpc-services`, `--skip-signatures` |
| `sparkle delta` | `--old`, `--new` (`.app` or `.dmg`), `--out`, `--archive`, fetch options, `--format-version`, `--compression`, `--no-verify` |
| `delta create` | `--old`, `--new`, `--from`, `--to`, `--out`, `--asset`, `--no-verify` |
| `delta bases` | `--direct`, versions |

## Library use

```rust,no_run
use gpui_updater_release::{
    Appcast, AppcastItem, Enclosure, FeedLayout, ManifestBuilder, SparkleKey, write_manifest,
};

# fn main() -> gpui_updater_release::Result<()> {
let layout = FeedLayout::new("https://dl.example.com", "stable");
let manifest = ManifestBuilder::for_layout(&layout, "v1.2.3")
    .app_id("com.example.MyApp")
    .require_os("macos")
    .scan_dir("dist")?
    .build()?;
write_manifest(&manifest, "feeds/latest.json".as_ref())?;

let key = SparkleKey::from_base64(&std::env::var("SPARKLE_PRIVATE_KEY").unwrap())?;
let archive = std::path::Path::new("dist/MyApp-v1.2.3-macos-arm64.dmg");
let url = layout.file_url("v1.2.3", "MyApp-v1.2.3-macos-arm64.dmg");
let item = AppcastItem::new("1.2.3", Enclosure::for_file(archive, url, Some(&key))?)
    .title("MyApp 1.2.3")
    .short_version("1.2.3");
Appcast::new("MyApp", layout.appcast_url("arm64"))
    .item(item)
    .write("feeds/appcast-arm64.xml".as_ref())?;
# Ok(())
# }
```

Every builder method has a matching command-line option. Use the library when
your release logic needs choices that flags cannot express, such as custom
asset rules or several products per release.

## License

MIT OR Apache-2.0. Sparkle is MIT-licensed; `sparkle embed` copies its license
into the embedded framework unless you pass `--no-license`.
