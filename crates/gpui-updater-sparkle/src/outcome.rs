//! How Sparkle's errors map onto driver events. Kept free of Objective-C so it
//! is testable on every platform.

use gpui_updater_core::ErrorKind;

/// Sparkle's error domain (`SUSparkleErrorDomain` in `SUErrors.h`).
pub(crate) const SPARKLE_ERROR_DOMAIN: &str = "SUSparkleErrorDomain";
/// Foundation's URL loading errors (`NSURLErrorDomain`).
const URL_ERROR_DOMAIN: &str = "NSURLErrorDomain";
/// Foundation's file system errors (`NSCocoaErrorDomain`).
const COCOA_ERROR_DOMAIN: &str = "NSCocoaErrorDomain";
/// `errno` values (`NSPOSIXErrorDomain`).
const POSIX_ERROR_DOMAIN: &str = "NSPOSIXErrorDomain";

/// `SUNoUpdateError`: the check found nothing; already reported as up to date.
const NO_UPDATE: isize = 1001;
/// `SUUpdateCheckDeclinedError`: the user declined to check.
const UPDATE_CHECK_DECLINED: isize = 1008;
/// `SUInstallationCanceledError`: the user cancelled the installation.
const INSTALLATION_CANCELED: isize = 4007;
/// `SUInstallationAuthorizeLaterError`: the user postponed authorization.
const INSTALLATION_AUTHORIZE_LATER: isize = 4008;

/// What the driver reports for an error Sparkle passed to its delegate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorDisposition {
    /// Not an error from the user's point of view; report nothing.
    Ignore,
    /// The user stopped the session; report that nothing is in flight.
    Settle,
    /// A real failure of this kind; show it.
    Fail(ErrorKind),
}

/// Classify a Sparkle `NSError` by its domain and code. `root` is the
/// innermost underlying error (`NSUnderlyingErrorKey`), when there is one: it
/// often names the actual cause behind Sparkle's generic top-level error.
pub(crate) fn classify(domain: &str, code: isize, root: Option<(&str, isize)>) -> ErrorDisposition {
    if domain == SPARKLE_ERROR_DOMAIN {
        match code {
            NO_UPDATE => return ErrorDisposition::Ignore,
            UPDATE_CHECK_DECLINED | INSTALLATION_CANCELED | INSTALLATION_AUTHORIZE_LATER => {
                return ErrorDisposition::Settle;
            }
            _ => {}
        }
    }
    let kind = kind_of(domain, code);
    // Sparkle reports, for example, a failed appcast fetch as the generic
    // `SUAppcastError`; the underlying URL error says it was the network.
    let refined = match (kind, root.map(|(domain, code)| kind_of(domain, code))) {
        (ErrorKind::Metadata | ErrorKind::Install | ErrorKind::Other, Some(root))
            if root != ErrorKind::Other =>
        {
            root
        }
        _ => kind,
    };
    ErrorDisposition::Fail(refined)
}

/// The kind of one error, ignoring what lies beneath it.
fn kind_of(domain: &str, code: isize) -> ErrorKind {
    match domain {
        URL_ERROR_DOMAIN => ErrorKind::Network,
        SPARKLE_ERROR_DOMAIN => sparkle_kind(code),
        // NSFileWriteNoPermissionError, NSFileWriteVolumeReadOnlyError.
        COCOA_ERROR_DOMAIN => match code {
            513 => ErrorKind::Permission,
            642 => ErrorKind::Location,
            _ => ErrorKind::Other,
        },
        // EACCES / EPERM, EROFS.
        POSIX_ERROR_DOMAIN => match code {
            1 | 13 => ErrorKind::Permission,
            30 => ErrorKind::Location,
            _ => ErrorKind::Other,
        },
        _ => ErrorKind::Other,
    }
}

/// The kind of a `SUError` code from Sparkle's `SUErrors.h`.
fn sparkle_kind(code: isize) -> ErrorKind {
    match code {
        // SUNoPublicDSAFoundError, SUInsufficientSigningError,
        // SUInsecureFeedURLError, SUSignatureError, SUValidationError.
        1..=3 | 3001 | 3002 => ErrorKind::Verification,
        // SUAppcastParseError, SUAppcastError.
        1000 | 1002 => ErrorKind::Metadata,
        // SURunningFromDiskImageError, SURunningTranslocated.
        1003 | 1005 => ErrorKind::Location,
        // SUDownloadError.
        2001 => ErrorKind::Network,
        // SUAuthenticationFailure, SUInstallationWriteNoPermissionError.
        4001 | 4012 => ErrorKind::Permission,
        // SUUnarchivingError and the other installation-phase errors.
        3000 | 4000..=4999 => ErrorKind::Install,
        _ => ErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(kind: ErrorKind) -> ErrorDisposition {
        ErrorDisposition::Fail(kind)
    }

    #[test]
    fn user_choices_are_not_failures() {
        assert_eq!(
            classify(SPARKLE_ERROR_DOMAIN, 1001, None),
            ErrorDisposition::Ignore
        );
        for code in [1008, 4007, 4008] {
            assert_eq!(
                classify(SPARKLE_ERROR_DOMAIN, code, None),
                ErrorDisposition::Settle
            );
        }
    }

    #[test]
    fn failures_carry_their_kind() {
        let sparkle = |code| classify(SPARKLE_ERROR_DOMAIN, code, None);
        assert_eq!(sparkle(3001), fail(ErrorKind::Verification));
        assert_eq!(sparkle(3), fail(ErrorKind::Verification));
        assert_eq!(sparkle(1003), fail(ErrorKind::Location));
        assert_eq!(sparkle(1005), fail(ErrorKind::Location));
        assert_eq!(sparkle(2001), fail(ErrorKind::Network));
        assert_eq!(sparkle(4012), fail(ErrorKind::Permission));
        assert_eq!(sparkle(4005), fail(ErrorKind::Install));
        assert_eq!(sparkle(5000), fail(ErrorKind::Other));
        assert_eq!(
            classify(URL_ERROR_DOMAIN, 1001, None),
            fail(ErrorKind::Network)
        );
        assert_eq!(
            classify(POSIX_ERROR_DOMAIN, 30, None),
            fail(ErrorKind::Location)
        );
    }

    #[test]
    fn a_generic_error_takes_the_kind_of_its_root_cause() {
        // "An error occurred in retrieving update information" over a dropped
        // connection is a network failure, not bad metadata.
        assert_eq!(
            classify(SPARKLE_ERROR_DOMAIN, 1002, Some((URL_ERROR_DOMAIN, -1009))),
            fail(ErrorKind::Network)
        );
        assert_eq!(
            classify(SPARKLE_ERROR_DOMAIN, 4000, Some((COCOA_ERROR_DOMAIN, 513))),
            fail(ErrorKind::Permission)
        );
        // A specific top-level kind is kept, and an unknown root changes nothing.
        assert_eq!(
            classify(SPARKLE_ERROR_DOMAIN, 3001, Some((URL_ERROR_DOMAIN, -1))),
            fail(ErrorKind::Verification)
        );
        assert_eq!(
            classify(SPARKLE_ERROR_DOMAIN, 1002, Some(("Other", 7))),
            fail(ErrorKind::Metadata)
        );
    }
}
