# Migrating

Each section lists the breaking changes of one release and what to change in
your app. The [API documentation](https://docs.rs/gpui-updater-core) describes
the current behaviour.

## Migrating from 0.3

Breaking changes:

- `UpdateStatus::Errored(String)` → `Errored(UpdateError)`. Show the error
  with its `Display` (or `message()`), and branch on `kind()`, an
  `ErrorKind`. See
  [`ErrorKind`](https://docs.rs/gpui-updater-core/latest/gpui_updater_core/enum.ErrorKind.html)
  for what each kind means.
- `DriverEvent::Failed(String)` → `Failed(UpdateError)`. A custom driver
  builds one with `UpdateError::new(ErrorKind::Install, message)`.
- `UpdateStatus` is now `#[non_exhaustive]`: add a wildcard arm to matches.

New, with no change needed: `Updater::cancel`, `CancelToken` with
`UpdateEngine::download_cancellable`, `UpdateDriver::cancel` (defaults to
doing nothing), `Release::published_at` / `release_url`, `HttpOptions`,
`FallbackSource`, `UpdateEngine::check_location` / `install::check_location`,
`CodeSigning::Publisher`, `InstallOptions` / `MsiUi` with
`EngineConfig::install_options` and `install::install_with`,
`UpdateEngine::remove_leftovers` / `install::remove_leftovers`, and
`install::ownership` / `UpdateEngine::ownership` with `Ownership`,
`OwnershipHints`, and `PackageManager`.

Behavior changes:

- Dropping the `Updater` now cancels a running download; before, the
  download ran on to the end.
- A download that fails mid-body continues with a `Range` request when the
  server allows it, instead of starting over.
- With `EngineConfig::download_dir`, an earlier download that still verifies
  is reused instead of downloaded again.
- `download` fails with `Location` or `Permission` before downloading when
  the update could not be installed where the app is. Before, the download
  finished and the install failed.
- On Linux, `current_install_root` returns the `AppImage` file when the app
  runs from inside its mount, not the executable in the read-only mount. An
  install root that is a directory is replaced as a whole.
- On Windows, `CodeSigning::MatchInstalled` (the default) now checks
  Authenticode: an app with a valid signature takes only updates signed by
  the same publisher. Before, nothing was checked on Windows. Opt out with
  `.code_signing(CodeSigning::Off)`.
- The `.msi` apply script relaunches the app after exit code 3010 (installed,
  reboot needed); before, it treated 3010 as a failure. It also writes
  `<package>.log`.

## Migrating from 0.2

- `EngineConfig::minisign_public_key` (the field) became
  `minisign_public_keys: Vec<String>`. The builder method of the same name is
  unchanged and may be called more than once to trust several keys.
- On macOS, downloads are now checked before install: the `.app` must declare
  the release version, and when the installed app is signed by a team, the
  update must be validly signed by the same team. Opt out with
  `.bundle_version(BundleVersion::Ignore)` or `.code_signing(CodeSigning::Off)`.
- `UpdateEngine::download` returns an `Artifact` instead of a `PathBuf`, and
  `UpdateEngine::install` takes `&Artifact`, so the engine only installs a file
  it downloaded and checked. Read the location with `artifact.path()`; call
  `install::install` directly to install some other file on purpose.
- Every request, and every redirect it follows, must use HTTPS. A plain
  `http://` URL fails with `Error::InsecureUrl`, and `check()` rejects a
  release whose artifact or `signature_url` is not HTTPS. Plain HTTP still
  works for loopback hosts (`localhost`, `127.0.0.1`, `[::1]`), for local
  testing.
- A download is cut off once it exceeds the size the release publishes
  (GitHub always publishes one) or, without one,
  `EngineConfig::max_download_size` (4 GiB by default). Manifests, appcasts,
  and GitHub API responses are limited to 10 MiB, signature and checksums
  files to 1 MiB.
- The default download directory is created fresh with owner-only permissions
  (`0700` on Unix), and so is a configured `download_dir` the engine has to
  create. Asset names from release metadata are reduced to a plain file name,
  so a name such as `../x` cannot place the download outside that directory.
- `Release` gains `deltas` (build it with `Release::new` / `with_deltas`), and
  `Error` gains `Payload`, `ManifestExpired`, `Delta`, and `InsecureUrl`.

## Migrating from 0.1

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

## Migrating from 0.0.x

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
