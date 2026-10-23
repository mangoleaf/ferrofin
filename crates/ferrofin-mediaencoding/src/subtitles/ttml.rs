//! TTML (`.ttml`) writer.
//!
//! Port of libse 4.0.12's `Nikse.SubtitleEdit.Core.SubtitleFormats.TimedText10.ToText`,
//! which Jellyfin 12's `SubtitleEncoder.TryGetWriter` returns for `ttml`. libse builds
//! the document with `XmlDocument` and writes it with an indenting `XmlWriter`, and
//! the output bytes follow from both, so this module carries a small private model of
//! the two: an element tree that keeps what `XmlDocument` keeps (namespaces, attribute
//! order, `<x />` against `<x></x>`), the writer's indentation, which stops inside an
//! element once text is written into it, and its escaping.
//!
//! A source SSA/ASS script's styles become TTML styles
//! (`SubStationAlphaHeaderToTimedText`); its cues then sit in a `div` created without
//! a namespace, which is why each `<p>` re-declares the TTML namespace there.
// TODO(gl-768): libse also writes from a TTML source header (an existing `<head>`
// with its own styles and regions, several `div`s), and from paragraphs with a
// `Language` or `NewSection`. Only libse's TTML and other XML loaders set those, and
// none is ported; port those `ToText` branches with the loader that reaches them.

use super::dotnet::format_double_custom;
use super::libse_text::{
    color_to_hex, fix_invalid_italic_tags, remove_control_characters_but_white_space,
    remove_ssa_tags, split_to_lines,
};
use super::model::{Paragraph, Subtitle, TimeCode};
use std::fmt::Write as _;

use super::ssa_style::{ssa_style_from_header, styles_from_header};

/// `TimedText10.TtmlNamespace`.
const TTML_NS: &str = "http://www.w3.org/ns/ttml";
/// `TimedText10.TtmlParameterNamespace`.
const PARAMETER_NS: &str = "http://www.w3.org/ns/ttml#parameter";
/// `TimedText10.TtmlStylingNamespace`.
const STYLING_NS: &str = "http://www.w3.org/ns/ttml#styling";
/// `TimedText10.TtmlMetadataNamespace`.
const METADATA_NS: &str = "http://www.w3.org/ns/ttml#metadata";
/// The namespace the `xml` prefix is bound to.
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
/// The namespace of `xmlns` declarations.
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// The nine regions of libse's default document, as `(extent, origin, displayAlign,
/// textAlign, xml:id)`.
const DEFAULT_REGIONS: [(&str, &str, &str, &str, &str); 9] = [
    ("80% 40%", "10% 10%", "before", "start", "topLeft"),
    ("80% 40%", "10% 30%", "center", "start", "centerLeft"),
    ("80% 40%", "10% 50%", "after", "start", "bottomLeft"),
    ("80% 40%", "10% 10%", "before", "center", "topCenter"),
    ("80% 40%", "10% 30%", "center", "center", "centerCenter"),
    ("80% 40%", "10% 50%", "after", "center", "bottomCenter"),
    ("80% 40%", "10% 10%", "before", "end", "topRight"),
    ("80% 40%", "10% 30%", "center", "end", "centerRight"),
    ("80% 40%", "10% 50%", "after", "end", "bottomRight"),
];

/// `MakeParagraph`'s `{\anN}` regions: the tag, the region it must find, and the
/// region it writes. libse writes `centerСenter` with a Cyrillic `С` for `{\an5}`.
const ALIGNMENT_REGIONS: [(&str, &str, &str); 9] = [
    ("{\\an1}", "bottomLeft", "bottomLeft"),
    ("{\\an2}", "bottomCenter", "bottomCenter"),
    ("{\\an3}", "bottomRight", "bottomRight"),
    ("{\\an4}", "centerLeft", "centerLeft"),
    ("{\\an5}", "centerCenter", "center\u{0421}enter"),
    ("{\\an6}", "centerRight", "centerRight"),
    ("{\\an7}", "topLeft", "topLeft"),
    ("{\\an8}", "topCenter", "topCenter"),
    ("{\\an9}", "topRight", "topRight"),
];

/// The `tts:` effects `MakeParagraph` turns into attributes, in its order.
const EFFECT_ATTRIBUTES: [&str; 6] = [
    "tts:fontSize",
    "tts:fontFamily",
    "tts:backgroundColor",
    "tts:origin",
    "tts:extent",
    "tts:textAlign",
];

/// Serializes `subtitle` to TTML 1.0 text.
///
/// Port of `TimedText10.ToText(subtitle, title)`, which does not use the title.
#[must_use]
pub fn to_text(subtitle: &Subtitle, _title: &str) -> String {
    // `hasStyleHead`: an SSA/ASS header is never XML, so it only becomes a style head
    // by conversion.
    let converted =
        subtitle.header.contains("[V4+ Styles]") || subtitle.header.contains("[V4 Styles]");
    let mut root = if converted {
        ssa_header_document(&subtitle.header)
    } else {
        default_document()
    };

    let default_style = body(&root)
        .and_then(|b| b.attr("style"))
        .unwrap_or_default()
        .to_owned();
    let header_styles = ids_of(&root, "style");
    let regions = ids_of(&root, "region");

    let mut paragraphs = Vec::with_capacity(subtitle.paragraphs.len());
    for (no, p) in subtitle.paragraphs.iter().enumerate() {
        // `p.Style = p.Extra` for a converted script, on the paragraph being written.
        let style = match (&p.style, converted) {
            (Some(style), _) if !style.is_empty() => Some(style.as_str()),
            (_, true) => p.extra.as_deref(),
            (style, false) => style.as_deref(),
        };
        paragraphs.push(make_paragraph(
            &mut root,
            &default_style,
            no,
            &header_styles,
            &regions,
            p,
            style,
        ));
    }

    if let Some(body) = body_mut(&mut root) {
        // The `div` the paragraphs go in: the body's TTML `div`, else its first child,
        // else a new `div` with no namespace.
        let index = body
            .children
            .iter()
            .position(|n| matches!(n, Node::Element(e) if e.is("div", TTML_NS)))
            .or_else(|| (!body.children.is_empty()).then_some(0));
        let index = index.unwrap_or_else(|| {
            body.append(Node::Element(Element::created("", "div", "")));
            body.children.len() - 1
        });
        if let Some(Node::Element(div)) = body.children.get_mut(index) {
            for p in paragraphs {
                div.append(Node::Element(p));
            }
        }
        // An empty first child of the body is dropped.
        if matches!(body.children.first(), Some(Node::Element(first)) if first.children.is_empty())
        {
            body.remove_child(0);
        }
    }

    XmlWriter::serialize(&root)
        .trim()
        .replace(" xmlns=\"\"", "")
        .replace(" xmlns:tts=\"http://www.w3.org/ns/10/ttml#style\">", ">")
}

/// `ConvertToTimeString` with the default `TimedText10TimeCodeFormat` (`Source`, no
/// source format): seconds as `{0:0.0##}s`.
fn time_string(tc: TimeCode) -> String {
    format!("{}s", format_double_custom(tc.total_seconds(), 1, 3))
}

/// `TimedText10.GetEffect`: the value of `tag=value` among the `|`-separated
/// effects, or empty.
fn effect(p: &Paragraph, tag: &str) -> String {
    p.effect
        .as_deref()
        .unwrap_or("")
        .split('|')
        .find_map(|s| {
            let parts: Vec<&str> = s.split('=').collect();
            (parts.len() == 2 && parts[0] == tag).then(|| parts[1].to_owned())
        })
        .unwrap_or_default()
}

/// `MakeParagraph`: one cue as a `<p>`.
fn make_paragraph(
    root: &mut Element,
    default_style: &str,
    no: usize,
    header_styles: &[String],
    regions: &[String],
    p: &Paragraph,
    style: Option<&str>,
) -> Element {
    let mut paragraph = Element::created("", "p", TTML_NS);
    let text = remove_control_characters_but_white_space(&p.text);

    let mut region = effect(p, "region");
    if let Some(own) = p.region.as_deref().filter(|r| !r.is_empty())
        && regions.contains(&region)
    {
        own.clone_into(&mut region);
    }
    if region.is_empty() {
        for (tag, needed, written) in ALIGNMENT_REGIONS {
            if text.starts_with(tag) && add_default_region_if_not_exists(root, needed) {
                written.clone_into(&mut region);
            }
        }
    } else if text.starts_with("{\\an8}") {
        let top = aligned_regions(root, top_aligned);
        if top.len() == 1 {
            region.clone_from(&top[0]);
        } else if let Some(t) = top.iter().find(|r| r.to_lowercase() == "top") {
            region.clone_from(t);
        }
    } else if text.starts_with("{\\an2}") || !text.contains("{\\an") {
        let bottom = aligned_regions(root, bottom_aligned);
        if bottom.len() == 1 {
            region.clone_from(&bottom[0]);
        } else if let Some(b) = bottom.iter().find(|r| r.to_lowercase() == "bottom") {
            region.clone_from(b);
        }
    }

    let text = fix_invalid_italic_tags(&remove_ssa_tags(&text));
    let text = split_to_lines(&text).join("<br/>");
    // `LoadXml($"<root>{text.Replace("&", "&amp;")}</root>")`, then the same with an
    // existing `&amp;` kept as it is; libse protects it with a GUID, here with NUL,
    // which the control-character removal above guarantees is absent.
    let content =
        parse_content(&format!("<root>{}</root>", text.replace('&', "&amp;"))).or_else(|| {
            let protected = text.replace("&amp;", "\0").replace(['&', '\0'], "&amp;");
            parse_content(&format!("<root>{protected}</root>"))
        });
    match content {
        Some(nodes) => convert_paragraph_nodes(&nodes, &mut paragraph),
        // Wrong markup: the text without any `<` or `>`.
        None => paragraph.append(Node::Text(text.replace(['<', '>'], ""))),
    }

    paragraph.set("", "begin", "", &time_string(p.start_time));
    paragraph.set("xml", "id", XML_NS, &format!("p{no}"));
    paragraph.set("", "end", "", &time_string(p.end_time));
    if !region.is_empty() {
        paragraph.set("", "region", "", &region);
    }
    let xml_space = effect(p, "xml:space");
    if !xml_space.is_empty() {
        paragraph.set("xml", "space", XML_NS, &xml_space);
    }
    for name in EFFECT_ATTRIBUTES {
        let value = effect(p, name);
        if !value.is_empty() {
            paragraph.set("tts", &name[4..], STYLING_NS, &value);
        }
    }
    if let Some(style) = style
        && header_styles.iter().any(|s| s == style)
        && style != default_style
    {
        paragraph.set("", "style", "", style);
    }
    paragraph
}

/// `ConvertParagraphNodeToTtmlNode`: `<br>` stays, `<i>`/`<b>`/`<u>`/`<font>` become
/// styled `<span>`s, ruby and bouten classes become a `style` span, and any other
/// element is unwrapped. The new elements have no namespace.
fn convert_paragraph_nodes(nodes: &[ContentNode], target: &mut Element) {
    for node in nodes {
        match node {
            ContentNode::Text(t) => target.append(Node::Text(t.clone())),
            ContentNode::Element {
                name,
                attrs,
                children,
            } => {
                let attr = |n: &str| attrs.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
                let mut span = Element::created("", "span", "");
                match name.as_str() {
                    "br" => span = Element::created("", "br", ""),
                    "i" => span.set("tts", "fontStyle", STYLING_NS, "italic"),
                    "b" => span.set("tts", "fontWeight", STYLING_NS, "bold"),
                    "u" => span.set("tts", "textDecoration", STYLING_NS, "underline"),
                    "font" => {
                        if let Some(face) = attr("face") {
                            span.set("tts", "fontFamily", STYLING_NS, face);
                        }
                        if let Some(color) = attr("color") {
                            span.set("tts", "color", STYLING_NS, color);
                        }
                    }
                    other
                        if other.starts_with("bouten-")
                            || other == "horizontalDigit"
                            || other.starts_with("ruby-") =>
                    {
                        span.set("", "style", "", other);
                    }
                    _ => {
                        convert_paragraph_nodes(children, target);
                        continue;
                    }
                }
                convert_paragraph_nodes(children, &mut span);
                target.append(Node::Element(span));
            }
        }
    }
}

// ── The document ─────────────────────────────────────────────────────────────

/// A node of the document being written.
#[derive(Debug, Clone)]
enum Node {
    /// An element.
    Element(Element),
    /// A text node.
    Text(String),
}

/// An attribute: its prefix, local name, namespace and value. A namespace declaration
/// is an attribute in [`XMLNS_NS`] (`xmlns`, or prefix `xmlns`).
#[derive(Debug, Clone)]
struct Attr {
    prefix: String,
    local: String,
    ns: String,
    value: String,
}

impl Attr {
    fn qualified(&self) -> String {
        if self.prefix.is_empty() {
            self.local.clone()
        } else {
            format!("{}:{}", self.prefix, self.local)
        }
    }

    /// The prefix this declares (`""` for the default namespace), when it is a
    /// namespace declaration.
    fn declares(&self) -> Option<&str> {
        match (self.prefix.as_str(), self.local.as_str()) {
            ("", "xmlns") => Some(""),
            ("xmlns", local) => Some(local),
            _ => None,
        }
    }
}

/// An element, as `XmlDocument` keeps it.
#[derive(Debug, Clone)]
struct Element {
    prefix: String,
    local: String,
    ns: String,
    attrs: Vec<Attr>,
    children: Vec<Node>,
    /// `XmlElement.IsEmpty` when it has no children: a created or `<x/>`-parsed
    /// element writes as `<x />`, one parsed as `<x></x>` or emptied by removing its
    /// children as `<x></x>`.
    short_when_empty: bool,
}

impl Element {
    /// `XmlDocument.CreateElement`: starts out empty, in the short form.
    fn created(prefix: &str, local: &str, ns: &str) -> Self {
        Self {
            prefix: prefix.to_owned(),
            local: local.to_owned(),
            ns: ns.to_owned(),
            attrs: Vec::new(),
            children: Vec::new(),
            short_when_empty: true,
        }
    }

    /// An element `LoadXml` read with an end tag of its own (`<x></x>`).
    fn long(prefix: &str, local: &str, ns: &str) -> Self {
        Self {
            short_when_empty: false,
            ..Self::created(prefix, local, ns)
        }
    }

    fn with(mut self, attrs: &[(&str, &str, &str, &str)]) -> Self {
        for &(prefix, local, ns, value) in attrs {
            self.set(prefix, local, ns, value);
        }
        self
    }

    fn with_children(mut self, children: Vec<Element>) -> Self {
        for child in children {
            self.append(Node::Element(child));
        }
        self
    }

    fn is(&self, local: &str, ns: &str) -> bool {
        self.local == local && self.ns == ns
    }

    /// `Attributes[name].Value`, by qualified name.
    fn attr(&self, qualified: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.qualified() == qualified)
            .map(|a| a.value.as_str())
    }

    /// `SetAttribute` / `Attributes.Append`: replaces a same-named attribute in
    /// place, else appends.
    fn set(&mut self, prefix: &str, local: &str, ns: &str, value: &str) {
        if let Some(a) = self
            .attrs
            .iter_mut()
            .find(|a| a.prefix == prefix && a.local == local)
        {
            value.clone_into(&mut a.value);
        } else {
            self.attrs.push(Attr {
                prefix: prefix.to_owned(),
                local: local.to_owned(),
                ns: ns.to_owned(),
                value: value.to_owned(),
            });
        }
    }

    fn append(&mut self, node: Node) {
        self.children.push(node);
    }

    /// `RemoveChild`: an element left without children writes as `<x></x>`.
    fn remove_child(&mut self, index: usize) {
        self.children.remove(index);
        if self.children.is_empty() {
            self.short_when_empty = false;
        }
    }

    fn child_mut(&mut self, local: &str) -> Option<&mut Element> {
        self.children.iter_mut().find_map(|n| match n {
            Node::Element(e) if e.is(local, TTML_NS) => Some(e),
            _ => None,
        })
    }

    fn child(&self, local: &str) -> Option<&Element> {
        self.children.iter().find_map(|n| match n {
            Node::Element(e) if e.is(local, TTML_NS) => Some(e),
            _ => None,
        })
    }

    /// Every descendant TTML element named `local`, in document order
    /// (`SelectNodes("//ttml:<local>")`).
    fn descendants<'a>(&'a self, local: &str, out: &mut Vec<&'a Element>) {
        for node in &self.children {
            if let Node::Element(e) = node {
                if e.is(local, TTML_NS) {
                    out.push(e);
                }
                e.descendants(local, out);
            }
        }
    }
}

/// libse's `xmlStructure` as `LoadXml` reads it: the document every TTML it writes
/// starts from.
fn default_document() -> Element {
    let tt = Element::created("", "tt", TTML_NS).with(&[
        ("", "xmlns", XMLNS_NS, TTML_NS),
        ("xmlns", "ttp", XMLNS_NS, PARAMETER_NS),
        ("ttp", "timeBase", PARAMETER_NS, "media"),
        ("xmlns", "tts", XMLNS_NS, STYLING_NS),
        ("xml", "lang", XML_NS, "en"),
        ("xmlns", "ttm", XMLNS_NS, METADATA_NS),
    ]);
    let style = Element::created("", "style", TTML_NS).with(&[
        ("xml", "id", XML_NS, "s0"),
        ("tts", "backgroundColor", STYLING_NS, "black"),
        ("tts", "fontStyle", STYLING_NS, "normal"),
        ("tts", "fontSize", STYLING_NS, "16px"),
        ("tts", "fontFamily", STYLING_NS, "sansSerif"),
        ("tts", "color", STYLING_NS, "white"),
    ]);
    let regions = DEFAULT_REGIONS
        .iter()
        .map(|&(extent, origin, display, text, id)| {
            Element::created("", "region", TTML_NS).with(&[
                ("tts", "extent", STYLING_NS, extent),
                ("tts", "origin", STYLING_NS, origin),
                ("tts", "displayAlign", STYLING_NS, display),
                ("tts", "textAlign", STYLING_NS, text),
                ("xml", "id", XML_NS, id),
            ])
        });
    let head = Element::long("", "head", TTML_NS).with_children(vec![
        Element::long("", "metadata", TTML_NS).with_children(vec![Element::long(
            "ttm",
            "title",
            METADATA_NS,
        )]),
        Element::long("", "styling", TTML_NS).with_children(vec![style]),
        Element::long("", "layout", TTML_NS).with_children(regions.collect()),
    ]);
    let body = Element::long("", "body", TTML_NS)
        .with(&[("", "style", "", "s0")])
        .with_children(vec![Element::created("", "div", TTML_NS)]);
    tt.with_children(vec![head, body])
}

/// `SubStationAlphaHeaderToTimedText`: libse's empty document (whose empty `div` its
/// own `ToText` has already dropped) with the styling replaced by one TTML style per
/// script style.
fn ssa_header_document(header: &str) -> Element {
    let mut root = default_document();
    if let Some(body) = body_mut(&mut root) {
        body.remove_child(0);
    }
    if let Some(styling) = root.child_mut("head").and_then(|h| h.child_mut("styling")) {
        // `RemoveAll()`.
        styling.attrs.clear();
        styling.children.clear();
        for name in styles_from_header(header) {
            let ssa = ssa_style_from_header(&name, header);
            // `AddStyleToXml`.
            styling.append(Node::Element(Element::created("", "style", TTML_NS).with(
                &[
                    ("xml", "id", XML_NS, &ssa.name),
                    ("tts", "fontFamily", STYLING_NS, &ssa.font_name),
                    (
                        "tts",
                        "fontWeight",
                        STYLING_NS,
                        if ssa.bold { "bold" } else { "normal" },
                    ),
                    (
                        "tts",
                        "fontStyle",
                        STYLING_NS,
                        if ssa.italic { "italic" } else { "normal" },
                    ),
                    ("tts", "color", STYLING_NS, &color_to_hex(ssa.primary)),
                    (
                        "tts",
                        "fontSize",
                        STYLING_NS,
                        &ssa.font_size.to_invariant_string(),
                    ),
                ],
            )));
        }
    }
    root
}

fn body(root: &Element) -> Option<&Element> {
    root.child("body")
}

fn body_mut(root: &mut Element) -> Option<&mut Element> {
    root.child_mut("body")
}

/// `GetStylesFromHeader` / `GetRegionsFromHeader`: the `xml:id` (else `id`) of every
/// TTML `style` or `region` element.
fn ids_of(root: &Element, local: &str) -> Vec<String> {
    let mut found = Vec::new();
    root.descendants(local, &mut found);
    found
        .iter()
        .filter_map(|e| e.attr("xml:id").or_else(|| e.attr("id")).map(str::to_owned))
        .collect()
}

/// `AddDefaultRegionIfNotExists`: whether the head has the region, copying it from
/// libse's default document into the layout when it does not.
fn add_default_region_if_not_exists(root: &mut Element, region: &str) -> bool {
    let has = |root: &Element| {
        let mut found = Vec::new();
        if let Some(head) = root.child("head") {
            head.descendants("region", &mut found);
        }
        found.iter().any(|r| r.attr("xml:id") == Some(region))
    };
    if has(root) {
        return true;
    }
    let mut defaults = Vec::new();
    let template = default_document();
    template.descendants("region", &mut defaults);
    let Some(copy) = defaults
        .iter()
        .find(|r| r.attr("xml:id") == Some(region))
        .map(|r| (*r).clone())
    else {
        return false;
    };
    match root.child_mut("head").and_then(|h| h.child_mut("layout")) {
        Some(layout) => {
            layout.append(Node::Element(copy));
            true
        }
        None => false,
    }
}

/// `GetRegionsTopFromHeader` / `GetRegionsBottomFromHeader`: the ids of the regions a
/// child (else the region itself) is aligned for; a malformed percentage ends the
/// search where libse's exception does.
fn aligned_regions(root: &Element, aligned: fn(&Element) -> Result<bool, ()>) -> Vec<String> {
    let mut regions = Vec::new();
    root.descendants("region", &mut regions);
    let mut out = Vec::new();
    for region in regions {
        let mut hit = false;
        for child in &region.children {
            if let Node::Element(e) = child {
                match aligned(e) {
                    Ok(true) => {
                        hit = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(()) => return out,
                }
            }
        }
        if !hit {
            match aligned(region) {
                Ok(v) => hit = v,
                Err(()) => return out,
            }
        }
        if hit && let Some(id) = region.attr("xml:id").or_else(|| region.attr("id")) {
            out.push(id.to_owned());
        }
    }
    out
}

/// The `tts:`-or-bare attribute lookups `GetIfTopAligned`/`GetIfBottomAligned` share:
/// origin and extent split on spaces, and the display alignment.
fn alignment_parts(node: &Element) -> (Vec<String>, Vec<String>, String) {
    let get = |name: &str| {
        node.attr(&format!("tts:{name}"))
            .or_else(|| node.attr(name))
            .unwrap_or("")
            .to_owned()
    };
    let split = |s: String| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
    (
        split(get("origin")),
        split(get("extent")),
        get("displayAlign"),
    )
}

/// Whether origin and extent are both two percentages and the alignment is set.
fn percentages(origin: &[String], extent: &[String], display: &str) -> bool {
    origin.len() == 2
        && extent.len() == 2
        && origin.iter().chain(extent).all(|v| v.ends_with('%'))
        && !display.is_empty()
}

/// `Convert.ToDouble(value.TrimEnd('%'), CultureInfo.InvariantCulture)`.
fn percent(value: &str) -> Result<f64, ()> {
    value.trim_end_matches('%').trim().parse().map_err(|_| ())
}

/// `GetIfTopAligned`.
fn top_aligned(node: &Element) -> Result<bool, ()> {
    let (origin, extent, display) = alignment_parts(node);
    if percentages(&origin, &extent, &display) {
        let y = percent(&origin[1])?;
        let height = percent(&extent[1])?;
        Ok((y <= 15.0 && height < 40.0) || (y <= 25.0 && display == "before"))
    } else {
        Ok(display == "before")
    }
}

/// `GetIfBottomAligned`.
fn bottom_aligned(node: &Element) -> Result<bool, ()> {
    let (origin, extent, display) = alignment_parts(node);
    if percentages(&origin, &extent, &display) {
        Ok(percent(&origin[1])? > 40.0 && display == "after")
    } else {
        Ok(false)
    }
}

// ── The writer ───────────────────────────────────────────────────────────────

/// `XmlDocument.Save` through `XmlWriter.Create(StringBuilder, { Indent = true })`:
/// the XML declaration, two-space indentation, which stops inside an element once
/// text has been written into it, `<x />` for an element still in its short form,
/// and the namespace declarations an element or attribute needs, after its attributes.
struct XmlWriter {
    out: String,
    indent: usize,
    mixed: bool,
    mixed_stack: Vec<bool>,
    /// Where the last text write ended (`_textPos`).
    text_pos: usize,
    /// Where the last start tag's content began (`_contentPos`).
    content_pos: usize,
    /// The prefix bindings in scope, innermost last.
    scopes: Vec<Vec<(String, String)>>,
}

impl XmlWriter {
    fn serialize(root: &Element) -> String {
        let mut w = Self {
            out: String::new(),
            indent: 0,
            mixed: false,
            mixed_stack: Vec::new(),
            text_pos: 0,
            content_pos: 0,
            scopes: vec![vec![
                (String::new(), String::new()),
                ("xml".to_owned(), XML_NS.to_owned()),
                ("xmlns".to_owned(), XMLNS_NS.to_owned()),
            ]],
        };
        w.out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>");
        w.element(root);
        w.out
    }

    fn lookup(&self, prefix: &str) -> Option<&str> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.iter().rev())
            .find(|(p, _)| p == prefix)
            .map(|(_, ns)| ns.as_str())
    }

    fn write_indent(&mut self) {
        self.out.push('\n');
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
    }

    fn element(&mut self, e: &Element) {
        if !self.mixed && self.text_pos != self.out.len() {
            self.write_indent();
        }
        self.indent += 1;
        self.mixed_stack.push(self.mixed);
        let name = if e.prefix.is_empty() {
            e.local.clone()
        } else {
            format!("{}:{}", e.prefix, e.local)
        };
        self.out.push('<');
        self.out.push_str(&name);

        let mut scope = Vec::new();
        for a in &e.attrs {
            if let Some(prefix) = a.declares() {
                scope.push((prefix.to_owned(), a.value.clone()));
            }
            let _ = write!(
                self.out,
                " {}=\"{}\"",
                a.qualified(),
                escape_attribute(&a.value)
            );
        }
        self.scopes.push(scope);
        // The declarations the element and its attributes still need.
        let mut implied = Vec::new();
        if self.lookup(&e.prefix) != Some(e.ns.as_str()) {
            implied.push((e.prefix.clone(), e.ns.clone()));
        }
        for a in e
            .attrs
            .iter()
            .filter(|a| a.declares().is_none() && !a.prefix.is_empty())
        {
            if self.lookup(&a.prefix) != Some(a.ns.as_str())
                && !implied.iter().any(|(p, _)| *p == a.prefix)
            {
                implied.push((a.prefix.clone(), a.ns.clone()));
            }
        }
        for (prefix, ns) in implied {
            // Writing into a String cannot fail.
            if prefix.is_empty() {
                let _ = write!(self.out, " xmlns=\"{}\"", escape_attribute(&ns));
            } else {
                let _ = write!(self.out, " xmlns:{prefix}=\"{}\"", escape_attribute(&ns));
            }
            if let Some(scope) = self.scopes.last_mut() {
                scope.push((prefix, ns));
            }
        }
        self.out.push('>');
        self.content_pos = self.out.len();

        for child in &e.children {
            match child {
                Node::Element(c) => self.element(c),
                Node::Text(t) => {
                    self.mixed = true;
                    self.out.push_str(&escape_text(t));
                    self.text_pos = self.out.len();
                }
            }
        }

        self.indent -= 1;
        let has_content = self.content_pos != self.out.len();
        if !self.mixed && has_content && self.text_pos != self.out.len() {
            self.write_indent();
        }
        self.mixed = self.mixed_stack.pop().unwrap_or(false);
        if e.children.is_empty() && e.short_when_empty && !has_content {
            // `WriteEndElement` on an empty element overwrites its `>`.
            self.out.pop();
            self.out.push_str(" />");
        } else {
            self.out.push_str("</");
            self.out.push_str(&name);
            self.out.push('>');
        }
        self.scopes.pop();
    }
}

/// `XmlEncodedRawTextWriter`'s text escaping.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(ch),
        }
    }
    out
}

/// `XmlEncodedRawTextWriter`'s attribute-value escaping.
fn escape_attribute(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            _ => out.push(ch),
        }
    }
    out
}

// ── The paragraph content ────────────────────────────────────────────────────

/// A node of a cue's markup as `LoadXml` reads it: whitespace-only text, comments,
/// CDATA and processing instructions are not kept (none is an `XmlText`, so
/// `ConvertParagraphNodeToTtmlNode` skips them anyway).
#[derive(Debug)]
enum ContentNode {
    Element {
        name: String,
        attrs: Vec<(String, String)>,
        children: Vec<ContentNode>,
    },
    Text(String),
}

/// How deep a cue's markup may nest before the cue takes libse's "wrong markup"
/// fallback (its `<` and `>` stripped). libse recurses as deep as the markup goes;
/// the parser, the node conversion and the writer here recurse too, so a cue nested
/// thousands of tags deep would overflow the worker's stack. No real subtitle nests
/// anywhere near this deep.
const MAX_CONTENT_DEPTH: usize = 256;

/// `XmlDocument.LoadXml(xml)` (whitespace not preserved) for a cue's markup: the
/// root element's children, or `None` where `LoadXml` throws, or where the markup
/// nests deeper than [`MAX_CONTENT_DEPTH`].
fn parse_content(xml: &str) -> Option<Vec<ContentNode>> {
    let mut parser = ContentParser {
        chars: xml.chars().collect(),
        pos: 0,
        scopes: vec![vec!["xml".to_owned()]],
    };
    parser.skip_misc()?;
    if parser.peek() != Some('<') {
        return None;
    }
    let root = parser.element()?;
    parser.skip_misc()?;
    if parser.pos != parser.chars.len() {
        return None;
    }
    match root {
        ContentNode::Element { children, .. } => Some(children),
        ContentNode::Text(_) => None,
    }
}

/// A well-formedness parser for one element and its content, with XML namespaces.
struct ContentParser {
    chars: Vec<char>,
    pos: usize,
    /// The prefixes declared in scope, one entry per open element (plus the
    /// document's), so its length is also the nesting depth.
    scopes: Vec<Vec<String>>,
}

impl ContentParser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn starts(&self, s: &str) -> bool {
        s.chars()
            .enumerate()
            .all(|(i, c)| self.chars.get(self.pos + i) == Some(&c))
    }

    fn skip_ws(&mut self) -> bool {
        let start = self.pos;
        while matches!(self.peek(), Some(' ' | '\t' | '\r' | '\n')) {
            self.pos += 1;
        }
        self.pos > start
    }

    /// Whitespace, comments and processing instructions around the root.
    fn skip_misc(&mut self) -> Option<()> {
        loop {
            self.skip_ws();
            if self.starts("<!--") {
                self.comment()?;
            } else if self.starts("<?") {
                self.processing_instruction()?;
            } else {
                return Some(());
            }
        }
    }

    fn comment(&mut self) -> Option<()> {
        self.pos += 4;
        loop {
            if self.starts("--") {
                return self.starts("-->").then(|| self.pos += 3);
            }
            self.peek().filter(|&c| is_xml_char(c))?;
            self.pos += 1;
        }
    }

    fn processing_instruction(&mut self) -> Option<()> {
        self.pos += 2;
        let target = self.name()?;
        if target.eq_ignore_ascii_case("xml") || target.contains(':') {
            return None;
        }
        if !self.starts("?>") && !self.skip_ws() {
            return None;
        }
        loop {
            if self.starts("?>") {
                self.pos += 2;
                return Some(());
            }
            self.peek().filter(|&c| is_xml_char(c))?;
            self.pos += 1;
        }
    }

    fn name(&mut self) -> Option<String> {
        let first = self.peek().filter(|&c| is_name_start(c))?;
        let mut name = String::from(first);
        self.pos += 1;
        while let Some(c) = self.peek().filter(|&c| is_name_char(c)) {
            name.push(c);
            self.pos += 1;
        }
        Some(name)
    }

    /// `&lt;`, `&gt;`, `&amp;`, `&apos;`, `&quot;` and character references; any other
    /// entity is undefined.
    fn reference(&mut self) -> Option<char> {
        self.pos += 1;
        let end = self.chars[self.pos..].iter().position(|&c| c == ';')? + self.pos;
        let body: String = self.chars[self.pos..end].iter().collect();
        self.pos = end + 1;
        let c = match body.as_str() {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "apos" => '\'',
            "quot" => '"',
            _ => {
                let number = body.strip_prefix('#')?;
                let (digits, radix) = match number.strip_prefix('x') {
                    Some(hex) => (hex, 16),
                    None => (number, 10),
                };
                if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                    return None;
                }
                let code = u32::from_str_radix(digits, radix).ok()?;
                char::from_u32(code).filter(|&c| is_xml_char(c))?
            }
        };
        Some(c)
    }

    /// A start tag's attributes, through its `>` or `/>`: each name, its value with
    /// references resolved and white space normalized, and whether the tag was empty.
    fn attributes(&mut self) -> Option<(Vec<(String, String)>, bool)> {
        let mut attrs: Vec<(String, String)> = Vec::new();
        let empty = loop {
            let spaced = self.skip_ws();
            if self.starts("/>") {
                self.pos += 2;
                break true;
            }
            if self.peek() == Some('>') {
                self.pos += 1;
                break false;
            }
            if !spaced {
                return None;
            }
            let attr = self.name()?;
            self.skip_ws();
            if self.peek() != Some('=') {
                return None;
            }
            self.pos += 1;
            self.skip_ws();
            let quote = self.peek().filter(|&q| q == '"' || q == '\'')?;
            self.pos += 1;
            let mut value = String::new();
            loop {
                match self.peek()? {
                    q if q == quote => {
                        self.pos += 1;
                        break;
                    }
                    '<' => return None,
                    '&' => value.push(self.reference()?),
                    // Attribute-value normalization.
                    '\t' | '\n' | '\r' => {
                        value.push(' ');
                        self.pos += 1;
                    }
                    c if is_xml_char(c) => {
                        value.push(c);
                        self.pos += 1;
                    }
                    _ => return None,
                }
            }
            if attrs.iter().any(|(n, _)| *n == attr) {
                return None;
            }
            attrs.push((attr, value));
        };
        Some((attrs, empty))
    }

    fn element(&mut self) -> Option<ContentNode> {
        if self.scopes.len() > MAX_CONTENT_DEPTH {
            return None;
        }
        self.pos += 1; // `<`
        let name = self.name()?;
        let (attrs, empty) = self.attributes()?;

        // Namespaces: declarations first, then every prefix used must be declared.
        let mut scope = Vec::new();
        for (attr, value) in &attrs {
            if let Some(prefix) = attr.strip_prefix("xmlns:") {
                if value.is_empty() || prefix == "xmlns" || (prefix == "xml") != (value == XML_NS) {
                    return None;
                }
                scope.push(prefix.to_owned());
            }
        }
        self.scopes.push(scope);
        let declared = |scopes: &[Vec<String>], prefix: &str| {
            scopes.iter().any(|s| s.iter().any(|p| p == prefix))
        };
        if !qualified_name_ok(&name, false, |p| declared(&self.scopes, p)) {
            return None;
        }
        for (attr, _) in &attrs {
            if attr != "xmlns"
                && !attr.starts_with("xmlns:")
                && !qualified_name_ok(attr, true, |p| declared(&self.scopes, p))
            {
                return None;
            }
        }

        let mut children = Vec::new();
        if !empty {
            let mut text = String::new();
            let flush = |text: &mut String, children: &mut Vec<ContentNode>| {
                if !text.is_empty() && !text.chars().all(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
                {
                    children.push(ContentNode::Text(std::mem::take(text)));
                }
                text.clear();
            };
            loop {
                if self.starts("</") {
                    flush(&mut text, &mut children);
                    self.pos += 2;
                    let end = self.name()?;
                    self.skip_ws();
                    if end != name || self.peek() != Some('>') {
                        return None;
                    }
                    self.pos += 1;
                    break;
                }
                if self.starts("<!--") {
                    flush(&mut text, &mut children);
                    self.comment()?;
                } else if self.starts("<![CDATA[") {
                    flush(&mut text, &mut children);
                    self.pos += 9;
                    while !self.starts("]]>") {
                        self.peek().filter(|&c| is_xml_char(c))?;
                        self.pos += 1;
                    }
                    self.pos += 3;
                } else if self.starts("<?") {
                    flush(&mut text, &mut children);
                    self.processing_instruction()?;
                } else if self.starts("<!") {
                    return None;
                } else if self.peek() == Some('<') {
                    flush(&mut text, &mut children);
                    children.push(self.element()?);
                } else if self.peek() == Some('&') {
                    text.push(self.reference()?);
                } else if self.starts("]]>") {
                    return None;
                } else {
                    let c = self.peek().filter(|&c| is_xml_char(c))?;
                    text.push(c);
                    self.pos += 1;
                }
            }
        }
        self.scopes.pop();
        Some(ContentNode::Element {
            name,
            attrs,
            children,
        })
    }
}

/// A QName: at most one `:`, neither first nor last, its prefix declared (`xml` always
/// is; `xmlns` never names an element).
fn qualified_name_ok(name: &str, is_attribute: bool, declared: impl Fn(&str) -> bool) -> bool {
    match name.split_once(':') {
        None => true,
        Some((prefix, local)) => {
            !prefix.is_empty()
                && !local.is_empty()
                && !local.contains(':')
                && (is_attribute || prefix != "xmlns")
                && (prefix == "xml" || declared(prefix))
        }
    }
}

/// XML 1.0 `Char`.
fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

/// XML 1.0 (fifth edition) `NameStartChar`.
fn is_name_start(c: char) -> bool {
    matches!(c,
        ':' | 'A'..='Z' | '_' | 'a'..='z'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}' | '\u{370}'..='\u{37D}'
        | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}'
        | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}' | '\u{F900}'..='\u{FDCF}'
        | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
}

/// XML 1.0 (fifth edition) `NameChar`.
fn is_name_char(c: char) -> bool {
    is_name_start(c)
        || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}
