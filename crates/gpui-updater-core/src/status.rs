//! Backend-independent update state shared by the GPUI adapters.

use semver::Version;

/// Observable state of an update operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UpdateStatus {
    /// Nothing has been checked yet.
    #[default]
    Idle,
    /// A check is in flight.
    Checking,
    /// The running version is the latest.
    UpToDate,
    /// A newer version is available to download and install.
    Available(Version),
    /// The update artifact is downloading. `total` is `None` until/unless the
    /// server reports a `Content-Length`.
    Downloading { downloaded: u64, total: Option<u64> },
    /// The download is verified and being swapped into place.
    Installing,
    /// The update is installed and ready to launch on restart.
    Staged(Version),
    /// The last operation failed.
    Errored(String),
}

impl UpdateStatus {
    /// Whether an operation is currently in flight.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Downloading { .. } | Self::Installing
        )
    }
}
