
The functions here are the checks the engine runs on every download. They
are public so that release tooling can run the same checks. This page
describes the whole trust model of [`UpdateEngine`].

# Transport and staging

Every request, including each redirect it follows, must use HTTPS. Plain HTTP
is accepted only for loopback hosts, for local testing. Downloads land in a
fresh directory that only the current user can access, under a file name
reduced to its last path component. [`UpdateEngine::install`] accepts only
the [`Artifact`] that [`UpdateEngine::download`] returns, so nothing reaches
the installer without passing the checks below. HTTPS protects the transfer;
the checks decide whether the file is trusted.

# Retries and resuming

A download that fails with a transient network error (a dropped connection, a
timeout, a 5xx status) is retried with backoff. When the server named the
body with a strong `ETag` or a `Last-Modified` date, the retry asks for the
rest only, with `Range` and `If-Range`. The file continues only if the server
answers `206` for exactly that range; any other answer starts the file over.
Up to five attempts in a row may fail without moving the download forward.
Resuming works within one `download` call; a new call starts over. The checks
below always run on the whole file.

# Reusing a finished download

With [`EngineConfig::download_dir`], the directory persists between
downloads. If it already holds the release's artifact, of the published size,
and that file passes a published checksum or signature and the payload check,
`download` returns it without fetching anything. So an app that restarts
between downloading and installing does not download again. A file that
nothing published can check is downloaded again.

# The checks

Every downloaded file, the full artifact or a delta, goes through the same
checks, in this order:

1. **Size.** When the release publishes one (`size` in a manifest; GitHub
   always does), a response that announces another size is rejected, and a
   body that grows past it is cut off. Without a published size, the body is
   cut off at [`EngineConfig::max_download_size`] (4 GiB by default). Either
   way, a hostile mirror cannot fill the disk.
2. **SHA-256** against the inline `sha256` or the release's checksums file.
3. **Signatures.** A minisign signature (inline `signature`, else
   `signature_url`) made by any configured public key, and a Sparkle-style
   Ed25519 signature (`ed25519_signature`) checked against
   [`EngineConfig::ed25519_public_key`]. Every signature that is published
   *and* has a configured key is verified; any mismatch fails the download.
4. **Signed version.** If the signature's trusted comment contains
   `version:<semver>`, it must equal the version the file is supposed to be:
   the release version, or for a delta the version it produces. Sign with
   `minisign -S -m App.dmg -t "version:1.2.3"`. This stops an older, validly
   signed artifact from being served as a newer release.
   [`VersionBinding::Required`] also rejects signatures without the token,
   once all your releases carry it. Ed25519 signatures cover the file alone
   and cannot name a version; on macOS, check 5 covers them.
5. **App bundle (macOS).** The `.app` in the update must declare the release
   version as `CFBundleShortVersionString` ([`BundleVersion::MustMatch`],
   where `1.2` matches `1.2.0`). When the installed app is signed by a team,
   the update must pass `codesign --verify --deep --strict` and be signed by
   the same Team ID ([`CodeSigning::MatchInstalled`]). Pin a team with
   [`CodeSigning::TeamId`], or turn either check off. A delta-rebuilt bundle
   that fails these checks falls back to the full download. As in Sparkle,
   a stolen minisign key alone cannot ship someone else's app.
6. **Authenticode (Windows).** When the installed `.exe` has a valid
   Authenticode signature, the downloaded `.exe` or `.msi` must have a valid
   one whose certificate has the same common name (CN), such as
   `Example Corp` ([`CodeSigning::MatchInstalled`], like electron-updater's
   `publisherName`). Pin a publisher with [`CodeSigning::Publisher`]. An
   unsigned installed app is not checked. The check runs PowerShell's
   `Get-AuthenticodeSignature`, so it accepts what Windows trusts, catalog
   signatures included. When PowerShell cannot read a signature, of the
   update or of the installed app, the download fails with
   [`Error::Payload`] ([`ErrorKind::Verification`]) instead of skipping the
   check. Where PowerShell is blocked, turn the check off with
   [`CodeSigning::Off`]. Windows executables need not declare a version, so
   [`BundleVersion`] does not apply.

# Policies

By default ([`Verification::BestEffort`]), checks 2 and 3 are skipped when
their input is absent, so an unsigned release still installs. To **fail
closed**, set a stricter policy:

```
use gpui_updater_core::{EngineConfig, Verification};
# let current_version = gpui_updater_core::Version::new(1, 0, 0);

let config = EngineConfig::new(current_version)
    .minisign_public_key("RWQ…")
    .verification(Verification::Strict);
```

| Policy | Behaviour |
| --- | --- |
| [`BestEffort`](crate::Verification::BestEffort) (default) | Verify what is available; skip missing checks (fails open). |
| [`Off`](crate::Verification::Off) | Skip all checks, including the bundle checks (tests and local development only). |
| [`Checksum`](crate::Verification::Checksum) | Require a matching SHA-256. |
| [`Signature`](crate::Verification::Signature) | Require a public key **and** an advertised signature it can check (minisign or Ed25519). |
| [`Strict`](crate::Verification::Strict) | Require both a valid signature and a matching SHA-256. |

Under `Signature` and `Strict`, a release that cannot be verified is rejected
by [`UpdateEngine::check`], before it is ever reported as available, and
again before a download is verified.

For the strongest setup, combine `Verification::Strict`, a `version:` token
in every signature with [`VersionBinding::Required`], and, for static
manifests, a signed manifest with `expires`.

# Key rotation

Call [`EngineConfig::minisign_public_key`] more than once: a signature by any
configured key is accepted. Ship a release that trusts both the old and the
new key, then start signing with the new one.

# Signed manifests

Signed artifacts prove what a file is, but not that the manifest that lists
them is current. Whoever controls the host could keep serving an old
manifest, so users never see a security fix (a *freeze* attack).
[`StaticManifestSource`] can require the manifest itself to be signed:

```
use gpui_updater_core::StaticManifestSource;

let source = StaticManifestSource::new("https://dl.example.com/channels/stable/latest.json")
    .minisign_public_key("RWQ…"); // verifies latest.json against latest.json.minisig
```

The bytes are verified before they are parsed.
[`signature_url`](crate::StaticManifestSource::signature_url) overrides the default
`<url>.minisig`. With a top-level `"expires"` (RFC 3339) in the signed
manifest, `check()` fails with [`Error::ManifestExpired`] after that time
instead of reporting "up to date". This bounds how long a replay works.
Re-sign the manifest on a schedule shorter than its lifetime, for example
weekly with a two-week `expires`. Rollback is never possible: `check()`
offers only versions newer than the running one.

[`UpdateEngine`]: crate::UpdateEngine
[`UpdateEngine::install`]: crate::UpdateEngine::install
[`UpdateEngine::download`]: crate::UpdateEngine::download
[`UpdateEngine::check`]: crate::UpdateEngine::check
[`Artifact`]: crate::Artifact
[`EngineConfig::download_dir`]: crate::EngineConfig::download_dir
[`EngineConfig::max_download_size`]: crate::EngineConfig::max_download_size
[`EngineConfig::ed25519_public_key`]: crate::EngineConfig::ed25519_public_key
[`EngineConfig::minisign_public_key`]: crate::EngineConfig::minisign_public_key
[`VersionBinding::Required`]: crate::VersionBinding::Required
[`BundleVersion`]: crate::BundleVersion
[`BundleVersion::MustMatch`]: crate::BundleVersion::MustMatch
[`CodeSigning::MatchInstalled`]: crate::CodeSigning::MatchInstalled
[`CodeSigning::TeamId`]: crate::CodeSigning::TeamId
[`CodeSigning::Publisher`]: crate::CodeSigning::Publisher
[`CodeSigning::Off`]: crate::CodeSigning::Off
[`Error::Payload`]: crate::Error::Payload
[`Error::ManifestExpired`]: crate::Error::ManifestExpired
[`ErrorKind::Verification`]: crate::ErrorKind::Verification
[`Verification::BestEffort`]: crate::Verification::BestEffort
[`StaticManifestSource`]: crate::StaticManifestSource
[`Verification::Off`]: crate::Verification::Off
[`Verification::Checksum`]: crate::Verification::Checksum
[`Verification::Signature`]: crate::Verification::Signature
[`Verification::Strict`]: crate::Verification::Strict
