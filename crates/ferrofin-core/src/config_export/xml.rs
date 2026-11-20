//! A JSON document written as the XML .NET's `XmlSerializer` reads.
//!
//! The conventions come from the documents Jellyfin 12.4 writes itself (the
//! `testdata/` files, from `jellyfin/jellyfin:12.4`): the `xsi`/`xsd` declarations on the root,
//! two-space indentation, `<Name />` for an empty value, `xsi:nil="true"` for
//! a null one, and each collection entry named after its type (`<string>`,
//! `<int>`, `<MetadataOptions>`).

use std::fmt::Write as _;

use serde_json::{Map, Number, Value};

use ferrofin_traits::error::ServiceError;

/// The schema-instance namespace `xsi:nil` belongs to.
const XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";

/// The schema namespace `XmlSerializer` declares beside it.
const XSD: &str = "http://www.w3.org/2001/XMLSchema";

/// Writes `fields` as the document `<root>`.
///
/// `entries` names the element of each entry of a collection of objects,
/// keyed by the collection's member name. Entries of primitives are named
/// after their type, as `XmlSerializer` names them.
///
/// shortcut: members are written in key order, not the C# declaration order
/// Jellyfin writes. `XmlSerializer` reads a class's members in any order;
/// write them in declaration order if a reader ever depends on it.
///
/// # Errors
///
/// Returns [`ServiceError::Backend`] for a collection of objects `entries`
/// does not name, or a string holding a character XML 1.0 cannot carry.
pub(super) fn document(
    root: &str,
    fields: &Map<String, Value>,
    entries: &[(&str, &str)],
) -> Result<String, ServiceError> {
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<{root} xmlns:xsi=\"{XSI}\" xmlns:xsd=\"{XSD}\">\n"
    );
    for (name, value) in fields {
        element(&mut out, 1, name, value, entries)?;
    }
    // Jellyfin's own files end at the closing tag, with no final newline.
    let _ = write!(out, "</{root}>");
    Ok(out)
}

/// Appends one element and its children at `depth`.
fn element(
    out: &mut String,
    depth: usize,
    name: &str,
    value: &Value,
    entries: &[(&str, &str)],
) -> Result<(), ServiceError> {
    let pad = "  ".repeat(depth);
    match value {
        Value::Null => {
            let _ = writeln!(out, "{pad}<{name} xsi:nil=\"true\" />");
        }
        Value::Bool(b) => {
            let _ = writeln!(out, "{pad}<{name}>{b}</{name}>");
        }
        Value::Number(n) => {
            let _ = writeln!(out, "{pad}<{name}>{}</{name}>", number(n));
        }
        Value::String(s) if s.is_empty() => {
            let _ = writeln!(out, "{pad}<{name} />");
        }
        Value::String(s) => {
            if let Some(c) = s.chars().find(|&c| !is_xml_char(c)) {
                return Err(ServiceError::Backend(format!(
                    "setting {name} holds U+{:04X}, which an XML document cannot carry",
                    u32::from(c)
                )));
            }
            let _ = writeln!(
                out,
                "{pad}<{name}>{}</{name}>",
                quick_xml::escape::partial_escape(s.as_str())
            );
        }
        Value::Array(items) if items.is_empty() => {
            let _ = writeln!(out, "{pad}<{name} />");
        }
        Value::Object(fields) if fields.is_empty() => {
            let _ = writeln!(out, "{pad}<{name} />");
        }
        Value::Array(items) => {
            let _ = writeln!(out, "{pad}<{name}>");
            for item in items {
                element(
                    out,
                    depth + 1,
                    entry_name(name, item, entries)?,
                    item,
                    entries,
                )?;
            }
            let _ = writeln!(out, "{pad}</{name}>");
        }
        Value::Object(fields) => {
            let _ = writeln!(out, "{pad}<{name}>");
            for (child, item) in fields {
                element(out, depth + 1, child, item, entries)?;
            }
            let _ = writeln!(out, "{pad}</{name}>");
        }
    }
    Ok(())
}

/// The element `XmlSerializer` names an entry of the collection `field`.
fn entry_name<'a>(
    field: &str,
    item: &Value,
    entries: &[(&str, &'a str)],
) -> Result<&'a str, ServiceError> {
    match item {
        // A null entry only occurs in a string collection here; it is written
        // `<string xsi:nil="true" />`, as `XmlSerializer` writes one.
        Value::String(_) | Value::Null => Ok("string"),
        Value::Bool(_) => Ok("boolean"),
        Value::Number(n) if n.is_f64() => Ok("double"),
        Value::Number(_) => Ok("int"),
        Value::Object(_) | Value::Array(_) => entries
            .iter()
            .find(|(member, _)| *member == field)
            .map(|(_, entry)| *entry)
            .ok_or_else(|| {
                ServiceError::Backend(format!(
                    "no XML entry name is known for the entries of {field}"
                ))
            }),
    }
}

/// A number as `XmlConvert.ToString` writes it: a whole `double` has no
/// fractional part (`2`, not `2.0`).
fn number(n: &Number) -> String {
    match n.as_f64() {
        // Below 2^53 an `f64` holds every whole number exactly.
        Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 => {
            format!("{f:.0}")
        }
        _ => n.to_string(),
    }
}

/// Whether XML 1.0 can carry `c` (its `Char` production).
fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | ' '..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn write(fields: &Value, entries: &[(&str, &str)]) -> Result<String, ServiceError> {
        document("Root", fields.as_object().expect("an object"), entries)
    }

    /// Every value kind takes the form Jellyfin's own documents give it.
    #[test]
    fn values_are_written_as_xml_serializer_writes_them() {
        let xml = write(
            &json!({
                "Flag": true,
                "Count": 3,
                "Boost": 2.0,
                "Ratio": 0.75,
                "Empty": "",
                "Missing": null,
                "Names": ["a", "b"],
                "None": [],
                "Widths": [320, 640],
                "Nested": {"Inner": "x"},
                "Pairs": [{"Name": "n", "Value": "v"}],
            }),
            &[("Pairs", "NameValuePair")],
        )
        .expect("writes");
        assert_eq!(
            xml,
            r#"<?xml version="1.0" encoding="utf-8"?>
<Root xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <Boost>2</Boost>
  <Count>3</Count>
  <Empty />
  <Flag>true</Flag>
  <Missing xsi:nil="true" />
  <Names>
    <string>a</string>
    <string>b</string>
  </Names>
  <Nested>
    <Inner>x</Inner>
  </Nested>
  <None />
  <Pairs>
    <NameValuePair>
      <Name>n</Name>
      <Value>v</Value>
    </NameValuePair>
  </Pairs>
  <Ratio>0.75</Ratio>
  <Widths>
    <int>320</int>
    <int>640</int>
  </Widths>
</Root>"#
        );
    }

    /// Markup in a value is escaped, so a disclaimer with `<b>` stays text.
    #[test]
    fn markup_in_a_value_is_escaped() {
        let xml = write(&json!({"LoginDisclaimer": "<b>Tom & Jerry</b>"}), &[]).expect("writes");
        assert!(
            xml.contains("<LoginDisclaimer>&lt;b&gt;Tom &amp; Jerry&lt;/b&gt;</LoginDisclaimer>"),
            "{xml}"
        );
    }

    /// A control character would make the whole document unreadable, and
    /// Jellyfin would load its defaults for every setting in it, so the
    /// export refuses it, naming the setting.
    #[test]
    fn a_character_xml_cannot_carry_is_refused() {
        let error = write(&json!({"ServerName": "bad\u{1}name"}), &[]).expect_err("refused");
        assert!(error.to_string().contains("ServerName"), "{error}");
        assert!(error.to_string().contains("U+0001"), "{error}");
    }

    /// A collection of objects whose entry type is not known is refused, not
    /// written under a guessed name Jellyfin would skip.
    #[test]
    fn an_unnamed_collection_of_objects_is_refused() {
        let error = write(&json!({"Things": [{"A": 1}]}), &[]).expect_err("refused");
        assert!(error.to_string().contains("Things"), "{error}");
    }

    /// Entries of the other primitive kinds take their .NET type names.
    #[test]
    fn primitive_entries_take_their_type_names() {
        let xml = write(
            &json!({"Flags": [true], "Ratios": [0.5], "Gaps": [null]}),
            &[],
        )
        .expect("writes");
        assert!(xml.contains("<boolean>true</boolean>"), "{xml}");
        assert!(xml.contains("<double>0.5</double>"), "{xml}");
        assert!(xml.contains("<string xsi:nil=\"true\" />"), "{xml}");
    }

    /// An empty object is written as an empty element.
    #[test]
    fn an_empty_object_is_an_empty_element() {
        let xml = write(&json!({"Options": {}}), &[]).expect("writes");
        assert!(xml.contains("  <Options />\n"), "{xml}");
    }
}
