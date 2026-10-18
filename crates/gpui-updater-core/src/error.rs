//! Error types for the updater.

use std::io;

/// Errors produced while checking for, downloading, verifying, or installing an
/// update.
///
/// Use [`Error::kind`] to decide what to tell the user; the variants carry the
/// details.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A network request failed or returned a non-success status.
    #[error("http error: {0}")]
    Http(String),

    /// A URL would be fetched without TLS: it, or a redirect it led to, is not
    /// `https://`. Plain HTTP is accepted only for loopback hosts
    /// (`localhost`, `127.0.0.0/8`, `::1`).
    #[error("refusing to fetch {0} without HTTPS (plain HTTP is allowed for loopback hosts only)")]
    InsecureUrl(String),

    /// Release metadata could not be parsed (bad JSON, missing fields, or an
    /// unparseable version tag).
    #[error("failed to parse release metadata: {0}")]
    Parse(String),

    /// No release asset matched the running platform.
    #[error("no release asset matched the current platform ({target_os}/{target_arch})")]
    NoMatchingAsset {
        /// `std::env::consts::OS` of the running build.
        target_os: &'static str,
        /// `std::env::consts::ARCH` of the running build.
        target_arch: &'static str,
    },

    /// The downloaded file's SHA-256 did not match the expected checksum.
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// The checksum published alongside the release.
        expected: String,
        /// The checksum computed from the downloaded bytes.
        actual: String,
    },

    /// Minisign signature verification failed.
    #[error("signature verification failed: {0}")]
    Signature(String),

    /// The configured [`Verification`](crate::Verification) policy required a
    /// check that could not run because its input was missing — no public key,
    /// no published signature, or no published checksum.
    #[error("verification policy not satisfied: {0}")]
    VerificationRequired(String),

    /// The downloaded app is not one the policy accepts: its bundle version
    /// differs from the release version, or it is not code-signed by the
    /// expected team.
    #[error("update payload rejected: {0}")]
    Payload(String),
    /// The signed release manifest is past its `expires` time, so it may be
    /// a stale copy replayed to hide newer releases.
    #[error("release manifest expired at {expires}")]
    ManifestExpired {
        /// The manifest's `expires` value.
        expires: String,
    },
    /// Installing the downloaded update failed.
    #[error("install failed: {0}")]
    Install(String),

    /// The app runs from a location it cannot update in place: a read-only
    /// volume such as a mounted disk image, or the randomized read-only path
    /// macOS App Translocation chose. Moving the app (for example into
    /// `/Applications`) fixes it.
    #[error("cannot update the app where it is: {0}")]
    Location(String),

    /// The current user may not write where the update must go, such as an
    /// install directory owned by another user or by the system.
    #[error("permission denied: {0}")]
    Permission(String),

    /// The operation was cancelled through a
    /// [`CancelToken`](crate::CancelToken).
    #[error("cancelled")]
    Cancelled,

    /// A binary delta could not be created or applied: it is malformed, or the
    /// installed files are not the version it expects. The engine treats this
    /// as recoverable and falls back to the full download.
    #[error("delta update failed: {0}")]
    Delta(String),

    /// The current platform has no install strategy yet.
    #[error("update install is not supported on this platform ({0})")]
    UnsupportedPlatform(&'static str),

    /// An underlying I/O error.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl Error {
    /// What kind of failure this is, for choosing what to tell the user.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Http(_) => ErrorKind::Network,
            Self::Parse(_) | Self::NoMatchingAsset { .. } => ErrorKind::Metadata,
            Self::InsecureUrl(_)
            | Self::ChecksumMismatch { .. }
            | Self::Signature(_)
            | Self::VerificationRequired(_)
            | Self::Payload(_)
            | Self::ManifestExpired { .. } => ErrorKind::Verification,
            Self::Install(_) => ErrorKind::Install,
            Self::Location(_) => ErrorKind::Location,
            Self::Permission(_) => ErrorKind::Permission,
            Self::Cancelled => ErrorKind::Cancelled,
            Self::UnsupportedPlatform(_) => ErrorKind::Unsupported,
            Self::Delta(_) => ErrorKind::Other,
            Self::Io(e) => match e.kind() {
                io::ErrorKind::PermissionDenied => ErrorKind::Permission,
                io::ErrorKind::ReadOnlyFilesystem => ErrorKind::Location,
                _ => ErrorKind::Other,
            },
        }
    }

    /// An install-step I/O failure, classified by its cause: no permission
    /// becomes [`Error::Permission`], a read-only file system
    /// [`Error::Location`], and anything else [`Error::Install`].
    #[cfg_attr(
        all(
            not(test),
            not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
        ),
        expect(dead_code, reason = "only platforms with an install strategy call it")
    )]
    pub(crate) fn install_io(context: &str, e: &io::Error) -> Self {
        let message = format!("{context}: {e}");
        match e.kind() {
            io::ErrorKind::PermissionDenied => Self::Permission(message),
            io::ErrorKind::ReadOnlyFilesystem => Self::Location(message),
            _ => Self::Install(message),
        }
    }
}

/// The broad class of an [`Error`], stable enough to drive user interface
/// decisions: which message to show, and whether retrying can help.
///
/// Marked `#[non_exhaustive]`: match it with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The network failed: DNS, connect, timeout, or a non-success HTTP
    /// status. Retrying later may help.
    Network,
    /// The release metadata is malformed or lists no artifact for this
    /// platform. The publisher has to fix it.
    Metadata,
    /// The update failed an integrity, authenticity, or transport-security
    /// check, or the policy requires a check the release cannot satisfy. Do
    /// not install it.
    Verification,
    /// The app runs from a location it cannot update in place, such as a disk
    /// image or an App Translocation path. Moving the app fixes it.
    Location,
    /// The current user may not write where the update must go.
    Permission,
    /// Installing failed for another reason.
    Install,
    /// This platform or artifact type has no install strategy.
    Unsupported,
    /// The operation was cancelled on request.
    Cancelled,
    /// Anything else, such as a local I/O error.
    Other,
}

/// Convenience alias for results returned by this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_are_classified_by_cause() {
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        let read_only = io::Error::from(io::ErrorKind::ReadOnlyFilesystem);
        let other = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(Error::Io(denied).kind(), ErrorKind::Permission);
        assert_eq!(Error::Io(read_only).kind(), ErrorKind::Location);
        assert_eq!(Error::Io(other).kind(), ErrorKind::Other);
    }

    #[test]
    fn install_io_keeps_the_cause() {
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        let error = Error::install_io("could not replace binary", &denied);
        assert!(matches!(error, Error::Permission(_)), "{error:?}");
        assert!(error.to_string().contains("could not replace binary"));
        let busy = io::Error::other("busy");
        assert_eq!(Error::install_io("x", &busy).kind(), ErrorKind::Install);
    }

    #[test]
    fn verification_failures_share_a_kind() {
        for error in [
            Error::InsecureUrl("http://x".into()),
            Error::Signature("bad".into()),
            Error::VerificationRequired("none".into()),
            Error::Payload("team".into()),
            Error::ManifestExpired {
                expires: "2020-01-01T00:00:00Z".into(),
            },
        ] {
            assert_eq!(error.kind(), ErrorKind::Verification, "{error:?}");
        }
        assert_eq!(Error::Http("503".into()).kind(), ErrorKind::Network);
        assert_eq!(Error::Cancelled.kind(), ErrorKind::Cancelled);
    }
}
