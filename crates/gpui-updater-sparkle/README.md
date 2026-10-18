# gpui-updater-sparkle

[Sparkle] 2 update driver for the [`gpui-updater`](../../README.md) GPUI
adapters (`gpui-updater` and `gpui-updater-pre`) on macOS.

Sparkle is the established macOS updater: it installs into `/Applications` for
standard (non-admin) users, installs on quit, applies delta updates, supports
phased rollouts, and ships a familiar update UI. `SparkleDriver` wraps it as a
`gpui_updater_core::UpdateDriver`, so a GPUI app hands its whole update flow
to Sparkle while views keep reading the same `UpdateStatus` from the adapter's
`Updater`. The crate depends only on `gpui-updater-core`, so it works with
either adapter.

## Usage

```rust
// With gpui-updater-pre (without the Cargo alias), import from `gpui_updater_pre`.
use gpui_updater::{AppcastSource, EngineConfig, Updater};
use gpui_updater_sparkle::{SparkleDriver, SparkleOptions};

let updater = cx.new(|cx| match SparkleDriver::load(SparkleOptions::new()) {
    Ok(Some(sparkle)) => Updater::with_driver(sparkle, cx),
    other => {
        if let Err(e) = other {
            tracing::warn!("Sparkle unavailable: {e}");
        }
        // Same appcast, built-in engine: `cargo run`, Linux, Windows.
        Updater::new(
            AppcastSource::new(FEED_URL),
            EngineConfig::new(current_version).ed25519_public_key(SU_PUBLIC_ED_KEY),
            cx,
        )
    }
});
```

`SparkleDriver::load` must run on the main thread. It returns `Ok(None)` — fall
back to the built-in engine — on platforms other than macOS, when the process
is not running from a `.app` bundle, or when the bundle does not embed
`Sparkle.framework`. It returns an error when the bundle embeds Sparkle but is
misconfigured (missing `SUPublicEDKey` / `SUFeedURL`, or not Sparkle 2).
`SparkleDriver` exists on every platform, so call sites need no `cfg`.

### How `Updater` calls map onto Sparkle

| `Updater` call | Sparkle |
| --- | --- |
| `check` | `-[SPUUpdater checkForUpdates]`, which shows Sparkle's UI |
| `download_and_install` | `-[SPUUpdater checkForUpdates]`, which brings the found update forward |
| `restart` | Under `StagedUpdatePolicy::HostRelaunches`, Sparkle's immediate-install block; otherwise `checkForUpdates`, which shows Sparkle's "ready to install" prompt |

Sparkle's updater delegate callbacks become `UpdateStatus` changes:

| Sparkle delegate callback | `UpdateStatus` |
| --- | --- |
| `updater:didFindValidUpdate:` | `Available(version)` |
| `updaterDidNotFindUpdate:error:` | `UpToDate` |
| `updater:willDownloadUpdate:withRequest:` | `Downloading { downloaded: 0, total: None }` |
| `updater:willExtractUpdate:`, `updater:willInstallUpdate:` | `Installing` |
| `updater:willInstallUpdateOnQuit:immediateInstallationBlock:` | `Staged(version)` |
| `updater:didFinishUpdateCycleForUpdateCheck:error:` | `Errored(error)` for real errors; otherwise, if still checking, downloading, or installing, back to `Available(version)` when an update is known, else `Idle` |

Versions come from `displayVersionString` (Sparkle's `shortVersionString`,
falling back to `CFBundleVersion`), by the same rule as `AppcastSource`, so
both backends report the same version for the same feed. User choices
(cancel, "remind me later", declining a check) are not errors. Error messages
carry Sparkle's own text plus the innermost underlying cause, e.g. `… (The
download request URL must use http or https …)`.

The error's `ErrorKind` comes from the Sparkle error code. For example,
`SURunningFromDiskImageError` and `SURunningTranslocated` become `Location`,
`SUSignatureError` becomes `Verification`, and `SUDownloadError` becomes
`Network`. A generic top-level error, such as `SUAppcastError`, takes the
kind of its innermost cause, so a dropped connection reports `Network`.

Sparkle does not expose byte progress to delegates; its own window shows the
progress bar.

### Extra controls

Reach the driver through the entity with
`updater.driver_mut::<SparkleDriver>()` (inside `update`):

- `check_in_background()` — check without UI unless an update is found.
- `check_for_update_information()` — ask the feed without any Sparkle UI; the
  answer arrives as `Available` / `UpToDate`.
- `automatically_checks_for_updates()` / `set_…`,
  `automatically_downloads_updates()` / `set_…` — Sparkle's own settings,
  persisted in the app's user defaults. Only call the setters when the user
  changes the setting; don't mirror them in your own preferences.
- `SparkleOptions::feed_url`, `::channel`, `::framework_path`,
  `::staged_updates` — override `SUFeedURL`, allow `sparkle:channel`s, load the
  framework from elsewhere, and choose who offers the relaunch.

## Bundle checklist

Sparkle is loaded at run time with `NSBundle`, not linked, so `cargo build`
and `cargo test` never need the framework and no rpath is required. To ship:

1. Embed Sparkle 2's `Sparkle.framework` (from the [release
   archive](https://github.com/sparkle-project/Sparkle/releases)) at
   `<App>.app/Contents/Frameworks/Sparkle.framework`, copied with `ditto` to
   keep its symlinks.
2. Set `CFBundleIdentifier`, `CFBundleVersion`, and
   `CFBundleShortVersionString` in `Info.plist`. Sparkle orders updates by
   `CFBundleVersion`, so it must increase with every release.
3. Generate a key pair with Sparkle's `generate_keys` and put the public key in
   `SUPublicEDKey`. Set `SUFeedURL` (HTTPS) or pass
   `SparkleOptions::feed_url`.
4. Sign inside-out with your Developer ID and the hardened runtime, as in
   Sparkle's [sandboxing and code signing
   guide](https://sparkle-project.org/documentation/sandboxing/): the XPC
   services, `Autoupdate`, `Updater.app`, then the framework, then the app.
   Avoid `codesign --deep`. Then notarize.
5. Publish each release with `generate_appcast` (or `sign_update`), which
   writes `sparkle:edSignature` for every archive.

The same appcast also feeds `gpui_updater_core::AppcastSource` on other platforms
and in unbundled runs; configure that engine with the same `SUPublicEDKey`
via `EngineConfig::ed25519_public_key`.

Sandboxed apps need Sparkle's extra XPC and entitlement setup, which this
driver does not configure.

## Testing

- `cargo test -p gpui-updater-sparkle` runs everywhere. `tests/load_unbundled.rs`
  runs on the real main thread and checks that unbundled runs decline Sparkle
  and that loading off the main thread is rejected.
- `scripts/smoke.sh [Sparkle.framework]` (macOS) drives the real framework end
  to end: it builds `examples/smoke.rs` into two throwaway, ad-hoc-signed app
  bundles, serves `crates/gpui-updater-core/tests/fixtures/sparkle-2.10-generate_appcast.xml` on
  `127.0.0.1`, and checks that the older bundle sees `Found(1.5.1)` and the
  newer one `UpToDate`. Without an argument it downloads Sparkle 2.10.0 and
  checks its SHA-256. CI runs it on `macos-latest`.

[Sparkle]: https://sparkle-project.org
