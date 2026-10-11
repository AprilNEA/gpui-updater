//! `SparkleDriver::load` from an unbundled binary — which is how `cargo test`
//! runs — must decline with `Ok(None)` so apps fall back to the built-in
//! engine. Runs without libtest's harness so `main` is the main thread, as
//! Sparkle requires.

use gpui_updater_sparkle::{SparkleDriver, SparkleOptions};

fn main() {
    match SparkleDriver::load(SparkleOptions::new()) {
        Ok(None) => println!("load_unbundled: unbundled run declines Sparkle ... ok"),
        other => {
            eprintln!("load_unbundled: expected Ok(None), got {other:?}");
            std::process::exit(1);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let off_main = std::thread::spawn(|| SparkleDriver::load(SparkleOptions::new()).is_ok())
            .join()
            .unwrap_or(true);
        if off_main {
            eprintln!("load_unbundled: loading off the main thread must fail");
            std::process::exit(1);
        }
        println!("load_unbundled: off-main-thread load is rejected ... ok");
    }
}
