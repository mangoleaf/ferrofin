//! `PlaylistDto` — port of `MediaBrowser.Model.Dto.PlaylistDto`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::entities_media::PlaylistUserPermissions;

/// DTO for playlists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlaylistDto {
    /// Gets or sets a value indicating whether the playlist is publicly readable.
    pub open_access: bool,

    /// Gets or sets the share permissions.
    pub shares: Vec<PlaylistUserPermissions>,

    /// Gets or sets the item ids.
    #[schema(value_type = Vec<String>, format = "uuid")]
    pub item_ids: Vec<Uuid>,
}
