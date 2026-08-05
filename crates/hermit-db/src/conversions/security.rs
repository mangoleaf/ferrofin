//! Conversions for the security entities.
//!
//! - [`DeviceEntity`] → [`DeviceInfo`]

use hermit_model::devices::DeviceInfo;
use hermit_model::session::ClientCapabilities;

use crate::conversions::parse_guid;
use crate::entities::security::DeviceEntity;
use crate::error::DbError;

impl TryFrom<DeviceEntity> for DeviceInfo {
    type Error = DbError;

    /// Maps a stored `Devices` row onto the wire DTO.
    ///
    /// `CustomName` (from the separate `DeviceOptions` table), `LastUserName`,
    /// `Capabilities` (session-scoped), and `IconUrl` are not carried by the
    /// `Devices` row, so they take their empty/default values. The stored
    /// `DeviceId` populates the DTO's `Id`, and `UserId` its `LastUserId`.
    ///
    /// # Errors
    /// Returns [`DbError::InvalidGuid`] if the stored `UserId` is not a valid
    /// `Guid`.
    fn try_from(entity: DeviceEntity) -> Result<Self, Self::Error> {
        Ok(Self {
            name: Some(entity.device_name),
            custom_name: None,
            access_token: Some(entity.access_token.into()),
            id: Some(entity.device_id),
            last_user_name: None,
            app_name: Some(entity.app_name),
            app_version: Some(entity.app_version),
            last_user_id: Some(parse_guid("Devices.UserId", &entity.user_id)?),
            date_last_activity: Some(entity.date_last_activity),
            capabilities: ClientCapabilities::default(),
            icon_url: None,
        })
    }
}
