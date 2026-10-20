#![allow(clippy::unwrap_used)]

use std::fs;

use super::*;

const KEY: &str = "dwOe6ZzIDrpl1SpM+iAu1my3EquhGspSGlWBlq1zkdI=";

fn info_plist(dir: &Path, bundle: &str, short: &str) -> PathBuf {
    let path = dir.join("Info.plist");
    let mut dict = plist::Dictionary::new();
    dict.insert("CFBundleIdentifier".into(), "org.example.App".into());
    dict.insert("CFBundleVersion".into(), bundle.into());
    dict.insert("CFBundleShortVersionString".into(), short.into());
    Value::Dictionary(dict).to_file_xml(&path).unwrap();
    path
}

/// `OpenLogi`'s settings: no scheduled checks, automatic installs allowed, no
/// profiling, and the signature checked before unpacking.
fn openlogi() -> SparkleSettings {
    SparkleSettings::new(
        "https://updates.openlogi.org/channels/stable/appcast-arm64.xml",
        KEY,
    )
    .unwrap()
    .enable_automatic_checks(false)
    .allows_automatic_updates(true)
    .enable_system_profiling(false)
    .verify_update_before_extraction(true)
}

#[test]
fn writes_and_reads_back_every_key() {
    let dir = tempfile::tempdir().unwrap();
    let plist = info_plist(dir.path(), "0.8.14", "0.8.14");
    let settings = openlogi()
        .scheduled_check_interval(86_400)
        .allowed_url_schemes(vec!["https".into()])
        .set("SUCustom", "x");
    settings.apply(&plist).unwrap();
    let keys: Vec<String> = settings.entries().into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        [
            "SUFeedURL",
            "SUPublicEDKey",
            "SUEnableAutomaticChecks",
            "SUAllowsAutomaticUpdates",
            "SUScheduledCheckInterval",
            "SUEnableSystemProfiling",
            "SUVerifyUpdateBeforeExtraction",
            "SUAllowedURLSchemes",
            "SUCustom",
        ]
    );
    let root = Value::from_file(&plist).unwrap();
    let dict = root.as_dictionary().unwrap();
    assert_eq!(
        dict.get("CFBundleIdentifier").and_then(Value::as_string),
        Some("org.example.App")
    );
    assert_eq!(
        dict.get("SUEnableAutomaticChecks")
            .and_then(Value::as_boolean),
        Some(false)
    );
    assert_eq!(
        dict.get("SUScheduledCheckInterval")
            .and_then(Value::as_unsigned_integer),
        Some(86_400)
    );
    settings.check(&plist).unwrap();
    let other = SparkleSettings::new("https://elsewhere/appcast.xml", KEY).unwrap();
    assert!(
        other
            .check(&plist)
            .unwrap_err()
            .to_string()
            .contains("SUFeedURL")
    );
}

#[test]
fn refuses_a_bundle_version_sparkle_would_misorder() {
    let dir = tempfile::tempdir().unwrap();
    let plist = info_plist(dir.path(), "20261018.1200", "0.8.14");
    let err = openlogi().apply(&plist).unwrap_err();
    assert!(err.to_string().contains("CFBundleVersion"), "{err}");
    openlogi().allow_any_bundle_version().apply(&plist).unwrap();
}

#[test]
fn refuses_a_key_that_is_not_a_public_key() {
    assert!(SparkleSettings::new("https://e", "not base64!").is_err());
    let err = SparkleSettings::new("https://e", "AAAA").unwrap_err();
    assert!(err.to_string().contains("3 bytes"), "{err}");
}

#[test]
fn signs_inside_out_with_or_without_xpc_services() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("App.app");
    assert_eq!(code_inside_out(&app), Vec::<PathBuf>::new());
    let b = framework(&app).join("Versions/B");
    fs::create_dir_all(b.join("Updater.app")).unwrap();
    fs::write(b.join("Autoupdate"), "").unwrap();
    let names = |app: &Path| -> Vec<String> {
        code_inside_out(app)
            .iter()
            // `join` writes `\` on Windows; compare with `/` everywhere.
            .map(|p| {
                p.strip_prefix(app)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    };
    let f = "Contents/Frameworks/Sparkle.framework";
    assert_eq!(
        names(&app),
        [
            format!("{f}/Versions/B/Autoupdate"),
            format!("{f}/Versions/B/Updater.app"),
            f.to_string()
        ]
    );
    fs::create_dir_all(b.join("XPCServices/Downloader.xpc")).unwrap();
    fs::create_dir_all(b.join("XPCServices/Installer.xpc")).unwrap();
    assert_eq!(
        names(&app)[..2],
        [
            format!("{f}/Versions/B/XPCServices/Installer.xpc"),
            format!("{f}/Versions/B/XPCServices/Downloader.xpc"),
        ]
    );
}

/// A minimal Sparkle release tree with the framework's version links.
#[cfg(target_os = "macos")]
fn fake_distribution(root: &Path) -> SparkleDistribution {
    use std::os::unix::fs::symlink;
    let framework = root.join("Sparkle.framework");
    let b = framework.join("Versions/B");
    fs::create_dir_all(b.join("Resources")).unwrap();
    fs::create_dir_all(b.join("XPCServices/Installer.xpc")).unwrap();
    fs::write(b.join("Sparkle"), "bin").unwrap();
    symlink("B", framework.join("Versions/Current")).unwrap();
    symlink("Versions/Current/Sparkle", framework.join("Sparkle")).unwrap();
    symlink(
        "Versions/Current/XPCServices",
        framework.join("XPCServices"),
    )
    .unwrap();
    fs::write(root.join("LICENSE"), "MIT").unwrap();
    SparkleDistribution::from_dir(root).unwrap()
}

#[cfg(target_os = "macos")]
#[test]
fn embeds_with_links_intact_and_without_xpc_services() {
    let dir = tempfile::tempdir().unwrap();
    let dist = fake_distribution(&dir.path().join("dist"));
    let app = dir.path().join("App.app");
    fs::create_dir_all(framework(&app).join("Stale")).unwrap();
    let embedded = embed(&app, &dist, &EmbedOptions::default()).unwrap();
    assert!(!embedded.join("Stale").exists());
    assert!(
        fs::symlink_metadata(embedded.join("Sparkle"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::symlink_metadata(embedded.join("XPCServices")).is_err());
    assert!(!embedded.join("Versions/B/XPCServices").exists());
    assert_eq!(
        fs::read_to_string(embedded.join("Versions/B/Resources/LICENSE")).unwrap(),
        "MIT"
    );

    let kept = embed(
        &app,
        &dist,
        &EmbedOptions::default()
            .keep_xpc_services()
            .without_license(),
    )
    .unwrap();
    assert!(kept.join("Versions/B/XPCServices/Installer.xpc").is_dir());
    assert!(!kept.join("Versions/B/Resources/LICENSE").exists());

    let check = BundleCheck {
        signatures: false,
        ..BundleCheck::new(None)
    };
    let err = verify_bundle(&app, &check).unwrap_err();
    assert!(err.to_string().contains("XPC"), "{err}");
}
