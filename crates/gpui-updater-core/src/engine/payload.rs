//! Checks on the app an update would install (macOS and Windows).
//!
//! A valid minisign signature proves the publisher's key signed the file, not
//! that the file is the version it claims to be or comes from the same
//! developer as the installed app. These checks close both gaps on macOS, the
//! way Sparkle does: the bundle must declare the release's version, and when
//! the installed app is signed by a team, the update must be validly signed by
//! that same team. On Windows the `.exe` or `.msi` must carry a valid
//! Authenticode signature by the installed app's publisher, and so must an
//! executable rebuilt from deltas.

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

/// Check the app inside `artifact`. Linux has no platform code signature or
/// bundle metadata to check.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[expect(clippy::unnecessary_wraps, reason = "same signature as on macOS")]
pub(super) fn check(_artifact: &Path, _expect: &Expectations<'_>) -> Result<()> {
    Ok(())
}

/// Check the app inside `artifact` (a `.dmg` or an unpacked payload
/// directory).
#[cfg(target_os = "macos")]
pub(super) fn check(artifact: &Path, expect: &Expectations<'_>) -> Result<()> {
    let team = match expect.code_signing {
        // `Publisher` names a Windows Authenticode signer; macOS ignores it.
        CodeSigning::Off | CodeSigning::Publisher(_) => None,
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

/// Check the Authenticode signature of `artifact`: an `.exe` or `.msi`, or a
/// directory holding the executable rebuilt from deltas under the installed
/// file's name.
/// Bundle versions are not compared: Windows executables need not declare one.
/// A signature that cannot be read is an error, not a pass, for the installed
/// app as for the update: a check that turns itself off checks nothing.
#[cfg(target_os = "windows")]
pub(super) fn check(artifact: &Path, expect: &Expectations<'_>) -> Result<()> {
    let publisher = match expect.code_signing {
        CodeSigning::Publisher(name) => Some(name.clone()),
        // Nothing is installed at the root yet, so there is nothing to match.
        CodeSigning::MatchInstalled if !expect.installed.exists() => None,
        CodeSigning::MatchInstalled => authenticode::query(expect.installed)?.valid_publisher(),
        _ => None,
    };
    let Some(publisher) = publisher else {
        return Ok(());
    };
    // A missing rebuilt file fails the query, so it cannot pass the check.
    let signed = match expect.installed.file_name() {
        Some(name) if artifact.is_dir() => artifact.join(name),
        _ => artifact.to_path_buf(),
    };
    authenticode::query(&signed)?.require(&publisher)
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

/// Authenticode signatures, read with PowerShell's `Get-AuthenticodeSignature`
/// (core has no `unsafe` code, so it does not call `WinVerifyTrust` itself).
#[cfg(any(target_os = "windows", test))]
mod authenticode {
    use crate::error::{Error, Result};

    /// What `Get-AuthenticodeSignature` reports for a file.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Signature {
        /// `Valid`, `NotSigned`, `HashMismatch`, `NotTrusted`, and so on.
        pub(super) status: String,
        /// The signer certificate's subject, such as
        /// `CN=Example Corp, O=Example Corp, C=US`.
        pub(super) subject: Option<String>,
    }

    impl Signature {
        /// Parse the query's output: the status on the first line and the
        /// subject, if any, on the second.
        pub(super) fn parse(stdout: &str) -> Option<Self> {
            let mut lines = stdout.trim_start_matches('\u{feff}').lines().map(str::trim);
            let status = lines.next().filter(|s| !s.is_empty())?.to_string();
            let subject = lines.next().filter(|s| !s.is_empty()).map(str::to_string);
            Some(Self { status, subject })
        }

        /// The publisher name, the subject's common name, when the signature
        /// is valid.
        pub(super) fn valid_publisher(&self) -> Option<String> {
            if self.status != "Valid" {
                return None;
            }
            common_name(self.subject.as_deref()?)
        }

        /// Require a valid signature by `publisher`.
        pub(super) fn require(&self, publisher: &str) -> Result<()> {
            if self.status != "Valid" {
                return Err(Error::Payload(format!(
                    "the Authenticode signature is not valid ({})",
                    self.status
                )));
            }
            match self.valid_publisher() {
                Some(found) if found == publisher => Ok(()),
                found => Err(Error::Payload(format!(
                    "the update is signed by {}, expected {publisher}",
                    found.as_deref().unwrap_or("(no common name)")
                ))),
            }
        }
    }

    /// The common name (CN) in a certificate subject such as
    /// `CN="Example, Inc.", O=Example, C=US`.
    pub(super) fn common_name(subject: &str) -> Option<String> {
        attributes(subject)
            .into_iter()
            .find_map(|(key, value)| key.eq_ignore_ascii_case("CN").then_some(value))
    }

    /// Split a distinguished name into `(attribute, value)` pairs. A value
    /// may be quoted to hold a comma, with `""` standing for a quote.
    fn attributes(subject: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut chars = subject.chars().peekable();
        while chars.peek().is_some() {
            let key: String = chars.by_ref().take_while(|&c| c != '=').collect();
            while chars.next_if_eq(&' ').is_some() {}
            let mut value = String::new();
            if chars.next_if_eq(&'"').is_some() {
                while let Some(c) = chars.next() {
                    if c != '"' {
                        value.push(c);
                    } else if chars.next_if_eq(&'"').is_some() {
                        value.push('"');
                    } else {
                        break;
                    }
                }
                // Skip anything up to and including the separator.
                chars.by_ref().take_while(|&c| c != ',').for_each(drop);
            } else {
                value = chars.by_ref().take_while(|&c| c != ',').collect();
                value.truncate(value.trim_end().len());
            }
            out.push((key.trim().to_string(), value));
        }
        out
    }

    /// Read the Authenticode signature of `path`.
    #[cfg(target_os = "windows")]
    pub(super) fn query(path: &std::path::Path) -> Result<Signature> {
        // The path travels in an environment variable, so no quoting can
        // turn it into PowerShell code. Constrained Language Mode forbids
        // setting the output encoding; the script then goes on in the
        // console code page, which only matters for non-ASCII names.
        const SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
            try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false) } catch { }; \
            $s = Get-AuthenticodeSignature -LiteralPath $env:GPUI_UPDATER_SIGNED_FILE; \
            Write-Output ([string]$s.Status); \
            if ($s.SignerCertificate) { Write-Output $s.SignerCertificate.Subject }";
        let root = std::env::var_os("SystemRoot").map_or_else(
            || std::path::PathBuf::from(r"C:\Windows"),
            std::path::PathBuf::from,
        );
        let out = std::process::Command::new(
            root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
        )
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .env("GPUI_UPDATER_SIGNED_FILE", path)
        // An app started from PowerShell 7 inherits its module path, from
        // which Windows PowerShell cannot load Get-AuthenticodeSignature.
        // Without the variable, it uses its own default path.
        .env_remove("PSModulePath")
        .output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        match Signature::parse(&stdout) {
            Some(signature) if out.status.success() => Ok(signature),
            _ => Err(Error::Payload(format!(
                "could not read the Authenticode signature of {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, reason = "tests")]
    mod tests {
        use super::*;

        #[test]
        fn parses_the_query_output() {
            let out = "\u{feff}Valid\r\nCN=Microsoft Windows, O=Microsoft Corporation, C=US\r\n";
            let signature = Signature::parse(out).unwrap();
            assert_eq!(signature.status, "Valid");
            assert_eq!(
                signature.valid_publisher().as_deref(),
                Some("Microsoft Windows")
            );
            let unsigned = Signature::parse("NotSigned\r\n").unwrap();
            assert_eq!(unsigned.subject, None);
            assert_eq!(unsigned.valid_publisher(), None);
            assert_eq!(Signature::parse(""), None);
            assert_eq!(Signature::parse("\r\n"), None);
        }

        #[test]
        fn finds_the_common_name_in_a_subject() {
            let cn = |s: &str| common_name(s);
            assert_eq!(
                cn("CN=Example Corp, O=Example Corp, C=US").as_deref(),
                Some("Example Corp")
            );
            assert_eq!(
                cn("O=Example, CN=Example App ,C=US").as_deref(),
                Some("Example App")
            );
            assert_eq!(
                cn(r#"CN="Example, Inc.", O=Example"#).as_deref(),
                Some("Example, Inc.")
            );
            assert_eq!(
                cn(r#"CN="Say ""hi"" Ltd""#).as_deref(),
                Some(r#"Say "hi" Ltd"#)
            );
            assert_eq!(cn("cn=lower").as_deref(), Some("lower"));
            assert_eq!(cn("O=No Common Name, C=US"), None);
            assert_eq!(cn(""), None);
            // A CN-like text inside another value is not the CN.
            assert_eq!(cn(r#"O="CN=Fake", CN=Real"#).as_deref(), Some("Real"));
        }

        #[test]
        fn requires_a_valid_signature_by_the_publisher() {
            let signed = |status: &str| Signature {
                status: status.to_string(),
                subject: Some("CN=Example Corp, O=Example Corp".to_string()),
            };
            signed("Valid").require("Example Corp").unwrap();
            for (signature, publisher) in [
                (signed("Valid"), "Other Corp"),
                (signed("HashMismatch"), "Example Corp"),
                (signed("NotTrusted"), "Example Corp"),
                (
                    Signature {
                        status: "NotSigned".into(),
                        subject: None,
                    },
                    "Example Corp",
                ),
            ] {
                assert!(
                    matches!(signature.require(publisher), Err(Error::Payload(_))),
                    "{signature:?} for {publisher}"
                );
            }
        }
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

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use std::path::Path;

    use semver::Version;

    use super::{BundleVersion, CodeSigning, Expectations, check};

    fn checked(artifact: &Path, signing: &CodeSigning) -> crate::Result<()> {
        let version = Version::new(1, 0, 0);
        check(
            artifact,
            &Expectations {
                version: &version,
                installed: artifact,
                bundle_version: BundleVersion::Ignore,
                code_signing: signing,
            },
        )
    }

    #[test]
    fn a_windows_publisher_is_ignored() {
        // With nothing to check, the artifact is not opened, so a missing
        // one passes. A Team ID opens it, so the same path fails.
        let missing = Path::new("/nonexistent/gpui-updater/update.dmg");
        let publisher = CodeSigning::Publisher("Example Corp".into());
        assert!(checked(missing, &publisher).is_ok());
        assert!(checked(missing, &CodeSigning::Off).is_ok());
        let team = CodeSigning::TeamId("ABCDE12345".into());
        assert!(checked(missing, &team).is_err());
    }
}

#[cfg(all(test, target_os = "windows"))]
#[allow(clippy::unwrap_used, reason = "tests")]
mod windows_tests {
    use std::path::PathBuf;

    use semver::Version;

    use super::authenticode;
    use super::{BundleVersion, CodeSigning, Expectations, check};
    use crate::error::Error;

    /// A file Windows ships signed (by catalog).
    fn signed_system_file() -> PathBuf {
        let root = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        root.join("explorer.exe")
    }

    fn unsigned_file(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("unsigned.exe");
        std::fs::write(&path, b"MZ not really a program").unwrap();
        path
    }

    fn checked(
        artifact: &std::path::Path,
        installed: &std::path::Path,
        signing: &CodeSigning,
    ) -> crate::Result<()> {
        let version = Version::new(1, 0, 0);
        check(
            artifact,
            &Expectations {
                version: &version,
                installed,
                bundle_version: BundleVersion::MustMatch,
                code_signing: signing,
            },
        )
    }

    #[test]
    fn reads_the_signature_of_a_system_file() {
        // Run from PowerShell 7 (as on CI), `cargo test` passes on its
        // module path, as an app started from it would. The query must
        // still find Get-AuthenticodeSignature.
        let signature = authenticode::query(&signed_system_file()).unwrap();
        assert_eq!(signature.status, "Valid", "{signature:?}");
        assert!(signature.valid_publisher().is_some(), "{signature:?}");
        let dir = tempfile::tempdir().unwrap();
        let unsigned = authenticode::query(&unsigned_file(dir.path())).unwrap();
        assert_ne!(unsigned.status, "Valid");
        assert_eq!(unsigned.valid_publisher(), None);
    }

    #[test]
    fn match_installed_requires_the_installed_publisher() {
        let dir = tempfile::tempdir().unwrap();
        let signed = signed_system_file();
        let unsigned = unsigned_file(dir.path());
        let matching = CodeSigning::MatchInstalled;
        // A signed app takes only an update signed by the same publisher.
        checked(&signed, &signed, &matching).unwrap();
        assert!(matches!(
            checked(&unsigned, &signed, &matching),
            Err(Error::Payload(_))
        ));
        // An unsigned app is not checked.
        checked(&unsigned, &unsigned, &matching).unwrap();
        // Off and the macOS-only TeamId check nothing.
        checked(&unsigned, &signed, &CodeSigning::Off).unwrap();
        checked(
            &unsigned,
            &signed,
            &CodeSigning::TeamId("ABCDE12345".into()),
        )
        .unwrap();
    }

    #[test]
    fn publisher_names_the_required_signer() {
        let dir = tempfile::tempdir().unwrap();
        let signed = signed_system_file();
        let unsigned = unsigned_file(dir.path());
        let publisher = authenticode::query(&signed)
            .unwrap()
            .valid_publisher()
            .unwrap();
        checked(
            &signed,
            &unsigned,
            &CodeSigning::Publisher(publisher.clone()),
        )
        .unwrap();
        let other = CodeSigning::Publisher("Example Corp".into());
        assert!(matches!(
            checked(&signed, &unsigned, &other),
            Err(Error::Payload(_))
        ));
        let wanted = CodeSigning::Publisher(publisher);
        assert!(matches!(
            checked(&unsigned, &unsigned, &wanted),
            Err(Error::Payload(_))
        ));
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_pass() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.exe");
        assert!(authenticode::query(&missing).is_err());
        let wanted = CodeSigning::Publisher("Example Corp".into());
        assert!(checked(&missing, &missing, &wanted).is_err());
        // A signed app cannot be matched by an update that cannot be read.
        let matching = CodeSigning::MatchInstalled;
        assert!(checked(&missing, &signed_system_file(), &matching).is_err());
        // With nothing installed at the root, there is nothing to match.
        let unsigned = unsigned_file(dir.path());
        checked(&unsigned, &missing, &matching).unwrap();
    }

    #[test]
    fn checks_the_executable_inside_a_rebuilt_directory() {
        // A delta rebuilds the executable under the installed file's name.
        let dir = tempfile::tempdir().unwrap();
        let signed = signed_system_file();
        let matching = CodeSigning::MatchInstalled;
        let rebuilt = dir.path().join("delta-1.1.0");
        std::fs::create_dir(&rebuilt).unwrap();
        let exe = rebuilt.join(signed.file_name().unwrap());
        // Nothing under that name: an error, not a pass.
        assert!(checked(&rebuilt, &signed, &matching).is_err());
        std::fs::write(&exe, b"MZ not really a program").unwrap();
        assert!(matches!(
            checked(&rebuilt, &signed, &matching),
            Err(Error::Payload(_))
        ));
        // A copy keeps the catalog signature.
        std::fs::copy(&signed, &exe).unwrap();
        checked(&rebuilt, &signed, &matching).unwrap();
    }
}
