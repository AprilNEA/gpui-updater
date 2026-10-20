//! Backend-independent update state shared by the GPUI adapters.

use std::fmt;

use semver::Version;

use crate::error::{Error, ErrorKind};

/// Observable state of an update operation.
///
/// ```text
/// Idle → Checking → UpToDate | Available(v)
///                               → Downloading { downloaded, total } → Installing → Staged(v)
/// any operation → Errored(error)
/// ```
///
/// `Downloading` carries live byte counts. A cancelled download returns to
/// `Available(v)`, not `Errored`.
///
/// Marked `#[non_exhaustive]`: later versions may add states, so match it
/// with a wildcard arm.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
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
    /// The last operation failed. [`UpdateError::kind`] says what kind of
    /// failure it was; its `Display` is the message to show.
    Errored(UpdateError),
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

/// A failed update operation, as the user interface sees it: what kind of
/// failure it was and the message describing it.
///
/// Unlike [`Error`], this is cheap to clone and compare, so it can live in an
/// [`UpdateStatus`]. Build one from an [`Error`] with `From`, or with
/// [`UpdateError::new`] for failures an
/// [`UpdateDriver`](crate::UpdateDriver) reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateError {
    kind: ErrorKind,
    message: String,
}

impl UpdateError {
    /// A failure of `kind`, described by `message`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// What kind of failure this is.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The message describing the failure.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UpdateError {}

impl From<&Error> for UpdateError {
    fn from(error: &Error) -> Self {
        Self::new(error.kind(), error.to_string())
    }
}

impl From<Error> for UpdateError {
    fn from(error: Error) -> Self {
        Self::from(&error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_kind_and_message_of_an_error() {
        let error = UpdateError::from(Error::Http("GET x -> 503".into()));
        assert_eq!(error.kind(), ErrorKind::Network);
        assert_eq!(error.message(), "http error: GET x -> 503");
        assert_eq!(error.to_string(), error.message());
    }
}
