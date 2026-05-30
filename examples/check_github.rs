//! Check a GitHub repository's releases for a newer version, using the blocking
//! engine (no GPUI). Demonstrates the core API end to end.
//!
//! ```text
//! cargo run --example check_github -- AprilNEA OpenLogi 0.0.0
//! ```
//!
//! The asset is auto-selected by the running OS (`.dmg` / `.exe` / `.tar.gz`),
//! so run it on the platform whose artifact you want to resolve.

use gpui_updater::{EngineConfig, GitHubSource, UpdateEngine};
use semver::Version;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let owner = args.next().unwrap_or_else(|| "AprilNEA".to_string());
    let repo = args.next().unwrap_or_else(|| "OpenLogi".to_string());
    let current = args.next().unwrap_or_else(|| "0.0.0".to_string());

    let source = GitHubSource::new(&owner, &repo).with_checksums("SHA256SUMS");
    let engine = UpdateEngine::new(source, EngineConfig::new(Version::parse(&current)?));

    println!("checking {owner}/{repo} against current {current}…");
    match engine.check()? {
        Some(release) => {
            println!(
                "update available: v{} -> {}",
                release.version, release.asset.name
            );
            println!("  download: {}", release.asset.url);
            if let Some(sha) = &release.sha256 {
                println!("  sha256:   {sha}");
            }
            // To apply it:
            //   let artifact = engine.download(&release, |done, total| { /* … */ })?;
            //   engine.install(&artifact)?;
        }
        None => println!("already up to date"),
    }
    Ok(())
}
