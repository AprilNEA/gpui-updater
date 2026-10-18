//! Put Sparkle.framework into an app bundle, configure it in `Info.plist`,
//! sign its code in the order `codesign` needs, and check the result.
//!
//! These steps follow Sparkle's own documentation: embed with `ditto` so the
//! framework's version links survive; non-sandboxed apps remove the XPC
//! services; sign `Autoupdate`, `Updater.app`, then the framework, each with
//! the hardened runtime and without `--deep`; and keep `CFBundleVersion`
//! comparable with the feed's `sparkle:version`. All of it needs macOS tools
//! (`ditto`, `codesign`) except the `Info.plist` work.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use plist::Value;

use super::SparkleDistribution;
use crate::error::{Error, Result};
use crate::util::run;

/// Sparkle 2's framework version directory.
pub const FRAMEWORK_VERSION: &str = "B";

/// `Contents/Frameworks/Sparkle.framework` inside `app`.
pub fn framework(app: &Path) -> PathBuf {
    app.join("Contents/Frameworks/Sparkle.framework")
}

/// How [`embed`] installs the framework.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EmbedOptions {
    /// Keep `XPCServices`. Sandboxed apps need them; Sparkle's sandboxing
    /// guide has other apps remove them rather than sign code that never
    /// runs. Default `false`.
    pub keep_xpc_services: bool,
    /// Copy Sparkle's `LICENSE` into the framework's resources, since its MIT
    /// license asks that the notice travel with it. Default `true`.
    pub copy_license: bool,
    /// The framework version directory. Default [`FRAMEWORK_VERSION`].
    pub framework_version: String,
}

impl Default for EmbedOptions {
    fn default() -> Self {
        Self {
            keep_xpc_services: false,
            copy_license: true,
            framework_version: FRAMEWORK_VERSION.into(),
        }
    }
}

impl EmbedOptions {
    /// Keep the XPC services, for a sandboxed app.
    #[must_use]
    pub fn keep_xpc_services(mut self) -> Self {
        self.keep_xpc_services = true;
        self
    }

    /// Do not copy the license file.
    #[must_use]
    pub fn without_license(mut self) -> Self {
        self.copy_license = false;
        self
    }
}

/// Replace any Sparkle.framework in `app` with the one from `dist`; returns
/// the embedded framework's path. Run it before signing the app.
pub fn embed(app: &Path, dist: &SparkleDistribution, options: &EmbedOptions) -> Result<PathBuf> {
    let target = framework(app);
    if target.exists() {
        fs::remove_dir_all(&target).map_err(Error::io("remove", &target))?;
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(Error::io("create", parent))?;
    }
    run(Command::new("ditto").arg(dist.framework()).arg(&target))?;
    let version = target.join("Versions").join(&options.framework_version);
    if !version.join("Sparkle").is_file() {
        return Err(Error::invalid(format!(
            "the framework has no Versions/{}/Sparkle; set EmbedOptions::framework_version",
            options.framework_version
        )));
    }
    if !options.keep_xpc_services {
        let link = target.join("XPCServices");
        if fs::symlink_metadata(&link).is_ok() {
            fs::remove_file(&link).map_err(Error::io("remove", &link))?;
        }
        let services = version.join("XPCServices");
        if services.exists() {
            fs::remove_dir_all(&services).map_err(Error::io("remove", &services))?;
        }
    }
    if options.copy_license {
        let to = version.join("Resources/LICENSE");
        fs::copy(dist.license(), &to).map_err(Error::io("copy the license to", &to))?;
    }
    Ok(target)
}

/// Sparkle's `Info.plist` keys. `None` leaves a key as it is.
///
/// A privacy-minded setup that never polls on a schedule but still lets
/// people choose automatic installs, and checks signatures before unpacking:
///
/// ```
/// use gpui_updater_release::sparkle::SparkleSettings;
///
/// let settings = SparkleSettings::new("https://dl.example.com/appcast.xml", "dwOe6ZzIDrpl1SpM+iAu1my3EquhGspSGlWBlq1zkdI=")
///     .unwrap()
///     .enable_automatic_checks(false)
///     .allows_automatic_updates(true)
///     .enable_system_profiling(false)
///     .verify_update_before_extraction(true);
/// assert_eq!(settings.entries().len(), 6);
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct SparkleSettings {
    /// `SUFeedURL`.
    pub feed_url: Option<String>,
    /// `SUPublicEDKey`, base64.
    pub public_ed_key: Option<String>,
    /// `SUEnableAutomaticChecks`.
    pub enable_automatic_checks: Option<bool>,
    /// `SUAllowsAutomaticUpdates`.
    pub allows_automatic_updates: Option<bool>,
    /// `SUAutomaticallyUpdate`.
    pub automatically_update: Option<bool>,
    /// `SUScheduledCheckInterval`, seconds.
    pub scheduled_check_interval: Option<u64>,
    /// `SUEnableSystemProfiling`.
    pub enable_system_profiling: Option<bool>,
    /// `SUShowReleaseNotes`.
    pub show_release_notes: Option<bool>,
    /// `SUVerifyUpdateBeforeExtraction`.
    pub verify_update_before_extraction: Option<bool>,
    /// `SURequireSignedFeed`.
    pub require_signed_feed: Option<bool>,
    /// `SUEnableInstallerLauncherService` (sandboxed apps).
    pub enable_installer_launcher_service: Option<bool>,
    /// `SUEnableDownloaderService` (sandboxed apps).
    pub enable_downloader_service: Option<bool>,
    /// `SUEnableJavaScript` in release notes.
    pub enable_javascript: Option<bool>,
    /// `SUAllowedURLSchemes`.
    pub allowed_url_schemes: Option<Vec<String>>,
    /// `SUDefaultsDomain`.
    pub defaults_domain: Option<String>,
    /// Any other key, written as given after the ones above.
    pub extra: Vec<(String, Value)>,
    /// Before writing, require `CFBundleVersion` to equal
    /// `CFBundleShortVersionString`. Sparkle compares the feed's
    /// `sparkle:version` with `CFBundleVersion`; a build number or timestamp
    /// there that outranks the release version means no update is ever
    /// offered. Default `true` from [`new`](Self::new).
    pub require_bundle_version_match: bool,
}

macro_rules! setters {
    ($($(#[$doc:meta])* $name:ident: $ty:ty),* $(,)?) => {
        $(
            $(#[$doc])*
            #[must_use]
            pub fn $name(mut self, value: $ty) -> Self {
                self.$name = Some(value);
                self
            }
        )*
    };
}

impl SparkleSettings {
    /// Settings for a feed and its signing key. Fails unless `public_ed_key`
    /// is a base64 32-byte Ed25519 public key.
    pub fn new(feed_url: impl Into<String>, public_ed_key: &str) -> Result<Self> {
        let key = public_ed_key.trim();
        let decoded = STANDARD
            .decode(key)
            .map_err(|e| Error::Key(format!("SUPublicEDKey is not base64: {e}")))?;
        if decoded.len() != 32 {
            return Err(Error::Key(format!(
                "SUPublicEDKey must be a 32-byte Ed25519 public key, not {} bytes",
                decoded.len()
            )));
        }
        Ok(Self {
            feed_url: Some(feed_url.into()),
            public_ed_key: Some(key.to_string()),
            require_bundle_version_match: true,
            ..Self::default()
        })
    }

    setters! {
        /// Set `SUEnableAutomaticChecks`.
        enable_automatic_checks: bool,
        /// Set `SUAllowsAutomaticUpdates`.
        allows_automatic_updates: bool,
        /// Set `SUAutomaticallyUpdate`.
        automatically_update: bool,
        /// Set `SUScheduledCheckInterval` in seconds.
        scheduled_check_interval: u64,
        /// Set `SUEnableSystemProfiling`.
        enable_system_profiling: bool,
        /// Set `SUShowReleaseNotes`.
        show_release_notes: bool,
        /// Set `SUVerifyUpdateBeforeExtraction`.
        verify_update_before_extraction: bool,
        /// Set `SURequireSignedFeed`.
        require_signed_feed: bool,
        /// Set `SUEnableInstallerLauncherService`.
        enable_installer_launcher_service: bool,
        /// Set `SUEnableDownloaderService`.
        enable_downloader_service: bool,
        /// Set `SUEnableJavaScript`.
        enable_javascript: bool,
        /// Set `SUAllowedURLSchemes`.
        allowed_url_schemes: Vec<String>,
        /// Set `SUDefaultsDomain`.
        defaults_domain: String,
    }

    /// Set any other key.
    #[must_use]
    pub fn set(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extra.push((key.into(), value.into()));
        self
    }

    /// Do not require `CFBundleVersion` to equal the short version.
    #[must_use]
    pub fn allow_any_bundle_version(mut self) -> Self {
        self.require_bundle_version_match = false;
        self
    }

    /// The keys to write, in order.
    pub fn entries(&self) -> Vec<(String, Value)> {
        let mut out: Vec<(String, Value)> = Vec::new();
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                out.push((key.to_string(), value));
            }
        };
        put("SUFeedURL", self.feed_url.clone().map(Value::String));
        put(
            "SUPublicEDKey",
            self.public_ed_key.clone().map(Value::String),
        );
        put(
            "SUEnableAutomaticChecks",
            self.enable_automatic_checks.map(Value::Boolean),
        );
        put(
            "SUAllowsAutomaticUpdates",
            self.allows_automatic_updates.map(Value::Boolean),
        );
        put(
            "SUAutomaticallyUpdate",
            self.automatically_update.map(Value::Boolean),
        );
        put(
            "SUScheduledCheckInterval",
            self.scheduled_check_interval
                .map(|s| Value::Integer(s.into())),
        );
        put(
            "SUEnableSystemProfiling",
            self.enable_system_profiling.map(Value::Boolean),
        );
        put(
            "SUShowReleaseNotes",
            self.show_release_notes.map(Value::Boolean),
        );
        put(
            "SUVerifyUpdateBeforeExtraction",
            self.verify_update_before_extraction.map(Value::Boolean),
        );
        put(
            "SURequireSignedFeed",
            self.require_signed_feed.map(Value::Boolean),
        );
        put(
            "SUEnableInstallerLauncherService",
            self.enable_installer_launcher_service.map(Value::Boolean),
        );
        put(
            "SUEnableDownloaderService",
            self.enable_downloader_service.map(Value::Boolean),
        );
        put(
            "SUEnableJavaScript",
            self.enable_javascript.map(Value::Boolean),
        );
        put(
            "SUAllowedURLSchemes",
            self.allowed_url_schemes
                .clone()
                .map(|s| Value::Array(s.into_iter().map(Value::String).collect())),
        );
        put(
            "SUDefaultsDomain",
            self.defaults_domain.clone().map(Value::String),
        );
        out.extend(self.extra.iter().cloned());
        out
    }

    /// Write the keys into `info_plist` and read every one back.
    pub fn apply(&self, info_plist: &Path) -> Result<()> {
        if self.require_bundle_version_match {
            check_bundle_version(info_plist)?;
        }
        let mut root = read_plist(info_plist)?;
        let dict = root.as_dictionary_mut().ok_or_else(|| {
            Error::invalid(format!("`{}` is not a dictionary", info_plist.display()))
        })?;
        let entries = self.entries();
        for (key, value) in &entries {
            dict.insert(key.clone(), value.clone());
        }
        root.to_file_xml(info_plist)
            .map_err(|source| Error::Plist {
                path: info_plist.to_path_buf(),
                source,
            })?;
        self.check(info_plist)
    }

    /// Fail unless `info_plist` has every key with its value.
    pub fn check(&self, info_plist: &Path) -> Result<()> {
        let root = read_plist(info_plist)?;
        let dict = root.as_dictionary();
        let mut wrong = Vec::new();
        for (key, expected) in self.entries() {
            let actual = dict.and_then(|d| d.get(&key));
            if actual != Some(&expected) {
                wrong.push(format!("{key} = {actual:?}, expected {expected:?}"));
            }
        }
        if wrong.is_empty() {
            Ok(())
        } else {
            Err(Error::invalid(format!(
                "`{}`: {}",
                info_plist.display(),
                wrong.join("; ")
            )))
        }
    }
}

/// Fail unless `CFBundleVersion` equals `CFBundleShortVersionString`.
pub fn check_bundle_version(info_plist: &Path) -> Result<()> {
    let bundle = read_string(info_plist, "CFBundleVersion")?;
    let short = read_string(info_plist, "CFBundleShortVersionString")?;
    if bundle.is_some() && bundle == short {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "`{}` has CFBundleVersion {bundle:?}; Sparkle compares the feed's sparkle:version \
             with it, so it must be the release version {short:?}",
            info_plist.display()
        )))
    }
}

pub(crate) fn read_plist(path: &Path) -> Result<Value> {
    Value::from_file(path).map_err(|source| Error::Plist {
        path: path.to_path_buf(),
        source,
    })
}

/// A string value of a property list's top-level dictionary.
pub fn read_string(path: &Path, key: &str) -> Result<Option<String>> {
    Ok(read_plist(path)?
        .as_dictionary()
        .and_then(|d| d.get(key))
        .and_then(Value::as_string)
        .map(str::to_string))
}

/// Sparkle's code in the inside-out order `codesign` must seal it; empty when
/// `app` embeds no Sparkle. Sign each with the hardened runtime and without
/// `--deep`, then sign the app.
pub fn code_inside_out(app: &Path) -> Vec<PathBuf> {
    let framework = framework(app);
    if !framework.exists() {
        return Vec::new();
    }
    let version = framework.join("Versions").join(FRAMEWORK_VERSION);
    let mut code = Vec::new();
    // Kept only by sandboxed apps; Sparkle's guide signs them first, the
    // installer before the downloader.
    for service in ["Installer.xpc", "Downloader.xpc"] {
        let path = version.join("XPCServices").join(service);
        if path.exists() {
            code.push(path);
        }
    }
    code.push(version.join("Autoupdate"));
    code.push(version.join("Updater.app"));
    code.push(framework);
    code
}

/// How to run `codesign`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CodesignOptions {
    /// Signing identity, for example `Developer ID Application: … (TEAMID)`,
    /// or `-` for ad-hoc.
    pub identity: String,
    /// Request a secure timestamp (`--timestamp`). Default `true`; notarization
    /// requires it.
    pub timestamp: bool,
    /// Enable the hardened runtime (`--options runtime`). Default `true`.
    pub hardened_runtime: bool,
    /// Keychain to find the identity in.
    pub keychain: Option<PathBuf>,
    /// More arguments, placed before the path.
    pub extra_args: Vec<String>,
}

impl CodesignOptions {
    /// Sign with `identity`, timestamped, with the hardened runtime.
    pub fn new(identity: impl Into<String>) -> Self {
        Self {
            identity: identity.into(),
            timestamp: true,
            hardened_runtime: true,
            keychain: None,
            extra_args: Vec::new(),
        }
    }

    /// Run `codesign` on one path.
    pub fn sign(&self, path: &Path) -> Result<()> {
        let mut cmd = Command::new("codesign");
        cmd.arg("--force").arg("--sign").arg(&self.identity);
        if self.hardened_runtime {
            cmd.args(["--options", "runtime"]);
        }
        if self.timestamp {
            cmd.arg("--timestamp");
        }
        if let Some(keychain) = &self.keychain {
            cmd.arg("--keychain").arg(keychain);
        }
        cmd.args(&self.extra_args).arg(path);
        run(&mut cmd).map(drop)
    }
}

/// Sign Sparkle's code inside `app` in order (see [`code_inside_out`]).
/// Sign the app itself afterwards; this does not.
pub fn codesign_sparkle(app: &Path, options: &CodesignOptions) -> Result<()> {
    for path in code_inside_out(app) {
        if path.file_name().is_some_and(|n| n == "Downloader.xpc") {
            // Sparkle ships the downloader with the network-client
            // entitlement it needs inside the sandbox.
            let mut keep = options.clone();
            keep.extra_args
                .push("--preserve-metadata=entitlements".into());
            keep.sign(&path)?;
        } else {
            options.sign(&path)?;
        }
    }
    Ok(())
}

/// The Team ID `path` is signed by, or `None` for ad-hoc or unsigned code.
pub fn team_identifier(path: &Path) -> Result<Option<String>> {
    let output = Command::new("codesign")
        .args(["--display", "--verbose=2"])
        .arg(path)
        .output()
        .map_err(|e| Error::Command {
            program: "codesign".into(),
            detail: format!("could not start: {e}"),
        })?;
    // `codesign --display` prints to standard error.
    let text = String::from_utf8_lossy(&output.stderr);
    Ok(text
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|team| !team.is_empty() && *team != "not set")
        .map(str::to_string))
}

/// What [`verify_bundle`] checks.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct BundleCheck {
    /// Keys that must be in `Info.plist` with these values.
    pub settings: Option<SparkleSettings>,
    /// Whether the XPC services must be gone. Default `true` from
    /// [`new`](Self::new).
    pub expect_no_xpc_services: bool,
    /// Whether the app and Sparkle's code must be signed by one Team ID, and
    /// `codesign --verify --deep --strict` must pass. Default `true` from
    /// [`new`](Self::new).
    pub signatures: bool,
}

impl BundleCheck {
    /// Every check, with `settings` when given.
    pub fn new(settings: Option<SparkleSettings>) -> Self {
        Self {
            settings,
            expect_no_xpc_services: true,
            signatures: true,
        }
    }
}

/// Check an embedded, configured, and signed bundle; the error lists every
/// problem found.
pub fn verify_bundle(app: &Path, check: &BundleCheck) -> Result<()> {
    let mut problems = Vec::new();
    let framework = framework(app);
    let version = framework.join("Versions").join(FRAMEWORK_VERSION);
    if !version.join("Sparkle").is_file() {
        problems.push(format!("no {}", version.join("Sparkle").display()));
    }
    if check.expect_no_xpc_services
        && (fs::symlink_metadata(framework.join("XPCServices")).is_ok()
            || version.join("XPCServices").exists())
    {
        problems.push("Sparkle's XPC services are still embedded".into());
    }
    if let Some(settings) = &check.settings {
        let info = app.join("Contents/Info.plist");
        if let Err(e) = settings.check(&info) {
            problems.push(e.to_string());
        }
        if settings.require_bundle_version_match {
            if let Err(e) = check_bundle_version(&info) {
                problems.push(e.to_string());
            }
        }
    }
    if check.signatures {
        if let Err(e) = run(Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app))
        {
            problems.push(e.to_string());
        }
        let team = team_identifier(app)?;
        for code in code_inside_out(app) {
            let theirs = team_identifier(&code)?;
            if theirs != team {
                problems.push(format!(
                    "{} is signed by team {theirs:?}, the app by {team:?}",
                    code.display()
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "`{}`:\n{}",
            app.display(),
            problems.join("\n")
        )))
    }
}

#[cfg(test)]
mod tests;
