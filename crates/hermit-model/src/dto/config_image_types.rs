//! `ConfigImageTypes` — port of `TMDbLib.Objects.General.ConfigImageTypes`.
//!
//! The image portion of TMDb's `/configuration` response, surfaced verbatim by
//! `TmdbController.TmdbClientConfiguration`. TMDb publishes these values so a
//! client can build image URLs as `{BaseUrl}{size}{file_path}`; the fields are
//! nullable to match the wire contract (a partial or absent configuration).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The TMDb image configuration: the base URLs and the per-image-kind size
/// buckets a client uses to construct image URLs.
///
/// Port of `TMDbLib.Objects.General.ConfigImageTypes`. Every field is optional,
/// mirroring the OpenAPI contract; the size lists are the discrete widths TMDb
/// serves each image kind at (plus `"original"`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ConfigImageTypes {
    /// The available backdrop sizes (e.g. `w300`, `w780`, `w1280`, `original`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backdrop_sizes: Option<Vec<String>>,

    /// The (insecure) base URL image paths are appended to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// The available logo sizes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo_sizes: Option<Vec<String>>,

    /// The available poster sizes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_sizes: Option<Vec<String>>,

    /// The available profile (person) sizes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_sizes: Option<Vec<String>>,

    /// The HTTPS base URL image paths are appended to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secure_base_url: Option<String>,

    /// The available still (episode) sizes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub still_sizes: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::ConfigImageTypes;

    #[test]
    fn serializes_pascal_case_and_omits_none() {
        let config = ConfigImageTypes {
            secure_base_url: Some("https://image.tmdb.org/t/p/".to_owned()),
            poster_sizes: Some(vec!["w92".to_owned(), "original".to_owned()]),
            ..ConfigImageTypes::default()
        };
        let json = serde_json::to_value(&config).expect("serialize");
        assert_eq!(
            json["SecureBaseUrl"], "https://image.tmdb.org/t/p/",
            "PascalCase field name"
        );
        assert_eq!(json["PosterSizes"][1], "original");
        // Absent fields are omitted, not serialized as null.
        assert!(json.get("BaseUrl").is_none());
    }

    #[test]
    fn round_trips_through_json() {
        let config = ConfigImageTypes {
            base_url: Some("http://image.tmdb.org/t/p/".to_owned()),
            backdrop_sizes: Some(vec!["w300".to_owned()]),
            ..ConfigImageTypes::default()
        };
        let json = serde_json::to_string(&config).expect("serialize");
        let back: ConfigImageTypes = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, config);
    }
}
