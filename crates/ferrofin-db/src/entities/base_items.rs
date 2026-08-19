//! `FromRow` structs for the base-item core tables and their per-item child,
//! map, and stream tables — everything keyed to (or mapping into) `BaseItems`.
//!
//! Covered tables: `BaseItems`, `BaseItemImageInfos`, `BaseItemMetadataFields`,
//! `BaseItemProviders`, `BaseItemTrailerTypes`, `Chapters`, `AncestorIds`,
//! `ItemValues`, `ItemValuesMap`, `Peoples`, `PeopleBaseItemMap`,
//! `FerrofinLinkedChildren`, `AttachmentStreamInfos`, `MediaStreamInfos`, and
//! `KeyframeData`.
//!
//! Each struct mirrors one table one-to-one: field names and order match the
//! columns in `migrations/0001_initial.sql` (which reflects the EF model
//! snapshot). Column-to-Rust type mapping follows the conventions in the
//! [module docs](crate::entities):
//! - `INTEGER` surrogate keys → [`i64`]; other `INTEGER` numerics → [`i64`]
//!   (`Option<i64>` where nullable).
//! - `TEXT` `Guid` columns → [`String`] (the hyphenated form as stored; the
//!   conversion layer parses these into `Uuid`). All foreign keys to
//!   `BaseItems` are bare `Guid` columns (there is no ORM navigation).
//! - `TEXT` `DateTime` columns → [`DateTime<Utc>`](chrono::DateTime).
//! - `REAL` columns → [`f64`]; `INTEGER` booleans → [`bool`].
//! - Enum-valued `INTEGER` columns are kept as [`i32`] discriminants and mapped
//!   onto the [`crate::enums`] types (`ItemValueType`, `LinkedChildType`,
//!   `MediaStreamTypeEntity`, `ImageInfoImageType`) by the conversion layer.
//! - The `Blurhash` `BLOB` → [`Vec<u8>`]; the `KeyframeTicks` JSON `TEXT` → a
//!   [`String`] holding the raw JSON (parsed by the conversion layer).

use chrono::{DateTime, Utc};

/// A row of the `BaseItems` table — the central library-item record.
///
/// `OwnerId` and `ParentId` are self-referential foreign keys back into
/// `BaseItems` (bare `Guid` columns, no ORM navigation). `Audio`
/// (`ProgramAudio`) and `ExtraType` are stored as `INTEGER` discriminants and
/// kept here as [`i32`].
// A 1:1 mirror of the ~60-column `BaseItems` table; its many boolean flags are
// intrinsic to the schema, not a refactorable design.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Default, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct BaseItemEntity {
    /// The item's `Guid` primary key, hyphenated (`Id`).
    pub id: String,
    /// The album name (`Album`), if any.
    pub album: Option<String>,
    /// The album artists, as stored (`AlbumArtists`), if any.
    pub album_artists: Option<String>,
    /// The artists, as stored (`Artists`), if any.
    pub artists: Option<String>,
    /// The program-audio discriminant (`Audio`), if any.
    pub audio: Option<i32>,
    /// The owning channel's `Guid`, hyphenated (`ChannelId`), if any.
    pub channel_id: Option<String>,
    /// The normalized (clean) name for lookups (`CleanName`), if any.
    pub clean_name: Option<String>,
    /// The community rating (`CommunityRating`), if any.
    pub community_rating: Option<f64>,
    /// The critic rating (`CriticRating`), if any.
    pub critic_rating: Option<f64>,
    /// The custom rating string (`CustomRating`), if any.
    pub custom_rating: Option<String>,
    /// The serialized item payload (`Data`), if any.
    pub data: Option<String>,
    /// When the item was created (`DateCreated`), if known.
    pub date_created: Option<DateTime<Utc>>,
    /// When media was last added under the item (`DateLastMediaAdded`).
    pub date_last_media_added: Option<DateTime<Utc>>,
    /// When the item's metadata was last refreshed (`DateLastRefreshed`).
    pub date_last_refreshed: Option<DateTime<Utc>>,
    /// When the item was last saved (`DateLastSaved`), if known.
    pub date_last_saved: Option<DateTime<Utc>>,
    /// When the item was last modified (`DateModified`), if known.
    pub date_modified: Option<DateTime<Utc>>,
    /// The item's end date (`EndDate`), if any.
    pub end_date: Option<DateTime<Utc>>,
    /// The episode title (`EpisodeTitle`), if any.
    pub episode_title: Option<String>,
    /// The external id (`ExternalId`), if any.
    pub external_id: Option<String>,
    /// The external series id (`ExternalSeriesId`), if any.
    pub external_series_id: Option<String>,
    /// The external service id (`ExternalServiceId`), if any.
    pub external_service_id: Option<String>,
    /// The extra-type discriminant (`ExtraType`), if any.
    pub extra_type: Option<i32>,
    /// A forced sort name overriding `SortName` (`ForcedSortName`), if any.
    pub forced_sort_name: Option<String>,
    /// The genres, as stored (`Genres`), if any.
    pub genres: Option<String>,
    /// The pixel height (`Height`), if any.
    pub height: Option<i64>,
    /// The index number (e.g. episode number) (`IndexNumber`), if any.
    pub index_number: Option<i64>,
    /// The inherited parental rating sub-value (`InheritedParentalRatingSubValue`).
    pub inherited_parental_rating_sub_value: Option<i64>,
    /// The inherited parental rating value (`InheritedParentalRatingValue`).
    pub inherited_parental_rating_value: Option<i64>,
    /// Whether the item is a folder (`IsFolder`).
    pub is_folder: bool,
    /// Whether the item is in a mixed folder (`IsInMixedFolder`).
    pub is_in_mixed_folder: bool,
    /// Whether the item's metadata is locked (`IsLocked`).
    pub is_locked: bool,
    /// Whether the item is a movie (`IsMovie`).
    pub is_movie: bool,
    /// Whether the item is a repeat (`IsRepeat`).
    pub is_repeat: bool,
    /// Whether the item is a series (`IsSeries`).
    pub is_series: bool,
    /// Whether the item is a virtual (placeholder) item (`IsVirtualItem`).
    pub is_virtual_item: bool,
    /// The integrated loudness in LUFS (`LUFS`), if measured.
    #[sqlx(rename = "LUFS")]
    pub lufs: Option<f64>,
    /// The media type (`MediaType`), if any.
    pub media_type: Option<String>,
    /// The item's display name (`Name`), if any.
    pub name: Option<String>,
    /// The normalization gain in dB (`NormalizationGain`), if any.
    pub normalization_gain: Option<f64>,
    /// The official rating (`OfficialRating`), if any.
    pub official_rating: Option<String>,
    /// Pipe-delimited lowercase hyphenated GUIDs of this item's extras
    /// (`ExtraIds`) — 10.11.8's extras linkage (C# `string.Join('|', …)` over
    /// `Guid.ToString()`), kept in sync with `OwnerId` on the extras.
    pub extra_ids: Option<String>,
    /// The original title (`OriginalTitle`), if any.
    pub original_title: Option<String>,
    /// The overview text (`Overview`), if any.
    pub overview: Option<String>,
    /// The owning item's `Guid`, hyphenated (`OwnerId`, self-ref FK →
    /// `BaseItems`), if any.
    pub owner_id: Option<String>,
    /// The parent item's `Guid`, hyphenated (`ParentId`, self-ref FK →
    /// `BaseItems`), if any.
    pub parent_id: Option<String>,
    /// The parent index number (e.g. season number) (`ParentIndexNumber`).
    pub parent_index_number: Option<i64>,
    /// The item's file-system path (`Path`), if any.
    pub path: Option<String>,
    /// The preferred metadata country code (`PreferredMetadataCountryCode`).
    pub preferred_metadata_country_code: Option<String>,
    /// The preferred metadata language (`PreferredMetadataLanguage`).
    pub preferred_metadata_language: Option<String>,
    /// The premiere date (`PremiereDate`), if any.
    pub premiere_date: Option<DateTime<Utc>>,
    /// The presentation unique key (`PresentationUniqueKey`), if any.
    pub presentation_unique_key: Option<String>,
    /// The primary version item's `Guid`, hyphenated (`PrimaryVersionId`).
    pub primary_version_id: Option<String>,
    /// The production locations, as stored (`ProductionLocations`), if any.
    pub production_locations: Option<String>,
    /// The production year (`ProductionYear`), if any.
    pub production_year: Option<i64>,
    /// The runtime in ticks (`RunTimeTicks`), if any.
    pub run_time_ticks: Option<i64>,
    /// The owning season's `Guid`, hyphenated (`SeasonId`), if any.
    pub season_id: Option<String>,
    /// The season name (`SeasonName`), if any.
    pub season_name: Option<String>,
    /// The owning series' `Guid`, hyphenated (`SeriesId`), if any.
    pub series_id: Option<String>,
    /// The series name (`SeriesName`), if any.
    pub series_name: Option<String>,
    /// The series presentation unique key (`SeriesPresentationUniqueKey`).
    pub series_presentation_unique_key: Option<String>,
    /// The owning show's `Guid`, hyphenated (`ShowId`), if any.
    pub show_id: Option<String>,
    /// The item size in bytes (`Size`), if known.
    pub size: Option<i64>,
    /// The sort name (`SortName`), if any.
    pub sort_name: Option<String>,
    /// The item's start date (`StartDate`), if any.
    pub start_date: Option<DateTime<Utc>>,
    /// The studios, as stored (`Studios`), if any.
    pub studios: Option<String>,
    /// The tagline (`Tagline`), if any.
    pub tagline: Option<String>,
    /// The tags, as stored (`Tags`), if any.
    pub tags: Option<String>,
    /// The top-most parent's `Guid`, hyphenated (`TopParentId`), if any.
    pub top_parent_id: Option<String>,
    /// The total bitrate in bits per second (`TotalBitrate`), if known.
    pub total_bitrate: Option<i64>,
    /// The item's concrete type key (`Type`).
    #[sqlx(rename = "Type")]
    pub type_: String,
    /// The unrated-item type key (`UnratedType`), if any.
    pub unrated_type: Option<String>,
    /// The pixel width (`Width`), if any.
    pub width: Option<i64>,
}

/// A row of the `BaseItemImageInfos` table — one image attached to an item.
///
/// `ImageType` (`ImageInfoImageType`) is stored as an `INTEGER` discriminant
/// and kept here as [`i32`]. `Blurhash` is a `BLOB` and kept as [`Vec<u8>`].
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct BaseItemImageInfoEntity {
    /// The image row's `Guid` primary key, hyphenated (`Id`).
    pub id: String,
    /// The blurhash bytes (`Blurhash`), if computed.
    pub blurhash: Option<Vec<u8>>,
    /// When the image file was last modified (`DateModified`), if known.
    pub date_modified: Option<DateTime<Utc>>,
    /// The pixel height (`Height`).
    pub height: i64,
    /// The image-type discriminant (`ImageType`).
    pub image_type: i32,
    /// The owning item's `Guid`, hyphenated (`ItemId`, FK → `BaseItems`).
    pub item_id: String,
    /// The image's file path (`Path`).
    pub path: String,
    /// The pixel width (`Width`).
    pub width: i64,
}

/// A row of the `BaseItemMetadataFields` table — one locked metadata field for
/// an item (join of field id to item).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct BaseItemMetadataFieldEntity {
    /// The metadata-field id (`Id`, part of the composite key).
    pub id: i64,
    /// The owning item's `Guid`, hyphenated (`ItemId`, FK → `BaseItems`).
    pub item_id: String,
}

/// A row of the `BaseItemProviders` table — one external provider id/value pair
/// for an item.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct BaseItemProviderEntity {
    /// The owning item's `Guid`, hyphenated (`ItemId`, FK → `BaseItems`).
    pub item_id: String,
    /// The provider key (`ProviderId`).
    pub provider_id: String,
    /// The provider's value for the item (`ProviderValue`).
    pub provider_value: String,
}

/// A row of the `BaseItemTrailerTypes` table — one trailer-type flag for an
/// item (join of trailer-type id to item).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct BaseItemTrailerTypeEntity {
    /// The trailer-type id (`Id`, part of the composite key).
    pub id: i64,
    /// The owning item's `Guid`, hyphenated (`ItemId`, FK → `BaseItems`).
    pub item_id: String,
}

/// A row of the `Chapters` table — one chapter marker on an item.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct ChapterEntity {
    /// The owning item's `Guid`, hyphenated (`ItemId`, FK → `BaseItems`).
    pub item_id: String,
    /// The chapter's zero-based index within the item (`ChapterIndex`).
    pub chapter_index: i64,
    /// When the chapter image was last modified (`ImageDateModified`).
    pub image_date_modified: Option<DateTime<Utc>>,
    /// The chapter image's file path (`ImagePath`), if any.
    pub image_path: Option<String>,
    /// The chapter's name (`Name`), if any.
    pub name: Option<String>,
    /// The chapter's start position in ticks (`StartPositionTicks`).
    pub start_position_ticks: i64,
}

/// A row of the `AncestorIds` table — an (item, ancestor-item) closure edge.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct AncestorIdEntity {
    /// The descendant item's `Guid`, hyphenated (`ItemId`, FK → `BaseItems`).
    pub item_id: String,
    /// The ancestor item's `Guid`, hyphenated (`ParentItemId`, FK →
    /// `BaseItems`).
    pub parent_item_id: String,
}

/// A row of the `ItemValues` table — a distinct, categorized value (artist,
/// genre, tag, …) shared across items via `ItemValuesMap`.
///
/// `Type` (`ItemValueType`) is stored as an `INTEGER` discriminant and kept
/// here as [`i32`].
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct ItemValueEntity {
    /// The value's `Guid` primary key, hyphenated (`ItemValueId`).
    pub item_value_id: String,
    /// The normalized (clean) value for lookups (`CleanValue`).
    pub clean_value: String,
    /// The item-value-type discriminant (`Type`).
    #[sqlx(rename = "Type")]
    pub type_: i32,
    /// The value as displayed (`Value`).
    pub value: String,
}

/// A row of the `ItemValuesMap` table — a many-to-many edge linking an item to
/// an [`ItemValueEntity`].
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct ItemValueMapEntity {
    /// The linked value's `Guid`, hyphenated (`ItemValueId`, FK →
    /// `ItemValues`).
    pub item_value_id: String,
    /// The owning item's `Guid`, hyphenated (`ItemId`, FK → `BaseItems`).
    pub item_id: String,
}

/// A row of the `Peoples` table — a distinct person (actor, director, …).
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct PeopleEntity {
    /// The person's `Guid` primary key, hyphenated (`Id`).
    pub id: String,
    /// The person's name (`Name`).
    pub name: String,
    /// The person-type key (`PersonType`), if any.
    pub person_type: Option<String>,
    /// The credited role on the item being written (e.g. a character name).
    ///
    /// Not a `Peoples` column — it belongs to the `PeopleBaseItemMap` join, so it
    /// is `#[sqlx(default)]` (absent when reading a bare `Peoples` row) and carried
    /// on the write path so `update_people` can persist it.
    #[sqlx(default)]
    pub role: Option<String>,
    /// The remote profile-image URL to download for this person, on the write
    /// path. Not a column; `#[sqlx(default)]` so reads ignore it.
    #[sqlx(default)]
    pub primary_image_url: Option<String>,
    /// The remote provider id (TMDB person id) for a biography lookup, on the
    /// write path. Not a column; `#[sqlx(default)]` so reads ignore it.
    #[sqlx(default)]
    pub provider_id: Option<i64>,
}

/// A row of the `PeopleBaseItemMap` table — a person's credited role on an item.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct PeopleBaseItemMapEntity {
    /// The owning item's `Guid`, hyphenated (`ItemId`, FK → `BaseItems`).
    pub item_id: String,
    /// The person's `Guid`, hyphenated (`PeopleId`, FK → `Peoples`).
    pub people_id: String,
    /// The credited role (`Role`, part of the composite key).
    pub role: String,
    /// The display list order (`ListOrder`), if any.
    pub list_order: Option<i64>,
    /// The sort order (`SortOrder`), if any.
    pub sort_order: Option<i64>,
}

/// A row of the `LinkedChildren` table — a directed link from a parent item to
/// a child item (e.g. a playlist entry or alternate version).
///
/// `ChildType` (`LinkedChildType`) is stored as an `INTEGER` discriminant and
/// kept here as [`i32`].
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct LinkedChildEntity {
    /// The parent item's `Guid`, hyphenated (`ParentId`, FK → `BaseItems`).
    pub parent_id: String,
    /// The child item's `Guid`, hyphenated (`ChildId`, FK → `BaseItems`).
    pub child_id: String,
    /// The linked-child-type discriminant (`ChildType`).
    pub child_type: i32,
    /// The sort order within the parent (`SortOrder`), if any.
    pub sort_order: Option<i64>,
}

/// A row of the `AttachmentStreamInfos` table — one media attachment on an item.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct AttachmentStreamInfoEntity {
    /// The owning item's `Guid`, hyphenated (`ItemId`, FK → `BaseItems`).
    pub item_id: String,
    /// The attachment's zero-based index within the item (`Index`).
    pub index: i64,
    /// The codec (`Codec`), if any.
    pub codec: Option<String>,
    /// The codec tag (`CodecTag`), if any.
    pub codec_tag: Option<String>,
    /// A comment (`Comment`), if any.
    pub comment: Option<String>,
    /// The attachment's filename (`Filename`), if any.
    pub filename: Option<String>,
    /// The MIME type (`MimeType`), if any.
    pub mime_type: Option<String>,
}

/// A row of the `MediaStreamInfos` table — one media stream (video, audio,
/// subtitle, …) belonging to an item.
///
/// `StreamType` (`MediaStreamTypeEntity`) is stored as an `INTEGER`
/// discriminant and kept here as [`i32`]. The many nullable Dolby-Vision /
/// HDR flag columns are stored as nullable `INTEGER` booleans and kept as
/// [`Option<bool>`].
// A 1:1 mirror of the ~55-column `MediaStreamInfos` table; its many optional
// codec/HDR flags are intrinsic to the schema, not a refactorable design.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct MediaStreamInfoEntity {
    /// The owning item's `Guid`, hyphenated (`ItemId`, FK → `BaseItems`).
    pub item_id: String,
    /// The stream's zero-based index within the item (`StreamIndex`).
    pub stream_index: i64,
    /// The display aspect ratio (`AspectRatio`), if any.
    pub aspect_ratio: Option<String>,
    /// The average frame rate (`AverageFrameRate`), if any.
    pub average_frame_rate: Option<f64>,
    /// The audio bit depth (`BitDepth`), if any.
    pub bit_depth: Option<i64>,
    /// The bit rate in bits per second (`BitRate`), if any.
    pub bit_rate: Option<i64>,
    /// Whether the Dolby-Vision base-layer is present (`BlPresentFlag`).
    pub bl_present_flag: Option<bool>,
    /// The audio channel layout (`ChannelLayout`), if any.
    pub channel_layout: Option<String>,
    /// The number of audio channels (`Channels`), if any.
    pub channels: Option<i64>,
    /// The codec (`Codec`), if any.
    pub codec: Option<String>,
    /// The codec tag (`CodecTag`), if any.
    pub codec_tag: Option<String>,
    /// The codec time base (`CodecTimeBase`), if any.
    pub codec_time_base: Option<String>,
    /// The color primaries (`ColorPrimaries`), if any.
    pub color_primaries: Option<String>,
    /// The color space (`ColorSpace`), if any.
    pub color_space: Option<String>,
    /// The color transfer characteristics (`ColorTransfer`), if any.
    pub color_transfer: Option<String>,
    /// A comment (`Comment`), if any.
    pub comment: Option<String>,
    /// The Dolby-Vision BL signal compatibility id (`DvBlSignalCompatibilityId`).
    pub dv_bl_signal_compatibility_id: Option<i64>,
    /// The Dolby-Vision level (`DvLevel`), if any.
    pub dv_level: Option<i64>,
    /// The Dolby-Vision profile (`DvProfile`), if any.
    pub dv_profile: Option<i64>,
    /// The Dolby-Vision major version (`DvVersionMajor`), if any.
    pub dv_version_major: Option<i64>,
    /// The Dolby-Vision minor version (`DvVersionMinor`), if any.
    pub dv_version_minor: Option<i64>,
    /// Whether the Dolby-Vision enhancement-layer is present (`ElPresentFlag`).
    pub el_present_flag: Option<bool>,
    /// Whether HDR10+ is present (`Hdr10PlusPresentFlag`).
    pub hdr10_plus_present_flag: Option<bool>,
    /// The pixel height (`Height`), if any.
    pub height: Option<i64>,
    /// Whether the video is anamorphic (`IsAnamorphic`), if known.
    pub is_anamorphic: Option<bool>,
    /// Whether the stream is AVC (`IsAvc`), if known.
    pub is_avc: Option<bool>,
    /// Whether this is the default stream (`IsDefault`).
    pub is_default: bool,
    /// Whether the stream is external (`IsExternal`).
    pub is_external: bool,
    /// Whether the stream is forced (`IsForced`).
    pub is_forced: bool,
    /// Whether the stream is for the hearing impaired (`IsHearingImpaired`).
    pub is_hearing_impaired: Option<bool>,
    /// Whether the video is interlaced (`IsInterlaced`), if known.
    pub is_interlaced: Option<bool>,
    /// The key frames, as stored (`KeyFrames`), if any.
    pub key_frames: Option<String>,
    /// The stream language (`Language`), if any.
    pub language: Option<String>,
    /// The codec level (`Level`), if any.
    pub level: Option<f64>,
    /// The NAL length size (`NalLengthSize`), if any.
    pub nal_length_size: Option<String>,
    /// The stream's external file path (`Path`), if any.
    pub path: Option<String>,
    /// The pixel format (`PixelFormat`), if any.
    pub pixel_format: Option<String>,
    /// The codec profile (`Profile`), if any.
    pub profile: Option<String>,
    /// The real frame rate (`RealFrameRate`), if any.
    pub real_frame_rate: Option<f64>,
    /// The number of reference frames (`RefFrames`), if any.
    pub ref_frames: Option<i64>,
    /// The rotation in degrees (`Rotation`), if any.
    pub rotation: Option<i64>,
    /// Whether the Dolby-Vision RPU is present (`RpuPresentFlag`).
    pub rpu_present_flag: Option<bool>,
    /// The audio sample rate in Hz (`SampleRate`), if any.
    pub sample_rate: Option<i64>,
    /// The stream-type discriminant (`StreamType`).
    pub stream_type: i32,
    /// The time base (`TimeBase`), if any.
    pub time_base: Option<String>,
    /// The stream title (`Title`), if any.
    pub title: Option<String>,
    /// The pixel width (`Width`), if any.
    pub width: Option<i64>,
}

/// A row of the `KeyframeData` table — the extracted keyframe timings for an
/// item.
///
/// `KeyframeTicks` is a JSON `TEXT` column and kept here as a [`String`]
/// holding the raw JSON (parsed by the conversion layer).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct KeyframeDataEntity {
    /// The owning item's `Guid`, hyphenated (`ItemId`, PK / FK → `BaseItems`).
    pub item_id: String,
    /// The keyframe ticks as a raw JSON array string (`KeyframeTicks`), if any.
    pub keyframe_ticks: Option<String>,
    /// The item's total duration in ticks (`TotalDuration`).
    pub total_duration: i64,
}
