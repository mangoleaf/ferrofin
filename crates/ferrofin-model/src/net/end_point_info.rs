//! Port of `MediaBrowser.Model.Net.EndPointInfo`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Information about a request endpoint's network position.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct EndPointInfo {
    /// Whether the endpoint is on the local machine.
    pub is_local: bool,
    /// Whether the endpoint is within the configured network.
    pub is_in_network: bool,
}
