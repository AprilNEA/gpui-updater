//! Reading an `SUAppcastItem` into a [`Release`].

use gpui_updater_core::{AppcastVersion, Asset, Release};
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSString, NSURL};

/// The release an `SUAppcastItem` describes, with its version resolved by the
/// same rule as `gpui_updater::AppcastSource`
/// ([`AppcastVersion::ShortVersionString`]), so both backends report the same
/// version for the same feed. `None` when that version cannot be parsed.
///
/// `item` must be an `SUAppcastItem` passed in by Sparkle.
pub(super) fn release(item: &AnyObject) -> Option<Release> {
    // SAFETY (all `msg_send!` below): these are read-only properties declared
    // in Sparkle 2's `SUAppcastItem.h` with the given types; nullable object
    // properties are read as `Option`.
    // SAFETY: see above; `NSString *displayVersionString`.
    let display: Option<Retained<NSString>> = unsafe { msg_send![item, displayVersionString] };
    // SAFETY: see above; `NSString *versionString`.
    let bundle: Option<Retained<NSString>> = unsafe { msg_send![item, versionString] };
    let display = display.map(|s| s.to_string());
    let bundle = bundle.map(|s| s.to_string());
    let version =
        match AppcastVersion::ShortVersionString.resolve(display.as_deref(), bundle.as_deref()) {
            Ok(version) => version,
            Err(e) => {
                tracing::warn!("Sparkle offered an update whose version cannot be ordered: {e}");
                return None;
            }
        };

    // SAFETY: see above; `nullable NSURL *fileURL`.
    let file_url: Option<Retained<NSURL>> = unsafe { msg_send![item, fileURL] };
    // SAFETY: see above; `nullable NSURL *infoURL` (informational updates).
    let info_url: Option<Retained<NSURL>> = unsafe { msg_send![item, infoURL] };
    // SAFETY: see above; `uint64_t contentLength`.
    let length: u64 = unsafe { msg_send![item, contentLength] };
    // SAFETY: see above; `nullable NSString *itemDescription`.
    let description: Option<Retained<NSString>> = unsafe { msg_send![item, itemDescription] };
    // SAFETY: see above; `nullable NSURL *releaseNotesURL`.
    let notes_url: Option<Retained<NSURL>> = unsafe { msg_send![item, releaseNotesURL] };

    let url = file_url
        .or(info_url)
        .and_then(|u| absolute(&u))
        .unwrap_or_default();
    let name = url
        .split(['?', '#'])
        .next()
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("update")
        .to_string();
    let mut release = Release::new(version, Asset::new(name, url, length));
    release.notes = description
        .map(|d| d.to_string())
        .filter(|d| !d.trim().is_empty())
        .or_else(|| notes_url.and_then(|u| absolute(&u)));
    Some(release)
}

fn absolute(url: &NSURL) -> Option<String> {
    url.absoluteString().map(|s| s.to_string())
}
