//! [`FerrofinCollectionManager`] + [`FerrofinPlaylistManager`] — **minimal**
//! [`CollectionManager`]/[`PlaylistManager`] impls.
//!
//! Ports of `Emby.Server.Implementations.Collections.CollectionManager` and
//! `Emby.Server.Implementations.Playlists.PlaylistManager`. Both are thin unit-8
//! managers: a box set and a playlist are both `BaseItem` rows, so creating one
//! inserts a minimal `BaseItems` row (of the `BoxSet`/`Playlist` stored type)
//! through `ferrofin-db`, and membership is recorded as manual **linked children**
//! via the injected
//! [`LinkedChildrenService`](ferrofin_traits::persistence::LinkedChildrenService).
//! Reads go through the injected
//! [`LibraryManager`](ferrofin_traits::library::LibraryManager) so the same
//! persistence seam backs everything.
//!
//! Playlist **ownership and access** are real: the `FerrofinPlaylists` table records the
//! owner + open-access flag (C# `Playlist.OwnerUserId`/`OpenAccess`), the
//! `FerrofinPlaylistShares` table records per-user share permissions (`Playlist.Shares`),
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
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities_media::PlaylistUserPermissions;
use ferrofin_model::playlists::{
    PlaylistCreationRequest, PlaylistCreationResult, PlaylistUpdateRequest,
    PlaylistUserUpdateRequest,
};
use uuid::Uuid;

use ferrofin_traits::collections::{
    CollectionCreationOptions, CollectionManager, PlaylistAccess, PlaylistAccessLevel,
    PlaylistManager,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::persistence::{ItemRepository, LinkedChildrenService};
use ferrofin_traits::system::ServerApplicationPaths;

use crate::db_error::db_err;
use crate::item_data;
use crate::item_type_lookup::stored_type_name;

/// The manual [`LinkedChildType`](ferrofin_db::enums::LinkedChildType) discriminant
/// (`0`) used to record a box-set/playlist member.
const LINKED_CHILD_MANUAL: i32 = 0;

/// Decides the caller's [`PlaylistAccess`] from the three joined access columns
/// (C# `Playlist.IsVisible` plus the owner/`CanEdit` split the controller checks
/// derive from it). `None` means invisible — callers report `NotFound`.
///
/// Shared by the access query and the joined items read so both paths can never
/// drift apart.
fn decide_access(
    owner: Option<&str>,
    open_access: Option<i64>,
    can_edit: Option<i64>,
    user_id: Uuid,
) -> Option<PlaylistAccess> {
    // `open_access` is NULL only when the meta row is absent — a legacy
    // playlist predating owner tracking: every caller is owner-equivalent.
    let Some(open_access) = open_access else {
        return Some(PlaylistAccess {
            level: PlaylistAccessLevel::Owner,
            open_access: false,
        });
    };
    let open_access = open_access != 0;
    // A NULL owner in an existing meta row is an API-key playlist: no user
    // owns it, so — like a legacy row — every caller is owner-equivalent.
    // Parse-compare the stored owner GUID so rows written in either case
    // (legacy lowercase, canonical uppercase) still match.
    let owner_is_caller = owner.and_then(|o| Uuid::parse_str(o).ok()) == Some(user_id);
    let level = if owner.is_none() || owner_is_caller {
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
        return None;
    };
    Some(PlaylistAccess { level, open_access })
}

/// The id derivation this database uses, and the user-root row a provisioned
/// container hangs off.
///
/// Both come from the database rather than a default: the derivation is
/// whatever `FerrofinMeta.item_id_derivation` says (an adopted or fresh
/// database is Jellyfin-parity, a pre-parity Ferrofin one is grandfathered),
/// and using the wrong one gives the container an id Jellyfin will not
/// recognise as its own.
async fn container_identity(
    db: &Database,
    paths: &Arc<dyn ServerApplicationPaths>,
) -> Result<(crate::item_type_lookup::IdDerivation, Option<Uuid>), ServiceError> {
    let stored = db
        .meta_get("item_id_derivation")
        .await
        .map_err(|e| ServiceError::Backend(e.to_string()))?;
    let mode = crate::item_type_lookup::IdDerivation::from_meta(
        stored.as_deref(),
        Some(paths.program_data_path()),
    );
    let root =
        crate::item_type_lookup::user_root_folder_id(&mode, &paths.default_user_views_path());
    Ok((mode, root))
}

/// The `Data` blob the auto-provisioned "Collections" library carries, and the
/// only key of it Ferrofin reads or writes.
///
/// 10.11.8 keeps a `CollectionFolder`'s `CollectionType` in that column
/// (`DtoService.AttachBasicFields` reads `IHasCollectionType.CollectionType`,
/// `CollectionFolder.cs:32`), and its own row for this library says
/// `"CollectionType":"boxsets"` because `CollectionManager.EnsureLibraryFolder`
/// creates it with `CollectionTypeOptions.boxsets`. Only written over an EMPTY
/// `Data` column — see `item_persistence_service::backfill_container_data` for
/// why an adopted database's richer blob must survive untouched.
const COLLECTIONS_FOLDER_DATA: &str = r#"{"CollectionType":"boxsets"}"#;

/// The "Collections" library a created box set belongs to, adopting any
/// orphans a previous version left behind.
async fn collections_folder(
    db: &Database,
    paths: &Arc<dyn ServerApplicationPaths>,
) -> Result<Option<Uuid>, ServiceError> {
    let path = format!("{}/collections", paths.data_path());
    let (mode, root) = container_identity(db, paths).await?;
    let container = crate::item_persistence_service::ensure_container(
        db,
        BaseItemKind::CollectionFolder,
        "Collections",
        &path,
        &mode,
        root,
        // Upstream provisions this library through
        // `AddVirtualFolder(name, CollectionTypeOptions.boxsets, …)`
        // (v10.11.8 CollectionManager.cs:81-109), which is what puts
        // `"CollectionType":"boxsets"` in the row's `Data` blob. Without it the
        // folder went out with a null `CollectionType` everywhere it is listed.
        Some(COLLECTIONS_FOLDER_DATA),
    )
    .await?;
    if let Some(id) = container {
        crate::item_persistence_service::adopt_orphans(db, BaseItemKind::BoxSet, id).await?;
    }
    Ok(container)
}

/// The playlists folder a created playlist belongs to, adopting any orphans a
/// previous version left behind.
async fn playlists_folder(
    db: &Database,
    paths: &Arc<dyn ServerApplicationPaths>,
) -> Result<Option<Uuid>, ServiceError> {
    // `PlaylistsFolder`, not `ManualPlaylistsFolder`: 10.11.8 has no class of
    // the latter name — it is only `PlaylistsFolder.GetClientTypeName()` — and
    // the row an adopted database carries is stored as
    // `…Playlists.PlaylistsFolder`. Asking for the other one would never find
    // Jellyfin's folder and would quietly create a second beside it.
    let path = format!("{}/playlists", paths.data_path());
    let (mode, _root) = container_identity(db, paths).await?;
    // The playlists folder's parent is the **AggregateFolder**, not the user
    // root: `LibraryManager.CreateRootFolder` sets `folder.ParentId =
    // rootFolder.Id` where `rootFolder` is the aggregate, and only
    // `AddVirtualChild` puts the folder among the user root's children
    // (LibraryManager.cs:855-885). Parenting it to the user root instead gives
    // the right `ChildCount` for the wrong reason and answers
    // `GET /Items/{playlistsId}/Ancestors` with the wrong row.
    let aggregate = crate::item_type_lookup::aggregate_folder_id(&mode, &paths.root_folder_path());
    let container = crate::item_persistence_service::ensure_container(
        db,
        BaseItemKind::PlaylistsFolder,
        "Playlists",
        &path,
        &mode,
        aggregate,
        // No `Data`: `PlaylistsFolder.CollectionType` is a constant on the type
        // (PlaylistsFolder.cs:29), not a persisted value — Jellyfin's own row
        // carries an empty `Data` column, and `collection_type_of` answers
        // `playlists` from the kind alone.
        None,
    )
    .await?;
    if let Some(id) = container {
        crate::item_persistence_service::adopt_orphans(db, BaseItemKind::Playlist, id).await?;
    }
    Ok(container)
}

/// The concrete (minimal) collection (box-set) manager.
#[derive(Clone)]
pub struct FerrofinCollectionManager {
    db: Database,
    library_manager: Arc<dyn LibraryManager>,
    linked_children: Arc<dyn LinkedChildrenService>,
    paths: Arc<dyn ServerApplicationPaths>,
}

impl std::fmt::Debug for FerrofinCollectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinCollectionManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinCollectionManager {
    /// Creates a collection manager from its injected collaborators.
    #[must_use]
    pub fn new(
        db: Database,
        library_manager: Arc<dyn LibraryManager>,
        linked_children: Arc<dyn LinkedChildrenService>,
        paths: Arc<dyn ServerApplicationPaths>,
    ) -> Self {
        Self {
            db,
            library_manager,
            linked_children,
            paths,
        }
    }
}

#[async_trait]
impl CollectionManager for FerrofinCollectionManager {
    async fn create_collection(
        &self,
        options: &CollectionCreationOptions,
    ) -> Result<BaseItemEntity, ServiceError> {
        let id = Uuid::new_v4();
        let row = crate::item_persistence_service::insert_named_item(
            &self.db,
            id,
            BaseItemKind::BoxSet,
            &options.name,
            true,
            collections_folder(&self.db, &self.paths).await?,
        )
        .await?;
        for item_id in &options.item_id_list {
            self.linked_children
                .upsert_linked_child(id, *item_id, LINKED_CHILD_MANUAL)
                .await?;
        }
        // Even an empty collection gets its Data blob (upserts sync per child,
        // but a zero-item create would otherwise leave Data without the
        // LinkedChildren key Jellyfin expects to own).
        item_data::sync_container_data(&self.db, id).await?;
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
                r#"DELETE FROM "FerrofinLinkedChildren"
                   WHERE "ParentId" = ?1 AND "ChildId" = ?2"#,
            )
            .bind(guid_to_db(collection_id))
            .bind(guid_to_db(*item_id))
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        }
        item_data::sync_container_data(&self.db, collection_id).await?;
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
        create_if_needed: bool,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        // Port of `CollectionManager.EnsureLibraryFolder`
        // (v10.11.8 Emby.Server.Implementations/Collections/CollectionManager.cs
        // :81-109): look the library up BY PATH, and provision it — directory,
        // row, boxsets collection type and all — only when the caller asked for
        // it. This used to answer `Ok(None)` unconditionally under a "documented
        // deferral" comment; the tree it was said to need is the same
        // `ensure_container` the create path has been using all along.
        let path = format!("{}/collections", self.paths.data_path());
        let id = if create_if_needed {
            collections_folder(&self.db, &self.paths).await?
        } else {
            crate::item_persistence_service::container_at(&self.db, &path).await?
        };
        let Some(id) = id else {
            return Ok(None);
        };
        crate::item_persistence_service::container_row(&self.db, id).await
    }
}

/// The concrete (minimal) playlist manager.
#[derive(Clone)]
pub struct FerrofinPlaylistManager {
    db: Database,
    library_manager: Arc<dyn LibraryManager>,
    linked_children: Arc<dyn LinkedChildrenService>,
    items: Arc<dyn ItemRepository>,
    paths: Arc<dyn ServerApplicationPaths>,
}

impl std::fmt::Debug for FerrofinPlaylistManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinPlaylistManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinPlaylistManager {
    /// Creates a playlist manager from its injected collaborators.
    #[must_use]
    pub fn new(
        db: Database,
        library_manager: Arc<dyn LibraryManager>,
        linked_children: Arc<dyn LinkedChildrenService>,
        items: Arc<dyn ItemRepository>,
        paths: Arc<dyn ServerApplicationPaths>,
    ) -> Self {
        Self {
            db,
            library_manager,
            linked_children,
            items,
            paths,
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
    /// One LEFT-JOIN query: the base row proves existence, the `FerrofinPlaylists` meta
    /// row carries owner/open-access, and the caller's own `FerrofinPlaylistShares` row
    /// (if any) carries `CanEdit`. Missing **or invisible** → `NotFound`.
    async fn access(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
    ) -> Result<PlaylistAccess, ServiceError> {
        let row: Option<(Option<String>, Option<i64>, Option<i64>)> = sqlx::query_as(
            r#"SELECT p."OwnerUserId", p."OpenAccess", s."CanEdit"
               FROM "BaseItems" bi
               LEFT JOIN "FerrofinPlaylists" p ON p."PlaylistId" = bi."Id"
               LEFT JOIN "FerrofinPlaylistShares" s ON s."PlaylistId" = bi."Id" AND s."UserId" = ?2
               WHERE bi."Id" = ?1"#,
        )
        .bind(guid_to_db(playlist_id))
        .bind(guid_to_db(user_id))
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        let not_found = || ServiceError::not_found(format!("playlist {playlist_id}"));
        let (owner, open_access, can_edit) = row.ok_or_else(not_found)?;
        decide_access(owner.as_deref(), open_access, can_edit, user_id).ok_or_else(not_found)
    }
}

#[async_trait]
impl PlaylistManager for FerrofinPlaylistManager {
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
        crate::item_persistence_service::insert_named_item(
            &self.db,
            id,
            BaseItemKind::Playlist,
            &name,
            true,
            playlists_folder(&self.db, &self.paths).await?,
        )
        .await?;
        // The ownership meta row (C# `OwnerUserId`/`OpenAccess`). A nil user id
        // (API-key create) stores NULL — unowned, visible to all.
        sqlx::query(
            r#"INSERT INTO "FerrofinPlaylists" ("PlaylistId", "OwnerUserId", "OpenAccess")
               VALUES (?1, ?2, ?3)"#,
        )
        .bind(guid_to_db(id))
        .bind((!request.user_id.is_nil()).then(|| guid_to_db(request.user_id)))
        .bind(i64::from(request.public.unwrap_or(false)))
        .execute(self.db.writer())
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
        // Write the full playlist state (owner/shares/children) into
        // BaseItems.Data — the 10.11.8 storage Jellyfin reads on a swap back.
        item_data::sync_container_data(&self.db, id).await?;
        // Jellyfin's Playlist carries PlaylistMediaType in Data; without it a
        // swap back would default the playlist to Audio.
        if let Some(media_type) = &request.media_type {
            // The enum's serde form IS Jellyfin's JSON string ("Video", …).
            let value = serde_json::to_value(media_type)
                .map_err(|e| ServiceError::Backend(format!("serialize MediaType: {e}")))?;
            item_data::set_data_key(&self.db, id, "PlaylistMediaType", value).await?;
        }
        Ok(PlaylistCreationResult { id: id.to_string() })
    }

    async fn update_playlist(&self, request: &PlaylistUpdateRequest) -> Result<(), ServiceError> {
        // Ensure it exists, then apply the name (the only field this leaf row
        // carries); membership changes arrive through `add_item_to_playlist`.
        self.require_playlist(request.id).await?;
        if let Some(name) = &request.name {
            sqlx::query(r#"UPDATE "BaseItems" SET "Name" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(request.id))
                .bind(name)
                .execute(self.db.writer())
                .await
                .map_err(db_err)?;
        }
        if let Some(public) = request.public {
            // A legacy playlist has no meta row; updating 0 rows is harmless.
            sqlx::query(
                r#"UPDATE "FerrofinPlaylists" SET "OpenAccess" = ?2 WHERE "PlaylistId" = ?1"#,
            )
            .bind(guid_to_db(request.id))
            .bind(i64::from(public))
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        }
        if let Some(users) = &request.users {
            // C# replaces `Shares` wholesale on update.
            sqlx::query(r#"DELETE FROM "FerrofinPlaylistShares" WHERE "PlaylistId" = ?1"#)
                .bind(guid_to_db(request.id))
                .execute(self.db.writer())
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
        item_data::sync_container_data(&self.db, request.id).await?;
        Ok(())
    }

    async fn get_playlists(&self, user_id: Uuid) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let type_name = stored_type_name(BaseItemKind::Playlist)
            .ok_or_else(|| ServiceError::backend("no stored type name for Playlist"))?;
        // Visibility predicate (C# `Playlist.IsVisible`): legacy/unowned rows,
        // open-access, owned by the caller, or shared with the caller.
        let rows = sqlx::query_as::<_, BaseItemEntity>(
            r#"SELECT bi.* FROM "BaseItems" bi
               LEFT JOIN "FerrofinPlaylists" p ON p."PlaylistId" = bi."Id"
               WHERE bi."Type" = ?1 AND (
                   p."PlaylistId" IS NULL
                   OR p."OwnerUserId" IS NULL
                   OR p."OpenAccess" = 1
                   OR p."OwnerUserId" = ?2
                   OR EXISTS (SELECT 1 FROM "FerrofinPlaylistShares" s
                              WHERE s."PlaylistId" = bi."Id" AND s."UserId" = ?2))
               ORDER BY bi."Name""#,
        )
        .bind(type_name)
        .bind(guid_to_db(user_id))
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
        // One round-trip through the repository seam: the member rows already
        // in link order (the same ordering `get_linked_children_ids` applies),
        // each carrying the caller's access columns, so the visibility gate
        // costs no extra query. The C# `Forbid()` branch is unreachable here
        // because visible ⇔ permitted-to-read; per-member parental-control
        // filtering stays deferred.
        let page = self
            .items
            .get_playlist_items_with_access(playlist_id, user_id, LINKED_CHILD_MANUAL)
            .await?;
        let Some(access) = page.access else {
            // No member rows: the join can't tell "empty playlist" from
            // "missing/invisible playlist", so resolve access on its own. Only
            // empty playlists pay for this second query.
            self.access(playlist_id, user_id).await?;
            return Ok(Vec::new());
        };
        // Every row repeats the same access columns; the first decides.
        if decide_access(
            access.owner_user_id.as_deref(),
            access.open_access,
            access.share_can_edit,
            user_id,
        )
        .is_none()
        {
            return Err(ServiceError::not_found(format!("playlist {playlist_id}")));
        }
        Ok(page.items)
    }

    async fn add_user_to_shares(
        &self,
        request: &PlaylistUserUpdateRequest,
    ) -> Result<(), ServiceError> {
        // Upsert the share row (POST is idempotent — re-sharing updates CanEdit).
        sqlx::query(
            r#"INSERT INTO "FerrofinPlaylistShares" ("PlaylistId", "UserId", "CanEdit") VALUES (?1, ?2, ?3)
               ON CONFLICT ("PlaylistId", "UserId") DO UPDATE SET "CanEdit" = excluded."CanEdit""#,
        )
        .bind(guid_to_db(request.id))
        .bind(guid_to_db(request.user_id))
        .bind(i64::from(request.can_edit.unwrap_or(false)))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        item_data::sync_container_data(&self.db, request.id).await?;
        Ok(())
    }

    async fn remove_user_from_shares(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
        _share: &PlaylistUserPermissions,
    ) -> Result<(), ServiceError> {
        sqlx::query(
            r#"DELETE FROM "FerrofinPlaylistShares" WHERE "PlaylistId" = ?1 AND "UserId" = ?2"#,
        )
        .bind(guid_to_db(playlist_id))
        .bind(guid_to_db(user_id))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        item_data::sync_container_data(&self.db, playlist_id).await?;
        Ok(())
    }

    async fn get_playlist_shares(
        &self,
        playlist_id: Uuid,
    ) -> Result<Vec<PlaylistUserPermissions>, ServiceError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT "UserId", "CanEdit" FROM "FerrofinPlaylistShares"
               WHERE "PlaylistId" = ?1 ORDER BY "UserId""#,
        )
        .bind(guid_to_db(playlist_id))
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
        let pid = guid_to_db(playlist_id);
        let added: Vec<String> = item_ids.iter().copied().map(guid_to_db).collect();
        let mut order: Vec<String> = sqlx::query_scalar(
            r#"SELECT "ChildId" FROM "FerrofinLinkedChildren"
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
        write_sort_order(self.db.writer(), &pid, &order).await?;
        item_data::sync_container_data(&self.db, playlist_id).await
    }

    async fn remove_item_from_playlist(
        &self,
        playlist_id: &str,
        entry_ids: &[String],
    ) -> Result<(), ServiceError> {
        // Entry ids are approximated by the child item id (see module docs). The
        // wire form is a dashless `Guid('N')` (`GetPlaylistItems` emits
        // `item.id.replace('-', "")`), but `ChildId` is stored in the canonical
        // dashed uppercase form. Parse and re-format via `guid_to_db` to
        // normalise back to the stored form (same for the playlist id, which
        // arrives as a caller-formatted string); skip any entry id that isn't a
        // valid GUID.
        let pid = Uuid::parse_str(playlist_id).map_or_else(|_| playlist_id.to_owned(), guid_to_db);
        for entry_id in entry_ids {
            let Ok(child) = Uuid::parse_str(entry_id) else {
                continue;
            };
            sqlx::query(
                r#"DELETE FROM "FerrofinLinkedChildren"
                   WHERE "ParentId" = ?1 AND "ChildId" = ?2"#,
            )
            .bind(&pid)
            .bind(guid_to_db(child))
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        }
        if let Ok(playlist) = Uuid::parse_str(playlist_id) {
            item_data::sync_container_data(&self.db, playlist).await?;
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
        // relocation matches what the client sees. The playlist id arrives as a
        // caller-formatted string — normalise to the stored canonical form.
        let pid = Uuid::parse_str(playlist_id).map_or_else(|_| playlist_id.to_owned(), guid_to_db);
        let mut order: Vec<String> = sqlx::query_scalar(
            r#"SELECT "ChildId" FROM "FerrofinLinkedChildren"
               WHERE "ParentId" = ?1 ORDER BY "SortOrder""#,
        )
        .bind(&pid)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        // `entry_id` arrives dashless (`Guid('N')`); `ChildId` is stored in the
        // canonical dashed uppercase form. Normalise before locating the entry
        // position (see `remove_item_from_playlist`).
        let Ok(child_id) = Uuid::parse_str(entry_id) else {
            return Ok(()); // not a GUID — nothing to move
        };
        let needle = guid_to_db(child_id);
        let Some(pos) = order.iter().position(|c| *c == needle) else {
            return Ok(()); // entry not in the playlist — nothing to move
        };
        let child = order.remove(pos);
        // Clamp to the valid range of the now-shorter list (C# `Math.Clamp`).
        let target = usize::try_from(new_index.max(0))
            .unwrap_or(usize::MAX)
            .min(order.len());
        order.insert(target, child);
        write_sort_order(self.db.writer(), &pid, &order).await?;
        if let Ok(playlist) = Uuid::parse_str(playlist_id) {
            item_data::sync_container_data(&self.db, playlist).await?;
        }
        Ok(())
    }

    async fn remove_playlists(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        // Ownership is recorded (the `FerrofinPlaylists` table), but the user-deletion
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
            r#"UPDATE "FerrofinLinkedChildren" SET "SortOrder" = ?1
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

    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::playlists::{PlaylistCreationRequest, PlaylistUpdateRequest};
    use uuid::Uuid;

    use ferrofin_traits::collections::{
        CollectionCreationOptions, CollectionManager, PlaylistManager,
    };

    use crate::linked_children_service::FerrofinLinkedChildrenService;
    use crate::test_support::{item_repository_over, library_manager_over, seed_item, test_db};

    use super::{
        COLLECTIONS_FOLDER_DATA, FerrofinCollectionManager, FerrofinPlaylistManager,
        collections_folder,
    };

    /// Test paths under a temp root, so the provisioned containers land
    /// somewhere harmless.
    fn test_paths() -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
        Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
            "/tmp/ferrofin-collections-test",
            "/tmp/ferrofin-collections-test/log",
            "/tmp/ferrofin-collections-test/config",
            "/tmp/ferrofin-collections-test/cache",
            "/tmp/ferrofin-collections-test/web",
        ))
    }

    fn collection_manager_over(db: &ferrofin_db::Database) -> FerrofinCollectionManager {
        FerrofinCollectionManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(FerrofinLinkedChildrenService::new(db.clone())),
            test_paths(),
        )
    }

    /// A created playlist has to stay reachable too, and must reuse the
    /// playlists folder the user-view manager already provisions.
    ///
    /// Ferrofin writes that folder as `ManualPlaylistsFolder` and an adopted
    /// database carries a `PlaylistsFolder` — at the same path. If the two
    /// provisioners disagreed there would be two rows, and a playlist parented
    /// to the one the query scope does not accept simply vanishes.
    /// A container provisioned before the user root existed is adopted by it
    /// later, rather than staying parentless forever because the row is already
    /// there.
    #[tokio::test]
    async fn a_parentless_container_is_attached_once_the_root_appears() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root_dir = tmp.path().to_string_lossy().into_owned();
        let paths: Arc<dyn ferrofin_traits::system::ServerApplicationPaths> =
            Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
                root_dir.clone(),
                format!("{root_dir}/log"),
                format!("{root_dir}/config"),
                format!("{root_dir}/cache"),
                format!("{root_dir}/web"),
            ));
        let mode = crate::item_type_lookup::IdDerivation::from_meta(
            Some("jellyfin-10.11.8"),
            Some(paths.program_data_path()),
        );
        let root =
            crate::item_type_lookup::user_root_folder_id(&mode, &paths.default_user_views_path())
                .expect("root id");
        let path = format!("{}/collections", paths.data_path());

        // First provision: the root does not exist yet, so the row is created
        // without a parent rather than failing the foreign key.
        let id = crate::item_persistence_service::ensure_container(
            &db,
            BaseItemKind::CollectionFolder,
            "Collections",
            &path,
            &mode,
            Some(root),
            Some(COLLECTIONS_FOLDER_DATA),
        )
        .await
        .expect("provision")
        .expect("an id");
        assert!(
            crate::test_support::fetch_item(&db, id)
                .await
                .parent_id
                .is_none(),
            "nothing to parent to yet"
        );

        // The root appears, and the next provision adopts the existing row.
        crate::test_support::seed_named_item(
            &db,
            root,
            BaseItemKind::UserRootFolder,
            "Media Folders",
        )
        .await;
        let again = crate::item_persistence_service::ensure_container(
            &db,
            BaseItemKind::CollectionFolder,
            "Collections",
            &path,
            &mode,
            Some(root),
            Some(COLLECTIONS_FOLDER_DATA),
        )
        .await
        .expect("provision")
        .expect("an id");
        assert_eq!(again, id, "the same row, not a second one");
        assert_eq!(
            crate::test_support::fetch_item(&db, id).await.parent_id,
            Some(ferrofin_db::store::guid_to_db(root)),
            "…now attached to the root"
        );
    }

    /// `get_collections_folder` is the real `EnsureLibraryFolder`: it answers
    /// `None` before the library exists when the caller did not ask for it to be
    /// created, and the provisioned row afterwards. It used to answer `None`
    /// unconditionally.
    #[tokio::test]
    async fn get_collections_folder_looks_up_and_provisions() {
        let db = test_db().await;
        let manager = collection_manager_over(&db);
        assert!(
            manager
                .get_collections_folder(false)
                .await
                .expect("lookup")
                .is_none(),
            "no library yet, and the caller did not ask for one"
        );
        let made = manager
            .get_collections_folder(true)
            .await
            .expect("provision")
            .expect("a row");
        assert_eq!(made.name.as_deref(), Some("Collections"));
        assert_eq!(made.data.as_deref(), Some(COLLECTIONS_FOLDER_DATA));
        let found = manager
            .get_collections_folder(false)
            .await
            .expect("lookup")
            .expect("a row");
        assert_eq!(found.id, made.id, "the same row, found by path");
    }

    /// The auto-provisioned Collections library carries
    /// `"CollectionType":"boxsets"` in `Data` — the column 10.11.8 reads that
    /// field from.
    ///
    /// Without it, `/Items?parentId={root}`, `/UserViews` and
    /// `/Library/MediaFolders` all sent a null `CollectionType` for the folder
    /// where Jellyfin sends `boxsets` (measured on the parity pair 2026-08-31).
    #[tokio::test]
    async fn the_collections_library_carries_the_boxsets_collection_type() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root_dir = tmp.path().to_string_lossy().into_owned();
        let paths: Arc<dyn ferrofin_traits::system::ServerApplicationPaths> =
            Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
                root_dir.clone(),
                format!("{root_dir}/log"),
                format!("{root_dir}/config"),
                format!("{root_dir}/cache"),
                format!("{root_dir}/web"),
            ));
        let id = collections_folder(&db, &paths)
            .await
            .expect("provision")
            .expect("an id");
        let data = crate::test_support::fetch_item(&db, id).await.data;
        assert_eq!(
            data.as_deref(),
            Some(COLLECTIONS_FOLDER_DATA),
            "the provisioned row carries the boxsets collection type"
        );
        // …and the DTO layer really reads it back as `boxsets`, which is the
        // observable the parity pair measures.
        let parsed: serde_json::Value =
            serde_json::from_str(data.as_deref().expect("data")).expect("valid json");
        assert_eq!(
            parsed.get("CollectionType").and_then(|v| v.as_str()),
            Some("boxsets")
        );

        // The other half — that an ADOPTED database's richer blob survives
        // re-provisioning — lives in `item_persistence_service`'s own tests,
        // because seeding it is SQL and SQL stays behind that boundary.
    }

    /// The provisioned container carries the id JELLYFIN would compute for that
    /// directory, its directory exists, and it hangs off the user root.
    ///
    /// The id is the one that matters for drop-in: `ensure_container` used to
    /// derive it with `IdDerivation::LegacyLowercase` whatever the database
    /// said, so on an adopted (Jellyfin-parity) database Jellyfin would look for
    /// its own id, miss, and create a SECOND Collections library beside ours —
    /// with the round-trip guarantee going with it.
    #[tokio::test]
    async fn a_provisioned_container_is_shaped_the_way_jellyfin_expects() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root_dir = tmp.path().to_string_lossy().into_owned();
        let paths: Arc<dyn ferrofin_traits::system::ServerApplicationPaths> =
            Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
                root_dir.clone(),
                format!("{root_dir}/log"),
                format!("{root_dir}/config"),
                format!("{root_dir}/cache"),
                format!("{root_dir}/web"),
            ));

        // A parity database, as a fresh or adopted one is.
        db.meta_set("item_id_derivation", "jellyfin-10.11.8")
            .await
            .expect("mode");
        // The user root has to exist for the container to be parented to it.
        let mode = crate::item_type_lookup::IdDerivation::from_meta(
            Some("jellyfin-10.11.8"),
            Some(paths.program_data_path()),
        );
        let root =
            crate::item_type_lookup::user_root_folder_id(&mode, &paths.default_user_views_path())
                .expect("root id");
        crate::test_support::seed_named_item(
            &db,
            root,
            BaseItemKind::UserRootFolder,
            "Media Folders",
        )
        .await;

        let path = format!("{}/collections", paths.data_path());
        let id = crate::item_persistence_service::ensure_container(
            &db,
            BaseItemKind::CollectionFolder,
            "Collections",
            &path,
            &mode,
            Some(root),
            Some(COLLECTIONS_FOLDER_DATA),
        )
        .await
        .expect("provision")
        .expect("an id");

        // The id Jellyfin derives for that directory, under the same rule.
        let expected = crate::item_type_lookup::derive_item_id_with(
            &mode,
            BaseItemKind::CollectionFolder,
            &path,
        )
        .expect("expected id");
        assert_eq!(id, expected, "the container carries Jellyfin's id");
        assert_ne!(
            id,
            crate::item_type_lookup::derive_item_id(BaseItemKind::CollectionFolder, &path)
                .expect("legacy id"),
            "…which is NOT the legacy-lowercase one, or this test proves nothing"
        );

        assert!(
            tokio::fs::metadata(&path).await.is_ok(),
            "the directory the row describes exists"
        );
        let row = crate::test_support::fetch_item(&db, id).await;
        assert_eq!(
            row.parent_id.as_deref(),
            Some(ferrofin_db::store::guid_to_db(root).as_str()),
            "and it hangs off the user root"
        );
    }

    #[tokio::test]
    async fn a_created_playlist_reuses_the_existing_playlists_folder() {
        let db = test_db().await;
        // What `user_view_manager::ensure_playlists_folder` leaves behind.
        let existing = Uuid::from_u128(0xF0DE);
        crate::item_persistence_service::insert_named_item(
            &db,
            existing,
            BaseItemKind::ManualPlaylistsFolder,
            "Playlists",
            true,
            None,
        )
        .await
        .expect("seed the existing folder");
        crate::test_support::set_item_path(
            &db,
            existing,
            "/tmp/ferrofin-collections-test/data/playlists",
        )
        .await;

        let mgr = playlist_manager(&db);
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("My Playlist".to_owned()),
                ..Default::default()
            })
            .await
            .expect("create playlist");

        let row = crate::test_support::fetch_item(
            &db,
            Uuid::parse_str(&created.id).expect("playlist id"),
        )
        .await;
        assert_eq!(
            row.parent_id,
            Some(ferrofin_db::store::guid_to_db(existing)),
            "the playlist reuses the folder already there, rather than a second one"
        );

        let folders = crate::test_support::items_at_path(
            &db,
            "/tmp/ferrofin-collections-test/data/playlists",
        )
        .await;
        assert_eq!(folders, 1, "exactly one playlists folder at that path");
    }

    /// A created collection has to stay reachable.
    ///
    /// A query naming no scope is confined to the user's libraries, so an item
    /// with no parent and no top parent is invisible to every user browse.
    /// Upstream never creates one: `CreateCollectionAsync` goes through
    /// `EnsureLibraryFolder`, which auto-provisions the "Collections" library.
    #[tokio::test]
    async fn a_created_collection_lands_in_the_collections_library() {
        let db = test_db().await;
        let mgr = collection_manager_over(&db);
        let row = mgr
            .create_collection(&CollectionCreationOptions {
                name: "My Collection".to_owned(),
                ..Default::default()
            })
            .await
            .expect("create");

        let parent = row.parent_id.expect("a collection has a parent");
        assert_eq!(
            row.top_parent_id.as_deref(),
            Some(parent.as_str()),
            "the library is also the top parent"
        );
        let container = crate::test_support::fetch_item(
            &db,
            Uuid::parse_str(&parent).expect("the parent id is a guid"),
        )
        .await;
        assert_eq!(container.name.as_deref(), Some("Collections"));
        assert_eq!(
            container.type_,
            crate::item_type_lookup::stored_type_name(BaseItemKind::CollectionFolder)
                .expect("kind is known")
        );
    }

    /// The container is provisioned once, and an existing one is reused — an
    /// adopted database keeps Jellyfin's rather than gaining a second.
    #[tokio::test]
    async fn the_collections_library_is_provisioned_once() {
        let db = test_db().await;
        let mgr = collection_manager_over(&db);
        let mut parents = Vec::new();
        for name in ["First", "Second"] {
            parents.push(
                mgr.create_collection(&CollectionCreationOptions {
                    name: name.to_owned(),
                    ..Default::default()
                })
                .await
                .expect("create")
                .parent_id
                .expect("parent"),
            );
        }
        assert_eq!(parents[0], parents[1]);
        let containers = crate::test_support::item_repository_over(db.clone())
            .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                include_item_types: vec![BaseItemKind::CollectionFolder],
                ..Default::default()
            })
            .await
            .expect("libraries");
        assert_eq!(containers.len(), 1, "provisioned once, then reused");
    }

    /// The orphans an older Ferrofin created are adopted on the next create.
    #[tokio::test]
    async fn an_orphaned_collection_is_taken_into_the_library() {
        let db = test_db().await;
        let mgr = collection_manager_over(&db);
        // What the previous code wrote: no parent, no top parent.
        let orphan = Uuid::from_u128(0xB0C5);
        crate::item_persistence_service::insert_named_item(
            &db,
            orphan,
            BaseItemKind::BoxSet,
            "Old Collection",
            true,
            None,
        )
        .await
        .expect("orphan");

        mgr.create_collection(&CollectionCreationOptions {
            name: "New Collection".to_owned(),
            ..Default::default()
        })
        .await
        .expect("create");

        let adopted = crate::test_support::fetch_item(&db, orphan).await;
        assert!(
            adopted.parent_id.is_some(),
            "the orphan was taken into the library"
        );
        assert_eq!(adopted.parent_id, adopted.top_parent_id);
    }

    #[tokio::test]
    async fn create_collection_persists_row_and_members() {
        let db = test_db().await;
        let movie_a = Uuid::new_v4();
        let movie_b = Uuid::new_v4();
        seed_item(&db, movie_a, BaseItemKind::Movie).await;
        seed_item(&db, movie_b, BaseItemKind::Movie).await;

        let mgr = collection_manager_over(&db);

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
        use ferrofin_traits::options::InternalItemsQuery;
        let db = test_db().await;
        let movie_a = Uuid::new_v4();
        let movie_b = Uuid::new_v4();
        seed_item(&db, movie_a, BaseItemKind::Movie).await;
        seed_item(&db, movie_b, BaseItemKind::Movie).await;

        let library = library_manager_over(db.clone());
        let mgr = collection_manager_over(&db);
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
        let ids: Vec<Uuid> = result
            .items
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert!(ids.contains(&movie_a), "movie_a missing: {ids:?}");
        assert!(ids.contains(&movie_b), "movie_b missing: {ids:?}");
        assert_eq!(result.total_record_count, 2);
    }

    #[tokio::test]
    async fn deleting_a_collection_does_not_delete_its_members() {
        // Data-loss regression: DELETE /Items/{boxset} cascades to PHYSICAL children only.
        // A box-set/playlist's members are LinkedChildren (references); deleting the container
        // must leave the referenced items intact. (WI-6 made parentId browse merge LinkedChildren;
        // without physical_children_only the delete-cascade would wipe the movies.)
        use ferrofin_traits::options::DeleteOptions;
        let db = test_db().await;
        let movie_a = Uuid::new_v4();
        let movie_b = Uuid::new_v4();
        seed_item(&db, movie_a, BaseItemKind::Movie).await;
        seed_item(&db, movie_b, BaseItemKind::Movie).await;

        let library = library_manager_over(db.clone());
        let mgr = collection_manager_over(&db);
        let created = mgr
            .create_collection(&CollectionCreationOptions {
                name: "Doomed".to_owned(),
                item_id_list: vec![movie_a, movie_b],
                ..CollectionCreationOptions::default()
            })
            .await
            .expect("create");
        let cid = Uuid::parse_str(&created.id).expect("uuid");

        library
            .delete_item(cid, &DeleteOptions::default())
            .await
            .expect("delete collection");

        // The collection is gone, but both movies survive.
        assert!(library.get_item_by_id(cid).await.expect("q").is_none());
        assert!(
            library.get_item_by_id(movie_a).await.expect("q").is_some(),
            "movie_a deleted!"
        );
        assert!(
            library.get_item_by_id(movie_b).await.expect("q").is_some(),
            "movie_b deleted!"
        );
    }

    #[tokio::test]
    async fn create_and_update_playlist() {
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;

        let mgr = playlist_manager(&db);

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

        let mgr = playlist_manager(&db);

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
        let ids: Vec<Uuid> = items
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert!(ids.contains(&track_a));
        assert!(ids.contains(&track_b));
    }

    #[tokio::test]
    async fn get_playlist_items_preserves_link_order() {
        // Members must come back in link order, not id/insertion order — seed
        // three tracks and add them out of sort order; the result must echo the
        // added sequence. (No prior coverage guarded playlist ordering.)
        let db = test_db().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        for id in [a, b, c] {
            seed_item(&db, id, BaseItemKind::Audio).await;
        }
        let mgr = playlist_manager(&db);
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Ordered".to_owned()),
                item_id_list: vec![c, a, b],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");

        let ids: Vec<Uuid> = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items")
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert_eq!(ids, vec![c, a, b]);
    }

    #[tokio::test]
    async fn remove_by_dashless_playlist_item_id_removes_member() {
        // `GetPlaylistItems` emits `PlaylistItemId` dashless (`Guid('N')`), but
        // `ChildId` is stored dashed. Removing by the dashless id the DTO exposes
        // must still hit the row (regression: it matched zero rows before).
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;

        let mgr = playlist_manager(&db);

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
        let mgr = playlist_manager(&db);
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
        let order: Vec<Uuid> = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items")
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert_eq!(
            order,
            vec![c, a, b],
            "position 0 should move the appended item to the front"
        );

        // No position → plain append at the end.
        let d = Uuid::new_v4();
        seed_item(&db, d, BaseItemKind::Audio).await;
        mgr.add_item_to_playlist(playlist_id, &[d], None, owner)
            .await
            .expect("append");
        let last = Uuid::parse_str(
            &mgr.get_playlist_items(playlist_id, owner)
                .await
                .expect("items2")
                .last()
                .expect("non-empty")
                .id,
        )
        .expect("uuid");
        assert_eq!(last, d, "no position should append at the end");
    }

    #[tokio::test]
    async fn get_playlist_items_reflects_move_item_order() {
        // Playlists are user-ordered, so the read must echo the stored
        // `SortOrder` — including after a reorder. The member ids are FIXED and
        // ascending while the link order is their exact reverse, so any
        // id/rowid/insertion ordering (e.g. dropping `ORDER BY lc."SortOrder"`)
        // fails this deterministically rather than by luck of random guids.
        let db = test_db().await;
        let (a, b, c) = (
            Uuid::from_u128(0x1111_1111),
            Uuid::from_u128(0x2222_2222),
            Uuid::from_u128(0x3333_3333),
        );
        for id in [a, b, c] {
            seed_item(&db, id, BaseItemKind::Audio).await;
        }
        let mgr = playlist_manager(&db);
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Reordered".to_owned()),
                item_id_list: vec![c, b, a],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");
        let read = |user| {
            let mgr = mgr.clone();
            async move {
                mgr.get_playlist_items(playlist_id, user)
                    .await
                    .expect("items")
                    .iter()
                    .filter_map(|i| Uuid::parse_str(&i.id).ok())
                    .collect::<Vec<Uuid>>()
            }
        };
        assert_eq!(read(owner).await, vec![c, b, a], "link order, not id order");

        // Move the middle entry to the end: [C, A, B].
        mgr.move_item(&created.id, &b.to_string(), 2, owner)
            .await
            .expect("move");
        assert_eq!(
            read(owner).await,
            vec![c, a, b],
            "read must echo the rewritten SortOrder"
        );
    }

    #[tokio::test]
    async fn get_playlist_items_pins_entry_id_derivation() {
        // `PlaylistItemId` on the wire is `item.id.replace('-', "")` over the row
        // this returns, so the row's `Id` must stay the canonical stored guid
        // (dashed uppercase) — the joined read must project the CHILD's `Id`,
        // never the playlist's.
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;
        let mgr = playlist_manager(&db);
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Pinned".to_owned()),
                item_id_list: vec![track],
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
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].id,
            ferrofin_db::store::guid_to_db(track),
            "row id must be the child's canonical stored guid"
        );
        assert_eq!(
            items[0].id.replace('-', ""),
            track.simple().to_string().to_uppercase(),
            "PlaylistItemId derivation must be unchanged"
        );
        // And the member's own fields came back (not the playlist's).
        assert_eq!(
            Some(items[0].type_.as_str()),
            crate::item_type_lookup::stored_type_name(BaseItemKind::Audio)
        );
    }

    #[tokio::test]
    async fn get_playlist_items_denies_stranger_on_populated_playlist() {
        // The single-query read decides access from columns joined onto the
        // member rows; a stranger must still get NotFound and see NO items.
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;
        let mgr = playlist_manager(&db);
        let (alice, carol) = (Uuid::new_v4(), Uuid::new_v4());
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Private".to_owned()),
                item_id_list: vec![track],
                user_id: alice,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");

        // The owner sees it…
        assert_eq!(
            mgr.get_playlist_items(playlist_id, alice)
                .await
                .expect("owner reads")
                .len(),
            1
        );
        // …a stranger does not, and leaks nothing.
        let err = mgr
            .get_playlist_items(playlist_id, carol)
            .await
            .expect_err("stranger must not read a private playlist's members");
        assert!(matches!(
            err,
            ferrofin_traits::error::ServiceError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn get_playlist_items_visible_to_shared_and_open_access() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;

        let db = test_db().await;
        // Fixed, ascending ids added in reverse: the assertion below pins link
        // order too, and can't pass by chance on an id ordering.
        let (a, b) = (Uuid::from_u128(0x4444_4444), Uuid::from_u128(0x5555_5555));
        for id in [a, b] {
            seed_item(&db, id, BaseItemKind::Audio).await;
        }
        let mgr = playlist_manager(&db);
        let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());

        // Shared read-only with bob.
        let shared = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Shared".to_owned()),
                item_id_list: vec![b, a],
                user_id: alice,
                users: vec![PlaylistUserPermissions::new(bob, false)],
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create shared")
            .id,
        )
        .expect("uuid");
        let ids: Vec<Uuid> = mgr
            .get_playlist_items(shared, bob)
            .await
            .expect("share grants read")
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert_eq!(ids, vec![b, a], "a shared reader sees the members in order");

        // Open access: any caller reads it.
        let public = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Public".to_owned()),
                item_id_list: vec![b],
                user_id: alice,
                public: Some(true),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create public")
            .id,
        )
        .expect("uuid");
        assert_eq!(
            mgr.get_playlist_items(public, Uuid::new_v4())
                .await
                .expect("open access grants read")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn get_playlist_items_empty_playlist_still_gates_access() {
        // With no member rows the joined read has no access columns to read, so
        // the fallback must still answer: empty for the owner, NotFound for a
        // stranger.
        let db = test_db().await;
        let mgr = playlist_manager(&db);
        let alice = Uuid::new_v4();
        let playlist_id = Uuid::parse_str(
            &mgr.create_playlist(&PlaylistCreationRequest {
                name: Some("Empty".to_owned()),
                user_id: alice,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create")
            .id,
        )
        .expect("uuid");

        assert!(
            mgr.get_playlist_items(playlist_id, alice)
                .await
                .expect("owner reads an empty playlist")
                .is_empty()
        );
        assert!(matches!(
            mgr.get_playlist_items(playlist_id, Uuid::new_v4()).await,
            Err(ferrofin_traits::error::ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_playlist_items_legacy_playlist_without_meta_row_reads() {
        // A pre-ownership playlist (no `FerrofinPlaylists` row) keeps granting
        // owner-equivalent access on the joined path — the LEFT JOIN's NULLs must
        // be read as "legacy", not "denied".
        let db = test_db().await;
        let legacy = Uuid::new_v4();
        let track = Uuid::new_v4();
        seed_item(&db, legacy, BaseItemKind::Playlist).await;
        seed_item(&db, track, BaseItemKind::Audio).await;
        let mgr = playlist_manager(&db);
        mgr.add_item_to_playlist(legacy, &[track], None, Uuid::new_v4())
            .await
            .expect("add");

        let items = mgr
            .get_playlist_items(legacy, Uuid::new_v4())
            .await
            .expect("legacy playlists stay readable");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, ferrofin_db::store::guid_to_db(track));
    }

    #[tokio::test]
    async fn get_playlist_items_ignores_non_manual_links() {
        // Only manual (ChildType 0) edges are playlist members; a shortcut edge
        // on the same parent must not surface.
        let db = test_db().await;
        let member = Uuid::new_v4();
        let shortcut = Uuid::new_v4();
        for id in [member, shortcut] {
            seed_item(&db, id, BaseItemKind::Audio).await;
        }
        let mgr = playlist_manager(&db);
        let owner = Uuid::new_v4();
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Typed".to_owned()),
                item_id_list: vec![member],
                user_id: owner,
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        let playlist_id = Uuid::parse_str(&created.id).expect("uuid");
        let links = FerrofinLinkedChildrenService::new(db.clone());
        ferrofin_traits::persistence::LinkedChildrenService::upsert_linked_child(
            &links,
            playlist_id,
            shortcut,
            1,
        )
        .await
        .expect("shortcut link");

        let ids: Vec<Uuid> = mgr
            .get_playlist_items(playlist_id, owner)
            .await
            .expect("items")
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect();
        assert_eq!(ids, vec![member], "shortcut links are not playlist members");
    }

    #[tokio::test]
    async fn get_playlist_items_missing_is_not_found() {
        let db = test_db().await;
        let mgr = playlist_manager(&db);
        let err = mgr
            .get_playlist_items(Uuid::new_v4(), Uuid::new_v4())
            .await
            .expect_err("missing");
        assert!(matches!(
            err,
            ferrofin_traits::error::ServiceError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn get_missing_playlist_is_not_found() {
        let db = test_db().await;
        let mgr = playlist_manager(&db);
        let err = mgr
            .get_playlist_for_user(Uuid::new_v4(), Uuid::new_v4())
            .await
            .expect_err("missing");
        assert!(matches!(
            err,
            ferrofin_traits::error::ServiceError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn playlist_shares_add_update_get_remove() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;
        use ferrofin_model::playlists::PlaylistUserUpdateRequest;

        let db = test_db().await;
        let mgr = playlist_manager(&db);
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
    fn playlist_manager(db: &ferrofin_db::Database) -> FerrofinPlaylistManager {
        FerrofinPlaylistManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(FerrofinLinkedChildrenService::new(db.clone())),
            item_repository_over(db.clone()),
            test_paths(),
        )
    }

    #[tokio::test]
    async fn create_playlist_records_owner_and_seeds_shares() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;
        use ferrofin_traits::collections::PlaylistAccessLevel;

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
            Err(ferrofin_traits::error::ServiceError::NotFound(_))
        ));
        assert!(matches!(
            mgr.get_playlist_for_user(playlist_id, carol).await,
            Err(ferrofin_traits::error::ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn open_access_playlist_is_readable_by_anyone() {
        use ferrofin_traits::collections::PlaylistAccessLevel;

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
        use ferrofin_traits::collections::PlaylistAccessLevel;

        let db = test_db().await;
        let mgr = playlist_manager(&db);
        // A pre-ownership row: the BaseItems row exists with no `FerrofinPlaylists` meta.
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
        use ferrofin_model::entities_media::PlaylistUserPermissions;

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

        let p1 = Uuid::parse_str(&p1).expect("uuid");
        let p2 = Uuid::parse_str(&p2).expect("uuid");
        let visible: Vec<Uuid> = mgr
            .get_playlists(alice)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|r| Uuid::parse_str(&r.id).ok())
            .collect();
        assert_eq!(visible.len(), 2, "alice sees her own + bob's shared");
        assert!(visible.contains(&p1) && visible.contains(&p2));
    }

    #[tokio::test]
    async fn update_playlist_applies_public_and_replaces_shares() {
        use ferrofin_model::entities_media::PlaylistUserPermissions;
        use ferrofin_traits::collections::PlaylistAccessLevel;

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

    /// Seeds a one-track playlist owned by `owner`, shared read-only with
    /// `shared_with` (when given) and open-access when `public`. Returns the
    /// manager, the playlist id and the member track id — the fixture the
    /// `get_playlist_items` visibility tests share.
    async fn playlist_with_one_track(
        db: &ferrofin_db::Database,
        owner: Uuid,
        shared_with: Option<Uuid>,
        public: bool,
    ) -> (FerrofinPlaylistManager, Uuid, Uuid) {
        use ferrofin_model::entities_media::PlaylistUserPermissions;

        let track = Uuid::new_v4();
        seed_item(db, track, BaseItemKind::Audio).await;
        let mgr = playlist_manager(db);
        let created = mgr
            .create_playlist(&PlaylistCreationRequest {
                name: Some("Members".to_owned()),
                item_id_list: vec![track],
                user_id: owner,
                users: shared_with
                    .into_iter()
                    .map(|u| PlaylistUserPermissions::new(u, false))
                    .collect(),
                public: Some(public),
                ..PlaylistCreationRequest::default()
            })
            .await
            .expect("create");
        (mgr, Uuid::parse_str(&created.id).expect("uuid"), track)
    }

    /// The member ids `get_playlist_items` returns for `reader`, in link order.
    async fn items_for(
        mgr: &FerrofinPlaylistManager,
        playlist_id: Uuid,
        reader: Uuid,
    ) -> Vec<Uuid> {
        mgr.get_playlist_items(playlist_id, reader)
            .await
            .expect("items")
            .iter()
            .filter_map(|i| Uuid::parse_str(&i.id).ok())
            .collect()
    }

    #[tokio::test]
    async fn get_playlist_items_readable_by_owner_and_shared_user() {
        // The members read is gated by the `access` join, whose share leg keys
        // on the *caller's* `UserId`. Owner and share-holder are the two callers
        // that leg must let through.
        let db = test_db().await;
        let (owner, friend) = (Uuid::new_v4(), Uuid::new_v4());
        let (mgr, playlist_id, track) =
            playlist_with_one_track(&db, owner, Some(friend), false).await;

        assert_eq!(
            items_for(&mgr, playlist_id, owner).await,
            vec![track],
            "the owner reads their own playlist's members"
        );
        assert_eq!(
            items_for(&mgr, playlist_id, friend).await,
            vec![track],
            "a user the playlist is explicitly shared with reads its members"
        );
    }

    #[tokio::test]
    async fn get_playlist_items_hidden_from_unrelated_user() {
        // The share row belongs to `friend`; the join's `UserId` predicate is
        // the only thing standing between `stranger` and another user's private
        // playlist. Invisible reads as missing (never a partial member list).
        let db = test_db().await;
        let (owner, friend, stranger) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (mgr, playlist_id, _track) =
            playlist_with_one_track(&db, owner, Some(friend), false).await;

        let err = mgr
            .get_playlist_items(playlist_id, stranger)
            .await
            .expect_err("an unrelated user must not read a shared-with-someone-else playlist");
        assert!(
            matches!(err, ferrofin_traits::error::ServiceError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
        // Same verdict through the sibling read paths, so the gate can't be
        // routed around: neither exposes the playlist to a non-share-holder.
        assert!(matches!(
            mgr.get_playlist_access(playlist_id, stranger).await,
            Err(ferrofin_traits::error::ServiceError::NotFound(_))
        ));
        assert!(matches!(
            mgr.get_playlist_for_user(playlist_id, stranger).await,
            Err(ferrofin_traits::error::ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_playlist_items_open_access_readable_by_unrelated_user() {
        // Open access is orthogonal to the share join: with no share row at all
        // any caller still reads the members (unchanged behaviour).
        let db = test_db().await;
        let (mgr, playlist_id, track) =
            playlist_with_one_track(&db, Uuid::new_v4(), None, true).await;

        assert_eq!(
            items_for(&mgr, playlist_id, Uuid::new_v4()).await,
            vec![track],
            "an open-access playlist stays readable by any user"
        );
    }
}
