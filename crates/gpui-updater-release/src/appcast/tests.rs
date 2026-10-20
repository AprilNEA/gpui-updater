#![allow(clippy::unwrap_used)]

use gpui_updater_core::{AppcastSource, UpdateSource as _, Version};

use super::*;

/// What `OpenLogi`'s own `xtask release appcast` wrote before it moved here,
/// byte for byte (signature and date fixed).
const OPENLOGI_0_8_14: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>OpenLogi</title>
    <link>https://updates.openlogi.org/channels/stable/appcast-arm64.xml</link>
    <description>OpenLogi updates (stable channel)</description>
    <item>
      <title>OpenLogi 0.8.14</title>
      <pubDate>Sat, 18 Oct 2026 12:00:00 +0000</pubDate>
      <sparkle:version>0.8.14</sparkle:version>
      <sparkle:shortVersionString>0.8.14</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>13.0</sparkle:minimumSystemVersion>
      <link>https://github.com/AprilNEA/OpenLogi/releases/tag/v0.8.14</link>
      <sparkle:fullReleaseNotesLink>https://github.com/AprilNEA/OpenLogi/releases/tag/v0.8.14</sparkle:fullReleaseNotesLink>
      <description sparkle:format="markdown"><![CDATA[- a ]]]]><![CDATA[> b]]></description>
      <enclosure url="https://updates.openlogi.org/releases/v0.8.14/OpenLogi-v0.8.14-macos-arm64.dmg" length="20741647" type="application/x-apple-diskimage" sparkle:edSignature="c2ln"/>
    </item>
  </channel>
</rss>
"#;

fn openlogi_item() -> AppcastItem {
    let page = "https://github.com/AprilNEA/OpenLogi/releases/tag/v0.8.14";
    let mut enclosure = Enclosure::new(
        "https://updates.openlogi.org/releases/v0.8.14/OpenLogi-v0.8.14-macos-arm64.dmg",
        20_741_647,
        "application/x-apple-diskimage",
    );
    enclosure.ed_signature = Some("c2ln".into());
    AppcastItem::new("0.8.14", enclosure)
        .title("OpenLogi 0.8.14")
        .pub_date(Some("Sat, 18 Oct 2026 12:00:00 +0000".into()))
        .short_version("0.8.14")
        .minimum_system_version("13.0")
        .link(page)
        .full_release_notes_link(page)
        .notes(ReleaseNotes::Markdown("- a ]]> b".into()))
}

#[test]
fn renders_the_openlogi_feed_byte_for_byte() {
    let feed = Appcast::new(
        "OpenLogi",
        "https://updates.openlogi.org/channels/stable/appcast-arm64.xml",
    )
    .description("OpenLogi updates (stable channel)")
    .item(openlogi_item());
    assert_eq!(feed.render(), OPENLOGI_0_8_14);
}

#[test]
fn renders_every_option_and_the_deltas() {
    let item = openlogi_item()
        .channel("beta")
        .maximum_system_version("26.0")
        .hardware_requirements("arm64")
        .minimum_autoupdate_version("0.8.0")
        .minimum_update_version("0.7.0")
        .ignore_skipped_upgrades_below_version("0.8.10")
        .critical(Critical::Below("0.8.12".into()))
        .informational(Informational::Below(vec!["0.5.0".into(), "0.6.0".into()]))
        .phased_rollout_interval(86_400)
        .release_notes_link("https://e/notes?a=1&b=2")
        .delta(
            DeltaEnclosure::new("https://e/d.delta", "0.8.13", 1_731_598)
                .old_sparkle(Some(1_234), Some("de,fr".into())),
        )
        .extra_xml("<sparkle:tags><sparkle:criticalUpdate/></sparkle:tags>");
    let xml = Appcast::new("A & B", "https://e/feed.xml")
        .language("en")
        .item(item)
        .render();
    let doc = roxmltree::Document::parse(&xml).unwrap();
    let ns = SPARKLE_NAMESPACE;
    let find = |name: &str| {
        doc.descendants()
            .find(|n| n.tag_name().name() == name && n.tag_name().namespace() == Some(ns))
            .unwrap()
    };
    assert_eq!(find("channel").text(), Some("beta"));
    assert_eq!(
        find("criticalUpdate").attribute((ns, "version")),
        Some("0.8.12")
    );
    let below: Vec<_> = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "belowVersion")
        .map(|n| n.text().unwrap())
        .collect();
    assert_eq!(below, ["0.5.0", "0.6.0"]);
    assert_eq!(find("phasedRolloutInterval").text(), Some("86400"));
    assert_eq!(
        find("releaseNotesLink").text(),
        Some("https://e/notes?a=1&b=2")
    );
    let delta = find("deltas")
        .children()
        .find(roxmltree::Node::is_element)
        .unwrap();
    assert_eq!(delta.attribute((ns, "deltaFrom")), Some("0.8.13"));
    assert_eq!(delta.attribute("length"), Some("1731598"));
    assert_eq!(delta.attribute("type"), Some("application/octet-stream"));
    assert_eq!(
        delta.attribute((ns, "deltaFromSparkleExecutableSize")),
        Some("1234")
    );
    assert_eq!(
        delta.attribute((ns, "deltaFromSparkleLocales")),
        Some("de,fr")
    );
    let description = doc
        .descendants()
        .find(|n| n.tag_name().name() == "description")
        .unwrap();
    assert_eq!(description.text(), Some("- a ]]> b"));
    assert!(doc.descendants().any(|n| n.tag_name().name() == "tags"));
    assert_eq!(
        doc.descendants()
            .find(|n| n.tag_name().name() == "title")
            .unwrap()
            .text(),
        Some("A & B")
    );
}

#[test]
fn signs_enclosures_and_deltas_from_files() {
    let dir = tempfile::tempdir().unwrap();
    let dmg = dir.path().join("App.dmg");
    let delta = dir.path().join("App.delta");
    std::fs::write(&dmg, b"dmg").unwrap();
    std::fs::write(&delta, b"delta").unwrap();
    let key = SparkleKey::generate();
    let enclosure = Enclosure::for_file(&dmg, "https://e/App.dmg", Some(&key)).unwrap();
    assert_eq!(
        (enclosure.length, enclosure.mime_type.as_str()),
        (3, "application/x-apple-diskimage")
    );
    gpui_updater_core::verify::verify_ed25519(
        &dmg,
        &key.public_key_base64(),
        enclosure.ed_signature.as_deref().unwrap(),
    )
    .unwrap();
    let delta =
        DeltaEnclosure::for_file(&delta, "https://e/App.delta", "1.0.0", Some(&key)).unwrap();
    assert_eq!(delta.length, 5);
    assert!(delta.ed_signature.is_some());
}

#[test]
fn sparkle_version_rule() {
    ensure_sparkle_version("0.8.14", 3, 3).unwrap();
    ensure_sparkle_version("12", 1, 4).unwrap();
    for bad in ["1.2.0-rc.1", "1.2", "1..2", "v1.2.3", "1.2.3.4"] {
        assert!(ensure_sparkle_version(bad, 3, 3).is_err(), "{bad}");
    }
}

#[test]
fn the_core_source_reads_what_the_writer_writes() {
    let key = SparkleKey::generate();
    let dir = tempfile::tempdir().unwrap();
    let dmg = dir.path().join("App.dmg");
    std::fs::write(&dmg, b"dmg").unwrap();
    let enclosure = Enclosure::for_file(&dmg, "https://e/App-1.3.0.dmg", Some(&key))
        .unwrap()
        .os("macos");
    let new = AppcastItem::new("130", enclosure.clone()).short_version("1.3.0");
    let old = AppcastItem::new("120", enclosure).short_version("1.2.0");
    let beta = AppcastItem::new(
        "200",
        Enclosure::new("https://e/b.dmg", 1, "application/x-apple-diskimage"),
    )
    .short_version("2.0.0")
    .channel("beta");
    let feed = Appcast::new("App", "https://e/appcast.xml")
        .item(beta)
        .item(new)
        .item(old);
    let base = crate::test_server::serve(vec![("/appcast.xml".into(), feed.render().into_bytes())]);
    // The enclosure is for macOS; select it on whichever host runs the test.
    let release = AppcastSource::new(format!("{base}/appcast.xml"))
        .os("macos")
        .fetch_latest()
        .unwrap();
    assert_eq!(release.version, Version::new(1, 3, 0));
    assert_eq!(release.asset.url, "https://e/App-1.3.0.dmg");
    assert_eq!(release.asset.size, 3);
    gpui_updater_core::verify::verify_ed25519(
        &dmg,
        &key.public_key_base64(),
        release.ed25519_signature.as_deref().unwrap(),
    )
    .unwrap();
    let beta = AppcastSource::new(format!("{base}/appcast.xml"))
        .os("macos")
        .channel("beta")
        .fetch_latest()
        .unwrap();
    assert_eq!(beta.version, Version::new(2, 0, 0));
}
