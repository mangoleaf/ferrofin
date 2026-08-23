//! `GeneralCommand` — port of `MediaBrowser.Model.Session.GeneralCommand`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::GeneralCommandType;

/// A general remote-control command issued to a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct GeneralCommand {
    /// Gets or sets the command name.
    pub name: GeneralCommandType,

    /// Gets or sets the controlling user identifier.
    ///
    /// Optional on the wire: clients (and Jellyfin) omit it — the server fills it from
    /// the caller's session — so it defaults to the nil GUID when absent.
    #[serde(default)]
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub controlling_user_id: Uuid,

    /// Gets the command arguments.
    #[serde(default)]
    pub arguments: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_without_controlling_user_or_arguments() {
        // A client body carrying only Name — Jellyfin accepts this; the server fills
        // ControllingUserId from the caller's session and Arguments defaults to empty.
        let cmd: GeneralCommand = serde_json::from_str(r#"{"Name":"DisplayMessage"}"#).unwrap();
        assert_eq!(cmd.name, GeneralCommandType::DisplayMessage);
        assert_eq!(cmd.controlling_user_id, Uuid::nil());
        assert!(cmd.arguments.is_empty());
    }
}
