//! `ClientCapabilitiesDto` — port of
//! `MediaBrowser.Model.Dto.ClientCapabilitiesDto`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::data::MediaType;
use crate::dlna::DeviceProfile;
use crate::session::{ClientCapabilities, GeneralCommandType};

/// Client capabilities DTO.
///
/// Upstream `PlayableMediaTypes`/`SupportedCommands` use a comma-delimited
/// collection JSON converter for query-string binding; on the wire in JSON they
/// serialize as plain arrays, which is what the derived serde impl produces.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct ClientCapabilitiesDto {
    /// Gets or sets the list of playable media types.
    pub playable_media_types: Vec<MediaType>,

    /// Gets or sets the list of supported commands.
    pub supported_commands: Vec<GeneralCommandType>,

    /// Gets or sets a value indicating whether the session supports media control.
    pub supports_media_control: bool,

    /// Gets or sets a value indicating whether the session supports a persistent
    /// identifier.
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

impl ClientCapabilitiesDto {
    /// Converts the DTO to the full [`ClientCapabilities`] model.
    #[must_use]
    pub fn to_client_capabilities(&self) -> ClientCapabilities {
        ClientCapabilities {
            playable_media_types: self.playable_media_types.clone(),
            supported_commands: self.supported_commands.clone(),
            supports_media_control: self.supports_media_control,
            supports_persistent_identifier: self.supports_persistent_identifier,
            device_profile: self.device_profile.clone(),
            app_store_url: self.app_store_url.clone(),
            icon_url: self.icon_url.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ClientCapabilitiesDto {
        ClientCapabilitiesDto {
            playable_media_types: vec![MediaType::Video, MediaType::Audio],
            supported_commands: vec![GeneralCommandType::MoveUp],
            supports_media_control: true,
            supports_persistent_identifier: true,
            device_profile: None,
            app_store_url: Some("https://store".to_owned()),
            icon_url: Some("https://icon".to_owned()),
        }
    }

    #[test]
    fn round_trips_through_json() {
        let value = sample();
        let json = serde_json::to_string(&value).unwrap();
        let back: ClientCapabilitiesDto = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn uses_contract_field_names() {
        let json = serde_json::to_value(sample()).unwrap();
        assert_eq!(
            json["PlayableMediaTypes"],
            serde_json::json!(["Video", "Audio"])
        );
        assert_eq!(json["SupportsMediaControl"], true);
        assert_eq!(json["SupportsPersistentIdentifier"], true);
        assert_eq!(json["AppStoreUrl"], "https://store");
        assert_eq!(json["IconUrl"], "https://icon");
    }

    #[test]
    fn to_client_capabilities_copies_fields() {
        let dto = sample();
        let caps = dto.to_client_capabilities();
        assert_eq!(caps.playable_media_types, dto.playable_media_types);
        assert_eq!(caps.supported_commands, dto.supported_commands);
        assert_eq!(caps.supports_media_control, dto.supports_media_control);
        assert_eq!(
            caps.supports_persistent_identifier,
            dto.supports_persistent_identifier
        );
        assert_eq!(caps.app_store_url, dto.app_store_url);
        assert_eq!(caps.icon_url, dto.icon_url);
    }

    #[test]
    fn default_omits_optional_urls() {
        let json = serde_json::to_value(ClientCapabilitiesDto::default()).unwrap();
        assert!(json.get("AppStoreUrl").is_none());
        assert!(json.get("IconUrl").is_none());
        assert!(json.get("DeviceProfile").is_none());
    }
}
