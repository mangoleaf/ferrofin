//! Port of `MediaBrowser.Model.Devices`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::secret::Secret;
use crate::session::ClientCapabilities;

/// A class for device information.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceInfo {
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

    /// Gets or sets the last name of the user.
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
    #[serde(default, with = "crate::json::guid::option")]
    pub last_user_id: Option<Uuid>,

    /// Gets or sets the date last activity.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub date_last_activity: Option<DateTime<Utc>>,

    /// Gets or sets the capabilities.
    pub capabilities: ClientCapabilities,

    /// Gets or sets the icon URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}
