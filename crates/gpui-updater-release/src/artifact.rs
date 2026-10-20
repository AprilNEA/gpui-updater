//! Recognize release files by name: which OS, architecture, and format each
//! one is, and its MIME type.
//!
//! The default rules match the `<name>-<os>-<arch>.<ext>` convention, for
//! example `App-v1.2.3-macos-arm64.dmg`. Replace or extend them with
//! [`ArtifactRule`]s for other naming schemes, or classify files yourself and
//! hand the result to [`ManifestBuilder::asset`](crate::ManifestBuilder::asset).

/// What a release file is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Classified {
    /// Operating system, for example `macos`.
    pub os: String,
    /// CPU architecture, when the rule determines one.
    pub arch: Option<String>,
    /// Package format, for example `dmg`.
    pub format: String,
    /// MIME type.
    pub content_type: Option<String>,
    /// Oldest supported OS version.
    pub minimum_os_version: Option<String>,
}

/// How a rule finds the architecture in a file name.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArchRule {
    /// The file has no architecture.
    None,
    /// Every matching file has this architecture, for example `universal`.
    Fixed(String),
    /// The architecture is the rest of the name (without the suffix) after
    /// the last `marker`, and must be one of `allowed`. A name without the
    /// marker, or with another architecture, does not match the rule.
    After {
        /// Text right before the architecture, for example `-macos-`.
        marker: String,
        /// Accepted architectures, compared case-insensitively.
        allowed: Vec<String>,
    },
}

/// One file-name rule.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArtifactRule {
    /// File-name suffix, compared case-insensitively, for example `.dmg`.
    pub suffix: String,
    /// Operating system the file is for.
    pub os: String,
    /// Package format.
    pub format: String,
    /// MIME type.
    pub content_type: Option<String>,
    /// Oldest supported OS version.
    pub minimum_os_version: Option<String>,
    /// Where the architecture comes from.
    pub arch: ArchRule,
}

/// The architectures the default rules accept.
pub const DEFAULT_ARCHES: [&str; 5] = ["arm64", "aarch64", "x86_64", "x64", "universal"];

impl ArtifactRule {
    /// A rule for files ending in `suffix`, with the architecture after
    /// `-<os>-` (one of [`DEFAULT_ARCHES`]).
    pub fn new(
        suffix: impl Into<String>,
        os: impl Into<String>,
        format: impl Into<String>,
    ) -> Self {
        let os = os.into();
        Self {
            suffix: suffix.into(),
            arch: ArchRule::After {
                marker: format!("-{os}-"),
                allowed: DEFAULT_ARCHES.iter().map(ToString::to_string).collect(),
            },
            os,
            format: format.into(),
            content_type: None,
            minimum_os_version: None,
        }
    }

    /// Set the MIME type.
    #[must_use]
    pub fn content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Set the oldest supported OS version.
    #[must_use]
    pub fn minimum_os_version(mut self, version: impl Into<String>) -> Self {
        self.minimum_os_version = Some(version.into());
        self
    }

    /// Set how the architecture is found.
    #[must_use]
    pub fn arch(mut self, arch: ArchRule) -> Self {
        self.arch = arch;
        self
    }

    /// Accept only these architectures after the current marker.
    #[must_use]
    pub fn arches<I, S>(mut self, allowed: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let allowed = allowed.into_iter().map(Into::into).collect();
        self.arch = match self.arch {
            ArchRule::After { marker, .. } => ArchRule::After { marker, allowed },
            _ => ArchRule::After {
                marker: format!("-{}-", self.os),
                allowed,
            },
        };
        self
    }

    /// Classify `name`, or `None` when the rule does not apply.
    pub fn classify(&self, name: &str) -> Option<Classified> {
        let lower = name.to_ascii_lowercase();
        let suffix = self.suffix.to_ascii_lowercase();
        if !lower.ends_with(&suffix) || lower.len() == suffix.len() {
            return None;
        }
        let stem = &name[..name.len() - suffix.len()];
        let arch = match &self.arch {
            ArchRule::None => None,
            ArchRule::Fixed(arch) => Some(arch.clone()),
            ArchRule::After { marker, allowed } => {
                let lower_stem = stem.to_ascii_lowercase();
                let at = lower_stem.rfind(&marker.to_ascii_lowercase())?;
                let arch = &stem[at + marker.len()..];
                let known = allowed.iter().find(|a| a.eq_ignore_ascii_case(arch))?;
                Some(known.clone())
            }
        };
        Some(Classified {
            os: self.os.clone(),
            arch,
            format: self.format.clone(),
            content_type: self.content_type.clone(),
            minimum_os_version: self.minimum_os_version.clone(),
        })
    }
}

/// An ordered list of rules; the first that applies wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classifier {
    rules: Vec<ArtifactRule>,
}

impl Default for Classifier {
    /// The `<name>-<os>-<arch>.<ext>` rules for the formats the engine installs:
    /// `.dmg` and `.zip` on macOS, `.msi`, `.exe`, and `.zip` on Windows, and
    /// `.tar.gz` and `.AppImage` on Linux.
    fn default() -> Self {
        Self {
            rules: vec![
                ArtifactRule::new(".dmg", "macos", "dmg")
                    .content_type("application/x-apple-diskimage"),
                ArtifactRule::new(".zip", "macos", "zip").content_type("application/zip"),
                ArtifactRule::new(".msi", "windows", "msi").content_type("application/x-msi"),
                ArtifactRule::new(".exe", "windows", "exe")
                    .content_type("application/vnd.microsoft.portable-executable"),
                ArtifactRule::new(".zip", "windows", "zip").content_type("application/zip"),
                ArtifactRule::new(".tar.gz", "linux", "tar.gz").content_type("application/gzip"),
                ArtifactRule::new(".AppImage", "linux", "appimage")
                    .content_type("application/vnd.appimage"),
            ],
        }
    }
}

impl Classifier {
    /// A classifier with no rules.
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    /// Append a rule, tried after the existing ones.
    #[must_use]
    pub fn rule(mut self, rule: ArtifactRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Insert a rule before the existing ones.
    #[must_use]
    pub fn rule_first(mut self, rule: ArtifactRule) -> Self {
        self.rules.insert(0, rule);
        self
    }

    /// Keep only the rules for these operating systems.
    #[must_use]
    pub fn only_os(mut self, oses: &[&str]) -> Self {
        self.rules
            .retain(|r| oses.iter().any(|os| os.eq_ignore_ascii_case(&r.os)));
        self
    }

    /// Keep only the rules for these formats.
    #[must_use]
    pub fn only_formats(mut self, formats: &[&str]) -> Self {
        self.rules
            .retain(|r| formats.iter().any(|f| f.eq_ignore_ascii_case(&r.format)));
        self
    }

    /// Set the oldest supported version for every rule of `os`.
    #[must_use]
    pub fn minimum_os_version(mut self, os: &str, version: &str) -> Self {
        for rule in self
            .rules
            .iter_mut()
            .filter(|r| r.os.eq_ignore_ascii_case(os))
        {
            rule.minimum_os_version = Some(version.to_string());
        }
        self
    }

    /// Accept only these architectures in every rule that reads one from the
    /// name.
    #[must_use]
    pub fn arches(mut self, allowed: &[&str]) -> Self {
        for rule in &mut self.rules {
            if let ArchRule::After { allowed: a, .. } = &mut rule.arch {
                *a = allowed.iter().map(ToString::to_string).collect();
            }
        }
        self
    }

    /// The rules, in order.
    pub fn rules(&self) -> &[ArtifactRule] {
        &self.rules
    }

    /// Classify `name` with the first rule that applies.
    pub fn classify(&self, name: &str) -> Option<Classified> {
        self.rules.iter().find_map(|rule| rule.classify(name))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn default_rules_follow_the_os_arch_convention() {
        let c = Classifier::default()
            .minimum_os_version("macos", "13.0")
            .minimum_os_version("windows", "10.0");
        let dmg = c.classify("OpenLogi-v0.8.14-macos-arm64.dmg").unwrap();
        assert_eq!(
            (dmg.os.as_str(), dmg.arch.as_deref(), dmg.format.as_str()),
            ("macos", Some("arm64"), "dmg")
        );
        assert_eq!(
            dmg.content_type.as_deref(),
            Some("application/x-apple-diskimage")
        );
        assert_eq!(dmg.minimum_os_version.as_deref(), Some("13.0"));
        let zip = c.classify("OpenLogi-v0.8.14-windows-x86_64.zip").unwrap();
        assert_eq!((zip.os.as_str(), zip.format.as_str()), ("windows", "zip"));
        assert_eq!(zip.minimum_os_version.as_deref(), Some("10.0"));
        let mac_zip = c.classify("App-1.0-macos-universal.zip").unwrap();
        assert_eq!(mac_zip.os, "macos");
        let tgz = c.classify("App-1.0-linux-x86_64.tar.gz").unwrap();
        assert_eq!((tgz.os.as_str(), tgz.format.as_str()), ("linux", "tar.gz"));
        assert_eq!(tgz.minimum_os_version, None);
    }

    #[test]
    fn non_release_files_and_unknown_arches_do_not_match() {
        let c = Classifier::default();
        for name in [
            "SHA256SUMS",
            "OpenLogi-v0.8.14-macos-arm64.dmg.minisig",
            "OpenLogi-v0.8.14-macos-riscv.dmg",
            "OpenLogi.dmg",
            ".dmg",
        ] {
            assert_eq!(c.classify(name), None, "{name}");
        }
        assert_eq!(
            c.clone().arches(&["arm64"]).classify("A-macos-x86_64.dmg"),
            None
        );
    }

    #[test]
    fn custom_rules_and_filters() {
        let c = Classifier::empty().rule(
            ArtifactRule::new(".pkg", "macos", "pkg").arch(ArchRule::Fixed("universal".into())),
        );
        assert_eq!(
            c.classify("Setup.pkg").unwrap().arch.as_deref(),
            Some("universal")
        );
        let only = Classifier::default()
            .only_os(&["windows"])
            .only_formats(&["msi"]);
        assert_eq!(only.rules().len(), 1);
        let after = ArtifactRule::new(".deb", "linux", "deb").arches(["amd64"]);
        assert_eq!(
            after
                .classify("app_1.0-linux-AMD64.deb")
                .unwrap()
                .arch
                .as_deref(),
            Some("amd64")
        );
    }
}
