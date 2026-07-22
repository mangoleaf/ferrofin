//! `ExternalIdInfo` and `ExternalIdMediaType` — port of the matching types in
//! `MediaBrowser.Model.Providers`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The specific media type of an [`ExternalIdInfo`].
///
/// Client applications may use this as a translation key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum ExternalIdMediaType {
    /// A music album.
    Album = 1,
    /// The artist of a music album.
    AlbumArtist = 2,
    /// The artist of a media item.
    Artist = 3,
    /// A boxed set of media.
    BoxSet = 4,
    /// A series episode.
    Episode = 5,
    /// A movie.
    Movie = 6,
    /// An alternative artist apart from the main artist.
    OtherArtist = 7,
    /// A person.
    Person = 8,
    /// A release group.
    ReleaseGroup = 9,
    /// A single season of a series.
    Season = 10,
    /// A series.
    Series = 11,
    /// A music track.
    Track = 12,
    /// A book.
    Book = 13,
    /// A music recording.
    Recording = 14,
}

/// The external id information for serialization to the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ExternalIdInfo {
    /// Gets or sets the display name of the external id provider (IMDB,
    /// `MusicBrainz`, etc).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the unique key for this id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,

    /// Gets or sets the specific media type for this id.
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<ExternalIdMediaType>,
}

impl ExternalIdInfo {
    /// Initializes a new [`ExternalIdInfo`].
    #[must_use]
    pub fn new(name: String, key: String, type_: Option<ExternalIdMediaType>) -> Self {
        Self {
            name: Some(name),
            key: Some(key),
            type_,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let value = ExternalIdInfo::new(
            "IMDB".to_owned(),
            "imdb".to_owned(),
            Some(ExternalIdMediaType::Movie),
        );
        let json = serde_json::to_string(&value).unwrap();
        let back: ExternalIdInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn uses_contract_field_names() {
        let value = ExternalIdInfo::new(
            "TheMovieDb".to_owned(),
            "tmdb".to_owned(),
            Some(ExternalIdMediaType::Series),
        );
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["Name"], "TheMovieDb");
        assert_eq!(json["Key"], "tmdb");
        assert_eq!(json["Type"], "Series");
    }

    #[test]
    fn omits_none_fields() {
        let value = ExternalIdInfo {
            name: None,
            key: None,
            type_: None,
        };
        let json = serde_json::to_value(&value).unwrap();
        assert!(json.as_object().unwrap().is_empty());
    }

    #[test]
    fn media_type_round_trips_all_variants() {
        for variant in [
            ExternalIdMediaType::Album,
            ExternalIdMediaType::AlbumArtist,
            ExternalIdMediaType::Artist,
            ExternalIdMediaType::BoxSet,
            ExternalIdMediaType::Episode,
            ExternalIdMediaType::Movie,
            ExternalIdMediaType::OtherArtist,
            ExternalIdMediaType::Person,
            ExternalIdMediaType::ReleaseGroup,
            ExternalIdMediaType::Season,
            ExternalIdMediaType::Series,
            ExternalIdMediaType::Track,
            ExternalIdMediaType::Book,
            ExternalIdMediaType::Recording,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let back: ExternalIdMediaType = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, back);
        }
    }
}
