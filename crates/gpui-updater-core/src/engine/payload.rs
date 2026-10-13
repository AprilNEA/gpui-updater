//! Checks on the app an update would install (macOS).
//!
//! A valid minisign signature proves the publisher's key signed the file, not
//! that the file is the version it claims to be or comes from the same
//! developer as the installed app. These checks close both gaps on macOS, the
//! way Sparkle does: the bundle must declare the release's version, and when
//! the installed app is signed by a team, the update must be validly signed by
//! that same team.

use std::path::Path;

use semver::Version;

use super::config::{BundleVersion, CodeSigning};
use crate::error::Result;

/// What the app inside an update must look like.
#[cfg_attr(
    not(target_os = "macos"),
    expect(dead_code, reason = "only macOS bundles carry this metadata")
)]
pub(super) struct Expectations<'a> {
    /// The release version the bundle must declare.
    pub(super) version: &'a Version,
    /// The installed app, whose code signature is matched.
    pub(super) installed: &'a Path,
    pub(super) bundle_version: BundleVersion,
    pub(super) code_signing: &'a CodeSigning,
}

/// Check the app inside `artifact` (a `.dmg` or an unpacked payload
/// directory). Other platforms have no bundle metadata to check.
#[cfg(not(target_os = "macos"))]
#[expect(clippy::unnecessary_wraps, reason = "same signature as on macOS")]
pub(super) fn check(_artifact: &Path, _expect: &Expectations<'_>) -> Result<()> {
    Ok(())
}

/// Check the app inside `artifact` (a `.dmg` or an unpacked payload
/// directory).
#[cfg(target_os = "macos")]
pub(super) fn check(artifact: &Path, expect: &Expectations<'_>) -> Result<()> {
    let team = match expect.code_signing {
        CodeSigning::Off => None,
        CodeSigning::TeamId(team) => Some(team.clone()),
        CodeSigning::MatchInstalled => macos::team_identifier(expect.installed),
    };
    if expect.bundle_version == BundleVersion::Ignore && team.is_none() {
        return Ok(());
    }
    crate::install::macos::with_app(artifact, |app| {
        if expect.bundle_version == BundleVersion::MustMatch {
            macos::check_bundle_version(app, expect.version)?;
        }
        if let Some(team) = &team {
            macos::check_code_signature(app, team)?;
        }
        Ok(())
    })
}

/// Parse a bundle version leniently: an optional `v`, and missing minor or
/// patch components counted as zero (`1.2` is `1.2.0`).
#[cfg(any(target_os = "macos", test))]
fn lenient_version(text: &str) -> Option<Version> {
    let text = text.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    let split = text.find(['-', '+']).unwrap_or(text.len());
    let (core, rest) = text.split_at(split);
    let mut parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    parts.resize(3, "0");
    Version::parse(&format!("{}{rest}", parts.join("."))).ok()
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::Path;
    use std::process::Command;

    use semver::Version;

    use super::lenient_version;
    use crate::error::{Error, Result};

    pub(super) fn check_bundle_version(app: &Path, expected: &Version) -> Result<()> {
        let plist = app.join("Contents/Info.plist");
        let out = Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print :CFBundleShortVersionString"])
            .arg(&plist)
            .output()?;
        if !out.status.success() {
            return Err(Error::Payload(format!(
                "cannot read CFBundleShortVersionString from {}",
                plist.display()
            )));
        }
        let declared = String::from_utf8_lossy(&out.stdout).trim().to_string();
        match lenient_version(&declared) {
            Some(v) if v.cmp_precedence(expected).is_eq() => Ok(()),
            _ => Err(Error::Payload(format!(
                "the app declares version {declared}, but the release is {expected}"
            ))),
        }
    }

    pub(super) fn check_code_signature(app: &Path, team: &str) -> Result<()> {
        let out = Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app)
            .output()?;
        if !out.status.success() {
            return Err(Error::Payload(format!(
                "code signature is invalid: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        match team_identifier(app) {
            Some(found) if found == team => Ok(()),
            found => Err(Error::Payload(format!(
                "the app is signed by team {}, expected {team}",
                found.as_deref().unwrap_or("(none)")
            ))),
        }
    }

    /// The Team ID an app is signed by, or `None` when it is unsigned,
    /// ad-hoc signed, or unreadable.
    pub(super) fn team_identifier(app: &Path) -> Option<String> {
        let out = Command::new("codesign")
            .args(["-d", "--verbose=2"])
            .arg(app)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // `codesign -d` prints its report on stderr.
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find_map(|line| line.strip_prefix("TeamIdentifier="))
            .map(str::trim)
            .filter(|team| !team.is_empty() && *team != "not set")
            .map(str::to_string)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::lenient_version;
    use semver::Version;

    #[test]
    fn parses_bundle_versions_leniently() {
        let v = |s: &str| lenient_version(s).map(|v| v.to_string());
        assert_eq!(v("1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(v(" v1.2 ").as_deref(), Some("1.2.0"));
        assert_eq!(v("7").as_deref(), Some("7.0.0"));
        assert_eq!(v("1.2-rc.1").as_deref(), Some("1.2.0-rc.1"));
        assert_eq!(v("1.2.3.4"), None);
        assert_eq!(v("one"), None);
        assert!(
            lenient_version("0.8.13+42")
                .unwrap()
                .cmp_precedence(&Version::new(0, 8, 13))
                .is_eq()
        );
    }
}
