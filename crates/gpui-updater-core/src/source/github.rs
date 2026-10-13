//! [`UpdateSource`] backed by the GitHub Releases API.

use serde::Deserialize;

use super::UpdateSource;
use crate::error::{Error, Result};
use crate::http::{self, Metadata};
use crate::release::{Asset, DELTA_FORMAT_FILES_BSDIFF_V1, DeltaAsset, Release, parse_tag};
use crate::verify::sha256_from_sums;

const API: &str = "https://api.github.com";

/// Reads releases from a GitHub repository's Releases.
///
/// Binary deltas (see the `delta` feature) are discovered by name: for the
/// selected asset `<asset>`, a release asset named `<asset>.from-<version>.delta`
/// is a delta from `<version>`. Its checksum is read from the same checksums
/// file and its signature from `<asset>.from-<version>.delta.minisig` (with the
/// configured suffix). Delta files are never picked as the main asset. With
/// [`delta_history`](Self::delta_history), deltas attached to earlier releases
/// are listed too, so the engine can chain them.
///
/// ```no_run
/// use gpui_updater_core::GitHubSource;
///
/// let source = GitHubSource::new("AprilNEA", "OpenLogi")
///     .asset_contains("macos")
///     .asset_contains(".dmg")
///     .with_checksums("SHA256SUMS")
///     .with_minisig();
/// ```
pub struct GitHubSource {
    owner: String,
    repo: String,
    /// Case-insensitive substrings; an asset matches when its name contains all
    /// of them. Defaults to a single extension guessed from the running OS.
    asset_patterns: Vec<String>,
    /// Name of a checksums file asset (e.g. `SHA256SUMS`) used to resolve the
    /// expected SHA-256 of the selected asset.
    sums_asset: Option<String>,
    /// Suffix of the per-asset detached minisign signature (e.g. `.minisig`).
    signature_suffix: Option<String>,
    /// Whether to consider pre-releases as well as full releases.
    include_prereleases: bool,
    /// How many earlier releases to read deltas from.
    delta_history: usize,
    /// Optional token for private repos / higher rate limits.
    token: Option<String>,
}

impl GitHubSource {
    /// Create a source for `owner/repo`, matching assets by an extension
    /// guessed from the current OS (`.dmg` / `.exe` / `.tar.gz`).
    pub fn new(owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
            asset_patterns: vec![default_extension().to_string()],
            sums_asset: None,
            signature_suffix: None,
            include_prereleases: false,
            delta_history: 0,
            token: None,
        }
    }

    /// Require the selected asset's name to contain `substr` (case-insensitive).
    /// Call multiple times to require several substrings (e.g. `macos` + `arm64`).
    #[must_use]
    pub fn asset_contains(mut self, substr: impl Into<String>) -> Self {
        self.asset_patterns.push(substr.into());
        self
    }

    /// Replace the asset-matching substrings entirely.
    #[must_use]
    pub fn asset_patterns(mut self, patterns: Vec<String>) -> Self {
        self.asset_patterns = patterns;
        self
    }

    /// Resolve the expected SHA-256 from a checksums file asset (one
    /// `<hex>  <name>` line per asset, as produced by `shasum -a 256`).
    #[must_use]
    pub fn with_checksums(mut self, sums_asset: impl Into<String>) -> Self {
        self.sums_asset = Some(sums_asset.into());
        self
    }

    /// Look for a detached minisign signature named `<asset>.minisig`.
    #[must_use]
    pub fn with_minisig(mut self) -> Self {
        self.signature_suffix = Some(".minisig".to_string());
        self
    }

    /// Use a custom signature suffix instead of `.minisig`.
    #[must_use]
    pub fn signature_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.signature_suffix = Some(suffix.into());
        self
    }

    /// Consider pre-releases in addition to full releases.
    #[must_use]
    pub fn include_prereleases(mut self, yes: bool) -> Self {
        self.include_prereleases = yes;
        self
    }

    /// Authenticate requests with a token (private repos / rate limits).
    #[must_use]
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Also read the deltas attached to up to `releases` earlier releases, so
    /// the engine can chain them when no delta starts at the installed version
    /// (publish them with `delta::delta_bases`). This lists releases instead
    /// of asking for `/releases/latest`: still one API request, covering up
    /// to 100 releases.
    #[must_use]
    pub fn delta_history(mut self, releases: usize) -> Self {
        self.delta_history = releases;
        self
    }

    /// The release to offer, followed by up to `delta_history` earlier ones.
    fn fetch_releases(&self, headers: &[(&str, &str)]) -> Result<Vec<GhRelease>> {
        if !self.include_prereleases && self.delta_history == 0 {
            let url = format!("{API}/repos/{}/{}/releases/latest", self.owner, self.repo);
            return Ok(vec![http::get_json(&url, headers)?]);
        }
        let per_page = (self.delta_history + 1).clamp(30, 100);
        let url = format!(
            "{API}/repos/{}/{}/releases?per_page={per_page}",
            self.owner, self.repo
        );
        let list: Vec<GhRelease> = http::get_json(&url, headers)?;
        Ok(list
            .into_iter()
            .filter(|r| !r.draft && (self.include_prereleases || !r.prerelease))
            .take(self.delta_history + 1)
            .collect())
    }
}

impl UpdateSource for GitHubSource {
    fn fetch_latest(&self) -> Result<Release> {
        let auth = self.token.as_ref().map(|t| format!("Bearer {t}"));
        let mut headers: Vec<(&str, &str)> = vec![
            ("accept", "application/vnd.github+json"),
            ("x-github-api-version", "2022-11-28"),
        ];
        if let Some(auth) = &auth {
            headers.push(("authorization", auth));
        }

        let releases = self.fetch_releases(&headers)?;
        let Some((release, older)) = releases.split_first() else {
            return Err(Error::Parse("repository has no releases".to_string()));
        };
        let version = parse_tag(&release.tag_name)?;
        let main =
            select_asset(&release.assets, &self.asset_patterns).ok_or(Error::NoMatchingAsset {
                target_os: std::env::consts::OS,
                target_arch: std::env::consts::ARCH,
            })?;

        let sums = match self.sums_asset_of(release) {
            Some(sums) => Some(http::get_string(
                &sums.browser_download_url,
                &headers,
                Metadata::Sidecar,
            )?),
            None => None,
        };
        let sha256 = sums
            .as_deref()
            .and_then(|text| sha256_from_sums(text, &main.name));
        let mut deltas = self.deltas_of(release, main, &Sums::Fetched(sums.as_deref()));
        // Earlier releases' deltas let the engine chain several hops. Their
        // checksums are only fetched if a chain actually uses them.
        for old in older {
            if let Some(old_main) = select_asset(&old.assets, &self.asset_patterns) {
                let url = self
                    .sums_asset_of(old)
                    .map(|a| a.browser_download_url.clone());
                deltas.extend(self.deltas_of(old, old_main, &Sums::Deferred(url)));
            }
        }

        Ok(Release {
            version,
            notes: release
                .body
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| release.name.clone()),
            asset: Asset {
                name: main.name.clone(),
                url: main.browser_download_url.clone(),
                size: main.size,
            },
            signature: None,
            signature_url: self.signature_for(release, &main.name),
            sha256,
            ed25519_signature: None,
            deltas,
        })
    }
}

/// Where the checksums of a release's deltas come from.
enum Sums<'a> {
    /// The release's checksums file, already downloaded.
    Fetched(Option<&'a str>),
    /// The URL of the release's checksums file, for the engine to fetch if
    /// it uses one of these deltas.
    Deferred(Option<String>),
}

impl GitHubSource {
    fn signature_for(&self, release: &GhRelease, name: &str) -> Option<String> {
        let suffix = self.signature_suffix.as_ref()?;
        let want = format!("{name}{suffix}");
        release
            .assets
            .iter()
            .find(|a| a.name == want)
            .map(|a| a.browser_download_url.clone())
    }

    fn sums_asset_of<'a>(&self, release: &'a GhRelease) -> Option<&'a GhAsset> {
        let name = self.sums_asset.as_ref()?;
        release.assets.iter().find(|a| &a.name == name)
    }

    /// The deltas attached to `release`, whose platform asset is `main`.
    fn deltas_of(&self, release: &GhRelease, main: &GhAsset, sums: &Sums<'_>) -> Vec<DeltaAsset> {
        let Ok(to) = parse_tag(&release.tag_name) else {
            return Vec::new();
        };
        release
            .assets
            .iter()
            .filter_map(|a| {
                let from = delta_from(&main.name, &a.name)?;
                let (sha256, checksums_url) = match sums {
                    Sums::Fetched(text) => (text.and_then(|t| sha256_from_sums(t, &a.name)), None),
                    Sums::Deferred(url) => (None, url.clone()),
                };
                Some(DeltaAsset {
                    from,
                    to: to.clone(),
                    format: DELTA_FORMAT_FILES_BSDIFF_V1.to_string(),
                    asset: Asset {
                        name: a.name.clone(),
                        url: a.browser_download_url.clone(),
                        size: a.size,
                    },
                    sha256,
                    checksums_url,
                    signature: None,
                    signature_url: self.signature_for(release, &a.name),
                    ed25519_signature: None,
                })
            })
            .collect()
    }
}

fn default_extension() -> &'static str {
    match std::env::consts::OS {
        "macos" => ".dmg",
        "windows" => ".exe",
        _ => ".tar.gz",
    }
}

/// True when `name` contains every pattern (case-insensitive).
fn asset_matches(name: &str, patterns: &[String]) -> bool {
    let lower = name.to_ascii_lowercase();
    patterns
        .iter()
        .all(|p| lower.contains(&p.to_ascii_lowercase()))
}

fn select_asset<'a>(assets: &'a [GhAsset], patterns: &[String]) -> Option<&'a GhAsset> {
    assets
        .iter()
        .find(|a| !is_delta_asset(&a.name) && asset_matches(&a.name, patterns))
}

/// Delta files and their signatures, which must never be the main asset even
/// though their names contain the main asset's name.
fn is_delta_asset(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.rsplit('.').next() == Some("delta") || lower.contains(".delta.")
}

/// The `from` version when `candidate` is `<main>.from-<version>.delta`.
fn delta_from(main: &str, candidate: &str) -> Option<semver::Version> {
    let version = candidate
        .strip_prefix(main)?
        .strip_prefix(".from-")?
        .strip_suffix(".delta")?;
    parse_tag(version).ok()
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(p: &[&str]) -> Vec<String> {
        p.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn asset_matching_requires_all_substrings_case_insensitively() {
        assert!(asset_matches(
            "OpenLogi-v0.2.0-macOS.DMG",
            &pats(&["macos", ".dmg"])
        ));
        assert!(!asset_matches(
            "OpenLogi-v0.2.0-linux.tar.gz",
            &pats(&["macos"])
        ));
        assert!(asset_matches(
            "app-arm64-macos.dmg",
            &pats(&["arm64", "macos"])
        ));
        assert!(!asset_matches(
            "app-x86_64-macos.dmg",
            &pats(&["arm64", "macos"])
        ));
    }

    #[test]
    fn select_picks_first_matching_asset() {
        let assets = vec![
            GhAsset {
                name: "SHA256SUMS".into(),
                browser_download_url: "u0".into(),
                size: 0,
            },
            GhAsset {
                name: "app-linux.tar.gz".into(),
                browser_download_url: "u1".into(),
                size: 1,
            },
            GhAsset {
                name: "app-macos.dmg".into(),
                browser_download_url: "u2".into(),
                size: 2,
            },
        ];
        let picked = select_asset(&assets, &pats(&[".dmg"])).unwrap();
        assert_eq!(picked.name, "app-macos.dmg");
        assert!(select_asset(&assets, &pats(&[".exe"])).is_none());
    }

    #[test]
    fn delta_assets_are_matched_by_name_and_never_selected_as_main() {
        let assets = vec![
            GhAsset {
                name: "app-macos.dmg.from-0.8.12.delta".into(),
                browser_download_url: "d".into(),
                size: 10,
            },
            GhAsset {
                name: "app-macos.dmg.from-0.8.12.delta.minisig".into(),
                browser_download_url: "s".into(),
                size: 1,
            },
            GhAsset {
                name: "app-macos.dmg".into(),
                browser_download_url: "m".into(),
                size: 100,
            },
        ];
        let picked = select_asset(&assets, &pats(&["macos", ".dmg"])).unwrap();
        assert_eq!(picked.name, "app-macos.dmg");

        assert_eq!(
            delta_from("app-macos.dmg", "app-macos.dmg.from-0.8.12.delta").map(|v| v.to_string()),
            Some("0.8.12".to_string())
        );
        assert_eq!(
            delta_from("app-macos.dmg", "app-macos.dmg.from-v1.0.0-rc.1.delta")
                .map(|v| v.to_string()),
            Some("1.0.0-rc.1".to_string())
        );
        assert!(delta_from("app-macos.dmg", "app-macos.dmg.from-0.8.12.delta.minisig").is_none());
        assert!(delta_from("app-macos.dmg", "app-linux.tar.gz.from-0.8.12.delta").is_none());
        assert!(delta_from("app-macos.dmg", "app-macos.dmg.from-latest.delta").is_none());
    }

    fn gh_release(tag: &str, names: &[&str]) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            name: None,
            body: None,
            draft: false,
            prerelease: false,
            assets: names
                .iter()
                .map(|n| GhAsset {
                    name: (*n).into(),
                    browser_download_url: format!("https://x/{tag}/{n}"),
                    size: 7,
                })
                .collect(),
        }
    }

    #[test]
    fn lists_deltas_of_earlier_releases_with_deferred_checksums() {
        let source = GitHubSource::new("o", "r")
            .asset_patterns(pats(&["macos", ".dmg"]))
            .with_checksums("SHA256SUMS")
            .with_minisig();
        let old = gh_release(
            "v0.8.12",
            &[
                "app-0.8.12-macos.dmg",
                "app-0.8.12-macos.dmg.from-0.8.11.delta",
                "app-0.8.12-macos.dmg.from-0.8.11.delta.minisig",
                "app-0.8.12-linux.tar.gz.from-0.8.11.delta",
                "SHA256SUMS",
            ],
        );
        let main = select_asset(&old.assets, &source.asset_patterns).unwrap();
        let deferred = Sums::Deferred(Some("https://x/v0.8.12/SHA256SUMS".into()));
        let deltas = source.deltas_of(&old, main, &deferred);
        assert_eq!(deltas.len(), 1);
        let d = &deltas[0];
        assert_eq!(
            (d.from.to_string(), d.to.to_string()),
            ("0.8.11".into(), "0.8.12".into())
        );
        assert_eq!(d.sha256, None);
        assert_eq!(
            d.checksums_url.as_deref(),
            Some("https://x/v0.8.12/SHA256SUMS")
        );
        assert_eq!(
            d.signature_url.as_deref(),
            Some("https://x/v0.8.12/app-0.8.12-macos.dmg.from-0.8.11.delta.minisig")
        );

        let sums = "abc  app-0.8.12-macos.dmg.from-0.8.11.delta\n";
        let fetched = source.deltas_of(&old, main, &Sums::Fetched(Some(sums)));
        assert_eq!(fetched[0].sha256.as_deref(), Some("abc"));
        assert_eq!(fetched[0].checksums_url, None);
    }

    #[test]
    fn deserializes_github_release_json() {
        let json = r#"{
            "tag_name": "v0.2.0",
            "name": "Release 0.2.0",
            "body": "notes here",
            "draft": false,
            "assets": [
                {"name": "app-macos.dmg", "browser_download_url": "https://x/app-macos.dmg", "size": 1234}
            ]
        }"#;
        let r: GhRelease = serde_json::from_str(json).unwrap();
        assert_eq!(r.tag_name, "v0.2.0");
        assert_eq!(r.assets.len(), 1);
        assert_eq!(r.assets[0].size, 1234);
        assert_eq!(parse_tag(&r.tag_name).unwrap().minor, 2);
    }
}
