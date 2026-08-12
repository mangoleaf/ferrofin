//! `LyricProviderInfo` — port of
//! `MediaBrowser.Model.Providers.LyricProviderInfo`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Lyric provider info.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LyricProviderInfo {
    /// Gets the provider name.
    pub name: String,

    /// Gets the provider id.
    pub id: String,
}
