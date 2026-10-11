//! How Sparkle's errors map onto driver events. Kept free of Objective-C so it
//! is testable on every platform.

/// Sparkle's error domain (`SUSparkleErrorDomain` in `SUErrors.h`).
pub(crate) const SPARKLE_ERROR_DOMAIN: &str = "SUSparkleErrorDomain";

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
    /// A real failure; show it.
    Fail,
}

/// Classify a Sparkle `NSError` by its domain and code.
pub(crate) fn classify(domain: &str, code: isize) -> ErrorDisposition {
    if domain != SPARKLE_ERROR_DOMAIN {
        return ErrorDisposition::Fail;
    }
    match code {
        NO_UPDATE => ErrorDisposition::Ignore,
        UPDATE_CHECK_DECLINED | INSTALLATION_CANCELED | INSTALLATION_AUTHORIZE_LATER => {
            ErrorDisposition::Settle
        }
        _ => ErrorDisposition::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorDisposition, SPARKLE_ERROR_DOMAIN, classify};

    #[test]
    fn user_choices_are_not_failures() {
        assert_eq!(
            classify(SPARKLE_ERROR_DOMAIN, 1001),
            ErrorDisposition::Ignore
        );
        for code in [1008, 4007, 4008] {
            assert_eq!(
                classify(SPARKLE_ERROR_DOMAIN, code),
                ErrorDisposition::Settle
            );
        }
    }

    #[test]
    fn everything_else_fails() {
        // SUSignatureError, SUInsecureFeedURLError, and a network error.
        assert_eq!(classify(SPARKLE_ERROR_DOMAIN, 3001), ErrorDisposition::Fail);
        assert_eq!(classify(SPARKLE_ERROR_DOMAIN, 3), ErrorDisposition::Fail);
        assert_eq!(classify("NSURLErrorDomain", 1001), ErrorDisposition::Fail);
    }
}
