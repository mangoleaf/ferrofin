//! Collection & playlist manager traits — user-curated item groupings.
//!
//! Ports of `MediaBrowser.Controller.Collections.ICollectionManager` and
//! `MediaBrowser.Controller.Playlists.IPlaylistManager`.
//!
//! Port rules applied:
//! - The C# `BoxSet` / `Folder` / `Playlist` domain items become
//!   [`BaseItemEntity`](hermit_db::entities::base_items::BaseItemEntity) rows
//!   (a box set and a playlist are both `BaseItem`s); item and user identities
//!   become [`uuid::Uuid`].
//! - `CollectionCreationOptions` lives under `MediaBrowser.Controller`
//!   (service-layer, not a wire DTO), so it is ported here as
//!   [`CollectionCreationOptions`].
//! - Playlist request/result value types are wire DTOs already defined in
//!   `hermit-model` and are reused
//!   ([`PlaylistCreationRequest`]/[`PlaylistCreationResult`]/
//!   [`PlaylistUpdateRequest`]/[`PlaylistUserUpdateRequest`]/
//!   [`PlaylistUserPermissions`]).
//! - The `CollapseItemsWithinBoxSets` helper (operates on the un-ported domain
//!   `BaseItem` tree) and the `.NET` collection events are dropped; they
//!   resurface as `hermit-core` logic in Wave 6.
//! - `IEnumerable`/`IReadOnlyCollection` → `Vec`; `Task<T>` → `async fn ->
//!   Result<T, ServiceError>`.
//!
//! Both traits are object-safe and carry `_assert_object_safe_*` assertions.

use std::collections::HashMap;

use async_trait::async_trait;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_model::entities_media::PlaylistUserPermissions;
use hermit_model::playlists::{
    PlaylistCreationRequest, PlaylistCreationResult, PlaylistUpdateRequest,
    PlaylistUserUpdateRequest,
};
use uuid::Uuid;

use crate::error::ServiceError;

/// The options for creating a collection (box set).
///
/// Port of `MediaBrowser.Controller.Collections.CollectionCreationOptions`; the
/// nullable `Guid ParentId` becomes `Option<Uuid>` and the item/user id lists
/// become `Vec<Uuid>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CollectionCreationOptions {
    /// The display name of the new collection.
    pub name: String,
    /// The parent folder the collection is created under, if any.
    pub parent_id: Option<Uuid>,
    /// Whether the collection's metadata is locked against refresh.
    pub is_locked: bool,
    /// External provider ids to seed on the collection.
    pub provider_ids: HashMap<String, String>,
    /// The ids of the items to seed the collection with.
    pub item_id_list: Vec<Uuid>,
    /// The ids of users granted access to the collection.
    pub user_ids: Vec<Uuid>,
}

/// Creates and mutates collections (box sets).
///
/// Port of `ICollectionManager`.
#[async_trait]
pub trait CollectionManager: Send + Sync {
    /// Creates a new collection, returning the persisted box-set row.
    async fn create_collection(
        &self,
        options: &CollectionCreationOptions,
    ) -> Result<BaseItemEntity, ServiceError>;

    /// Adds items to an existing collection.
    async fn add_to_collection(
        &self,
        collection_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), ServiceError>;

    /// Removes items from a collection.
    async fn remove_from_collection(
        &self,
        collection_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), ServiceError>;

    /// Gets the collections accessible to a user that contain the given item.
    async fn get_collections_containing_item(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Gets the folder collections are stored under, creating it if requested.
    async fn get_collections_folder(
        &self,
        create_if_needed: bool,
    ) -> Result<Option<BaseItemEntity>, ServiceError>;
}

fn _assert_object_safe_collection_manager(_: &dyn CollectionManager) {}

/// Creates and mutates playlists.
///
/// Port of `IPlaylistManager`. The `SavePlaylistFile` side-channel (writes the
/// on-disk `.m3u`/`.pls`) is folded into the impl of the mutating methods and is
/// not part of the trait surface.
#[async_trait]
pub trait PlaylistManager: Send + Sync {
    /// Gets a playlist visible to the given user.
    async fn get_playlist_for_user(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
    ) -> Result<BaseItemEntity, ServiceError>;

    /// Creates a new playlist.
    async fn create_playlist(
        &self,
        request: &PlaylistCreationRequest,
    ) -> Result<PlaylistCreationResult, ServiceError>;

    /// Updates an existing playlist.
    async fn update_playlist(&self, request: &PlaylistUpdateRequest) -> Result<(), ServiceError>;

    /// Gets all playlists a user has access to.
    async fn get_playlists(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// Adds a user share to a playlist.
    async fn add_user_to_shares(
        &self,
        request: &PlaylistUserUpdateRequest,
    ) -> Result<(), ServiceError>;

    /// Removes a user share from a playlist.
    async fn remove_user_from_shares(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
        share: &PlaylistUserPermissions,
    ) -> Result<(), ServiceError>;

    /// Adds items to a playlist, optionally at a zero-based position.
    async fn add_item_to_playlist(
        &self,
        playlist_id: Uuid,
        item_ids: &[Uuid],
        position: Option<i32>,
        user_id: Uuid,
    ) -> Result<(), ServiceError>;

    /// Removes entries from a playlist by their entry ids.
    async fn remove_item_from_playlist(
        &self,
        playlist_id: &str,
        entry_ids: &[String],
    ) -> Result<(), ServiceError>;

    /// Moves a playlist entry to a new zero-based index.
    async fn move_item(
        &self,
        playlist_id: &str,
        entry_id: &str,
        new_index: i32,
        calling_user_id: Uuid,
    ) -> Result<(), ServiceError>;

    /// Removes all playlists owned by a user (transferring shared ones).
    async fn remove_playlists(&self, user_id: Uuid) -> Result<(), ServiceError>;
}

fn _assert_object_safe_playlist_manager(_: &dyn PlaylistManager) {}

#[cfg(test)]
mod tests {
    use super::CollectionCreationOptions;

    #[test]
    fn creation_options_default_is_empty() {
        let o = CollectionCreationOptions::default();
        assert!(o.name.is_empty());
        assert!(o.parent_id.is_none());
        assert!(!o.is_locked);
        assert!(o.provider_ids.is_empty());
        assert!(o.item_id_list.is_empty());
        assert!(o.user_ids.is_empty());
    }
}
