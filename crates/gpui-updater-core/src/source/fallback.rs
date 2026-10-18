//! [`FallbackSource`]: ask several sources in order.

use std::fmt;

use super::UpdateSource;
use crate::error::Result;
use crate::release::Release;

/// Asks several sources in order and returns the release from the first one
/// that answers: a mirror or a second host for when the primary is down.
///
/// The first source that answers wins, even when a later one knows a newer
/// release; the sources are not compared. When every source fails, the error
/// is the primary's, and every failure is logged at `warn`.
///
/// Each source checks what it fetches with its own settings, such as a
/// manifest's signing keys. The engine checks the artifact with the same
/// [`EngineConfig`](crate::EngineConfig) whichever source named it.
///
/// ```
/// use gpui_updater_core::{FallbackSource, GitHubSource, StaticManifestSource};
///
/// let source = FallbackSource::new(StaticManifestSource::new(
///     "https://updates.example.com/latest.json",
/// ))
/// .or(GitHubSource::new("owner", "repo"));
/// ```
pub struct FallbackSource {
    primary: Box<dyn UpdateSource>,
    fallbacks: Vec<Box<dyn UpdateSource>>,
}

impl FallbackSource {
    /// Ask `primary` first.
    #[must_use]
    pub fn new(primary: impl UpdateSource) -> Self {
        Self {
            primary: Box::new(primary),
            fallbacks: Vec::new(),
        }
    }

    /// Ask `next` when every source before it has failed.
    #[must_use]
    pub fn or(mut self, next: impl UpdateSource) -> Self {
        self.fallbacks.push(Box::new(next));
        self
    }
}

impl fmt::Debug for FallbackSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FallbackSource")
            .field("sources", &(1 + self.fallbacks.len()))
            .finish_non_exhaustive()
    }
}

impl UpdateSource for FallbackSource {
    fn fetch_latest(&self) -> Result<Release> {
        let primary_error = match self.primary.fetch_latest() {
            Ok(release) => return Ok(release),
            Err(error) => error,
        };
        tracing::warn!(error = %primary_error, "the primary update source failed");
        for (index, source) in self.fallbacks.iter().enumerate() {
            match source.fetch_latest() {
                Ok(release) => {
                    tracing::info!(fallback = index + 1, "using a fallback update source");
                    return Ok(release);
                }
                Err(error) => {
                    tracing::warn!(fallback = index + 1, %error, "a fallback update source failed");
                }
            }
        }
        Err(primary_error)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::error::Error;
    use crate::release::Asset;
    use semver::Version;

    /// Returns `release(major)`, or fails with `error`, counting its calls.
    struct Fake {
        answer: std::result::Result<u64, &'static str>,
        calls: Arc<AtomicUsize>,
    }

    fn fake(answer: std::result::Result<u64, &'static str>) -> (Fake, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let source = Fake {
            answer,
            calls: calls.clone(),
        };
        (source, calls)
    }

    impl UpdateSource for Fake {
        fn fetch_latest(&self) -> Result<Release> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.answer {
                Ok(major) => Ok(Release::new(
                    Version::new(major, 0, 0),
                    Asset::new("app.dmg", "https://example.com/app.dmg", 1),
                )),
                Err(message) => Err(Error::Http(message.to_string())),
            }
        }
    }

    #[test]
    fn the_primary_wins_when_it_answers() {
        let (primary, _) = fake(Ok(1));
        let (mirror, mirror_calls) = fake(Ok(2));
        let release = FallbackSource::new(primary)
            .or(mirror)
            .fetch_latest()
            .unwrap();
        assert_eq!(release.version, Version::new(1, 0, 0));
        assert_eq!(mirror_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn falls_back_in_order_and_stops_at_the_first_answer() {
        let (primary, _) = fake(Err("primary down"));
        let (second, _) = fake(Err("second down"));
        let (third, _) = fake(Ok(3));
        let (fourth, fourth_calls) = fake(Ok(4));
        let release = FallbackSource::new(primary)
            .or(second)
            .or(third)
            .or(fourth)
            .fetch_latest()
            .unwrap();
        assert_eq!(release.version, Version::new(3, 0, 0));
        assert_eq!(fourth_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn returns_the_primary_error_when_every_source_fails() {
        let (primary, _) = fake(Err("primary down"));
        let (mirror, mirror_calls) = fake(Err("mirror down"));
        let source = FallbackSource::new(primary).or(mirror);
        let err = source.fetch_latest().unwrap_err();
        assert_eq!(
            err.to_string(),
            Error::Http("primary down".into()).to_string()
        );
        assert_eq!(mirror_calls.load(Ordering::SeqCst), 1);
        assert_eq!(format!("{source:?}"), "FallbackSource { sources: 2, .. }");
    }
}
