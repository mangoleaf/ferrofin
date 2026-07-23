//! Remote-search lookup-info DTOs — port of the
//! `MediaBrowser.Controller.Providers.ItemLookupInfo` hierarchy and the generic
//! `RemoteSearchQuery<T>` request wrapper.
//!
//! Every concrete `*Info` type flattens the shared [`ItemLookupInfo`] base
//! (`Name`/`Path`/`ProviderIds`/…) via `#[serde(flatten)]`, matching the C#
//! inheritance where each lookup type derives from `ItemLookupInfo`. The
//! type-specific fields (album artists, series name, …) are added alongside.
//!
//! Serde casing matches the Jellyfin JSON contract (PascalCase). Because the C#
//! constructor seeds `IsAutomated = true`, [`ItemLookupInfo`] does **not** derive
//! `Default` blindly — its [`Default`] impl sets `is_automated` to `true` to stay
//! faithful.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entities_media::IHasProviderIds;

/// The shared base for every remote-search lookup info.
///
/// Port of `MediaBrowser.Controller.Providers.ItemLookupInfo`. Flattened into the
/// concrete `*Info` types so their wire shape carries these fields inline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ItemLookupInfo {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the original title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_title: Option<String>,

    /// Gets or sets the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Gets or sets the metadata language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_language: Option<String>,

    /// Gets or sets the metadata country code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_country_code: Option<String>,

    /// Gets or sets the provider ids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_ids: Option<HashMap<String, String>>,

    /// Gets or sets the year.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,

    /// Gets or sets the index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_number: Option<i32>,

    /// Gets or sets the parent index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_index_number: Option<i32>,

    /// Gets or sets the premiere date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub premiere_date: Option<DateTime<Utc>>,

    /// Gets or sets a value indicating whether this lookup was automated.
    ///
    /// The C# constructor seeds this to `true`; [`Default`] does the same.
    #[serde(default)]
    pub is_automated: bool,
}

impl Default for ItemLookupInfo {
    fn default() -> Self {
        Self {
            name: None,
            original_title: None,
            path: None,
            metadata_language: None,
            metadata_country_code: None,
            provider_ids: None,
            year: None,
            index_number: None,
            parent_index_number: None,
            premiere_date: None,
            is_automated: true,
        }
    }
}

impl IHasProviderIds for ItemLookupInfo {
    fn provider_ids(&self) -> Option<&HashMap<String, String>> {
        self.provider_ids.as_ref()
    }

    fn provider_ids_mut(&mut self) -> &mut HashMap<String, String> {
        self.provider_ids.get_or_insert_with(HashMap::new)
    }

    fn provider_ids_opt_mut(&mut self) -> &mut Option<HashMap<String, String>> {
        &mut self.provider_ids
    }
}

/// Generates a concrete lookup-info newtype flattening [`ItemLookupInfo`].
macro_rules! lookup_info {
    (
        $(#[$meta:meta])*
        $name:ident { $( $(#[$fmeta:meta])* $field:ident : $ty:ty ),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
        #[serde(rename_all = "PascalCase")]
        pub struct $name {
            /// The shared [`ItemLookupInfo`] fields (flattened inline on the wire).
            #[serde(flatten)]
            pub base: ItemLookupInfo,
            $(
                $(#[$fmeta])*
                #[serde(
                    default,
                    skip_serializing_if = "crate::providers::lookup_info::is_empty_default"
                )]
                pub $field: $ty,
            )*
        }
    };
}

/// True when a flattened optional field should be skipped on serialization.
///
/// Used by the [`lookup_info!`] macro for the type-specific extension fields so a
/// default value (empty vec / `None`) is omitted, matching the contract's
/// `nullable`/optional shape.
fn is_empty_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

lookup_info! {
    /// Movie remote-search lookup info. Port of `MovieInfo`.
    MovieInfo {}
}

lookup_info! {
    /// Trailer remote-search lookup info. Port of `TrailerInfo`.
    TrailerInfo {}
}

lookup_info! {
    /// Box-set remote-search lookup info. Port of `BoxSetInfo`.
    BoxSetInfo {}
}

lookup_info! {
    /// Series remote-search lookup info. Port of `SeriesInfo`.
    SeriesInfo {}
}

lookup_info! {
    /// Person remote-search lookup info. Port of `PersonLookupInfo`.
    PersonLookupInfo {}
}

lookup_info! {
    /// Music-video remote-search lookup info. Port of `MusicVideoInfo`.
    MusicVideoInfo {
        /// Gets or sets the artists.
        artists: Vec<String>,
    }
}

lookup_info! {
    /// Book remote-search lookup info. Port of `BookInfo`.
    BookInfo {
        /// Gets or sets the series name.
        series_name: Option<String>,
    }
}

lookup_info! {
    /// Song remote-search lookup info. Port of `SongInfo`.
    ///
    /// Not a top-level route in this batch, but carried inside [`AlbumInfo`] and
    /// [`ArtistInfo`] via `SongInfos`.
    SongInfo {
        /// Gets or sets the album artists.
        album_artists: Vec<String>,
        /// Gets or sets the album.
        album: Option<String>,
        /// Gets or sets the artists.
        artists: Vec<String>,
    }
}

lookup_info! {
    /// Music-album remote-search lookup info. Port of `AlbumInfo`.
    AlbumInfo {
        /// Gets or sets the album artists.
        album_artists: Vec<String>,
        /// Gets or sets the artist provider ids.
        artist_provider_ids: Option<HashMap<String, String>>,
        /// Gets or sets the contained song lookup infos.
        song_infos: Vec<SongInfo>,
    }
}

lookup_info! {
    /// Music-artist remote-search lookup info. Port of `ArtistInfo`.
    ArtistInfo {
        /// Gets or sets the contained song lookup infos.
        song_infos: Vec<SongInfo>,
    }
}

/// A remote metadata search request wrapping a lookup info of type `T`.
///
/// Port of `MediaBrowser.Controller.Providers.RemoteSearchQuery<T>`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct RemoteSearchQuery<T> {
    /// The lookup info describing what to search for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_info: Option<T>,

    /// The id of an existing item to use as the reference for the search.
    #[serde(default, skip_serializing_if = "uuid::Uuid::is_nil")]
    #[schema(value_type = String, format = "uuid")]
    pub item_id: uuid::Uuid,

    /// Gets or sets the provider name to search within if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_provider_name: Option<String>,

    /// Gets or sets a value indicating whether disabled providers should be
    /// included.
    #[serde(default)]
    pub include_disabled_providers: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_lookup_info_defaults_is_automated_true() {
        let info = ItemLookupInfo::default();
        assert!(info.is_automated);
        assert!(info.name.is_none());
    }

    #[test]
    fn base_fields_flatten_inline() {
        let base = ItemLookupInfo {
            name: Some("Inception".to_owned()),
            year: Some(2010),
            ..ItemLookupInfo::default()
        };
        let info = MovieInfo { base };
        let json = serde_json::to_value(&info).unwrap();
        // Flattened base fields sit at the top level, not nested under "Base".
        assert_eq!(json["Name"], "Inception");
        assert_eq!(json["Year"], 2010);
        assert!(json.get("Base").is_none());
    }

    #[test]
    fn album_info_round_trips_with_song_infos() {
        let info = AlbumInfo {
            base: ItemLookupInfo {
                name: Some("Kind of Blue".to_owned()),
                ..ItemLookupInfo::default()
            },
            album_artists: vec!["Miles Davis".to_owned()],
            song_infos: vec![SongInfo {
                base: ItemLookupInfo {
                    name: Some("So What".to_owned()),
                    ..ItemLookupInfo::default()
                },
                ..SongInfo::default()
            }],
            ..AlbumInfo::default()
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: AlbumInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value["AlbumArtists"][0], "Miles Davis");
        assert_eq!(value["SongInfos"][0]["Name"], "So What");
    }

    #[test]
    fn empty_extension_fields_are_omitted() {
        let json = serde_json::to_value(MusicVideoInfo::default()).unwrap();
        // Empty `Artists` vec is skipped, but base `IsAutomated` is always present.
        assert!(json.get("Artists").is_none());
        assert_eq!(json["IsAutomated"], true);
    }

    #[test]
    fn query_deserializes_from_contract_shape() {
        let raw = r#"{
            "SearchInfo": { "Name": "The Matrix", "Year": 1999 },
            "SearchProviderName": "TheMovieDb",
            "IncludeDisabledProviders": true
        }"#;
        let query: RemoteSearchQuery<MovieInfo> = serde_json::from_str(raw).unwrap();
        let info = query.search_info.expect("search info present");
        assert_eq!(info.base.name.as_deref(), Some("The Matrix"));
        assert_eq!(info.base.year, Some(1999));
        assert_eq!(query.search_provider_name.as_deref(), Some("TheMovieDb"));
        assert!(query.include_disabled_providers);
        assert!(query.item_id.is_nil());
    }

    #[test]
    fn book_info_carries_series_name() {
        let info = BookInfo {
            series_name: Some("Discworld".to_owned()),
            ..BookInfo::default()
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["SeriesName"], "Discworld");
    }

    #[test]
    fn provider_ids_accessors_delegate_to_base() {
        let mut info = ItemLookupInfo::default();
        assert!(info.provider_ids().is_none());
        info.provider_ids_mut()
            .insert("Tmdb".to_owned(), "27205".to_owned());
        assert_eq!(info.provider_ids().unwrap()["Tmdb"], "27205");
    }
}
