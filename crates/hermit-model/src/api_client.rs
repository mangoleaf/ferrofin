//! Port of `MediaBrowser.Model.ApiClient`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The server discovery info model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ServerDiscoveryInfo {
    /// Gets the address.
    pub address: String,

    /// Gets the server identifier.
    pub id: String,

    /// Gets the name.
    pub name: String,

    /// Gets the endpoint address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_address: Option<String>,
}
