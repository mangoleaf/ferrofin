//! `BaseItemPerson` — port of `MediaBrowser.Model.Dto.BaseItemPerson`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::data::PersonKind;
use crate::entities::ImageType;

/// Information about a person within a `BaseItem`, used by the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct BaseItemPerson {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the identifier.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,

    /// Gets or sets the role.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,

    /// Gets or sets the type.
    #[serde(rename = "Type")]
    pub type_: PersonKind,

    /// Gets or sets the primary image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_image_tag: Option<String>,

    /// Gets or sets the primary image blurhashes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_blur_hashes: Option<HashMap<ImageType, HashMap<String, String>>>,
}

impl BaseItemPerson {
    /// Gets a value indicating whether this instance has a primary image.
    #[must_use]
    pub fn has_primary_image(&self) -> bool {
        self.primary_image_tag.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BaseItemPerson {
        BaseItemPerson {
            name: Some("Morgan Freeman".to_owned()),
            id: Uuid::from_u128(7),
            role: Some("Red".to_owned()),
            type_: PersonKind::Actor,
            primary_image_tag: Some("abc123".to_owned()),
            image_blur_hashes: None,
        }
    }

    #[test]
    fn round_trips_through_json() {
        let value = sample();
        let json = serde_json::to_string(&value).unwrap();
        let back: BaseItemPerson = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn uses_contract_field_names() {
        let json = serde_json::to_value(sample()).unwrap();
        assert_eq!(json["Name"], "Morgan Freeman");
        assert_eq!(json["Id"], Uuid::from_u128(7).simple().to_string());
        assert_eq!(json["Role"], "Red");
        assert_eq!(json["Type"], "Actor");
        assert_eq!(json["PrimaryImageTag"], "abc123");
    }

    #[test]
    fn has_primary_image_reflects_tag() {
        assert!(sample().has_primary_image());
        let without = BaseItemPerson {
            primary_image_tag: None,
            ..sample()
        };
        assert!(!without.has_primary_image());
    }
}
