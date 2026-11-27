//! A plugin's settings as the XML its Jellyfin counterpart reads (gl-1556,
//! D7): `{program data}/plugins/configurations/<file>`, its
//! `PluginConfiguration` as `XmlSerializer` writes it
//! (`BasePlugin<T>.ConfigurationFilePath`). Which plugins have a counterpart,
//! and under which file name, is the composition root's to say: it knows the
//! compiled-in extensions.

use serde_json::{Map, Value};

use ferrofin_traits::error::ServiceError;

use super::{ConfigExport, ExportedFile, NotExported, xml};

/// Where Jellyfin keeps plugin settings, relative to its program data
/// directory.
pub const PLUGIN_CONFIGURATIONS: &str = "plugins/configurations";

/// How one plugin's settings are written for its Jellyfin counterpart.
#[derive(Debug, Clone, Copy)]
pub struct PluginDocument {
    /// The file the counterpart reads (`LDAP-Auth.xml`).
    pub file: &'static str,
    /// The members of its `PluginConfiguration` that `XmlSerializer` writes,
    /// in the class's declaration order. These are the settings written, in
    /// this order; a member Ferrofin does not keep is reported.
    pub members: &'static [&'static str],
    /// The element `XmlSerializer` names each entry of a collection of
    /// objects, keyed by the collection.
    pub entries: &'static [(&'static str, &'static str)],
    /// Members the counterpart works out for itself (`[XmlIgnore]`, a flag it
    /// sets when it signs in): neither written nor reported.
    pub derived: &'static [&'static str],
    /// The enum members, each with its names in declaration order:
    /// `XmlSerializer` reads only an exact name, and refuses the whole
    /// document for anything else.
    pub enums: &'static [(&'static str, &'static [&'static str])],
}

/// Writes `settings`, the plugin's settings as Ferrofin serves them, as the
/// file of `document`, into `export`. A setting the counterpart lacks is
/// reported when it is not at its value in `defaults`, and a member Ferrofin
/// does not keep always is; neither ever by value. A null is left out, as
/// `XmlSerializer` leaves out a null reference. The file can hold a password
/// or a key, so it is private.
///
/// # Errors
///
/// Returns [`ServiceError::Backend`] for a value XML cannot carry, or a
/// collection of objects `document` does not name.
pub fn export_plugin(
    document: &PluginDocument,
    settings: &Map<String, Value>,
    defaults: &Map<String, Value>,
    export: &mut ConfigExport,
) -> Result<(), ServiceError> {
    let path = format!("{PLUGIN_CONFIGURATIONS}/{}", document.file);
    let mut fields = Map::new();
    for (name, value) in settings {
        if document.members.contains(&name.as_str()) {
            let written = match document.enums.iter().find(|(member, _)| member == name) {
                Some((_, names)) => enum_name(value, names),
                None => without_nulls(value),
            };
            match written {
                Some(written) => {
                    fields.insert(name.clone(), written);
                }
                None if !value.is_null() => export.not_exported.push(NotExported {
                    file: path.clone(),
                    setting: name.clone(),
                    reason: "a value Jellyfin's plugin cannot read; it uses its default",
                }),
                None => {}
            }
        } else if !document.derived.contains(&name.as_str()) && defaults.get(name) != Some(value) {
            export.not_exported.push(NotExported {
                file: path.clone(),
                setting: name.clone(),
                reason: "Jellyfin's plugin has no such setting",
            });
        }
    }
    for member in document.members {
        if !settings.contains_key(*member) && !document.derived.contains(member) {
            export.not_exported.push(NotExported {
                file: path.clone(),
                setting: (*member).to_owned(),
                reason: "Ferrofin does not keep it; Jellyfin's plugin uses its default",
            });
        }
    }
    // In declaration order: a setter may read a member set before it.
    let ordered = document
        .members
        .iter()
        .filter_map(|member| fields.get_key_value(*member));
    export.files.push(ExportedFile {
        contents: xml::document("PluginConfiguration", ordered, document.entries)?,
        path,
        private: true,
    });
    Ok(())
}

/// An enum setting as the name `XmlSerializer` reads: one of `names` in any
/// case, or its index (as .NET numbers an enum), as a number or as the text a
/// save stores a posted number as; `None` for anything else.
fn enum_name(value: &Value, names: &[&str]) -> Option<Value> {
    let name = match value {
        Value::String(text) => names
            .iter()
            .find(|n| n.eq_ignore_ascii_case(text.trim()))
            .or_else(|| text.trim().parse::<usize>().ok().and_then(|i| names.get(i))),
        Value::Number(n) => n
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| names.get(i)),
        _ => None,
    };
    name.map(|name| Value::String((*name).to_owned()))
}

/// `value` with every null member of every object in it left out; `None` for
/// a null.
fn without_nulls(value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::Object(fields) => Some(Value::Object(
            fields
                .iter()
                .filter_map(|(k, v)| without_nulls(v).map(|v| (k.clone(), v)))
                .collect(),
        )),
        Value::Array(items) => Some(Value::Array(
            items.iter().filter_map(without_nulls).collect(),
        )),
        other => Some(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const DOCUMENT: PluginDocument = PluginDocument {
        file: "Example.xml",
        members: &["Users", "Server", "Format", "Missing", "Bad"],
        entries: &[("Users", "User")],
        derived: &["Derived"],
        enums: &[("Format", &["Default", "Binary"]), ("Bad", &["Default"])],
    };

    fn map(value: &Value) -> Map<String, Value> {
        value.as_object().expect("an object").clone()
    }

    /// The members the counterpart has are written, nulls left out at any
    /// depth; a Ferrofin-only setting at its default and a derived one are
    /// silent, a changed Ferrofin-only one and a member Ferrofin lacks are
    /// reported by name.
    #[test]
    fn members_are_written_and_the_rest_reported_by_name() {
        let mut export = ConfigExport::default();
        export_plugin(
            &DOCUMENT,
            &map(&json!({
                "Server": "ldap.example",
                "Format": "binary",
                "Bad": "nonsense",
                "Users": [{"Uid": "ann", "Hash": null}],
                "Extra": "changed",
                "Quiet": 1,
                "Derived": true,
            })),
            &map(&json!({"Extra": "default", "Quiet": 1})),
            &mut export,
        )
        .expect("writes");
        let [file] = export.files.as_slice() else {
            panic!("{:?}", export.files);
        };
        assert_eq!(file.path, "plugins/configurations/Example.xml");
        assert!(file.private);
        assert_eq!(
            file.contents,
            r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <Users>
    <User>
      <Uid>ann</Uid>
    </User>
  </Users>
  <Server>ldap.example</Server>
  <Format>Binary</Format>
</PluginConfiguration>"#
        );
        let reported: Vec<(&str, &str)> = export
            .not_exported
            .iter()
            .map(|n| (n.setting.as_str(), n.reason))
            .collect();
        assert_eq!(
            reported,
            [
                (
                    "Bad",
                    "a value Jellyfin's plugin cannot read; it uses its default"
                ),
                ("Extra", "Jellyfin's plugin has no such setting"),
                (
                    "Missing",
                    "Ferrofin does not keep it; Jellyfin's plugin uses its default"
                ),
            ]
        );
    }

    /// An enum is written by its exact name, from any case or its index, a
    /// number or its text.
    #[test]
    fn an_enum_takes_its_exact_name() {
        let names = &["Default", "Binary", "Base64", "Url"];
        for (value, name) in [
            (json!("URL"), Some("Url")),
            (json!(" base64 "), Some("Base64")),
            (json!(1), Some("Binary")),
            (json!("2"), Some("Base64")),
            (json!(9), None),
            (json!("9"), None),
            (json!("nonsense"), None),
            (json!(true), None),
        ] {
            assert_eq!(enum_name(&value, names), name.map(|n| json!(n)), "{value}");
        }
    }
}
