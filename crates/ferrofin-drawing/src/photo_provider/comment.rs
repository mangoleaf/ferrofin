//! The comments TagLibSharp 2.3.0 reads beside a photo's EXIF IFD for its
//! combined `ImageTag.Comment`: an XMP packet's, a JPEG's IPTC-IIM caption
//! and a PNG's text chunks. `jpeg_header` reads a JPEG's COM segment.
//!
//! Where TagLib# throws on one of them, `TagLib.File.Create` fails and
//! Jellyfin leaves the photo alone, its EXIF included. These readers return
//! [`Refused`] there, and the caller reads that source as holding no comment
//! and opens the file as it did before; whether such a photo should be left
//! alone is the owner's open question (gl-1973).

use std::io::{BufReader, Read, Seek};

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

use super::utf8_or_latin1;

/// A source TagLibSharp 2.3.0 throws on.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Refused;

/// TagLib#'s combined `ImageTag.Comment` (`Image/CombinedImageTag.cs:621-631`):
/// the first non-empty comment of `comments`, which come in its `AllTags`
/// order (the XMP tag, the IFD tag, then the others), else "".
pub(super) fn first_comment(comments: impl IntoIterator<Item = Option<String>>) -> String {
    comments
        .into_iter()
        .flatten()
        .find(|comment| !comment.is_empty())
        .unwrap_or_default()
}

/// The text of a JPEG COM segment, a PNG text chunk or an IPTC-IIM caption,
/// none of which says its character set.
///
/// TagLib# reads the first two as UTF-8 (`ByteVector.ToString`, which also
/// drops a leading byte-order mark) and the caption as Latin-1. This reads
/// UTF-8 when the bytes are valid UTF-8, else Latin-1, the rule the EXIF
/// comment follows (owner decision, gl-1613): Jellyfin's text wherever it is
/// readable, and the writer's where Jellyfin shows `\u{fffd}` or `cafÃ©`.
pub(super) fn decode_text(bytes: &[u8]) -> String {
    let text = utf8_or_latin1(bytes);
    match without_bom(&text) {
        Some(rest) => rest.to_owned(),
        None => text,
    }
}

/// `text` after the byte-order mark `ByteVector.ToString` drops, a leading
/// U+FEFF or U+FFFE; `None` when it starts with neither.
fn without_bom(text: &str) -> Option<&str> {
    text.strip_prefix(['\u{feff}', '\u{fffe}'])
}

/// The text of an XMP packet TagLib# takes as bytes (`ByteVector.ToString`:
/// UTF-8, a byte-order mark dropped), as a JPEG's APP1 segment, a PNG's iTXt
/// chunk and a TIFF's `XMLPacket` hold it.
fn xmp_text(packet: &[u8]) -> String {
    let text = String::from_utf8_lossy(packet);
    match without_bom(&text) {
        Some(rest) => rest.to_owned(),
        None => text.into_owned(),
    }
}

const ADOBE_X_NS: &str = "adobe:ns:meta/";
const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const EXIF_NS: &str = "http://ns.adobe.com/exif/1.0/";

/// [`xmp_comment`] of a packet TagLib# takes as bytes (see [`xmp_text`]).
pub(super) fn xmp_bytes_comment(packet: &[u8]) -> Result<Option<String>, Refused> {
    if packet.len() > TEXT_LIMIT {
        return Ok(None);
    }
    xmp_comment(&xmp_text(packet))
}

/// `XmpTag.Comment` (`Xmp/XmpTag.cs:1082-1091`) of the tag
/// `new XmpTag(packet, file)` parses: its `dc:description`, else its
/// `exif:UserComment`, each as `GetLangAltNode` (`:750-791`) reads it.
/// `None` when neither is there or readable, and for a packet over
/// [`TEXT_LIMIT`], which is not read; `Refused` where the constructor throws.
///
/// The constructor parses the packet with .NET's `XmlDocument`, then walks
/// the RDF in `ParseRDF` and the methods it calls (`:289-547`), throwing on
/// many shapes; this walks the packet as quick-xml streams it, making the
/// same choices. Measured on Jellyfin 12.4 (gl-1612): line breaks and the
/// spaces around a value are kept as written, a whitespace-only text node is
/// dropped, and a reference to an entity a DTD declares refuses the packet,
/// as quick-xml expands none. quick-xml also refuses a packet nested past
/// 65,535 elements, where TagLib#'s recursive parse overflows the stack, and
/// one with more than 128 namespace bindings in scope at once, which TagLib#
/// reads (a real packet declares a few dozen).
///
/// shortcut: an element with several `rdf:parseType`, `rdf:datatype` or
/// `rdf:resource` attributes, which `ParsePropertyElement` parses once per
/// attribute, is read by its last one, and `xml:space="preserve"` is not
/// honoured; upgrade if a photo with such a packet turns up.
pub(super) fn xmp_comment(packet: &str) -> Result<Option<String>, Refused> {
    // The constructor reads the last character, so an empty packet throws; a
    // NUL there it drops, and .NET reads past any more (measured).
    if packet.is_empty() {
        return Err(Refused);
    }
    if packet.len() > TEXT_LIMIT {
        return Ok(None);
    }
    let [description, user_comment] = parse_rdf(packet.trim_end_matches('\0'))?;
    Ok(description
        .and_then(LangAlt::value)
        .or_else(|| user_comment.and_then(LangAlt::value)))
}

/// Which of the two nodes `XmpTag.Comment` reads a node is: its index in
/// [`Tree::found`].
#[derive(Debug, Clone, Copy)]
enum Target {
    /// `dc:description`.
    Description = 0,
    /// `exif:UserComment`.
    UserComment = 1,
}

/// The [`Target`] a node of the namespace `ns` and name `local` is.
fn target(ns: &str, local: &str) -> Option<Target> {
    // `XmpNode` adds the `/` a namespace ending in neither `/` nor `#` lacks.
    let is = |want: &str| ns == want || want.strip_suffix('/') == Some(ns);
    match local {
        "description" if is(DC_NS) => Some(Target::Description),
        "UserComment" if is(EXIF_NS) => Some(Target::UserComment),
        _ => None,
    }
}

/// What `GetLangAltNode` reads of a node.
#[derive(Debug, Default)]
struct LangAlt {
    /// Its value, when its `Type` is `Simple`.
    simple: Option<String>,
    /// Whether its `Type` is `Alt`.
    alt: bool,
    /// The value of its first child whose `xml:lang` is `x-default`.
    default_item: Option<String>,
    /// Its first child: whether it is simple, and its value.
    first_item: Option<(bool, String)>,
}

impl LangAlt {
    /// `GetLangAltNode`: a simple node's value; an `rdf:Alt`'s `x-default`
    /// item, else its first item when that is simple.
    fn value(self) -> Option<String> {
        if self.simple.is_some() {
            return self.simple;
        }
        if !self.alt {
            return None;
        }
        self.default_item.or_else(|| {
            self.first_item
                .filter(|(simple, _)| *simple)
                .map(|(_, value)| value)
        })
    }

    /// Adds a child node, its value when it is simple, of `xml:lang` `lang`.
    fn add_item(&mut self, simple: Option<&str>, lang: Option<&str>) {
        // A child that is not simple has the value "".
        let value = simple.unwrap_or_default();
        if self.first_item.is_none() {
            self.first_item = Some((simple.is_some(), value.to_owned()));
        }
        if self.default_item.is_none() && lang == Some("x-default") {
            self.default_item = Some(value.to_owned());
        }
    }
}

/// How `ParsePropertyElement` parses a property element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No attribute but `xml:lang` and `rdf:ID`: chosen by its first child,
    /// and `ParseEmptyPropertyElement` when it has none.
    Open,
    /// `ParseLiteralPropertyElement` over text children only.
    Literal,
    /// `ParseResourcePropertyElement`: node element children.
    Resource,
    /// `ParseEmptyPropertyElement`, which throws on any child.
    Empty,
    /// `rdf:parseType="Resource"`: property children.
    Struct,
    /// `rdf:datatype`: `ParseLiteralPropertyElement` over all the text inside.
    Typed,
}

/// A property element being read.
#[derive(Debug)]
struct Prop {
    /// Its node's place in the tree's pre-order, which `NodeIndexVisitor`
    /// indexes in, the last node of a name winning.
    order: usize,
    /// Which node `XmpTag.Comment` reads it is, if either.
    target: Option<Target>,
    /// The stack position of the target property whose child node it is.
    item_of: Option<usize>,
    /// Its `xml:lang`.
    lang: Option<String>,
    /// How it is parsed.
    mode: Mode,
    /// Whether a child throws: `ParseEmptyPropertyElement` runs on it.
    childless: bool,
    /// `rdf:value`, else `rdf:resource`: an empty element's value.
    value: Option<String>,
    /// Its text so far, kept only for a target or an item of one.
    text: String,
    /// What a target holds so far.
    node: LangAlt,
}

impl Prop {
    /// Whether its text is read.
    fn keeps_text(&self) -> bool {
        self.target.is_some() || self.item_of.is_some()
    }
}

/// An open element.
#[derive(Debug)]
enum Frame {
    /// The root, `x:xmpmeta` or `x:xapmeta`.
    Root,
    /// Its first `rdf:RDF`, the one `SelectSingleNode` finds.
    Rdf,
    /// A node element (`rdf:Description`, `rdf:Alt`, `rdf:Bag`, `rdf:Seq`),
    /// describing the property at that stack position, or the tree's top.
    Node(Option<usize>),
    /// A property element.
    Prop(Box<Prop>),
}

/// A subtree the tree holds no node of, walked by its depth alone so that
/// its nesting costs nothing: an element the tree does not hold (outside the
/// RDF, or under a throw), or the content of a typed literal.
#[derive(Debug, Clone, Copy)]
struct Passive {
    /// How many of its elements are open.
    depth: usize,
    /// The stack position of the typed literal whose text it is, if it is one.
    text_of: Option<usize>,
}

/// One XML attribute, `xmlns` declarations left out.
#[derive(Debug)]
struct Attr {
    /// Its namespace, "" when it has none.
    ns: String,
    /// Its local name.
    local: String,
    /// Its value, references expanded.
    value: String,
}

impl Attr {
    /// Whether it is `rdf:<name>`.
    fn is_rdf(&self, name: &str) -> bool {
        self.ns == RDF_NS && self.local == name
    }

    /// Whether it is `xml:lang`.
    fn is_lang(&self) -> bool {
        self.ns == XML_NS && self.local == "lang"
    }
}

/// The walk of one packet's RDF.
#[derive(Debug, Default)]
struct Tree {
    /// The open elements the tree holds nodes of.
    stack: Vec<Frame>,
    /// The passive subtree open above them, if any.
    passive: Option<Passive>,
    /// The nodes made so far, which number them in pre-order.
    nodes: usize,
    /// The last node of each [`Target`], with its pre-order place.
    found: [Option<(usize, LangAlt)>; 2],
    /// Whether the root element came.
    root: bool,
    /// Whether the root's `rdf:RDF` came.
    rdf: bool,
    /// What the RDF's parse comes to.
    outcome: Outcome,
}

/// What `ParseRDF` makes of the RDF.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The tree it parses.
    #[default]
    Parsed,
    /// A throw inside it.
    Refused,
    /// A child of `rdf:RDF` other than an `rdf:Description`: it marks the
    /// file corrupt and keeps an empty tree, before it parses any node, so
    /// no throw counts.
    Empty,
}

/// What kind of node a non-element XML event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Other {
    /// A comment.
    Comment,
    /// A processing instruction.
    Pi,
}

impl Tree {
    /// The next node's pre-order place.
    fn next_order(&mut self) -> usize {
        self.nodes += 1;
        self.nodes
    }

    /// Notes a throw at an element, whose subtree is then not read: `None`,
    /// the frame [`Tree::start`] opens for it.
    fn refuse_subtree(&mut self) -> Option<Frame> {
        self.note_refusal();
        None
    }

    /// Notes a throw, which an empty tree outranks.
    fn note_refusal(&mut self) {
        if self.outcome == Outcome::Parsed {
            self.outcome = Outcome::Refused;
        }
    }

    /// A node made: a target's candidate, and an item of the target
    /// property at `item_of`.
    fn made(
        &mut self,
        order: usize,
        target: Option<Target>,
        item_of: Option<usize>,
        node: LangAlt,
        lang: Option<&str>,
    ) {
        if let Some(Frame::Prop(owner)) = item_of.and_then(|at| self.stack.get_mut(at)) {
            owner.node.add_item(node.simple.as_deref(), lang);
        }
        if let Some(target) = target {
            let slot = &mut self.found[target as usize];
            if slot.as_ref().is_none_or(|(at, _)| *at < order) {
                *slot = Some((order, node));
            }
        }
    }

    /// A simple node of `value` made from an attribute.
    fn attribute_node(&mut self, attr: &Attr, item_of: Option<usize>) {
        let order = self.next_order();
        let node = LangAlt {
            simple: Some(attr.value.clone()),
            ..LangAlt::default()
        };
        self.made(order, target(&attr.ns, &attr.local), item_of, node, None);
    }

    /// The stack position of the property at `at` when it is a target, the
    /// owner of the items under it.
    fn target_at(&self, at: Option<usize>) -> Option<usize> {
        at.filter(
            |at| matches!(self.stack.get(*at), Some(Frame::Prop(prop)) if prop.target.is_some()),
        )
    }

    /// An element's start tag.
    fn start(&mut self, ns: &str, local: &str, attrs: &[Attr]) -> Result<(), Refused> {
        if let Some(passive) = &mut self.passive {
            passive.depth += 1;
            return Ok(());
        }
        let parent = self.stack.len().checked_sub(1);
        let mut text_of = None;
        let frame = match self.stack.last() {
            None => {
                // `LoadXml` takes one root element.
                if self.root {
                    return Err(Refused);
                }
                self.root = true;
                (ns == ADOBE_X_NS && matches!(local, "xmpmeta" | "xapmeta")).then_some(Frame::Root)
            }
            Some(Frame::Root) if !self.rdf && ns == RDF_NS && local == "RDF" => {
                self.rdf = true;
                Some(Frame::Rdf)
            }
            Some(Frame::Root) => None,
            Some(Frame::Rdf) if ns == RDF_NS && local == "Description" => {
                self.node(None, ns, local, attrs)
            }
            Some(Frame::Rdf) => {
                self.outcome = Outcome::Empty;
                None
            }
            Some(Frame::Node(owner)) => {
                let item_of = self.target_at(*owner);
                self.property(item_of, ns, local, attrs)
            }
            Some(Frame::Prop(prop)) => match (prop.mode, prop.childless, prop.lang.is_some()) {
                // Not text only: `ParseResourcePropertyElement`, which throws
                // on the text, and on the `xml:lang` it takes as a qualifier
                // and then refuses as an attribute.
                (_, true, _)
                | (Mode::Literal | Mode::Empty, ..)
                | (Mode::Open | Mode::Resource, _, true) => self.refuse_subtree(),
                (Mode::Open | Mode::Resource, ..) => {
                    if let Some(Frame::Prop(prop)) = self.stack.last_mut() {
                        prop.mode = Mode::Resource;
                    }
                    self.node(parent, ns, local, attrs)
                }
                (Mode::Struct, ..) => self.property(None, ns, local, attrs),
                (Mode::Typed, ..) => {
                    text_of = parent;
                    None
                }
            },
        };
        match frame {
            Some(frame) => self.stack.push(frame),
            None => self.passive = Some(Passive { depth: 1, text_of }),
        }
        Ok(())
    }

    /// `ParseNodeElement` of a node element describing the property at
    /// `owner`, or the tree's top.
    fn node(
        &mut self,
        owner: Option<usize>,
        ns: &str,
        local: &str,
        attrs: &[Attr],
    ) -> Option<Frame> {
        let alt = match local {
            "Alt" if ns == RDF_NS => true,
            "Seq" | "Bag" | "Description" if ns == RDF_NS => false,
            // `IsNodeElement`, then "Unknown nodeelement found".
            _ => return self.refuse_subtree(),
        };
        if let Some(Frame::Prop(prop)) = owner.and_then(|at| self.stack.get_mut(at)) {
            prop.node.alt = alt;
        }
        let item_of = self.target_at(owner);
        for attr in attrs {
            if attr.is_rdf("ID") || attr.is_rdf("nodeID") || attr.is_rdf("about") {
                continue;
            }
            if attr.is_lang() {
                return self.refuse_subtree();
            }
            self.attribute_node(attr, item_of);
        }
        Some(Frame::Node(owner))
    }

    /// `ParsePropertyElement` of a property element, a child node of the
    /// target property at `item_of` if any.
    fn property(
        &mut self,
        item_of: Option<usize>,
        ns: &str,
        local: &str,
        attrs: &[Attr],
    ) -> Option<Frame> {
        // `IsPropertyElement`, which each of the parse methods checks first.
        let rdf_term = |names: &[&str]| ns == RDF_NS && names.contains(&local);
        if rdf_term(&[
            "RDF",
            "ID",
            "about",
            "parseType",
            "resource",
            "nodeID",
            "datatype",
            "Description",
            "aboutEach",
            "aboutEachPrefix",
            "bagID",
        ]) {
            return self.refuse_subtree();
        }
        let order = self.next_order();
        let mut mode = Mode::Open;
        let mut childless = attrs.len() > 3;
        if childless {
            mode = Mode::Empty;
        } else {
            for attr in attrs
                .iter()
                .filter(|attr| !attr.is_lang() && !attr.is_rdf("ID"))
            {
                mode = if attr.is_rdf("datatype") {
                    Mode::Typed
                } else if !attr.is_rdf("parseType") {
                    childless = true;
                    Mode::Empty
                } else if attr.value == "Resource" {
                    Mode::Struct
                } else {
                    return self.refuse_subtree();
                };
            }
        }
        let find = |name| attrs.iter().find(|attr| attr.is_rdf(name));
        let value = find("value")
            .or_else(|| find("resource"))
            .map(|attr| attr.value.clone());
        if mode == Mode::Empty && value.is_none() {
            // Its attributes are child nodes, `xml:lang` among them.
            for attr in attrs {
                if !attr.is_rdf("ID") && !attr.is_rdf("nodeID") {
                    self.attribute_node(attr, None);
                }
            }
        }
        Some(Frame::Prop(Box::new(Prop {
            order,
            target: target(ns, local),
            item_of,
            lang: attrs
                .iter()
                .find(|attr| attr.is_lang())
                .map(|attr| attr.value.clone()),
            mode,
            childless,
            value,
            text: String::new(),
            node: LangAlt::default(),
        })))
    }

    /// A run of character data, `cdata` when it is a CDATA section.
    fn text(&mut self, text: &str, cdata: bool) -> Result<(), Refused> {
        // `XmlDocument` keeps no whitespace-only text node.
        if !cdata && text.chars().all(|c| matches!(c, ' ' | '\t' | '\r' | '\n')) {
            return Ok(());
        }
        if let Some(passive) = self.passive {
            if let Some(Frame::Prop(prop)) = passive.text_of.and_then(|at| self.stack.get_mut(at))
                && prop.keeps_text()
            {
                prop.text.push_str(text);
            }
            return Ok(());
        }
        let mut refused = false;
        match self.stack.last_mut() {
            // Text outside the root fails `LoadXml`.
            None => return Err(Refused),
            Some(Frame::Root) => {}
            Some(Frame::Rdf) => self.outcome = Outcome::Empty,
            // A text node has no attributes: `ParsePropertyElement` throws.
            Some(Frame::Node(_)) => refused = true,
            Some(Frame::Prop(prop)) => match prop.mode {
                _ if prop.childless => refused = true,
                // A CDATA section is not an `XmlText`: the element goes to
                // `ParseResourcePropertyElement`, which throws on it.
                Mode::Open | Mode::Literal if cdata => refused = true,
                Mode::Open | Mode::Literal | Mode::Typed => {
                    if prop.mode == Mode::Open {
                        prop.mode = Mode::Literal;
                    }
                    if prop.keeps_text() {
                        prop.text.push_str(text);
                    }
                }
                Mode::Resource | Mode::Struct | Mode::Empty => refused = true,
            },
        }
        if refused {
            self.note_refusal();
        }
        Ok(())
    }

    /// A comment or a processing instruction.
    fn other(&mut self, other: Other) {
        if self.passive.is_some() {
            return;
        }
        let pi = other == Other::Pi;
        let refused = match self.stack.last() {
            None | Some(Frame::Root) => false,
            Some(Frame::Rdf) => {
                self.outcome = Outcome::Empty;
                false
            }
            // `ParseNodeElement` skips a comment, and throws on a PI.
            Some(Frame::Node(_)) => pi,
            Some(Frame::Prop(prop)) => match prop.mode {
                _ if prop.childless => true,
                Mode::Typed => false,
                Mode::Struct => pi,
                Mode::Open | Mode::Literal | Mode::Resource | Mode::Empty => true,
            },
        };
        if refused {
            self.note_refusal();
        }
    }

    /// An element's end tag.
    fn end(&mut self) {
        if let Some(passive) = &mut self.passive {
            passive.depth -= 1;
            if passive.depth == 0 {
                self.passive = None;
            }
            return;
        }
        let Some(Frame::Prop(prop)) = self.stack.pop() else {
            return;
        };
        let mut node = prop.node;
        node.simple = match prop.mode {
            Mode::Literal | Mode::Typed => Some(prop.text),
            Mode::Open | Mode::Empty => Some(prop.value.unwrap_or_default()),
            Mode::Resource | Mode::Struct => None,
        };
        self.made(
            prop.order,
            prop.target,
            prop.item_of,
            node,
            prop.lang.as_deref(),
        );
    }
}

/// The last `dc:description` and `exif:UserComment` nodes of the tree
/// `XmpTag(string, File)` parses from `packet`, or `Refused` where it throws.
fn parse_rdf(packet: &str) -> Result<[Option<LangAlt>; 2], Refused> {
    let mut reader = NsReader::from_str(packet);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_comments = true;
    let mut tree = Tree::default();
    // Text and references arrive as separate events; one text node joins them.
    let mut run = String::new();
    loop {
        let event = reader.read_event().map_err(|_| Refused)?;
        match &event {
            Event::Text(text) => {
                run.push_str(text);
                continue;
            }
            Event::GeneralRef(reference) => {
                let reference = format!("&{};", reference.as_ref());
                run.push_str(&quick_xml::escape::unescape(&reference).map_err(|_| Refused)?);
                continue;
            }
            _ => {}
        }
        tree.text(&std::mem::take(&mut run), false)?;
        match event {
            Event::Start(start) => {
                let (ns, local) = element_name(&reader, &start)?;
                let attrs = attributes(&reader, &start)?;
                tree.start(&ns, &local, &attrs)?;
            }
            Event::End(_) => tree.end(),
            Event::CData(cdata) => tree.text(cdata.as_ref(), true)?,
            Event::Comment(_) => tree.other(Other::Comment),
            Event::PI(_) => tree.other(Other::Pi),
            Event::Eof => break,
            _ => {}
        }
    }
    // `LoadXml` wants a root, closed; `SelectSingleNode` an `rdf:RDF` in it.
    if !tree.root || !tree.stack.is_empty() || tree.passive.is_some() || !tree.rdf {
        return Err(Refused);
    }
    match tree.outcome {
        Outcome::Empty => Ok([None, None]),
        Outcome::Refused => Err(Refused),
        Outcome::Parsed => Ok(tree.found.map(|found| found.map(|(_, node)| node))),
    }
}

/// A start tag's namespace and local name. An unbound prefix fails `LoadXml`.
fn element_name(reader: &NsReader<&[u8]>, start: &BytesStart) -> Result<(String, String), Refused> {
    let (ns, local) = reader.resolver().resolve_element(start.name());
    Ok((namespace(&ns)?, local.as_ref().to_owned()))
}

/// The namespace of a resolved name, "" when it has none.
fn namespace(ns: &ResolveResult) -> Result<String, Refused> {
    match ns {
        ResolveResult::Bound(ns) => Ok(ns.0.to_owned()),
        ResolveResult::Unbound => Ok(String::new()),
        ResolveResult::Unknown(_) => Err(Refused),
    }
}

/// A start tag's attributes but its `xmlns` declarations, which TagLib#
/// skips everywhere (`In (XMLNS_NS)`).
fn attributes(reader: &NsReader<&[u8]>, start: &BytesStart) -> Result<Vec<Attr>, Refused> {
    let mut attrs = Vec::new();
    for attr in start.attributes() {
        let attr = attr.map_err(|_| Refused)?;
        if attr.key.as_namespace_binding().is_some() {
            continue;
        }
        let (ns, local) = reader.resolver().resolve_attribute(attr.key);
        attrs.push(Attr {
            ns: namespace(&ns)?,
            local: local.as_ref().to_owned(),
            // Unescaped but not normalized: `XmlTextReader`'s `Normalization`
            // is off, as the kept line breaks of a text node show.
            value: quick_xml::escape::unescape(&attr.value)
                .map_err(|_| Refused)?
                .into_owned(),
        });
    }
    Ok(attrs)
}

/// The start of an APP13 segment `ReadAPP13Segment` (`Jpeg/File.cs:532-568`)
/// reads: Photoshop's identifier, then an IPTC-IIM resource (`8BIM` 0x0404)
/// first.
const IPTC_APP13: &[u8] = b"Photoshop 3.0\x008BIM\x04\x04";

/// The caption (dataset 2:120, Caption/Abstract) of the IPTC-IIM tag TagLib#
/// reads from a JPEG's APP13 segment, `None` when it reads none.
///
/// Port of `ReadAPP13Segment` and `IIMReader.Process` (`IIM/IIMReader.cs`):
/// after the resource's name, every `1C 02` marker in the rest of the
/// segment starts a dataset (number, two-byte size, data), the last caption
/// winning; a dataset it reads that runs past the end throws.
pub(super) fn iim_caption(segment: &[u8]) -> Result<Option<String>, Refused> {
    let Some(rest) = segment.strip_prefix(IPTC_APP13) else {
        return Ok(None);
    };
    // A Pascal-string name (and its padding), then the resource's size.
    let name = usize::from(*rest.first().ok_or(Refused)?);
    let skip = if name > 0 { 1 + name + 4 } else { 6 };
    let data = rest.get(skip..).ok_or(Refused)?;
    let mut caption = None;
    let mut from = 0;
    while let Some(found) = data
        .get(from..)
        .and_then(|tail| tail.windows(2).position(|pair| pair == [0x1c, 0x02]))
    {
        let at = from + found + 2;
        // `ByteVector.ToUShort` reads what bytes there are, up to two.
        let size = match data.get(at + 1..).ok_or(Refused)? {
            [high, low, ..] => usize::from(u16::from_be_bytes([*high, *low])),
            [low] => usize::from(*low),
            [] => 0,
        };
        let number = *data.get(at).ok_or(Refused)?;
        // Object Name, Keywords, By-line, Copyright Notice, Caption/Abstract.
        if matches!(number, 5 | 25 | 80 | 116 | 120) {
            let value = data.get(at + 3..at + 3 + size).ok_or(Refused)?;
            if number == 120 {
                caption = Some(decode_text(value));
            }
        }
        from = at + 3 + size;
    }
    Ok(caption)
}

/// What TagLibSharp 2.3.0's PNG reader (`Png/File.cs`, `ReadMetadata`)
/// takes from a PNG's text chunks for the comment.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct PngText {
    /// The XMP tag's comment: the last XMP packet's, as each one `AddTag`s
    /// replaces the one before.
    pub(super) xmp: Option<String>,
    /// `PngTag.Comment` (`Png/PngTag.cs`): the `Description` keyword when it
    /// is not empty, else `Comment`, each from the first chunk holding it.
    pub(super) comment: Option<String>,
}

/// The XMP iTXt chunk's start: its keyword, then no compression, no
/// language and no translated keyword.
const XMP_ITXT: &[u8] = b"XML:com.adobe.xmp\0\0\0\0\0";

/// The most bytes of a PNG text chunk, of a zTXt chunk's inflated text, or of
/// an XMP packet, that the comment is read from; a longer one is not read
/// (a JPEG's XMP packet fits in a segment well under it). Upstream has none.
// TODO(owner, gl-1974): a candidate setting; this value is provisional.
pub(super) const TEXT_LIMIT: usize = 16 * 1024 * 1024;

/// One text chunk, as TagLib# reads it.
#[derive(Debug, PartialEq, Eq)]
enum PngChunk {
    /// An XMP packet's text.
    Xmp(String),
    /// A `Description` or `Comment` keyword's value.
    Keyword(&'static str, String),
}

/// The [`PngText`] of the PNG `reader` is at the start of, a file of `len`
/// bytes (0 when unknown).
///
/// It walks every chunk up to IEND as `ReadMetadata` does, seeking past all
/// but the text chunks, and stops where TagLib# would throw on the walk: a
/// length over 2^31 - 1, a chunk running past the end of the file. Neither
/// the IHDR chunk nor any CRC is checked: TagLib# throws on a bad one, and
/// whether that should leave the photo alone is gl-1973. A text chunk over
/// [`TEXT_LIMIT`] is seeked past, and only the last XMP packet is parsed.
pub(super) fn png_text<R: Read + Seek>(reader: &mut BufReader<R>, len: u64) -> PngText {
    let mut packet = None;
    let (mut description, mut comment) = (None, None);
    let mut at: u64 = 8;
    if reader.seek_relative(8).is_ok() {
        loop {
            let mut head = [0; 8];
            if reader.read_exact(&mut head).is_err() {
                break;
            }
            let (size, kind) = head.split_at(4);
            let size = u32::from_be_bytes([size[0], size[1], size[2], size[3]]);
            let next = at + 12 + u64::from(size);
            if kind == b"IEND" || i32::try_from(size).is_err() || (len > 0 && next > len) {
                break;
            }
            let text = matches!(kind, b"tEXt" | b"zTXt" | b"iTXt");
            if text && usize::try_from(size).is_ok_and(|size| size <= TEXT_LIMIT) {
                // `take` grows the buffer with what the file holds.
                let mut data = Vec::new();
                let read = reader.by_ref().take(u64::from(size)).read_to_end(&mut data);
                if read.is_err()
                    || data.len() as u64 != u64::from(size)
                    || reader.seek_relative(4).is_err()
                {
                    break;
                }
                let filled = |keyword| match keyword {
                    "Description" => description.is_some(),
                    _ => comment.is_some(),
                };
                match png_chunk(kind, &data, filled) {
                    Ok(Some(PngChunk::Xmp(text))) => packet = Some(text),
                    Ok(Some(PngChunk::Keyword("Description", value))) => {
                        description.get_or_insert(value);
                    }
                    Ok(Some(PngChunk::Keyword(_, value))) => {
                        comment.get_or_insert(value);
                    }
                    Ok(None) | Err(Refused) => {}
                }
            } else if reader.seek_relative(i64::from(size) + 4).is_err() {
                break;
            }
            at = next;
        }
    }
    PngText {
        xmp: packet.and_then(|packet: String| xmp_comment(&packet).ok().flatten()),
        comment: description
            .filter(|text: &String| !text.is_empty())
            .or(comment),
    }
}

/// What TagLib# reads of one tEXt, zTXt or iTXt chunk for the comment;
/// `None` for one it reads for something else, or not at all, and for a
/// keyword `filled` says an earlier chunk holds, which wins.
///
/// A zTXt chunk of another keyword, or of one already filled, is not
/// inflated, so one TagLib# throws on is not noticed.
fn png_chunk(
    kind: &[u8],
    data: &[u8],
    filled: impl Fn(&'static str) -> bool,
) -> Result<Option<PngChunk>, Refused> {
    if kind == b"iTXt"
        && let Some(packet) = data.strip_prefix(XMP_ITXT)
    {
        return Ok(Some(PngChunk::Xmp(xmp_text(packet))));
    }
    // `ReadKeyword`: up to a NUL, not empty.
    let end = data.iter().position(|byte| *byte == 0).ok_or(Refused)?;
    let (keyword, rest) = (&data[..end], &data[end + 1..]);
    if keyword.is_empty() {
        return Err(Refused);
    }
    let comment_keyword = match keyword {
        b"Description" => Some("Description"),
        b"Comment" => Some("Comment"),
        _ => None,
    }
    .filter(|keyword| !filled(keyword));
    let value = match kind {
        b"tEXt" if comment_keyword.is_none() => return Ok(None),
        b"tEXt" => decode_text(rest),
        b"zTXt" => {
            let raw_profile = keyword.starts_with(b"Raw profile type");
            if comment_keyword.is_none() && !raw_profile && keyword != b"xmp" {
                return Ok(None);
            }
            let (&method, compressed) = rest.split_first().ok_or(Refused)?;
            // An unknown compression method is skipped.
            if method != 0 {
                return Ok(None);
            }
            let Some(plain) = inflate(compressed)? else {
                return Ok(None);
            };
            if raw_profile {
                return Ok(raw_xmp(&String::from_utf8_lossy(&plain))?.map(PngChunk::Xmp));
            }
            // `ReadzTXtChunk` hands a plain `xmp` keyword's profile, which it
            // has not made, to `XmpTag`.
            if keyword == b"xmp" {
                return Err(Refused);
            }
            decode_text(&plain)
        }
        _ => {
            // `ReadiTXtChunk`: a compression flag and method must follow.
            let [flag, method, rest @ ..] = rest else {
                return Err(Refused);
            };
            // It inflates from the flag rather than the text, which throws
            // (measured), and skips an unknown method.
            if *flag != 0 {
                return if *method == 0 { Err(Refused) } else { Ok(None) };
            }
            if comment_keyword.is_none() {
                return Ok(None);
            }
            // It also keeps the language tag and translated keyword (and the
            // flag and method) at the start of the value, so Jellyfin shows
            // four NULs before the text: that is not ported (gl-1612).
            let mut fields = rest.splitn(3, |byte| *byte == 0);
            match (fields.next(), fields.next(), fields.next()) {
                (Some(_), Some(_), Some(text)) => decode_text(text),
                _ => return Ok(None),
            }
        }
    };
    Ok(comment_keyword.map(|keyword| PngChunk::Keyword(keyword, value)))
}

/// TagLib#'s `Inflate` (`Png/File.cs`): the zlib header's two bytes skipped
/// and the rest read as raw deflate, its checksum unchecked. `None` when the
/// text runs past [`TEXT_LIMIT`].
fn inflate(compressed: &[u8]) -> Result<Option<Vec<u8>>, Refused> {
    let mut plain = Vec::new();
    flate2::read::DeflateDecoder::new(compressed.get(2..).unwrap_or_default())
        .take(TEXT_LIMIT as u64 + 1)
        .read_to_end(&mut plain)
        .map_err(|_| Refused)?;
    Ok((plain.len() <= TEXT_LIMIT).then_some(plain))
}

/// The XMP packet of an ImageMagick raw profile (`"\n<name>\n<length>\n"`
/// and the data in hex lines) as `ProcessRawProfile` (`Png/File.cs`) reads
/// it, `None` when the profile is not named `xmp`.
///
/// Each line's hex pairs are decoded on their own, an odd last digit
/// dropped, and the line's text joined to the others with its NULs trimmed.
/// Fewer than four lines, or a pair that is not hex, throws.
fn raw_xmp(profile: &str) -> Result<Option<String>, Refused> {
    let mut lines = profile.split('\n');
    let (Some(_), Some(name), Some(_)) = (lines.next(), lines.next(), lines.next()) else {
        return Err(Refused);
    };
    let mut lines = lines.peekable();
    if lines.peek().is_none() {
        return Err(Refused);
    }
    let digit = |c: char| c.to_digit(16).ok_or(Refused);
    let (mut packet, mut bytes) = (String::new(), Vec::new());
    for line in lines {
        bytes.clear();
        let mut digits = line.chars();
        while let (Some(high), Some(low)) = (digits.next(), digits.next()) {
            bytes.push(u8::try_from(digit(high)? * 16 + digit(low)?).unwrap_or_default());
        }
        packet.push_str(String::from_utf8_lossy(&bytes).trim_matches('\0'));
    }
    Ok(name.eq_ignore_ascii_case("xmp").then_some(packet))
}

#[cfg(test)]
pub(super) mod tests {
    //! Each case was scanned by Jellyfin 12.4 (gl-1612) unless it says
    //! otherwise: the expected value is the `Overview` it stored, read
    //! through this module's part of it.
    use super::*;
    use rstest::rstest;
    use std::fmt::Write as _;

    const RDF_OPEN: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">"#;
    const RDF_CLOSE: &str = "</rdf:RDF></x:xmpmeta>";
    const NS: &str = r#"xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:exif="http://ns.adobe.com/exif/1.0/" xmlns:foo="http://example.com/foo/""#;

    /// A packet whose RDF holds `rdf`.
    fn packet(rdf: &str) -> String {
        format!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>{RDF_OPEN}{rdf}{RDF_CLOSE}<?xpacket end=\"w\"?>"
        )
    }

    /// An `rdf:Description` of `props`, with `attrs` on it.
    fn desc(props: &str, attrs: &str) -> String {
        format!(r#"<rdf:Description rdf:about="" {NS} {attrs}>{props}</rdf:Description>"#)
    }

    /// An `rdf:Alt` property `name` of the items `(lang, text)`.
    fn alt(name: &str, items: &[(&str, &str)]) -> String {
        let mut lis = String::new();
        for (lang, text) in items {
            write!(lis, r#"<rdf:li xml:lang="{lang}">{text}</rdf:li>"#).unwrap();
        }
        format!("<{name}><rdf:Alt>{lis}</rdf:Alt></{name}>")
    }

    fn description(text: &str) -> String {
        alt("dc:description", &[("x-default", text)])
    }

    #[rstest]
    #[case::an_alt_default(packet(&desc(&description("xmp description"), "")), Ok(Some("xmp description")))]
    #[case::the_user_comment(
        packet(&desc(&alt("exif:UserComment", &[("x-default", "xmp user comment")]), "")),
        Ok(Some("xmp user comment"))
    )]
    #[case::the_first_item_without_a_default(
        packet(&desc(&alt("dc:description", &[("de", "deutsch"), ("fr", "francais")]), "")),
        Ok(Some("deutsch"))
    )]
    #[case::an_attribute(packet(&desc("", r#"dc:description="attribute description""#)), Ok(Some("attribute description")))]
    #[case::a_literal_element(packet(&desc("<dc:description>plain text</dc:description>", "")), Ok(Some("plain text")))]
    #[case::line_breaks_kept(packet(&desc(&description("line1\r\nline2"), "")), Ok(Some("line1\r\nline2")))]
    #[case::spaces_kept(packet(&desc(&description("  padded  "), "")), Ok(Some("  padded  ")))]
    #[case::a_whitespace_only_item(packet(&desc(&description("   "), "")), Ok(Some("")))]
    #[case::references(packet(&desc(&description("a &amp; b &#233; &lt;c&gt;"), "")), Ok(Some("a & b \u{e9} <c>")))]
    #[case::the_last_node_wins(
        packet(&format!("{}{}", desc(&description("first"), ""), desc(&description("second"), ""))),
        Ok(Some("second"))
    )]
    #[case::a_nested_node_wins(
        packet(&desc(&format!(
            r#"{}<foo:bar rdf:parseType="Resource"><dc:description>inner</dc:description></foo:bar>"#,
            description("outer")
        ), "")),
        Ok(Some("inner"))
    )]
    #[case::an_empty_description_hides_the_user_comment(
        packet(&desc(&format!("{}{}", description(""), alt("exif:UserComment", &[("x-default", "uc")])), "")),
        Ok(Some(""))
    )]
    #[case::a_bag_is_no_comment(
        packet(&desc("<dc:description><rdf:Bag><rdf:li>bag item</rdf:li></rdf:Bag></dc:description>", "")),
        Ok(None)
    )]
    #[case::a_struct_first_item_is_no_comment(
        packet(&desc(
            r#"<dc:description><rdf:Alt><rdf:li rdf:parseType="Resource"><foo:a>x</foo:a></rdf:li><rdf:li xml:lang="de">second</rdf:li></rdf:Alt></dc:description>"#,
            ""
        )),
        Ok(None)
    )]
    #[case::an_attribute_of_the_alt_is_its_first_item(
        packet(&desc(
            r#"<dc:description><rdf:Alt foo:a="attribute item"><rdf:li xml:lang="de">de item</rdf:li></rdf:Alt></dc:description>"#,
            ""
        )),
        Ok(Some("attribute item"))
    )]
    #[case::a_namespace_without_its_slash(
        packet(&desc(&description("no slash"), "").replace(r#"elements/1.1/""#, r#"elements/1.1""#)),
        Ok(Some("no slash"))
    )]
    #[case::a_comment_in_the_rdf_empties_the_tree(
        packet(&format!("<!-- a comment -->{}", desc(&description("xmp description"), ""))),
        Ok(None)
    )]
    #[case::trailing_nuls(format!("{}\0\0", packet(&desc(&description("xmp description"), ""))), Ok(Some("xmp description")))]
    #[case::a_cdata_item(packet(&desc(&description("<![CDATA[cdata text]]>"), "")), Err(Refused))]
    #[case::a_comment_in_an_item(packet(&desc(&description("text<!-- c -->"), "")), Err(Refused))]
    #[case::text_in_a_description(packet(&desc(&format!("stray text{}", description("x")), "")), Err(Refused))]
    #[case::a_language_on_a_resource(
        packet(&desc(r#"<dc:description xml:lang="en"><rdf:Alt><rdf:li xml:lang="x-default">x</rdf:li></rdf:Alt></dc:description>"#, "")),
        Err(Refused)
    )]
    #[case::no_xmpmeta(
        format!(r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">{}</rdf:RDF>"#, desc(&description("x"), "")),
        Err(Refused)
    )]
    #[case::a_dtd_entity(
        format!(r#"<!DOCTYPE x:xmpmeta [<!ENTITY e "entity text">]>{RDF_OPEN}{}{RDF_CLOSE}"#, desc(&description("&e;"), "")),
        Err(Refused)
    )]
    #[case::malformed(r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF"#.to_owned(), Err(Refused))]
    fn xmp_reads_the_comment_xmptag_reads(
        #[case] packet: String,
        #[case] expected: Result<Option<&str>, Refused>,
    ) {
        assert_eq!(
            xmp_comment(&packet),
            expected.map(|comment| comment.map(str::to_owned))
        );
    }

    // Source-read, not measured: shapes `ParseRDF` and its callees refuse
    // or empty that no fixture above holds.
    #[rstest]
    #[case::an_empty_packet("", Err(Refused))]
    #[case::xapmeta(
        packet(&desc(&description("xap"), "")).replace("x:xmpmeta", "x:xapmeta"),
        Ok(Some("xap"))
    )]
    #[case::an_element_child_of_the_rdf(packet(&format!("<foo:bar {NS}/>{}", desc(&description("x"), ""))), Ok(None))]
    #[case::a_pi_in_a_description(packet(&desc(&format!("<?pi x?>{}", description("x")), "")), Err(Refused))]
    #[case::a_language_on_a_description(packet(&desc(&description("x"), r#"xml:lang="en""#)), Err(Refused))]
    #[case::an_unknown_node_element(
        packet(&desc("<dc:description><rdf:List><rdf:li>x</rdf:li></rdf:List></dc:description>", "")),
        Err(Refused)
    )]
    #[case::a_resource_value(packet(&desc(r#"<dc:description rdf:resource="http://example.com/"/>"#, "")), Ok(Some("http://example.com/")))]
    #[case::another_parse_type(packet(&desc(r#"<dc:description rdf:parseType="Literal">x</dc:description>"#, "")), Err(Refused))]
    #[case::an_empty_element_with_children(
        packet(&desc(r#"<dc:description foo:a="1"><foo:b>x</foo:b></dc:description>"#, "")),
        Err(Refused)
    )]
    #[case::an_attribute_of_an_empty_element(
        packet(&desc(r#"<foo:bar foo:a="1" dc:description="from an attribute"/>"#, "")),
        Ok(Some("from an attribute"))
    )]
    #[case::a_typed_literal(
        packet(&desc(r#"<dc:description rdf:datatype="x"><foo:a>typed</foo:a> text</dc:description>"#, "")),
        Ok(Some("typed text"))
    )]
    #[case::an_unbound_prefix(packet(&desc("<zz:description>x</zz:description>", "")), Err(Refused))]
    #[case::two_roots(format!("{}<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>", packet(&desc(&description("x"), ""))), Err(Refused))]
    fn xmp_refuses_and_empties_as_the_parse_does(
        #[case] packet: String,
        #[case] expected: Result<Option<&str>, Refused>,
    ) {
        assert_eq!(
            xmp_comment(&packet),
            expected.map(|comment| comment.map(str::to_owned))
        );
    }

    #[test]
    fn a_byte_order_mark_before_the_packet_is_dropped() {
        let packet = format!("\u{feff}{}", packet(&desc(&description("bom"), "")));
        assert_eq!(
            xmp_bytes_comment(packet.as_bytes()),
            Ok(Some("bom".to_owned()))
        );
    }

    #[rstest]
    #[case::utf8("caf\u{e9}".as_bytes(), "caf\u{e9}")]
    #[case::latin1(b"caf\xe9", "caf\u{e9}")]
    #[case::a_byte_order_mark(b"\xef\xbb\xbftext", "text")]
    #[case::a_reversed_byte_order_mark("\u{fffe}text".as_bytes(), "text")]
    fn text_is_utf8_else_latin1(#[case] bytes: &[u8], #[case] expected: &str) {
        assert_eq!(decode_text(bytes), expected);
    }

    #[rstest]
    #[case::none(&[], "")]
    #[case::all_empty(&[None, Some(""), None], "")]
    #[case::the_first_non_empty(&[Some(""), Some("b"), Some("c")], "b")]
    fn first_comment_skips_missing_and_empty(
        #[case] comments: &[Option<&str>],
        #[case] expected: &str,
    ) {
        let comments = comments.iter().map(|comment| comment.map(str::to_owned));
        assert_eq!(first_comment(comments), expected);
    }

    /// An APP13 segment holding one IPTC-IIM resource with `datasets`.
    fn app13(datasets: &[(u8, &[u8])]) -> Vec<u8> {
        let mut iim = Vec::new();
        for (number, value) in datasets {
            iim.extend_from_slice(&[0x1c, 0x02, *number]);
            iim.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
            iim.extend_from_slice(value);
        }
        let mut segment = IPTC_APP13.to_vec();
        segment.extend_from_slice(&[0, 0]);
        segment.extend_from_slice(&u32::try_from(iim.len()).unwrap().to_be_bytes());
        segment.extend_from_slice(&iim);
        segment
    }

    #[rstest]
    #[case::a_caption(app13(&[(120, b"iptc caption")]), Ok(Some("iptc caption")))]
    #[case::utf8_reads_as_utf8(app13(&[(120, "caf\u{e9} caption".as_bytes())]), Ok(Some("caf\u{e9} caption")))]
    #[case::the_last_caption(app13(&[(120, b"first"), (5, b"title"), (120, b"last")]), Ok(Some("last")))]
    #[case::no_caption(app13(&[(5, b"title")]), Ok(None))]
    #[case::not_iptc(b"Photoshop 3.0\x008BIM\x04\x25\0\0\0\0\0\0".to_vec(), Ok(None))]
    #[case::nothing_after_the_identifier(IPTC_APP13.to_vec(), Err(Refused))]
    #[case::a_caption_past_the_end({
        let mut segment = app13(&[(120, b"cut")]);
        segment.truncate(segment.len() - 1);
        segment
    }, Err(Refused))]
    #[case::a_named_resource({
        let mut segment = IPTC_APP13.to_vec();
        segment.extend_from_slice(&[3, b'a', b'b', b'c', 0, 0, 0, 0]);
        segment.extend_from_slice(&[0x1c, 0x02, 120, 0, 4, b'n', b'a', b'm', b'e']);
        segment
    }, Ok(Some("name")))]
    fn iim_reads_the_caption_iimreader_reads(
        #[case] segment: Vec<u8>,
        #[case] expected: Result<Option<&str>, Refused>,
    ) {
        assert_eq!(
            iim_caption(&segment),
            expected.map(|caption| caption.map(str::to_owned))
        );
    }

    /// A PNG chunk of `kind` holding `data`, with its CRC.
    pub(in crate::photo_provider) fn chunk(kind: &[u8], data: &[u8]) -> Vec<u8> {
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(data);
        let mut chunk = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(data);
        chunk.extend_from_slice(&crc.sum().to_be_bytes());
        chunk
    }

    /// `text` zlib-compressed, as a zTXt chunk holds it.
    pub(in crate::photo_provider) fn zlib(text: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(text).unwrap();
        encoder.finish().unwrap()
    }

    fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut file = b"\x89PNG\r\n\x1a\n".to_vec();
        file.extend(chunk(b"IHDR", &[0; 13]));
        for each in chunks {
            file.extend_from_slice(each);
        }
        file.extend(chunk(b"IDAT", b"pixels"));
        file.extend(chunk(b"IEND", b""));
        file
    }

    fn read_png(file: &[u8]) -> PngText {
        let len = u64::try_from(file.len()).unwrap();
        png_text(&mut BufReader::new(std::io::Cursor::new(file)), len)
    }

    fn png_xmp(text: &str) -> Vec<u8> {
        [XMP_ITXT, packet(&desc(&description(text), "")).as_bytes()].concat()
    }

    /// `packet` as an ImageMagick raw profile, in hex lines.
    pub(in crate::photo_provider) fn raw_profile(packet: &[u8]) -> Vec<u8> {
        let mut hex = String::new();
        for byte in packet {
            write!(hex, "{byte:02x}").unwrap();
        }
        let lines: Vec<&str> = hex
            .as_bytes()
            .chunks(72)
            .map(|line| std::str::from_utf8(line).unwrap())
            .collect();
        format!("\nxmp\n{:8}\n{}\n", packet.len(), lines.join("\n")).into_bytes()
    }

    #[rstest]
    #[case::a_comment(vec![chunk(b"tEXt", b"Comment\0png comment")], None, Some("png comment"))]
    #[case::the_description_first(
        vec![chunk(b"tEXt", b"Comment\0png comment"), chunk(b"tEXt", b"Description\0png description")],
        None,
        Some("png description")
    )]
    #[case::the_first_chunk_of_a_keyword(
        vec![chunk(b"tEXt", b"Comment\0first"), chunk(b"tEXt", b"Comment\0second")],
        None,
        Some("first")
    )]
    #[case::an_empty_description(
        vec![chunk(b"tEXt", b"Description\0"), chunk(b"tEXt", b"Comment\0png comment")],
        None,
        Some("png comment")
    )]
    #[case::a_compressed_comment(
        vec![chunk(b"zTXt", &[b"Comment\0\0".as_slice(), &zlib(b"compressed comment")].concat())],
        None,
        Some("compressed comment")
    )]
    // Jellyfin stores "\0\0\0\0itxt comment": the NULs are not ported.
    #[case::an_itxt_comment(vec![chunk(b"iTXt", b"Comment\0\0\0\0\0itxt comment")], None, Some("itxt comment"))]
    #[case::a_compressed_itxt_is_refused(
        vec![chunk(b"iTXt", &[b"Comment\0\x01\0\0\0".as_slice(), &zlib(b"compressed itxt")].concat())],
        None,
        None
    )]
    #[case::the_xmp(
        vec![chunk(b"tEXt", b"Comment\0png comment"), chunk(b"iTXt", &png_xmp("png xmp description"))],
        Some("png xmp description"),
        Some("png comment")
    )]
    #[case::the_last_xmp(
        vec![chunk(b"iTXt", &png_xmp("first")), chunk(b"iTXt", &png_xmp("second"))],
        Some("second"),
        None
    )]
    #[case::a_raw_profile(
        vec![chunk(b"zTXt", &[b"Raw profile type xmp\0\0".as_slice(), &zlib(&raw_profile(packet(&desc(&description("raw profile xmp"), "")).as_bytes()))].concat())],
        Some("raw profile xmp"),
        None
    )]
    #[case::other_keywords(vec![chunk(b"tEXt", b"Title\0a title"), chunk(b"tEXt", b"Software\0x")], None, None)]
    #[case::after_iend(vec![chunk(b"IEND", b""), chunk(b"tEXt", b"Comment\0late")], None, None)]
    fn png_reads_the_text_pngtag_reads(
        #[case] chunks: Vec<Vec<u8>>,
        #[case] xmp: Option<&str>,
        #[case] comment: Option<&str>,
    ) {
        let expected = PngText {
            xmp: xmp.map(str::to_owned),
            comment: comment.map(str::to_owned),
        };
        assert_eq!(read_png(&png(&chunks)), expected);
    }

    #[test]
    fn a_png_text_chunk_past_the_end_stops_the_walk() {
        let mut file = b"\x89PNG\r\n\x1a\n".to_vec();
        file.extend(chunk(b"IHDR", &[0; 13]));
        file.extend(chunk(b"tEXt", b"Comment\0kept"));
        let mut cut = chunk(b"tEXt", b"Description\0cut short");
        cut.truncate(cut.len() - 2);
        file.extend(cut);
        assert_eq!(read_png(&file).comment.as_deref(), Some("kept"));
    }

    #[test]
    fn a_zip_bomb_is_not_inflated_past_the_limit() {
        let bomb = zlib(&vec![b'a'; TEXT_LIMIT + 1]);
        let file = png(&[chunk(b"zTXt", &[b"Comment\0\0".as_slice(), &bomb].concat())]);
        assert_eq!(read_png(&file).comment, None);
    }

    #[test]
    fn a_png_of_unknown_length_reads_the_same() {
        let file = png(&[chunk(b"tEXt", b"Comment\0png comment")]);
        let text = png_text(&mut BufReader::new(std::io::Cursor::new(&file)), 0);
        assert_eq!(text.comment.as_deref(), Some("png comment"));
    }

    #[test]
    fn a_text_chunk_over_the_limit_is_skipped() {
        let mut big = b"Comment\0".to_vec();
        big.resize(TEXT_LIMIT + 1, b'a');
        let file = png(&[chunk(b"tEXt", &big), chunk(b"tEXt", b"Comment\0next")]);
        assert_eq!(read_png(&file).comment.as_deref(), Some("next"));
    }

    #[test]
    fn an_xmp_packet_over_the_limit_is_not_read() {
        let mut packet = packet(&desc(&description("big"), ""));
        packet.push_str(&" ".repeat(TEXT_LIMIT));
        assert_eq!(xmp_comment(&packet), Ok(None));
    }

    /// Nesting costs no stack: the walk keeps its frames on the heap, and a
    /// subtree it holds no node of only by its depth. quick-xml refuses a
    /// packet nested past 65,535 elements, a depth at which TagLib#'s
    /// recursive parse has long overflowed the stack.
    #[rstest]
    #[case::nested_structs(
        50_000,
        r#"<foo:a rdf:parseType="Resource">"#,
        "</foo:a>",
        Ok(Some("deep"))
    )]
    #[case::nested_resources(
        30_000,
        "<foo:a><rdf:Description>",
        "</rdf:Description></foo:a>",
        Ok(Some("deep"))
    )]
    // quick-xml's own bound: it refuses nesting past 65,535 elements.
    #[case::past_quick_xmls_depth(
        70_000,
        r#"<foo:a rdf:parseType="Resource">"#,
        "</foo:a>",
        Err(Refused)
    )]
    #[case::a_refused_deep_subtree(500_000, "<foo:a>", "</foo:a>", Err(Refused))]
    fn deep_nesting_is_walked(
        #[case] levels: usize,
        #[case] open: &str,
        #[case] close: &str,
        #[case] expected: Result<Option<&str>, Refused>,
    ) {
        let inner = format!(
            "{}{}{}",
            open.repeat(levels),
            description("deep"),
            close.repeat(levels)
        );
        let packet = packet(&desc(&inner, ""));
        assert_eq!(
            xmp_comment(&packet),
            expected.map(|comment| comment.map(str::to_owned))
        );
    }

    #[rstest]
    #[case::two_lines("\nxmp\n   4\n6162\n6364\n", Ok(Some("abcd")))]
    #[case::an_odd_digit_dropped("\nxmp\n   1\n616\n", Ok(Some("a")))]
    #[case::nuls_trimmed("\nxmp\n   2\n00610000\n", Ok(Some("a")))]
    #[case::another_profile("\nexif\n   1\n61\n", Ok(None))]
    #[case::too_few_lines("\nxmp\n", Err(Refused))]
    #[case::not_hex("\nxmp\n   1\nzz\n", Err(Refused))]
    fn raw_profiles_read_as_processrawprofile_reads(
        #[case] profile: &str,
        #[case] expected: Result<Option<&str>, Refused>,
    ) {
        assert_eq!(
            raw_xmp(profile),
            expected.map(|packet| packet.map(str::to_owned))
        );
    }
}
