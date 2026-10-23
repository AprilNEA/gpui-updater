//! Release-side tooling for [gpui-updater](https://github.com/AprilNEA/gpui-updater):
//! everything a release job does between "the builds are done" and "the files
//! are uploaded".
//!
//! The client crates read update metadata; this crate writes it, so both
//! sides share one schema and the output is tested against the client's own
//! parsers.
//!
//! | Step | Module | Runs on |
//! | --- | --- | --- |
//! | Recognize release files by name | [`artifact`] | any |
//! | Lay out channels and releases on a static host | [`layout`] | any |
//! | Sign files with minisign or Sparkle Ed25519 | [`sign`] | any |
//! | Write the static JSON manifest | [`manifest`] | any |
//! | Write Sparkle appcasts, with `<sparkle:deltas>` | [`appcast`] | any |
//! | Fetch, embed, configure, sign, and check Sparkle.framework | [`sparkle`] | macOS |
//! | Make Sparkle deltas with `BinaryDelta`, verified | [`sparkle::delta`] | macOS |
//! | Make native `files-bsdiff-v1` deltas, verified (feature `delta`) | `native_delta` | any |
//!
//! Every step takes options with defaults that match the README's
//! recommended setup, and every default can be replaced. The
//! `gpui-updater-release` binary (feature `cli`) exposes the same steps to
//! shell scripts and CI workflows.

pub mod appcast;
pub mod artifact;
mod error;
pub mod layout;
pub mod manifest;
#[cfg(feature = "delta")]
pub mod native_delta;
pub mod sign;
pub mod sparkle;
pub mod tree;
mod util;

#[cfg(test)]
mod test_server;

/// Compiles the Rust examples in the README.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

pub use appcast::{
    Appcast, AppcastItem, Critical, DeltaEnclosure, Enclosure, Informational, ReleaseNotes,
    ensure_sparkle_delta_url, ensure_sparkle_version,
};
pub use artifact::{ArchRule, ArtifactRule, Classified, Classifier};
pub use error::{Error, Result};
pub use gpui_updater_core::manifest::{Manifest, ManifestAsset, ManifestDelta};
pub use layout::FeedLayout;
pub use manifest::{
    DeltaFile, ManifestBuilder, PublishedAt, SignaturePolicy, SignatureStyle, Unmatched,
    read_manifest, write_manifest,
};
pub use sign::{MinisignKey, MinisignOptions, SparkleKey};
pub use util::sha256_file;
