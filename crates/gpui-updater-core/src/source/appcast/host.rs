//! Facts about the running system that decide which appcast items apply.

use semver::Version;

/// Ranks an enclosure's `sparkle:os` value for the running platform. Lower
/// ranks are preferred; `None` means the enclosure is not for this platform.
#[derive(Debug, Clone)]
pub(super) struct OsMatcher {
    /// Accepted values, most specific first, all lowercase.
    preferred: Vec<String>,
    /// Rank of an enclosure without `sparkle:os`.
    missing_rank: Option<usize>,
}

impl OsMatcher {
    /// The default matcher for the platform this binary was built for.
    pub(super) fn for_current_platform() -> Self {
        Self::for_platform(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// The default matcher for `os` / `arch` (values of
    /// [`std::env::consts`]).
    ///
    /// - macOS accepts `macos` and, as Sparkle does, enclosures without
    ///   `sparkle:os`.
    /// - Windows prefers WinSparkle's arch-specific value (`windows-x64`,
    ///   `windows-arm64`, `windows-x86`), then `windows`, then — for feeds
    ///   written only for WinSparkle — enclosures without `sparkle:os`.
    /// - Linux follows the same naming (`linux-x64`, `linux-arm64`, then
    ///   `linux`) but never accepts a missing `sparkle:os`, which Sparkle reads
    ///   as macOS.
    pub(super) fn for_platform(os: &str, arch: &str) -> Self {
        let arch = match arch {
            "x86_64" => Some("x64"),
            "aarch64" => Some("arm64"),
            "x86" => Some("x86"),
            _ => None,
        };
        let arch_specific = |family: &str| {
            let mut preferred: Vec<String> =
                arch.map(|a| format!("{family}-{a}")).into_iter().collect();
            preferred.push(family.to_string());
            preferred
        };
        match os {
            "windows" => {
                let preferred = arch_specific("windows");
                let missing_rank = Some(preferred.len());
                Self {
                    preferred,
                    missing_rank,
                }
            }
            "linux" => Self {
                preferred: arch_specific("linux"),
                missing_rank: None,
            },
            other => Self::exact(other),
        }
    }

    /// Accept exactly `os` (case-insensitive). An enclosure without
    /// `sparkle:os` counts as `macos`, as in Sparkle.
    pub(super) fn exact(os: &str) -> Self {
        let os = os.trim().to_ascii_lowercase();
        let missing_rank = (os == "macos").then_some(0);
        Self {
            preferred: vec![os],
            missing_rank,
        }
    }

    pub(super) fn rank(&self, os: Option<&str>) -> Option<usize> {
        match os {
            None => self.missing_rank,
            Some(os) => self
                .preferred
                .iter()
                .position(|p| p.eq_ignore_ascii_case(os.trim())),
        }
    }
}

/// What the item filters need to know about the running system.
#[derive(Debug, Clone, Default)]
pub(super) struct HostFacts {
    /// Compared against `sparkle:minimumSystemVersion` and
    /// `sparkle:maximumSystemVersion`; `None` skips both checks.
    pub(super) system_version: Option<Version>,
    /// Whether items requiring `arm64` hardware can run here.
    pub(super) supports_arm64: bool,
}

/// The running OS version, where it is meaningful for appcasts (macOS only).
pub(super) fn current_system_version() -> Option<Version> {
    #[cfg(target_os = "macos")]
    {
        let plist =
            std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
        product_version_from_plist(&plist)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Read `ProductVersion` out of a `SystemVersion.plist` document.
#[cfg(any(target_os = "macos", test))]
pub(super) fn product_version_from_plist(xml: &str) -> Option<Version> {
    let options = roxmltree::ParsingOptions {
        // The plist starts with Apple's DOCTYPE; roxmltree never fetches it.
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(xml, options).ok()?;
    let dict = doc.descendants().find(|n| n.has_tag_name("dict"))?;
    let mut entries = dict.children().filter(roxmltree::Node::is_element);
    while let Some(node) = entries.next() {
        if node.has_tag_name("key") && node.text() == Some("ProductVersion") {
            let value = entries.next()?;
            return crate::release::parse_version_lenient(value.text()?).ok();
        }
    }
    None
}

/// Whether this process can run arm64 code: natively, or — for an `x86_64`
/// build on Apple silicon under Rosetta — after relaunching natively.
pub(super) fn supports_arm64() -> bool {
    if cfg!(target_arch = "aarch64") {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.optional.arm64"])
            .output()
            .is_ok_and(|out| out.status.success() && out.stdout.trim_ascii() == b"1")
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}
