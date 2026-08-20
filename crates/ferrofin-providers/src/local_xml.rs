//! `collection.xml` / `playlist.xml` — port of
//! `MediaBrowser.LocalMetadata`'s `BoxSetXmlParser`/`PlaylistXmlParser` and
//! `BoxSetXmlSaver`/`PlaylistXmlSaver`.
//!
//! A Jellyfin-authored collection or playlist folder carries its membership in
//! an XML file next to the media, in a dialect distinct from the Kodi NFO one
//! ([`crate::xbmc`]): an `<Item>` root with `LocalTitle`/`Overview`/`Genres`
//! and a `<CollectionItems>`/`<PlaylistItems>` list of `<Path>`/`<ItemId>`
//! links. Reading it is what lets Ferrofin adopt a library Jellyfin populated;
//! writing it is what keeps the reverse true.
//!
//! # Not yet reachable from the server
//!
//! Both C# savers write to `Path.Combine(item.Path, …)`, and both parsers run
//! against an item resolved from a folder on disk. Jellyfin gives a box set
//! such a folder — `{DataPath}/collections/{Name} [boxset]/collection.xml` —
//! so an **adopted** Jellyfin database does carry those paths and those files.
//! Ferrofin, however, creates a `BoxSet` or `Playlist` as a **pathless** DB row
//! (`collection_manager`'s `insert_named_item` writes no `Path`) and its
//! scanner resolves no collection/playlist folders, so nothing in the server
//! calls into this module today.
//!
//! Nothing is lost by that: membership lives in `BaseItems."Data"`, which is
//! Jellyfin's own DB source of truth and is what the drop-in round trip
//! actually exercises — an adopted collection reads back correctly from the
//! row whether or not its `collection.xml` was parsed.
//!
//! This module is the complete, tested reader/writer pair, ready for the day
//! Ferrofin materializes folder-backed containers. It is deliberately NOT
//! wired to a synthetic path: writing `collection.xml` somewhere Jellyfin
//! would not look for it would be worse than not writing it.

use std::fmt::Write as _;

use crate::xbmc::xml_reader::XmlCursor;

/// One entry of a `<CollectionItems>`/`<PlaylistItems>` list — the C#
/// `LinkedChild`, restricted to the two fields these files carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalLinkedChild {
    /// The child's on-disk path, Jellyfin's primary link key.
    pub path: Option<String>,
    /// The child's item id, when the file links by id (playlists).
    pub item_id: Option<String>,
}

impl LocalLinkedChild {
    /// Whether the entry links to anything at all — C# drops a `LinkedChild`
    /// with neither a path nor an id.
    fn is_linked(&self) -> bool {
        self.path.is_some() || self.item_id.is_some()
    }
}

/// A parsed `collection.xml` / `playlist.xml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalContainerXml {
    /// `<LocalTitle>` — the container's display name.
    pub name: Option<String>,
    /// `<Overview>`.
    pub overview: Option<String>,
    /// `<PlaylistMediaType>` — a playlist's media type.
    pub playlist_media_type: Option<String>,
    /// The linked children, in file order.
    pub children: Vec<LocalLinkedChild>,
}

/// Parses a `collection.xml` (`BoxSetXmlParser`) or `playlist.xml`
/// (`PlaylistXmlParser`) document.
///
/// Both dialects are read by one pass: the element names they add on top of the
/// shared base (`CollectionItem` vs `PlaylistItem`) are simply both accepted,
/// since a document only ever carries one of them.
#[must_use]
pub fn parse_container_xml(xml: &str) -> Option<LocalContainerXml> {
    let mut out = LocalContainerXml::default();
    let Ok(mut cursor) = XmlCursor::new(xml) else {
        return None;
    };
    cursor.move_to_content();
    cursor.read();
    while !cursor.eof() {
        if !cursor.is_element() {
            cursor.read();
            continue;
        }
        match cursor.name() {
            "CollectionItems" | "PlaylistItems" => {
                // Container elements: descend rather than read their content,
                // which would swallow every child into one string.
                cursor.read();
            }
            "CollectionItem" | "PlaylistItem" => {
                let mut sub = cursor.read_subtree();
                if let Some(child) = read_linked_child(&mut sub) {
                    out.children.push(child);
                }
                cursor.skip();
            }
            _ => {
                let name = cursor.name().to_owned();
                let value = cursor.read_element_content_as_string();
                let value = value.trim();
                if value.is_empty() {
                    continue;
                }
                match name.as_str() {
                    "LocalTitle" => out.name = Some(value.to_owned()),
                    "Overview" => out.overview = Some(value.to_owned()),
                    "PlaylistMediaType" => out.playlist_media_type = Some(value.to_owned()),
                    _ => {}
                }
            }
        }
    }
    Some(out)
}

/// Reads one `<CollectionItem>`/`<PlaylistItem>` subtree — port of
/// `BaseItemXmlParser.GetLinkedChild`.
fn read_linked_child(sub: &mut XmlCursor) -> Option<LocalLinkedChild> {
    let mut child = LocalLinkedChild::default();
    sub.read();
    while !sub.eof() {
        if !sub.is_element() {
            sub.read();
            continue;
        }
        let name = sub.name().to_owned();
        let value = sub.read_element_content_as_string();
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match name.as_str() {
            "Path" => child.path = Some(value.to_owned()),
            "ItemId" => child.item_id = Some(value.to_owned()),
            _ => {}
        }
    }
    child.is_linked().then_some(child)
}

/// Serializes a `collection.xml` (`BoxSetXmlSaver`).
#[must_use]
pub fn save_collection_xml(container: &LocalContainerXml) -> String {
    write_container_xml(container, "CollectionItems", "CollectionItem", false)
}

/// Serializes a `playlist.xml` (`PlaylistXmlSaver`), which adds
/// `<PlaylistMediaType>` before the item list.
#[must_use]
pub fn save_playlist_xml(container: &LocalContainerXml) -> String {
    write_container_xml(container, "PlaylistItems", "PlaylistItem", true)
}

/// The shared writer for both dialects.
fn write_container_xml(
    container: &LocalContainerXml,
    list_element: &str,
    item_element: &str,
    write_media_type: bool,
) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<Item>\n");
    if let Some(overview) = container.overview.as_deref().filter(|v| !v.is_empty()) {
        push_element(&mut xml, 2, "Overview", overview);
    }
    if let Some(name) = container.name.as_deref().filter(|v| !v.is_empty()) {
        push_element(&mut xml, 2, "LocalTitle", name);
    }
    if !container.children.is_empty() {
        let _ = writeln!(xml, "  <{list_element}>");
        for child in &container.children {
            let _ = writeln!(xml, "    <{item_element}>");
            // C# `AddLinkedChildren` writes only `<Path>` — it resolves ids to
            // paths first. The reader accepts both, as upstream's does.
            if let Some(path) = child.path.as_deref().filter(|v| !v.is_empty()) {
                push_element(&mut xml, 6, "Path", path);
            }
            let _ = writeln!(xml, "    </{item_element}>");
        }
        let _ = writeln!(xml, "  </{list_element}>");
    }
    // C# emits this from `WriteCustomElementsAsync`, which runs *after*
    // `AddCommonNodesAsync` writes the item list.
    if write_media_type
        && let Some(media_type) = container
            .playlist_media_type
            .as_deref()
            .filter(|v| !v.is_empty())
    {
        push_element(&mut xml, 2, "PlaylistMediaType", media_type);
    }
    xml.push_str("</Item>");
    xml
}

/// Appends one indented `<name>value</name>` line, escaping the value.
fn push_element(xml: &mut String, indent: usize, name: &str, value: &str) {
    xml.push_str(&" ".repeat(indent));
    let _ = writeln!(xml, "<{name}>{}</{name}>", escape(value));
}

/// Escapes the five XML entities.
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_collection_xml_yields_its_title_and_members() {
        let parsed = parse_container_xml(
            r"<Item>
                <LocalTitle>The Matrix Collection</LocalTitle>
                <Overview>Neo.</Overview>
                <CollectionItems>
                  <CollectionItem><Path>/media/The Matrix.mkv</Path></CollectionItem>
                  <CollectionItem><Path>/media/Reloaded.mkv</Path></CollectionItem>
                </CollectionItems>
              </Item>",
        )
        .expect("parse");
        assert_eq!(parsed.name.as_deref(), Some("The Matrix Collection"));
        assert_eq!(parsed.overview.as_deref(), Some("Neo."));
        assert_eq!(parsed.children.len(), 2);
        assert_eq!(
            parsed.children[0].path.as_deref(),
            Some("/media/The Matrix.mkv")
        );
    }

    #[test]
    fn a_playlist_xml_yields_its_media_type_and_id_links() {
        let parsed = parse_container_xml(
            r"<Item>
                <LocalTitle>Road Trip</LocalTitle>
                <PlaylistMediaType>Audio</PlaylistMediaType>
                <PlaylistItems>
                  <PlaylistItem><ItemId>abc123</ItemId><Path>/music/a.flac</Path></PlaylistItem>
                </PlaylistItems>
              </Item>",
        )
        .expect("parse");
        assert_eq!(parsed.playlist_media_type.as_deref(), Some("Audio"));
        assert_eq!(parsed.children[0].item_id.as_deref(), Some("abc123"));
        assert_eq!(parsed.children[0].path.as_deref(), Some("/music/a.flac"));
    }

    #[test]
    fn an_item_linking_to_nothing_is_dropped() {
        let parsed = parse_container_xml(
            r"<Item><CollectionItems><CollectionItem><Type>Manual</Type></CollectionItem>
              </CollectionItems></Item>",
        )
        .expect("parse");
        assert!(parsed.children.is_empty());
    }

    #[test]
    fn a_collection_survives_a_save_and_reparse() {
        let original = LocalContainerXml {
            name: Some("Bond & Co".into()),
            overview: Some("Shaken <not> stirred".into()),
            playlist_media_type: None,
            children: vec![LocalLinkedChild {
                path: Some("/media/Dr. No.mkv".into()),
                item_id: None,
            }],
        };
        let xml = save_collection_xml(&original);
        let reparsed = parse_container_xml(&xml).expect("reparse");
        assert_eq!(reparsed, original, "{xml}");
    }

    #[test]
    fn a_playlist_survives_a_save_and_reparse() {
        let original = LocalContainerXml {
            name: Some("Road Trip".into()),
            overview: None,
            playlist_media_type: Some("Audio".into()),
            children: vec![
                LocalLinkedChild {
                    path: Some("/music/a.flac".into()),
                    item_id: Some("abc123".into()),
                },
                LocalLinkedChild {
                    path: Some("/music/b.flac".into()),
                    item_id: None,
                },
            ],
        };
        let xml = save_playlist_xml(&original);
        assert!(xml.contains("<PlaylistMediaType>Audio</PlaylistMediaType>"));
        // C# writes the media type from `WriteCustomElementsAsync`, which runs
        // after the common nodes have emitted the item list.
        assert!(
            xml.find("<PlaylistItems>") < xml.find("<PlaylistMediaType>"),
            "the item list comes first: {xml}"
        );
        // Only `<Path>` is written (C# `AddLinkedChildren` resolves ids to
        // paths), so an id does not survive the round trip — the reader still
        // accepts one, because a Jellyfin-written file may carry it.
        let reparsed = parse_container_xml(&xml).expect("reparse");
        assert_eq!(
            reparsed.children,
            vec![
                LocalLinkedChild {
                    path: Some("/music/a.flac".into()),
                    item_id: None,
                },
                LocalLinkedChild {
                    path: Some("/music/b.flac".into()),
                    item_id: None,
                },
            ]
        );
        assert_eq!(reparsed.name, original.name);
        assert_eq!(reparsed.playlist_media_type, original.playlist_media_type);
    }

    #[test]
    fn an_empty_container_writes_no_item_list() {
        let xml = save_collection_xml(&LocalContainerXml {
            name: Some("Empty".into()),
            ..LocalContainerXml::default()
        });
        assert!(!xml.contains("<CollectionItems>"));
        assert!(xml.contains("<LocalTitle>Empty</LocalTitle>"));
    }
}
