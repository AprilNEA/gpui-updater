# Contributing

## Checks

The workspace defaults to core, so a plain `cargo test` does not cover the
adapters. Run:

```bash
cargo +1.85.0 test -p gpui-updater-core --all-targets --locked
cargo +1.85.0 test -p gpui-updater-core --features delta --all-targets --locked
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p gpui-updater-core --features delta --all-targets --locked -- -D warnings
cargo +1.85.0 test -p gpui-updater-release --features cli,delta --all-targets --locked
cargo clippy -p gpui-updater-release --features cli,delta --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo xtask check-packages
```

CI tests core and the platform installers on Linux, macOS, and Windows, and
builds each registry adapter independently on Linux. Adapter tests use GPUI's
seeded test executor, local one-shot HTTP fixtures, and temporary install
roots. They exercise observer notifications, busy guards, progress timing,
errors, and entity and task lifetime. `gpui-pre` captures the fake platform
restart request; official GPUI 0.2.2 discards its path, so exact-path coverage
uses a small isolated handoff boundary. No test restarts a real app or
installs over the test binary.

`cargo xtask check-packages` packages all five crates without publishing,
checks the archive contents, builds fresh consumers from the extracted
packages, and checks the selected normal and build dependency closures (core,
Sparkle, and release: no GPUI; official: no pre; pre: no official). Package locks may
list unrelated platform or optional dependencies; they are not evidence that
those dependencies are compiled. Pass package names to check only those, for
example `cargo xtask check-packages gpui-updater-core gpui-updater-pre` on a
Mac without the Metal toolchain.

## Dependencies

The `delta` feature adds `qbsdiff` and a pure-Rust bzip2 (both used only to
create deltas; applying them uses this crate's own bounds-checked bspatch) and
the pure-Rust `lzma-rust2`. qbsdiff's suffix sorting builds a small C library
(`cdivsufsort`), so the feature needs a C compiler, as `ring` already does.

On macOS inside some Nix shells, Cargo may pick Nix's clang wrapper as `cc`
and fail to link build scripts with missing system symbols such as
`__Unwind_*` or `_pthread_*`. Point Cargo at Apple's linker for the host
target. The same prefix works for `cargo check` and `cargo clippy`:

```bash
CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/cc cargo test
```

## Documentation

The README is the landing page on GitHub and crates.io. It says what the
crates do and how to start, and links to docs.rs for everything else. Write
usage details in the rustdoc of the item they describe. Write a longer guide
as a Markdown file in the crate's `docs/` directory, and include it in its
module with `#![doc = include_str!("../docs/<guide>.md")]`:

| Guide | Module |
| --- | --- |
| `crates/gpui-updater-core/docs/sources.md` | `gpui_updater_core::source` |
| `crates/gpui-updater-core/docs/verification.md` | `gpui_updater_core::verify` |
| `crates/gpui-updater-core/docs/delta.md` | `gpui_updater_core::delta` |
| `crates/gpui-updater-core/docs/install.md` | `gpui_updater_core::install` |
| `crates/gpui-updater/docs/guide.md` | `gpui_updater` (crate root) |
| `crates/gpui-updater-pre/docs/guide.md` | `gpui_updater_pre` (crate root) |

The two adapter guides differ only in the crate name; change both together.
A guide is compiled with its module, so its Rust code blocks are doctests and
its links are intra-doc links: use `no_run` for examples that touch the
network, and `crate::` paths for items outside the module. Keep each guide
inside its crate directory, so that the published package contains it.
`MIGRATING.md` holds the breaking changes of each release.

Check the docs.rs build with the nightly toolchain:

```bash
RUSTDOCFLAGS="--cfg docsrs -D warnings" cargo +nightly doc -p gpui-updater-core --all-features --no-deps
```

## Releasing

Publish `gpui-updater-core` first. After it is available in the registry,
publish `gpui-updater`, `gpui-updater-pre`, `gpui-updater-sparkle`, and
`gpui-updater-release` at the same version. Run the normal package verification again before publishing. A
repository tag is not needed for registry resolution; if a coordinated release
tag is created, `vX.Y.Z` should identify the published workspace commit. Never
rewrite an existing release tag or embed a consumer's GPUI Git pin here.
