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
//! [`READINGS`] in order and keeps the first that `T` accepts.
//!
//! ## The rest of `config/`
//!
//! - **`network.xml` is not imported yet, and that is an open work item, not a
//!   decision.** It carries `KnownProxies`, `EnableRemoteAccess`, `BaseUrl` and
//!   — the reason it matters — `RemoteIPFilter`/`IsRemoteIPFilterBlacklist`,
//!   which are an access-control list. It is blocked on a separate bug:
//!   `ferrofin-networking`'s `NetworkConfiguration` serves `EnableIpv4`,
//!   `EnableIpv6`, `RemoteIpFilter` and `IsRemoteIpFilterBlacklist` where both
//!   the vendored contract and `MediaBrowser.Common/Net/NetworkConfiguration.cs`
//!   say `EnableIPv4`, `EnableIPv6`, `RemoteIPFilter` and
//!   `IsRemoteIPFilterBlacklist`. Importing before that is fixed would drop
//!   exactly the security-relevant fields. Fix the names, then add
//!   `network.xml` here with a deny list for the deployment-specific ports.
//! - **`database.xml` is deliberately skipped**: it selects Jellyfin's database
//!   provider, and Ferrofin is SQLite-only, so there is nothing to carry over.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tracing::{debug, warn};

use ferrofin_traits::error::ServiceError;

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
    "string", "int", "long", "short", "double", "float", "decimal", "boolean", "dateTime", "guid",
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
#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    name: String,
    text: String,
    nil: bool,
    children: Vec<Node>,
}

/// Imports `xml` over `default`, returning the merged configuration.
///
/// `root_name` is the document element the caller expects
/// (`ServerConfiguration`, `EncodingOptions`, …); a document with any other
/// root is rejected rather than imported as a silent no-op.
///
/// Elements named in `deny` are skipped, as are elements Ferrofin's
/// configuration does not have. A field whose value `T` rejects under every
/// [`Reading`] is logged and left at its default.
///
/// # Errors
///
/// Returns [`ServiceError::Backend`] if the XML is malformed, its root element
/// is not `root_name`, or `T` does not serialize to a JSON object.
pub fn import_over<T>(
    default: &T,
    xml: &str,
    root_name: &str,
    deny: &[&str],
) -> Result<T, ServiceError>
where
    T: Serialize + DeserializeOwned,
{
    let root = parse(xml)?;
    if root.name != root_name {
        return Err(ServiceError::Backend(format!(
            "expected a <{root_name}> document, found <{}>",
            root.name
        )));
    }
    let base = serde_json::to_value(default).map_err(|e| backend("serialize", &e))?;
    let Value::Object(mut obj) = base else {
        return Err(ServiceError::Backend(
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
        match serde_json::from_value::<T>(Value::Object(candidate.clone())) {
            // A name that differs from ours only in casing (Jellyfin's
            // `EnableIPv4` against our `EnableIpv4`) deserializes without error
            // and is then silently dropped, so an unfamiliar name has to
            // survive a round trip before it counts as imported.
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
fn parse(xml: &str) -> Result<Node, ServiceError> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;

    loop {
        let event = reader.read_event().map_err(|e| {
            ServiceError::Backend(format!(
                "malformed XML at byte {}: {e}",
                reader.buffer_position()
            ))
        })?;
        match event {
            Event::Start(e) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(ServiceError::Backend(format!(
                        "XML nested deeper than {MAX_DEPTH} levels"
                    )));
                }
                stack.push(new_node(&e));
            }
            Event::Empty(e) => {
                let node = new_node(&e);
                close(&mut stack, &mut root, node);
            }
            Event::Text(e) => {
                if let Some(top) = stack.last_mut() {
                    let text = e
                        .unescape()
                        .map_err(|err| ServiceError::Backend(format!("bad XML text: {err}")))?;
                    top.text.push_str(text.as_ref());
                }
            }
            Event::CData(e) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&String::from_utf8_lossy(e.as_ref()));
                }
            }
            Event::End(_) => {
                let Some(node) = stack.pop() else {
                    return Err(ServiceError::Backend("unbalanced XML end tag".to_owned()));
                };
                close(&mut stack, &mut root, node);
            }
            Event::Eof => break,
            _ => {}
        }
    }

    root.ok_or_else(|| ServiceError::Backend("XML document has no root element".to_owned()))
}

/// Builds an empty node from a start tag, recording `xsi:nil="true"`.
fn new_node(tag: &quick_xml::events::BytesStart<'_>) -> Node {
    let nil = tag.attributes().flatten().any(|a| {
        a.key.local_name().as_ref() == b"nil" && a.value.as_ref().eq_ignore_ascii_case(b"true")
    });
    Node {
        name: String::from_utf8_lossy(tag.local_name().as_ref()).into_owned(),
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

/// Wraps a serde failure as a [`ServiceError`].
fn backend(action: &str, err: &serde_json::Error) -> ServiceError {
    ServiceError::Backend(format!("could not {action} the configuration: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration_manager::default_server_configuration;
    use ferrofin_model::configuration::{EncodingOptions, ServerConfiguration};
    use ferrofin_model::entities::HardwareAccelerationType;

    /// A trimmed `config/system.xml` in the shapes the .NET XML serializer
    /// emits: scalars, `<string>` collections, a collection of objects, a
    /// nested object, and a self-closing empty element.
    const SYSTEM_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<ServerConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <LogFileRetentionDays>7</LogFileRetentionDays>
  <IsStartupWizardCompleted>true</IsStartupWizardCompleted>
  <EnableMetrics>true</EnableMetrics>
  <IsPortAuthorized>true</IsPortAuthorized>
  <CacheSize>800</CacheSize>
  <ServerName>basement</ServerName>
  <MetadataPath>/config/metadata</MetadataPath>
  <SortRemoveWords />
  <SortRemoveCharacters>
    <string>&amp;</string>
  </SortRemoveCharacters>
  <SortReplaceCharacters>
    <string>.</string>
    <string>!</string>
  </SortReplaceCharacters>
  <PluginRepositories>
    <RepositoryInfo>
      <Name>Someone Else's Plugins</Name>
      <Url>https://example.invalid/manifest.json</Url>
      <Enabled>true</Enabled>
    </RepositoryInfo>
  </PluginRepositories>
  <CastReceiverApplications>
    <CastReceiverApplication>
      <Id>F007D354</Id>
      <Name>Stable</Name>
    </CastReceiverApplication>
    <CastReceiverApplication>
      <Id>6F511C87</Id>
      <Name>Unstable</Name>
    </CastReceiverApplication>
  </CastReceiverApplications>
  <TrickplayOptions>
    <EnableHwAcceleration>true</EnableHwAcceleration>
    <Interval>10000</Interval>
  </TrickplayOptions>
</ServerConfiguration>"#;

    fn import_system(xml: &str) -> ServerConfiguration {
        import_over(
            &default_server_configuration(),
            xml,
            "ServerConfiguration",
            SYSTEM_XML_DENY,
        )
        .expect("system.xml imports")
    }

    fn imported_system() -> ServerConfiguration {
        import_system(SYSTEM_XML)
    }

    #[test]
    fn scalars_take_their_type_from_the_ferrofin_default() {
        let cfg = imported_system();
        assert!(cfg.is_startup_wizard_completed, "bool");
        assert!(cfg.enable_metrics);
        assert!(cfg.is_port_authorized);
        assert_eq!(cfg.log_file_retention_days, 7, "integer");
        assert_eq!(cfg.cache_size, 800);
        assert_eq!(cfg.server_name, "basement", "string");
    }

    #[test]
    fn string_collections_replace_the_default_and_are_unescaped() {
        let cfg = imported_system();
        assert_eq!(cfg.sort_remove_characters, ["&"]);
        assert_eq!(cfg.sort_replace_characters, [".", "!"]);
    }

    #[test]
    fn an_empty_element_clears_a_collection_that_has_a_non_empty_default() {
        // `SortRemoveWords` defaults to ["the", "a", "an"], so an operator who
        // emptied it must not have it handed back.
        assert_eq!(default_server_configuration().sort_remove_words.len(), 3);
        assert!(imported_system().sort_remove_words.is_empty());
    }

    #[test]
    fn collections_of_objects_and_nested_objects_are_imported() {
        let cfg = imported_system();
        let apps = &cfg.cast_receiver_applications;
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].id, "F007D354");
        assert_eq!(apps[1].name, "Unstable");
        assert!(cfg.trickplay_options.enable_hw_acceleration);
        assert_eq!(cfg.trickplay_options.interval, 10_000);
    }

    #[test]
    fn a_nested_object_keeps_the_defaults_the_xml_does_not_mention() {
        let default = default_server_configuration();
        let cfg = imported_system();
        assert_eq!(
            cfg.trickplay_options.tile_width,
            default.trickplay_options.tile_width
        );
        assert_eq!(
            cfg.trickplay_options.qscale,
            default.trickplay_options.qscale
        );
    }

    #[test]
    fn denied_fields_are_not_imported() {
        let default = default_server_configuration();
        let cfg = imported_system();
        // A path inside the Jellyfin container, and a repository of .NET
        // assemblies Ferrofin cannot load. The fixture's repository is one the
        // default does NOT carry, so this fails if the deny list stops working.
        assert!(
            !default
                .plugin_repositories
                .iter()
                .any(|r| r.name.as_deref() == Some("Someone Else's Plugins")),
            "the fixture repository must not be one of the defaults"
        );
        assert_eq!(cfg.metadata_path, "");
        assert_eq!(cfg.plugin_repositories, default.plugin_repositories);
    }

    #[test]
    fn a_field_ferrofin_does_not_have_is_reported_and_the_rest_still_imports() {
        // Jellyfin writes `EnableIPv4`; Ferrofin serves `EnableIpv4`. Serde
        // ignores the odd casing silently, so the round-trip check has to catch
        // it — and the fields around it must still import.
        let mut obj = match serde_json::to_value(default_server_configuration()) {
            Ok(Value::Object(m)) => m,
            other => panic!("expected a JSON object, got {other:?}"),
        };
        obj.remove("ServerName");
        let mut unknown = node("EnableIPv4");
        unknown.text = "false".to_owned();
        assert!(
            matches!(
                accept::<ServerConfiguration>(&obj, &unknown),
                Some(Accepted::Unrecognised)
            ),
            "a name ferrofin does not serve must be reported as unrecognised"
        );
        let mut known = node("ServerName");
        known.text = "basement".to_owned();
        assert!(
            matches!(
                accept::<ServerConfiguration>(&obj, &known),
                Some(Accepted::Applied(_))
            ),
            "a name ferrofin does serve must be applied"
        );

        let xml = "<ServerConfiguration><EnableIPv4>false</EnableIPv4>\
                   <ServerName>basement</ServerName></ServerConfiguration>";
        assert_eq!(import_system(xml).server_name, "basement");
    }

    #[test]
    fn a_value_ferrofin_rejects_costs_only_its_own_field() {
        let xml = "<ServerConfiguration><ImageSavingConvention>Nonsense</ImageSavingConvention>\
                   <ServerName>basement</ServerName></ServerConfiguration>";
        let default = default_server_configuration();
        let cfg = import_system(xml);
        assert_eq!(cfg.image_saving_convention, default.image_saving_convention);
        assert_eq!(cfg.server_name, "basement");
    }

    #[test]
    fn an_empty_member_of_a_collection_entry_does_not_lose_the_whole_field() {
        // `PathSubstitutions` defaults to empty, so an entry has no schema at
        // all and `<To />` has to be read from the element alone. It is an
        // empty *string*, not null — `XmlSerializer` writes `<Foo />` for `""`
        // and omits a null member — and reading it as null would take the whole
        // `PathSubstitutions` field down with it, since `To` is a `String`.
        let xml = "<ServerConfiguration><PathSubstitutions><PathSubstitution>\
                   <From>/media</From><To /></PathSubstitution></PathSubstitutions>\
                   </ServerConfiguration>";
        let subs = import_system(xml).path_substitutions;
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].from, "/media");
        assert_eq!(subs[0].to, "");
    }

    #[test]
    fn xsi_nil_is_the_one_thing_that_does_mean_null() {
        // How `XmlSerializer` writes a null *value* type. `CacheSize` is where
        // it shows up in practice: the C# leaves it null on a fresh instance.
        let xml = "<ServerConfiguration xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\
                   <ActivityLogRetentionDays xsi:nil=\"true\" />\
                   <ServerName>basement</ServerName></ServerConfiguration>";
        let cfg = import_system(xml);
        assert_eq!(cfg.activity_log_retention_days, None);
        assert_eq!(
            cfg.server_name, "basement",
            "the fields around it still import"
        );

        // And the parser records it rather than treating it as empty text.
        let root = parse(xml).expect("parses");
        assert!(root.children[0].nil);
        assert!(!root.children[1].nil);
    }

    #[test]
    fn a_nil_element_under_a_name_ferrofin_lacks_is_reported_not_counted() {
        // The null exemption on the round-trip check must not become a way for
        // an unknown name to slip through as "applied" and leave a junk key.
        let obj = match serde_json::to_value(default_server_configuration()) {
            Ok(Value::Object(m)) => m,
            other => panic!("expected a JSON object, got {other:?}"),
        };
        let mut unknown = node("EnableIPv4");
        unknown.nil = true;
        assert!(matches!(
            accept::<ServerConfiguration>(&obj, &unknown),
            Some(Accepted::Unrecognised)
        ));
    }

    #[test]
    fn a_numeric_looking_string_member_stays_a_string() {
        let xml = "<ServerConfiguration><PathSubstitutions><PathSubstitution>\
                   <From>/media</From><To>2</To></PathSubstitution></PathSubstitutions>\
                   </ServerConfiguration>";
        let subs = import_system(xml).path_substitutions;
        assert_eq!(subs.len(), 1, "the AllStrings reading has to rescue this");
        assert_eq!(subs[0].to, "2");
    }

    #[test]
    fn a_collection_entry_does_not_inherit_the_first_default_entry_s_lists() {
        // The default `MetadataOptions` give some types a non-empty
        // `DisabledMetadataFetchers`; an XML entry that omits the element means
        // the operator cleared it, not "copy whatever entry 0 had".
        let default = default_server_configuration();
        assert!(
            default
                .metadata_options
                .iter()
                .any(|o| !o.disabled_metadata_fetchers.is_empty()),
            "the default table must have a non-empty list for this to test anything"
        );
        let xml = "<ServerConfiguration><MetadataOptions>\
                   <MetadataOptions><ItemType>MusicVideo</ItemType></MetadataOptions>\
                   </MetadataOptions></ServerConfiguration>";
        let options = import_system(xml).metadata_options;
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].item_type.as_deref(), Some("MusicVideo"));
        assert!(options[0].disabled_metadata_fetchers.is_empty());
    }

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
    fn the_wrong_document_is_rejected_rather_than_imported_as_a_no_op() {
        let err = import_over(
            &default_server_configuration(),
            "<BrandingOptions><SplashscreenEnabled>true</SplashscreenEnabled></BrandingOptions>",
            "ServerConfiguration",
            SYSTEM_XML_DENY,
        )
        .expect_err("root element mismatch");
        assert!(format!("{err}").contains("BrandingOptions"), "{err}");
    }

    #[test]
    fn malformed_xml_is_an_error_rather_than_a_partial_import() {
        for bad in [
            "not xml at all",
            "<ServerConfiguration><A></B></ServerConfiguration>",
        ] {
            import_over(
                &default_server_configuration(),
                bad,
                "ServerConfiguration",
                SYSTEM_XML_DENY,
            )
            .expect_err(bad);
        }
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
}
