//! `RemoteImageQuery` — port of
//! `MediaBrowser.Model.Providers.RemoteImageQuery`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entities::ImageType;

/// A query for remote images from a specific provider.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct RemoteImageQuery {
    /// Gets the provider name.
    pub provider_name: String,

    /// Gets or sets the image type.
    #[serde(rename = "ImageType", skip_serializing_if = "Option::is_none")]
    pub image_type: Option<ImageType>,

    /// Gets or sets a value indicating whether to include disabled providers.
    pub include_disabled_providers: bool,

    /// Gets or sets a value indicating whether to include all languages.
    pub include_all_languages: bool,
}

impl RemoteImageQuery {
    /// Initializes a new [`RemoteImageQuery`] for the given provider.
    #[must_use]
    pub fn new(provider_name: String) -> Self {
        Self {
            provider_name,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let value = RemoteImageQuery {
            image_type: Some(ImageType::Primary),
            include_disabled_providers: true,
            include_all_languages: true,
            ..RemoteImageQuery::new("TheMovieDb".to_owned())
        };
        let json = serde_json::to_string(&value).unwrap();
        let back: RemoteImageQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn uses_contract_field_names() {
        let value = RemoteImageQuery {
            image_type: Some(ImageType::Backdrop),
            include_disabled_providers: true,
            include_all_languages: false,
            ..RemoteImageQuery::new("Fanart".to_owned())
        };
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["ProviderName"], "Fanart");
        assert_eq!(json["ImageType"], "Backdrop");
        assert_eq!(json["IncludeDisabledProviders"], true);
        assert_eq!(json["IncludeAllLanguages"], false);
    }

    #[test]
    fn new_defaults_image_type_to_none() {
        let value = RemoteImageQuery::new("x".to_owned());
        assert!(value.image_type.is_none());
        let json = serde_json::to_value(&value).unwrap();
        assert!(json.get("ImageType").is_none());
    }
}
