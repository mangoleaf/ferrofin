//! `SubtitleProviderInfo` — port of
//! `MediaBrowser.Model.Providers.SubtitleProviderInfo`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Describes a subtitle provider.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SubtitleProviderInfo {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}
