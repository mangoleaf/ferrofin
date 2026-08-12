//! `ExternalUrl` — port of `MediaBrowser.Model.Providers.ExternalUrl`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A named external URL for an item.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ExternalUrl {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
