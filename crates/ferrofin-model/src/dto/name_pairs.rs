//! The `Name*Pair` helper DTOs — port of `NameGuidPair`, `NameIdPair`,
//! and `NameValuePair` from `MediaBrowser.Model.Dto`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// A name paired with a GUID identifier.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct NameGuidPair {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the identifier.
    ///
    /// `#[serde(default)]`: the metadata editor posts studios/genres as bare
    /// `{ "Name": … }` with no id, which strict serde would reject — default it to
    /// nil so those bodies deserialize (serialization still always emits it).
    #[serde(default)]
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,
}

/// A name paired with a string identifier.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct NameIdPair {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// A name paired with a string value.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct NameValuePair {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

impl NameValuePair {
    /// Initializes a new [`NameValuePair`] from a name and value.
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            value: Some(value.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_guid_pair_round_trips() {
        let value = NameGuidPair {
            name: Some("Genre".to_owned()),
            id: Uuid::from_u128(0x1234),
        };
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["Name"], "Genre");
        assert_eq!(json["Id"], value.id.simple().to_string());
        let back: NameGuidPair = serde_json::from_value(json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn name_id_pair_round_trips_and_omits_none() {
        let value = NameIdPair {
            name: Some("Studio".to_owned()),
            id: Some("42".to_owned()),
        };
        let back: NameIdPair =
            serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap();
        assert_eq!(value, back);

        let empty = serde_json::to_value(NameIdPair::default()).unwrap();
        assert!(empty.as_object().unwrap().is_empty());
    }

    #[test]
    fn name_value_pair_new_and_round_trip() {
        let value = NameValuePair::new("Key", "Value");
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["Name"], "Key");
        assert_eq!(json["Value"], "Value");
        let back: NameValuePair = serde_json::from_value(json).unwrap();
        assert_eq!(value, back);
    }
}
