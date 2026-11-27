//! One-way import of a Jellyfin `config/*.xml` document into Ferrofin's JSON
//! configuration.
//!
//! Jellyfin persists its server configuration as XML (`config/system.xml`,
//! `config/encoding.xml`, `config/branding.xml`); Ferrofin persists the same
//! settings as JSON. When Ferrofin adopts an existing Jellyfin data directory
//! the JSON files do not exist yet, and without this import every setting the
//! operator had chosen silently reverts to a Ferrofin default — including
//! `IsStartupWizardCompleted` (which re-opens the anonymous first-time-setup
//! endpoints) and `HardwareAccelerationType` (which turns hardware transcoding
//! off).
//!
//! The import is deliberately **one-way**, and runs per document only while
//! that document's JSON counterpart is absent: Ferrofin never writes back to
//! the XML and never re-imports over a configuration the operator has since
//! changed.
//!
//! Rather than transliterate every field twice, the import walks the XML
//! against the *serialized Ferrofin default*, which doubles as the schema: the
//! default's JSON type at each key decides how that element's text is coerced.
//! Each field is applied independently and validated by a round trip through
//! `T`, so one unrecognised value costs that field and not the whole import.
//!
//! Where the schema runs out — a key Ferrofin's configuration does not have, or
//! an entry of a collection whose default is empty — the value has to be
//! inferred from the element itself, and XML alone cannot say whether
//! `<Foo />` means `""` or `[]`. Instead of guessing once, the import tries the
//! `READINGS` in order and keeps the first that `T` accepts. Within a reading, a
//! member `T` refuses as the number or bool its text was read as (a `ZipCode`
//! of `02134` in a string member) is read as its text, in every entry of its
//! list, and `T` asked again (gl-1965).
//!
//! ## The rest of `config/`
//!
//! - **The named documents** (`encoding.xml`, `branding.xml`, `network.xml`,
//!   `metadata.xml`, `xbmcmetadata.xml` and `livetv.xml`) are seeded as
//!   `named/<key>.json` by `FerrofinServerConfigurationManager::load`, each
//!   with its own deny list. `livetv.xml`'s tuners and listing providers go to
//!   the database instead (`live_tv_import`).
//! - **`database.xml` is deliberately skipped**: it selects Jellyfin's database
//!   provider, and Ferrofin is SQLite-only, so there is nothing to carry over.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tracing::{debug, warn};

/// Why a document could not be imported: malformed XML, another root element
/// than the caller expects, or a configuration that is not a JSON object. The
/// message says which. `ferrofin-traits` turns it into a
/// `ServiceError::Backend` with the same message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigImportError(String);

/// Fields of `system.xml` that must not be carried over.
///
/// Paths are meaningless outside the container Jellyfin ran in, plugin
/// repositories serve .NET assemblies Ferrofin cannot load, and the recorded
/// previous version belongs to Jellyfin's own migration bookkeeping.
pub const SYSTEM_XML_DENY: &[&str] = &[
    "MetadataPath",
    "CachePath",
    "PluginRepositories",
    "PreviousVersion",
    "PreviousVersionStr",
];

/// Fields of `encoding.xml` that must not be carried over: every one of them is
/// a path into the Jellyfin container's filesystem.
pub const ENCODING_XML_DENY: &[&str] = &[
    "TranscodingTempPath",
    "EncoderAppPath",
    "EncoderAppPathDisplay",
    "FallbackFontPath",
];

/// Fields of `branding.xml` that must not be carried over: the splash screen is
/// stored by path, and that path is inside the Jellyfin container.
pub const BRANDING_XML_DENY: &[&str] = &["SplashscreenLocation"];

/// Fields of `network.xml` that must not be carried over: the certificate lives
/// at a path inside the Jellyfin container, and its password is a secret with
/// nothing left to unlock once the path is gone.
///
/// Everything else is the operator's own network policy — `RemoteIPFilter`,
/// `LocalNetworkSubnets`, `KnownProxies`, `EnableRemoteAccess` — and is exactly
/// what adoption must not silently drop.
pub const NETWORK_XML_DENY: &[&str] = &["CertificatePath", "CertificatePassword"];

/// Fields of `livetv.xml` that must not be carried over: all three are
/// recording paths inside the Jellyfin container's filesystem.
pub const LIVETV_XML_DENY: &[&str] =
    &["RecordingPath", "MovieRecordingPath", "SeriesRecordingPath"];

/// Fields of `livetv.xml` left out of the seeded `named/livetv.json`: the
/// recording paths of [`LIVETV_XML_DENY`], and the tuners and listing
/// providers, which Ferrofin keeps in the database (`live_tv_import`) and
/// merges into the document when it is served.
pub const LIVETV_OPTIONS_XML_DENY: &[&str] = &[
    "RecordingPath",
    "MovieRecordingPath",
    "SeriesRecordingPath",
    "TunerHosts",
    "ListingProviders",
];

/// How deep an imported document may nest.
///
/// The real documents are three levels deep. The cap exists because both
/// [`convert`] and `Node`'s derived `Drop` recurse per level, so an absurdly
/// nested file would otherwise overflow the stack instead of being rejected.
const MAX_DEPTH: usize = 64;

/// The element names the .NET XML serializer gives the entries of a collection
/// of primitives (`<KnownProxies><string>…</string></KnownProxies>`).
///
/// A single-entry collection is otherwise indistinguishable from a nested
/// object with one field, so these names break the tie.
const COLLECTION_ITEM_NAMES: &[&str] = &[
    "string",
    "int",
    "long",
    "short",
    "double",
    "float",
    "decimal",
    "boolean",
    "dateTime",
    "guid",
    // `XmlSerializer` names a class-typed list entry after its class, so a
    // one-entry `<PathInfos><MediaPathInfo>…` (a library with a single path —
    // the common case) is indistinguishable from a nested object without
    // these. They are the entry classes of a per-library `options.xml`, and of
    // a guide provider's `ChannelMappings` in `livetv.xml`, whose entry type
    // the empty default provider list cannot supply.
    "MediaPathInfo",
    "TypeOptions",
    "ImageOption",
    "NameValuePair",
];

/// How an element with no Ferrofin default to take its shape from is read.
///
/// A *null* member needs no reading: `XmlSerializer` omits it entirely, or
/// writes `xsi:nil="true"`, which [`Node::nil`] already records. So an empty
/// element always means an empty *value* — the only question is of which type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// Infer a scalar from the text; an empty element is an empty string.
    Inferred,
    /// Infer a scalar from the text; an empty element is an empty collection.
    EmptyAsList,
    /// Every scalar is a string, so a member that merely *looks* numeric or
    /// boolean (`<To>2</To>` of a path substitution) stays text.
    AllStrings,
}

/// Every [`Reading`], in the order they are tried; the first whose result `T`
/// accepts wins.
const READINGS: [Reading; 3] = [Reading::Inferred, Reading::EmptyAsList, Reading::AllStrings];

/// A parsed XML element: local name, trimmed text, child elements, and whether
/// it carried `xsi:nil="true"` (how `XmlSerializer` writes a null value type).
/// Public for the configuration export's tests, which compare documents as
/// these trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The local name, without a namespace prefix.
    pub name: String,
    /// The trimmed text.
    pub text: String,
    /// Whether the element carried `xsi:nil="true"`.
    pub nil: bool,
    /// The child elements, in document order.
    pub children: Vec<Node>,
}

/// Imports `xml` over `default`, returning the merged configuration.
///
/// `root_name` is the document element the caller expects
/// (`ServerConfiguration`, `EncodingOptions`, …); a document with any other
/// root is rejected rather than imported as a silent no-op.
///
/// Elements named in `deny` are skipped, as are elements Ferrofin's
/// configuration does not have. A field whose value `T` rejects under every
/// `Reading` is logged and left at its default.
///
/// # Errors
///
/// Returns a [`ConfigImportError`] if the XML is malformed, its root element
/// is not `root_name`, or `T` does not serialize to a JSON object.
pub fn import_over<T>(
    default: &T,
    xml: &str,
    root_name: &str,
    deny: &[&str],
) -> Result<T, ConfigImportError>
where
    T: Serialize + DeserializeOwned,
{
    let root = parse(xml)?;
    if root.name != root_name {
        return Err(ConfigImportError(format!(
            "expected a <{root_name}> document, found <{}>",
            root.name
        )));
    }
    let base = serde_json::to_value(default).map_err(|e| backend("serialize", &e))?;
    let Value::Object(mut obj) = base else {
        return Err(ConfigImportError(
            "configuration does not serialize to a JSON object".to_owned(),
        ));
    };

    let mut applied = 0_usize;
    let mut ignored: Vec<&str> = Vec::new();
    for child in &root.children {
        if deny.contains(&child.name.as_str()) {
            continue;
        }
        match accept::<T>(&obj, child) {
            Some(Accepted::Applied(next)) => {
                obj = next;
                applied += 1;
            }
            Some(Accepted::Unrecognised) => ignored.push(&child.name),
            None => {}
        }
    }

    if !ignored.is_empty() {
        // Silent setting loss is the failure this whole module exists to stop,
        // so a field Ferrofin has no home for is said out loud, not swallowed.
        warn!(
            document = root_name,
            fields = %ignored.join(", "),
            "jellyfin configuration fields have no ferrofin counterpart and were not imported"
        );
    }
    debug!(
        document = root_name,
        applied, "imported jellyfin configuration fields"
    );
    serde_json::from_value::<T>(Value::Object(obj)).map_err(|e| backend("rebuild", &e))
}

/// What happened to one field.
enum Accepted {
    /// `T` took the value; the merged object carries it.
    Applied(Map<String, Value>),
    /// `T` deserialized but dropped the key — the name is not one of ours.
    Unrecognised,
}

/// Tries every [`Reading`] of `child` against `obj`, returning the first `T`
/// accepts, or `None` when the value is rejected under all of them (logged).
fn accept<T>(obj: &Map<String, Value>, child: &Node) -> Option<Accepted>
where
    T: Serialize + DeserializeOwned,
{
    // A name the serialized default already carries is ours by construction,
    // and needs no further proof. Everything else has to earn it below.
    let known = obj.contains_key(&child.name);
    let schema = obj.get(&child.name).unwrap_or(&Value::Null);
    let mut last_error = None;
    for reading in READINGS {
        let value = convert(schema, child, reading);
        let mut candidate = obj.clone();
        candidate.insert(child.name.clone(), value);
        match from_candidate::<T>(&mut candidate, child) {
            // A name that differs from ours only in casing deserializes
            // without error and is then silently dropped, so an unfamiliar name
            // has to survive a round trip before it counts as imported. This
            // is not hypothetical: `ferrofin-networking` served four network
            // settings as `EnableIpv4`/`RemoteIpFilter` where Jellyfin writes
            // `EnableIPv4`/`RemoteIPFilter`, and nothing noticed until a test
            // compared the served names against the vendored contract.
            //
            // Asking this of a *familiar* name would be wrong, not merely
            // redundant: a field skipped by `skip_serializing_if` — an
            // `Option` the XML set to nil, or one day a collection the
            // operator cleared — legitimately has no key on the way back out,
            // and would be mistaken for a name we do not have.
            Ok(parsed) if !known && !round_trips(&parsed, &child.name) => {
                return Some(Accepted::Unrecognised);
            }
            Ok(_) => return Some(Accepted::Applied(candidate)),
            Err(e) => last_error = Some(e),
        }
    }
    if let Some(e) = last_error {
        warn!(
            field = %child.name,
            error = %e,
            "could not import jellyfin configuration field; keeping ferrofin's default"
        );
    }
    None
}

/// Deserializes `candidate` as `T`. Where `T` rejects a number or a bool a
/// member's text was read as, the member is read as that text instead and `T`
/// asked again (gl-1965): an entry of a list whose default is empty has no
/// schema, so a US `ZipCode` (`02134`) or a station id is guessed a number its
/// string member refuses, and with it the whole list. The text is the
/// element's own, so a leading zero survives, and the member is read as text
/// in every entry of its list at once, so the retries grow with the members a
/// document has, not with its entries. A member its text does not fit either
/// reports the error its number or bool met.
fn from_candidate<T: DeserializeOwned>(
    candidate: &mut Map<String, Value>,
    child: &Node,
) -> Result<T, serde_json::Error> {
    let mut whole = Value::Object(std::mem::take(candidate));
    // The error each member read as text first met, by its path with list
    // indices left out.
    let mut first_errors: std::collections::HashMap<String, serde_json::Error> =
        std::collections::HashMap::new();
    let result = loop {
        match serde_path_to_error::deserialize::<_, T>(&whole) {
            Ok(parsed) => break Ok(parsed),
            Err(err) => {
                let member = member_path(err.path());
                if let Some(first) = first_errors.remove(&member) {
                    break Err(first);
                }
                let Value::Object(members) = &mut whole else {
                    break Err(err.into_inner());
                };
                if !as_text(members, child, err.path()) {
                    break Err(err.into_inner());
                }
                first_errors.insert(member, err.into_inner());
            }
        }
    };
    if let Value::Object(members) = whole {
        *candidate = members;
    }
    result
}

/// `path` with its list indices left out: the member it names in every entry.
fn member_path(path: &serde_path_to_error::Path) -> String {
    use serde_path_to_error::Segment;
    path.iter()
        .map(|segment| match segment {
            Segment::Seq { .. } => "[]".to_owned(),
            Segment::Map { key } => format!(".{key}"),
            Segment::Enum { variant } => format!("::{variant}"),
            Segment::Unknown => "?".to_owned(),
        })
        .collect()
}

/// Reads as its element's text the number or bool at `path` (whose first
/// segment is `child`'s member), and at the same member of every other entry
/// of the lists on the way; `false` when that reads nothing: `path` names a
/// value from the default, an object or a list, a nil element, or a string
/// already.
fn as_text(
    candidate: &mut Map<String, Value>,
    child: &Node,
    path: &serde_path_to_error::Path,
) -> bool {
    use serde_path_to_error::Segment;
    let segments: Vec<&Segment> = path.iter().collect();
    let [Segment::Map { key }, rest @ ..] = segments.as_slice() else {
        return false;
    };
    if *key != child.name {
        return false;
    }
    candidate
        .get_mut(&child.name)
        .is_some_and(|value| as_text_at(value, child, rest))
}

/// [`as_text`] below `value`, read from `node`, along `rest`.
fn as_text_at(value: &mut Value, node: &Node, rest: &[&serde_path_to_error::Segment]) -> bool {
    use serde_path_to_error::Segment;
    let Some((segment, deeper)) = rest.split_first() else {
        if node.nil
            || !node.children.is_empty()
            || !matches!(value, Value::Number(_) | Value::Bool(_))
        {
            return false;
        }
        *value = Value::String(node.text.clone());
        return true;
    };
    match (segment, value) {
        // `convert` maps a list's elements to its entries in order; the
        // member is read as text in each of them.
        (Segment::Seq { .. }, Value::Array(items)) => {
            let mut read = false;
            for (item, element) in items.iter_mut().zip(&node.children) {
                read |= as_text_at(item, element, deeper);
            }
            read
        }
        // An object's members come from the children they are named for, the
        // last of a repeated name winning.
        (Segment::Map { key }, Value::Object(members)) => {
            match (
                members.get_mut(key),
                node.children.iter().rev().find(|c| &c.name == key),
            ) {
                (Some(inner), Some(element)) => as_text_at(inner, element, deeper),
                _ => false,
            }
        }
        _ => false,
    }
}

/// Whether `field` is still present once `parsed` is serialized again.
fn round_trips<T: Serialize>(parsed: &T, field: &str) -> bool {
    matches!(serde_json::to_value(parsed), Ok(Value::Object(m)) if m.contains_key(field))
}

/// Converts one element against the Ferrofin default that occupies its slot.
fn convert(schema: &Value, node: &Node, reading: Reading) -> Value {
    // `xsi:nil="true"` is how `XmlSerializer` writes a null value type, and it
    // outranks whatever the default has in that slot.
    if node.nil {
        return Value::Null;
    }
    match schema {
        Value::Array(items) => {
            // The default's first entry supplies the entry *types*, and fills
            // the gaps for members the entry omits. `XmlSerializer` writes
            // every public member of an entry, so a real document never leaves
            // one out; a hand-edited one that does inherits that member from
            // the first default entry, which is the best a schema-driven walk
            // manages without a serde default to fall back on.
            let element = items.first().map_or(Value::Null, blank_collections);
            Value::Array(
                node.children
                    .iter()
                    .map(|c| convert(&element, c, reading))
                    .collect(),
            )
        }
        Value::Object(fields) => {
            let mut out = fields.clone();
            for child in &node.children {
                let sub = convert(
                    fields.get(&child.name).unwrap_or(&Value::Null),
                    child,
                    reading,
                );
                out.insert(child.name.clone(), sub);
            }
            Value::Object(out)
        }
        Value::Bool(b) => parse_bool(&node.text).map_or_else(|| Value::Bool(*b), Value::Bool),
        Value::Number(n) => parse_number(&node.text).unwrap_or_else(|| Value::Number(n.clone())),
        Value::String(_) => Value::String(node.text.clone()),
        // No default to take the shape from — either the field is `None` on the
        // Ferrofin default, or it sits inside a collection whose default is
        // empty. Infer it from the element itself.
        Value::Null => guess(node, reading),
    }
}

/// A copy of `schema` with every collection emptied and scalars left alone.
///
/// Used as the *shape* of a collection entry. Emptying the collections stops an
/// entry inheriting another entry's lists — the visible half of the problem,
/// since those are the members that differ per entry in the real
/// `MetadataOptions` table. Scalars stay: they are what makes an omitted
/// non-optional member (an enum, most of all) still deserialize, and
/// `XmlSerializer` never omits one anyway.
fn blank_collections(schema: &Value) -> Value {
    match schema {
        Value::Array(_) => Value::Array(Vec::new()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), blank_collections(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Infers a JSON value for an element with no corresponding Ferrofin default.
fn guess(node: &Node, reading: Reading) -> Value {
    if node.nil {
        return Value::Null;
    }
    if let Some(first) = node.children.first() {
        let uniform = node.children.iter().all(|c| c.name == first.name);
        // `<Foo><string>a</string><string>b</string></Foo>` is a collection; an
        // element with differently named children is a nested object. A single
        // child is ambiguous, so the .NET entry names break the tie — and a
        // wrong call here is still caught by the next `Reading`.
        let is_collection = uniform
            && (node.children.len() > 1 || COLLECTION_ITEM_NAMES.contains(&first.name.as_str()));
        return if is_collection {
            Value::Array(node.children.iter().map(|c| guess(c, reading)).collect())
        } else {
            Value::Object(
                node.children
                    .iter()
                    .map(|c| (c.name.clone(), guess(c, reading)))
                    .collect::<Map<_, _>>(),
            )
        };
    }
    if node.text.is_empty() {
        // Never `null`: `XmlSerializer` writes `<Foo />` for an empty string or
        // an empty collection and omits a null member altogether, so the only
        // open question here is which of the two empties it is.
        return match reading {
            Reading::EmptyAsList => Value::Array(Vec::new()),
            Reading::Inferred | Reading::AllStrings => Value::String(String::new()),
        };
    }
    if reading == Reading::AllStrings {
        return Value::String(node.text.clone());
    }
    parse_bool(&node.text)
        .map(Value::Bool)
        .or_else(|| parse_number(&node.text))
        .unwrap_or_else(|| Value::String(node.text.clone()))
}

/// Parses the `true`/`false` the .NET XML serializer writes.
fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Parses an XML numeric literal, preferring an integer representation.
fn parse_number(text: &str) -> Option<Value> {
    if let Ok(i) = text.parse::<i64>() {
        return Some(Value::Number(i.into()));
    }
    text.parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
}

/// Parses an XML document into its root [`Node`].
///
/// # Errors
///
/// Returns [`ConfigImportError`] if the XML is malformed, nests deeper than
/// the import allows, or has no root element.
pub fn parse(xml: &str) -> Result<Node, ConfigImportError> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut text = String::new();

    loop {
        let event = reader.read_event().map_err(|e| {
            ConfigImportError(format!(
                "malformed XML at byte {}: {e}",
                reader.buffer_position()
            ))
        })?;
        // Entity references are separate events. Trim only after joining a
        // complete text run, retaining spaces on either side of an entity.
        if !matches!(&event, Event::Text(_) | Event::GeneralRef(_)) {
            if let Some(top) = stack.last_mut() {
                top.text.push_str(text.trim());
            }
            text.clear();
        }
        match event {
            Event::Start(e) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(ConfigImportError(format!(
                        "XML nested deeper than {MAX_DEPTH} levels"
                    )));
                }
                stack.push(new_node(&e));
            }
            Event::Empty(e) => {
                let node = new_node(&e);
                close(&mut stack, &mut root, node);
            }
            Event::Text(e) => text.push_str(e.as_ref()),
            Event::GeneralRef(e) => {
                if !stack.is_empty() {
                    let reference = format!("&{};", e.as_ref());
                    let value = quick_xml::escape::unescape(&reference)
                        .map_err(|err| ConfigImportError(format!("bad XML text: {err}")))?;
                    text.push_str(&value);
                }
            }
            Event::CData(e) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(e.as_ref());
                }
            }
            Event::End(_) => {
                let Some(node) = stack.pop() else {
                    return Err(ConfigImportError("unbalanced XML end tag".to_owned()));
                };
                close(&mut stack, &mut root, node);
            }
            Event::Eof => break,
            _ => {}
        }
    }

    root.ok_or_else(|| ConfigImportError("XML document has no root element".to_owned()))
}

/// Builds an empty node from a start tag, recording `xsi:nil="true"`.
fn new_node(tag: &quick_xml::events::BytesStart<'_>) -> Node {
    let nil = tag.attributes().flatten().any(|a| {
        a.key.local_name().as_ref() == "nil" && a.value.as_ref().eq_ignore_ascii_case("true")
    });
    Node {
        name: tag.local_name().as_ref().to_owned(),
        text: String::new(),
        nil,
        children: Vec::new(),
    }
}

/// Attaches a finished node to its parent, or records it as the root.
fn close(stack: &mut [Node], root: &mut Option<Node>, node: Node) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else if root.is_none() {
        *root = Some(node);
    }
}

/// Wraps a serde failure as a [`ConfigImportError`].
fn backend(action: &str, err: &serde_json::Error) -> ConfigImportError {
    ConfigImportError(format!("could not {action} the configuration: {err}"))
}

#[cfg(test)]
mod tests {
    /// The seeded `named/livetv.json` leaves out at least what the tuner
    /// import does: a recording path added to one list belongs in both.
    #[test]
    fn the_live_tv_options_seed_denies_every_recording_path() {
        for field in super::LIVETV_XML_DENY {
            assert!(
                super::LIVETV_OPTIONS_XML_DENY.contains(field),
                "{field} is missing from LIVETV_OPTIONS_XML_DENY"
            );
        }
    }

    #[test]
    fn entity_text_is_joined_before_trimming() {
        let node = super::parse("<Name>  Fish &amp; Chips &#33;  </Name>").expect("valid XML");
        assert_eq!(node.text, "Fish & Chips !");
        let node = super::parse("<Name><![CDATA[  keep spaces  ]]></Name>").expect("CDATA");
        assert_eq!(node.text, "  keep spaces  ");
        assert!(super::parse("<Name>&missing;</Name>").is_err());
    }

    /// Entries of a list whose default is empty have no schema, so their
    /// members are guessed from their text. A member `T` types as a string
    /// keeps its text when the guess made it a number or a bool, the leading
    /// zero of `02134` included, beside members that are numbers and bools
    /// (gl-1965).
    #[test]
    fn a_guessed_scalar_a_string_member_rejects_is_read_as_its_text() {
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Entry {
            enabled: bool,
            zip: Option<String>,
            count: i32,
            label: String,
            pairs: Vec<Pair>,
        }
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Pair {
            name: Option<String>,
            value: Option<String>,
        }
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Doc {
            entries: Vec<Entry>,
        }
        let xml = "<Doc><Entries>\
                   <Entry><Enabled>true</Enabled><Zip>02134</Zip><Count>3</Count>\
                   <Label>false</Label><Pairs><Pair><Name>1</Name><Value>2.5</Value></Pair>\
                   <Pair><Name>a</Name><Value>b</Value></Pair></Pairs></Entry>\
                   <Entry><Enabled>false</Enabled><Zip>90210</Zip><Count>0</Count></Entry>\
                   </Entries></Doc>";
        let doc: Doc = super::import_over(&Doc::default(), xml, "Doc", &[]).expect("imports");
        assert_eq!(doc.entries.len(), 2, "{doc:?}");
        let first = &doc.entries[0];
        assert!(first.enabled);
        assert_eq!(first.zip.as_deref(), Some("02134"));
        assert_eq!(first.count, 3);
        assert_eq!(first.label, "false");
        assert_eq!(first.pairs[0].name.as_deref(), Some("1"));
        assert_eq!(first.pairs[0].value.as_deref(), Some("2.5"));
        assert_eq!(doc.entries[1].zip.as_deref(), Some("90210"));
        assert!(!doc.entries[1].enabled);
    }

    /// A number read as text and still refused (an `i32` member given `1.5`)
    /// is rejected with the error the number met, and the field keeps its
    /// default.
    #[test]
    fn a_member_its_text_does_not_fit_either_is_rejected() {
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Entry {
            count: i32,
        }
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Doc {
            entries: Vec<Entry>,
        }
        let xml = "<Doc><Entries><Entry><Count>1.5</Count></Entry>\
                   <Entry><Count>1</Count></Entry></Entries></Doc>";
        let doc: Doc = super::import_over(&Doc::default(), xml, "Doc", &[]).expect("imports");
        assert_eq!(doc, Doc::default(), "the field keeps its default");
        let mut candidate = serde_json::Map::new();
        let node = super::parse(xml).expect("xml");
        let entries = &node.children[0];
        candidate.insert(
            "Entries".to_owned(),
            super::convert(&serde_json::Value::Null, entries, super::Reading::Inferred),
        );
        let err = super::from_candidate::<Doc>(&mut candidate, entries).expect_err("refused");
        assert!(err.to_string().contains("floating point"), "{err}");
    }

    /// Text read into a list of strings, a repeated element (the last wins)
    /// and a nil one: each keeps what `convert` gives it.
    #[test]
    fn a_number_in_a_string_list_and_a_repeated_member_read_as_text() {
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Entry {
            tags: Vec<String>,
            zip: Option<String>,
            note: Option<String>,
            kind: String,
        }
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Doc {
            entries: Vec<Entry>,
        }
        let xml = "<Doc xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"><Entries><Entry>\
                   <Tags><string>1080</string><string>hd</string></Tags>\
                   <Kind>a</Kind><Zip>1</Zip><Zip>02</Zip><Note xsi:nil=\"true\" />\
                   </Entry></Entries></Doc>";
        let doc: Doc = super::import_over(&Doc::default(), xml, "Doc", &[]).expect("imports");
        let [entry] = doc.entries.as_slice() else {
            panic!("expected one entry, got {doc:?}");
        };
        assert_eq!(entry.tags, ["1080", "hd"]);
        assert_eq!(
            entry.zip.as_deref(),
            Some("02"),
            "the last of a repeated name"
        );
        assert_eq!(entry.note, None);
        assert_eq!(entry.kind, "a");
    }

    /// A thousand entries with numeric-looking members import, each member
    /// read as text in every entry at once (about 30 ms in a debug build).
    #[test]
    fn a_thousand_numeric_mappings_import() {
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Pair {
            name: Option<String>,
            value: Option<String>,
        }
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Entry {
            enabled: bool,
            mappings: Vec<Pair>,
        }
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Doc {
            entries: Vec<Entry>,
        }
        let mut pairs = String::new();
        for i in 0..1000 {
            use std::fmt::Write as _;
            let _ = write!(
                pairs,
                "<Pair><Name>{i}.1</Name><Value>{}</Value></Pair>",
                20000 + i
            );
        }
        let xml = format!(
            "<Doc><Entries><Entry><Enabled>true</Enabled><Mappings>{pairs}</Mappings>\
             </Entry></Entries></Doc>"
        );
        let doc: Doc = super::import_over(&Doc::default(), &xml, "Doc", &[]).expect("imports");
        let mappings = &doc.entries[0].mappings;
        assert_eq!(mappings.len(), 1000);
        assert_eq!(mappings[999].name.as_deref(), Some("999.1"));
        assert_eq!(mappings[999].value.as_deref(), Some("20999"));
    }

    /// A value no reading of its text fits stays rejected: only a guessed
    /// number or bool where a string belongs is read as its text.
    #[test]
    fn a_member_no_text_fits_is_still_rejected() {
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Entry {
            count: i32,
        }
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Doc {
            entries: Vec<Entry>,
        }
        let xml = "<Doc><Entries><Entry><Count>many</Count></Entry>\
                   <Entry><Count>1</Count></Entry></Entries></Doc>";
        let doc: Doc = super::import_over(&Doc::default(), xml, "Doc", &[]).expect("imports");
        assert_eq!(doc, Doc::default(), "the field keeps its default");
    }

    #[test]
    fn single_class_typed_entry_is_read_as_a_list() {
        // A library with one path: `<PathInfos>` has exactly one
        // `<MediaPathInfo>` child, which must come back as a one-element list,
        // not an object keyed by the entry's class name.
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Opts {
            path_infos: Vec<ferrofin_model::configuration::MediaPathInfo>,
        }
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Opts xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <PathInfos>
    <MediaPathInfo>
      <Path>/srv/media/shows</Path>
    </MediaPathInfo>
  </PathInfos>
</Opts>"#;
        let opts = super::import_over(&Opts::default(), xml, "Opts", &[]).expect("import");
        assert_eq!(opts.path_infos.len(), 1);
        assert_eq!(opts.path_infos[0].path, "/srv/media/shows");
    }

    #[test]
    fn nested_objects_and_lists_of_objects_take_their_shape_from_the_default() {
        #[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Inner {
            enabled: bool,
            names: Vec<String>,
        }
        impl Default for Inner {
            fn default() -> Self {
                Self {
                    enabled: false,
                    names: vec!["a".to_owned()],
                }
            }
        }
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(rename_all = "PascalCase", default)]
        struct Outer {
            inner: Inner,
            items: Vec<Inner>,
        }
        let xml = "<Outer><Inner><Enabled>true</Enabled></Inner>\
                   <Items><Inner><Enabled>true</Enabled>\
                   <Names><string>x</string></Names></Inner></Items></Outer>";
        let default = Outer {
            items: vec![Inner::default()],
            ..Outer::default()
        };
        let outer = super::import_over(&default, xml, "Outer", &[]).expect("import");
        assert!(outer.inner.enabled);
        assert_eq!(
            outer.inner.names,
            ["a"],
            "a nested object keeps the defaults the XML does not mention"
        );
        assert_eq!(outer.items.len(), 1);
        assert!(outer.items[0].enabled);
        assert_eq!(outer.items[0].names, ["x"]);
    }

    use super::*;
    use ferrofin_model::configuration::EncodingOptions;
    use ferrofin_model::entities::HardwareAccelerationType;

    #[test]
    fn hardware_acceleration_survives_adoption() {
        // The setting this whole import exists for: adopting a Jellyfin install
        // used to silently turn hardware transcoding off.
        let xml = "<EncodingOptions><HardwareAccelerationType>nvenc</HardwareAccelerationType>\
                   <TranscodingTempPath>/config/transcodes</TranscodingTempPath>\
                   <EncodingThreadCount>8</EncodingThreadCount>\
                   <H265Crf>26</H265Crf><QsvDevice />\
                   <HardwareDecodingCodecs><string>h264</string><string>av1</string>\
                   </HardwareDecodingCodecs>\
                   <AllowOnDemandMetadataBasedKeyframeExtractionForExtensions>\
                   <string>mkv</string>\
                   </AllowOnDemandMetadataBasedKeyframeExtractionForExtensions>\
                   </EncodingOptions>";
        let options = import_over(
            &EncodingOptions::default(),
            xml,
            "EncodingOptions",
            ENCODING_XML_DENY,
        )
        .expect("encoding.xml imports");
        assert_eq!(
            options.hardware_acceleration_type,
            HardwareAccelerationType::nvenc
        );
        assert_eq!(options.encoding_thread_count, 8);
        assert_eq!(options.h265_crf, 26);
        assert_eq!(options.hardware_decoding_codecs, ["h264", "av1"]);
        // A single-entry `<string>` collection is a collection, not an object.
        assert_eq!(
            options.allow_on_demand_metadata_based_keyframe_extraction_for_extensions,
            ["mkv"]
        );
        // Denied: the transcode directory belongs to the Jellyfin container.
        assert_eq!(
            options.transcoding_temp_path,
            EncodingOptions::default().transcoding_temp_path
        );
    }

    #[test]
    fn absurdly_nested_xml_is_rejected_instead_of_overflowing_the_stack() {
        let deep = "<ServerConfiguration>".to_owned() + &"<A>".repeat(MAX_DEPTH + 1);
        let err = parse(&deep).expect_err("depth cap");
        assert!(format!("{err}").contains("nested deeper"), "{err}");
    }

    #[test]
    fn cdata_becomes_element_text() {
        let root =
            parse("<BrandingOptions><CustomCss><![CDATA[a > b]]></CustomCss></BrandingOptions>")
                .expect("parses");
        assert_eq!(root.children[0].text, "a > b");
    }

    /// A bare node, as the parser would build it for `<name />`.
    fn node(name: &str) -> Node {
        Node {
            name: name.to_owned(),
            text: String::new(),
            nil: false,
            children: Vec::new(),
        }
    }

    #[test]
    fn guessing_reads_the_element_when_ferrofin_has_no_default() {
        let mut node = node("X");
        // Never null: `XmlSerializer` omits a null member rather than writing
        // an empty element for it.
        assert_eq!(
            guess(&node, Reading::Inferred),
            Value::String(String::new())
        );
        assert_eq!(guess(&node, Reading::EmptyAsList), Value::Array(Vec::new()));
        node.text = "true".to_owned();
        assert_eq!(guess(&node, Reading::Inferred), Value::Bool(true));
        assert_eq!(
            guess(&node, Reading::AllStrings),
            Value::String("true".to_owned())
        );
        node.text = "42".to_owned();
        assert_eq!(guess(&node, Reading::Inferred), Value::Number(42.into()));
        node.text = "1.5".to_owned();
        assert_eq!(guess(&node, Reading::Inferred), serde_json::json!(1.5));
        node.text = "/dev/dri/renderD128".to_owned();
        assert_eq!(
            guess(&node, Reading::Inferred),
            Value::String("/dev/dri/renderD128".to_owned())
        );
    }

    #[test]
    fn a_lone_primitive_entry_is_a_collection_and_a_lone_field_is_an_object() {
        let list = parse("<R><Foo><string>a</string></Foo></R>").expect("parses");
        assert_eq!(
            guess(&list.children[0], Reading::Inferred),
            serde_json::json!(["a"])
        );
        let object = parse("<R><Foo><Bar>a</Bar></Foo></R>").expect("parses");
        assert_eq!(
            guess(&object.children[0], Reading::Inferred),
            serde_json::json!({ "Bar": "a" })
        );
    }

    /// `EncodingOptions` as a JSON object: the schema the per-field checks
    /// below run against.
    fn encoding_schema() -> Map<String, Value> {
        match serde_json::to_value(EncodingOptions::default()) {
            Ok(Value::Object(m)) => m,
            other => panic!("expected a JSON object, got {other:?}"),
        }
    }

    #[test]
    fn a_field_ferrofin_does_not_have_is_reported_and_one_it_has_is_applied() {
        // The casing class, using the shape that actually bit us elsewhere:
        // Jellyfin writes `EnableIPv4`, a `PascalCase` derive gives
        // `EnableIpv4`. Serde ignores the mismatch silently, so the round-trip
        // check has to catch it.
        let mut obj = encoding_schema();
        obj.remove("TranscodingTempPath");
        let mut unknown = node("EnableIPv4");
        unknown.text = "false".to_owned();
        assert!(
            matches!(
                accept::<EncodingOptions>(&obj, &unknown),
                Some(Accepted::Unrecognised)
            ),
            "a name ferrofin does not serve must be reported as unrecognised"
        );
        let mut known = node("TranscodingTempPath");
        known.text = "/config/transcodes".to_owned();
        assert!(
            matches!(
                accept::<EncodingOptions>(&obj, &known),
                Some(Accepted::Applied(_))
            ),
            "a name ferrofin does serve must be applied"
        );
    }

    #[test]
    fn the_parser_records_xsi_nil_rather_than_empty_text() {
        let root = parse(
            "<ServerConfiguration xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\
             <ActivityLogRetentionDays xsi:nil=\"true\" />\
             <ServerName>basement</ServerName></ServerConfiguration>",
        )
        .expect("parses");
        assert!(root.children[0].nil);
        assert!(!root.children[1].nil);
    }

    #[test]
    fn a_nil_element_under_a_name_ferrofin_lacks_is_reported_not_counted() {
        // The null exemption on the round-trip check must not become a way for
        // an unknown name to slip through as "applied" and leave a junk key.
        let mut unknown = node("EnableIPv4");
        unknown.nil = true;
        assert!(matches!(
            accept::<EncodingOptions>(&encoding_schema(), &unknown),
            Some(Accepted::Unrecognised)
        ));
    }

    #[test]
    fn the_wrong_document_is_rejected_rather_than_imported_as_a_no_op() {
        let err = import_over(
            &EncodingOptions::default(),
            "<BrandingOptions><SplashscreenEnabled>true</SplashscreenEnabled></BrandingOptions>",
            "EncodingOptions",
            ENCODING_XML_DENY,
        )
        .expect_err("root element mismatch");
        assert!(format!("{err}").contains("BrandingOptions"), "{err}");
    }

    #[test]
    fn malformed_xml_is_an_error_rather_than_a_partial_import() {
        for bad in [
            "not xml at all",
            "<EncodingOptions><A></B></EncodingOptions>",
        ] {
            import_over(
                &EncodingOptions::default(),
                bad,
                "EncodingOptions",
                ENCODING_XML_DENY,
            )
            .expect_err(bad);
        }
    }
}
