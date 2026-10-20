//! `cargo xtask check-packages`: check the crates as they would be published.
//!
//! Cargo stages unpublished workspace dependencies together, so the archives
//! come from one `cargo package --workspace`. Everything after that uses only
//! the extracted archives, never the workspace sources: each archive's files
//! and manifest are checked, then a throwaway consumer of each extracted
//! package is built from a fresh resolve and its normal/build dependency
//! closure is checked against the package's GPUI identity.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail, ensure};

/// Every published crate. Each is packaged, extracted, checked, and consumed.
const PUBLISHED: [&str; 5] = [
    "gpui-updater-core",
    "gpui-updater",
    "gpui-updater-pre",
    "gpui-updater-sparkle",
    "gpui-updater-release",
];

/// Files every published archive must carry.
const REQUIRED_FILES: [&str; 4] = ["src/lib.rs", "README.md", "LICENSE-MIT", "LICENSE-APACHE"];

/// libc 0.2.190 removed Linux `ENOATTR` (deprecated since 0.2.55), which breaks
/// xattr 0.2.3; registry gpui 0.2.2 pulls that in via `gpui_http_client` →
/// `zed-async-tar`. A fresh resolve picks the newest libc, so consumers with
/// xattr 0.2 in their graph hold libc here. The hold stops applying on its own
/// once GPUI's dependency chain moves past xattr 0.2.
const LIBC_HOLD: &str = "0.2.189";

pub fn run(args: &[String]) -> Result<()> {
    let packages = select(args)?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask lives inside the workspace")?
        .to_path_buf();
    let version = workspace_version(&root)?;
    let target_dir =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let cargo = Cargo { root, target_dir };
    let scratch = tempfile::tempdir().context("create a scratch directory")?;

    // Always package the whole workspace: the adapters' archives need core
    // staged next to them, whichever packages are checked afterwards.
    execute(cargo.command().args([
        "package",
        "--workspace",
        "--exclude",
        "xtask",
        "--allow-dirty",
        "--no-verify",
    ]))?;

    for package in &packages {
        let archive = cargo
            .target_dir
            .join("package")
            .join(format!("{package}-{version}.crate"));
        unpack(&archive, scratch.path())?;
        check_archive(&scratch.path().join(format!("{package}-{version}")))
            .with_context(|| format!("{package}: archive check failed"))?;
    }

    for package in &packages {
        check_consumer(&cargo, package, &version, scratch.path())
            .with_context(|| format!("{package}: consumer check failed"))?;
        println!("PASS {package}: extracted consumer build and isolated normal/build closure");
    }
    Ok(())
}

/// The packages named on the command line, or all of them.
fn select(args: &[String]) -> Result<Vec<&'static str>> {
    if args.is_empty() {
        return Ok(PUBLISHED.to_vec());
    }
    args.iter()
        .map(|arg| {
            PUBLISHED
                .iter()
                .copied()
                .find(|package| package == arg)
                .with_context(|| {
                    format!("`{arg}` is not a published package (expected one of {PUBLISHED:?})")
                })
        })
        .collect()
}

fn workspace_version(root: &Path) -> Result<String> {
    let manifest: toml::Table = read_toml(&root.join("Cargo.toml"))?;
    manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .context("workspace Cargo.toml has no `workspace.package.version`")
}

fn unpack(archive: &Path, into: &Path) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    tar::Archive::new(flate2::read::GzDecoder::new(file))
        .unpack(into)
        .with_context(|| format!("unpack {}", archive.display()))
}

fn check_archive(extracted: &Path) -> Result<()> {
    for file in REQUIRED_FILES {
        ensure!(
            extracted.join(file).is_file(),
            "archive is missing `{file}`"
        );
    }
    check_manifest(&read_toml(&extracted.join("Cargo.toml"))?)
}

/// A published manifest must stand alone: no workspace inheritance left
/// unresolved, and no `path` / `git` dependencies a registry user cannot fetch.
fn check_manifest(manifest: &toml::Table) -> Result<()> {
    ensure!(
        !manifest.contains_key("workspace"),
        "manifest still has a `workspace` table"
    );
    let targets = manifest
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|targets| targets.values().filter_map(toml::Value::as_table));
    for section in std::iter::once(manifest).chain(targets) {
        for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(dependencies) = section.get(kind).and_then(toml::Value::as_table) else {
                continue;
            };
            for (name, dependency) in dependencies {
                let local = dependency.as_table().and_then(|dependency| {
                    ["path", "git"]
                        .into_iter()
                        .find(|key| dependency.contains_key(*key))
                });
                if let Some(key) = local {
                    bail!("{kind}.{name} is a `{key}` dependency");
                }
            }
        }
    }
    Ok(())
}

fn check_consumer(cargo: &Cargo, package: &str, version: &str, scratch: &Path) -> Result<()> {
    let consumer = scratch.join(format!("consumer-{package}"));
    fs::create_dir_all(consumer.join("src"))?;
    let manifest_path = consumer.join("Cargo.toml");
    fs::write(&manifest_path, consumer_manifest(package, version, scratch))?;
    fs::write(consumer.join("src/main.rs"), consumer_main(package))?;
    let manifest_arg: OsString = manifest_path.into();

    execute(
        cargo
            .command()
            .arg("generate-lockfile")
            .arg("--manifest-path")
            .arg(&manifest_arg),
    )?;
    let lock = fs::read_to_string(consumer.join("Cargo.lock"))
        .context("read the consumer's Cargo.lock")?;
    if needs_libc_hold(&lock)? {
        execute(
            cargo
                .command()
                .arg("update")
                .arg("--manifest-path")
                .arg(&manifest_arg)
                .args(["--package", "libc", "--precise", LIBC_HOLD]),
        )?;
    }
    execute(
        cargo
            .command()
            .arg("build")
            .arg("--manifest-path")
            .arg(&manifest_arg),
    )?;

    let tree = capture(
        cargo
            .command()
            .arg("tree")
            .arg("--manifest-path")
            .arg(&manifest_arg)
            .args(["-e", "normal,build", "--prefix", "none", "--format", "{p}"]),
    )?;
    let names = tree
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect::<BTreeSet<_>>();
    check_closure(package, &names)
}

fn consumer_manifest(package: &str, version: &str, scratch: &Path) -> String {
    // TOML literal strings ('…') take Windows paths without escaping.
    let extracted = |name: &str| {
        scratch
            .join(format!("{name}-{version}"))
            .display()
            .to_string()
    };
    let gpui = match package {
        "gpui-updater" => "gpui = { version = \"=0.2.2\", default-features = false }\n",
        "gpui-updater-pre" => {
            "gpui = { package = \"gpui-pre\", version = \"=0.3.4\", default-features = false }\n"
        }
        _ => "",
    };
    format!(
        "[package]\n\
         name = \"consumer-{package}\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         \n\
         [dependencies]\n\
         updater = {{ package = \"{package}\", path = '{}' }}\n\
         {gpui}\
         \n\
         [patch.crates-io]\n\
         gpui-updater-core = {{ path = '{}' }}\n",
        extracted(package),
        extracted("gpui-updater-core"),
    )
}

fn consumer_main(package: &str) -> String {
    const ENGINE: &str = r#"use updater::{EngineConfig, StaticManifestSource, UpdateEngine, UpdateStatus, Verification, Version};

fn main() {
    let config = EngineConfig::new(Version::new(1, 2, 3)).verification(Verification::Strict);
    let source = StaticManifestSource::new("https://example.invalid/latest.json");
    let engine = UpdateEngine::new(source, config);
    assert_eq!(engine.current_version(), &Version::new(1, 2, 3));
    assert!(!UpdateStatus::Idle.is_busy());
}
"#;
    const ADAPTER: &str = r#"
use gpui::AppContext as _;

// Type-check the adapter against the consumer's own GPUI dependency identity.
pub fn create(cx: &mut gpui::App) -> gpui::Entity<updater::Updater> {
    cx.new(|cx| updater::Updater::new(
        StaticManifestSource::new("https://example.invalid/latest.json"),
        EngineConfig::new(Version::new(1, 0, 0)), cx))
}
"#;
    const SPARKLE: &str = r"use updater::{SparkleDriver, SparkleOptions};

// Built, never run: on a non-macOS host this compiles the uninhabited stub.
fn main() {
    let _driver = SparkleDriver::load(SparkleOptions::new());
}
";
    const RELEASE: &str = r#"use updater::{AppcastItem, Classifier, FeedLayout, ManifestBuilder, SparkleKey};

// The release tooling is a library first; the CLI is behind `cli`.
fn main() {
    let layout = FeedLayout::new("https://example.invalid", "stable");
    assert_eq!(layout.manifest_url(), "https://example.invalid/channels/stable/latest.json");
    let _builder = ManifestBuilder::for_layout(&layout, "v1.2.3").classifier(Classifier::default());
    let _key = SparkleKey::generate();
    let _ = std::any::type_name::<AppcastItem>();
}
"#;
    match package {
        "gpui-updater" | "gpui-updater-pre" => format!("{ENGINE}{ADAPTER}"),
        "gpui-updater-sparkle" => SPARKLE.to_owned(),
        "gpui-updater-release" => RELEASE.to_owned(),
        _ => ENGINE.to_owned(),
    }
}

/// Whether a consumer's lockfile has xattr 0.2, which libc 0.2.190 no longer
/// builds (see [`LIBC_HOLD`]).
fn needs_libc_hold(lock: &str) -> Result<bool> {
    let lock: toml::Table = toml::from_str(lock).context("parse Cargo.lock")?;
    let packages = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    Ok(packages.iter().any(|package| {
        package.get("name").and_then(toml::Value::as_str) == Some("xattr")
            && package
                .get("version")
                .and_then(toml::Value::as_str)
                .is_some_and(|version| version.starts_with("0.2."))
    }))
}

/// The selected normal/build closure must match the package's GPUI identity:
/// core, Sparkle, and the release tooling pull in no GPUI, official pulls in no pre, pre no official.
/// Package locks may list unrelated platform/optional dependencies; only the
/// `cargo tree` closure is evidence of what is compiled.
fn check_closure(package: &str, names: &BTreeSet<&str>) -> Result<()> {
    let present = |wanted: &[&str]| -> Vec<String> {
        wanted
            .iter()
            .filter(|name| names.contains(*name))
            .map(|name| (*name).to_owned())
            .collect()
    };
    let matching = |forbidden: &dyn Fn(&str) -> bool| -> Vec<String> {
        names
            .iter()
            .filter(|name| forbidden(name))
            .map(|name| (*name).to_owned())
            .collect()
    };
    let (required, unexpected): (&[&str], Vec<String>) = match package {
        "gpui-updater-core" | "gpui-updater-sparkle" | "gpui-updater-release" => (
            &["gpui-updater-core"],
            matching(&|name| {
                name.starts_with("gpui") && name != "gpui-updater-core" && name != package
            }),
        ),
        "gpui-updater" => (
            &["gpui", "gpui-updater-core"],
            matching(&|name| name.starts_with("gpui-pre") || name == "gpui-updater-pre"),
        ),
        "gpui-updater-pre" => {
            let mut unexpected = present(&["gpui", "gpui-updater"]);
            unexpected.extend(matching(&|name| {
                name.starts_with("gpui_") || name.starts_with("gpui-macros")
            }));
            (&["gpui-pre", "gpui-updater-core"], unexpected)
        }
        _ => bail!("no closure rule for `{package}`"),
    };
    let missing: Vec<_> = required
        .iter()
        .filter(|name| !names.contains(*name))
        .collect();
    ensure!(missing.is_empty(), "closure is missing {missing:?}");
    ensure!(
        unexpected.is_empty(),
        "closure must not contain {unexpected:?}"
    );
    Ok(())
}

fn read_toml(path: &Path) -> Result<toml::Table> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Runs the same cargo that runs this xtask, with the settings the checks
/// share: one target directory and no debug info for consumer builds.
struct Cargo {
    root: PathBuf,
    target_dir: PathBuf,
}

impl Cargo {
    fn command(&self) -> Command {
        let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        command
            .current_dir(&self.root)
            .env("CARGO_TARGET_DIR", &self.target_dir)
            .env("CARGO_PROFILE_DEV_DEBUG", "0");
        command
    }
}

fn execute(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to start {command:?}"))?;
    ensure!(status.success(), "{command:?} failed ({status})");
    Ok(())
}

fn capture(command: &mut Command) -> Result<String> {
    let output = command
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("failed to start {command:?}"))?;
    ensure!(
        output.status.success(),
        "{command:?} failed ({})",
        output.status
    );
    String::from_utf8(output.stdout).context("cargo printed non-UTF-8 output")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names<'a>(list: &[&'a str]) -> BTreeSet<&'a str> {
        list.iter().copied().collect()
    }

    fn manifest(text: &str) -> toml::Table {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn select_defaults_to_everything_and_rejects_unknown_names() {
        assert_eq!(select(&[]).unwrap(), PUBLISHED);
        assert_eq!(
            select(&["gpui-updater-pre".into(), "gpui-updater-core".into()]).unwrap(),
            ["gpui-updater-pre", "gpui-updater-core"]
        );
        assert!(select(&["xtask".into()]).is_err());
    }

    #[test]
    fn manifest_check_rejects_workspace_path_and_git() {
        let published = r#"
            [package]
            name = "x"
            [dependencies]
            semver = "1"
            core = { version = "0.1.0", package = "gpui-updater-core" }
            [target.'cfg(target_os = "macos")'.dependencies]
            objc2 = "0.6"
        "#;
        check_manifest(&manifest(published)).unwrap();

        assert!(check_manifest(&manifest("[workspace]\n")).is_err());
        let path = "[dev-dependencies]\nlocal = { path = \"../local\" }\n";
        assert!(check_manifest(&manifest(path)).is_err());
        let git = "[target.'cfg(unix)'.build-dependencies]\nzed = { git = \"https://x\" }\n";
        let error = check_manifest(&manifest(git)).unwrap_err().to_string();
        assert_eq!(error, "build-dependencies.zed is a `git` dependency");
    }

    #[test]
    fn libc_hold_tracks_xattr_0_2_only() {
        let lock = |version: &str| {
            format!(
                "version = 4\n[[package]]\nname = \"libc\"\nversion = \"0.2.190\"\n\
                 [[package]]\nname = \"xattr\"\nversion = \"{version}\"\n"
            )
        };
        assert!(needs_libc_hold(&lock("0.2.3")).unwrap());
        assert!(!needs_libc_hold(&lock("1.6.1")).unwrap());
        assert!(!needs_libc_hold("version = 4\n").unwrap());
    }

    #[test]
    fn closure_rules_keep_gpui_identities_apart() {
        let core = names(&["consumer-gpui-updater-core", "gpui-updater-core", "semver"]);
        check_closure("gpui-updater-core", &core).unwrap();
        assert!(
            check_closure(
                "gpui-updater-core",
                &names(&["gpui-updater-core", "gpui_util"])
            )
            .is_err()
        );

        let release = names(&["gpui-updater-release", "gpui-updater-core", "minisign"]);
        check_closure("gpui-updater-release", &release).unwrap();
        assert!(
            check_closure(
                "gpui-updater-release",
                &names(&["gpui-updater-release", "gpui-updater-core", "gpui"])
            )
            .is_err()
        );
        let sparkle = names(&["gpui-updater-sparkle", "gpui-updater-core", "objc2"]);
        check_closure("gpui-updater-sparkle", &sparkle).unwrap();
        assert!(
            check_closure(
                "gpui-updater-sparkle",
                &names(&["gpui-updater-sparkle", "gpui"])
            )
            .is_err()
        );

        let official = names(&["gpui", "gpui_util", "gpui-updater", "gpui-updater-core"]);
        check_closure("gpui-updater", &official).unwrap();
        let mixed = names(&["gpui", "gpui-updater-core", "gpui-pre-scheduler"]);
        assert!(check_closure("gpui-updater", &mixed).is_err());

        let pre = names(&[
            "gpui-pre",
            "gpui-pre-macros",
            "gpui-updater-pre",
            "gpui-updater-core",
        ]);
        check_closure("gpui-updater-pre", &pre).unwrap();
        for leaked in ["gpui", "gpui-updater", "gpui_util", "gpui-macros"] {
            let mut leaky = pre.clone();
            leaky.insert(leaked);
            assert!(
                check_closure("gpui-updater-pre", &leaky).is_err(),
                "{leaked}"
            );
        }
        assert!(check_closure("gpui-updater-pre", &names(&["gpui-updater-core"])).is_err());
    }

    #[test]
    fn consumer_manifests_pin_each_adapter_to_its_gpui() {
        let scratch = Path::new("/tmp/scratch");
        let official = consumer_manifest("gpui-updater", "0.1.0", scratch);
        assert!(official.contains("gpui = { version = \"=0.2.2\", default-features = false }"));
        assert!(official.contains("path = '/tmp/scratch/gpui-updater-0.1.0'"));
        assert!(
            official
                .contains("gpui-updater-core = { path = '/tmp/scratch/gpui-updater-core-0.1.0' }")
        );
        let pre = consumer_manifest("gpui-updater-pre", "0.1.0", scratch);
        assert!(pre.contains("package = \"gpui-pre\", version = \"=0.3.4\""));
        let sparkle = consumer_manifest("gpui-updater-sparkle", "0.1.0", scratch);
        assert!(!sparkle.contains("gpui ="));
        assert!(manifest(&sparkle).contains_key("patch"));
    }
}
