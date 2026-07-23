//! [`HermitCollectionManager`] + [`HermitPlaylistManager`] — **minimal**
//! [`CollectionManager`]/[`PlaylistManager`] impls.
//!
//! Ports of `Emby.Server.Implementations.Collections.CollectionManager` and
//! `Emby.Server.Implementations.Playlists.PlaylistManager`. Both are thin unit-8
//! managers: a box set and a playlist are both `BaseItem` rows, so creating one
//! inserts a minimal `BaseItems` row (of the `BoxSet`/`Playlist` stored type)
//! through `hermit-db`, and membership is recorded as manual **linked children**
//! via the injected
//! [`LinkedChildrenService`](hermit_traits::persistence::LinkedChildrenService).
//! Reads go through the injected
//! [`LibraryManager`](hermit_traits::library::LibraryManager) so the same
//! persistence seam backs everything.
//!
//! Deferred (documented, per the unit-8 minimal-manager rule):
//! - the on-disk `.m3u`/`.pls` *playlist file* writes (`SavePlaylistFile`) — a
//!   filesystem concern folded away from the trait; not performed here;
//!   ordering therefore lives purely in the linked-children rows;
//!   entry-id addressing (`remove_item_from_playlist`/`move_item` take opaque
//!   entry-id strings) is approximated by the child item id;
//! - per-user **share** permissions (`PlaylistUserPermissions`) are stored only
//!   in memory is *not* attempted — the share methods are accepted no-ops until
//!   the `PlaylistShares` persistence lands (flagged deferred);
//! - the collections **folder** (`GetCollectionsFolder`) resolution needs the
//!   user-view tree and returns `None` here.

use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_model::data::BaseItemKind;
use hermit_model::entities_media::PlaylistUserPermissions;
use hermit_model::playlists::{
    PlaylistCreationRequest, PlaylistCreationResult, PlaylistUpdateRequest,
    PlaylistUserUpdateRequest,
};
use uuid::Uuid;

use hermit_traits::collections::{CollectionCreationOptions, CollectionManager, PlaylistManager};
use hermit_traits::error::ServiceError;
use hermit_traits::library::LibraryManager;
use hermit_traits::persistence::LinkedChildrenService;

use crate::db_error::db_err;
use crate::item_type_lookup::stored_type_name;

/// The manual [`LinkedChildType`](hermit_db::enums::LinkedChildType) discriminant
/// (`0`) used to record a box-set/playlist member.
const LINKED_CHILD_MANUAL: i32 = 0;

/// Inserts a minimal `BaseItems` row of the given folder-ish kind and returns
/// the persisted row. Only the schema-required columns are set; richer metadata
/// is populated by later refreshes (mirrors how the C# path creates a stub item
/// then refreshes it).
async fn insert_named_item(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
    name: &str,
    is_folder: bool,
) -> Result<BaseItemEntity, ServiceError> {
    let type_name = stored_type_name(kind)
        .ok_or_else(|| ServiceError::backend(format!("no stored type name for {kind:?}")))?;
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Name")
           VALUES (?1, ?2, ?3, 0, 0, 0, 0, 0, 0, ?4)"#,
    )
    .bind(id.to_string())
    .bind(type_name)
    .bind(i64::from(is_folder))
    .bind(name)
    .execute(db.pool())
    .await
    .map_err(db_err)?;

    sqlx::query_as::<_, BaseItemEntity>(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(id.to_string())
        .fetch_one(db.pool())
        .await
        .map_err(db_err)
}

/// The concrete (minimal) collection (box-set) manager.
#[derive(Clone)]
pub struct HermitCollectionManager {
    db: Database,
    library_manager: Arc<dyn LibraryManager>,
    linked_children: Arc<dyn LinkedChildrenService>,
}

impl std::fmt::Debug for HermitCollectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitCollectionManager")
            .finish_non_exhaustive()
    }
}

impl HermitCollectionManager {
    /// Creates a collection manager from its injected collaborators.
    #[must_use]
    pub fn new(
        db: Database,
        library_manager: Arc<dyn LibraryManager>,
        linked_children: Arc<dyn LinkedChildrenService>,
    ) -> Self {
        Self {
            db,
            library_manager,
            linked_children,
        }
    }
}

#[async_trait]
impl CollectionManager for HermitCollectionManager {
    async fn create_collection(
        &self,
        options: &CollectionCreationOptions,
    ) -> Result<BaseItemEntity, ServiceError> {
        let id = Uuid::new_v4();
        let row =
            insert_named_item(&self.db, id, BaseItemKind::BoxSet, &options.name, true).await?;
        for item_id in &options.item_id_list {
            self.linked_children
                .upsert_linked_child(id, *item_id, LINKED_CHILD_MANUAL)
                .await?;
        }
        Ok(row)
    }

    async fn add_to_collection(
        &self,
        collection_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        for item_id in item_ids {
            self.linked_children
                .upsert_linked_child(collection_id, *item_id, LINKED_CHILD_MANUAL)
                .await?;
        }
        Ok(())
    }

    async fn remove_from_collection(
        &self,
        collection_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        for item_id in item_ids {
            sqlx::query(
                r#"DELETE FROM "LinkedChildren"
                   WHERE "ParentId" = ?1 AND "ChildId" = ?2"#,
            )
            .bind(collection_id.to_string())
            .bind(item_id.to_string())
            .execute(self.db.pool())
            .await
            .map_err(db_err)?;
        }
        Ok(())
    }

    async fn get_collections_containing_item(
        &self,
        _user_id: Uuid,
        item_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // Parents of `item_id` that are box sets (per-user visibility filtering
        // is a deferred parental-control concern).
        let parent_ids = self
            .linked_children
            .get_manual_linked_parent_ids(item_id, Some(BaseItemKind::BoxSet))
            .await?;
        let mut out = Vec::new();
        for parent_id in parent_ids {
            if let Some(row) = self.library_manager.get_item_by_id(parent_id).await? {
                out.push(row);
            }
        }
        Ok(out)
    }

    async fn get_collections_folder(
        &self,
        _create_if_needed: bool,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        // Resolving/creating the "Collections" library folder needs the user-view
        // tree (documented deferral).
        Ok(None)
    }
}

/// The concrete (minimal) playlist manager.
#[derive(Clone)]
pub struct HermitPlaylistManager {
    db: Database,
    library_manager: Arc<dyn LibraryManager>,
    linked_children: Arc<dyn LinkedChildrenService>,
}

impl std::fmt::Debug for HermitPlaylistManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitPlaylistManager")
            .finish_non_exhaustive()
    }
}

impl HermitPlaylistManager {
    /// Creates a playlist manager from its injected collaborators.
    #[must_use]
    pub fn new(
        db: Database,
        library_manager: Arc<dyn LibraryManager>,
        linked_children: Arc<dyn LinkedChildrenService>,
    ) -> Self {
        Self {
            db,
            library_manager,
            linked_children,
        }
    }

    /// Loads a playlist row or errors with [`ServiceError::NotFound`].
    async fn require_playlist(&self, id: Uuid) -> Result<BaseItemEntity, ServiceError> {
        self.library_manager
            .get_item_by_id(id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("playlist {id}")))
    }
}

#[async_trait]
impl PlaylistManager for HermitPlaylistManager {
    async fn get_playlist_for_user(
        &self,
        playlist_id: Uuid,
        _user_id: Uuid,
    ) -> Result<BaseItemEntity, ServiceError> {
        // Per-user share visibility is a deferred concern; the row is returned
        // as-is once it exists.
        self.require_playlist(playlist_id).await
    }

    async fn create_playlist(
        &self,
        request: &PlaylistCreationRequest,
    ) -> Result<PlaylistCreationResult, ServiceError> {
        let id = Uuid::new_v4();
        let name = request.name.clone().unwrap_or_default();
        insert_named_item(&self.db, id, BaseItemKind::Playlist, &name, true).await?;
        for item_id in &request.item_id_list {
            self.linked_children
                .upsert_linked_child(id, *item_id, LINKED_CHILD_MANUAL)
                .await?;
        }
        Ok(PlaylistCreationResult { id: id.to_string() })
    }

    async fn update_playlist(&self, request: &PlaylistUpdateRequest) -> Result<(), ServiceError> {
        // Ensure it exists, then apply the name (the only field this leaf row
        // carries); membership changes arrive through `add_item_to_playlist`.
        self.require_playlist(request.id).await?;
        if let Some(name) = &request.name {
            sqlx::query(r#"UPDATE "BaseItems" SET "Name" = ?2 WHERE "Id" = ?1"#)
                .bind(request.id.to_string())
                .bind(name)
                .execute(self.db.pool())
                .await
                .map_err(db_err)?;
        }
        if let Some(ids) = &request.ids {
            for item_id in ids {
                self.linked_children
                    .upsert_linked_child(request.id, *item_id, LINKED_CHILD_MANUAL)
                    .await?;
            }
        }
        Ok(())
    }

    async fn get_playlists(&self, _user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let type_name = stored_type_name(BaseItemKind::Playlist)
            .ok_or_else(|| ServiceError::backend("no stored type name for Playlist"))?;
        let rows = sqlx::query_as::<_, BaseItemEntity>(
            r#"SELECT * FROM "BaseItems" WHERE "Type" = ?1 ORDER BY "Name""#,
        )
        .bind(type_name)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows)
    }

    async fn add_user_to_shares(
        &self,
        _request: &PlaylistUserUpdateRequest,
    ) -> Result<(), ServiceError> {
        // Playlist share persistence is deferred (see module docs).
        Ok(())
    }

    async fn remove_user_from_shares(
        &self,
        _playlist_id: Uuid,
        _user_id: Uuid,
        _share: &PlaylistUserPermissions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn add_item_to_playlist(
        &self,
        playlist_id: Uuid,
        item_ids: &[Uuid],
        _position: Option<i32>,
        _user_id: Uuid,
    ) -> Result<(), ServiceError> {
        // Explicit-position insertion is deferred (linked-children carry no
        // ordinal in this leaf schema); items are appended.
        for item_id in item_ids {
            self.linked_children
                .upsert_linked_child(playlist_id, *item_id, LINKED_CHILD_MANUAL)
                .await?;
        }
        Ok(())
    }

    async fn remove_item_from_playlist(
        &self,
        playlist_id: &str,
        entry_ids: &[String],
    ) -> Result<(), ServiceError> {
        // Entry ids are approximated by the child item id (see module docs).
        for entry_id in entry_ids {
            sqlx::query(
                r#"DELETE FROM "LinkedChildren"
                   WHERE "ParentId" = ?1 AND "ChildId" = ?2"#,
            )
            .bind(playlist_id)
            .bind(entry_id)
            .execute(self.db.pool())
            .await
            .map_err(db_err)?;
        }
        Ok(())
    }

    async fn move_item(
        &self,
        _playlist_id: &str,
        _entry_id: &str,
        _new_index: i32,
        _calling_user_id: Uuid,
    ) -> Result<(), ServiceError> {
        // Reordering needs an ordinal column the leaf schema lacks (deferred).
        Ok(())
    }

    async fn remove_playlists(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        // Owner tracking (and share transfer) is deferred; nothing is removed.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hermit_model::data::BaseItemKind;
    use hermit_model::playlists::{PlaylistCreationRequest, PlaylistUpdateRequest};
    use uuid::Uuid;

    use hermit_traits::collections::{
        CollectionCreationOptions, CollectionManager, PlaylistManager,
    };

    use crate::linked_children_service::HermitLinkedChildrenService;
    use crate::test_support::{library_manager_over, seed_item, test_db};

    use super::{HermitCollectionManager, HermitPlaylistManager};

    #[tokio::test]
    async fn create_collection_persists_row_and_members() {
        let db = test_db().await;
        let movie_a = Uuid::new_v4();
        let movie_b = Uuid::new_v4();
        seed_item(&db, movie_a, BaseItemKind::Movie).await;
        seed_item(&db, movie_b, BaseItemKind::Movie).await;

        let mgr = HermitCollectionManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(HermitLinkedChildrenService::new(db.clone())),
        );

        let options = CollectionCreationOptions {
            name: "Favourites".to_owned(),
            item_id_list: vec![movie_a],
            ..CollectionCreationOptions::default()
        };
        let created = mgr.create_collection(&options).await.expect("create");
        assert_eq!(created.name.as_deref(), Some("Favourites"));
        let collection_id = Uuid::parse_str(&created.id).expect("uuid");

        mgr.add_to_collection(collection_id, &[movie_b])
            .await
            .expect("add");

        let containing = mgr
            .get_collections_containing_item(Uuid::new_v4(), movie_b)
            .await
            .expect("containing");
        assert_eq!(containing.len(), 1);
        assert_eq!(containing[0].id, created.id);

        mgr.remove_from_collection(collection_id, &[movie_b])
            .await
            .expect("remove");
        let containing = mgr
            .get_collections_containing_item(Uuid::new_v4(), movie_b)
            .await
            .expect("containing2");
        assert!(containing.is_empty());
    }

    #[tokio::test]
    async fn create_and_update_playlist() {
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;

        let mgr = HermitPlaylistManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(HermitLinkedChildrenService::new(db.clone())),
        );

        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Roadtrip".to_owned()),
                item_id_list: vec![track],
                user_id: Uuid::new_v4(),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        let row = mgr
            .get_playlist_for_user(playlist_id, Uuid::new_v4())
            .await
            .expect("get");
        assert_eq!(row.name.as_deref(), Some("Roadtrip"));

        mgr.update_playlist(&PlaylistUpdateRequest {
            id: playlist_id,
            name: Some("Roadtrip 2".to_owned()),
            ..PlaylistUpdateRequest::default()
        })
        .await
        .expect("update");
        let row = mgr
            .get_playlist_for_user(playlist_id, Uuid::new_v4())
            .await
            .expect("get2");
        assert_eq!(row.name.as_deref(), Some("Roadtrip 2"));

        let all = mgr.get_playlists(Uuid::new_v4()).await.expect("list");
        assert_eq!(all.len(), 1);

        // Removing the sole member leaves the playlist row intact.
        mgr.remove_item_from_playlist(&created.id, &[track.to_string()])
            .await
            .expect("remove");
        assert_eq!(
            mgr.get_playlists(Uuid::new_v4())
                .await
                .expect("list2")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn get_missing_playlist_is_not_found() {
        let db = test_db().await;
        let mgr = HermitPlaylistManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(HermitLinkedChildrenService::new(db.clone())),
        );
        let err = mgr
            .get_playlist_for_user(Uuid::new_v4(), Uuid::new_v4())
            .await
            .expect_err("missing");
        assert!(matches!(
            err,
            hermit_traits::error::ServiceError::NotFound(_)
        ));
    }
}
