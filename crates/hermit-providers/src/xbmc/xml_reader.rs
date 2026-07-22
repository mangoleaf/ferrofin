//! A forward-only, element-oriented XML cursor mirroring the subset of
//! `System.Xml.XmlReader` the NFO parsers rely on, layered over `quick-xml`.
//!
//! The C# parsers are written against `XmlReader`'s stateful streaming API
//! (`ReadElementContentAsString`, `GetAttribute`, `IsEmptyElement`, `ReadSubtree`,
//! `Skip`, `MoveToContent`). Porting those call sites faithfully requires the
//! same positional semantics, so this module first tokenizes the document into a
//! flat [`XmlToken`] list (entities unescaped, adjacent text coalesced) and then
//! exposes an [`XmlCursor`] over it that re-creates the reader contract.
//!
//! Only the surface the parsers actually use is implemented; unused `XmlReader`
//! features (namespaces, attribute-node navigation, DTDs) are omitted.

use quick_xml::Reader;
use quick_xml::events::Event;

/// A flattened XML node, entity-unescaped and with adjacent text coalesced.
#[derive(Debug, Clone, PartialEq, Eq)]
enum XmlToken {
    /// An opening tag `<name ...>` with its attributes as `(key, value)` pairs.
    Start {
        /// The (local) element name.
        name: String,
        /// The attributes in document order.
        attributes: Vec<(String, String)>,
    },
    /// A self-closing tag `<name .../>`.
    Empty {
        /// The (local) element name.
        name: String,
        /// The attributes in document order.
        attributes: Vec<(String, String)>,
    },
    /// A closing tag `</name>`.
    End {
        /// The (local) element name.
        name: String,
    },
    /// Coalesced text / CDATA content.
    Text {
        /// The already-unescaped text value.
        value: String,
    },
}

impl XmlToken {
    fn name(&self) -> &str {
        match self {
            Self::Start { name, .. } | Self::Empty { name, .. } | Self::End { name } => name,
            Self::Text { .. } => "",
        }
    }

    fn is_element(&self) -> bool {
        matches!(self, Self::Start { .. } | Self::Empty { .. })
    }
}

/// The error raised when the document is not well-formed enough to tokenize.
///
/// Mirrors C# `XmlException`, which the parsers catch and swallow.
#[derive(Debug)]
pub struct XmlError;

/// Tokenizes an XML string into a flat node list.
///
/// Entities are unescaped and adjacent text runs are coalesced (as `XmlReader`
/// surfaces a single text node between elements). Comments and processing
/// instructions are skipped (matching `IgnoreComments`/
/// `IgnoreProcessingInstructions`). Returns [`XmlError`] on a malformed document.
fn tokenize(xml: &str) -> Result<Vec<XmlToken>, XmlError> {
    let mut reader = Reader::from_str(xml);
    let config = reader.config_mut();
    config.check_end_names = false;
    config.trim_text(false);

    let mut tokens: Vec<XmlToken> = Vec::new();
    let decoder = reader.decoder();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let attributes = read_attrs(&e, decoder)?;
                tokens.push(XmlToken::Start {
                    name: local_name(&e),
                    attributes,
                });
            }
            Ok(Event::Empty(e)) => {
                let attributes = read_attrs(&e, decoder)?;
                tokens.push(XmlToken::Empty {
                    name: local_name(&e),
                    attributes,
                });
            }
            Ok(Event::End(e)) => {
                tokens.push(XmlToken::End {
                    name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(),
                });
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().map_err(|_| XmlError)?.into_owned();
                push_text(&mut tokens, text);
            }
            Ok(Event::CData(e)) => {
                let text = String::from_utf8_lossy(e.as_ref()).into_owned();
                push_text(&mut tokens, text);
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return Err(XmlError),
        }
    }

    Ok(tokens)
}

/// Reads a start/empty element's attributes as `(local-name, unescaped-value)`.
fn read_attrs(
    e: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
) -> Result<Vec<(String, String)>, XmlError> {
    let mut attrs = Vec::new();
    for attr in e.attributes() {
        let attr = attr.map_err(|_| XmlError)?;
        let key = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
        let value = attr
            .decode_and_unescape_value(decoder)
            .map_err(|_| XmlError)?
            .into_owned();
        attrs.push((key, value));
    }
    Ok(attrs)
}

/// Returns the local (namespace-stripped) name of a start/empty element.
fn local_name(e: &quick_xml::events::BytesStart<'_>) -> String {
    String::from_utf8_lossy(e.local_name().as_ref()).into_owned()
}

/// Appends `text` to the token stream, coalescing with a preceding text node.
fn push_text(tokens: &mut Vec<XmlToken>, text: String) {
    if let Some(XmlToken::Text { value }) = tokens.last_mut() {
        value.push_str(&text);
    } else {
        tokens.push(XmlToken::Text { value: text });
    }
}

/// A forward-only cursor over a tokenized XML document.
///
/// The cursor is positioned "on" a node at `pos`; the parser inspects that node
/// (name/attributes) and then consumes it with one of the `read_*` helpers,
/// which advance `pos`. Mirrors the positional contract of `XmlReader`.
pub struct XmlCursor {
    tokens: Vec<XmlToken>,
    pos: usize,
}

impl XmlCursor {
    /// Tokenizes `xml` and positions the cursor on the first node.
    ///
    /// # Errors
    /// Returns [`XmlError`] if the document cannot be tokenized (the parsers
    /// treat this the same as a caught `XmlException`).
    pub fn new(xml: &str) -> Result<Self, XmlError> {
        Ok(Self {
            tokens: tokenize(xml)?,
            pos: 0,
        })
    }

    /// Whether the cursor has consumed every node (`XmlReader.EOF`).
    #[must_use]
    pub fn eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn current(&self) -> Option<&XmlToken> {
        self.tokens.get(self.pos)
    }

    /// The name of the current node, or `""` if none/at a non-element
    /// (`XmlReader.Name`).
    #[must_use]
    pub fn name(&self) -> &str {
        self.current().map_or("", XmlToken::name)
    }

    /// Whether the current node is an element start/empty
    /// (`NodeType == Element`).
    #[must_use]
    pub fn is_element(&self) -> bool {
        self.current().is_some_and(XmlToken::is_element)
    }

    /// Whether the current node is a text node (`NodeType == Text`).
    #[must_use]
    pub fn is_text(&self) -> bool {
        matches!(self.current(), Some(XmlToken::Text { .. }))
    }

    /// The current text node's value, if positioned on one (`XmlReader.Value`).
    #[must_use]
    pub fn text_value(&self) -> Option<&str> {
        match self.current() {
            Some(XmlToken::Text { value }) => Some(value),
            _ => None,
        }
    }

    /// Whether the current node is a self-closing element
    /// (`XmlReader.IsEmptyElement`).
    #[must_use]
    pub fn is_empty_element(&self) -> bool {
        matches!(self.current(), Some(XmlToken::Empty { .. }))
    }

    /// Advances to the next node (`XmlReader.Read`).
    pub fn read(&mut self) {
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
    }

    /// Advances to the first element node (`XmlReader.MoveToContent`).
    ///
    /// A no-op if already on an element; otherwise skips leading text.
    pub fn move_to_content(&mut self) {
        while let Some(token) = self.current() {
            if token.is_element() {
                break;
            }
            self.pos += 1;
        }
    }

    /// Returns the value of attribute `name` on the current element, if present
    /// (`XmlReader.GetAttribute`).
    #[must_use]
    pub fn get_attribute(&self, name: &str) -> Option<&str> {
        match self.current() {
            Some(XmlToken::Start { attributes, .. } | XmlToken::Empty { attributes, .. }) => {
                attributes
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.as_str())
            }
            _ => None,
        }
    }

    /// Reads the text content of the current element and advances past its end
    /// tag (`XmlReader.ReadElementContentAsString`).
    ///
    /// For an empty element returns `""` and advances one node. For a start
    /// element, concatenates the text descendants until the matching end tag
    /// (ignoring child element markup, as `ReadElementContentAsString` does for
    /// mixed/element content it can flatten). Nested element *content* text is
    /// still included, matching how the parsers only call this on leaf-ish nodes.
    pub fn read_element_content_as_string(&mut self) -> String {
        if let Some(XmlToken::Start { .. }) = self.current() {
            let mut depth = 1;
            let mut out = String::new();
            self.pos += 1;
            while self.pos < self.tokens.len() && depth > 0 {
                match &self.tokens[self.pos] {
                    XmlToken::Start { .. } => depth += 1,
                    XmlToken::Empty { .. } => {}
                    XmlToken::End { .. } => depth -= 1,
                    XmlToken::Text { value } => {
                        if depth == 1 {
                            out.push_str(value);
                        }
                    }
                }
                self.pos += 1;
            }
            out
        } else {
            // Empty element or non-element: consume one node, yield "".
            self.pos += 1;
            String::new()
        }
    }

    /// Reads the raw inner XML of the current element (`XmlReader.ReadInnerXml`),
    /// re-serialized from the token stream.
    ///
    /// Used by the movie `<set>` handler, which re-parses the inner markup.
    /// Attributes on descendant elements are preserved.
    pub fn read_inner_xml(&mut self) -> String {
        if let Some(XmlToken::Start { .. }) = self.current() {
            let mut depth = 1;
            let mut out = String::new();
            self.pos += 1;
            while self.pos < self.tokens.len() && depth > 0 {
                match &self.tokens[self.pos] {
                    XmlToken::Start { name, attributes } => {
                        depth += 1;
                        out.push_str(&serialize_open(name, attributes, false));
                    }
                    XmlToken::Empty { name, attributes } => {
                        out.push_str(&serialize_open(name, attributes, true));
                    }
                    XmlToken::End { name } => {
                        depth -= 1;
                        if depth > 0 {
                            out.push_str("</");
                            out.push_str(name);
                            out.push('>');
                        }
                    }
                    XmlToken::Text { value } => out.push_str(&escape_text(value)),
                }
                self.pos += 1;
            }
            out
        } else {
            // Empty element or non-element: consume one node, yield "".
            self.pos += 1;
            String::new()
        }
    }

    /// Skips the current node's entire subtree (`XmlReader.Skip`).
    pub fn skip(&mut self) {
        match self.current() {
            Some(XmlToken::Start { .. }) => {
                let mut depth = 1;
                self.pos += 1;
                while self.pos < self.tokens.len() && depth > 0 {
                    match &self.tokens[self.pos] {
                        XmlToken::Start { .. } => depth += 1,
                        XmlToken::End { .. } => depth -= 1,
                        _ => {}
                    }
                    self.pos += 1;
                }
            }
            Some(_) => self.pos += 1,
            None => {}
        }
    }

    /// Returns a cursor over the current element's subtree
    /// (`XmlReader.ReadSubtree`) and advances the parent past it.
    ///
    /// The returned [`XmlCursor`] starts positioned on the subtree root element,
    /// mirroring how a fresh subtree reader is consumed with
    /// `MoveToContent(); Read();`.
    #[must_use]
    pub fn read_subtree(&mut self) -> XmlCursor {
        let start = self.pos;
        // Compute the subtree extent, then hand back a copy of that token slice.
        let end = match self.current() {
            Some(XmlToken::Start { .. }) => {
                let mut depth = 1;
                let mut i = self.pos + 1;
                while i < self.tokens.len() && depth > 0 {
                    match &self.tokens[i] {
                        XmlToken::Start { .. } => depth += 1,
                        XmlToken::End { .. } => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                i
            }
            Some(_) => self.pos + 1,
            None => self.pos,
        };
        let sub = self.tokens[start..end].to_vec();
        // Advance the parent past the consumed subtree.
        self.pos = end;
        XmlCursor {
            tokens: sub,
            pos: 0,
        }
    }
}

/// Serializes an opening (or self-closing) tag from a token's parts.
fn serialize_open(name: &str, attributes: &[(String, String)], self_closing: bool) -> String {
    let mut out = String::from("<");
    out.push_str(name);
    for (k, v) in attributes {
        out.push(' ');
        out.push_str(k);
        out.push_str("=\"");
        out.push_str(&escape_attr(v));
        out.push('"');
    }
    if self_closing {
        out.push_str("/>");
    } else {
        out.push('>');
    }
    out
}

/// Escapes text content for re-serialization.
fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escapes an attribute value for re-serialization.
fn escape_attr(text: &str) -> String {
    escape_text(text).replace('"', "&quot;")
}
