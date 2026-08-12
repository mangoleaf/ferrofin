//! `RemoteImageResult` — port of
//! `MediaBrowser.Model.Providers.RemoteImageResult`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::RemoteImageInfo;

/// The result of a remote image query.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct RemoteImageResult {
    /// Gets or sets the images.
    pub images: Vec<RemoteImageInfo>,

    /// Gets or sets the total record count.
    pub total_record_count: i32,

    /// Gets or sets the providers.
    pub providers: Vec<String>,
}
