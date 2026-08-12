//! `FromRow` structs for the playback / media leaf tables — `UserData`,
//! `TrickplayInfos`, and `MediaSegments`.
//!
//! Each struct mirrors one table one-to-one: field names and order match the
//! columns in `migrations/0001_initial.sql` (which reflects the EF model
//! snapshot). Column-to-Rust type mapping follows the conventions in the
//! [module docs](crate::entities):
//! - `INTEGER` surrogate/count columns → [`i32`] / [`i64`] (per the C# `int` /
//!   `long` width),
//! - `TEXT` `Guid` columns → [`String`] (the hyphenated form as stored; the
//!   conversion layer parses these into `Uuid`),
//! - `TEXT` `DateTime` columns → [`DateTime<Utc>`](chrono::DateTime),
//! - `INTEGER` booleans → [`bool`], `REAL` → [`f64`],
//! - the enum-valued `INTEGER` `Type` column of `MediaSegments` is kept as its
//!   [`i32`] discriminant here and mapped onto
//!   [`ferrofin_model::media_segments::MediaSegmentType`] by the conversion layer.

use chrono::{DateTime, Utc};

/// A row of the `UserData` table — a user's per-item playback state.
///
/// The natural primary key is the (`ItemId`, `UserId`, `CustomDataKey`) triple
/// (`CustomDataKey` distinguishes multiple data slots for the same item/user).
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct UserDataEntity {
    /// The item's `Guid`, hyphenated (`ItemId`, part of PK, FK → `BaseItems`).
    pub item_id: String,
    /// The user's `Guid`, hyphenated (`UserId`, part of PK, FK → `Users`).
    pub user_id: String,
    /// The custom data slot key (`CustomDataKey`, part of PK).
    pub custom_data_key: String,
    /// The index of the preferred audio stream (`AudioStreamIndex`), if any.
    pub audio_stream_index: Option<i32>,
    /// Whether the user has favourited the item (`IsFavorite`).
    pub is_favorite: bool,
    /// When the item was last played by the user (`LastPlayedDate`), if ever.
    pub last_played_date: Option<DateTime<Utc>>,
    /// Whether the user likes (`Some(true)`) or dislikes (`Some(false)`) the
    /// item (`Likes`); `None` when unset.
    pub likes: Option<bool>,
    /// How many times the user has played the item (`PlayCount`).
    pub play_count: i32,
    /// The resume position, in ticks (`PlaybackPositionTicks`).
    pub playback_position_ticks: i64,
    /// Whether the item is marked played (`Played`).
    pub played: bool,
    /// The user's 0–10 rating (`Rating`), if any.
    pub rating: Option<f64>,
    /// When the referenced item was deleted, for retention (`RetentionDate`).
    pub retention_date: Option<DateTime<Utc>>,
    /// The index of the preferred subtitle stream (`SubtitleStreamIndex`).
    pub subtitle_stream_index: Option<i32>,
}

/// A row of the `TrickplayInfos` table — metadata for one group of trickplay
/// (scrubbing-preview) tiles for an item.
///
/// The natural primary key is the (`ItemId`, `Width`) pair.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct TrickplayInfoEntity {
    /// The item's `Guid`, hyphenated (`ItemId`, part of PK, FK → `BaseItems`).
    pub item_id: String,
    /// The width of an individual thumbnail (`Width`, part of PK).
    pub width: i32,
    /// The peak bandwidth usage, in bits per second (`Bandwidth`).
    pub bandwidth: i32,
    /// The height of an individual thumbnail (`Height`).
    pub height: i32,
    /// The interval, in milliseconds, between thumbnails (`Interval`).
    pub interval: i32,
    /// The count of non-black thumbnails (`ThumbnailCount`).
    pub thumbnail_count: i32,
    /// The number of thumbnails per column (`TileHeight`).
    pub tile_height: i32,
    /// The number of thumbnails per row (`TileWidth`).
    pub tile_width: i32,
}

/// A row of the `MediaSegments` table — one typed time span within an item
/// (an intro, outro, recap, and so on).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct MediaSegmentEntity {
    /// The segment's `Guid`, hyphenated (`Id`, primary key).
    pub id: String,
    /// The end position, in ticks (`EndTicks`).
    pub end_ticks: i64,
    /// The item's `Guid`, hyphenated (`ItemId`).
    pub item_id: String,
    /// The identifier of the provider that produced the segment
    /// (`SegmentProviderId`).
    pub segment_provider_id: String,
    /// The start position, in ticks (`StartTicks`).
    pub start_ticks: i64,
    /// The segment type discriminant (`Type`), mapped onto
    /// [`ferrofin_model::media_segments::MediaSegmentType`] by the conversion
    /// layer.
    #[sqlx(rename = "Type")]
    pub type_: i32,
}
