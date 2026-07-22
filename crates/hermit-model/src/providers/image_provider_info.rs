//! `ImageProviderInfo` — port of
//! `MediaBrowser.Model.Providers.ImageProviderInfo`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entities::ImageType;

/// Describes an image provider and the image types it supports.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ImageProviderInfo {
    /// Gets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets the supported image types.
    pub supported_images: Vec<ImageType>,
}

impl ImageProviderInfo {
    /// Initializes a new [`ImageProviderInfo`].
    #[must_use]
    pub fn new(name: String, supported_images: Vec<ImageType>) -> Self {
        Self {
            name: Some(name),
            supported_images,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let value = ImageProviderInfo::new(
            "TheTVDB".to_owned(),
            vec![ImageType::Primary, ImageType::Backdrop],
        );
        let json = serde_json::to_string(&value).unwrap();
        let back: ImageProviderInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn uses_contract_field_names() {
        let value = ImageProviderInfo::new("Fanart".to_owned(), vec![ImageType::Logo]);
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["Name"], "Fanart");
        assert_eq!(json["SupportedImages"], serde_json::json!(["Logo"]));
    }

    #[test]
    fn default_omits_name() {
        let json = serde_json::to_value(ImageProviderInfo::default()).unwrap();
        assert!(json.get("Name").is_none());
        assert_eq!(json["SupportedImages"], serde_json::json!([]));
    }
}
