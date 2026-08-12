//! `DeviceInfoDto` — port of `MediaBrowser.Model.Dto.DeviceInfoDto`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::ClientCapabilitiesDto;
use crate::secret::Secret;

/// A DTO representing device information.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceInfoDto {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the custom name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_name: Option<String>,

    /// Gets or sets the access token.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub access_token: Option<Secret>,

    /// Gets or sets the identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the last username.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_user_name: Option<String>,

    /// Gets or sets the name of the application.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,

    /// Gets or sets the application version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,

    /// Gets or sets the last user identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    pub last_user_id: Option<Uuid>,

    /// Gets or sets the date of last activity.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub date_last_activity: Option<DateTime<Utc>>,

    /// Gets or sets the capabilities.
    pub capabilities: ClientCapabilitiesDto,

    /// Gets or sets the icon URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

/// A DTO representing custom options for a device.
///
/// Port of `MediaBrowser.Model.Dto.DeviceOptionsDto`. Returned by `GET
/// /Devices/Options` and accepted (only its [`custom_name`](Self::custom_name))
/// by `POST /Devices/Options`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct DeviceOptionsDto {
    /// Gets or sets the id.
    pub id: i32,

    /// Gets or sets the device id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,

    /// Gets or sets the custom name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_name: Option<String>,
}
