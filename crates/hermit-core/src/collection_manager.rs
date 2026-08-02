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
//! Playlist **ownership and access** are real: the `Playlists` table records the
//! owner + open-access flag (C# `Playlist.OwnerUserId`/`OpenAccess`), the
//! `PlaylistShares` table records per-user share permissions (`Playlist.Shares`),
//! and every read resolves the caller's [`PlaylistAccess`] — owner, `CanEdit`
//! share, read-only share, or open-access — with invisible playlists reported as
//! `NotFound` (the C# `GetPlaylistForUser` null). A playlist with no `Playlists`
//! row (created before owner tracking, or by an API key) grants `Owner` to every
//! caller for back-compat.
//!
//! Deferred (documented, per the unit-8 minimal-manager rule):
//! - the on-disk `.m3u`/`.pls` *playlist file* writes (`SavePlaylistFile`) — a
//!   filesystem concern folded away from the trait; not performed here;
//!   ordering therefore lives purely in the linked-children rows;
//!   entry-id addressing (`remove_item_from_playlist`/`move_item` take opaque
//!   entry-id strings) is approximated by the child item id;
//! - `remove_playlists` (owned-playlist deletion + share transfer on user
//!   removal) — ownership is recorded now, but the user-deletion cascade is not
//!   wired;
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

use hermit_traits::collections::{
    CollectionCreationOptions, CollectionManager, PlaylistAccess, PlaylistAccessLevel,
    PlaylistManager,
};
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

    /// Resolves the caller's access to a playlist (C# `Playlist.IsVisible` plus
    /// the owner/`CanEdit` split the controller checks derive from it).
    ///
    /// One LEFT-JOIN query: the base row proves existence, the `Playlists` meta
    /// row carries owner/open-access, and the caller's own `PlaylistShares` row
    /// (if any) carries `CanEdit`. Missing **or invisible** → `NotFound`.
    async fn access(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
    ) -> Result<PlaylistAccess, ServiceError> {
        let row: Option<(Option<String>, Option<i64>, Option<i64>)> = sqlx::query_as(
            r#"SELECT p."OwnerUserId", p."OpenAccess", s."CanEdit"
               FROM "BaseItems" bi
               LEFT JOIN "Playlists" p ON p."PlaylistId" = bi."Id"
               LEFT JOIN "PlaylistShares" s ON s."PlaylistId" = bi."Id" AND s."UserId" = ?2
               WHERE bi."Id" = ?1"#,
        )
        .bind(playlist_id.to_string())
        .bind(user_id.to_string())
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        let not_found = || ServiceError::not_found(format!("playlist {playlist_id}"));
        let (owner, open_access, can_edit) = row.ok_or_else(not_found)?;
        // `open_access` is NULL only when the meta row is absent — a legacy
        // playlist predating owner tracking: every caller is owner-equivalent.
        let Some(open_access) = open_access else {
            return Ok(PlaylistAccess {
                level: PlaylistAccessLevel::Owner,
                open_access: false,
            });
        };
        let open_access = open_access != 0;
        // A NULL owner in an existing meta row is an API-key playlist: no user
        // owns it, so — like a legacy row — every caller is owner-equivalent.
        let level = if owner.is_none() || owner.as_deref() == Some(user_id.to_string().as_str()) {
            PlaylistAccessLevel::Owner
        } else if let Some(can_edit) = can_edit {
            if can_edit != 0 {
                PlaylistAccessLevel::CanEdit
            } else {
                PlaylistAccessLevel::Read
            }
        } else if open_access {
            PlaylistAccessLevel::Read
        } else {
            return Err(not_found());
        };
        Ok(PlaylistAccess { level, open_access })
    }
}

#[async_trait]
impl PlaylistManager for HermitPlaylistManager {
    async fn get_playlist_access(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
    ) -> Result<PlaylistAccess, ServiceError> {
        self.access(playlist_id, user_id).await
    }

    async fn get_playlist_for_user(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
    ) -> Result<BaseItemEntity, ServiceError> {
        // Visibility gate first: invisible reads as missing (the C#
        // `GetPlaylistForUser` returns null for both).
        self.access(playlist_id, user_id).await?;
        self.require_playlist(playlist_id).await
    }

    async fn create_playlist(
        &self,
        request: &PlaylistCreationRequest,
    ) -> Result<PlaylistCreationResult, ServiceError> {
        let id = Uuid::new_v4();
        let name = request.name.clone().unwrap_or_default();
        insert_named_item(&self.db, id, BaseItemKind::Playlist, &name, true).await?;
        // The ownership meta row (C# `OwnerUserId`/`OpenAccess`). A nil user id
        // (API-key create) stores NULL — unowned, visible to all.
        sqlx::query(
            r#"INSERT INTO "Playlists" ("PlaylistId", "OwnerUserId", "OpenAccess")
               VALUES (?1, ?2, ?3)"#,
        )
        .bind(id.to_string())
        .bind((!request.user_id.is_nil()).then(|| request.user_id.to_string()))
        .bind(i64::from(request.public.unwrap_or(false)))
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;
        // Seed the share list from the request (C# `Shares = options.Users`).
        for share in &request.users {
            self.add_user_to_shares(&PlaylistUserUpdateRequest {
                id,
                user_id: share.user_id,
                can_edit: Some(share.can_edit),
            })
            .await?;
        }
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
        if let Some(public) = request.public {
            // A legacy playlist has no meta row; updating 0 rows is harmless.
            sqlx::query(r#"UPDATE "Playlists" SET "OpenAccess" = ?2 WHERE "PlaylistId" = ?1"#)
                .bind(request.id.to_string())
                .bind(i64::from(public))
                .execute(self.db.pool())
                .await
                .map_err(db_err)?;
        }
        if let Some(users) = &request.users {
            // C# replaces `Shares` wholesale on update.
            sqlx::query(r#"DELETE FROM "PlaylistShares" WHERE "PlaylistId" = ?1"#)
                .bind(request.id.to_string())
                .execute(self.db.pool())
                .await
                .map_err(db_err)?;
            for share in users {
                self.add_user_to_shares(&PlaylistUserUpdateRequest {
                    id: request.id,
                    user_id: share.user_id,
                    can_edit: Some(share.can_edit),
                })
                .await?;
            }
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

    async fn get_playlists(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let type_name = stored_type_name(BaseItemKind::Playlist)
            .ok_or_else(|| ServiceError::backend("no stored type name for Playlist"))?;
        // Visibility predicate (C# `Playlist.IsVisible`): legacy/unowned rows,
        // open-access, owned by the caller, or shared with the caller.
        let rows = sqlx::query_as::<_, BaseItemEntity>(
            r#"SELECT bi.* FROM "BaseItems" bi
               LEFT JOIN "Playlists" p ON p."PlaylistId" = bi."Id"
               WHERE bi."Type" = ?1 AND (
                   p."PlaylistId" IS NULL
                   OR p."OwnerUserId" IS NULL
                   OR p."OpenAccess" = 1
                   OR p."OwnerUserId" = ?2
                   OR EXISTS (SELECT 1 FROM "PlaylistShares" s
                              WHERE s."PlaylistId" = bi."Id" AND s."UserId" = ?2))
               ORDER BY bi."Name""#,
        )
        .bind(type_name)
        .bind(user_id.to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows)
    }

    async fn get_playlist_items(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // Visibility gate (invisible → NotFound); the C# `Forbid()` branch is
        // unreachable here because visible ⇔ permitted-to-read. Per-member
        // parental-control filtering stays deferred: members return in link order.
        self.access(playlist_id, user_id).await?;
        let child_ids = self
            .linked_children
            .get_linked_children_ids(playlist_id, Some(LINKED_CHILD_MANUAL))
            .await?;
        let mut out = Vec::with_capacity(child_ids.len());
        for child_id in child_ids {
            if let Some(row) = self.library_manager.get_item_by_id(child_id).await? {
                out.push(row);
            }
        }
        Ok(out)
    }

    async fn add_user_to_shares(
        &self,
        request: &PlaylistUserUpdateRequest,
    ) -> Result<(), ServiceError> {
        // Upsert the share row (POST is idempotent — re-sharing updates CanEdit).
        sqlx::query(
            r#"INSERT INTO "PlaylistShares" ("PlaylistId", "UserId", "CanEdit") VALUES (?1, ?2, ?3)
               ON CONFLICT ("PlaylistId", "UserId") DO UPDATE SET "CanEdit" = excluded."CanEdit""#,
        )
        .bind(request.id.to_string())
        .bind(request.user_id.to_string())
        .bind(i64::from(request.can_edit.unwrap_or(false)))
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn remove_user_from_shares(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
        _share: &PlaylistUserPermissions,
    ) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "PlaylistShares" WHERE "PlaylistId" = ?1 AND "UserId" = ?2"#)
            .bind(playlist_id.to_string())
            .bind(user_id.to_string())
            .execute(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn get_playlist_shares(
        &self,
        playlist_id: Uuid,
    ) -> Result<Vec<PlaylistUserPermissions>, ServiceError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT "UserId", "CanEdit" FROM "PlaylistShares"
               WHERE "PlaylistId" = ?1 ORDER BY "UserId""#,
        )
        .bind(playlist_id.to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|(uid, can_edit)| {
                Uuid::parse_str(&uid)
                    .ok()
                    .map(|user_id| PlaylistUserPermissions {
                        user_id,
                        can_edit: can_edit != 0,
                    })
            })
            .collect())
    }

    async fn add_item_to_playlist(
        &self,
        playlist_id: Uuid,
        item_ids: &[Uuid],
        position: Option<i32>,
        _user_id: Uuid,
    ) -> Result<(), ServiceError> {
        // Append each item (upsert assigns `SortOrder = max + 1`).
        for item_id in item_ids {
            self.linked_children
                .upsert_linked_child(playlist_id, *item_id, LINKED_CHILD_MANUAL)
                .await?;
        }
        // If a position was requested, relocate the just-appended items there.
        let Some(pos) = position else {
            return Ok(());
        };
        let pid = playlist_id.to_string();
        let added: Vec<String> = item_ids.iter().map(Uuid::to_string).collect();
        let mut order: Vec<String> = sqlx::query_scalar(
            r#"SELECT "ChildId" FROM "LinkedChildren"
               WHERE "ParentId" = ?1 ORDER BY "SortOrder""#,
        )
        .bind(&pid)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        // Pull the added items out (they were appended), then re-insert as a block
        // at the clamped target index, preserving their given order.
        order.retain(|c| !added.contains(c));
        let target = usize::try_from(pos.max(0))
            .unwrap_or(usize::MAX)
            .min(order.len());
        for (offset, child) in added.iter().enumerate() {
            let at = (target + offset).min(order.len());
            order.insert(at, child.clone());
        }
        write_sort_order(self.db.pool(), &pid, &order).await
    }

    async fn remove_item_from_playlist(
        &self,
        playlist_id: &str,
        entry_ids: &[String],
    ) -> Result<(), ServiceError> {
        // Entry ids are approximated by the child item id (see module docs). The
        // wire form is a dashless `Guid('N')` (`GetPlaylistItems` emits
        // `item.id.replace('-', "")`), but `ChildId` is stored dashed. Parse and
        // re-`to_string()` to normalise back to the stored dashed form; skip any
        // entry id that isn't a valid GUID.
        for entry_id in entry_ids {
            let Ok(child) = Uuid::parse_str(entry_id) else {
                continue;
            };
            sqlx::query(
                r#"DELETE FROM "LinkedChildren"
                   WHERE "ParentId" = ?1 AND "ChildId" = ?2"#,
            )
            .bind(playlist_id)
            .bind(child.to_string())
            .execute(self.db.pool())
            .await
            .map_err(db_err)?;
        }
        Ok(())
    }

    async fn move_item(
        &self,
        playlist_id: &str,
        entry_id: &str,
        new_index: i32,
        _calling_user_id: Uuid,
    ) -> Result<(), ServiceError> {
        // Port of `Playlist.MoveItem`: pull the current order, relocate the entry,
        // then rewrite the `SortOrder` ordinals. `entry_id` is the `ChildId` (as
        // `remove_item_from_playlist` treats it).
        // Same ordering the read path uses (`get_linked_children_ids`), so the
        // relocation matches what the client sees.
        let mut order: Vec<String> = sqlx::query_scalar(
            r#"SELECT "ChildId" FROM "LinkedChildren"
               WHERE "ParentId" = ?1 ORDER BY "SortOrder""#,
        )
        .bind(playlist_id)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        // `entry_id` arrives dashless (`Guid('N')`); `ChildId` is stored dashed.
        // Normalise before locating the entry position (see
        // `remove_item_from_playlist`).
        let Ok(child_id) = Uuid::parse_str(entry_id) else {
            return Ok(()); // not a GUID — nothing to move
        };
        let needle = child_id.to_string();
        let Some(pos) = order.iter().position(|c| *c == needle) else {
            return Ok(()); // entry not in the playlist — nothing to move
        };
        let child = order.remove(pos);
        // Clamp to the valid range of the now-shorter list (C# `Math.Clamp`).
        let target = usize::try_from(new_index.max(0))
            .unwrap_or(usize::MAX)
            .min(order.len());
        order.insert(target, child);
        write_sort_order(self.db.pool(), playlist_id, &order).await
    }

    async fn remove_playlists(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        // Ownership is recorded (the `Playlists` table), but the user-deletion
        // cascade — delete owned playlists, transfer shared ones (C#
        // `RemovePlaylists`) — is still deferred; nothing is removed.
        Ok(())
    }
}

/// Rewrites the `SortOrder` ordinals (`0,1,2,…`) for a playlist's children in the
/// given order, in one transaction. Shared by add-at-position and move.
async fn write_sort_order(
    pool: &sqlx::SqlitePool,
    playlist_id: &str,
    order: &[String],
) -> Result<(), ServiceError> {
    let mut tx = pool.begin().await.map_err(db_err)?;
    for (ordinal, child) in order.iter().enumerate() {
        sqlx::query(
            r#"UPDATE "LinkedChildren" SET "SortOrder" = ?1
               WHERE "ParentId" = ?2 AND "ChildId" = ?3"#,
        )
        .bind(i64::try_from(ordinal).unwrap_or(i64::MAX))
        .bind(playlist_id)
        .bind(child)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    tx.commit().await.map_err(db_err)
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
    async fn collection_members_visible_via_parentid_browse() {
        // Live-path repro (WI-6): create a collection with one member, add a second,
        // then browse it the way GET /Items?parentId=<collection> does —
        // library.query_items -> item_repository.get_items -> translate_query.
        // Both members must surface (C# Folder.GetChildren merges LinkedChildren).
        use hermit_traits::options::InternalItemsQuery;
        let db = test_db().await;
        let movie_a = Uuid::new_v4();
        let movie_b = Uuid::new_v4();
        seed_item(&db, movie_a, BaseItemKind::Movie).await;
        seed_item(&db, movie_b, BaseItemKind::Movie).await;

        let library = library_manager_over(db.clone());
        let mgr = HermitCollectionManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(HermitLinkedChildrenService::new(db.clone())),
        );
        let created = mgr
            .create_collection(&CollectionCreationOptions {
                name: "Watchlist".to_owned(),
                item_id_list: vec![movie_a],
                ..CollectionCreationOptions::default()
            })
            .await
            .expect("create");
        let cid = Uuid::parse_str(&created.id).expect("uuid");
        mgr.add_to_collection(cid, &[movie_b]).await.expect("add");

        let result = library
            .query_items(&InternalItemsQuery {
                parent_id: cid,
                ..InternalItemsQuery::default()
            })
            .await
            .expect("browse");
        let ids: Vec<_> = result.items.iter().map(|i| i.id.clone()).collect();
        assert!(
            ids.contains(&movie_a.to_string()),
            "movie_a missing: {ids:?}"
        );
        assert!(
            ids.contains(&movie_b.to_string()),
            "movie_b missing: {ids:?}"
        );
        assert_eq!(result.total_record_count, 2);
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

        // Reads below must come from the owner: playlists are visible only to
        // their owner / shared users now.
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Roadtrip".to_owned()),
                item_id_list: vec![track],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        let row = mgr
            .get_playlist_for_user(playlist_id, owner)
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
            .get_playlist_for_user(playlist_id, owner)
            .await
            .expect("get2");
        assert_eq!(row.name.as_deref(), Some("Roadtrip 2"));

        let all = mgr.get_playlists(owner).await.expect("list");
        assert_eq!(all.len(), 1);

        // Removing the sole member leaves the playlist row intact.
        mgr.remove_item_from_playlist(&created.id, &[track.to_string()])
            .await
            .expect("remove");
        assert_eq!(mgr.get_playlists(owner).await.expect("list2").len(), 1);
    }

    #[tokio::test]
    async fn get_playlist_items_returns_members() {
        let db = test_db().await;
        let track_a = Uuid::new_v4();
        let track_b = Uuid::new_v4();
        seed_item(&db, track_a, BaseItemKind::Audio).await;
        seed_item(&db, track_b, BaseItemKind::Audio).await;

        let mgr = HermitPlaylistManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(HermitLinkedChildrenService::new(db.clone())),
        );

        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Mix".to_owned()),
                item_id_list: vec![track_a, track_b],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        let items = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items");
        assert_eq!(items.len(), 2);
        let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
        assert!(ids.contains(&track_a.to_string()));
        assert!(ids.contains(&track_b.to_string()));
    }

    #[tokio::test]
    async fn remove_by_dashless_playlist_item_id_removes_member() {
        // `GetPlaylistItems` emits `PlaylistItemId` dashless (`Guid('N')`), but
        // `ChildId` is stored dashed. Removing by the dashless id the DTO exposes
        // must still hit the row (regression: it matched zero rows before).
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;

        let mgr = HermitPlaylistManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(HermitLinkedChildrenService::new(db.clone())),
        );

        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Mix".to_owned()),
                item_id_list: vec![track],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        // Read the entry id exactly as the wire DTO exposes it: dashless.
        let items = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items");
        assert_eq!(items.len(), 1);
        let dashless = items[0].id.replace('-', "");
        assert!(!dashless.contains('-'));

        mgr.remove_item_from_playlist(&created.id, &[dashless])
            .await
            .expect("remove");

        let after = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items after");
        assert!(
            after.is_empty(),
            "removing by dashless PlaylistItemId should empty the playlist"
        );
    }

    #[tokio::test]
    async fn add_item_at_position_relocates_appended_block() {
        let db = test_db().await;
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for id in [a, b, c] {
            seed_item(&db, id, BaseItemKind::Audio).await;
        }
        let mgr = HermitPlaylistManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(HermitLinkedChildrenService::new(db.clone())),
        );
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Ordered".to_owned()),
                item_id_list: vec![a, b],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        // Insert C at the front: expect [C, A, B].
        mgr.add_item_to_playlist(playlist_id, &[c], Some(0), owner)
            .await
            .expect("add at 0");
        let order: Vec<String> = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items")
            .iter()
            .map(|i| i.id.clone())
            .collect();
        assert_eq!(
            order,
            vec![c.to_string(), a.to_string(), b.to_string()],
            "position 0 should move the appended item to the front"
        );

        // No position → plain append at the end.
        let d = Uuid::new_v4();
        seed_item(&db, d, BaseItemKind::Audio).await;
        mgr.add_item_to_playlist(playlist_id, &[d], None, owner)
            .await
            .expect("append");
        let last = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items2")
            .last()
            .expect("non-empty")
            .id
            .clone();
        assert_eq!(last, d.to_string(), "no position should append at the end");
    }

    #[tokio::test]
    async fn get_playlist_items_missing_is_not_found() {
        let db = test_db().await;
        let mgr = HermitPlaylistManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(HermitLinkedChildrenService::new(db.clone())),
        );
        let err = mgr
            .get_playlist_items(Uuid::new_v4(), Uuid::new_v4())
            .await
            .expect_err("missing");
        assert!(matches!(
            err,
            hermit_traits::error::ServiceError::NotFound(_)
        ));
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

    #[tokio::test]
    async fn playlist_shares_add_update_get_remove() {
        use hermit_model::entities_media::PlaylistUserPermissions;
        use hermit_model::playlists::PlaylistUserUpdateRequest;

        let db = test_db().await;
        let mgr = HermitPlaylistManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(HermitLinkedChildrenService::new(db.clone())),
        );
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Shared".to_owned()),
                user_id: Uuid::new_v4(),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");
        let bob = Uuid::new_v4();

        assert!(
            mgr.get_playlist_shares(playlist_id)
                .await
                .unwrap()
                .is_empty()
        );

        // Add a share.
        mgr.add_user_to_shares(&PlaylistUserUpdateRequest {
            id: playlist_id,
            user_id: bob,
            can_edit: Some(true),
        })
        .await
        .unwrap();
        let shares = mgr.get_playlist_shares(playlist_id).await.unwrap();
        assert_eq!(shares, vec![PlaylistUserPermissions::new(bob, true)]);

        // POST is idempotent and updates CanEdit.
        mgr.add_user_to_shares(&PlaylistUserUpdateRequest {
            id: playlist_id,
            user_id: bob,
            can_edit: Some(false),
        })
        .await
        .unwrap();
        let shares = mgr.get_playlist_shares(playlist_id).await.unwrap();
        assert_eq!(shares, vec![PlaylistUserPermissions::new(bob, false)]);

        // Remove it.
        mgr.remove_user_from_shares(playlist_id, bob, &PlaylistUserPermissions::new(bob, false))
            .await
            .unwrap();
        assert!(
            mgr.get_playlist_shares(playlist_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Builds a playlist manager over `db` (the repeated three-collaborator
    /// constructor the access tests share).
    fn playlist_manager(db: &hermit_db::Database) -> HermitPlaylistManager {
        HermitPlaylistManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(HermitLinkedChildrenService::new(db.clone())),
        )
    }

    #[tokio::test]
    async fn create_playlist_records_owner_and_seeds_shares() {
        use hermit_model::entities_media::PlaylistUserPermissions;
        use hermit_traits::collections::PlaylistAccessLevel;

        let db = test_db().await;
        let mgr = playlist_manager(&db);
        let (alice, bob, carol) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Owned".to_owned()),
                user_id: alice,
                users: vec![PlaylistUserPermissions::new(bob, true)],
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        let alice_access = mgr.get_playlist_access(playlist_id, alice).await.unwrap();
        assert_eq!(alice_access.level, PlaylistAccessLevel::Owner);
        assert!(!alice_access.open_access);
        let bob_access = mgr.get_playlist_access(playlist_id, bob).await.unwrap();
        assert_eq!(bob_access.level, PlaylistAccessLevel::CanEdit);
        // The owner is not in the share list; the seeded share is.
        assert_eq!(
            mgr.get_playlist_shares(playlist_id).await.unwrap(),
            vec![PlaylistUserPermissions::new(bob, true)]
        );
        // A stranger cannot see the playlist at all.
        assert!(matches!(
            mgr.get_playlist_access(playlist_id, carol).await,
            Err(hermit_traits::error::ServiceError::NotFound(_))
        ));
        assert!(matches!(
            mgr.get_playlist_for_user(playlist_id, carol).await,
            Err(hermit_traits::error::ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn open_access_playlist_is_readable_by_anyone() {
        use hermit_traits::collections::PlaylistAccessLevel;

        let db = test_db().await;
        let mgr = playlist_manager(&db);
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Public".to_owned()),
                user_id: Uuid::new_v4(),
                public: Some(true),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");

        let stranger = mgr
            .get_playlist_access(playlist_id, Uuid::new_v4())
            .await
            .expect("open playlists are visible to all");
        assert_eq!(stranger.level, PlaylistAccessLevel::Read);
        assert!(stranger.open_access);
        assert!(!stranger.level.can_edit());
    }

    #[tokio::test]
    async fn legacy_playlist_without_meta_row_grants_owner_to_all() {
        use hermit_traits::collections::PlaylistAccessLevel;

        let db = test_db().await;
        let mgr = playlist_manager(&db);
        // A pre-ownership row: the BaseItems row exists with no `Playlists` meta.
        let legacy = Uuid::new_v4();
        seed_item(&db, legacy, BaseItemKind::Playlist).await;

        let access = mgr
            .get_playlist_access(legacy, Uuid::new_v4())
            .await
            .expect("legacy rows stay visible");
        assert_eq!(access.level, PlaylistAccessLevel::Owner);
        assert!(!access.open_access);
        assert_eq!(mgr.get_playlists(Uuid::new_v4()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_playlists_filters_by_visibility() {
        use hermit_model::entities_media::PlaylistUserPermissions;

        let db = test_db().await;
        let mgr = playlist_manager(&db);
        let (alice, bob, carol) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let create = |name: &str, owner: Uuid, users: Vec<PlaylistUserPermissions>| {
            PlaylistCreationRequest {
                name: Some(name.to_owned()),
                user_id: owner,
                users,
                ..PlaylistCreationRequest::default()
            }
        };
        let req = create("Alice's", alice, Vec::new());
        let p1 = mgr.create_playlist(&req).await.expect("p1").id;
        let p2 = mgr
            .create_playlist(&create(
                "Bob's shared",
                bob,
                vec![PlaylistUserPermissions::new(alice, false)],
            ))
            .await
            .expect("p2")
            .id;
        mgr.create_playlist(&create("Carol's", carol, Vec::new()))
            .await
            .expect("p3");

        let visible: Vec<String> = mgr
            .get_playlists(alice)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(visible.len(), 2, "alice sees her own + bob's shared");
        assert!(visible.contains(&p1) && visible.contains(&p2));
    }

    #[tokio::test]
    async fn update_playlist_applies_public_and_replaces_shares() {
        use hermit_model::entities_media::PlaylistUserPermissions;
        use hermit_traits::collections::PlaylistAccessLevel;

        let db = test_db().await;
        let mgr = playlist_manager(&db);
        let (alice, bob, dave) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Mutable".to_owned()),
                user_id: alice,
                users: vec![PlaylistUserPermissions::new(bob, true)],
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");

        mgr.update_playlist(&PlaylistUpdateRequest {
            id: playlist_id,
            user_id: alice,
            users: Some(vec![PlaylistUserPermissions::new(dave, false)]),
            public: Some(true),
            ..PlaylistUpdateRequest::default()
        })
        .await
        .expect("update");

        // The share list was replaced wholesale (bob is gone, dave read-only).
        assert_eq!(
            mgr.get_playlist_shares(playlist_id).await.unwrap(),
            vec![PlaylistUserPermissions::new(dave, false)]
        );
        // The open-access flag applied: a stranger can now read.
        let stranger = mgr
            .get_playlist_access(playlist_id, Uuid::new_v4())
            .await
            .expect("now public");
        assert_eq!(stranger.level, PlaylistAccessLevel::Read);
        assert!(stranger.open_access);
    }
}
