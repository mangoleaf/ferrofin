//! Conversions for the user-area entities.
//!
//! - [`ActivityLogEntity`] → [`ActivityLogEntry`]
//! - [`ImageInfoEntity`] → [`ImageInfo`] (a user profile image)

use ferrofin_model::activity::{ActivityLogEntry, LogLevel};
use ferrofin_model::dto::ImageInfo;
use ferrofin_model::entities::ImageType;

use crate::conversions::parse_guid;
use crate::entities::users::{ActivityLogEntity, ImageInfoEntity};
use crate::error::DbError;

impl TryFrom<ActivityLogEntity> for ActivityLogEntry {
    type Error = DbError;

    /// Maps a stored `ActivityLogs` row onto the wire DTO. `UserPrimaryImageTag`
    /// is deprecated and unused upstream, so it is left `None`.
    ///
    /// # Errors
    /// Returns [`DbError::InvalidGuid`] for a malformed `UserId`, or
    /// [`DbError::InvalidEnumValue`] for a `LogSeverity` outside `0..=6`.
    fn try_from(entity: ActivityLogEntity) -> Result<Self, Self::Error> {
        #[allow(deprecated)]
        Ok(Self {
            id: entity.id,
            name: entity.name,
            overview: entity.overview,
            short_overview: entity.short_overview,
            type_: entity.type_,
            item_id: entity.item_id,
            date: entity.date_created,
            user_id: parse_guid("ActivityLogs.UserId", &entity.user_id)?,
            user_primary_image_tag: None,
            // C# reads any stored integer back, named or not.
            severity: LogLevel::from_json_value(entity.log_severity),
        })
    }
}

impl TryFrom<ImageInfoEntity> for ImageInfo {
    type Error = DbError;

    /// Maps a stored `ImageInfos` row (a user's profile image) onto the wire
    /// DTO.
    ///
    /// The `ImageInfos` table stores only the path and modification time, so
    /// the image type is fixed to [`ImageType::Profile`] and the index, tag,
    /// blurhash, dimensions, and size take their empty/zero values.
    ///
    /// # Errors
    /// This conversion is infallible; the [`Result`] and [`DbError`] error type
    /// are kept for symmetry with the other entity conversions.
    fn try_from(entity: ImageInfoEntity) -> Result<Self, Self::Error> {
        Ok(Self {
            image_type: ImageType::Profile,
            image_index: None,
            image_tag: None,
            path: Some(entity.path),
            blur_hash: None,
            height: None,
            width: None,
            size: 0,
        })
    }
}
