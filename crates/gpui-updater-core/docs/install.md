
# Strategies

The engine calls [`install_with`] with the verified artifact and the install
root. The strategy depends on the platform:

| Platform | Artifact | What happens |
| --- | --- | --- |
| macOS | `.dmg` | Mount it, copy the new (already notarized) `.app` onto the volume of the current bundle with `ditto`, then swap it into place. Nothing is re-signed at run time: the new bundle carries its own signature. |
| Linux | `.tar.gz` | Extract it and atomically replace the executable. When the install root is a directory (an `app.app/` with `bin/` and `lib/`, as Zed ships), replace the whole directory. |
| Linux | `.AppImage` | Replace the `.AppImage` file that the app runs from. |
| Windows | bare `.exe` | Rename the running executable to `.old.exe` and put the new one in its place. |
| Windows | `.exe` rebuilt from deltas | The same as a bare `.exe`. With the `delta` feature, the engine rebuilds it only when the install root and the release asset are both `.exe` files. |
| Windows | `.msi` | Stage the package, and run msiexec after the app exits, through the restart path; the app then relaunches. |

On Windows, msiexec runs with `/passive /norestart` by default; [`MsiUi`] in
[`InstallOptions`] picks `/qb` or `/quiet` instead. msiexec logs to
`<package>.log`, and exit code 3010 (reboot needed) counts as success. Call
[`UpdateEngine::remove_leftovers`] (or [`remove_leftovers`]) at startup to
delete the `.old.exe` that a bare-`.exe` update leaves behind. Restart
Manager integration for locked sibling DLLs is not implemented.

# Install location

[`current_install_root`] finds what an update replaces: the `.app` bundle on
macOS, the executable or AppImage on Linux, and the executable on Windows.
Set [`EngineConfig::install_root`] when that is wrong, for example for a
Linux app directory.

Before it downloads anything, [`UpdateEngine::download`] calls
[`check_location`]. It fails with [`ErrorKind::Location`] for an App
Translocation path or a read-only volume on macOS, and with
[`ErrorKind::Permission`] for an install folder that the user cannot write
to. So the user learns that the update cannot be installed before waiting for
the download. Call [`UpdateEngine::check_location`] earlier, for example to
grey out the update button.

# Installs a package manager owns

An app that Homebrew, apt, or Scoop installed should not replace itself. The
package manager then reports a version it did not install, and its next
upgrade can undo the update. Call [`ownership`] (or
[`UpdateEngine::ownership`]) at startup. When it is not
[`Ownership::SelfUpdate`], show how to update instead of an update button.

```no_run
use gpui_updater_core::{Ownership, OwnershipHints, current_install_root, install};

// A distribution that builds the app sets MYAPP_UPDATE_EXPLANATION, as Zed's
// packagers set ZED_UPDATE_EXPLANATION.
let hints = OwnershipHints::new().explanation(option_env!("MYAPP_UPDATE_EXPLANATION"));
let owner = current_install_root()
    .map_or(Ownership::SelfUpdate, |root| install::ownership(&root, &hints));
if !owner.is_self_update() {
    // "installed by Homebrew (myapp)", and "brew upgrade myapp"
    println!("{owner}: {}", owner.update_command().unwrap_or_default());
}
```

The check does not block downloads; the app decides. It looks, in order, at:

| Source | Recognized as |
| --- | --- |
| [`OwnershipHints::explanation`], when not blank | [`Ownership::Packager`] |
| `/nix/store/…` | Nix |
| `/snap/<name>/…` | Snap |
| `/app/…` with `FLATPAK_ID`, or `…/flatpak/app/<id>/…` | Flatpak |
| `<prefix>/Cellar/<formula>/…`, prefix from `HOMEBREW_PREFIX` or the defaults | Homebrew |
| `…\scoop\apps\<app>\…`, or under `SCOOP` or `SCOOP_GLOBAL` | Scoop |
| `<ChocolateyInstall>\lib\<package>\…` | Chocolatey |
| `…\Microsoft\WinGet\Packages\<id>_<source>\…` | winget |
| `C:\Program Files\WindowsApps\…` | Microsoft Store |
| `Contents/_MASReceipt/receipt` in the bundle | Mac App Store |
| A `Caskroom/<token>/<version>/<App>.app` symlink back to the app | Homebrew cask |
| `dpkg-query --search`, then `rpm -qf`, then `pacman -Qo` (Linux) | apt, rpm, pacman |

The Linux queries run with `LC_ALL=C` for at most two seconds each. A missing
command, a timeout, or an unreadable folder does not claim the install, so
the result is then `SelfUpdate`. Turn the queries off with
[`OwnershipHints::query_package_database(false)`](OwnershipHints::query_package_database).

# Platform notes

- **macOS**: replacing an app in `/Applications` needs write permission to
  it. An admin who drag-installed the app can replace it without a prompt; a
  standard user cannot. A privilege-escalation fallback (an `osascript …
  with administrator privileges` prompt, as Velopack does) is not
  implemented. Instead, [`check_location`] reports the missing permission as
  [`ErrorKind::Permission`] before anything is downloaded. Sparkle handles
  this case with its installer, which is one reason to use
  `gpui-updater-sparkle`.
- **Linux app directories**: [`current_install_root`] cannot know that the
  executable belongs to a larger directory, so set
  [`EngineConfig::install_root`] to that directory. The `.tar.gz` may hold
  the directory itself, a single wrapper folder such as `app-1.2.0/`, or the
  directory's contents. When the running executable is inside the directory,
  the update must hold an executable at the same relative path, and that path
  is the restart path. A symlink to the directory is kept, and the directory
  it points to is replaced.
- **Linux AppImage**: the root is `$APPIMAGE` only when the running
  executable is inside `$APPDIR`. A program started from another AppImage,
  such as a terminal, inherits both variables; this check keeps it from
  replacing that other AppImage.
- **Linux** has no bundle metadata or platform code signature to check, so
  [`BundleVersion`] and [`CodeSigning`] do not apply there; on Windows, only
  `CodeSigning` does. Sign your artifacts and bind signatures to versions
  instead (see [`verify`](crate::verify)).

[`UpdateEngine::download`]: crate::UpdateEngine::download
[`UpdateEngine::check_location`]: crate::UpdateEngine::check_location
[`UpdateEngine::ownership`]: crate::UpdateEngine::ownership
[`UpdateEngine::remove_leftovers`]: crate::UpdateEngine::remove_leftovers
[`EngineConfig::install_root`]: crate::EngineConfig::install_root
[`ErrorKind::Location`]: crate::ErrorKind::Location
[`ErrorKind::Permission`]: crate::ErrorKind::Permission
[`BundleVersion`]: crate::BundleVersion
[`CodeSigning`]: crate::CodeSigning
