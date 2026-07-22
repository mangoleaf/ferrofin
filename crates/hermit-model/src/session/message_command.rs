//! `MessageCommand` — port of `MediaBrowser.Model.Session.MessageCommand`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A command to display a message on a client.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MessageCommand {
    /// Gets or sets the message header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,

    /// Gets or sets the message text.
    pub text: String,

    /// Gets or sets the timeout in milliseconds after which the message should
    /// be dismissed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<i64>,
}
