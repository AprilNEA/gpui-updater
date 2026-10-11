//! Repository maintenance tasks, run as `cargo xtask <task>` (the alias lives in
//! `.cargo/config.toml`).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod check_packages;

use anyhow::{Result, bail};

const USAGE: &str = "\
Usage: cargo xtask <task> [args]

Tasks:
  check-packages [PACKAGE...]
      Package the publishable crates without publishing, check every archive's
      files and manifest, then build a fresh consumer of each extracted package
      and check its normal/build dependency closure. With PACKAGE names, check
      only those (for example, skip `gpui-updater` on a Mac without the Metal
      toolchain).
";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("check-packages") => check_packages::run(&args.collect::<Vec<_>>()),
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some(task) => bail!("unknown task `{task}`\n\n{USAGE}"),
        None => bail!("no task given\n\n{USAGE}"),
    }
}
