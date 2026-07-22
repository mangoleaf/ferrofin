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
    #[schema(value_type = String, format = "uuid")]
    pub controlling_user_id: Uuid,

    /// Gets the command arguments.
    pub arguments: HashMap<String, String>,
}
