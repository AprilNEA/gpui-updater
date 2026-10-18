//! Sparkle on the release side: the framework and tools from a pinned Sparkle
//! release, embedding and configuring them in an app bundle, signing order,
//! bundle checks, and Sparkle-format deltas.
//!
//! A typical macOS release job:
//!
//! ```no_run
//! use gpui_updater_release::sparkle::{self, BundleCheck, CodesignOptions, EmbedOptions, SparkleRelease, SparkleSettings};
//!
//! let app = std::path::Path::new("target/release/bundle/osx/App.app");
//! let dist = SparkleRelease::pinned().fetch("target/sparkle".as_ref())?;
//! sparkle::embed(app, &dist, &EmbedOptions::default())?;
//! let settings = SparkleSettings::new("https://dl.example.com/appcast-arm64.xml", "dwOe6ZzIDrpl1SpM+iAu1my3EquhGspSGlWBlq1zkdI=")?
//!     .enable_automatic_checks(false);
//! settings.apply(&app.join("Contents/Info.plist"))?;
//! sparkle::codesign_sparkle(app, &CodesignOptions::new("Developer ID Application: Me (TEAMID)"))?;
//! // ... sign the app itself, then:
//! sparkle::verify_bundle(app, &BundleCheck::new(Some(settings)))?;
//! # Ok::<_, gpui_updater_release::Error>(())
//! ```

pub mod bundle;
pub mod delta;
pub mod distribution;
pub mod dmg;

pub use bundle::{
    BundleCheck, CodesignOptions, EmbedOptions, FRAMEWORK_VERSION, SparkleSettings,
    check_bundle_version, code_inside_out, codesign_sparkle, embed, framework, team_identifier,
    verify_bundle,
};
pub use delta::{DeltaOptions, OldApp, SparkleDelta};
pub use distribution::{SparkleDistribution, SparkleRelease};
pub use dmg::extract_app;
