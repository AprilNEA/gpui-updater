
# Enabling deltas

Enable the `delta` feature on whichever crate you depend on
(`gpui-updater-core`, `gpui-updater`, or `gpui-updater-pre`). Nothing else
changes in your app: [`UpdateEngine::download`] tries deltas from the running
version first, and otherwise behaves exactly as before.

```toml
gpui-updater = { version = "0.4", features = ["delta"] }
```

A delta stores each file the cheapest way: copied from the installed tree
(also across renames), patched with bsdiff, or stored whole. Patches and whole
files are LZMA-compressed at the `xz -9e` level. Signed Mach-O images have the
page hashes in their code signature zeroed before diffing and recomputed when
the delta is applied, because those hashes change on every build and do not
diff well. Every file records the SHA-256 of the installed file it starts
from and of the exact file it must produce, so the rebuilt bundle is still
byte-identical to the notarized one. A rebuilt Windows `.exe` is
byte-identical in the same way, so its Authenticode signature stays valid.

# Applying an update

When no delta starts at the running version, the engine chains deltas
(`1.2.0 → 1.2.2 → 1.2.3`). It picks the chain with the fewest bytes, of at
most [`max_delta_hops`] deltas (default 4), and only if that total is smaller
than the full download. Then:

1. Each delta is downloaded and verified under the same checks as the full
   artifact (see [`verify`](crate::verify)): size, `sha256`, signature, and
   the signed version, which is the version the delta produces. Under
   [`Verification::Strict`], an unsigned delta is never downloaded.
2. Its header must say exactly the `from -> to` it was listed with, so a
   validly signed delta cannot be replayed for another upgrade.
3. Each installed file is hashed before use. A user-modified or partially
   updated install is rejected instead of being patched into a broken one.
4. Each rebuilt file is hashed after writing. The next delta of a chain
   starts from the result. The final directory holds the rebuilt bundle or
   executable. It goes through the same payload checks as the full artifact
   (the bundle version and code signature on macOS, the Authenticode
   signature on Windows). Then `install` puts it in place exactly like a
   mounted `.dmg` or a downloaded `.exe`. Nothing is re-signed.

If any step fails (no usable chain, a network error, a bad checksum or
signature, a modified install), the failure is logged with `tracing::warn!`
and the full artifact is downloaded instead. The progress callback then
restarts from zero. Deltas never make an update fail. Turn them off at run
time with [`EngineConfig::deltas(false)`](crate::EngineConfig::deltas).

# Windows

On Windows, deltas rebuild a bare `.exe` release only. The install root (the
running executable, or [`EngineConfig::install_root`]) and the release asset
must both be `.exe` files. Otherwise the engine takes the full download and
does not fetch a delta. So an `.msi` release always takes the full download:
Windows Installer owns the files it installed, and replacing them would
bypass it. The rebuilt executable goes into place with the same rename as a
downloaded `.exe`, so keep calling [`UpdateEngine::remove_leftovers`] at
startup.

# Generating deltas

In release CI, pick the earlier releases to build deltas from, unpack each of
them and the new build, then create one delta per base:

```bash
MAKE="cargo run -p gpui-updater-core --features delta --release --example make_delta --"
$MAKE bases 1.2.2 1.2.1 1.2.0 1.1.9 1.1.8   # earlier releases, newest first
$MAKE old/MyApp.app new/MyApp.app 1.2.2 1.2.3 MyApp-macos-arm64.dmg.from-1.2.2.delta --verify
minisign -S -m MyApp-macos-arm64.dmg.from-1.2.2.delta -t "version:1.2.3"
```

For a Windows `.exe`, pass the two executables, for example
`old\MyApp.exe new\MyApp.exe 1.2.2 1.2.3 MyApp-windows-x64.exe.from-1.2.2.delta`.

`bases` prints the 3 newest earlier releases, plus the releases 2, 4, 8, …
further back (`--direct N` changes the 3). When every release is published
this way, a user `d` releases behind reaches the newest one in at most
⌊log₂ d⌋ + 1 deltas, while each release ships only a handful of them. Older
installs take the full download. `--verify` rebuilds the new build from the
old one with the fresh delta before you publish it. Library users call
[`create`] and [`delta_bases`] directly. Generation keeps whole files in
memory and is CPU-heavy, so it belongs in CI, not in the app.

[`gpui-updater-release`](https://github.com/AprilNEA/gpui-updater/tree/master/crates/gpui-updater-release)
does the same from one tool: `delta bases`, `delta create` (always
verified), and `sign minisign --version`, and its `manifest` command attaches
the deltas it finds to their asset. Its `sparkle delta` makes Sparkle's own
deltas for the `<sparkle:deltas>` of an appcast.

# Publishing

On GitHub Releases, name the delta `<asset>.from-<version>.delta`, for
example `MyApp-macos-arm64.dmg.from-1.2.2.delta`. Its checksum is read from
the same checksums file, and its signature from `<delta name>.minisig`. Delta
files are never picked as the main asset. To chain deltas attached to earlier
releases, let the source see them with [`delta_history`]:

```
use gpui_updater_core::GitHubSource;

let source = GitHubSource::new("AprilNEA", "OpenLogi")
    .asset_contains("macos")
    .asset_contains(".dmg")
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
[`AppcastSource`] lists no deltas, so appcast feeds always get the full
archive.

# Measurements

Measured on [OpenLogi] macOS arm64 releases (19.7 MB `.dmg`, 51.4 MB
unpacked):

| Upgrade | Delta | Share of full download |
| --- | --- | --- |
| 0.8.12 → 0.8.13 | 1.42 MB | 7.2% |
| 0.8.11 → 0.8.13 | 2.36 MB | 12.0% |
| 0.8.11 → 0.8.12 → 0.8.13 (chain, not chosen) | 3.50 MB | 17.8% |

Compared with plain bsdiff patches (bzip2-compressed), LZMA saved 18%, and
normalizing Mach-O code signatures saved another 6%. Generating a delta took
about 18 s with 4 worker threads and a peak of about 1 GB of memory, and the
output is identical on every run. Downloading and applying it locally took
under a second. The installed result was identical to the full 0.8.13
bundle, and both `codesign --verify --deep --strict` and Gatekeeper (`spctl`,
"Notarized Developer ID") accepted it.

On Windows, measured on two signed releases of the GitHub CLI (`gh.exe`
2.62.0 → 2.63.0, 50.5 MB), the delta was 0.90 MB, 1.8% of the new `.exe`.
The rebuilt `gh.exe` was identical to 2.63.0, and its Authenticode signature
(`GitHub, Inc.`) passed [`CodeSigning::MatchInstalled`].

[`UpdateEngine::download`]: crate::UpdateEngine::download
[`UpdateEngine::remove_leftovers`]: crate::UpdateEngine::remove_leftovers
[`EngineConfig::install_root`]: crate::EngineConfig::install_root
[`CodeSigning::MatchInstalled`]: crate::CodeSigning::MatchInstalled
[`max_delta_hops`]: crate::EngineConfig::max_delta_hops
[`Verification::Strict`]: crate::Verification::Strict
[`delta_history`]: crate::GitHubSource::delta_history
[`AppcastSource`]: crate::AppcastSource
[OpenLogi]: https://github.com/AprilNEA/OpenLogi
