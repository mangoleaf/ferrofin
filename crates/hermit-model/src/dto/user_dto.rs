//! `UserDto` — port of `MediaBrowser.Model.Dto.UserDto`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::configuration::UserConfiguration;
use crate::users::UserPolicy;

/// Class `UserDto`.
///
/// The public representation of a user, returned by the users endpoints and
/// embedded in [`AuthenticationResult`](crate::session::AuthenticationResult).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserDto {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the server identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,

    /// Gets or sets the name of the server.
    ///
    /// This is not used by the server and is for client-side usage only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,

    /// Gets or sets the id.
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,

    /// Gets or sets the primary image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_image_tag: Option<String>,

    /// Gets or sets a value indicating whether this instance has password.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_password: Option<bool>,

    /// Gets or sets a value indicating whether this instance has configured password.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_configured_password: Option<bool>,

    /// Gets or sets a value indicating whether this instance has configured easy password.
    ///
    /// Deprecated: easy password has been replaced with Quick Connect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_configured_easy_password: Option<bool>,

    /// Gets or sets whether auto login is enabled or not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_auto_login: Option<bool>,

    /// Gets or sets the last login date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub last_login_date: Option<DateTime<Utc>>,

    /// Gets or sets the last activity date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub last_activity_date: Option<DateTime<Utc>>,

    /// Gets or sets the configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<UserConfiguration>,

    /// Gets or sets the policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<UserPolicy>,

    /// Gets or sets the primary image aspect ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_image_aspect_ratio: Option<f64>,
}

impl Default for UserDto {
    fn default() -> Self {
        // Mirrors the C# constructor: a fresh `UserConfiguration`/`UserPolicy`
        // and `HasPassword`/`HasConfiguredPassword` default to `true`.
        Self {
            name: None,
            server_id: None,
            server_name: None,
            id: Uuid::nil(),
            primary_image_tag: None,
            has_password: Some(true),
            has_configured_password: Some(true),
            has_configured_easy_password: Some(false),
            enable_auto_login: None,
            last_login_date: None,
            last_activity_date: None,
            configuration: Some(UserConfiguration::default()),
            policy: Some(UserPolicy::default()),
            primary_image_aspect_ratio: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_csharp_constructor() {
        let dto = UserDto::default();
        assert_eq!(dto.has_password, Some(true));
        assert_eq!(dto.has_configured_password, Some(true));
        assert_eq!(dto.has_configured_easy_password, Some(false));
        assert!(dto.configuration.is_some());
        assert!(dto.policy.is_some());
    }

    #[test]
    fn field_names_are_pascal_case() {
        let dto = UserDto {
            name: Some("Alice".to_owned()),
            server_id: Some("srv".to_owned()),
            server_name: Some("Home".to_owned()),
            primary_image_tag: Some("tag".to_owned()),
            enable_auto_login: Some(true),
            primary_image_aspect_ratio: Some(1.5),
            ..UserDto::default()
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["Name"], "Alice");
        assert_eq!(json["ServerId"], "srv");
        assert_eq!(json["ServerName"], "Home");
        assert!(json.get("Id").is_some());
        assert_eq!(json["PrimaryImageTag"], "tag");
        assert_eq!(json["HasPassword"], true);
        assert_eq!(json["HasConfiguredPassword"], true);
        assert_eq!(json["HasConfiguredEasyPassword"], false);
        assert_eq!(json["EnableAutoLogin"], true);
        assert!(json.get("Configuration").is_some());
        assert!(json.get("Policy").is_some());
        assert_eq!(json["PrimaryImageAspectRatio"], 1.5);
    }

    #[test]
    fn round_trips() {
        let dto = UserDto {
            name: Some("Bob".to_owned()),
            id: Uuid::from_u128(42),
            last_login_date: Some(Utc::now()),
            ..UserDto::default()
        };
        let back: UserDto = serde_json::from_str(&serde_json::to_string(&dto).unwrap()).unwrap();
        assert_eq!(dto, back);
    }

    #[test]
    fn optional_fields_omitted_when_none() {
        let dto = UserDto {
            name: None,
            enable_auto_login: None,
            has_password: None,
            has_configured_password: None,
            has_configured_easy_password: None,
            configuration: None,
            policy: None,
            ..UserDto::default()
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert!(json.get("Name").is_none());
        assert!(json.get("EnableAutoLogin").is_none());
        assert!(json.get("HasPassword").is_none());
        assert!(json.get("Configuration").is_none());
    }
}
