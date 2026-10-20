
# Driving the entity

Build the entity once, observe it to re-render on status changes, and call
its methods from your buttons. [`Updater::status`] says which call makes
sense:

```no_run
use gpui::{AppContext as _, Context, Entity};
use gpui_updater_pre::{EngineConfig, GitHubSource, UpdateStatus, Updater, Version};

struct UpdatePanel {
    updater: Entity<Updater>,
}

impl UpdatePanel {
    fn new(cx: &mut Context<Self>) -> Self {
        let source = GitHubSource::new("AprilNEA", "OpenLogi")
            .asset_contains("macos")
            .asset_contains(".dmg")
            .with_checksums("SHA256SUMS");
        let version = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let updater = cx.new(|cx| Updater::new(source, EngineConfig::new(version), cx));

        // Re-render on status changes.
        cx.observe(&updater, |_, _, cx| cx.notify()).detach();
        Self { updater }
    }

    // One button whose action follows the status.
    fn on_update_button(&mut self, cx: &mut Context<Self>) {
        self.updater.update(cx, |updater, cx| match updater.status() {
            UpdateStatus::Available(_) => updater.download_and_install(cx),
            UpdateStatus::Downloading { .. } => updater.cancel(cx),
            UpdateStatus::Staged(_) => updater.restart(cx),
            _ => updater.check(cx),
        });
    }
}
```

There is no background polling. Call [`Updater::check`] on a timer if you
want one.

# Status and errors

[`UpdateStatus`] moves through `Idle → Checking → UpToDate | Available(v) →
Downloading { downloaded, total } → Installing → Staged(v)`, or to
`Errored(error)` from any operation. `Downloading` carries live byte counts;
`total` is `None` when the server omits `Content-Length`. `UpdateStatus` is
`#[non_exhaustive]`, so match it with a wildcard arm.

`Errored` carries an [`UpdateError`]. Its `Display` is the message to show,
and [`UpdateError::kind`] returns an [`ErrorKind`] that tells the user
interface what to do; the [`ErrorKind`] page has a table of typical
reactions.

`ErrorKind::Location` and `ErrorKind::Permission` come before anything is
downloaded: [`Updater::download_and_install`] fails early when the app runs
from an App Translocation path or a disk image, or when the user may not
write to the install folder. Call [`UpdateEngine::check_location`] yourself
to tell the user before offering the download. The [`install`] module
describes the checks and the platform install strategies.

A check that finds no newer release, or fails, keeps the previous
[`Updater::available`] release.

# Cancelling

[`Updater::cancel`] stops a download. The status returns to `Available(v)`,
not `Errored`, and the partial file is removed. The download stops at its
next checkpoint: after each 64 KiB of the body, during a retry wait, or while
it waits for response headers. A body read that has stalled can take up to
the 30-second stall timeout. An install that has started cannot be
cancelled. Without the entity, pass a [`CancelToken`] to
[`UpdateEngine::download_cancellable`].

Keep the entity alive while updating. Dropping it cancels a running download,
as `cancel` does, but does not interrupt an install that has started or roll
it back. A completed download may remain on disk.

# Sparkle on macOS

`gpui-updater-sparkle` provides `SparkleDriver`, an [`UpdateDriver`] that
hands checking, downloading, installing, and relaunching to Sparkle 2. Build
the entity with [`Updater::with_driver`] instead of [`Updater::new`]. Views
keep using [`UpdateStatus`] and the same `check`, `download_and_install`,
and `restart` calls.

```rust,ignore
use gpui_updater_pre::{AppcastSource, EngineConfig, Updater};
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

[`Updater::driver_mut`] reaches Sparkle's own settings, such as automatic
checks and downloads. The [`gpui-updater-sparkle`] documentation lists the
bundle requirements and how each call maps to Sparkle.

[`gpui-updater-sparkle`]: https://docs.rs/gpui-updater-sparkle
