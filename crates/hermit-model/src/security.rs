//! Port of `MediaBrowser.Controller.Security` — the authentication-info DTO.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::secret::Secret;

/// Information about an issued access token — a device session or an API key.
///
/// Port of `MediaBrowser.Controller.Security.AuthenticationInfo`. Surfaced by
/// `GET /Auth/Keys`, where each entry describes one stored server API key (its
/// device-related fields are empty, matching the C# projection).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct AuthenticationInfo {
    /// Gets or sets the identifier.
    pub id: i64,

    /// Gets or sets the access token.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub access_token: Option<Secret>,

    /// Gets or sets the device identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,

    /// Gets or sets the name of the application.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,

    /// Gets or sets the application version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,

    /// Gets or sets the name of the device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,

    /// Gets or sets the user identifier.
    #[schema(value_type = String, format = "uuid")]
    pub user_id: Uuid,

    /// Gets or sets a value indicating whether this instance is active.
    pub is_active: bool,

    /// Gets or sets the date created.
    #[schema(value_type = String, format = "date-time")]
    pub date_created: DateTime<Utc>,

    /// Gets or sets the date revoked.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub date_revoked: Option<DateTime<Utc>>,

    /// Gets or sets the date of last activity.
    #[schema(value_type = String, format = "date-time")]
    pub date_last_activity: DateTime<Utc>,

    /// Gets or sets the user name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::AuthenticationInfo;
    use crate::secret::Secret;

    #[test]
    fn serializes_to_pascal_case_keys() {
        let info = AuthenticationInfo {
            app_name: Some("Test App".to_owned()),
            access_token: Some(Secret::new("tok")),
            ..Default::default()
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["AppName"], "Test App");
        assert_eq!(json["AccessToken"], "tok");
        // Empty device fields are omitted, non-nullable scalars stay present.
        assert!(json.get("DeviceName").is_none());
        assert_eq!(json["IsActive"], false);
        assert!(json["Id"].is_i64());
    }

    #[test]
    fn round_trips_through_json() {
        let info = AuthenticationInfo {
            id: 7,
            app_name: Some("Key".to_owned()),
            ..Default::default()
        };
        let back: AuthenticationInfo =
            serde_json::from_str(&serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(info, back);
    }
}
