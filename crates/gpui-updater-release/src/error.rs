use std::path::PathBuf;

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while preparing a release.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A file-system operation failed.
    #[error("{action} `{}`: {source}", path.display())]
    Io {
        /// What was being done, for example `read`.
        action: &'static str,
        /// The path it was done to.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// An external tool (`codesign`, `ditto`, `hdiutil`, `BinaryDelta`, …)
    /// failed or could not be started.
    #[error("`{program}` failed: {detail}")]
    Command {
        /// The program that was run.
        program: String,
        /// Exit status and the end of its error output.
        detail: String,
    },
    /// The inputs or options do not describe a valid release.
    #[error("{0}")]
    Invalid(String),
    /// A key could not be read, or does not match what it should.
    #[error("key: {0}")]
    Key(String),
    /// A signature could not be made, or did not verify afterwards.
    #[error("signature: {0}")]
    Signature(String),
    /// A JSON document could not be read or written.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A property list could not be read or written.
    #[error("plist `{}`: {source}", path.display())]
    Plist {
        /// The property list.
        path: PathBuf,
        /// The underlying error.
        source: plist::Error,
    },
    /// The update engine's own code (for example native deltas) failed.
    #[error(transparent)]
    Core(#[from] gpui_updater_core::Error),
}

impl Error {
    pub(crate) fn io(
        action: &'static str,
        path: impl Into<PathBuf>,
    ) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io {
            action,
            path,
            source,
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}
