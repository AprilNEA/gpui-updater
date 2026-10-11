//! Parsing a Sparkle appcast (RSS 2.0 with the `sparkle:` namespace) into
//! plain data. Selection rules live in the parent module.

use roxmltree::{Document, Node};

use crate::error::{Error, Result};

/// The XML namespace Sparkle's elements and attributes live in.
pub(super) const SPARKLE_NS: &str = "http://www.andymatuschak.org/xml-namespaces/sparkle";

/// The XML namespace for gpui-updater's own appcast extensions (currently the
/// `sha256` enclosure attribute). Sparkle ignores attributes it does not know.
pub(super) const GPUI_UPDATER_NS: &str = "https://github.com/AprilNEA/gpui-updater";

/// One `<item>` of the feed.
#[derive(Debug, Default)]
pub(super) struct Item {
    pub(super) short_version: Option<String>,
    pub(super) bundle_version: Option<String>,
    pub(super) channel: Option<String>,
    pub(super) minimum_system_version: Option<String>,
    pub(super) maximum_system_version: Option<String>,
    /// Lowercased entries of `sparkle:hardwareRequirements` (e.g. `arm64`).
    pub(super) hardware_requirements: Vec<String>,
    pub(super) informational: bool,
    pub(super) description: Option<String>,
    pub(super) release_notes_link: Option<String>,
    pub(super) enclosures: Vec<Enclosure>,
}

/// One `<enclosure>` of an item. Items usually carry one; feeds shared with
/// WinSparkle may carry one per platform.
#[derive(Debug, Default)]
pub(super) struct Enclosure {
    pub(super) url: String,
    pub(super) length: u64,
    pub(super) os: Option<String>,
    pub(super) ed_signature: Option<String>,
    /// Lowercase hex, from `gpui-updater:sha256`.
    pub(super) sha256: Option<String>,
    /// Older feeds put the versions on the enclosure instead of the item.
    pub(super) short_version: Option<String>,
    pub(super) bundle_version: Option<String>,
}

/// Parse a feed into its items, in document order.
pub(super) fn parse(xml: &str) -> Result<Vec<Item>> {
    let doc =
        Document::parse(xml).map_err(|e| Error::Parse(format!("appcast is not valid XML: {e}")))?;
    let root = doc.root_element();
    if !is_rss(root, "rss") {
        return Err(Error::Parse(format!(
            "appcast root element is <{}>, expected <rss>",
            root.tag_name().name()
        )));
    }
    let channel = root
        .children()
        .find(|n| is_rss(*n, "channel"))
        .ok_or_else(|| Error::Parse("appcast has no <channel>".into()))?;
    Ok(channel
        .children()
        .filter(|n| is_rss(*n, "item"))
        .map(parse_item)
        .collect())
}

/// An RSS element (not in Sparkle's namespace) with the given local name.
fn is_rss(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element()
        && node.tag_name().name() == name
        && node.tag_name().namespace() != Some(SPARKLE_NS)
}

fn parse_item(node: Node<'_, '_>) -> Item {
    let mut item = Item::default();
    for child in node.children().filter(Node::is_element) {
        let tag = child.tag_name();
        match (tag.namespace(), tag.name()) {
            (Some(SPARKLE_NS), "version") => item.bundle_version = text(child),
            (Some(SPARKLE_NS), "shortVersionString") => item.short_version = text(child),
            (Some(SPARKLE_NS), "channel") => item.channel = text(child),
            (Some(SPARKLE_NS), "minimumSystemVersion") => {
                item.minimum_system_version = text(child);
            }
            (Some(SPARKLE_NS), "maximumSystemVersion") => {
                item.maximum_system_version = text(child);
            }
            (Some(SPARKLE_NS), "hardwareRequirements") => {
                item.hardware_requirements = text(child)
                    .map(|list| {
                        list.split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_ascii_lowercase)
                            .collect()
                    })
                    .unwrap_or_default();
            }
            (Some(SPARKLE_NS), "informationalUpdate") => item.informational = true,
            (Some(SPARKLE_NS), "releaseNotesLink") => {
                // Localized feeds repeat the link per `xml:lang`; keep the first.
                if item.release_notes_link.is_none() {
                    item.release_notes_link = text(child);
                }
            }
            (Some(SPARKLE_NS), _) => {}
            (_, "description") => item.description = text(child),
            (_, "enclosure") => item.enclosures.extend(parse_enclosure(child)),
            _ => {}
        }
    }
    item
}

fn parse_enclosure(node: Node<'_, '_>) -> Option<Enclosure> {
    let url = node
        .attribute("url")
        .map(str::trim)
        .filter(|u| !u.is_empty())?;
    Some(Enclosure {
        url: url.to_string(),
        length: node
            .attribute("length")
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or(0),
        os: attribute(node, SPARKLE_NS, "os"),
        ed_signature: attribute(node, SPARKLE_NS, "edSignature"),
        sha256: attribute(node, GPUI_UPDATER_NS, "sha256").map(|s| s.to_ascii_lowercase()),
        short_version: attribute(node, SPARKLE_NS, "shortVersionString"),
        bundle_version: attribute(node, SPARKLE_NS, "version"),
    })
}

/// A namespaced attribute, trimmed, with blank values treated as absent.
fn attribute(node: Node<'_, '_>, namespace: &str, name: &str) -> Option<String> {
    node.attribute((namespace, name))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The element's direct text (including CDATA), trimmed, with blank text
/// treated as absent.
fn text(node: Node<'_, '_>) -> Option<String> {
    let joined: String = node
        .children()
        .filter(Node::is_text)
        .filter_map(|n| n.text())
        .collect();
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}
