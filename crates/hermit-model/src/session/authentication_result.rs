//! `AuthenticationResult` — port of
//! `MediaBrowser.Controller.Authentication.AuthenticationResult`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::dto::{SessionInfoDto, UserDto};
use crate::secret::Secret;

/// A class representing an authentication result.
///
/// This is the response body of the `AuthenticateByName` endpoint: the
/// authenticated [`UserDto`], the new [`SessionInfoDto`], and the access token
/// the client must present on subsequent requests.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct AuthenticationResult {
    /// Gets or sets the user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<UserDto>,

    /// Gets or sets the session info.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_info: Option<SessionInfoDto>,

    /// Gets or sets the access token.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub access_token: Option<Secret>,

    /// Gets or sets the server id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_names_are_pascal_case() {
        let result = AuthenticationResult {
            user: Some(UserDto::default()),
            session_info: Some(SessionInfoDto::default()),
            access_token: Some(Secret::new("token")),
            server_id: Some("srv".to_owned()),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("User").is_some());
        assert!(json.get("SessionInfo").is_some());
        assert_eq!(json["AccessToken"], "token");
        assert_eq!(json["ServerId"], "srv");
    }

    #[test]
    fn round_trips() {
        let result = AuthenticationResult {
            user: Some(UserDto {
                name: Some("Alice".to_owned()),
                ..UserDto::default()
            }),
            access_token: Some(Secret::new("abc")),
            ..AuthenticationResult::default()
        };
        let back: AuthenticationResult =
            serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
        assert_eq!(result, back);
    }

    #[test]
    fn optional_fields_omitted_when_none() {
        let json = serde_json::to_value(AuthenticationResult::default()).unwrap();
        assert!(json.get("User").is_none());
        assert!(json.get("SessionInfo").is_none());
        assert!(json.get("AccessToken").is_none());
        assert!(json.get("ServerId").is_none());
    }
}
