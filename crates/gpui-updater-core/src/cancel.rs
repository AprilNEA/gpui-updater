//! Cooperative cancellation of a running download.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// How often a wait checks whether it was cancelled.
pub(crate) const POLL: Duration = Duration::from_millis(50);

/// Asks a running [`UpdateEngine::download_cancellable`] to stop.
///
/// Clones share one flag: hand one clone to the download and call
/// [`cancel`](Self::cancel) on another, from any thread. The download then
/// fails with [`Error::Cancelled`] at its next checkpoint: after each 64 KiB
/// of the body, while it waits to retry, and while it waits for response
/// headers. A body read that has stalled returns within the 30-second stall
/// timeout. Cancelling never interrupts an install.
///
/// A token stays cancelled; use a new one for the next download.
///
/// [`UpdateEngine::download_cancellable`]: crate::UpdateEngine::download_cancellable
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// A token that is not cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask every operation holding a clone of this token to stop.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Whether [`cancel`](Self::cancel) was called on any clone.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Fail with [`Error::Cancelled`] if the token was cancelled.
    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Sleep for `duration`, or fail as soon as the token is cancelled.
    pub(crate) fn sleep(&self, duration: Duration) -> Result<()> {
        let deadline = Instant::now() + duration;
        loop {
            self.check()?;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            thread::sleep(left.min(POLL));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_the_flag() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(token.check().is_ok());
        thread::spawn(move || clone.cancel()).join().unwrap();
        assert!(token.is_cancelled());
        assert!(matches!(token.check(), Err(Error::Cancelled)));
    }

    #[test]
    fn a_sleep_ends_early_when_cancelled() {
        let token = CancelToken::new();
        assert!(token.sleep(Duration::from_millis(1)).is_ok());
        let canceller = token.clone();
        let started = Instant::now();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            canceller.cancel();
        });
        assert!(matches!(
            token.sleep(Duration::from_secs(30)),
            Err(Error::Cancelled)
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
        handle.join().unwrap();
    }
}
