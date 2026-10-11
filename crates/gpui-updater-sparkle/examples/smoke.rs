//! Smoke test for the Sparkle driver without GPUI.
//!
//! Run it from an app bundle that embeds `Sparkle.framework` and sets
//! `SUFeedURL` / `SUPublicEDKey` (and `SUEnableAutomaticChecks = NO`, so
//! Sparkle does not ask for permission). It asks the feed for update
//! information without showing UI, prints every driver event, and exits after
//! Sparkle settles.

fn main() {
    #[cfg(target_os = "macos")]
    smoke::run();
    #[cfg(not(target_os = "macos"))]
    eprintln!("Sparkle is macOS-only");
}

#[cfg(target_os = "macos")]
mod smoke {
    use std::process::exit;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use gpui_updater_core::{DriverEvent, DriverEvents, UpdateDriver};
    use gpui_updater_sparkle::{SparkleDriver, SparkleOptions};
    use objc2_foundation::{NSDate, NSRunLoop};

    pub(super) fn run() {
        let mut driver = match SparkleDriver::load(SparkleOptions::new()) {
            Ok(Some(driver)) => driver,
            Ok(None) => {
                eprintln!("smoke: not running from a bundle that embeds Sparkle.framework");
                exit(2);
            }
            Err(e) => {
                eprintln!("smoke: {e}");
                exit(1);
            }
        };
        println!("smoke: loaded {driver:?}");

        let (tx, rx) = mpsc::channel();
        driver.start(DriverEvents::new(move |event| {
            // The receiver lives until `run` returns; a failed send only means
            // the smoke test is already exiting.
            if tx.send(event).is_err() {
                eprintln!("smoke: event after exit");
            }
        }));
        println!(
            "smoke: automatic checks = {:?}, automatic downloads = {:?}, can check = {}",
            driver.automatically_checks_for_updates(),
            driver.automatically_downloads_updates(),
            driver.can_check_for_updates()
        );
        driver.check_for_update_information();

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.1));
            while let Ok(event) = rx.try_recv() {
                println!("smoke: event {event:?}");
                if matches!(event, DriverEvent::Settled) {
                    return;
                }
            }
        }
        eprintln!("smoke: timed out waiting for Sparkle");
        exit(1);
    }
}
