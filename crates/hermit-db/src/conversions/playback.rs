//! Conversions for the playback / media leaf entities.
//!
//! - [`UserDataEntity`] → [`UserItemDataDto`]
//! - [`MediaSegmentEntity`] → [`MediaSegmentDto`]
//! - [`TrickplayInfoEntity`] → [`TrickplayInfoDto`]

use hermit_model::dto::{TrickplayInfoDto, UserItemDataDto};
use hermit_model::media_segments::{MediaSegmentDto, MediaSegmentType};

use crate::conversions::parse_guid;
use crate::entities::playback::{MediaSegmentEntity, TrickplayInfoEntity, UserDataEntity};
use crate::error::DbError;

impl TryFrom<UserDataEntity> for UserItemDataDto {
    type Error = DbError;

    /// Maps a stored `UserData` row onto the wire DTO.
    ///
    /// The `Key` field carries the row's `CustomDataKey`. `PlayedPercentage`
    /// and `UnplayedItemCount` are runtime-computed aggregates absent from the
    /// stored row, so they are left `None`.
    ///
    /// # Errors
    /// Returns [`DbError::InvalidGuid`] if the stored `ItemId` is not a valid
    /// `Guid`.
    fn try_from(entity: UserDataEntity) -> Result<Self, Self::Error> {
        Ok(Self {
            rating: entity.rating,
            played_percentage: None,
            unplayed_item_count: None,
            playback_position_ticks: entity.playback_position_ticks,
            play_count: entity.play_count,
            is_favorite: entity.is_favorite,
            likes: entity.likes,
            last_played_date: entity.last_played_date,
            played: entity.played,
            key: entity.custom_data_key,
            item_id: parse_guid("UserData.ItemId", &entity.item_id)?,
        })
    }
}

impl TryFrom<TrickplayInfoEntity> for TrickplayInfoDto {
    type Error = DbError;

    /// Maps a stored `TrickplayInfos` row onto the wire DTO (a field-for-field
    /// copy; the `ItemId` PK component is not part of the DTO).
    ///
    /// # Errors
    /// This conversion is infallible; the [`Result`] and [`DbError`] error type
    /// are kept for symmetry with the other entity conversions.
    fn try_from(entity: TrickplayInfoEntity) -> Result<Self, Self::Error> {
        Ok(Self {
            width: entity.width,
            height: entity.height,
            tile_width: entity.tile_width,
            tile_height: entity.tile_height,
            thumbnail_count: entity.thumbnail_count,
            interval: entity.interval,
            bandwidth: entity.bandwidth,
        })
    }
}

impl TryFrom<MediaSegmentEntity> for MediaSegmentDto {
    type Error = DbError;

    /// Maps a stored `MediaSegments` row onto the wire DTO, parsing the `Id`
    /// and `ItemId` `Guid`s and the `Type` discriminant.
    ///
    /// # Errors
    /// Returns [`DbError::InvalidGuid`] for a malformed `Id`/`ItemId`, or
    /// [`DbError::InvalidEnumValue`] for a `Type` outside `0..=5`.
    fn try_from(entity: MediaSegmentEntity) -> Result<Self, Self::Error> {
        Ok(Self {
            id: parse_guid("MediaSegments.Id", &entity.id)?,
            item_id: parse_guid("MediaSegments.ItemId", &entity.item_id)?,
            type_: media_segment_type_from_i32(entity.type_)?,
            start_ticks: entity.start_ticks,
            end_ticks: entity.end_ticks,
        })
    }
}

/// Reads a [`MediaSegmentType`] from its stored `INTEGER` discriminant.
///
/// Discriminants match the C# `MediaSegmentType` declaration order (0-based),
/// mirrored by the target enum.
///
/// # Errors
/// Returns [`DbError::InvalidEnumValue`] for a discriminant outside `0..=5`.
fn media_segment_type_from_i32(value: i32) -> Result<MediaSegmentType, DbError> {
    let kind = match value {
        0 => MediaSegmentType::Unknown,
        1 => MediaSegmentType::Commercial,
        2 => MediaSegmentType::Preview,
        3 => MediaSegmentType::Recap,
        4 => MediaSegmentType::Outro,
        5 => MediaSegmentType::Intro,
        other => {
            return Err(DbError::InvalidEnumValue {
                enum_name: "MediaSegmentType",
                value: other,
            });
        }
    };
    Ok(kind)
}
