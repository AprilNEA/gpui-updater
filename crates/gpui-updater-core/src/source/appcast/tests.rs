use semver::Version;

use super::host::{HostFacts, OsMatcher, product_version_from_plist};
use super::{AppcastSource, AppcastVersion, asset_name, feed};
use crate::error::Error;
use crate::release::Release;

fn mac_source() -> AppcastSource {
    AppcastSource::new("https://example.test/appcast.xml").os("macos")
}

fn host(system: Option<(u64, u64)>, arm64: bool) -> HostFacts {
    HostFacts {
        system_version: system.map(|(major, minor)| Version::new(major, minor, 0)),
        supports_arm64: arm64,
    }
}

fn pick(source: &AppcastSource, xml: &str, host: &HostFacts) -> crate::Result<Release> {
    source.select(feed::parse(xml).unwrap(), host)
}

fn feed_of(items: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"
     xmlns:gpui-updater="https://github.com/AprilNEA/gpui-updater">
  <channel>
    <title>App</title>
    {items}
  </channel>
</rss>"#
    )
}

fn item(short: &str, extra: &str, enclosure_attrs: &str) -> String {
    format!(
        r#"<item>
      <title>{short}</title>
      <sparkle:version>{short}</sparkle:version>
      <sparkle:shortVersionString>{short}</sparkle:shortVersionString>
      {extra}
      <enclosure url="https://dl.example.test/App-{short}.dmg" length="42" type="application/octet-stream" {enclosure_attrs}/>
    </item>"#
    )
}

#[test]
fn picks_the_highest_version_and_maps_sparkle_fields() {
    let xml = feed_of(&format!(
        "{}{}{}",
        item("1.1.0", "", r#"sparkle:edSignature="old""#),
        item(
            "1.3.0",
            "<description><![CDATA[<p>New</p>]]></description>",
            r#"sparkle:edSignature=" c2ln " gpui-updater:sha256="ABCD""#,
        ),
        item("1.2.0", "", ""),
    ));
    let release = pick(&mac_source(), &xml, &host(None, true)).unwrap();
    assert_eq!(release.version, Version::new(1, 3, 0));
    assert_eq!(release.asset.name, "App-1.3.0.dmg");
    assert_eq!(release.asset.url, "https://dl.example.test/App-1.3.0.dmg");
    assert_eq!(release.asset.size, 42);
    assert_eq!(release.notes.as_deref(), Some("<p>New</p>"));
    assert_eq!(release.ed25519_signature.as_deref(), Some("c2ln"));
    assert_eq!(release.sha256.as_deref(), Some("abcd"));
}

/// `tests/fixtures/sparkle-2.10-generate_appcast.xml` is the unedited output of
/// Sparkle 2.10.0's `generate_appcast` (including its trailing signed-feed
/// comment), run over a zipped copy of Sparkle's test app.
#[test]
fn parses_feeds_written_by_sparkle_generate_appcast() {
    let xml = include_str!("../../../tests/fixtures/sparkle-2.10-generate_appcast.xml");
    let release = pick(&mac_source(), xml, &host(Some((15, 0)), true)).unwrap();
    assert_eq!(release.version, Version::new(1, 5, 1));
    assert_eq!(release.asset.name, "TestApp-1.5.1.zip");
    assert_eq!(
        release.asset.url,
        "https://dl.example.test/releases/TestApp-1.5.1.zip"
    );
    assert_eq!(release.asset.size, 3_393_998);
    assert_eq!(
        release.ed25519_signature.as_deref(),
        Some(
            "AGa17JWIFL1V6vPcBT5H94vOLjUrBQMdkD3MGYCEUC8OTON+w0NiZgjUTbrDv6K1LgjT9nQRAato+6oGuW7BBw=="
        )
    );
    // The item requires macOS 12.
    assert!(pick(&mac_source(), xml, &host(Some((11, 7)), true)).is_err());
}

#[test]
fn falls_back_to_the_release_notes_link() {
    let xml = feed_of(&item(
        "1.0.0",
        r#"<sparkle:releaseNotesLink>https://example.test/1.0.html</sparkle:releaseNotesLink>
           <sparkle:releaseNotesLink xml:lang="de">https://example.test/de.html</sparkle:releaseNotesLink>"#,
        "",
    ));
    let release = pick(&mac_source(), &xml, &host(None, true)).unwrap();
    assert_eq!(
        release.notes.as_deref(),
        Some("https://example.test/1.0.html")
    );
}

#[test]
fn skips_informational_items_and_unlisted_channels() {
    let xml = feed_of(&format!(
        "{}{}{}",
        item("1.0.0", "", ""),
        item("2.0.0", "<sparkle:informationalUpdate/>", ""),
        item(
            "3.0.0-beta.1",
            "<sparkle:channel>beta</sparkle:channel>",
            ""
        ),
    ));
    let stable = pick(&mac_source(), &xml, &host(None, true)).unwrap();
    assert_eq!(stable.version, Version::new(1, 0, 0));

    let beta = pick(&mac_source().channel("beta"), &xml, &host(None, true)).unwrap();
    assert_eq!(beta.version.to_string(), "3.0.0-beta.1");
}

#[test]
fn honours_system_version_bounds_only_when_known() {
    let xml = feed_of(&format!(
        "{}{}",
        item(
            "1.0.0",
            "<sparkle:minimumSystemVersion>11.0</sparkle:minimumSystemVersion>",
            ""
        ),
        item(
            "2.0.0",
            "<sparkle:minimumSystemVersion>14.0</sparkle:minimumSystemVersion>
             <sparkle:maximumSystemVersion>15.9</sparkle:maximumSystemVersion>",
            ""
        ),
    ));
    let on = |system| {
        pick(&mac_source(), &xml, &host(system, true))
            .unwrap()
            .version
            .major
    };
    assert_eq!(on(Some((13, 6))), 1);
    assert_eq!(on(Some((15, 1))), 2);
    assert_eq!(on(Some((26, 0))), 1);
    assert_eq!(on(None), 2);
}

#[test]
fn arm64_only_items_need_arm64_hardware() {
    let xml = feed_of(&format!(
        "{}{}",
        item("1.0.0", "", ""),
        item(
            "2.0.0",
            "<sparkle:hardwareRequirements>ARM64</sparkle:hardwareRequirements>",
            ""
        ),
    ));
    let intel = pick(&mac_source(), &xml, &host(None, false)).unwrap();
    assert_eq!(intel.version.major, 1);
    let apple_silicon = pick(&mac_source(), &xml, &host(None, true)).unwrap();
    assert_eq!(apple_silicon.version.major, 2);
}

#[test]
fn macos_treats_a_missing_os_as_macos_and_ignores_other_platforms() {
    let xml = feed_of(
        r#"<item>
      <sparkle:version>5</sparkle:version>
      <sparkle:shortVersionString>1.5</sparkle:shortVersionString>
      <enclosure url="https://dl.example.test/App.exe" sparkle:os="windows-x64" length="1"/>
      <enclosure url="https://dl.example.test/App.dmg" length="2"/>
    </item>"#,
    );
    let release = pick(&mac_source(), &xml, &host(None, true)).unwrap();
    assert_eq!(release.asset.name, "App.dmg");
    assert_eq!(release.version, Version::new(1, 5, 0));
}

#[test]
fn windows_prefers_the_arch_specific_enclosure() {
    let source = |arch| AppcastSource {
        os: OsMatcher::for_platform("windows", arch),
        ..mac_source()
    };
    let xml = feed_of(
        r#"<item>
      <sparkle:shortVersionString>1.0.0</sparkle:shortVersionString>
      <enclosure url="https://dl.example.test/App.dmg" length="1"/>
      <enclosure url="https://dl.example.test/App-any.exe" sparkle:os="windows" length="2"/>
      <enclosure url="https://dl.example.test/App-arm64.exe" sparkle:os="windows-arm64" length="3"/>
    </item>"#,
    );
    let name = |arch| {
        pick(&source(arch), &xml, &host(None, true))
            .unwrap()
            .asset
            .name
    };
    assert_eq!(name("aarch64"), "App-arm64.exe");
    assert_eq!(name("x86_64"), "App-any.exe");

    // A WinSparkle-only feed without `sparkle:os` still works on Windows.
    let bare = feed_of(&item("1.0.0", "", ""));
    assert!(pick(&source("x86_64"), &bare, &host(None, true)).is_ok());
}

#[test]
fn linux_never_takes_an_enclosure_without_os() {
    let source = AppcastSource {
        os: OsMatcher::for_platform("linux", "x86_64"),
        ..mac_source()
    };
    let bare = feed_of(&item("1.0.0", "", ""));
    assert!(matches!(
        pick(&source, &bare, &host(None, true)),
        Err(Error::NoMatchingAsset { .. })
    ));
    let tagged = feed_of(&item("1.0.0", "", r#"sparkle:os="linux-x64""#));
    assert!(pick(&source, &tagged, &host(None, true)).is_ok());
}

#[test]
fn reads_versions_from_old_style_enclosure_attributes() {
    let xml = feed_of(
        r#"<item>
      <enclosure url="https://dl.example.test/App.zip" sparkle:version="12" sparkle:shortVersionString="2.1" length="7"/>
    </item>"#,
    );
    let release = pick(&mac_source(), &xml, &host(None, true)).unwrap();
    assert_eq!(release.version, Version::new(2, 1, 0));

    let by_build = mac_source().version_field(AppcastVersion::BundleVersion);
    assert_eq!(
        pick(&by_build, &xml, &host(None, true)).unwrap().version,
        Version::new(12, 0, 0)
    );
}

#[test]
fn version_resolution_follows_the_chosen_field() {
    let short = AppcastVersion::ShortVersionString;
    assert_eq!(
        short.resolve(Some("1.2"), Some("99")).unwrap(),
        Version::new(1, 2, 0)
    );
    assert_eq!(
        short.resolve(None, Some("3")).unwrap(),
        Version::new(3, 0, 0)
    );
    assert_eq!(
        short.resolve(Some(" "), Some("3")).unwrap(),
        Version::new(3, 0, 0)
    );
    // A present but unparsable short version does not silently fall back.
    assert!(short.resolve(Some("2.0 Beta"), Some("3")).is_err());

    let bundle = AppcastVersion::BundleVersion;
    assert!(bundle.resolve(Some("1.2.0"), None).is_err());
    assert_eq!(
        bundle.resolve(Some("1.2.0"), Some("4.5")).unwrap(),
        Version::new(4, 5, 0)
    );
}

#[test]
fn unorderable_versions_are_skipped_not_fatal() {
    let xml = feed_of(&format!(
        "{}{}",
        item("1.0.0", "", ""),
        item("2024 Edition", "", "")
    ));
    assert_eq!(
        pick(&mac_source(), &xml, &host(None, true))
            .unwrap()
            .version,
        Version::new(1, 0, 0)
    );
}

#[test]
fn rejects_documents_that_are_not_appcasts() {
    assert!(matches!(feed::parse("not xml"), Err(Error::Parse(_))));
    assert!(matches!(feed::parse("<feed/>"), Err(Error::Parse(_))));
    assert!(matches!(feed::parse("<rss/>"), Err(Error::Parse(_))));
    let empty = feed_of("");
    assert!(matches!(
        pick(&mac_source(), &empty, &host(None, true)),
        Err(Error::NoMatchingAsset { .. })
    ));
}

#[test]
fn asset_names_are_safe_file_names() {
    assert_eq!(
        asset_name("https://x.test/a/App-1.0.dmg?sig=1#frag"),
        "App-1.0.dmg"
    );
    assert_eq!(asset_name("https://x.test/"), "update");
    assert_eq!(asset_name("https://x.test/dir/.."), "update");
    assert_eq!(asset_name(r"https://x.test/a\..\b.dmg"), "update");
}

#[test]
fn reads_product_version_from_system_version_plist() {
    let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>ProductBuildVersion</key>
	<string>24B83</string>
	<key>ProductName</key>
	<string>macOS</string>
	<key>ProductVersion</key>
	<string>15.1</string>
</dict>
</plist>"#;
    assert_eq!(
        product_version_from_plist(plist),
        Some(Version::new(15, 1, 0))
    );
    assert_eq!(product_version_from_plist("<plist><dict/></plist>"), None);
}
