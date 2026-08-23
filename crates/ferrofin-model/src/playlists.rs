//! Port of `MediaBrowser.Model.Playlists`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::data::MediaType;
use crate::entities_media::PlaylistUserPermissions;

/// A playlist creation request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlaylistCreationRequest {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the list of items.
    #[schema(value_type = Vec<String>, format = "uuid")]
    #[serde(with = "crate::json::guid::vec")]
    pub item_id_list: Vec<Uuid>,

    /// Gets or sets the media type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<MediaType>,

    /// Gets or sets the user id.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub user_id: Uuid,

    /// Gets or sets the user permissions.
    pub users: Vec<PlaylistUserPermissions>,

    /// Gets or sets a value indicating whether the playlist is public.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
}

/// The result of a playlist creation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlaylistCreationResult {
    /// Gets the playlist id.
    pub id: String,
}

/// A playlist update request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlaylistUpdateRequest {
    /// Gets or sets the id of the playlist.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,

    /// Gets or sets the id of the user updating the playlist.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub user_id: Uuid,

    /// Gets or sets the name of the playlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets item ids to add to the playlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<String>>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option_vec")]
    pub ids: Option<Vec<Uuid>>,

    /// Gets or sets the playlist users.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<Vec<PlaylistUserPermissions>>,

    /// Gets or sets a value indicating whether the playlist is public.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
}

/// Create new playlist dto.
///
/// Port of `Jellyfin.Api.Models.PlaylistDtos.CreatePlaylistDto` — the request
/// body accepted by `POST /Playlists`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct CreatePlaylistDto {
    /// Gets or sets the name of the new playlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets item ids to add to the playlist.
    #[serde(default)]
    #[schema(value_type = Vec<String>, format = "uuid")]
    #[serde(with = "crate::json::guid::vec")]
    pub ids: Vec<Uuid>,

    /// Gets or sets the user id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(with = "crate::json::guid::option")]
    pub user_id: Option<Uuid>,

    /// Gets or sets the media type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<MediaType>,

    /// Gets or sets the playlist users.
    #[serde(default)]
    pub users: Vec<PlaylistUserPermissions>,

    /// Gets or sets a value indicating whether the playlist is public.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_public: Option<bool>,
}

/// Update existing playlist dto. Fields set to `null` will not be updated and
/// keep their current values.
///
/// Port of `Jellyfin.Api.Models.PlaylistDtos.UpdatePlaylistDto` — the request
/// body accepted by `POST /Playlists/{playlistId}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct UpdatePlaylistDto {
    /// Gets or sets the name of the playlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets item ids of the playlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<String>>, format = "uuid")]
    #[serde(with = "crate::json::guid::option_vec")]
    pub ids: Option<Vec<Uuid>>,

    /// Gets or sets the playlist users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub users: Option<Vec<PlaylistUserPermissions>>,

    /// Gets or sets a value indicating whether the playlist is public.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_public: Option<bool>,
}

/// Update existing playlist user dto. Fields set to `null` will not be updated
/// and keep their current values.
///
/// Port of `Jellyfin.Api.Models.PlaylistDtos.UpdatePlaylistUserDto` — the
/// request body accepted by `POST /Playlists/{playlistId}/Users/{userId}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct UpdatePlaylistUserDto {
    /// Gets or sets a value indicating whether the user can edit the playlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_edit: Option<bool>,
}

/// A playlist user update request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlaylistUserUpdateRequest {
    /// Gets or sets the id of the playlist.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,

    /// Gets or sets the id of the updated user.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub user_id: Uuid,

    /// Gets or sets a value indicating whether the user can edit the playlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_edit: Option<bool>,
}
