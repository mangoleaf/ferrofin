//! The library-item field bag the NFO parsers populate.
//!
//! Port of the `MediaBrowser.Controller.Entities` `BaseItem` hierarchy
//! (`BaseItem`/`Video`/`Movie`/`MusicVideo`/`Series`/`Season`/`Episode`) reduced
//! to the fields the XbmcMetadata NFO parsers read and write. Those entity
//! classes are server-side library plumbing deliberately dropped from
//! `ferrofin-model`, so — like `FileSystemMetadata` in `container_types` — they are
//! re-created locally here.
//!
//! Rather than a Rust trait hierarchy, the union of all fields lives on one
//! [`NfoBaseItem`] with an [`NfoItemKind`] discriminant. The parsers switch on
//! the kind exactly where the C# code did `item is Movie` / `is Series` /
//! `is IHasAspectRatio` type tests. This keeps the generic parser operating over
//! a single owned target (`MetadataResult<NfoBaseItem>`).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use ferrofin_model::dto::DayOfWeek;
use ferrofin_model::entities::{MetadataField, SeriesStatus, Video3DFormat};

use crate::container_types::set_provider_id;

/// Which `BaseItem` subclass an [`NfoBaseItem`] stands in for.
///
/// Drives the type-conditional branches in the parsers (`item is Movie`,
/// `is Series`, `is MusicVideo`, `is Video`, `IHasAspectRatio`,
/// `IHasDisplayOrder`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NfoItemKind {
    /// A plain video (`Video`).
    #[default]
    Video,
    /// A movie (`Movie` — `Video` + collection/aspect/display-order support).
    Movie,
    /// A music video (`MusicVideo` — `Video` + artist/album).
    MusicVideo,
    /// A TV series (`Series`).
    Series,
    /// A TV season (`Season`).
    Season,
    /// A TV episode (`Episode`).
    Episode,
}

impl NfoItemKind {
    /// Whether this kind derives from `Video` (`item is Video`).
    ///
    /// True for `Video`, `Movie` and `MusicVideo`; false for the TV kinds.
    #[must_use]
    pub fn is_video(self) -> bool {
        matches!(self, Self::Video | Self::Movie | Self::MusicVideo)
    }

    /// Whether this kind supports an aspect ratio (`item is IHasAspectRatio`).
    ///
    /// Upstream `Video` and `BaseItem` movie-set types implement it; here every
    /// video kind does.
    #[must_use]
    pub fn has_aspect_ratio(self) -> bool {
        self.is_video()
    }

    /// Whether this kind supports a display order (`item is IHasDisplayOrder`).
    ///
    /// Upstream `Series` and `BoxSet` implement it; here `Series` does.
    #[must_use]
    pub fn has_display_order(self) -> bool {
        matches!(self, Self::Series)
    }
}

/// The union field bag the NFO parsers populate.
///
/// A structural port of the read/written members across the `BaseItem`
/// hierarchy. Fields default to empty/`None` to match freshly-constructed C#
/// items. Provider ids are normalized on insert via [`NfoBaseItem::set_provider_id`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NfoBaseItem {
    /// The concrete item kind this stands in for.
    pub kind: NfoItemKind,

    /// The display name (`Name`).
    pub name: Option<String>,
    /// The original-language title (`OriginalTitle`).
    pub original_title: Option<String>,
    /// The sort name (`SortName`).
    pub sort_name: Option<String>,
    /// The forced sort name (`ForcedSortName`; from `sorttitle`).
    pub forced_sort_name: Option<String>,
    /// The plot/overview (`Overview`).
    pub overview: Option<String>,
    /// The tagline (`Tagline`).
    pub tagline: Option<String>,

    /// The critic rating, 0–100 (`CriticRating`).
    pub critic_rating: Option<f32>,
    /// The community rating, 0–10 (`CommunityRating`).
    pub community_rating: Option<f32>,
    /// The MPAA/official rating (`OfficialRating`).
    pub official_rating: Option<String>,
    /// The custom rating (`CustomRating`).
    pub custom_rating: Option<String>,

    /// The preferred metadata language (`PreferredMetadataLanguage`).
    pub preferred_metadata_language: Option<String>,
    /// The preferred metadata country code (`PreferredMetadataCountryCode`).
    pub preferred_metadata_country_code: Option<String>,

    /// The production year (`ProductionYear`).
    pub production_year: Option<i32>,
    /// The premiere date (`PremiereDate`).
    pub premiere_date: Option<DateTime<Utc>>,
    /// The end date (`EndDate`).
    pub end_date: Option<DateTime<Utc>>,
    /// The creation date (`DateCreated`; from `dateadded`).
    pub date_created: Option<DateTime<Utc>>,

    /// The run time in .NET ticks (`RunTimeTicks`; 100-ns units).
    pub run_time_ticks: Option<i64>,

    /// Whether the item is locked against auto edits (`IsLocked`).
    pub is_locked: bool,
    /// The fields locked against auto edits (`LockedFields`).
    pub locked_fields: Vec<MetadataField>,

    /// The genres (`Genres`).
    pub genres: Vec<String>,
    /// The studios (`Studios`).
    pub studios: Vec<String>,
    /// The tags (`Tags`).
    pub tags: Vec<String>,
    /// The production locations (`ProductionLocations`; from `country`).
    pub production_locations: Vec<String>,
    /// The trailer URLs (`RemoteTrailers`).
    pub remote_trailers: Vec<String>,

    /// External provider ids keyed by (normalized) provider name (`ProviderIds`).
    pub provider_ids: HashMap<String, String>,

    // ----- Video / IHasAspectRatio -----
    /// The aspect ratio (`AspectRatio`).
    pub aspect_ratio: Option<String>,
    /// The 3D format (`Video3DFormat`).
    pub video_3d_format: Option<Video3DFormat>,
    /// The pixel width (`Width`).
    pub width: Option<i32>,
    /// The pixel height (`Height`).
    pub height: Option<i32>,
    /// Whether the video carries subtitles (`HasSubtitles`).
    pub has_subtitles: bool,

    // ----- Movie -----
    /// The movie-set / collection name (`CollectionName`).
    pub collection_name: Option<String>,

    // ----- MusicVideo -----
    /// The artists (`Artists`).
    pub artists: Vec<String>,
    /// The album (`Album`).
    pub album: Option<String>,

    // ----- Series / IHasDisplayOrder -----
    /// The episode display order (`DisplayOrder`).
    pub display_order: Option<String>,
    /// The days the series airs (`AirDays`).
    pub air_days: Vec<DayOfWeek>,
    /// The time of day the series airs (`AirTime`).
    pub air_time: Option<String>,
    /// The series status (`Status`).
    pub status: Option<SeriesStatus>,

    // ----- Episode -----
    /// The episode/season index number (`IndexNumber`).
    pub index_number: Option<i32>,
    /// The last episode number in a multi-episode file (`IndexNumberEnd`).
    pub index_number_end: Option<i32>,
    /// The parent (season) index number (`ParentIndexNumber`).
    pub parent_index_number: Option<i32>,
    /// The owning series name (`SeriesName`).
    pub series_name: Option<String>,
    /// The episode this special airs before (`AirsBeforeEpisodeNumber`).
    pub airs_before_episode_number: Option<i32>,
    /// The season this special airs after (`AirsAfterSeasonNumber`).
    pub airs_after_season_number: Option<i32>,
    /// The season this special airs before (`AirsBeforeSeasonNumber`).
    pub airs_before_season_number: Option<i32>,
}

impl NfoBaseItem {
    /// Creates an empty item of the given [`NfoItemKind`].
    #[must_use]
    pub fn new(kind: NfoItemKind) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }

    /// Adds a genre if not already present (`AddGenre`).
    pub fn add_genre(&mut self, genre: impl Into<String>) {
        let genre = genre.into();
        if !self.genres.iter().any(|g| g.eq_ignore_ascii_case(&genre)) {
            self.genres.push(genre);
        }
    }

    /// Adds a studio if not already present (`AddStudio`).
    pub fn add_studio(&mut self, studio: impl Into<String>) {
        let studio = studio.into();
        if !self.studios.iter().any(|s| s.eq_ignore_ascii_case(&studio)) {
            self.studios.push(studio);
        }
    }

    /// Adds a tag if not already present (`AddTag`).
    pub fn add_tag(&mut self, tag: impl Into<String>) {
        let tag = tag.into();
        if !self.tags.iter().any(|t| t.eq_ignore_ascii_case(&tag)) {
            self.tags.push(tag);
        }
    }

    /// Adds a trailer URL if not already present (`AddTrailerUrl`).
    pub fn add_trailer_url(&mut self, url: impl Into<String>) {
        let url = url.into();
        if !self.remote_trailers.contains(&url) {
            self.remote_trailers.push(url);
        }
    }

    /// Sets a provider id, normalizing the key to the canonical provider
    /// spelling (`SetProviderId` / `TrySetProviderId`).
    ///
    /// Empty/whitespace keys or values (and keys containing `'='`) are ignored,
    /// matching `TrySetProviderId`'s no-throw contract.
    pub fn set_provider_id(&mut self, name: &str, value: &str) {
        set_provider_id(&mut self.provider_ids, name, value);
    }
}
