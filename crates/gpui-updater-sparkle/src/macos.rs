//! The macOS driver: loads the embedded Sparkle.framework at run time and
//! drives an `SPUStandardUpdaterController`.
//!
//! Sparkle is reached through the Objective-C runtime (`msg_send!`) rather
//! than generated bindings, so nothing links against the framework. Every
//! selector used here exists in Sparkle 2.0 and later.

mod delegate;
mod item;

use std::fmt;
use std::path::{Path, PathBuf};

use gpui_updater_core::{DriverEvent, DriverEvents, ErrorKind, UpdateError};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyClass;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, msg_send};
use objc2_foundation::{NSBundle, NSError, NSString};

use self::delegate::SparkleDelegate;
use crate::error::Error;
use crate::options::SparkleOptions;

/// Sparkle's Objective-C class for a standard updater with Sparkle's own UI.
const CONTROLLER_CLASS: &std::ffi::CStr = c"SPUStandardUpdaterController";

pub(crate) struct Driver {
    options: SparkleOptions,
    framework_path: PathBuf,
    controller_class: &'static AnyClass,
    /// Keeps the loaded framework alive for the lifetime of the driver.
    _framework: Retained<NSBundle>,
    session: Option<Session>,
}

/// The objects of a started updater.
struct Session {
    events: DriverEvents,
    /// Owns the updater and Sparkle's standard user driver.
    _controller: Retained<AnyObject>,
    /// The controller's `SPUUpdater`.
    updater: Retained<AnyObject>,
    /// Sparkle holds its delegate weakly; this keeps it alive.
    delegate: Retained<SparkleDelegate>,
}

impl Driver {
    pub(crate) fn load(options: SparkleOptions) -> Result<Option<Self>, Error> {
        if MainThreadMarker::new().is_none() {
            return Err(Error::NotMainThread);
        }
        let main = NSBundle::mainBundle();
        let bundle_path = PathBuf::from(main.bundlePath().to_string());
        if bundle_path.extension().is_none_or(|e| e != "app") {
            tracing::debug!("not running from an app bundle; Sparkle is not used");
            return Ok(None);
        }
        let framework_path = match &options.framework_path {
            Some(path) => path.clone(),
            None => match main.privateFrameworksPath() {
                Some(dir) => PathBuf::from(dir.to_string()).join("Sparkle.framework"),
                None => return Ok(None),
            },
        };
        if !framework_path.exists() {
            tracing::debug!(
                "{} does not exist; Sparkle is not used",
                framework_path.display()
            );
            return Ok(None);
        }
        if info_string(&main, "SUPublicEDKey").is_none() {
            return Err(Error::MissingInfoKey {
                key: "SUPublicEDKey",
            });
        }
        if options.feed_url.is_none() && info_string(&main, "SUFeedURL").is_none() {
            return Err(Error::MissingInfoKey { key: "SUFeedURL" });
        }

        let framework = load_framework(&framework_path)?;
        let controller_class = AnyClass::get(CONTROLLER_CLASS).ok_or_else(|| Error::Load {
            path: framework_path.clone(),
            message: "no SPUStandardUpdaterController class; Sparkle 2 is required".into(),
        })?;
        Ok(Some(Self {
            options,
            framework_path,
            controller_class,
            _framework: framework,
            session: None,
        }))
    }

    pub(crate) fn start(&mut self, events: DriverEvents) {
        if self.session.is_some() {
            return;
        }
        let Some(mtm) = MainThreadMarker::new() else {
            events.emit(DriverEvent::Failed(UpdateError::new(
                ErrorKind::Other,
                "Sparkle must be started on the main thread",
            )));
            return;
        };
        let delegate = SparkleDelegate::new(mtm, events.clone(), &self.options);

        // SAFETY: `alloc` on a class object returns a new, uninitialized
        // instance of that class.
        let allocated: Allocated<AnyObject> = unsafe { msg_send![self.controller_class, alloc] };
        // SAFETY: this is SPUStandardUpdaterController's designated
        // initializer: (BOOL startingUpdater, nullable id<SPUUpdaterDelegate>,
        // nullable id<SPUStandardUserDriverDelegate>). The delegate responds to
        // a subset of the optional SPUUpdaterDelegate methods with matching
        // signatures, and outlives the controller (both live in `Session`).
        let controller: Option<Retained<AnyObject>> = unsafe {
            msg_send![
                allocated,
                initWithStartingUpdater: false,
                updaterDelegate: &*delegate,
                userDriverDelegate: None::<&AnyObject>,
            ]
        };
        let Some(controller) = controller else {
            events.emit(DriverEvent::Failed(UpdateError::new(
                ErrorKind::Other,
                "Sparkle could not create its updater",
            )));
            return;
        };
        // SAFETY: `updater` is a nonnull `SPUUpdater *` property of the
        // controller.
        let updater: Retained<AnyObject> = unsafe { msg_send![&controller, updater] };
        // SAFETY: `-[SPUUpdater startUpdater:]` takes an `NSError **` and
        // returns BOOL, which is what the trailing `_` expects.
        let started: Result<(), Retained<NSError>> =
            unsafe { msg_send![&updater, startUpdater: _] };
        if let Err(error) = started {
            events.emit(DriverEvent::Failed(UpdateError::new(
                ErrorKind::Other,
                format!("Sparkle failed to start: {}", error.localizedDescription()),
            )));
        }
        self.session = Some(Session {
            events,
            _controller: controller,
            updater,
            delegate,
        });
    }

    pub(crate) fn check(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        if session.can_check_for_updates() {
            session.events.emit(DriverEvent::Checking);
        }
        session.check_for_updates();
    }

    pub(crate) fn install(&mut self) {
        // Sparkle has no separate "install" entry point: a user-initiated
        // check resumes the session and presents the update it found.
        if let Some(session) = &self.session {
            session.check_for_updates();
        }
    }

    pub(crate) fn relaunch(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        match session.delegate.install_now() {
            Some(install_now) => install_now.call(()),
            None => session.check_for_updates(),
        }
    }

    pub(crate) fn can_check_for_updates(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(Session::can_check_for_updates)
    }

    pub(crate) fn check_in_background(&self) {
        if let Some(session) = &self.session {
            // SAFETY: `-[SPUUpdater checkForUpdatesInBackground]` takes no
            // arguments and returns void.
            let () = unsafe { msg_send![&session.updater, checkForUpdatesInBackground] };
        }
    }

    pub(crate) fn check_for_update_information(&self) {
        if let Some(session) = &self.session {
            // SAFETY: `-[SPUUpdater checkForUpdateInformation]` takes no
            // arguments and returns void.
            let () = unsafe { msg_send![&session.updater, checkForUpdateInformation] };
        }
    }

    pub(crate) fn automatically_checks_for_updates(&self) -> Option<bool> {
        let session = self.session.as_ref()?;
        // SAFETY: `automaticallyChecksForUpdates` is a BOOL property of
        // SPUUpdater.
        Some(unsafe { msg_send![&session.updater, automaticallyChecksForUpdates] })
    }

    pub(crate) fn set_automatically_checks_for_updates(&self, enabled: bool) {
        if let Some(session) = &self.session {
            // SAFETY: the setter of SPUUpdater's BOOL
            // `automaticallyChecksForUpdates` property.
            let () =
                unsafe { msg_send![&session.updater, setAutomaticallyChecksForUpdates: enabled] };
        }
    }

    pub(crate) fn automatically_downloads_updates(&self) -> Option<bool> {
        let session = self.session.as_ref()?;
        // SAFETY: `automaticallyDownloadsUpdates` is a BOOL property of
        // SPUUpdater.
        Some(unsafe { msg_send![&session.updater, automaticallyDownloadsUpdates] })
    }

    pub(crate) fn set_automatically_downloads_updates(&self, enabled: bool) {
        if let Some(session) = &self.session {
            // SAFETY: the setter of SPUUpdater's BOOL
            // `automaticallyDownloadsUpdates` property.
            let () =
                unsafe { msg_send![&session.updater, setAutomaticallyDownloadsUpdates: enabled] };
        }
    }
}

impl Session {
    fn can_check_for_updates(&self) -> bool {
        // SAFETY: `canCheckForUpdates` is a BOOL property of SPUUpdater.
        unsafe { msg_send![&self.updater, canCheckForUpdates] }
    }

    fn check_for_updates(&self) {
        // SAFETY: `-[SPUUpdater checkForUpdates]` takes no arguments and
        // returns void.
        let () = unsafe { msg_send![&self.updater, checkForUpdates] };
    }
}

impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver")
            .field("framework_path", &self.framework_path)
            .field("options", &self.options)
            .field("started", &self.session.is_some())
            .finish_non_exhaustive()
    }
}

/// A non-empty string value from the main bundle's `Info.plist`.
fn info_string(bundle: &NSBundle, key: &str) -> Option<String> {
    let value = bundle.objectForInfoDictionaryKey(&NSString::from_str(key))?;
    let value = value.downcast::<NSString>().ok()?.to_string();
    (!value.trim().is_empty()).then_some(value)
}

fn load_framework(path: &Path) -> Result<Retained<NSBundle>, Error> {
    let load_error = |message: String| Error::Load {
        path: path.to_path_buf(),
        message,
    };
    let framework = NSBundle::bundleWithPath(&NSString::from_str(&path.to_string_lossy()))
        .ok_or_else(|| load_error("not a bundle".into()))?;
    // SAFETY: loading runs the framework's initializers. The framework is the
    // Sparkle.framework embedded in (and code-signed with) the running app,
    // which Sparkle supports loading at run time.
    unsafe { framework.loadAndReturnError() }
        .map_err(|error| load_error(error.localizedDescription().to_string()))?;
    Ok(framework)
}
