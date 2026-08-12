//! `ClientCapabilities` — port of
//! `MediaBrowser.Model.Session.ClientCapabilities`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::GeneralCommandType;
use crate::data::MediaType;
use crate::dlna::DeviceProfile;

/// The full client-capabilities model (as opposed to the request DTO).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ClientCapabilities {
    /// Gets or sets the playable media types.
    pub playable_media_types: Vec<MediaType>,

    /// Gets or sets the supported commands.
    pub supported_commands: Vec<GeneralCommandType>,

    /// Gets or sets a value indicating whether media control is supported.
    pub supports_media_control: bool,

    /// Gets or sets a value indicating whether a persistent identifier is
    /// supported.
    pub supports_persistent_identifier: bool,

    /// Gets or sets the device profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_profile: Option<DeviceProfile>,

    /// Gets or sets the app store url.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_store_url: Option<String>,

    /// Gets or sets the icon url.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

impl Default for ClientCapabilities {
    fn default() -> Self {
        Self {
            playable_media_types: Vec::new(),
            supported_commands: Vec::new(),
            supports_media_control: false,
            supports_persistent_identifier: true,
            device_profile: None,
            app_store_url: None,
            icon_url: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_persistent_identifier_is_true() {
        let caps = ClientCapabilities::default();
        assert!(!caps.supports_media_control);
        assert!(caps.supports_persistent_identifier);
        assert!(caps.playable_media_types.is_empty());
    }

    #[test]
    fn round_trips_through_json() {
        let caps = ClientCapabilities {
            playable_media_types: vec![MediaType::Audio],
            supported_commands: vec![GeneralCommandType::MoveDown],
            supports_media_control: true,
            app_store_url: Some("https://store".to_owned()),
            ..ClientCapabilities::default()
        };
        let json = serde_json::to_value(&caps).unwrap();
        assert_eq!(json["PlayableMediaTypes"], serde_json::json!(["Audio"]));
        assert_eq!(json["SupportsMediaControl"], true);
        assert_eq!(json["SupportsPersistentIdentifier"], true);
        assert_eq!(json["AppStoreUrl"], "https://store");
        let back: ClientCapabilities = serde_json::from_value(json).unwrap();
        assert_eq!(caps, back);
    }
}
