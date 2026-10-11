//! The `SPUUpdaterDelegate` that turns Sparkle's callbacks into
//! [`DriverEvent`]s.

use std::cell::RefCell;

use block2::{Block, RcBlock};
use gpui_updater_core::{DriverEvent, DriverEvents};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{NSError, NSInteger, NSSet, NSString, NSUnderlyingErrorKey};

use super::item;
use crate::options::{SparkleOptions, StagedUpdatePolicy};
use crate::outcome::{ErrorDisposition, classify};

pub(super) struct Ivars {
    events: DriverEvents,
    feed_url: Option<Retained<NSString>>,
    channels: Retained<NSSet<NSString>>,
    staged_updates: StagedUpdatePolicy,
    /// Sparkle's "install and relaunch now" block for a staged update, kept
    /// under [`StagedUpdatePolicy::HostRelaunches`].
    install_now: RefCell<Option<RcBlock<dyn Fn()>>>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - `SparkleDelegate` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    pub(super) struct SparkleDelegate;

    // SAFETY: NSObjectProtocol has no additional requirements.
    unsafe impl NSObjectProtocol for SparkleDelegate {}

    // Optional `SPUUpdaterDelegate` methods. Sparkle looks each one up with
    // `respondsToSelector:`, so no protocol conformance is declared. Every
    // signature matches `SPUUpdaterDelegate.h` in Sparkle 2: objects are
    // received as `&AnyObject` (`SPUUpdater *`, `SUAppcastItem *`,
    // `NSMutableURLRequest *`), `SPUUpdateCheck` is an `NSInteger`.
    impl SparkleDelegate {
        #[unsafe(method(updater:didFindValidUpdate:))]
        fn did_find_valid_update(&self, _updater: &AnyObject, item: &AnyObject) {
            if let Some(release) = item::release(item) {
                self.emit(DriverEvent::Found(release));
            }
        }

        #[unsafe(method(updaterDidNotFindUpdate:error:))]
        fn did_not_find_update(&self, _updater: &AnyObject, _error: &NSError) {
            self.emit(DriverEvent::UpToDate);
        }

        #[unsafe(method(updater:willDownloadUpdate:withRequest:))]
        fn will_download_update(
            &self,
            _updater: &AnyObject,
            _item: &AnyObject,
            _request: &AnyObject,
        ) {
            self.emit(DriverEvent::Downloading {
                downloaded: 0,
                total: None,
            });
        }

        #[unsafe(method(userDidCancelDownload:))]
        fn user_did_cancel_download(&self, _updater: &AnyObject) {
            self.emit(DriverEvent::Settled);
        }

        #[unsafe(method(updater:willExtractUpdate:))]
        fn will_extract_update(&self, _updater: &AnyObject, _item: &AnyObject) {
            self.emit(DriverEvent::Installing);
        }

        #[unsafe(method(updater:willInstallUpdate:))]
        fn will_install_update(&self, _updater: &AnyObject, _item: &AnyObject) {
            self.emit(DriverEvent::Installing);
        }

        #[unsafe(method(updater:willInstallUpdateOnQuit:immediateInstallationBlock:))]
        fn will_install_update_on_quit(
            &self,
            _updater: &AnyObject,
            item: &AnyObject,
            install_now: &Block<dyn Fn()>,
        ) -> bool {
            if let Some(release) = item::release(item) {
                self.emit(DriverEvent::ReadyToRelaunch(release.version));
            }
            match self.ivars().staged_updates {
                StagedUpdatePolicy::SparklePrompts => false,
                StagedUpdatePolicy::HostRelaunches => {
                    // Returning YES lets us keep and invoke the block later.
                    *self.ivars().install_now.borrow_mut() = Some(install_now.copy());
                    true
                }
            }
        }

        // Errors are reported only here: Sparkle passes the same error to
        // `updater:didAbortWithError:` (and, for downloads, to
        // `updater:failedToDownloadUpdate:error:`) right before this call, so
        // handling those too would report every failure twice.
        #[unsafe(method(updater:didFinishUpdateCycleForUpdateCheck:error:))]
        fn did_finish_update_cycle(
            &self,
            _updater: &AnyObject,
            _check: NSInteger,
            error: Option<&NSError>,
        ) {
            if let Some(error) = error {
                self.report_error(error);
            }
            self.emit(DriverEvent::Settled);
        }

        #[unsafe(method_id(feedURLStringForUpdater:))]
        fn feed_url_string(&self, _updater: &AnyObject) -> Option<Retained<NSString>> {
            self.ivars().feed_url.clone()
        }

        #[unsafe(method_id(allowedChannelsForUpdater:))]
        fn allowed_channels(&self, _updater: &AnyObject) -> Retained<NSSet<NSString>> {
            self.ivars().channels.clone()
        }
    }
);

impl SparkleDelegate {
    pub(super) fn new(
        mtm: MainThreadMarker,
        events: DriverEvents,
        options: &SparkleOptions,
    ) -> Retained<Self> {
        let channels: Vec<Retained<NSString>> = options
            .channels
            .iter()
            .map(|c| NSString::from_str(c))
            .collect();
        let this = Self::alloc(mtm).set_ivars(Ivars {
            events,
            feed_url: options.feed_url.as_deref().map(NSString::from_str),
            channels: NSSet::from_retained_slice(&channels),
            staged_updates: options.staged_updates,
            install_now: RefCell::new(None),
        });
        // SAFETY: `init` is NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    /// The kept "install and relaunch now" block, if Sparkle handed one over.
    pub(super) fn install_now(&self) -> Option<RcBlock<dyn Fn()>> {
        self.ivars().install_now.borrow().clone()
    }

    fn emit(&self, event: DriverEvent) {
        self.ivars().events.emit(event);
    }

    fn report_error(&self, error: &NSError) {
        match classify(&error.domain().to_string(), error.code()) {
            ErrorDisposition::Ignore => {}
            ErrorDisposition::Settle => self.emit(DriverEvent::Settled),
            ErrorDisposition::Fail => self.emit(DriverEvent::Failed(describe(error))),
        }
    }
}

/// Sparkle's own description, followed by the innermost underlying error
/// (`NSUnderlyingErrorKey`), which carries the actual cause — Sparkle's
/// top-level messages are generic, e.g. "An error occurred in retrieving
/// update information".
fn describe(error: &NSError) -> String {
    let message = error.localizedDescription().to_string();
    let mut root = None;
    let mut next = underlying(error);
    // Bounded in case of a cyclic chain.
    for _ in 0..4 {
        let Some(current) = next else { break };
        next = underlying(&current);
        root = Some(current);
    }
    match root {
        Some(root) => format!("{message} ({})", root.localizedDescription()),
        None => message,
    }
}

fn underlying(error: &NSError) -> Option<Retained<NSError>> {
    // SAFETY: `NSUnderlyingErrorKey` is an immutable Foundation constant.
    let key = unsafe { NSUnderlyingErrorKey };
    error
        .userInfo()
        .objectForKey(key)?
        .downcast::<NSError>()
        .ok()
}
