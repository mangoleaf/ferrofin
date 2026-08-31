//! [`FerrofinItemRepository`] — the concrete [`ItemRepository`] over `ferrofin-db`.
//!
//! Port of `BaseItemRepository` (the `Querying`/`ByName` partials). Reads and
//! queries `BaseItems` rows, materializing [`BaseItemEntity`] instead of the
//! un-ported C# `BaseItem` domain object (per the persistence-trait port rules).
//! The query translation lives in [`crate::translate_query`]; this type wires it
//! to the pool and runs the resulting statements.
//!
//! The `ConfigurationManager` is a constructor dependency in C# but only feeds
//! path normalization that is not needed for the row-level reads here, so it is
//! not taken as a field (it would be injected at the composition root if a later
//! method needs it).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::{BaseItemEntity, BaseItemImageInfoEntity, ItemTextRow};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::{ItemValueType, PermissionKind, PreferenceKind};
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_model::entities::ImageType;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use ferrofin_traits::options::ItemImageInfo;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{
    ItemRepository, ItemTypeLookup, ItemWithCounts, PlaylistAccessColumns, PlaylistItemsWithAccess,
};

use crate::aggregate_folder::RootFolderIds;
use crate::db_error::{db_err, media_stream_type_disc};
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::{
    PLACEHOLDER_ID, QueryShape, append_predicates, build_latest_item_list_query, build_query,
    non_blank, push_in_list, to_guid_strings,
};
use crate::user_entity_ext::{guid_preference, has_permission, live_tv_enabled_for};
use sqlx::{FromRow, QueryBuilder, Row, Sqlite};

/// The concrete item repository.
///
/// Holds a cheaply-cloneable [`Database`] handle plus the shared
/// [`ItemTypeLookup`] (injected so the composition root can share one instance).
#[derive(Clone)]
pub struct FerrofinItemRepository {
    db: Database,
    item_type_lookup: Arc<dyn ItemTypeLookup>,
    /// The `UserRootFolder`/`AggregateFolder` ids, injected once by the
    /// composition root (see [`FerrofinItemRepository::with_root_ids`]).
    roots: Option<RootFolderIds>,
}

impl std::fmt::Debug for FerrofinItemRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinItemRepository")
            .finish_non_exhaustive()
    }
}

/// The columns [`FerrofinItemRepository::top_parent_ids_for_query`] reads to
/// resolve one view.
#[derive(sqlx::FromRow)]
struct ViewRow {
    /// The stored CLR type name.
    #[sqlx(rename = "Type")]
    type_name: String,
    /// The relational parent, the third arm's pointer.
    #[sqlx(rename = "ParentId")]
    parent_id: Option<String>,
    /// The view's path, which identifies a Live TV view written without a
    /// `Data` blob.
    #[sqlx(rename = "Path")]
    path: Option<String>,
    /// The serialized blob carrying `ViewType` and `DisplayParentId`.
    #[sqlx(rename = "Data")]
    data: Option<String>,
    /// Where `GetTopParent` lands for a followed pointer.
    #[sqlx(rename = "TopParentId")]
    top_parent_id: Option<String>,
}

impl FerrofinItemRepository {
    /// Creates a repository over the given database and kind-lookup tables.
    #[must_use]
    pub fn new(db: Database, item_type_lookup: Arc<dyn ItemTypeLookup>) -> Self {
        Self {
            db,
            item_type_lookup,
            roots: None,
        }
    }

    /// Injects the two root-folder ids resolved once at startup, so a browse
    /// of the `UserRootFolder` can add the `AggregateFolder`'s virtual children
    /// without a lookup.
    ///
    /// Both are process constants (derived from the application paths), so the
    /// concat costs one `Uuid` comparison on the browse path and **zero**
    /// extra queries. Left unset, the repository behaves exactly as before.
    #[must_use]
    pub fn with_root_ids(mut self, roots: RootFolderIds) -> Self {
        self.roots = Some(roots);
        self
    }

    /// The C# `ItemsController.GetItems` **non-query** branch, or `None` when
    /// this request is not it.
    ///
    /// Upstream (ItemsController.cs:307, 525-529):
    ///
    /// ```text
    /// if ((recursive ?? false) || ids.Length != 0 || item is not UserRootFolder) { …query… }
    /// else { result = new QueryResult<BaseItem>(folder.GetChildren(user, true)); }
    /// ```
    ///
    /// `GetChildren` builds its OWN `InternalItemsQuery`, so the request's sort,
    /// paging, item types and filters are all discarded — measured against
    /// 10.11.8 on the batch pair, `sortBy=SortName&sortOrder=Descending`,
    /// `sortBy=DateCreated`, `limit=2&startIndex=1`, `includeItemTypes=Movie` and
    /// a bare `/Items` (no `parentId` at all —
    /// `LibraryManager.GetParentItem(null, userId)` returns the user root) each
    /// answered with the identical seven root rows in the identical order.
    ///
    /// Ferrofin used to answer that bare `/Items` with the WHOLE server (554
    /// rows against Jellyfin's 7) and to sort the concatenated playlists folder
    /// inline instead of appending it.
    ///
    /// Needs [`RootFolderIds`] to recognise the user root, so it is inert on a
    /// repository built without them (unit tests, and any composition that has
    /// no aggregate root to concatenate from in the first place).
    async fn user_root_children(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError> {
        if !filter.user_root_children {
            return Ok(None);
        }
        let Some(roots) = self.roots else {
            return Ok(None);
        };
        if filter.parent_id != Uuid::nil() && filter.parent_id != roots.user_root {
            return Ok(None);
        }
        let children = InternalItemsQuery {
            user: filter.user.clone(),
            parent_id: roots.user_root,
            ..InternalItemsQuery::default()
        };
        let mut rows = self.fetch_rows(&children, QueryShape::FullRows).await?;
        // `AddChildrenFromCollection` keeps only `e.IsVisible(user)`
        // (Folder.cs:1414-1417). `Folder.IsVisible` (Folder.cs:231-253) is the
        // blocked-/enabled-folders check, and it applies to `ICollectionFolder
        // && not BasePluginFolder` — so a `CollectionFolder` is filtered, while
        // a `UserView` (not an `ICollectionFolder`) and the playlists folder (a
        // `BasePluginFolder`) always pass.
        if let Some(user) = filter.user.as_ref() {
            let hidden = self.hidden_media_folders(user).await?;
            if !hidden.is_empty() {
                rows.retain(|row| Uuid::parse_str(&row.id).is_ok_and(|id| !hidden.contains(&id)));
            }
        }
        Ok(Some(rows))
    }

    /// The media-folder ids `user` may NOT see — `Folder.IsVisible`'s
    /// blocked-/enabled-folders arm, resolved once per call.
    ///
    /// Empty (and so a no-op) for the ordinary all-folders-enabled user, which
    /// is why the set is computed rather than probed per row.
    async fn hidden_media_folders(&self, user: &UserEntity) -> Result<Vec<Uuid>, ServiceError> {
        let blocked =
            folder_preference(&self.db, user, PreferenceKind::BlockedMediaFolders).await?;
        if !blocked.is_empty() {
            return Ok(blocked);
        }
        if has_permission(self.db.pool(), &user.id, PermissionKind::EnableAllFolders).await? {
            return Ok(Vec::new());
        }
        let Some(collection_folder) = stored_type_name(BaseItemKind::CollectionFolder) else {
            return Ok(Vec::new());
        };
        let allowed = folder_preference(&self.db, user, PreferenceKind::EnabledFolders).await?;
        let rows: Vec<String> =
            sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Type" = ?1"#)
                .bind(collection_folder)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|id| Uuid::parse_str(id).ok())
            .filter(|id| !allowed.contains(id))
            .collect())
    }

    /// Resolves the virtual library views a browse names into the physical
    /// folders its items actually hang off, returning `None` when there is
    /// nothing to resolve.
    ///
    /// Port of `LibraryManager.GetTopParentIdsForQuery`'s `CollectionFolder`
    /// arm and `CollectionFolder.GetActualChildren`. On a Jellyfin database a
    /// `CollectionFolder` is virtual — measured on a real 10.11.8 library, **0**
    /// rows carry one as `ParentId` and **0** carry one as `TopParentId` — so a
    /// browse or a Latest query scoped to a view finds nothing until it is
    /// translated. The link lives only in the item's serialized `Data` blob
    /// (`PhysicalFolderIds`); the physical folders themselves hang off the
    /// AggregateFolder, so there is no relational path to follow instead.
    ///
    /// A Ferrofin-written database keeps no `Data` blob and hangs items off the
    /// collection folder directly, so every lookup here comes back empty and
    /// the query is left exactly as it was.
    async fn resolve_views(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Option<InternalItemsQuery>, ServiceError> {
        // `physical_children_only` is the delete-cascade path: it must keep
        // meaning "the rows this item owns", never widen to a library's whole
        // contents.
        let wants_parent =
            filter.parent_id != Uuid::nil() && !filter.recursive && !filter.physical_children_only;
        // Every item on the server descends from the `AggregateFolder` (it IS
        // the physical root — `AggregateFolder.IsPhysicalRoot`), so a recursive
        // query scoped to it is a query with no scope at all. Jellyfin reaches
        // the same answer through its ancestor closure, which carries the
        // aggregate on every scanned row.
        //
        // …but "no scope" is not the same as "no user scope". C#
        // `SetTopParentIdsOrAncestors` puts the aggregate (neither an
        // `ICollectionFolder` nor a `UserView`) into `query.AncestorIds` and
        // only then clears `query.Parent` (LibraryManager.cs:1673-1698), so the
        // result stays inside the physical root's ancestor closure. Ferrofin
        // keeps a one-level `AncestorIds` table, not a transitive closure, so
        // binding the aggregate there would match nothing; the equivalent is to
        // answer exactly as the SAME query with no parent at all does — which
        // is what `scope_to_user_libraries` scopes. Without it the branch leaked
        // rows outside the closure: measured on the batch pair,
        // `/Items?parentId={aggregate}&recursive=true&includeItemTypes=Person`
        // was Ferrofin 3 / Jellyfin 0.
        if filter.recursive
            && !filter.physical_children_only
            && self
                .roots
                .is_some_and(|roots| filter.parent_id == roots.aggregate)
        {
            let mut resolved = filter.clone();
            resolved.parent_id = Uuid::nil();
            return Ok(Some(
                scope_to_user_libraries(&self.db, &resolved)
                    .await?
                    .unwrap_or(resolved),
            ));
        }
        // A *recursive* browse of a library is scoped by top parent, not by the
        // ancestor closure — C# `SetTopParentIdsOrAncestors`, which swaps a
        // `CollectionFolder`/`UserView` parent for its top parents and leaves
        // anything else to the closure. Measured against a live 10.11.8 on the
        // same database, the two differ: the closure misses the library's own
        // physical folder row, and the top-parent scope is what reproduces
        // Jellyfin's count exactly.
        // `physical_children_only` is excluded for the same reason it is
        // excluded from `wants_parent`: that shape is the delete cascade's
        // ("every row directly under this folder on disk"), and widening it to
        // the library's whole top-parent scope would delete the library.
        if filter.recursive
            && filter.parent_id != Uuid::nil()
            && !filter.physical_children_only
            && let Some(mut folders) =
                Self::top_parent_ids_for_query(&self.db, filter.parent_id, filter.user.as_ref(), 0)
                    .await?
        {
            // A view that resolves to nothing matches nothing — upstream
            // substitutes a fresh guid for exactly this reason ("Prevent
            // searching in all libraries due to empty filter"). Falling through
            // instead would answer a browse of an empty library with the whole
            // server.
            if folders.is_empty() {
                folders.push(Uuid::parse_str(PLACEHOLDER_ID).unwrap_or_else(|_| Uuid::nil()));
            }
            let mut resolved = filter.clone();
            resolved.top_parent_ids = folders;
            // …and the `ParentId` equality has to go with it, or the two scopes
            // intersect to nothing: no row carries a collection folder as its
            // parent.
            resolved.parent_id = Uuid::nil();
            return Ok(Some(resolved));
        }
        if !wants_parent && filter.top_parent_ids.is_empty() {
            return scope_to_user_libraries(&self.db, filter).await;
        }

        let mut resolved = None;
        if wants_parent && let Some(roots) = self.roots {
            // `UserRootFolder.GetEligibleChildrenForRecursiveChildren`
            // (UserRootFolder.cs:96-102) concatenates
            // `LibraryManager.RootFolder.VirtualChildren` — the plug-in folders
            // `CreateRootFolder` parented to the AGGREGATE — onto its own
            // children. Two id comparisons, no query.
            if filter.parent_id == roots.user_root {
                resolved
                    .get_or_insert_with(|| filter.clone())
                    .virtual_child_parent_id = Some(roots.aggregate);
            } else if filter.parent_id == roots.aggregate {
                // `Folder.GetChildren` (Folder.cs:1348-1360): "the true root
                // should return our users root folder children" when
                // `IsPhysicalRoot`. Measured on 10.11.8:
                // `/Items?parentId={aggregate}` answers with exactly the user
                // root's children.
                let r = resolved.get_or_insert_with(|| filter.clone());
                r.parent_id = roots.user_root;
                r.virtual_child_parent_id = Some(roots.aggregate);
            }
        }
        if wants_parent {
            let folders = self
                .physical_folders_by_view(&[filter.parent_id])
                .await?
                .remove(&filter.parent_id)
                .unwrap_or_default();
            if !folders.is_empty() {
                resolved
                    .get_or_insert_with(|| filter.clone())
                    .parent_physical_folder_ids = folders;
            }
        }
        if !filter.top_parent_ids.is_empty() {
            // Per id, as C# does it: `TopParentIds = parents.SelectMany(i =>
            // GetTopParentIdsForQuery(i, user))`. A collection folder
            // contributes its physical folders; anything else — a `UserView`
            // like Live TV or Playlists, or any id on a Ferrofin database,
            // where the view IS the top parent — contributes itself. Replacing
            // the whole set whenever *one* id expanded would silently drop the
            // scopes that did not.
            let by_view = self
                .physical_folders_by_view(&filter.top_parent_ids)
                .await?;
            let mut expanded = Vec::with_capacity(filter.top_parent_ids.len());
            let changed = !by_view.is_empty();
            for id in &filter.top_parent_ids {
                match by_view.get(id) {
                    Some(folders) => expanded.extend(folders.iter().copied()),
                    None => expanded.push(*id),
                }
            }
            if changed {
                resolved
                    .get_or_insert_with(|| filter.clone())
                    .top_parent_ids = expanded;
            }
        }
        Ok(resolved)
    }

    /// The top parents a query scoped to `id` should use — C#
    /// `GetTopParentIdsForQuery` (`LibraryManager.cs:1728`).
    ///
    /// `None` means "leave this to the ancestor closure" — the item is not a view
    /// at all (which is what `SetTopParentIdsOrAncestors` does when the parents are
    /// not all `ICollectionFolder`/`UserView`), or it is a collection folder with no
    /// physical folders, which on a Ferrofin-written database is every one of them.
    /// `Some(vec![])` is different and deliberate: a *view* that resolves to
    /// nothing, which upstream turns into a match-nothing scope rather than letting
    /// the query widen to every library.
    ///
    /// The arms, in upstream's order:
    ///
    /// 1. a Live TV view stands for itself;
    /// 2. a `DisplayParentId` is followed (and a dangling one resolves to nothing);
    /// 3. so is a `ParentId`;
    /// 4. a `CollectionFolder` becomes its `PhysicalFolderIds`;
    /// 5. anything else a view could be resolves to nothing.
    ///
    /// Both of the views a real 10.11.8 database carries take one of the first two
    /// arms: its Live TV view is `ViewType: livetv`, and its Playlists view carries
    /// a `DisplayParentId` pointing at the real `PlaylistsFolder`.
    ///
    /// NOT ported: upstream's fifth arm, which groups libraries when the user has a
    /// non-empty `GroupedFolders` preference. That needs each collection folder's
    /// `CollectionType`, which this layer has no access to — see the task filed
    /// against it. A user without that preference (every user of a default install,
    /// and both users of the verification deployment) never reaches it.
    async fn top_parent_ids_for_query(
        db: &Database,
        id: Uuid,
        user: Option<&UserEntity>,
        depth: u8,
    ) -> Result<Option<Vec<Uuid>>, ServiceError> {
        // A `DisplayParentId`/`ParentId` chain is data, so it can be a cycle.
        // Upstream recurses without a guard and would stack-overflow; refusing to
        // follow a long chain is the same answer for every shape that is not a bug.
        if depth > 8 {
            return Ok(Some(Vec::new()));
        }
        let row: Option<ViewRow> = sqlx::query_as(
            r#"SELECT "Type", "ParentId", "Path", "Data", "TopParentId"
               FROM "BaseItems" WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .fetch_optional(db.pool())
        .await
        .map_err(db_err)?;
        let Some(ViewRow {
            type_name,
            parent_id,
            path,
            data,
            top_parent_id,
        }) = row
        else {
            return Ok(None);
        };
        let kind = crate::item_type_lookup::kind_from_type_name(&type_name);
        if kind == Some(BaseItemKind::CollectionFolder) {
            let folders = physical_folders_by_view(db, &[id])
                .await?
                .remove(&id)
                .unwrap_or_default();
            // A collection folder with NO physical folders means two different
            // things, and only one of them is "an empty library". On a
            // Ferrofin-written database no collection folder has them — items hang
            // off the folder directly and there is no `Data` blob — so answering
            // "match nothing" here would empty every native browse. Deliberate
            // divergence, same as the one `resolve_views` already documents: an
            // unresolvable collection folder falls through to the ancestor closure,
            // which is right for both database shapes.
            return Ok((!folders.is_empty()).then_some(folders));
        }
        if kind != Some(BaseItemKind::UserView) {
            // C#'s last arm — `item.GetTopParent()` — but only when we got here by
            // FOLLOWING a view's pointer. At the top level
            // `SetTopParentIdsOrAncestors` checks the type first and sends anything
            // that is not a view to the ancestor closure instead, so returning a
            // top parent there would silently change what an ordinary folder browse
            // scopes by.
            if depth == 0 {
                return Ok(None);
            }
            let top = top_parent_id
                .as_deref()
                .and_then(|v| Uuid::parse_str(v).ok())
                .filter(|v| !v.is_nil())
                // A row with no top parent IS the top: `GetTopParent` walks up and
                // stops at the first item with no parent, which is the row itself.
                .unwrap_or(id);
            return Ok(Some(vec![top]));
        }

        let blob: serde_json::Value = data
            .as_deref()
            .and_then(|d| serde_json::from_str(d).ok())
            .unwrap_or(serde_json::Value::Null);
        // `ViewType` is what C# tests; the path is the fallback for a view written
        // without a `Data` blob.
        let is_live_tv = blob
            .get("ViewType")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|v| v.eq_ignore_ascii_case("livetv"))
            || path
                .as_deref()
                .is_some_and(crate::user_view_manager::is_live_tv_view_path);
        if is_live_tv {
            return Ok(Some(vec![id]));
        }
        let display_parent = blob
            .get("DisplayParentId")
            .and_then(serde_json::Value::as_str)
            .and_then(|v| Uuid::parse_str(v).ok())
            .filter(|v| !v.is_nil());
        let parent = parent_id
            .as_deref()
            .and_then(|v| Uuid::parse_str(v).ok())
            .filter(|v| !v.is_nil());
        if let Some(next) = display_parent.or(parent) {
            // A dangling pointer resolves to nothing, NOT to the ancestor closure:
            // upstream returns `[]` when `GetItemById` misses.
            return Ok(Some(
                Box::pin(Self::top_parent_ids_for_query(db, next, user, depth + 1))
                    .await?
                    .unwrap_or_default(),
            ));
        }

        Box::pin(Self::grouped_library_ids(db, &blob, user, depth)).await
    }

    /// The libraries a user has GROUPED into this view — C#
    /// `GetTopParentIdsForQuery`'s fifth arm, reached by a `UserView` that has
    /// no `DisplayParentId` and no `ParentId` of its own.
    ///
    /// `user is not null && ViewType != unknown && IsEligibleForGrouping(ViewType)
    /// && user.GetPreference(GroupedFolders).Length > 0`, then the user's
    /// collection folders whose type matches the view (or that have none),
    /// keeping the ones the user actually grouped, each resolved in turn.
    ///
    /// Everything short of that resolves to nothing, which is upstream's answer
    /// too — a view that groups no library stands for no library.
    async fn grouped_library_ids(
        db: &Database,
        blob: &serde_json::Value,
        user: Option<&UserEntity>,
        depth: u8,
    ) -> Result<Option<Vec<Uuid>>, ServiceError> {
        let Some(user) = user else {
            return Ok(Some(Vec::new()));
        };
        let view_type = blob
            .get("ViewType")
            .and_then(serde_json::Value::as_str)
            .map(str::to_ascii_lowercase);
        // `movies`, `tvshows`, and an untyped view — `UserView.cs:22`. A view of any
        // other type is never a grouping of libraries.
        if !matches!(view_type.as_deref(), Some("movies" | "tvshows")) {
            return Ok(Some(Vec::new()));
        }
        let grouped = guid_preference(db.pool(), &user.id, PreferenceKind::GroupedFolders).await?;
        if grouped.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let mut scope = Vec::new();
        for folder in visible_views(db, user).await? {
            if !grouped.contains(&folder) {
                continue;
            }
            // `CollectionType is null || CollectionType == view.ViewType` — an
            // untyped library groups into any view.
            let folder_type = Self::collection_type_of(db, folder).await?;
            if folder_type.is_some() && folder_type != view_type {
                continue;
            }
            match Box::pin(Self::top_parent_ids_for_query(
                db,
                folder,
                Some(user),
                depth + 1,
            ))
            .await?
            {
                Some(ids) if !ids.is_empty() => scope.extend(ids),
                // A library that resolves to nothing stands for ITSELF — the
                // same accommodation `scope_to_user_libraries` makes, and for
                // the same reason: a Ferrofin-written collection folder has no
                // `PhysicalFolderIds`, and its items hang off it directly.
                _ => scope.push(folder),
            }
        }
        Ok(Some(scope))
    }

    /// A library's `CollectionType`, from the same `Data` blob that carries its
    /// `PhysicalFolderIds` — `movies`, `tvshows`, … or `None` for an untyped
    /// library, which upstream treats as matching any view.
    async fn collection_type_of(db: &Database, id: Uuid) -> Result<Option<String>, ServiceError> {
        let data: Option<Option<String>> =
            sqlx::query_scalar(r#"SELECT "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_optional(db.pool())
                .await
                .map_err(db_err)?;
        Ok(data
            .flatten()
            .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
            .and_then(|v| {
                v.get("CollectionType")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_ascii_lowercase)
            }))
    }

    /// The physical folders each of `ids` stands for, for those that are
    /// Jellyfin collection folders. Ids that are not — every id on a
    /// Ferrofin-written database — are simply absent from the map.
    ///
    /// One statement for the whole set, and restricted to the collection-folder
    /// type: only those rows carry `PhysicalFolderIds` (7 of 7 on the real
    /// library, no other type), so without the guard every ordinary browse of a
    /// series or a folder would read and JSON-parse that item's `Data` blob to
    /// learn nothing.
    async fn physical_folders_by_view(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<Uuid>>, ServiceError> {
        physical_folders_by_view(&self.db, ids).await
    }

    /// Runs a translated query in the requested shape, returning full rows.
    async fn fetch_rows(
        &self,
        filter: &InternalItemsQuery,
        shape: QueryShape,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let resolved = self.resolve_views(filter).await?;
        self.fetch_rows_resolved(resolved.as_ref().unwrap_or(filter), shape)
            .await
    }

    /// [`Self::fetch_rows`] for a filter whose views are already resolved.
    ///
    /// Resolving twice is harmless — it is idempotent — but it is a wasted
    /// round trip, and `get_items` runs both this and the count off one scope.
    async fn fetch_rows_resolved(
        &self,
        filter: &InternalItemsQuery,
        shape: QueryShape,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let mut qb = build_query(filter, shape);
        let rows = qb
            .build_query_as::<BaseItemEntity>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows)
    }

    /// Runs a translated query returning only the id and clean-name columns.
    ///
    /// Same statement as [`Self::fetch_rows`] but for the projection, so the
    /// rows — and their order — are identical to what `get_item_list` would
    /// have returned.
    async fn fetch_id_clean_names(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<(String, Option<String>)>, ServiceError> {
        let resolved = self.resolve_views(filter).await?;
        let filter = resolved.as_ref().unwrap_or(filter);
        let mut qb = build_query(filter, QueryShape::IdAndCleanName);
        qb.build_query_as::<(String, Option<String>)>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
    }

    /// Runs a translated query returning only the id column.
    async fn fetch_ids(&self, filter: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        let resolved = self.resolve_views(filter).await?;
        let filter = resolved.as_ref().unwrap_or(filter);
        let mut qb = build_query(filter, QueryShape::IdsOnly);
        let ids: Vec<String> = qb
            .build_query_scalar::<String>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(ids.iter().filter_map(|s| Uuid::parse_str(s).ok()).collect())
    }

    /// Runs a `COUNT(*)` over the translated query, for a filter whose views
    /// are already resolved.
    ///
    /// Its one caller — `get_items` — resolves once for both the page and the
    /// count, so there is no resolving wrapper here to tempt a second lookup.
    async fn fetch_count_resolved(&self, filter: &InternalItemsQuery) -> Result<i32, ServiceError> {
        let mut qb = build_query(filter, QueryShape::GroupedCount);
        let count: i64 = qb
            .build_query_scalar::<i64>()
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(i32::try_from(count).unwrap_or(i32::MAX))
    }

    /// Distinct `ItemValues.Value`s of the given types, optionally scoped to items
    /// of certain stored `Type`s (C# `GetItemValueNames`).
    async fn item_value_names(
        &self,
        types: &[ItemValueType],
        with_item_types: &[&str],
        exclude_item_types: &[&str],
    ) -> Result<Vec<String>, ServiceError> {
        let type_ints: Vec<i64> = types.iter().map(|t| i64::from(i32::from(*t))).collect();
        let mut sql = String::from(
            r#"SELECT DISTINCT iv."Value" FROM "ItemValuesMap" ivm
               JOIN "ItemValues" iv ON iv."ItemValueId" = ivm."ItemValueId"
               JOIN "BaseItems" bi ON bi."Id" = ivm."ItemId"
               WHERE iv."Type" IN ("#,
        );
        sql.push_str(&placeholders(type_ints.len()));
        sql.push(')');
        if !with_item_types.is_empty() {
            sql.push_str(r#" AND bi."Type" IN ("#);
            sql.push_str(&placeholders(with_item_types.len()));
            sql.push(')');
        }
        if !exclude_item_types.is_empty() {
            sql.push_str(r#" AND bi."Type" NOT IN ("#);
            sql.push_str(&placeholders(exclude_item_types.len()));
            sql.push(')');
        }
        sql.push_str(r#" ORDER BY iv."Value""#);

        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for t in &type_ints {
            query = query.bind(*t);
        }
        for t in with_item_types {
            query = query.bind((*t).to_owned());
        }
        for t in exclude_item_types {
            query = query.bind((*t).to_owned());
        }
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }

    /// Fallback total for by-name pagination when the page is empty (past the
    /// last row). Accepts the SAME pre-computed vectors that the page query
    /// built, so the filter shape can never drift.
    async fn count_by_name_total(
        &self,
        scope: &ByNameScope<'_>,
        return_type: &str,
        filter: &InternalItemsQuery,
    ) -> Result<i32, ServiceError> {
        // `representativeIds.Count()` (ByName.cs:229-232), not a row count:
        // upstream counts the COLLAPSED set, so the total has to be the number
        // of `PresentationUniqueKey` GROUPS the filtered rows form — the same
        // grouping the page query applies. `COUNT(DISTINCT key)` would be
        // wrong: it drops NULL keys entirely, and the whole point of the
        // grouping is that the unkeyed rows form ONE group.
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new(r#"SELECT COUNT(*) FROM (SELECT 1 FROM "BaseItems" AS bi JOIN "#);
        // No per-kind counts here: this query only counts by-name rows, so the
        // seven conditional sums would be computed and thrown away. The shared
        // `scope` still guarantees the WHERE cannot drift from the page query.
        push_value_aggregate(&mut qb, scope, &[]);
        push_by_name_join(&mut qb, return_type);
        append_by_name_filters(&mut qb, filter);
        qb.push(r#" GROUP BY bi."PresentationUniqueKey")"#);
        let count: i64 = qb
            .build_query_scalar()
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(i32::try_from(count).unwrap_or(i32::MAX))
    }

    /// Resolves the by-name items of `kind` to [`ItemWithCounts`], counting the
    /// content items that reference each via `ItemValues` of the given types
    /// (port of C# `GetItemValues`).
    ///
    /// Scoped to the browse's `parent_id` (via [`InternalItemsQuery::ancestor_ids`])
    /// and `include_item_types`: the Movies "Genres" tab lists only genres carried
    /// by movies, the TV "Networks" tab only studios carried by items under the TV
    /// library, each with an in-scope item count — matching Jellyfin, which scopes
    /// its by-name aggregates to the query. Only values with an in-scope item (and
    /// a materialized by-name row) appear.
    async fn item_values_with_counts(
        &self,
        value_types: &[ItemValueType],
        return_type: BaseItemKind,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        // Every kind the six callers pass (`Genre`, `MusicGenre`, `Studio`,
        // `MusicArtist`) is in the lookup table. A kind that were not would
        // fall back to `""`, which no `Type` column holds — an empty result,
        // which is the right way for this to fail.
        let return_type = stored_type_name(return_type).unwrap_or_default();
        let type_ints: Vec<i64> = value_types
            .iter()
            .map(|t| i64::from(i32::from(*t)))
            .collect();
        // Content-item scoping. `GetItemValues` builds its inner query from
        // `IncludeItemTypes` ALONE (`BaseItemRepository.ByName.cs:154-168` on
        // master, `BaseItemRepository.cs:1258-1275` on v10.11.8 — the two trees
        // are structurally identical here). There is NO owner-kind restriction
        // on either `GetGenres` or `GetMusicGenres`: both are
        // `GetItemValues(filter, _getGenreValueTypes, <returnType>)` and differ
        // only in the by-name row `Type` the outer `.Where(e => e.Type ==
        // returnType)` selects. The owner-type split lives solely on the
        // *Names* variants (`GetItemValueNames`, ByName.cs:120-141), which
        // `get_genre_names`/`get_music_genre_names` still carry.
        let content_type_names: Vec<String> = filter
            .include_item_types
            .iter()
            .filter_map(|k| stored_type_name(*k).map(str::to_owned))
            .collect();
        let ancestors: Vec<String> = filter
            .ancestor_ids
            .iter()
            .copied()
            .map(guid_to_db)
            .collect();
        // `AddUserToQuery` for the by-name tabs. A no-op for a query that is
        // already scoped (a browse under one library passes `ancestor_ids`).
        let scoped = scope_to_user_libraries(&self.db, filter).await?;
        let top_parents: Vec<String> = scoped
            .as_ref()
            .map(|f| f.top_parent_ids.iter().copied().map(guid_to_db).collect())
            .unwrap_or_default();

        let want_total = filter.enable_total_record_count && filter.limit.is_some();
        // Master computes the per-kind counts only when the caller named item
        // types (`if (filter.IncludeItemTypes.Length > 0)`, ByName.cs:249),
        // so the aggregate carries the seven conditional sums only then — the
        // by-name browse that asks for no counts pays nothing for them.
        let count_types = per_kind_count_types(!content_type_names.is_empty());
        // Two levels, because the representative collapse sits BETWEEN the
        // filtered row set and the ordering/paging (see `push_representative_rank`).
        // The outer level is where `COUNT(*) OVER()` belongs: run in the inner
        // one it would count the rows BEFORE the collapse, i.e. report a total
        // larger than the number of rows any amount of paging could ever return.
        let mut select = String::from(r"SELECT bi.*");
        if want_total {
            select.push_str(r#", COUNT(*) OVER() AS "total_count""#);
        }
        select.push_str(r" FROM (SELECT bi.*, agg.cnt");
        for (alias, _) in &count_types {
            select.push_str(", agg.");
            select.push_str(alias);
        }
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(select);
        let scope = ByNameScope {
            type_ints: &type_ints,
            content_type_names: &content_type_names,
            ancestors: &ancestors,
            top_parents: &top_parents,
        };
        push_representative_rank(&mut qb, return_type, &top_parents);
        qb.push(r#" FROM "BaseItems" AS bi JOIN "#);
        push_value_aggregate(&mut qb, &scope, &count_types);
        push_by_name_join(&mut qb, return_type);
        append_by_name_filters(&mut qb, filter);
        qb.push(r#") AS bi WHERE bi."rep_rank" = 1"#);
        // C# `GetItemValues` runs the same `ApplyOrder(query, filter, context)`
        // the main browse does, so the caller's `sortBy`/`sortOrder` apply and
        // the no-`sortBy` default is `OrderBy(e => e.SortName)` — not `Name`.
        // It runs on the COLLAPSED set (`ApplyOrder` is applied to the query
        // over `representativeIds`, ByName.cs:232-236), which is why it is
        // pushed at this level and not inside the derived table. Every
        // expression `append_order_by` emits is `bi."<column>"`, and the
        // derived table is aliased `bi` and carries `bi.*`, so they still
        // resolve.
        crate::translate_query::append_order_by(&mut qb, filter);
        if let Some(limit) = filter.limit {
            qb.push(" LIMIT ").push_bind(i64::from(limit));
            let offset = filter.start_index.unwrap_or(0);
            if offset > 0 {
                qb.push(" OFFSET ").push_bind(i64::from(offset));
            }
        } else if let Some(offset) = filter.start_index.filter(|o| *o > 0) {
            qb.push(" LIMIT -1 OFFSET ").push_bind(i64::from(offset));
        }

        let rows = qb
            .build_query_as::<ByNameCountRow>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;

        // Total: C# `GetItemValues` forces EnableTotalRecordCount off when there is
        // no Limit and then never assigns `TotalRecordCount` — a non-nullable int —
        // so every unpaged (or count-disabled) by-name response carries
        // `TotalRecordCount: 0` on the wire, even with a bare StartIndex. Match
        // that exactly; the ledger's flagged /Genres//Studios read-diffs were this.
        let start_index = filter.start_index.unwrap_or(0);
        let total = if want_total {
            match rows.first() {
                Some(r) => i32::try_from(r.total_count).unwrap_or(i32::MAX),
                None if start_index > 0 => {
                    self.count_by_name_total(&scope, return_type, filter)
                        .await?
                }
                None => 0,
            }
        } else {
            0
        };
        let items: Vec<ItemWithCounts> = rows
            .into_iter()
            .map(|r| ItemWithCounts {
                item: r.item,
                // Port of master's `BuildItemCountsByCleanName` (ByName.cs:283-338):
                // one `ItemCounts` per `CleanName`, each field the number of
                // in-scope content items of that kind carrying the value.
                // v10.11.8 (BaseItemRepository.cs:1368-1392, comment "TODO: This
                // is bad refactor!") instead hands EVERY row the same
                // uncorrelated totals; master fixed it, so Ferrofin follows
                // master. `ItemCount` and `ProgramCount` are left at the
                // aggregate/zero because neither tree assigns them — see
                // `by_name::project_query_result`.
                counts: ferrofin_model::dto::ItemCounts {
                    item_count: i32::try_from(r.cnt).unwrap_or(i32::MAX),
                    series_count: clamp_count(r.series_count),
                    episode_count: clamp_count(r.episode_count),
                    movie_count: clamp_count(r.movie_count),
                    album_count: clamp_count(r.album_count),
                    artist_count: clamp_count(r.artist_count),
                    song_count: clamp_count(r.song_count),
                    trailer_count: clamp_count(r.trailer_count),
                    ..Default::default()
                },
            })
            .collect();
        Ok(QueryResult::new(Some(start_index), Some(total), items))
    }
}

/// A by-name row plus its in-scope item count, read from the joined by-name
/// aggregate query (the `cnt` column is the aggregate's per-value `COUNT(*)`).
/// `total_count` carries the `COUNT(*) OVER()` window-function total when the
/// caller needs pagination metadata, avoiding a separate COUNT round-trip.
#[derive(sqlx::FromRow)]
struct ByNameCountRow {
    #[sqlx(flatten)]
    item: BaseItemEntity,
    cnt: i64,
    #[sqlx(default)]
    total_count: i64,
    /// The seven per-kind counts of master's `BuildItemCountsByCleanName`.
    /// `#[sqlx(default)]` because the aggregate emits them only when the
    /// caller named item types — the browse that wants no counts does not
    /// pay for seven conditional sums.
    #[sqlx(default)]
    series_count: i64,
    #[sqlx(default)]
    episode_count: i64,
    #[sqlx(default)]
    movie_count: i64,
    #[sqlx(default)]
    album_count: i64,
    #[sqlx(default)]
    artist_count: i64,
    #[sqlx(default)]
    song_count: i64,
    #[sqlx(default)]
    trailer_count: i64,
}

/// The `(column alias, stored type name)` pairs of master's per-kind by-name
/// counts, or nothing when the caller named no item types.
///
/// Port of the seven `BaseItemKindNames[...]` locals master's
/// `BuildItemCountsByCleanName` reads (`BaseItemRepository.ByName.cs:304-310`).
/// A kind missing from the lookup table simply contributes no column, which
/// leaves its count at the `0` default — the same answer as counting nothing.
fn per_kind_count_types(wanted: bool) -> Vec<(&'static str, &'static str)> {
    if !wanted {
        return Vec::new();
    }
    [
        ("series_count", BaseItemKind::Series),
        ("episode_count", BaseItemKind::Episode),
        ("movie_count", BaseItemKind::Movie),
        ("album_count", BaseItemKind::MusicAlbum),
        ("artist_count", BaseItemKind::MusicArtist),
        ("song_count", BaseItemKind::Audio),
        ("trailer_count", BaseItemKind::Trailer),
    ]
    .into_iter()
    .filter_map(|(alias, kind)| stored_type_name(kind).map(|name| (alias, name)))
    .collect()
}

/// Narrows one aggregate count to the `i32` the DTO field is.
fn clamp_count(n: i64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Pushes the value-count aggregate as a derived table `agg(cval, cnt)`: for
/// each distinct `CleanValue` of one of `type_ints`, the count of in-scope
/// content items that carry it, scoped by content-type include/exclude and the
/// browse's `ancestors`. Shared by the page query and the total-count query so
/// their WHERE stays identical (C# `GetItemValues` inner filter).
///
/// Grouped by `CleanValue`, not `ItemValueId`, because `CleanValue` is the key
/// the by-name row is found by — see the `ON` clauses that consume this. Two
/// `ItemValues` rows can differ only in capitalization (`IX_ItemValues_Type_Value`
/// is unique on the raw `Value`), and they are one genre to a client, so
/// collapsing them here is also what stops a single by-name row being joined
/// twice.
fn push_value_aggregate<'a>(
    qb: &mut QueryBuilder<'a, Sqlite>,
    scope: &'a ByNameScope<'a>,
    count_types: &[(&'static str, &'static str)],
) {
    let ByNameScope {
        type_ints,
        content_type_names,
        ancestors,
        top_parents,
    } = scope;
    // `COUNT(*)`, not `COUNT(DISTINCT ivm."ItemId")`: `ItemValuesMap`'s primary
    // key IS `("ItemValueId", "ItemId")`, so within one `GROUP BY
    // iv."ItemValueId"` every `ItemId` is already distinct, and the `ci` join is
    // 1:1 on `BaseItems`'s primary key so it cannot duplicate a map row either.
    // The `DISTINCT` was therefore never able to remove a row — it only bought
    // SQLite a `USE TEMP B-TREE FOR count(DISTINCT)` per group, whose cost grows
    // with the number of items sharing a genre/studio. Row-identical on the
    // bench library; the statement behind `/Studios` measures 0.407 ms → 0.366,
    // `/Items/Filters2` 0.566 → 0.490.
    qb.push(r#"(SELECT iv."CleanValue" AS cval, COUNT(*) AS cnt"#);
    // Master's per-`CleanName` counts, folded into the SAME group-by rather
    // than run as a second dictionary query: `SUM(ci."Type" = ?)` is SQLite's
    // spelling of `g.Where(x => x.Type == seriesTypeName).Sum(x => x.Count)`.
    for (alias, type_name) in count_types {
        qb.push(r#", SUM(ci."Type" = "#)
            .push_bind((*type_name).to_owned())
            .push(") AS ")
            .push(*alias);
    }
    qb.push(
        r#"
           FROM "ItemValues" iv
           JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
           JOIN "BaseItems" ci ON ci."Id" = ivm."ItemId"
           WHERE "#,
    );
    push_in_list(qb, r#"iv."Type""#, type_ints);
    if !content_type_names.is_empty() {
        qb.push(" AND ");
        push_in_list(qb, r#"ci."Type""#, content_type_names);
    }
    if !ancestors.is_empty() {
        qb.push(r#" AND EXISTS (SELECT 1 FROM "AncestorIds" a WHERE a."ItemId" = ci."Id" AND "#);
        push_in_list(qb, r#"a."ParentItemId""#, ancestors);
        qb.push(")");
    }
    // The user's libraries. Upstream reaches this through `TranslateQuery` on
    // the inner item query, which `AddUserToQuery` has already confined; here
    // the caller resolves the scope and passes it down. Without it the
    // by-name tabs aggregate over the WHOLE server, so a restricted account is
    // offered a genre or a studio that `/Items` will then refuse to return —
    // a filter list that lies about what is behind it.
    if !top_parents.is_empty() {
        qb.push(" AND ");
        push_in_list(qb, r#"ci."TopParentId""#, top_parents);
    }
    qb.push(r#" GROUP BY iv."CleanValue") AS agg"#);
}

/// The pre-computed vectors that shape a by-name aggregate.
///
/// One struct so the page query and the paging-fallback count cannot drift
/// apart: they take the SAME value, and a filter added to one is a filter in
/// both.
struct ByNameScope<'a> {
    /// The `ItemValues.Type` numbers this tab aggregates.
    type_ints: &'a [i64],
    /// Stored type names of the content items that may contribute — the
    /// caller's `IncludeItemTypes` and nothing else, exactly as C# builds its
    /// `innerQueryFilter`.
    content_type_names: &'a [String],
    /// The browse's ancestor scope, if it is under a parent.
    ancestors: &'a [String],
    /// The user's libraries (`AddUserToQuery`), empty when unconfined.
    top_parents: &'a [String],
}

/// The representative-selection window — the port of `GetItemValues`'s
/// `GroupBy(e => e.PresentationUniqueKey)` collapse.
///
/// Both trees perform it and neither has changed it in spirit:
/// master `BaseItemRepository.ByName.cs:213-224` takes `g.Min(e => e.Id)`, and
/// v10.11.8 `BaseItemRepository.cs:1313-1316` takes `g.FirstOrDefault().Id`.
/// Ferrofin follows master, whose `Min(Id)` is deterministic where 10.11.8's
/// `FirstOrDefault` is whatever the engine emitted first.
///
/// What the collapse is FOR, in upstream's own words at that site: rows that
/// share a key are alternate versions of one thing, and "for MusicArtist,
/// prefer the entity from a library the user can actually access, since the
/// same artist can have a folder in multiple libraries". Without it a
/// two-library server lists the same artist twice, and a merged film's
/// alternate shows up beside its primary — so this is not a genre-tab detail.
///
/// Two things the SQL has to get right:
///
/// - `PARTITION BY` groups NULLs together, exactly as SQLite's `GROUP BY` and
///   as EF translates `GroupBy` — so a set of by-name rows that never got a
///   `PresentationUniqueKey` collapses to ONE row. That is faithful, and it is
///   also why [`crate::item_persistence_service::backfill_missing_presentation_keys`]
///   exists: reproducing upstream's collapse without also reproducing
///   upstream's keys would silently delete names from the tab.
/// - The window runs over the FILTERED row set (it is in the same SELECT as the
///   join and the `WHERE`), which is what upstream does — it groups
///   `masterQuery`, i.e. the inner query after `TranslateQuery` applied
///   `outerQueryFilter`. Grouping over the unfiltered table instead would let a
///   row excluded by `nameStartsWith` suppress the representative that was not.
///
/// `MIN(Id)` is expressed as `ROW_NUMBER() ... ORDER BY bi."Id"` so the chosen
/// row's own columns come back with it; ids are stored as the uppercase
/// hyphenated GUID text, so the ordering is the text ordering of that form.
fn push_representative_rank(
    qb: &mut QueryBuilder<'_, Sqlite>,
    return_type: &str,
    top_parents: &[String],
) {
    qb.push(r#", ROW_NUMBER() OVER (PARTITION BY bi."PresentationUniqueKey" ORDER BY "#);
    // `isMusicArtist` arm (ByName.cs:215-222): an artist that exists in a
    // library the caller can reach outranks the same artist's folder in one
    // they cannot, and `Id` is only the tiebreaker. Upstream applies it
    // unconditionally for MusicArtist; with no `TopParentIds` the `CASE` would
    // be a constant, so it is omitted rather than emitted as dead SQL.
    if return_type == stored_type_name(BaseItemKind::MusicArtist).unwrap_or("\u{0}")
        && !top_parents.is_empty()
    {
        qb.push(r#"CASE WHEN bi."TopParentId" IN ("#);
        let mut sep = qb.separated(", ");
        for t in top_parents {
            sep.push_bind(t.clone());
        }
        qb.push(") THEN 0 ELSE 1 END, ");
    }
    qb.push(r#"bi."Id") AS "rep_rank""#);
}

/// Joins the value aggregate to the by-name rows it describes.
///
/// Port of the C# outer filter, which is **two** clauses —
/// `.Where(e => e.Type == returnType).Where(e => itemValuesQuery.Contains(e.CleanName))`
/// (`BaseItemRepository.GetItemValues`). Both are load-bearing:
///
/// - `CleanName` is the only link on an adopted database, where
///   `ItemValues.ItemValueId` is a synthetic guid matching no item at all.
/// - `Type` is what keeps the answer to *this* browse. Without it a Movie or an
///   Episode named "Drama" is returned by `/Genres` beside the real genre, and
///   `/Genres` and `/MusicGenres` — which query the same `ItemValueType` and
///   differ only by return kind — stop being distinguishable. It is also the
///   leading column of `FerrofinIX_BaseItems_Type_CleanName`, so constraining
///   it keeps this an index seek instead of a scan of every item.
fn push_by_name_join(qb: &mut QueryBuilder<'_, Sqlite>, return_type: &str) {
    qb.push(r#" ON agg.cval = bi."CleanName" AND bi."Type" = "#)
        .push_bind(return_type.to_owned())
        .push(" WHERE 1 = 1");
}

/// The physical folders each of `ids` stands for, for those that are Jellyfin
/// collection folders — see
/// [`FerrofinItemRepository::physical_folders_by_view`], which is the doc for
/// why this translation exists at all.
///
/// A free function because the child-count service needs the same one: a
/// library's `ChildCount` is its physical folders' children, and grouping on
/// the raw `ParentId` reports 0 for every library on an adopted database.
pub(crate) async fn physical_folders_by_view(
    db: &Database,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Uuid>>, ServiceError> {
    let Some(collection_folder) = stored_type_name(BaseItemKind::CollectionFolder) else {
        return Ok(HashMap::new());
    };
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        r#"SELECT "Id", "Data" FROM "BaseItems" WHERE "Data" IS NOT NULL AND "Type" = "#,
    );
    qb.push_bind(collection_folder).push(" AND ");
    push_in_list(&mut qb, r#""Id""#, &to_guid_strings(ids));
    let rows: Vec<(String, String)> = qb
        .build_query_as::<(String, String)>()
        .fetch_all(db.pool())
        .await
        .map_err(db_err)?;
    Ok(rows
        .iter()
        .filter_map(|(id, blob)| {
            let folders = parse_physical_folder_ids(blob);
            if folders.is_empty() {
                return None;
            }
            Some((Uuid::parse_str(id).ok()?, folders))
        })
        .collect())
}

/// Reads `PhysicalFolderIds` out of a Jellyfin `BaseItems.Data` blob.
///
/// The ids are written N-format (32 lowercase hex, no hyphens) where the `Id`
/// column is uppercase and hyphenated; `Uuid::parse_str` accepts both, and
/// every comparison downstream goes through `guid_to_db`, so the two spellings
/// never meet. A blob that is not a `CollectionFolder`'s — or is not JSON at
/// all — simply yields nothing.
fn parse_physical_folder_ids(blob: &str) -> Vec<Uuid> {
    serde_json::from_str::<serde_json::Value>(blob)
        .ok()
        .as_ref()
        .and_then(|v| v.get("PhysicalFolderIds"))
        .and_then(serde_json::Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(serde_json::Value::as_str)
                .filter_map(|s| Uuid::parse_str(s).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The characters C# `BaseItemRepository.SearchWildcardTerms` treats as "the
/// caller is doing a wildcard search" — their presence routes `SearchTerm` to a
/// raw (unescaped) `LIKE` instead of a literal contains-match.
const SEARCH_WILDCARD_TERMS: &[char] = &['%', '_', '[', ']', '^'];

/// Hard cap on the ancestor-chain CTE depth — prevents unbounded recursion on
/// a cyclic `ParentId` chain (real libraries are <10 levels deep).
const MAX_ANCESTOR_DEPTH: i32 = 32;

/// The one query behind `GET /Playlists/{id}/Items`: every member row of the
/// playlist, in link order, each carrying the caller's access columns.
///
/// Collapsing the read into a single statement is load-bearing under open-loop
/// load — the previous shape took one reader-pool connection for the access
/// check, another for the child-id list, and another for the `IN (…)` detail
/// fetch, so a single request queued three times on a pool sized to the core
/// count. `?1` = playlist id, `?2` = caller, `?3` = the linked-child type.
///
/// `pl` proves the playlist's own `BaseItems` row exists (a member edge without
/// its container must still read as missing, exactly as the access query does);
/// `p`/`s` are LEFT-joined so a legacy playlist with no meta row and a caller
/// with no share row both come back as NULLs the playlist manager interprets.
const PLAYLIST_ITEMS_SQL: &str = r#"SELECT ch.*,
       p."OwnerUserId" AS "PlaylistOwnerUserId",
       p."OpenAccess"  AS "PlaylistOpenAccess",
       s."CanEdit"     AS "PlaylistShareCanEdit"
   FROM "FerrofinLinkedChildren" lc
   JOIN "BaseItems" pl ON pl."Id" = lc."ParentId"
   JOIN "BaseItems" ch ON ch."Id" = lc."ChildId"
   LEFT JOIN "FerrofinPlaylists" p ON p."PlaylistId" = lc."ParentId"
   LEFT JOIN "FerrofinPlaylistShares" s
          ON s."PlaylistId" = lc."ParentId" AND s."UserId" = ?2
   WHERE lc."ParentId" = ?1 AND lc."ChildType" = ?3
   ORDER BY lc."SortOrder""#;

/// One row of [`PLAYLIST_ITEMS_SQL`]: a member's `BaseItems` row plus the three
/// access columns (identical on every row of a given playlist).
struct PlaylistItemRow {
    /// The member item row.
    item: BaseItemEntity,
    /// `FerrofinPlaylists.OwnerUserId` (NULL for a legacy or API-key playlist).
    owner: Option<String>,
    /// `FerrofinPlaylists.OpenAccess` (NULL only when the meta row is absent).
    open_access: Option<i64>,
    /// The caller's `FerrofinPlaylistShares.CanEdit` (NULL when not shared).
    can_edit: Option<i64>,
}

impl<'r> FromRow<'r, sqlx::sqlite::SqliteRow> for PlaylistItemRow {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            item: BaseItemEntity::from_row(row)?,
            owner: row.try_get("PlaylistOwnerUserId")?,
            open_access: row.try_get("PlaylistOpenAccess")?,
            can_edit: row.try_get("PlaylistShareCanEdit")?,
        })
    }
}

/// Appends the caller's name filters against the by-name `bi` row (C#
/// `TranslateQuery`'s name predicates on the outer query).
///
/// - `SearchTerm` ports the C# two-branch shape: a term containing a wildcard
///   character goes through `LIKE '%term%'` **unescaped** (client wildcards pass
///   through, as upstream intends); a plain term is a literal contains-match.
///   Deliberate divergence: the plain term is cleaned (`get_clean_value`) before
///   matching `CleanName` — C# compares the raw lowered term against the cleaned
///   column, which makes e.g. hyphenated searches miss; Ferrofin stays correct.
/// - `NameStartsWith` is a prefix on the sort key. Deliberate divergence:
///   `COALESCE(SortName, Name)` because Ferrofin leaves by-name `SortName` NULL
///   (C# filters `SortName` alone, which would match nothing here).
/// - `NameStartsWithOrGreater`/`NameLessThan` port C#'s **first-character**
///   comparison (`SortName.FirstOrDefault() > x[0] || Name.FirstOrDefault() >
///   x[0]`), not a full-string range: `substr(...,1,1)` on both columns, OR'd,
///   binary collation — byte-identical to the C# char compare for ASCII.
fn append_by_name_filters<'a>(qb: &mut QueryBuilder<'a, Sqlite>, filter: &'a InternalItemsQuery) {
    let non_blank = |v: &'a Option<String>| v.as_deref().map(str::trim).filter(|s| !s.is_empty());
    // Favorite state lives in the by-name item's own `UserData` row (C# joins
    // `UserData` on the by-name item id) — same predicate the main browse uses.
    if let (Some(user_id), Some(want)) = (filter.user_id(), filter.is_favorite) {
        crate::translate_query::push_user_data_exists(
            qb,
            &guid_to_db(user_id),
            r#"ud."IsFavorite" = 1"#,
            want,
        );
    }
    if let Some(term) = non_blank(&filter.search_term) {
        let lowered = term.to_lowercase();
        if lowered.contains(SEARCH_WILDCARD_TERMS) {
            let like = format!("%{}%", lowered.trim_matches('%'));
            qb.push(r#" AND lower(bi."CleanName") LIKE "#)
                .push_bind(like);
        } else {
            let like = format!(
                "%{}%",
                crate::text_util::get_clean_value(term).trim_matches('%')
            );
            qb.push(r#" AND bi."CleanName" LIKE "#).push_bind(like);
        }
    }
    if let Some(prefix) = non_blank(&filter.name_starts_with) {
        qb.push(r#" AND lower(COALESCE(bi."SortName", bi."Name")) LIKE "#)
            .push_bind(format!("{}%", prefix.to_lowercase()));
    }
    // Both bounds are FULL-STRING comparisons against the lowercased parameter,
    // over `SortName` only (C# `BaseItemRepository.cs:2036-2046`:
    // `SortName!.CompareTo(param.ToLowerInvariant()) >= 0` / `< 0`). Comparing
    // only the first character, case-sensitively, made `nameLessThan=Jb` and
    // `nameStartsWithOrGreater=j` return the wrong page — and `>` instead of
    // `>=` dropped the boundary row itself.
    if let Some(boundary) = non_blank(&filter.name_starts_with_or_greater) {
        qb.push(r#" AND bi."SortName" >= "#)
            .push_bind(boundary.to_lowercase());
    }
    if let Some(boundary) = non_blank(&filter.name_less_than) {
        qb.push(r#" AND bi."SortName" < "#)
            .push_bind(boundary.to_lowercase());
    }
}

/// The `ItemValues` types treated as "genre" (C# `_getGenreValueTypes`).
const GENRE_TYPES: &[ItemValueType] = &[ItemValueType::Genre];
/// The `ItemValues` types treated as "studio" (C# `_getStudiosValueTypes`).
const STUDIO_TYPES: &[ItemValueType] = &[ItemValueType::Studios];
/// The `ItemValues` types treated as "artist" (C# `_getArtistValueTypes`).
const ARTIST_TYPES: &[ItemValueType] = &[ItemValueType::Artist];
/// The `ItemValues` types treated as "album artist" (C# `_getAlbumArtistValueTypes`).
const ALBUM_ARTIST_TYPES: &[ItemValueType] = &[ItemValueType::AlbumArtist];
/// All artist-ish `ItemValues` types (C# `_getAllArtistsValueTypes`).
const ALL_ARTIST_TYPES: &[ItemValueType] = &[ItemValueType::Artist, ItemValueType::AlbumArtist];

/// Maps the `BaseItemImageInfos.ImageType` integer discriminant to the wire
/// [`ImageType`]. The discriminants are the fixed `ImageInfoImageType` values and
/// line up 1:1 with [`ImageType`]; an out-of-range value falls back to
/// [`ImageType::Primary`] (the C# default when parsing a legacy row).
fn image_type_from_disc(disc: i32) -> ImageType {
    match disc {
        1 => ImageType::Art,
        2 => ImageType::Backdrop,
        3 => ImageType::Banner,
        4 => ImageType::Logo,
        5 => ImageType::Thumb,
        6 => ImageType::Disc,
        7 => ImageType::Box,
        8 => ImageType::Screenshot,
        9 => ImageType::Menu,
        10 => ImageType::Chapter,
        11 => ImageType::BoxRear,
        12 => ImageType::Profile,
        _ => ImageType::Primary,
    }
}

/// Maps a wire [`ImageType`] back to its `BaseItemImageInfos.ImageType` integer
/// discriminant — the inverse of [`image_type_from_disc`].
///
/// The discriminants line up 1:1 with the C# `ImageType` declaration order.
pub(crate) fn image_type_to_disc(image_type: ImageType) -> i32 {
    match image_type {
        ImageType::Primary => 0,
        ImageType::Art => 1,
        ImageType::Backdrop => 2,
        ImageType::Banner => 3,
        ImageType::Logo => 4,
        ImageType::Thumb => 5,
        ImageType::Disc => 6,
        ImageType::Box => 7,
        ImageType::Screenshot => 8,
        ImageType::Menu => 9,
        ImageType::Chapter => 10,
        ImageType::BoxRear => 11,
        ImageType::Profile => 12,
    }
}

/// Projects a persisted [`BaseItemImageInfoEntity`] row into an
/// [`ItemImageInfo`].
///
/// The stored `Blurhash` is a UTF-8 byte blob; an empty blob (or one that is not
/// valid UTF-8) becomes [`None`]. A zero/negative width or height (the "unknown"
/// sentinel) is preserved as-is; the API layer nulls those out per Jellyfin.
fn image_info_from_row(row: BaseItemImageInfoEntity) -> ItemImageInfo {
    let blur_hash = row
        .blurhash
        .filter(|b| !b.is_empty())
        .and_then(|b| String::from_utf8(b).ok());
    ItemImageInfo {
        path: row.path,
        image_type: image_type_from_disc(row.image_type),
        date_modified: row.date_modified.unwrap_or_else(default_epoch),
        width: i32::try_from(row.width).unwrap_or(0),
        height: i32::try_from(row.height).unwrap_or(0),
        blur_hash,
    }
}

/// The Unix epoch as a UTC timestamp — the placeholder for a row with no stored
/// `DateModified` (C# leaves the `default(DateTime)`).
fn default_epoch() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_else(Utc::now)
}

/// Scopes a query that carries **no scope at all** to the libraries its
/// user can see.
///
/// Port of `LibraryManager.AddUserToQuery`. Without it an unscoped
/// `/Items?recursive=true` answers with every row in the table, including
/// the ones that are under no library at all — on a real adopted library
/// that is 23,809 rows of people, studios and genres, and it is how
/// `items:all` came back 38,004 where Jellyfin says 14,347.
///
/// The seven fields checked are exactly the seven the C# checks; any one of
/// them already narrows the query, and Jellyfin leaves it alone then.
///
/// An empty scope means the user can see nothing, and C# guards it with a
/// fresh guid so the query matches nothing rather than everything. This
/// uses the placeholder id for the same purpose — deterministic, and a row
/// every query already excludes.
pub(crate) async fn scope_to_user_libraries(
    db: &Database,
    filter: &InternalItemsQuery,
) -> Result<Option<InternalItemsQuery>, ServiceError> {
    let Some(user) = filter.user.as_ref() else {
        return Ok(None);
    };
    let unscoped = filter.ancestor_ids.is_empty()
        && filter.parent_id == Uuid::nil()
        && filter.channel_ids.is_empty()
        && filter.top_parent_ids.is_empty()
        && non_blank(filter.ancestor_with_presentation_unique_key.as_ref()).is_none()
        && non_blank(filter.series_presentation_unique_key.as_ref()).is_none()
        && filter.item_ids.is_empty();
    if !unscoped {
        return Ok(None);
    }

    let views = visible_views(db, user).await?;
    let by_view = physical_folders_by_view(db, &views).await?;
    // `GetTopParentIdsForQuery` per view: a collection folder becomes its
    // physical folders, and anything else — a Live TV view, or any view on
    // a Ferrofin-written database — stands for itself.
    let mut scope: Vec<Uuid> = views
        .iter()
        .flat_map(|v| match by_view.get(v) {
            Some(folders) => folders.clone(),
            None => vec![*v],
        })
        .collect();
    if scope.is_empty() {
        scope.push(Uuid::parse_str(PLACEHOLDER_ID).unwrap_or_else(|_| Uuid::nil()));
    }
    let mut resolved = filter.clone();
    resolved.top_parent_ids = scope;
    Ok(Some(resolved))
}

/// The library views `user` may see, as item ids.
///
/// The `Folder.IsVisible` half of `AddUserToQuery`: a non-empty
/// `BlockedMediaFolders` preference hides exactly those; otherwise a user
/// without `EnableAllFolders` sees only their `EnabledFolders`.
///
/// `AddUserToQuery` scopes to `GetUserViews(IncludeExternalContent: true)`,
/// so the Live TV view is in scope exactly when that call yields it — which
/// is only while Live TV is available (see
/// [`user_entity_ext::live_tv_enabled_for`]).
async fn visible_views(db: &Database, user: &UserEntity) -> Result<Vec<Uuid>, ServiceError> {
    let Some(collection_folder) = stored_type_name(BaseItemKind::CollectionFolder) else {
        return Ok(Vec::new());
    };
    let Some(user_view) = stored_type_name(BaseItemKind::UserView) else {
        return Ok(Vec::new());
    };
    // What `GET /Library/MediaFolders` lists, which is Jellyfin's
    // `GetUserRootFolder().Children` — the libraries, the views, and the
    // playlists folder created playlists hang off.
    //
    // BOTH playlists-folder kinds. An adopted database stores
    // `PlaylistsFolder` (10.11.8 has no `ManualPlaylistsFolder` class —
    // that name is only its client type), while Ferrofin has historically
    // provisioned the other, and both are legitimate rows at
    // `{data}/playlists`. Accepting one and not the other is how a playlist
    // ends up parented to a row that is not in scope, and so invisible.
    let playlists_folder = stored_type_name(BaseItemKind::PlaylistsFolder).unwrap_or_default();
    let manual_playlists_folder =
        stored_type_name(BaseItemKind::ManualPlaylistsFolder).unwrap_or_default();
    let rows: Vec<(String, String)> =
        sqlx::query_as(r#"SELECT "Id", "Type" FROM "BaseItems" WHERE "Type" IN (?1, ?2, ?3, ?4)"#)
            .bind(collection_folder)
            .bind(user_view)
            .bind(playlists_folder)
            .bind(manual_playlists_folder)
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;

    // Live TV is a view only while Live TV exists — see
    // `live_tv_enabled_for`. Jellyfin leaves it out of `GetUserViews`
    // otherwise, so it is out of this scope too. The row is looked for
    // first: most databases have none, and then neither of the gate's two
    // queries needs to run at all.
    let live_tv_view = match live_tv_view_id(db).await? {
        Some(id) if !live_tv_enabled_for(db, &user.id).await? => Some(id),
        _ => None,
    };
    let blocked = folder_preference(db, user, PreferenceKind::BlockedMediaFolders).await?;
    let enabled = if blocked.is_empty()
        && !has_permission(db.pool(), &user.id, PermissionKind::EnableAllFolders).await?
    {
        Some(folder_preference(db, user, PreferenceKind::EnabledFolders).await?)
    } else {
        None
    };

    Ok(rows
        .iter()
        .filter_map(|(id, type_)| {
            let id = Uuid::parse_str(id).ok()?;
            // Neither a `UserView` (Live TV, Playlists) nor the playlists
            // folder is a media folder, so neither carries a visibility
            // preference.
            if live_tv_view == Some(id) {
                return None;
            }
            if type_ == user_view || type_ == playlists_folder || type_ == manual_playlists_folder {
                return Some(id);
            }
            if blocked.contains(&id) {
                return None;
            }
            match &enabled {
                Some(allowed) if !allowed.contains(&id) => None,
                _ => Some(id),
            }
        })
        .collect())
}

/// The `UserView` row that is the Live TV view, if this database has one.
///
/// Identified by its **path** — see
/// [`crate::user_view_manager::LIVE_TV_VIEW_PATH_SUFFIX`]. The name is the
/// localized `HeaderLiveTV` string ("TV en direct", "ライブTV", …), so
/// matching on it would silently do nothing on most servers and would
/// misfire on a library a user happened to call "Live TV".
async fn live_tv_view_id(db: &Database) -> Result<Option<Uuid>, ServiceError> {
    let Some(user_view) = stored_type_name(BaseItemKind::UserView) else {
        return Ok(None);
    };
    // Both separators: C# builds the path with `Path.Combine`, so a
    // database adopted from a Windows Jellyfin stores `…\views\livetv`
    // and a POSIX-only pattern would never fire — on exactly the adoption
    // case this gate exists for. (`LIKE` is `%`/`_` only, so a backslash
    // needs no escaping.)
    let suffix = crate::user_view_manager::LIVE_TV_VIEW_PATH_SUFFIX;
    let id: Option<String> = sqlx::query_scalar(
        r#"SELECT "Id" FROM "BaseItems"
           WHERE "Type" = ?1 AND ("Path" LIKE ?2 OR "Path" LIKE ?3)
           ORDER BY "Id" LIMIT 1"#,
    )
    .bind(user_view)
    .bind(format!("%{suffix}"))
    .bind(format!("%{}", suffix.replace('/', "\\")))
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)?;
    Ok(id.and_then(|id| Uuid::parse_str(&id).ok()))
}

/// One of the user's guid-list folder preferences.
async fn folder_preference(
    user_db: &Database,
    user: &UserEntity,
    kind: PreferenceKind,
) -> Result<Vec<Uuid>, ServiceError> {
    guid_preference(user_db.pool(), &user.id, kind).await
}

#[async_trait]
impl ItemRepository for FerrofinItemRepository {
    async fn retrieve_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        if id.is_nil() {
            return Err(ServiceError::invalid_input("item id can't be empty"));
        }
        let row = sqlx::query_as::<_, BaseItemEntity>(
            r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1 AND "Id" <> ?2"#,
        )
        .bind(guid_to_db(id))
        .bind(PLACEHOLDER_ID)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(row)
    }

    async fn locked_item_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        let rows: Vec<String> =
            sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "IsLocked" = 1"#)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|id| Uuid::parse_str(id).ok())
            .collect())
    }

    async fn item_text_rows(
        &self,
        kind: BaseItemKind,
        ids: &[Uuid],
    ) -> Result<Vec<ItemTextRow>, ServiceError> {
        let Some(type_name) = stored_type_name(kind) else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::new();
        for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            // The anonymous `?` list must come FIRST: SQLite gives an
            // anonymous parameter the next index after the largest assigned so
            // far, so an explicit `?N` ahead of the list pushes every `?` in it
            // past the bound arguments and the query silently matches nothing.
            let sql = format!(
                r#"SELECT "Id", "Name", "SortName", "Overview", "Path"
                   FROM "BaseItems" WHERE "Id" IN ({}) AND "Type" = ?{}"#,
                placeholders(chunk.len()),
                chunk.len() + 1
            );
            let mut query = sqlx::query_as::<_, ItemTextRow>(&sql);
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            query = query.bind(type_name);
            rows.extend(query.fetch_all(self.db.pool()).await.map_err(db_err)?);
        }
        Ok(rows)
    }

    async fn get_ancestor_chain(
        &self,
        item_id: Uuid,
    ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError> {
        let db_id = guid_to_db(item_id);
        let rows: Vec<BaseItemEntity> = sqlx::query_as(
            r#"WITH RECURSIVE chain(id, depth) AS (
                 SELECT ?1, 0
                 UNION ALL
                 SELECT bi."ParentId", c.depth + 1
                 FROM chain c
                 JOIN "BaseItems" bi ON bi."Id" = c.id
                 WHERE bi."ParentId" IS NOT NULL
                   AND bi."ParentId" <> ?2
                   AND c.depth < ?3
               )
               SELECT bi.* FROM chain c
               JOIN "BaseItems" bi ON bi."Id" = c.id
               WHERE c.depth > 0
               ORDER BY c.depth ASC"#,
        )
        .bind(&db_id)
        .bind(PLACEHOLDER_ID)
        .bind(MAX_ANCESTOR_DEPTH)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        if rows.is_empty() {
            let exists = self.retrieve_item(item_id).await?.is_some();
            return if exists {
                Ok(Some(Vec::new()))
            } else {
                Ok(None)
            };
        }
        // Deduplicate by id (nearest-first) in case the tree has a cycle.
        let mut seen = std::collections::HashSet::new();
        let deduped = rows
            .into_iter()
            .filter(|r| seen.insert(r.id.clone()))
            .collect();
        Ok(Some(deduped))
    }

    async fn get_items(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        // C# `ItemsController.GetItems` does NOT query when the parent is the
        // user root and the request is neither recursive nor id-scoped — see
        // [`Self::user_root_children`].
        if let Some(children) = self.user_root_children(filter).await? {
            let total = i32::try_from(children.len()).unwrap_or(i32::MAX);
            // `new QueryResult<BaseItem>(itemsArray)`: the total is the array's
            // own length, and the controller echoes the REQUESTED `startIndex`
            // back unchanged (it paged nothing).
            return Ok(QueryResult::new(
                Some(filter.start_index.unwrap_or(0)),
                Some(total),
                children,
            ));
        }
        // Resolve once for both statements below: the page and its count share
        // one scope, and asking the database twice for the same library's
        // folders is a round trip on every paged browse.
        let resolved = self.resolve_views(filter).await?;
        let filter = resolved.as_ref().unwrap_or(filter);
        let items = self
            .fetch_rows_resolved(filter, QueryShape::FullRows)
            .await?;
        let start_index = filter.start_index.unwrap_or(0);
        let page_len = i32::try_from(items.len()).unwrap_or(i32::MAX);
        // A page that came back SHORT of its own `LIMIT` has nothing after it,
        // so the total is already in hand: `start_index + page_len`, exactly what
        // the `COUNT(*)` would return. `fetch_rows` hands back precisely what SQL
        // produced — no Rust-side dedup or filtering — so a short page can only
        // mean the result set is exhausted.
        //
        // The count is otherwise a second full pass over the page query's
        // predicate, and unlike the page it cannot stop at `LIMIT` rows: 2.367 ms
        // of an 8.73 ms `/Items?limit=50` on the bench library. Skipping it costs
        // nothing when it fires and changes no response — the total is exact
        // either way. It fires for every browse whose filtered result is smaller
        // than one page, which is most of them in a real client: a genre with a
        // dozen titles, a season's episodes, a folder listing.
        // …but only when the page actually LANDED in the result set. An empty
        // page at a non-zero offset is the one short page that proves nothing:
        // it means the caller paged past the end, and `start_index + 0` is a
        // number above the real total, not the total. That case still counts.
        // (`by_name_total_survives_offset_past_end` records the same trap on the
        // by-name path.)
        let count_is_known = filter.limit.is_some_and(|limit| page_len < limit)
            && (page_len > 0 || start_index == 0);
        let total = if filter.enable_total_record_count
            && (filter.limit.is_some() || start_index > 0)
            && !count_is_known
        {
            self.fetch_count_resolved(filter).await?
        } else {
            page_len + start_index
        };
        Ok(QueryResult::new(Some(start_index), Some(total), items))
    }

    async fn get_item_ids(&self, filter: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        self.fetch_ids(filter).await
    }

    async fn get_item_list(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.fetch_rows(filter, QueryShape::FullRows).await
    }

    async fn get_item_id_clean_names(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<(String, Option<String>)>, ServiceError> {
        self.fetch_id_clean_names(filter).await
    }

    async fn get_latest_item_list(
        &self,
        filter: &InternalItemsQuery,
        collection_type: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // C# `BaseItemRepository.GetLatestItemList`: only tvshows and music
        // take the grouped path; every other collection type early-exits
        // empty (the user-view manager sends those through `get_item_list`).
        let group_column = match collection_type {
            CollectionType::tvshows => r#"bi."SeriesName""#,
            CollectionType::music => r#"bi."Album""#,
            _ => return Ok(Vec::new()),
        };
        // This is the path jellyfin-web's per-library "Latest in …" rows take
        // (grouping is on by default for tvshows and music), so it needs the
        // same view translation as every other browse — without it, Latest for
        // a TV or Music library on an adopted database comes back empty.
        let resolved = self.resolve_views(filter).await?;
        let filter = resolved.as_ref().unwrap_or(filter);
        // One statement: the newest `limit` groups' maxima set a DateCreated
        // threshold, and every row at or above it comes back in the caller's
        // order (`filter.limit` caps GROUPS, not rows — the outer query is
        // unpaged). The caller's `order_by` rides through untouched.
        let mut qb = build_latest_item_list_query(filter, group_column);
        qb.build_query_as::<BaseItemEntity>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
    }

    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError> {
        let exists: Option<i64> =
            sqlx::query_scalar(r#"SELECT 1 FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(exists.is_some())
    }

    async fn get_items_by_primary_version(
        &self,
        primary_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if primary_id.is_nil() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, BaseItemEntity>(
            r#"SELECT * FROM "BaseItems" WHERE "PrimaryVersionId" = ?1 AND "Id" <> ?2"#,
        )
        .bind(guid_to_db(primary_id))
        .bind(PLACEHOLDER_ID)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows)
    }

    async fn get_items_by_primary_version_batch(
        &self,
        primary_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<BaseItemEntity>>, ServiceError> {
        let mut map: HashMap<Uuid, Vec<BaseItemEntity>> = HashMap::new();
        for chunk in primary_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = format!(
                r#"SELECT * FROM "BaseItems"
                   WHERE "PrimaryVersionId" IN ({}) AND "Id" <> ?{}"#,
                placeholders(chunk.len()),
                chunk.len() + 1
            );
            let mut query = sqlx::query_as::<_, BaseItemEntity>(&sql);
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            query = query.bind(PLACEHOLDER_ID);
            for row in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Some(primary) = row
                    .primary_version_id
                    .as_deref()
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
                    map.entry(primary).or_default().push(row);
                }
            }
        }
        Ok(map)
    }

    async fn get_items_with_provider_id(
        &self,
        provider_key: &str,
    ) -> Result<Vec<(Uuid, String)>, ServiceError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT "ItemId", "ProviderValue" FROM "BaseItemProviders"
               WHERE "ProviderId" = ?1 COLLATE NOCASE"#,
        )
        .bind(provider_key)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, value)| Uuid::parse_str(&id).ok().map(|id| (id, value)))
            .collect())
    }

    async fn get_image_infos(&self, item_id: Uuid) -> Result<Vec<ItemImageInfo>, ServiceError> {
        // Order by image type then id so a multi-image type (e.g. Backdrop) is
        // returned in a stable order the index-based routes can address, matching
        // the C# `BaseItem.ImageInfos` insertion order.
        let rows = sqlx::query_as::<_, BaseItemImageInfoEntity>(
            r#"SELECT * FROM "BaseItemImageInfos" WHERE "ItemId" = ?1
                ORDER BY "ImageType", "Id""#,
        )
        .bind(guid_to_db(item_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(image_info_from_row).collect())
    }

    async fn swap_item_images(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        index1: i32,
        index2: i32,
    ) -> Result<(), ServiceError> {
        // A same-index swap is a no-op (matching C#, where swapping a row with
        // itself changes nothing) and avoids a needless write.
        if index1 == index2 {
            return Ok(());
        }
        // Load this item's rows for the requested type in the same stable order
        // get_image_infos exposes, so the caller's 0-based indices address the
        // same images the read side does.
        let disc = image_type_to_disc(image_type);
        let rows = sqlx::query_as::<_, BaseItemImageInfoEntity>(
            r#"SELECT * FROM "BaseItemImageInfos" WHERE "ItemId" = ?1 AND "ImageType" = ?2
                ORDER BY "Id""#,
        )
        .bind(guid_to_db(item_id))
        .bind(disc)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        // Out-of-range indices are a no-op — the C# `GetImageInfo` returns null and
        // SwapImagesAsync bails with "nothing to do".
        let (Ok(i1), Ok(i2)) = (usize::try_from(index1), usize::try_from(index2)) else {
            return Ok(());
        };
        let (Some(first), Some(second)) = (rows.get(i1), rows.get(i2)) else {
            return Ok(());
        };

        // C# swaps the two on-disk files and clears the cached dimensions. The
        // portable equivalent over stored rows is to exchange the two rows' paths
        // (so the image previously at index1 now resolves at index2) and reset
        // Width/Height to the "unknown" sentinel, stamping DateModified.
        let now = datetime_to_db(Utc::now());
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(
            r#"UPDATE "BaseItemImageInfos"
                SET "Path" = ?2, "Width" = 0, "Height" = 0, "DateModified" = ?3
                WHERE "Id" = ?1"#,
        )
        .bind(&first.id)
        .bind(&second.path)
        .bind(&now)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            r#"UPDATE "BaseItemImageInfos"
                SET "Path" = ?2, "Width" = 0, "Height" = 0, "DateModified" = ?3
                WHERE "Id" = ?1"#,
        )
        .bind(&second.id)
        .bind(&first.path)
        .bind(&now)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn get_genres(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        // `GetGenres` and `GetMusicGenres` are the SAME query over the same
        // `ItemValueType`; only the by-name row kind returned differs (master
        // `BaseItemRepository.ByName.cs:44-52`, v10.11.8
        // `BaseItemRepository.cs:211-222`). Neither restricts which content
        // items may contribute — the owner-kind split Ferrofin used to apply
        // here was a compensation for a scanner mis-split that is itself fixed
        // (a music owner materializes a `MusicGenre` row and nothing else,
        // `item_persistence_service.rs`), and it made `/MusicGenres` drop every
        // genre carried only by non-music items.
        self.item_values_with_counts(GENRE_TYPES, BaseItemKind::Genre, filter)
            .await
    }

    async fn get_music_genres(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(GENRE_TYPES, BaseItemKind::MusicGenre, filter)
            .await
    }

    async fn get_studios(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(STUDIO_TYPES, BaseItemKind::Studio, filter)
            .await
    }

    async fn get_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(ARTIST_TYPES, BaseItemKind::MusicArtist, filter)
            .await
    }

    async fn get_album_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(ALBUM_ARTIST_TYPES, BaseItemKind::MusicArtist, filter)
            .await
    }

    async fn get_all_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.item_values_with_counts(ALL_ARTIST_TYPES, BaseItemKind::MusicArtist, filter)
            .await
    }

    async fn get_music_genre_names(&self) -> Result<Vec<String>, ServiceError> {
        let music_types = self.item_type_lookup.music_genre_types();
        let with: Vec<&str> = music_types.iter().map(String::as_str).collect();
        self.item_value_names(GENRE_TYPES, &with, &[]).await
    }

    async fn get_studio_names(&self) -> Result<Vec<String>, ServiceError> {
        self.item_value_names(STUDIO_TYPES, &[], &[]).await
    }

    async fn get_genre_names(&self) -> Result<Vec<String>, ServiceError> {
        let music_types = self.item_type_lookup.music_genre_types();
        let exclude: Vec<&str> = music_types.iter().map(String::as_str).collect();
        self.item_value_names(GENRE_TYPES, &[], &exclude).await
    }

    async fn get_all_artist_names(&self) -> Result<Vec<String>, ServiceError> {
        self.item_value_names(ALL_ARTIST_TYPES, &[], &[]).await
    }

    async fn get_media_stream_languages(
        &self,
        filter: &InternalItemsQuery,
        stream_type: MediaStreamType,
    ) -> Result<Vec<String>, ServiceError> {
        // Restrict the item set with the filter, then collect distinct stream
        // languages of the requested type ("und" for missing), matching C#.
        let ids = self.fetch_ids(filter).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let stream_disc = i64::from(media_stream_type_disc(stream_type));
        let mut sql = String::from(
            r#"SELECT DISTINCT CASE WHEN ms."Language" IS NULL OR ms."Language" = ''
                 THEN 'und' ELSE ms."Language" END
               FROM "MediaStreamInfos" ms WHERE ms."StreamType" = ? AND ms."ItemId" IN ("#,
        );
        sql.push_str(&placeholders(ids.len()));
        sql.push(')');
        let mut query = sqlx::query_scalar::<_, String>(&sql).bind(stream_disc);
        for id in &ids {
            query = query.bind(guid_to_db(*id));
        }
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }

    async fn get_media_stream_languages_by_type(
        &self,
        filter: &InternalItemsQuery,
        stream_types: &[MediaStreamType],
    ) -> Result<std::collections::HashMap<MediaStreamType, Vec<String>>, ServiceError> {
        let mut out: std::collections::HashMap<MediaStreamType, Vec<String>> =
            stream_types.iter().map(|&t| (t, Vec::new())).collect();
        // Resolve the item set once (the fetch_ids + IN was previously run per
        // type — audio and subtitle each re-materialized the same ids).
        let ids = self.fetch_ids(filter).await?;
        if ids.is_empty() || stream_types.is_empty() {
            return Ok(out);
        }
        // Map disc -> type so the grouped rows sort back into per-type lists.
        let by_disc: std::collections::HashMap<i64, MediaStreamType> = stream_types
            .iter()
            .map(|&t| (i64::from(media_stream_type_disc(t)), t))
            .collect();
        let mut sql = String::from(
            r#"SELECT DISTINCT ms."StreamType",
                 CASE WHEN ms."Language" IS NULL OR ms."Language" = '' THEN 'und'
                      ELSE ms."Language" END
               FROM "MediaStreamInfos" ms WHERE ms."StreamType" IN ("#,
        );
        sql.push_str(&placeholders(by_disc.len()));
        sql.push_str(r#") AND ms."ItemId" IN ("#);
        sql.push_str(&placeholders(ids.len()));
        sql.push(')');
        let mut query = sqlx::query_as::<_, (i64, String)>(&sql);
        for disc in by_disc.keys() {
            query = query.bind(*disc);
        }
        for id in &ids {
            query = query.bind(guid_to_db(*id));
        }
        for (disc, lang) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
            if let Some(t) = by_disc.get(&disc) {
                out.entry(*t).or_default().push(lang);
            }
        }
        Ok(out)
    }

    async fn get_query_filters_legacy(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        // `AddUserToQuery` first, and once: every facet below runs the same
        // filter, so scoping here scopes all four. A filter dialog that offers a
        // genre, tag, rating or year from a library the account cannot see is
        // offering a choice that returns nothing.
        let scoped = scope_to_user_libraries(&self.db, filter).await?;
        let filter = scoped.as_ref().unwrap_or(filter);
        // Each facet runs the filter once as its own WHERE (via `append_predicates`)
        // instead of materializing the whole matching id set and binding it back as a
        // giant `IN` per facet. The old "resolve every matching id in the app, then
        // re-send them as a thousand-parameter IN, four times" round-trip dominated the
        // Filters/Filters2/Years CPU under load.
        let years = self.distinct_years(filter).await?;
        let official_ratings = self.distinct_official_ratings(filter).await?;
        let genres = self
            .distinct_item_values(filter, ItemValueType::Genre)
            .await?;
        let tags = self
            .distinct_item_values(filter, ItemValueType::Tags)
            .await?;

        Ok(QueryFiltersLegacy {
            genres,
            tags,
            official_ratings,
            years,
        })
    }

    async fn get_distinct_years(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<i32>, ServiceError> {
        // `/Years` wants the years and nothing else. Going through
        // `get_query_filters_legacy` for them also ran the official-ratings
        // scan and both `ItemValues` MIN aggregates and dropped all three — but
        // it does share that method's user scoping, which has to be applied
        // here too.
        let scoped = scope_to_user_libraries(&self.db, filter).await?;
        self.distinct_years(scoped.as_ref().unwrap_or(filter)).await
    }

    async fn get_is_played(
        &self,
        user: &UserEntity,
        id: Uuid,
        recursive: bool,
    ) -> Result<bool, ServiceError> {
        // Non-recursive: all direct, non-virtual leaf children played by the user.
        // Recursive descent (via the AncestorIds/LinkedChildren closure) is the
        // library manager's job; here the direct-children form is honored and the
        // recursive flag widens to the ancestor closure where present.
        let uid = user.id.clone();
        // Both forms select leaf descendants of `id`; they differ only in how a
        // child is related to the parent. Each contributes a FROM fragment (join)
        // and a scope predicate that folds into the single WHERE below — never a
        // second WHERE.
        let (join, scope) = if recursive {
            // Any descendant, via the AncestorIds closure.
            (
                r#"JOIN "AncestorIds" a ON a."ItemId" = bi."Id" AND a."ParentItemId" = ?1"#,
                "1 = 1",
            )
        } else {
            // Direct children only.
            ("", r#"bi."ParentId" = ?1"#)
        };
        let sql = format!(
            r#"SELECT NOT EXISTS (
                 SELECT 1 FROM "BaseItems" bi {join}
                 WHERE {scope} AND bi."IsFolder" = 0 AND bi."IsVirtualItem" = 0
                   AND NOT EXISTS (SELECT 1 FROM "UserData" ud
                       WHERE ud."ItemId" = bi."Id" AND ud."UserId" = ?2 AND ud."Played" = 1))"#,
        );
        let all_played: i64 = sqlx::query_scalar(&sql)
            .bind(guid_to_db(id))
            .bind(uid)
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(all_played != 0)
    }

    async fn get_playlist_items_with_access(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
        child_type: i32,
    ) -> Result<PlaylistItemsWithAccess, ServiceError> {
        let rows: Vec<PlaylistItemRow> = sqlx::query_as(PLAYLIST_ITEMS_SQL)
            .bind(guid_to_db(playlist_id))
            .bind(guid_to_db(user_id))
            .bind(i64::from(child_type))
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        // Every row repeats the same access columns; the first carries them.
        // No rows at all means the join matched nothing — the caller decides
        // whether that is an empty playlist or a missing/invisible one.
        let access = rows.first().map(|first| PlaylistAccessColumns {
            owner_user_id: first.owner.clone(),
            open_access: first.open_access,
            share_can_edit: first.can_edit,
        });
        Ok(PlaylistItemsWithAccess {
            items: rows.into_iter().map(|r| r.item).collect(),
            access,
        })
    }
}

impl FerrofinItemRepository {
    /// Distinct positive production years of the filter's matching items, ascending —
    /// the filter runs as this query's own WHERE, no app-side id materialization.
    async fn distinct_years(&self, filter: &InternalItemsQuery) -> Result<Vec<i32>, ServiceError> {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            r#"SELECT DISTINCT bi."ProductionYear" FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
        );
        qb.push_bind(PLACEHOLDER_ID);
        append_predicates(&mut qb, filter);
        qb.push(
            r#" AND bi."ProductionYear" IS NOT NULL AND bi."ProductionYear" > 0
                ORDER BY bi."ProductionYear""#,
        );
        let rows: Vec<i64> = qb
            .build_query_scalar()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|y| i32::try_from(y).unwrap_or(i32::MAX))
            .collect())
    }

    /// Distinct non-empty official ratings of the filter's matching items, ascending.
    async fn distinct_official_ratings(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            r#"SELECT DISTINCT bi."OfficialRating" FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
        );
        qb.push_bind(PLACEHOLDER_ID);
        append_predicates(&mut qb, filter);
        qb.push(
            r#" AND bi."OfficialRating" IS NOT NULL AND bi."OfficialRating" <> ''
                ORDER BY bi."OfficialRating""#,
        );
        qb.build_query_scalar()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
    }

    /// Distinct display values of one `ItemValues` type over the filter's matching items.
    async fn distinct_item_values(
        &self,
        filter: &InternalItemsQuery,
        value_type: ItemValueType,
    ) -> Result<Vec<String>, ServiceError> {
        // One entry per CLEANED value (upstream GetQueryFiltersLegacy groups by
        // CleanValue and keeps MIN(Value)), so "Sci-Fi"/"Sci-fi" case variants
        // collapse instead of doubling the filter dialog's list.
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            r#"SELECT MIN(iv."Value") FROM "ItemValues" AS iv
               JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
               JOIN "BaseItems" AS bi ON bi."Id" = ivm."ItemId"
               WHERE iv."Type" = "#,
        );
        qb.push_bind(i64::from(i32::from(value_type)));
        qb.push(r#" AND bi."Id" <> "#);
        qb.push_bind(PLACEHOLDER_ID);
        append_predicates(&mut qb, filter);
        qb.push(r#" GROUP BY iv."CleanValue" ORDER BY MIN(iv."Value")"#);
        qb.build_query_scalar()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
    }
}

/// Builds a `?, ?, …` placeholder list of length `n` (at least one).
fn placeholders(n: usize) -> String {
    if n == 0 {
        return "NULL".to_owned();
    }
    let mut s = String::with_capacity(n * 3);
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('?');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::test_support::{
        seed_child_item, seed_item, seed_item_genre, seed_item_with_data, seed_library_over,
        seed_named_item, seed_top_parented_item, seed_user, seed_user_data,
        seed_user_with_defaults, set_clean_name, test_db,
    };
    use ferrofin_db::Database;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::ExtraType;
    use ferrofin_traits::persistence::ItemPersistenceService;

    fn repo(db: &Database) -> FerrofinItemRepository {
        FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
    }

    /// A browse of the `UserRootFolder` lists the `AggregateFolder`'s virtual
    /// children alongside its own — `UserRootFolder.
    /// GetEligibleChildrenForRecursiveChildren` (UserRootFolder.cs:96-102)
    /// concatenating `LibraryManager.RootFolder.VirtualChildren`.
    #[tokio::test]
    async fn a_user_root_browse_concatenates_the_aggregates_virtual_children() {
        use crate::aggregate_folder::RootFolderIds;
        use crate::test_support::seed_folder_item;
        let db = test_db().await;
        let user_root = Uuid::from_u128(0xC001);
        let aggregate = Uuid::from_u128(0xC002);
        seed_folder_item(&db, aggregate, BaseItemKind::AggregateFolder, "root", None).await;
        seed_folder_item(&db, user_root, BaseItemKind::UserRootFolder, "MF", None).await;
        seed_folder_item(
            &db,
            Uuid::from_u128(0xC010),
            BaseItemKind::CollectionFolder,
            "Movies",
            Some(user_root),
        )
        .await;
        seed_folder_item(
            &db,
            Uuid::from_u128(0xC020),
            BaseItemKind::PlaylistsFolder,
            "Playlists",
            Some(aggregate),
        )
        .await;
        // A plain child of the aggregate — a physical library folder on an
        // adopted database. `AddVirtualChild` never saw it, so the user root
        // must not list it.
        seed_folder_item(
            &db,
            Uuid::from_u128(0xC021),
            BaseItemKind::Folder,
            "movies",
            Some(aggregate),
        )
        .await;
        let query = InternalItemsQuery {
            parent_id: user_root,
            ..InternalItemsQuery::default()
        };

        let plain = repo(&db).get_item_list(&query).await.expect("plain");
        assert_eq!(
            plain.len(),
            1,
            "without the root ids the browse is a plain ParentId equality"
        );

        let wired = repo(&db)
            .with_root_ids(RootFolderIds {
                user_root,
                aggregate,
            })
            .get_item_list(&query)
            .await
            .expect("wired");
        let names: Vec<&str> = wired.iter().filter_map(|r| r.name.as_deref()).collect();
        assert_eq!(names.len(), 2, "got {names:?}");
        assert!(names.contains(&"Movies") && names.contains(&"Playlists"));
        assert!(
            !names.contains(&"movies"),
            "the aggregate's physical folders are not virtual children"
        );
    }

    /// `/Items?parentId={aggregate}` answers with the user root's children —
    /// `Folder.GetChildren` (Folder.cs:1348-1360): "the true root should return
    /// our users root folder children".
    #[tokio::test]
    async fn a_browse_of_the_physical_root_answers_with_the_user_roots_children() {
        use crate::aggregate_folder::RootFolderIds;
        use crate::test_support::seed_folder_item;
        let db = test_db().await;
        let user_root = Uuid::from_u128(0xD001);
        let aggregate = Uuid::from_u128(0xD002);
        seed_folder_item(&db, aggregate, BaseItemKind::AggregateFolder, "root", None).await;
        seed_folder_item(&db, user_root, BaseItemKind::UserRootFolder, "MF", None).await;
        seed_folder_item(
            &db,
            Uuid::from_u128(0xD010),
            BaseItemKind::CollectionFolder,
            "Movies",
            Some(user_root),
        )
        .await;
        seed_folder_item(
            &db,
            Uuid::from_u128(0xD020),
            BaseItemKind::PlaylistsFolder,
            "Playlists",
            Some(aggregate),
        )
        .await;

        let rows = repo(&db)
            .with_root_ids(RootFolderIds {
                user_root,
                aggregate,
            })
            .get_item_list(&InternalItemsQuery {
                parent_id: aggregate,
                ..InternalItemsQuery::default()
            })
            .await
            .expect("browse");

        let mut names: Vec<&str> = rows.iter().filter_map(|r| r.name.as_deref()).collect();
        names.sort_unstable();
        assert_eq!(names, ["Movies", "Playlists"]);
    }

    /// The virtual children come LAST, not in `SortName` order among the rest:
    /// `UserRootFolder.GetEligibleChildrenForRecursiveChildren` APPENDS them
    /// (`list.AddRange`, UserRootFolder.cs:96-102). Measured on 10.11.8,
    /// `/Items?parentId={userRoot}` answers `[…, Shows (synth), Playlists]`
    /// although "playlists" sorts before "shows (synth)".
    #[tokio::test]
    async fn the_aggregates_virtual_children_are_appended_not_sorted_inline() {
        use crate::aggregate_folder::RootFolderIds;
        use crate::test_support::seed_folder_item;
        let db = test_db().await;
        let user_root = Uuid::from_u128(0xE001);
        let aggregate = Uuid::from_u128(0xE002);
        seed_folder_item(&db, aggregate, BaseItemKind::AggregateFolder, "root", None).await;
        seed_folder_item(&db, user_root, BaseItemKind::UserRootFolder, "MF", None).await;
        for (n, name) in [(0xE010_u128, "Collections"), (0xE011, "Shows")] {
            seed_folder_item(
                &db,
                Uuid::from_u128(n),
                BaseItemKind::CollectionFolder,
                name,
                Some(user_root),
            )
            .await;
        }
        // "Playlists" sorts between "Collections" and "Shows".
        seed_folder_item(
            &db,
            Uuid::from_u128(0xE020),
            BaseItemKind::PlaylistsFolder,
            "Playlists",
            Some(aggregate),
        )
        .await;

        let rows = repo(&db)
            .with_root_ids(RootFolderIds {
                user_root,
                aggregate,
            })
            .get_item_list(&InternalItemsQuery {
                parent_id: user_root,
                ..InternalItemsQuery::default()
            })
            .await
            .expect("browse");

        let names: Vec<&str> = rows.iter().filter_map(|r| r.name.as_deref()).collect();
        assert_eq!(
            names,
            ["Collections", "Shows", "Playlists"],
            "got {names:?}"
        );
    }

    /// `ItemsController.GetItems`' non-query branch (ItemsController.cs:307,
    /// 525-529): a non-recursive, id-less request whose parent resolves to the
    /// user root — INCLUDING an absent `parentId`, which
    /// `LibraryManager.GetParentItem(null, userId)` maps to the user root — is
    /// answered by `folder.GetChildren(user, true)`, so the request's sort,
    /// paging and item types are all discarded.
    #[tokio::test]
    async fn the_user_root_children_branch_ignores_sort_paging_and_filters() {
        use crate::aggregate_folder::RootFolderIds;
        use crate::test_support::seed_folder_item;
        let db = test_db().await;
        let user_root = Uuid::from_u128(0xE101);
        let aggregate = Uuid::from_u128(0xE102);
        seed_folder_item(&db, aggregate, BaseItemKind::AggregateFolder, "root", None).await;
        seed_folder_item(&db, user_root, BaseItemKind::UserRootFolder, "MF", None).await;
        for (n, name) in [(0xE110_u128, "Collections"), (0xE111, "Shows")] {
            seed_folder_item(
                &db,
                Uuid::from_u128(n),
                BaseItemKind::CollectionFolder,
                name,
                Some(user_root),
            )
            .await;
        }
        seed_folder_item(
            &db,
            Uuid::from_u128(0xE120),
            BaseItemKind::PlaylistsFolder,
            "Playlists",
            Some(aggregate),
        )
        .await;
        // A movie deep in the tree: it must never appear on this branch, and it
        // is what the unparented query used to answer with.
        seed_named_item(&db, Uuid::from_u128(0xE130), BaseItemKind::Movie, "Movie").await;

        let repository = repo(&db).with_root_ids(RootFolderIds {
            user_root,
            aggregate,
        });
        let noisy = InternalItemsQuery {
            user_root_children: true,
            limit: Some(1),
            start_index: Some(2),
            include_item_types: vec![BaseItemKind::Movie],
            order_by: vec![(
                ferrofin_model::live_tv::ItemSortBy::SortName,
                ferrofin_model::dto::SortOrder::Descending,
            )],
            ..InternalItemsQuery::default()
        };

        for parent in [Uuid::nil(), user_root] {
            let result = repository
                .get_items(&InternalItemsQuery {
                    parent_id: parent,
                    ..noisy.clone()
                })
                .await
                .expect("browse");
            let names: Vec<&str> = result
                .items
                .iter()
                .filter_map(|r| r.name.as_deref())
                .collect();
            assert_eq!(
                names,
                ["Collections", "Shows", "Playlists"],
                "parent {parent}: got {names:?}"
            );
            assert_eq!(result.total_record_count, 3);
            // The controller echoes the REQUESTED start index; it paged nothing.
            assert_eq!(result.start_index, 2);
        }

        // …and the branch is inert for a parent that is not the user root, and
        // for a recursive/id-scoped request (the handler does not set the flag
        // there, but the repository must not depend on that).
        let elsewhere = repository
            .get_items(&InternalItemsQuery {
                parent_id: Uuid::from_u128(0xE110),
                user_root_children: true,
                ..InternalItemsQuery::default()
            })
            .await
            .expect("elsewhere");
        assert!(
            elsewhere.items.is_empty(),
            "got {:?}",
            elsewhere.items.len()
        );
    }

    /// `AddChildrenFromCollection` keeps only `e.IsVisible(user)`
    /// (Folder.cs:1414-1417), and `Folder.IsVisible` (Folder.cs:231-253) applies
    /// the blocked-/enabled-folders check to `ICollectionFolder && not
    /// BasePluginFolder` — so a blocked `CollectionFolder` disappears while the
    /// playlists folder, a `BasePluginFolder`, always stays.
    #[tokio::test]
    async fn the_user_root_children_branch_hides_a_blocked_library() {
        use crate::aggregate_folder::RootFolderIds;
        use crate::test_support::seed_folder_item;
        let db = test_db().await;
        let user_root = Uuid::from_u128(0xE301);
        let aggregate = Uuid::from_u128(0xE302);
        let blocked = Uuid::from_u128(0xE310);
        seed_folder_item(&db, aggregate, BaseItemKind::AggregateFolder, "root", None).await;
        seed_folder_item(&db, user_root, BaseItemKind::UserRootFolder, "MF", None).await;
        seed_folder_item(
            &db,
            blocked,
            BaseItemKind::CollectionFolder,
            "Blocked",
            Some(user_root),
        )
        .await;
        seed_folder_item(
            &db,
            Uuid::from_u128(0xE311),
            BaseItemKind::CollectionFolder,
            "Allowed",
            Some(user_root),
        )
        .await;
        seed_folder_item(
            &db,
            Uuid::from_u128(0xE320),
            BaseItemKind::PlaylistsFolder,
            "Playlists",
            Some(aggregate),
        )
        .await;
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0xE3FF)).await;
        crate::user_entity_ext::set_preference(
            db.pool(),
            &user.id,
            PreferenceKind::BlockedMediaFolders,
            &[blocked.to_string()],
        )
        .await
        .expect("block");

        let rows = repo(&db)
            .with_root_ids(RootFolderIds {
                user_root,
                aggregate,
            })
            .get_items(&InternalItemsQuery {
                user_root_children: true,
                user: Some(user),
                ..InternalItemsQuery::default()
            })
            .await
            .expect("browse");

        let names: Vec<&str> = rows
            .items
            .iter()
            .filter_map(|r| r.name.as_deref())
            .collect();
        assert_eq!(names, ["Allowed", "Playlists"], "got {names:?}");
        assert_eq!(rows.total_record_count, 2);
    }

    /// A recursive browse of the physical root is a browse with no PARENT, not
    /// one with no SCOPE. C# leaves `AncestorIds = [aggregate]` behind
    /// (`SetTopParentIdsOrAncestors`, LibraryManager.cs:1673-1698), so rows
    /// outside the closure stay out; the equivalent here is the same scope an
    /// unparented query gets.
    #[tokio::test]
    async fn a_recursive_aggregate_browse_keeps_the_unparented_scope() {
        use crate::aggregate_folder::RootFolderIds;
        use crate::test_support::seed_folder_item;
        let db = test_db().await;
        let user_root = Uuid::from_u128(0xE201);
        let aggregate = Uuid::from_u128(0xE202);
        let library = Uuid::from_u128(0xE210);
        seed_folder_item(&db, aggregate, BaseItemKind::AggregateFolder, "root", None).await;
        seed_folder_item(&db, user_root, BaseItemKind::UserRootFolder, "MF", None).await;
        seed_folder_item(
            &db,
            library,
            BaseItemKind::CollectionFolder,
            "Movies",
            Some(user_root),
        )
        .await;
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0xE2FF)).await;
        seed_top_parented_item(
            &db,
            Uuid::from_u128(0xE220),
            BaseItemKind::Movie,
            "In library",
            library,
        )
        .await;
        // A by-name row: inside no library, so outside the aggregate's closure.
        seed_named_item(&db, Uuid::from_u128(0xE221), BaseItemKind::Person, "Actor").await;

        let rows = repo(&db)
            .with_root_ids(RootFolderIds {
                user_root,
                aggregate,
            })
            .get_item_list(&InternalItemsQuery {
                parent_id: aggregate,
                recursive: true,
                user: Some(user),
                ..InternalItemsQuery::default()
            })
            .await
            .expect("browse");

        let names: Vec<&str> = rows.iter().filter_map(|r| r.name.as_deref()).collect();
        assert!(names.contains(&"In library"), "got {names:?}");
        assert!(
            !names.contains(&"Actor"),
            "a row outside the aggregate's closure leaked: {names:?}"
        );
    }

    /// The regression the bare `GROUP BY "PresentationUniqueKey"` created.
    ///
    /// SQLite groups NULLs TOGETHER, so every keyless row in the recursive user
    /// universe shares ONE group and all but one of them vanish. Three older
    /// Ferrofin write paths left item-by-name rows keyless — and an
    /// `ids=`-scoped user query is one of the shapes that reaches the grouped
    /// subquery with no `TopParentId` filter in front of it
    /// (`scope_to_user_libraries` returns `None` once `item_ids` is set), so
    /// those rows were not merely mis-grouped, they were LOST.
    ///
    /// Measured on the lane pair before the repair landed:
    /// `GET /Items?userId=…&ids=<three Person ids>` answered F 1 / J 3.
    ///
    /// The rows are inserted RAW here, without a key, precisely because the
    /// shared fixture now writes one — the fixture is what a fixed insert
    /// produces, and this test has to reproduce what an upgraded database
    /// holds. Delete `backfill_by_name_presentation_keys` and this fails with
    /// one row.
    #[tokio::test]
    async fn presentation_key_backfill_rescues_a_grouped_query() {
        let db = test_db().await;
        let repository = repo(&db);
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0x9101)).await;
        let people = [
            (Uuid::from_u128(0x9111), "Alice Parity"),
            (Uuid::from_u128(0x9112), "Bob Parity"),
            (Uuid::from_u128(0x9113), "Carol Ferrofin"),
        ];
        for (id, name) in people {
            sqlx::query(
                r#"INSERT INTO "BaseItems"
                   ("Id","Type","Name","IsFolder","IsInMixedFolder","IsLocked",
                    "IsMovie","IsRepeat","IsSeries","IsVirtualItem")
                   VALUES (?1,'MediaBrowser.Controller.Entities.Person',?2,0,0,0,0,0,0,0)"#,
            )
            .bind(guid_to_db(id))
            .bind(name)
            .execute(db.writer())
            .await
            .expect("keyless person");
        }
        let query = InternalItemsQuery {
            user: Some(user.clone()),
            item_ids: people.iter().map(|(id, _)| *id).collect(),
            ..InternalItemsQuery::default()
        };
        assert!(
            crate::translate_query::group_by_presentation_unique_key(&query),
            "the shape under test must actually group"
        );
        // The keyless state: all three share the NULL group.
        assert_eq!(
            1,
            repository
                .get_item_list(&query)
                .await
                .expect("keyless")
                .len(),
            "keyless by-name rows collapse into one group"
        );

        let repaired =
            ferrofin_db::presentation_key::backfill_by_name_presentation_keys(db.writer())
                .await
                .expect("backfill");
        assert_eq!(3, repaired);

        let rows = repository.get_item_list(&query).await.expect("repaired");
        let mut names: Vec<_> = rows.iter().filter_map(|r| r.name.clone()).collect();
        names.sort();
        assert_eq!(
            vec!["Alice Parity", "Bob Parity", "Carol Ferrofin"],
            names,
            "every person survives the grouped query once it carries a key"
        );
    }

    #[test]
    fn placeholders_shapes() {
        assert_eq!(placeholders(0), "NULL");
        assert_eq!(placeholders(1), "?");
        assert_eq!(placeholders(3), "?, ?, ?");
    }

    #[tokio::test]
    async fn item_exists_and_get_items_total_without_limit() {
        let db = test_db().await;
        let repository = repo(&db);
        let id = Uuid::from_u128(0x7001);
        seed_named_item(&db, id, BaseItemKind::Movie, "Solo").await;

        assert!(repository.item_exists(id).await.expect("exists"));
        assert!(
            !repository
                .item_exists(Uuid::from_u128(0xBEEF))
                .await
                .expect("absent")
        );

        // No limit / no start_index → total is derived from the row count.
        let res = repository
            .get_items(&InternalItemsQuery::default())
            .await
            .expect("get_items");
        assert_eq!(res.total_record_count, 1);
        assert_eq!(res.items.len(), 1);
    }

    /// A page shorter than its own `LIMIT` skips the `COUNT(*)` and derives the
    /// total from the page — which must still be the TOTAL, not the page length.
    /// Every case below is one the short-circuit can get wrong: a short first
    /// page, a short page at an offset (where the answer is `start_index + len`
    /// and not `len`), an exactly-full page (which must still count, because
    /// there may be more), and an offset past the end.
    #[tokio::test]
    async fn short_pages_derive_the_total_without_counting() {
        let db = test_db().await;
        let repository = repo(&db);
        for i in 0..12u128 {
            seed_named_item(
                &db,
                Uuid::from_u128(0x7100 + i),
                BaseItemKind::Movie,
                &format!("Film {i:02}"),
            )
            .await;
        }
        let page = |limit: i32, start: i32| InternalItemsQuery {
            limit: Some(limit),
            start_index: if start > 0 { Some(start) } else { None },
            enable_total_record_count: true,
            ..InternalItemsQuery::default()
        };
        for (limit, start, want_len, want_total) in [
            (20, 0, 12, 12), // short first page: nothing follows it
            (5, 10, 2, 12),  // short page at an offset: total is start + len
            (5, 0, 5, 12),   // exactly full: there may be more, so it must count
            (12, 0, 12, 12), // full to the row: likewise
            (5, 20, 0, 12),  // past the end
        ] {
            let res = repository
                .get_items(&page(limit, start))
                .await
                .expect("get_items");
            assert_eq!(
                res.items.len(),
                want_len,
                "page length for limit={limit} start={start}"
            );
            assert_eq!(
                res.total_record_count, want_total,
                "total for limit={limit} start={start}"
            );
        }
    }

    /// The locked-item read must SEEK the partial index, never scan `BaseItems`.
    ///
    /// `run_scan` reads this set once, and it is shared with `scan_paths` —
    /// the library-monitor path that runs for one or two items on every
    /// filesystem event. Without `FerrofinIX_BaseItems_IsLocked` (migration
    /// 0017) the plan is `SCAN BaseItems`: 26-53 ms warm on a 100k-item table,
    /// paid per watcher event, where the old per-item lookup cost ~1 ms.
    #[tokio::test]
    async fn locked_item_ids_seeks_the_partial_index() {
        let db = test_db().await;
        let plan: Vec<String> = sqlx::query_as::<_, (i64, i64, i64, String)>(
            r#"EXPLAIN QUERY PLAN SELECT "Id" FROM "BaseItems" WHERE "IsLocked" = 1"#,
        )
        .fetch_all(db.pool())
        .await
        .expect("explain query plan")
        .into_iter()
        .map(|(_, _, _, detail)| detail)
        .collect();
        assert!(
            plan.iter()
                .any(|d| d.contains("FerrofinIX_BaseItems_IsLocked")),
            "the locked-item read must use the partial index, got: {plan:?}"
        );
        assert!(
            !plan.iter().any(|d| d.trim() == "SCAN BaseItems"),
            "the locked-item read must not scan BaseItems, got: {plan:?}"
        );
    }

    /// Two rows sharing a `CleanName` must resolve to the SAME id under both
    /// projections — the tie-break the by-name resolvers depend on.
    ///
    /// This is the one place the narrower projection could legitimately change
    /// the answer. `get_named_item_ids` passes a default query, so the ORDER BY
    /// is the bare `SortName`, and Person rows in a real database have
    /// `SortName IS NULL` — a total tie, where row order among duplicates is
    /// whatever the sorter happens to emit. The resolvers keep the first match
    /// (`or_insert`), so the two forms must agree on which row comes first.
    ///
    /// It is currently latent rather than live: `BaseItems.Id` is a TEXT PRIMARY
    /// KEY on a rowid table, so no index covers either projection and
    /// EXPLAIN QUERY PLAN is byte-identical for both. This test is what would
    /// catch that changing.
    #[tokio::test]
    async fn duplicate_clean_names_resolve_identically_under_both_projections() {
        let db = test_db().await;
        let repository = repo(&db);
        // Same CleanName, no SortName — the total-tie case.
        for n in [0x9201u128, 0x9202, 0x9203] {
            seed_named_item(&db, Uuid::from_u128(n), BaseItemKind::Person, "Jane Doe").await;
            set_clean_name(&db, Uuid::from_u128(n), "Jane Doe").await;
            sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = NULL WHERE "Id" = ?1"#)
                .bind(guid_to_db(Uuid::from_u128(n)))
                .execute(db.writer())
                .await
                .expect("null sort name");
        }

        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Person],
            ..InternalItemsQuery::default()
        };

        let first_full = |rows: Vec<ferrofin_db::entities::base_items::BaseItemEntity>| {
            rows.first().map(|r| r.id.clone())
        };
        // Repeated, because a tie-break that is merely *usually* stable is not a
        // tie-break — the resolvers cache the first id they see.
        for round in 0..5 {
            let full = first_full(repository.get_item_list(&query).await.expect("rows"));
            let pairs = repository
                .get_item_id_clean_names(&query)
                .await
                .expect("pairs");
            assert_eq!(
                pairs.len(),
                3,
                "round {round}: both forms must return every duplicate"
            );
            assert_eq!(
                full,
                pairs.first().map(|(id, _)| id.clone()),
                "round {round}: the two projections disagree on which duplicate is first, \
                 so the by-name resolvers would resolve different ids"
            );
        }
    }

    // The two-column projection is only safe because it runs the SAME statement
    // as `get_item_list` — same predicates, same ordering, same paging — so its
    // rows must line up with the full-row form one for one. If the two ever
    // drift, the by-name resolvers silently resolve the wrong id.
    #[tokio::test]
    async fn id_clean_name_projection_matches_the_full_row_query() {
        let db = test_db().await;
        let repository = repo(&db);
        for (n, name) in [(0x9101, "Zulu"), (0x9102, "Alpha"), (0x9103, "Mike")] {
            seed_named_item(&db, Uuid::from_u128(n), BaseItemKind::Movie, name).await;
            // The clean name is the join key the by-name resolvers match on, so
            // the projection has to carry the CLEANED value, not the display name.
            set_clean_name(&db, Uuid::from_u128(n), name).await;
            // A real SortName, so the shared ORDER BY actually orders and the
            // two forms are compared on a non-trivial row order.
            sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(Uuid::from_u128(n)))
                .bind(name.to_lowercase())
                .execute(db.writer())
                .await
                .expect("sort name");
        }
        // A different kind must be excluded by both forms alike.
        seed_named_item(&db, Uuid::from_u128(0x9104), BaseItemKind::Series, "Alpha").await;

        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            ..InternalItemsQuery::default()
        };
        let rows = repository.get_item_list(&query).await.expect("rows");
        let pairs = repository
            .get_item_id_clean_names(&query)
            .await
            .expect("pairs");
        assert_eq!(pairs.len(), 3, "the Series row must not leak in");
        assert_eq!(
            pairs.iter().map(|(_, c)| c.clone()).collect::<Vec<_>>(),
            vec![
                Some("alpha".to_owned()),
                Some("mike".to_owned()),
                Some("zulu".to_owned())
            ],
            "the projection carries the CLEAN name, in the query's sort order"
        );
        let expected: Vec<(String, Option<String>)> =
            rows.into_iter().map(|r| (r.id, r.clean_name)).collect();
        assert_eq!(pairs, expected);

        // Descending is the order no unordered scan can produce by accident, so
        // this is what pins the shared ORDER BY rather than a lucky row order.
        let desc = InternalItemsQuery {
            order_by: vec![(
                ferrofin_model::live_tv::ItemSortBy::SortName,
                ferrofin_model::dto::SortOrder::Descending,
            )],
            ..query.clone()
        };
        assert_eq!(
            repository
                .get_item_id_clean_names(&desc)
                .await
                .expect("desc pairs")
                .iter()
                .map(|(_, c)| c.clone())
                .collect::<Vec<_>>(),
            vec![
                Some("zulu".to_owned()),
                Some("mike".to_owned()),
                Some("alpha".to_owned())
            ]
        );

        // Paging applies to the projection exactly as it does to the rows.
        let paged = InternalItemsQuery {
            limit: Some(2),
            start_index: Some(1),
            ..query.clone()
        };
        let paged_rows: Vec<(String, Option<String>)> = repository
            .get_item_list(&paged)
            .await
            .expect("paged rows")
            .into_iter()
            .map(|r| (r.id, r.clean_name))
            .collect();
        assert_eq!(paged_rows.len(), 2);
        assert_eq!(
            repository
                .get_item_id_clean_names(&paged)
                .await
                .expect("paged pairs"),
            paged_rows
        );
    }

    #[tokio::test]
    async fn recursive_parent_matches_descendants_via_ancestor_closure() {
        let db = test_db().await;
        let repository = repo(&db);
        // library ─ series ─ episode. The episode is a direct child of the series,
        // NOT of the library, but the library is in its ancestor closure.
        let library = Uuid::from_u128(0xB001);
        let series = Uuid::from_u128(0xB002);
        let episode = Uuid::from_u128(0xB003);
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "TV").await;
        seed_named_item(&db, series, BaseItemKind::Series, "Show").await;
        seed_named_item(&db, episode, BaseItemKind::Episode, "Pilot").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(series))
            .bind(guid_to_db(library))
            .execute(db.writer())
            .await
            .expect("series parent");
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(episode))
            .bind(guid_to_db(series))
            .execute(db.writer())
            .await
            .expect("episode parent");
        for ancestor in [series, library] {
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
                .bind(guid_to_db(episode))
                .bind(guid_to_db(ancestor))
                .execute(db.writer())
                .await
                .expect("ancestor");
        }

        // Non-recursive: the library has one direct child (the series), no episode.
        let direct = InternalItemsQuery {
            parent_id: library,
            include_item_types: vec![BaseItemKind::Episode],
            ..InternalItemsQuery::default()
        };
        assert!(
            repository
                .get_item_list(&direct)
                .await
                .expect("direct")
                .is_empty()
        );

        // Recursive: the episode is reached through the ancestor closure.
        let recursive = InternalItemsQuery {
            parent_id: library,
            recursive: true,
            include_item_types: vec![BaseItemKind::Episode],
            ..InternalItemsQuery::default()
        };
        let rows = repository
            .get_item_list(&recursive)
            .await
            .expect("recursive");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, guid_to_db(episode));
    }

    #[tokio::test]
    async fn boxset_parent_browse_surfaces_linked_children() {
        use crate::linked_children_service::FerrofinLinkedChildrenService;
        use ferrofin_traits::persistence::LinkedChildrenService;

        let db = test_db().await;
        let repository = repo(&db);
        // A box-set and a movie. Membership lives ONLY as a LinkedChildren edge
        // (the movie's physical ParentId is unrelated), so a plain `ParentId`
        // browse must not see it — the merged `GetChildren` behaviour must.
        let boxset = Uuid::from_u128(0xB5E7);
        let movie = Uuid::from_u128(0xB5E8);
        seed_named_item(&db, boxset, BaseItemKind::BoxSet, "Trilogy").await;
        seed_named_item(&db, movie, BaseItemKind::Movie, "Part One").await;

        let links = FerrofinLinkedChildrenService::new(db.clone());
        // `add_to_collection` inserts a manual (ChildType = 0) edge.
        links
            .upsert_linked_child(boxset, movie, 0)
            .await
            .expect("add_to_collection");

        let query = InternalItemsQuery {
            parent_id: boxset,
            ..InternalItemsQuery::default()
        };
        let rows = repository.get_item_list(&query).await.expect("browse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, guid_to_db(movie));

        // Removing the membership makes the browse empty again.
        sqlx::query(
            r#"DELETE FROM "FerrofinLinkedChildren" WHERE "ParentId" = ?1 AND "ChildId" = ?2"#,
        )
        .bind(guid_to_db(boxset))
        .bind(guid_to_db(movie))
        .execute(db.writer())
        .await
        .expect("remove_from_collection");
        assert!(
            repository
                .get_item_list(&query)
                .await
                .expect("browse after remove")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn person_ids_filter_returns_that_persons_filmography() {
        let db = test_db().await;
        let repository = repo(&db);
        // Two movies; a person credited on only the first.
        let movie_a = Uuid::from_u128(0xC001);
        let movie_b = Uuid::from_u128(0xC002);
        let person = Uuid::from_u128(0xC0FF);
        seed_named_item(&db, movie_a, BaseItemKind::Movie, "Heat").await;
        seed_named_item(&db, movie_b, BaseItemKind::Movie, "Solaris").await;
        sqlx::query(
            r#"INSERT INTO "Peoples" ("Id","Name","PersonType") VALUES (?1,'Al Pacino','Actor')"#,
        )
        .bind(guid_to_db(person))
        .execute(db.writer())
        .await
        .expect("person");
        sqlx::query(
            r#"INSERT INTO "PeopleBaseItemMap" ("ItemId","PeopleId","Role","ListOrder","SortOrder")
               VALUES (?1,?2,'',0,0)"#,
        )
        .bind(guid_to_db(movie_a))
        .bind(guid_to_db(person))
        .execute(db.writer())
        .await
        .expect("credit");

        // By id: only the credited movie.
        let by_id = InternalItemsQuery {
            person_ids: vec![person],
            ..InternalItemsQuery::default()
        };
        let rows = repository.get_item_list(&by_id).await.expect("by id");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, guid_to_db(movie_a));

        // By name resolves the same filmography.
        let by_name = InternalItemsQuery {
            person: Some("Al Pacino".to_owned()),
            ..InternalItemsQuery::default()
        };
        assert_eq!(
            repository
                .get_item_list(&by_name)
                .await
                .expect("by name")
                .len(),
            1
        );

        // Production grain: the browsable `Person` item has a DIFFERENT id from
        // the `Peoples` row (per-name deterministic vs per-(name,type) row id).
        // The client opens the person by its item id — the filmography must still
        // resolve via the name bridge, not just when the two ids coincide.
        let person_item = Uuid::from_u128(0xB0FF);
        assert_ne!(person_item, person);
        sqlx::query(
            r#"INSERT INTO "BaseItems"
               ("Id","Type","Name","IsFolder","IsInMixedFolder","IsLocked",
                "IsMovie","IsRepeat","IsSeries","IsVirtualItem")
               VALUES (?1,'MediaBrowser.Controller.Entities.Person','Al Pacino',
                       0,0,0,0,0,0,0)"#,
        )
        .bind(guid_to_db(person_item))
        .execute(db.writer())
        .await
        .expect("person item");
        let by_item_id = InternalItemsQuery {
            person_ids: vec![person_item],
            ..InternalItemsQuery::default()
        };
        let rows = repository
            .get_item_list(&by_item_id)
            .await
            .expect("by item id");
        assert_eq!(
            rows.len(),
            1,
            "person item id resolves filmography via name"
        );
        assert_eq!(rows[0].id, guid_to_db(movie_a));
    }

    #[tokio::test]
    async fn any_provider_id_equals_matches_exact_value_case_insensitively() {
        let db = test_db().await;
        let repository = repo(&db);

        // Two movies: Heat (Imdb tt0113277 + Tmdb 949) and Solaris (Tmdb 296).
        let heat = Uuid::from_u128(0xA001);
        let solaris = Uuid::from_u128(0xA002);
        seed_named_item(&db, heat, BaseItemKind::Movie, "Heat").await;
        seed_named_item(&db, solaris, BaseItemKind::Movie, "Solaris").await;
        for (item, provider, value) in [
            (heat, "Imdb", "tt0113277"),
            (heat, "Tmdb", "949"),
            (solaris, "Tmdb", "296"),
        ] {
            sqlx::query(
                r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue")
                   VALUES (?1, ?2, ?3)"#,
            )
            .bind(guid_to_db(item))
            .bind(provider)
            .bind(value)
            .execute(db.writer())
            .await
            .expect("insert provider");
        }

        // Exact IMDb match (with a different-case value) selects only Heat.
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            any_provider_id_equals: vec![("imdb".to_owned(), "TT0113277".to_owned())],
            ..InternalItemsQuery::default()
        };
        let rows = repository.get_item_list(&query).await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, guid_to_db(heat));

        // A non-matching value returns nothing (no partial/prefix matching).
        let miss = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            any_provider_id_equals: vec![("Tmdb".to_owned(), "0".to_owned())],
            ..InternalItemsQuery::default()
        };
        assert!(
            repository
                .get_item_list(&miss)
                .await
                .expect("miss")
                .is_empty()
        );

        // Multiple pairs are OR-ed: Tmdb 296 OR Tmdb 949 selects both movies.
        let both = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            any_provider_id_equals: vec![
                ("Tmdb".to_owned(), "296".to_owned()),
                ("Tmdb".to_owned(), "949".to_owned()),
            ],
            ..InternalItemsQuery::default()
        };
        assert_eq!(
            repository.get_item_list(&both).await.expect("both").len(),
            2
        );
    }

    #[tokio::test]
    async fn get_image_infos_reads_rows_ordered_by_type() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0x9001);
        seed_named_item(&db, item, BaseItemKind::Movie, "Imaged").await;

        // A Backdrop (type 2) and a Primary (type 0); the query orders by type so
        // Primary comes back first regardless of insertion order.
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
                ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                VALUES (?1, ?2, NULL, 1080, 2, ?3, '/backdrop.jpg', 1920)"#,
        )
        .bind(guid_to_db(Uuid::from_u128(0x9101)))
        .bind("LKO2".as_bytes().to_vec())
        .bind(guid_to_db(item))
        .execute(db.writer())
        .await
        .expect("insert backdrop");

        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
                ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                VALUES (?1, NULL, NULL, 0, 0, ?2, '/poster.jpg', 0)"#,
        )
        .bind(guid_to_db(Uuid::from_u128(0x9102)))
        .bind(guid_to_db(item))
        .execute(db.writer())
        .await
        .expect("insert primary");

        let images = repository.get_image_infos(item).await.expect("images");
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].image_type, ImageType::Primary);
        assert_eq!(images[0].path, "/poster.jpg");
        assert!(images[0].blur_hash.is_none());
        assert_eq!(images[1].image_type, ImageType::Backdrop);
        assert_eq!(images[1].path, "/backdrop.jpg");
        assert_eq!(images[1].width, 1920);
        assert_eq!(images[1].blur_hash.as_deref(), Some("LKO2"));

        // An item with no images yields an empty list.
        let none = repository
            .get_image_infos(Uuid::from_u128(0xDEAD))
            .await
            .expect("no images");
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn swap_item_images_reorders_two_backdrops() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0x9200);
        seed_named_item(&db, item, BaseItemKind::Movie, "Reorder Me").await;

        // Three backdrops (type 2), addressed by index 0/1/2 in Id order.
        for (n, path) in [(0u128, "/a.jpg"), (1, "/b.jpg"), (2, "/c.jpg")] {
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos"
                    ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                    VALUES (?1, NULL, NULL, 1080, 2, ?2, ?3, 1920)"#,
            )
            .bind(guid_to_db(Uuid::from_u128(0x9210 + n)))
            .bind(guid_to_db(item))
            .bind(path)
            .execute(db.writer())
            .await
            .expect("insert backdrop");
        }

        // Swap index 0 (/a.jpg) with index 2 (/c.jpg).
        repository
            .swap_item_images(item, ImageType::Backdrop, 0, 2)
            .await
            .expect("swap");

        let images = repository.get_image_infos(item).await.expect("images");
        assert_eq!(images.len(), 3);
        // Paths are exchanged; the middle one is untouched. Dimensions of the two
        // swapped rows are reset to the unknown sentinel (0), matching C#.
        assert_eq!(images[0].path, "/c.jpg");
        assert_eq!(images[0].width, 0);
        assert_eq!(images[0].height, 0);
        assert_eq!(images[1].path, "/b.jpg");
        assert_eq!(images[1].width, 1920);
        assert_eq!(images[2].path, "/a.jpg");
        assert_eq!(images[2].width, 0);
    }

    #[tokio::test]
    async fn swap_item_images_out_of_range_index_is_noop() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0x9300);
        seed_named_item(&db, item, BaseItemKind::Movie, "One Backdrop").await;
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
                ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                VALUES (?1, NULL, NULL, 1080, 2, ?2, '/only.jpg', 1920)"#,
        )
        .bind(guid_to_db(Uuid::from_u128(0x9310)))
        .bind(guid_to_db(item))
        .execute(db.writer())
        .await
        .expect("insert backdrop");

        // Index 5 does not exist — a faithful no-op, and the row is untouched.
        repository
            .swap_item_images(item, ImageType::Backdrop, 0, 5)
            .await
            .expect("noop swap");
        let images = repository.get_image_infos(item).await.expect("images");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].path, "/only.jpg");
        assert_eq!(images[0].width, 1920);
    }

    #[tokio::test]
    async fn genres_studios_artists_roll_up_with_counts() {
        let db = test_db().await;
        let repository = repo(&db);

        // Seeding a movie's genre also materializes the browsable by-name Genre
        // row (id = ItemValueId), the way the scanner does.
        let movie = Uuid::from_u128(0x8002);
        seed_named_item(&db, movie, BaseItemKind::Movie, "A Drama Film").await;
        seed_item_genre(&db, movie, "Drama").await;

        let genres = repository
            .get_genres(&InternalItemsQuery::default())
            .await
            .expect("genres");
        assert_eq!(genres.items.len(), 1);
        assert_eq!(genres.items[0].counts.item_count, 1);

        // Studios / artists / album artists / all-artists resolve (empty here) and
        // music genres too — exercising every by-name entry point.
        assert!(
            repository
                .get_studios(&InternalItemsQuery::default())
                .await
                .expect("studios")
                .items
                .is_empty()
        );
        assert!(
            repository
                .get_artists(&InternalItemsQuery::default())
                .await
                .expect("artists")
                .items
                .is_empty()
        );
        assert!(
            repository
                .get_album_artists(&InternalItemsQuery::default())
                .await
                .expect("album artists")
                .items
                .is_empty()
        );
        assert!(
            repository
                .get_all_artists(&InternalItemsQuery::default())
                .await
                .expect("all artists")
                .items
                .is_empty()
        );
        assert!(
            repository
                .get_music_genres(&InternalItemsQuery::default())
                .await
                .expect("music genres")
                .items
                .is_empty()
        );
    }

    /// The by-name browse runs the SAME `ApplyOrder` the main browse does
    /// (C# `GetItemValues` -> `ApplyOrder(query, filter, context)`), and scopes
    /// its inner content query by `IncludeItemTypes` ALONE.
    ///
    /// Neither was true before: the ORDER BY was hardcoded `bi."Name" ASC` so
    /// `sortOrder` was a silent no-op, and `include_item_types` was UNIONed with
    /// the caller-forced content types, so naming a kind *widened* the aggregate
    /// instead of narrowing it. The parity diff engine aligns arrays by `Name`,
    /// which is exactly why the ordering half went unnoticed.
    #[tokio::test]
    async fn by_name_order_and_include_item_types_reach_the_sql() {
        let db = test_db().await;
        let repository = repo(&db);

        let movie = Uuid::from_u128(0xA011);
        seed_named_item(&db, movie, BaseItemKind::Movie, "One").await;
        for g in ["Action", "Adventure", "Comedy", "Drama", "Horror"] {
            seed_item_genre(&db, movie, g).await;
        }

        let desc = InternalItemsQuery {
            order_by: vec![(
                ferrofin_model::live_tv::ItemSortBy::SortName,
                ferrofin_model::dto::SortOrder::Descending,
            )],
            ..Default::default()
        };
        let desc_names: Vec<_> = repository
            .get_genres(&desc)
            .await
            .expect("desc")
            .items
            .iter()
            .filter_map(|i| i.item.name.clone())
            .collect();
        assert_eq!(
            desc_names,
            vec!["Horror", "Drama", "Comedy", "Adventure", "Action"]
        );

        // No Audio item carries these genres, so the aggregate is empty.
        let audio_only = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Audio],
            ..Default::default()
        };
        assert!(
            repository
                .get_genres(&audio_only)
                .await
                .expect("audio only")
                .items
                .is_empty()
        );

        // ...and naming the kind that DOES carry them keeps all five.
        let movie_only = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            ..Default::default()
        };
        assert_eq!(
            repository
                .get_genres(&movie_only)
                .await
                .expect("movie only")
                .items
                .len(),
            5
        );
    }

    #[tokio::test]
    async fn by_name_paging_and_filters_push_into_sql() {
        let db = test_db().await;
        let repository = repo(&db);

        // Five distinct genres, ordered by Name: Action, Adventure, Comedy, Drama,
        // Horror. A second movie shares "Drama" so its in-scope count is 2.
        let movie = Uuid::from_u128(0xA001);
        seed_named_item(&db, movie, BaseItemKind::Movie, "One").await;
        for g in ["Action", "Adventure", "Comedy", "Drama", "Horror"] {
            seed_item_genre(&db, movie, g).await;
        }
        let movie2 = Uuid::from_u128(0xA002);
        seed_named_item(&db, movie2, BaseItemKind::Movie, "Two").await;
        seed_item_genre(&db, movie2, "Drama").await;

        // Paging: offset 1, limit 2 → [Adventure, Comedy], total = all 5.
        let page = InternalItemsQuery {
            start_index: Some(1),
            limit: Some(2),
            ..Default::default()
        };
        let paged = repository.get_genres(&page).await.expect("paged");
        let names: Vec<_> = paged
            .items
            .iter()
            .filter_map(|i| i.item.name.clone())
            .collect();
        assert_eq!(names, vec!["Adventure", "Comedy"]);
        assert_eq!(paged.start_index, 1);
        assert_eq!(paged.total_record_count, 5);

        // nameStartsWith is a prefix filter on the by-name row. Unpaged responses
        // carry TotalRecordCount 0 — C# forces EnableTotalRecordCount off without
        // a Limit and the non-nullable int is never assigned.
        let starts = InternalItemsQuery {
            name_starts_with: Some("A".to_owned()),
            ..Default::default()
        };
        let a = repository.get_genres(&starts).await.expect("starts");
        let a_names: Vec<_> = a.items.iter().filter_map(|i| i.item.name.clone()).collect();
        assert_eq!(a_names, vec!["Action", "Adventure"]);
        assert_eq!(a.total_record_count, 0);

        // nameLessThan is a FULL-STRING comparison of `SortName` against the
        // lowercased bound (C# `SortName.CompareTo(bound.ToLowerInvariant()) < 0`).
        // "Comedy" is excluded because "comedy" < "comedy" is false…
        let less = InternalItemsQuery {
            name_less_than: Some("Comedy".to_owned()),
            ..Default::default()
        };
        let l = repository.get_genres(&less).await.expect("less");
        let l_names: Vec<_> = l.items.iter().filter_map(|i| i.item.name.clone()).collect();
        assert_eq!(l_names, vec!["Action", "Adventure"]);
        // …but it IS included under "Cz", because the whole string is compared:
        // "comedy" < "cz". A first-character-only test excluded it.
        let less_cz = InternalItemsQuery {
            name_less_than: Some("Cz".to_owned()),
            ..Default::default()
        };
        let lcz = repository.get_genres(&less_cz).await.expect("less cz");
        let lcz_names: Vec<_> = lcz
            .items
            .iter()
            .filter_map(|i| i.item.name.clone())
            .collect();
        assert_eq!(lcz_names, vec!["Action", "Adventure", "Comedy"]);

        // nameStartsWithOrGreater is the INCLUSIVE mirror (`>= 0`), also
        // lowercased and full-string: the boundary row itself is kept…
        let ge = InternalItemsQuery {
            name_starts_with_or_greater: Some("Comedy".to_owned()),
            ..Default::default()
        };
        let ge_names: Vec<_> = repository
            .get_genres(&ge)
            .await
            .expect("ge")
            .items
            .iter()
            .filter_map(|i| i.item.name.clone())
            .collect();
        assert_eq!(ge_names, vec!["Comedy", "Drama", "Horror"]);
        // …and the bound is case-insensitive, because C# lowercases it before
        // comparing against the already-lowercase `SortName`.
        let ge_lower = InternalItemsQuery {
            name_starts_with_or_greater: Some("comedy".to_owned()),
            ..Default::default()
        };
        let ge_lower_names: Vec<_> = repository
            .get_genres(&ge_lower)
            .await
            .expect("ge lower")
            .items
            .iter()
            .filter_map(|i| i.item.name.clone())
            .collect();
        assert_eq!(ge_lower_names, vec!["Comedy", "Drama", "Horror"]);

        // A plain searchTerm is a literal contains-match on CleanName, and the
        // count survives it.
        let search = InternalItemsQuery {
            search_term: Some("ram".to_owned()),
            ..Default::default()
        };
        let s = repository.get_genres(&search).await.expect("search");
        assert_eq!(s.items.len(), 1);
        assert_eq!(s.items[0].item.name.as_deref(), Some("Drama"));
        assert_eq!(s.items[0].counts.item_count, 2);

        // A searchTerm carrying a wildcard character routes through raw LIKE —
        // the `_` matches any one character (C# SearchWildcardTerms branch).
        let wild = InternalItemsQuery {
            search_term: Some("dr_ma".to_owned()),
            ..Default::default()
        };
        let w = repository.get_genres(&wild).await.expect("wild");
        assert_eq!(w.items.len(), 1);
        assert_eq!(w.items[0].item.name.as_deref(), Some("Drama"));
    }

    #[tokio::test]
    async fn is_liked_uses_upstreams_min_like_value_of_six_point_five() {
        // `UserItemData.MinLikeValue` is 6.5, so a 6.5 rating is liked and a 6 is
        // not; a whole-number threshold of 7 would drop 6.5 and 6.9.
        let db = test_db().await;
        let repository = repo(&db);
        let user_id = Uuid::from_u128(0x11B0);
        let user = seed_user_with_defaults(&db, user_id).await;

        let rated = |rating: f64, id: u128, name: &'static str| {
            let db = db.clone();
            async move {
                let item = Uuid::from_u128(id);
                seed_named_item(&db, item, BaseItemKind::Movie, name).await;
                sqlx::query(
                    r#"INSERT INTO "UserData"
                       ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
                        "PlaybackPositionTicks", "Played", "Rating")
                       VALUES (?1, ?2, ?1, 0, 0, 0, 0, ?3)"#,
                )
                .bind(guid_to_db(item))
                .bind(guid_to_db(user_id))
                .bind(rating)
                .execute(db.writer())
                .await
                .expect("rate");
            }
        };
        rated(6.0, 0x11B1, "Six").await;
        rated(6.5, 0x11B2, "Six And A Half").await;
        rated(9.0, 0x11B3, "Nine").await;
        // An unscoped query as a user is confined to that user's libraries.
        seed_library_over(
            &db,
            &[
                Uuid::from_u128(0x11B1),
                Uuid::from_u128(0x11B2),
                Uuid::from_u128(0x11B3),
            ],
        )
        .await;

        let liked = InternalItemsQuery {
            user: Some(user),
            is_liked: Some(true),
            include_item_types: vec![BaseItemKind::Movie],
            order_by: vec![(
                ferrofin_model::live_tv::ItemSortBy::SortName,
                ferrofin_model::dto::SortOrder::Ascending,
            )],
            ..Default::default()
        };
        let mut names: Vec<_> = repository
            .get_item_list(&liked)
            .await
            .expect("liked")
            .into_iter()
            .filter_map(|i| i.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["Nine", "Six And A Half"]);
    }

    #[tokio::test]
    async fn by_name_favorite_filter_matches_the_genre_rows_user_data() {
        let db = test_db().await;
        let repository = repo(&db);

        // Two movies carrying one genre each; seeding materializes the browsable
        // by-name Genre rows (id = ItemValueId).
        //
        // They hang off a library, and the user holds the default permissions,
        // because the by-name tabs are confined to the user's libraries now
        // (`AddUserToQuery`): a permission-less account, or one on a server with
        // no library rows at all, is correctly offered no genres.
        let library = Uuid::from_u128(0xFA00);
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "Movies").await;
        let action_movie = Uuid::from_u128(0xFA01);
        seed_top_parented_item(
            &db,
            action_movie,
            BaseItemKind::Movie,
            "Action Film",
            library,
        )
        .await;
        seed_item_genre(&db, action_movie, "Action").await;
        let drama_movie = Uuid::from_u128(0xFA02);
        seed_top_parented_item(&db, drama_movie, BaseItemKind::Movie, "Drama Film", library).await;
        seed_item_genre(&db, drama_movie, "Drama").await;

        let user_id = Uuid::from_u128(0xFA10);
        let user = seed_user_with_defaults(&db, user_id).await;

        // Favorite the "Action" genre: the state lives in the by-name row's OWN
        // UserData (C# joins UserData on the by-name item id), so the row is
        // keyed to the materialized Genre item, not to either movie.
        let genre_id: String = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems"
               WHERE "Name" = 'Action'
                 AND "Type" = 'MediaBrowser.Controller.Entities.Genre'"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("materialized genre row");
        sqlx::query(
            r#"INSERT INTO "UserData"
               ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
                "PlaybackPositionTicks", "Played")
               VALUES (?1, ?2, ?1, 1, 0, 0, 0)"#,
        )
        .bind(&genre_id)
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("favorite the genre");

        // isFavorite=true keeps only the favorited genre…
        let fav = InternalItemsQuery {
            user: Some(user.clone()),
            is_favorite: Some(true),
            ..Default::default()
        };
        let got = repository.get_genres(&fav).await.expect("favorites");
        let names: Vec<_> = got
            .items
            .iter()
            .filter_map(|i| i.item.name.clone())
            .collect();
        assert_eq!(names, vec!["Action"]);

        // …and isFavorite=false keeps only the un-favorited one (NOT EXISTS).
        let not_fav = InternalItemsQuery {
            user: Some(user),
            is_favorite: Some(false),
            ..Default::default()
        };
        let got = repository
            .get_genres(&not_fav)
            .await
            .expect("non-favorites");
        let names: Vec<_> = got
            .items
            .iter()
            .filter_map(|i| i.item.name.clone())
            .collect();
        assert_eq!(names, vec!["Drama"]);
    }

    #[tokio::test]
    async fn value_name_lists_are_distinct_and_ordered() {
        let db = test_db().await;
        let repository = repo(&db);

        let movie = Uuid::from_u128(0x9001);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Genred").await;
        seed_item_genre(&db, movie, "Zeta").await;
        seed_item_genre(&db, movie, "Alpha").await;
        // A second item sharing "Alpha" must not duplicate it.
        let movie2 = Uuid::from_u128(0x9002);
        seed_named_item(&db, movie2, BaseItemKind::Movie, "Genred Two").await;
        seed_item_genre(&db, movie2, "Alpha").await;

        let names = repository.get_genre_names().await.expect("genre names");
        assert_eq!(names, vec!["Alpha".to_owned(), "Zeta".to_owned()]);

        // The remaining name lists execute their SQL (empty result sets).
        assert!(
            repository
                .get_studio_names()
                .await
                .expect("studios")
                .is_empty()
        );
        assert!(
            repository
                .get_all_artist_names()
                .await
                .expect("artists")
                .is_empty()
        );
        assert!(
            repository
                .get_music_genre_names()
                .await
                .expect("music genres")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn query_filters_legacy_collects_years_ratings_genres_tags() {
        let db = test_db().await;
        let repository = repo(&db);

        let movie = Uuid::from_u128(0xA001);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Rated").await;
        seed_item_genre(&db, movie, "Comedy").await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "ProductionYear" = 1999, "OfficialRating" = 'PG-13'
               WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(movie))
        .execute(db.writer())
        .await
        .expect("set year/rating");

        let filters = repository
            .get_query_filters_legacy(&InternalItemsQuery::default())
            .await
            .expect("filters");
        assert_eq!(filters.years, vec![1999]);
        assert_eq!(filters.official_ratings, vec!["PG-13".to_owned()]);
        assert_eq!(filters.genres, vec!["Comedy".to_owned()]);
        assert!(filters.tags.is_empty());

        // With no matching items the filters come back empty (early return).
        let none = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Book],
            ..Default::default()
        };
        let empty = repository
            .get_query_filters_legacy(&none)
            .await
            .expect("empty filters");
        assert!(empty.years.is_empty() && empty.genres.is_empty());
    }

    /// `/Years` reads the year facet on its own. The override must answer
    /// exactly what the four-statement aggregate's `years` field held — the
    /// same filter, the same values, the same order — for every shape the
    /// aggregate handles, including the empty one.
    #[tokio::test]
    async fn distinct_years_answers_the_aggregates_year_facet() {
        let db = test_db().await;
        let repository = repo(&db);

        for (n, year) in [(0xA201_u128, 1999), (0xA202, 1977), (0xA203, 1999)] {
            let id = Uuid::from_u128(n);
            seed_named_item(&db, id, BaseItemKind::Movie, &format!("Film {n}")).await;
            sqlx::query(r#"UPDATE "BaseItems" SET "ProductionYear" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .bind(i64::from(year))
                .execute(db.writer())
                .await
                .expect("set year");
        }
        // A yearless row must not become a phantom facet value.
        seed_named_item(&db, Uuid::from_u128(0xA204), BaseItemKind::Movie, "No Year").await;

        for filter in [
            InternalItemsQuery::default(),
            InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Movie],
                ..Default::default()
            },
            // Matches nothing.
            InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Book],
                ..Default::default()
            },
        ] {
            let aggregate = repository
                .get_query_filters_legacy(&filter)
                .await
                .expect("aggregate");
            let direct = repository
                .get_distinct_years(&filter)
                .await
                .expect("distinct years");
            assert_eq!(
                direct, aggregate.years,
                "the year-only read must match the aggregate's facet"
            );
        }
        assert_eq!(
            repository
                .get_distinct_years(&InternalItemsQuery::default())
                .await
                .expect("years"),
            vec![1977, 1999],
            "distinct, ascending, and no entry for the yearless row"
        );
    }

    #[tokio::test]
    async fn media_stream_languages_dedup_and_default_und() {
        let db = test_db().await;
        let repository = repo(&db);

        let item = Uuid::from_u128(0xB001);
        seed_item(&db, item, BaseItemKind::Movie).await;
        // Two audio streams: one English, one with no language → 'und'.
        for (idx, lang) in [(0_i64, Some("eng")), (1, None)] {
            sqlx::query(
                r#"INSERT INTO "MediaStreamInfos"
                   ("ItemId", "StreamIndex", "IsDefault", "IsExternal", "IsForced",
                    "StreamType", "Language")
                   VALUES (?1, ?2, 0, 0, 0, 0, ?3)"#,
            )
            .bind(guid_to_db(item))
            .bind(idx)
            .bind(lang)
            .execute(db.writer())
            .await
            .expect("insert stream");
        }

        let mut langs = repository
            .get_media_stream_languages(&InternalItemsQuery::default(), MediaStreamType::Audio)
            .await
            .expect("langs");
        langs.sort();
        assert_eq!(langs, vec!["eng".to_owned(), "und".to_owned()]);

        // No matching items → empty (early return before the stream query).
        let none = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Book],
            ..Default::default()
        };
        assert!(
            repository
                .get_media_stream_languages(&none, MediaStreamType::Audio)
                .await
                .expect("empty langs")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn is_played_reflects_direct_children_user_data() {
        let db = test_db().await;
        let repository = repo(&db);

        let parent = Uuid::from_u128(0xC001);
        seed_item(&db, parent, BaseItemKind::Series).await;
        let child = Uuid::from_u128(0xC002);
        seed_item(&db, child, BaseItemKind::Episode).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(child))
            .bind(guid_to_db(parent))
            .execute(db.writer())
            .await
            .expect("set parent");

        let user = seed_user(&db, Uuid::from_u128(0xC0DE)).await;
        seed_user_data(&db, Uuid::from_u128(0xC0DE), child, true, None).await;

        // The recursive branch runs its AncestorIds join. With an ancestor row
        // present, the played child makes the closure fully played.
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(child))
            .bind(guid_to_db(parent))
            .execute(db.writer())
            .await
            .expect("ancestor");
        assert!(
            repository
                .get_is_played(&user, parent, true)
                .await
                .expect("is_played recursive")
        );

        // Non-recursive branch: the played child is a direct child of `parent`
        // (its ParentId was set above), so the direct-children closure is fully
        // played.
        assert!(
            repository
                .get_is_played(&user, parent, false)
                .await
                .expect("is_played non-recursive")
        );

        // And an UNplayed direct child makes it not-all-played.
        let unplayed = Uuid::from_u128(0xBEEF);
        seed_item(&db, unplayed, BaseItemKind::Episode).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(unplayed))
            .bind(guid_to_db(parent))
            .execute(db.writer())
            .await
            .expect("set parent of unplayed");
        assert!(
            !repository
                .get_is_played(&user, parent, false)
                .await
                .expect("is_played non-recursive with unplayed child")
        );
    }

    /// Seeds a full row through the persistence service so `DateCreated`,
    /// `SeriesName`/`Album`, `SortName` and `Type` are all under test control
    /// (the `test_support` inserts leave the dates `NULL`).
    async fn seed_latest_row(
        db: &Database,
        id: Uuid,
        kind: BaseItemKind,
        name: &str,
        group: &str,
        created: chrono::DateTime<Utc>,
    ) {
        let persistence =
            crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone());
        let (series_name, album) = match kind {
            BaseItemKind::Episode => (Some(group.to_owned()), None),
            BaseItemKind::Audio => (None, Some(group.to_owned())),
            _ => (None, None),
        };
        persistence
            .save_items(&[BaseItemEntity {
                id: guid_to_db(id),
                type_: stored_type_name(kind).unwrap_or_default().to_owned(),
                name: Some(name.to_owned()),
                sort_name: Some(name.to_owned()),
                series_name,
                album,
                date_created: Some(created),
                ..BaseItemEntity::default()
            }])
            .await
            .expect("seed latest row");
    }

    fn day(n: u32) -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(&format!("2024-01-{n:02}T00:00:00Z"))
            .expect("date")
            .with_timezone(&Utc)
    }

    /// The C# early exit: only `tvshows` and `music` take the grouped path —
    /// `movies` (which the old port accepted) and `books` return nothing, the
    /// user-view manager routes those through `get_item_list` instead.
    #[tokio::test]
    async fn latest_item_list_is_empty_for_movies_and_books() {
        let db = test_db().await;
        let repository = repo(&db);
        seed_latest_row(
            &db,
            Uuid::from_u128(0xD001),
            BaseItemKind::Movie,
            "New",
            "",
            day(1),
        )
        .await;

        for ct in [CollectionType::movies, CollectionType::books] {
            let rows = repository
                .get_latest_item_list(&InternalItemsQuery::default(), ct)
                .await
                .expect("latest");
            assert!(rows.is_empty(), "{ct:?} must early-exit empty");
        }
    }

    /// tvshows groups by `SeriesName`: with `limit` 2 and three series whose
    /// newest episodes are day 9 / day 5 / day 3, the threshold is day 5 (the
    /// smallest of the top-two maxima) — so series C is out entirely, and
    /// series A's OLD episode (day 2, older than the threshold) is out too,
    /// even though its series is in. That is upstream's exact semantics.
    #[tokio::test]
    async fn latest_item_list_groups_by_series_name_for_tvshows_with_min_of_top_n_threshold() {
        let db = test_db().await;
        let repository = repo(&db);
        let (a_new, a_old, b_new, b_mid, c_new) = (
            Uuid::from_u128(0xD101),
            Uuid::from_u128(0xD102),
            Uuid::from_u128(0xD103),
            Uuid::from_u128(0xD104),
            Uuid::from_u128(0xD105),
        );
        seed_latest_row(
            &db,
            a_new,
            BaseItemKind::Episode,
            "A e2",
            "Series A",
            day(9),
        )
        .await;
        seed_latest_row(
            &db,
            a_old,
            BaseItemKind::Episode,
            "A e1",
            "Series A",
            day(2),
        )
        .await;
        seed_latest_row(
            &db,
            b_new,
            BaseItemKind::Episode,
            "B e2",
            "Series B",
            day(5),
        )
        .await;
        seed_latest_row(
            &db,
            b_mid,
            BaseItemKind::Episode,
            "B e1",
            "Series B",
            day(5),
        )
        .await;
        seed_latest_row(
            &db,
            c_new,
            BaseItemKind::Episode,
            "C e1",
            "Series C",
            day(3),
        )
        .await;

        let filter = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Episode],
            limit: Some(2),
            order_by: vec![
                (
                    ferrofin_model::live_tv::ItemSortBy::DateCreated,
                    ferrofin_model::dto::SortOrder::Descending,
                ),
                (
                    ferrofin_model::live_tv::ItemSortBy::SortName,
                    ferrofin_model::dto::SortOrder::Descending,
                ),
            ],
            ..InternalItemsQuery::default()
        };
        let rows = repository
            .get_latest_item_list(&filter, CollectionType::tvshows)
            .await
            .expect("latest tvshows");
        let ids: Vec<Uuid> = rows
            .iter()
            .filter_map(|r| Uuid::parse_str(&r.id).ok())
            .collect();
        // DateCreated DESC, then SortName DESC on the day-5 tie ("B e2" > "B e1").
        assert_eq!(ids, vec![a_new, b_new, b_mid]);
    }

    /// music groups by `Album`. The `limit` caps GROUPS: with `limit` 2 the
    /// threshold is Album Y's maximum (day 6), so both of Album X's tracks (8,
    /// 7) and Y's (6) come back while Album Z (day 4) is out. With `limit` 1
    /// the threshold rises to X's own maximum and only the day-8 track
    /// survives — the threshold is a row filter, not a group filter, exactly
    /// as upstream's `DateCreated >= Min(MaxDateCreated)` behaves.
    #[tokio::test]
    async fn latest_item_list_groups_by_album_for_music() {
        let db = test_db().await;
        let repository = repo(&db);
        let (x1, x2, y1, z1) = (
            Uuid::from_u128(0xD201),
            Uuid::from_u128(0xD202),
            Uuid::from_u128(0xD203),
            Uuid::from_u128(0xD204),
        );
        seed_latest_row(&db, x1, BaseItemKind::Audio, "X t1", "Album X", day(8)).await;
        seed_latest_row(&db, x2, BaseItemKind::Audio, "X t2", "Album X", day(7)).await;
        seed_latest_row(&db, y1, BaseItemKind::Audio, "Y t1", "Album Y", day(6)).await;
        seed_latest_row(&db, z1, BaseItemKind::Audio, "Z t1", "Album Z", day(4)).await;

        let filter = |limit: i32| InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Audio],
            limit: Some(limit),
            order_by: vec![(
                ferrofin_model::live_tv::ItemSortBy::DateCreated,
                ferrofin_model::dto::SortOrder::Descending,
            )],
            ..InternalItemsQuery::default()
        };
        let ids = |rows: Vec<BaseItemEntity>| -> Vec<Uuid> {
            rows.iter()
                .filter_map(|r| Uuid::parse_str(&r.id).ok())
                .collect()
        };

        let two = repository
            .get_latest_item_list(&filter(2), CollectionType::music)
            .await
            .expect("latest music, two groups");
        assert_eq!(ids(two), vec![x1, x2, y1], "two newest albums, Z is out");

        let one = repository
            .get_latest_item_list(&filter(1), CollectionType::music)
            .await
            .expect("latest music, one group");
        assert_eq!(
            ids(one),
            vec![x1],
            "the threshold is X's own max, so X's older track is below it"
        );

        // No groups (`limit` 0) → `MIN` over nothing is NULL → `>= NULL` is
        // never true → no rows. A "helpful" COALESCE here would turn "no
        // groups" into "every row".
        let none = repository
            .get_latest_item_list(&filter(0), CollectionType::music)
            .await
            .expect("latest music, no groups");
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn extra_types_filter_matches_stored_discriminant() {
        let db = test_db().await;
        let repository = repo(&db);

        // Two extras owned by a movie: one trailer, one behind-the-scenes.
        let owner = Uuid::from_u128(0xE000);
        seed_item(&db, owner, BaseItemKind::Movie).await;
        let trailer = Uuid::from_u128(0xE001);
        seed_named_item(&db, trailer, BaseItemKind::Trailer, "T").await;
        let behind = Uuid::from_u128(0xE002);
        seed_named_item(&db, behind, BaseItemKind::Video, "B").await;
        for (id, extra) in [
            (trailer, ExtraType::Trailer),
            (behind, ExtraType::BehindTheScenes),
        ] {
            sqlx::query(
                r#"UPDATE "BaseItems" SET "OwnerId" = ?2, "ExtraType" = ?3 WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .bind(guid_to_db(owner))
            .bind(extra as i32)
            .execute(db.writer())
            .await
            .expect("set extra");
        }

        // Filtering to Trailer extras owned by `owner` returns only the trailer.
        let query = InternalItemsQuery {
            owner_ids: vec![owner],
            extra_types: vec![ExtraType::Trailer],
            ..InternalItemsQuery::default()
        };
        let res = repository.get_item_list(&query).await.expect("extras");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].id, guid_to_db(trailer));

        // Both display extras (Trailer + BehindTheScenes) return both.
        let query = InternalItemsQuery {
            owner_ids: vec![owner],
            extra_types: vec![ExtraType::Trailer, ExtraType::BehindTheScenes],
            ..InternalItemsQuery::default()
        };
        let res = repository.get_item_list(&query).await.expect("extras");
        assert_eq!(res.len(), 2);
    }

    #[tokio::test]
    async fn is_resumable_filters_on_in_progress_position() {
        let db = test_db().await;
        let repository = repo(&db);

        let user = seed_user_with_defaults(&db, Uuid::from_u128(0xF00D)).await;
        let resumable = Uuid::from_u128(0xF001);
        seed_item(&db, resumable, BaseItemKind::Movie).await;
        let not_resumable = Uuid::from_u128(0xF002);
        seed_item(&db, not_resumable, BaseItemKind::Movie).await;
        // An unscoped query as a user is confined to that user's libraries.
        seed_library_over(&db, &[resumable, not_resumable]).await;

        // A user-data row with a non-zero position marks the first item resumable.
        seed_user_data(&db, Uuid::from_u128(0xF00D), resumable, false, None).await;
        sqlx::query(r#"UPDATE "UserData" SET "PlaybackPositionTicks" = 5000 WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(resumable))
            .execute(db.writer())
            .await
            .expect("set position");

        let query = InternalItemsQuery {
            user: Some(user),
            is_resumable: Some(true),
            ..InternalItemsQuery::default()
        };
        let res = repository.get_item_list(&query).await.expect("resumable");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].id, guid_to_db(resumable));
    }

    #[tokio::test]
    async fn get_items_by_primary_version_returns_alternates_only() {
        let db = test_db().await;
        let repository = repo(&db);
        let primary = Uuid::from_u128(0x0B01);
        let alt = Uuid::from_u128(0x0B02);
        let unrelated = Uuid::from_u128(0x0B03);
        seed_item(&db, primary, BaseItemKind::Movie).await;
        seed_item(&db, alt, BaseItemKind::Movie).await;
        seed_item(&db, unrelated, BaseItemKind::Movie).await;
        // Only `alt` points at `primary`.
        sqlx::query(r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?1 WHERE "Id" = ?2"#)
            .bind(guid_to_db(primary))
            .bind(guid_to_db(alt))
            .execute(db.writer())
            .await
            .expect("link alternate");

        let rows = repository
            .get_items_by_primary_version(primary)
            .await
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, guid_to_db(alt));

        // A nil primary short-circuits to empty without hitting the pool.
        assert!(
            repository
                .get_items_by_primary_version(Uuid::nil())
                .await
                .expect("nil")
                .is_empty()
        );

        // The batch form groups alternates under their primary; primaries with
        // no alternates are absent.
        let batch = repository
            .get_items_by_primary_version_batch(&[primary, unrelated])
            .await
            .expect("batch");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[&primary].len(), 1);
        assert_eq!(batch[&primary][0].id, guid_to_db(alt));
        assert!(!batch.contains_key(&unrelated));
    }

    #[tokio::test]
    async fn by_name_total_survives_offset_past_end() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0xA0F1);
        seed_named_item(&db, movie, BaseItemKind::Movie, "One").await;
        for g in ["Action", "Adventure", "Comedy", "Drama", "Horror"] {
            seed_item_genre(&db, movie, g).await;
        }

        let past = InternalItemsQuery {
            start_index: Some(50),
            limit: Some(2),
            ..Default::default()
        };
        let r = repository.get_genres(&past).await.expect("past");
        assert!(r.items.is_empty());
        assert_eq!(
            r.total_record_count, 5,
            "total must survive an offset past the end"
        );

        let nomatch = InternalItemsQuery {
            search_term: Some("ZZZZ".to_owned()),
            limit: Some(2),
            ..Default::default()
        };
        let z = repository.get_genres(&nomatch).await.expect("z");
        assert_eq!(
            z.total_record_count, 0,
            "genuine empty is 0, not a stale count"
        );
    }

    /// Stamps a row's `PresentationUniqueKey` — the column the grouping keys
    /// off, written through the production writer.
    /// Merges `ids` through the real `LibraryManager::merge_versions`, so the
    /// fixture goes through the same path a client's
    /// `POST /Videos/MergeVersions` does.
    async fn merge_versions_over(db: &Database, ids: &[Uuid]) {
        use ferrofin_traits::library::LibraryManager;
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        crate::library_manager::FerrofinLibraryManager::new(
            Arc::new(FerrofinItemRepository::new(db.clone(), lookup)),
            Arc::new(crate::item_count_service::FerrofinItemCountService::new(
                db.clone(),
            )),
            Arc::new(
                crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone()),
            ),
            Arc::new(crate::people_repository::FerrofinPeopleRepository::new(
                db.clone(),
            )),
        )
        .merge_versions(ids)
        .await
        .expect("merge versions");
    }

    /// Gives `user` a playback position on `item`, which is what makes a row
    /// resumable.
    async fn set_playback_position(db: &Database, item: Uuid, user_id: &str, ticks: i64) {
        sqlx::query(
            r#"INSERT INTO "UserData"
               ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
                "PlaybackPositionTicks", "Played")
               VALUES (?1, ?2, ?1, 0, 0, ?3, 0)"#,
        )
        .bind(guid_to_db(item))
        .bind(user_id)
        .bind(ticks)
        .execute(db.writer())
        .await
        .expect("set playback position");
    }

    /// Stamps a library's `CollectionType` into its `Data` blob, where both
    /// Jellyfin and the grouping arm read it from.
    async fn set_collection_type(db: &Database, id: Uuid, collection_type: &str) {
        sqlx::query(r#"UPDATE "BaseItems" SET "Data" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .bind(format!(r#"{{"CollectionType":"{collection_type}"}}"#))
            .execute(db.writer())
            .await
            .expect("set collection type");
    }

    /// Stamps a row's `ProductionYear`, the column the year facet reads.
    async fn set_production_year(db: &Database, id: Uuid, year: i64) {
        let mut row = crate::test_support::fetch_item(db, id).await;
        row.production_year = Some(year);
        crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone())
            .save_items(std::slice::from_ref(&row))
            .await
            .expect("set production year");
    }

    /// These tests exercise what the QUERY does with a key, so the key is
    /// written straight to the column — the writer recomputes its own (see
    /// `kinds::presentation_unique_key`) and would overwrite the shape being
    /// probed.
    async fn set_presentation_key(db: &Database, id: Uuid, key: &str) {
        crate::item_persistence_service::seed_presentation_key(db, id, key).await;
    }

    /// What `/Items/Counts` reports for a filter.
    async fn count_over(db: &Database, filter: &InternalItemsQuery) -> i32 {
        use ferrofin_traits::persistence::ItemCountService;
        crate::item_count_service::FerrofinItemCountService::new(db.clone())
            .get_count(filter)
            .await
            .expect("count")
    }

    /// The `PhysicalFolderIds` a Jellyfin `CollectionFolder` carries in its
    /// `Data` blob: N-format guids (32 lowercase hex, no hyphens), where the
    /// `Id` column is uppercase and hyphenated.
    fn collection_folder_data(physical: Uuid) -> String {
        format!(
            r#"{{"PhysicalLocationsList":["/media/tv"],"PhysicalFolderIds":["{}"]}}"#,
            physical.simple()
        )
    }

    /// Seeds two libraries with one movie each, and a user who can see both.
    async fn two_libraries(db: &Database) -> (Uuid, Uuid, Uuid, UserEntity) {
        let first = Uuid::from_u128(0x9801);
        let second = Uuid::from_u128(0x9802);
        seed_named_item(db, first, BaseItemKind::CollectionFolder, "First").await;
        seed_named_item(db, second, BaseItemKind::CollectionFolder, "Second").await;
        seed_top_parented_item(
            db,
            Uuid::from_u128(0x9803),
            BaseItemKind::Movie,
            "In First",
            first,
        )
        .await;
        seed_top_parented_item(
            db,
            Uuid::from_u128(0x9804),
            BaseItemKind::Movie,
            "In Second",
            second,
        )
        .await;
        let user = seed_user_with_defaults(db, Uuid::from_u128(0x9805)).await;
        (first, second, Uuid::from_u128(0x9805), user)
    }

    /// The names an unscoped browse returns for `user`.
    async fn unscoped_names(repository: &FerrofinItemRepository, user: &UserEntity) -> Vec<String> {
        let mut names: Vec<String> = repository
            .get_item_list(&InternalItemsQuery {
                user: Some(user.clone()),
                recursive: true,
                include_item_types: vec![BaseItemKind::Movie],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("browse")
            .into_iter()
            .filter_map(|r| r.name)
            .collect();
        names.sort();
        names
    }

    /// Sets one of the user's guid-list preferences, through the production
    /// writer so the delimiter is never guessed.
    async fn set_folder_preference(
        db: &Database,
        user_id: Uuid,
        kind: PreferenceKind,
        ids: &[Uuid],
    ) {
        let values: Vec<String> = ids.iter().map(ToString::to_string).collect();
        crate::user_entity_ext::set_preference(db.pool(), &guid_to_db(user_id), kind, &values)
            .await
            .expect("set preference");
    }

    /// `EnabledFolders` narrows the browse to the libraries it names.
    ///
    /// The scoping only ever *removes* rows, so a test that asserts presence
    /// passes with it deleted. These assert the absence.
    #[tokio::test]
    async fn a_user_restricted_to_one_library_does_not_see_the_other() {
        let db = test_db().await;
        let repository = repo(&db);
        let (first, _second, user_id, user) = two_libraries(&db).await;
        assert_eq!(
            unscoped_names(&repository, &user).await,
            ["In First", "In Second"]
        );

        // Drop `EnableAllFolders` and name only the first library.
        sqlx::query(r#"UPDATE "Permissions" SET "Value" = 0 WHERE "UserId" = ?1 AND "Kind" = ?2"#)
            .bind(guid_to_db(user_id))
            .bind(i32::from(PermissionKind::EnableAllFolders))
            .execute(db.writer())
            .await
            .expect("revoke");
        set_folder_preference(&db, user_id, PreferenceKind::EnabledFolders, &[first]).await;

        assert_eq!(unscoped_names(&repository, &user).await, ["In First"]);
    }

    /// A Live TV view left behind in an adopted database is not part of the
    /// scope while no tuner is configured — Jellyfin drops it from
    /// `GetUserViews`, so the items under it are outside what an unscoped
    /// query sees. Measured on a real 10.11.8: including it put the
    /// unscoped item count one over upstream's.
    #[tokio::test]
    async fn a_tuner_less_live_tv_view_is_left_out_of_the_scope() {
        let db = test_db().await;
        let repository = repo(&db);
        let (_first, _second, _user_id, user) = two_libraries(&db).await;
        let live_tv = Uuid::from_u128(0x9806);
        // Named in the server's language (`HeaderLiveTV`) and identified by its
        // path — see `user_view_manager::LIVE_TV_VIEW_PATH_SUFFIX`.
        seed_named_item(&db, live_tv, BaseItemKind::UserView, "TV en direct").await;
        crate::test_support::set_item_path(
            &db,
            live_tv,
            &format!(
                "/meta/views{}",
                crate::user_view_manager::LIVE_TV_VIEW_PATH_SUFFIX
            ),
        )
        .await;
        seed_top_parented_item(
            &db,
            Uuid::from_u128(0x9807),
            BaseItemKind::Movie,
            "On Live TV",
            live_tv,
        )
        .await;

        assert_eq!(
            unscoped_names(&repository, &user).await,
            ["In First", "In Second"],
            "no tuner configured: the Live TV view is not a scope"
        );

        db.upsert_live_tv_tuner_host("t1", "http://tuner", "m3u", "{}")
            .await
            .expect("tuner");

        assert_eq!(
            unscoped_names(&repository, &user).await,
            ["In First", "In Second", "On Live TV"],
            "with a tuner, Live TV is a view like any other"
        );
    }

    /// A database adopted from a Windows Jellyfin stores the view path with
    /// backslashes, and the gate has to find it there too — otherwise it
    /// silently does nothing on one of the two platforms it exists for.
    #[tokio::test]
    async fn the_live_tv_view_is_found_under_a_windows_path() {
        let db = test_db().await;
        let repository = repo(&db);
        let (_first, _second, _user_id, user) = two_libraries(&db).await;
        let live_tv = Uuid::from_u128(0x9808);
        seed_named_item(&db, live_tv, BaseItemKind::UserView, "Live TV").await;
        crate::test_support::set_item_path(
            &db,
            live_tv,
            r"C:\ProgramData\Jellyfin\metadata\views\livetv",
        )
        .await;
        seed_top_parented_item(
            &db,
            Uuid::from_u128(0x9809),
            BaseItemKind::Movie,
            "On Live TV",
            live_tv,
        )
        .await;

        assert_eq!(
            unscoped_names(&repository, &user).await,
            ["In First", "In Second"],
            "the windows-pathed Live TV view is out of scope too"
        );
    }

    /// The by-name tabs and the counts are confined to the user's libraries,
    /// exactly as a browse is.
    ///
    /// Without it a restricted account was offered a genre from a library it
    /// cannot see — and `/Items` would then return nothing for it, so the
    /// filter list advertised something that did not exist. `/Items/Counts` had
    /// the same shape of bug: whole-server totals handed to someone who can
    /// browse a fraction of them.
    #[tokio::test]
    async fn the_by_name_tabs_and_counts_see_only_the_user_s_libraries() {
        let db = test_db().await;
        let repository = repo(&db);
        let (first, _second, user_id, user) = two_libraries(&db).await;
        seed_item_genre(&db, Uuid::from_u128(0x9803), "Adventure").await;
        seed_item_genre(&db, Uuid::from_u128(0x9804), "Mystery").await;

        let query = InternalItemsQuery {
            user: Some(user.clone()),
            ..InternalItemsQuery::default()
        };
        let names = |result: QueryResult<ItemWithCounts>| {
            let mut names: Vec<String> = result
                .items
                .iter()
                .filter_map(|i| i.item.name.clone())
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            names(repository.get_genres(&query).await.expect("genres")),
            ["Adventure", "Mystery"],
            "both libraries are visible to start with"
        );
        let counts = crate::item_count_service::FerrofinItemCountService::new(db.clone());
        {
            use ferrofin_traits::persistence::ItemCountService;
            assert_eq!(
                counts
                    .get_item_counts(&query)
                    .await
                    .expect("counts")
                    .movie_count,
                2
            );
        }

        // Restrict the user to the first library.
        sqlx::query(r#"UPDATE "Permissions" SET "Value" = 0 WHERE "UserId" = ?1 AND "Kind" = ?2"#)
            .bind(guid_to_db(user_id))
            .bind(i32::from(PermissionKind::EnableAllFolders))
            .execute(db.writer())
            .await
            .expect("revoke");
        set_folder_preference(&db, user_id, PreferenceKind::EnabledFolders, &[first]).await;

        assert_eq!(
            names(repository.get_genres(&query).await.expect("genres")),
            ["Adventure"],
            "the genre carried only by the hidden library is gone"
        );
        {
            use ferrofin_traits::persistence::ItemCountService;
            assert_eq!(
                counts
                    .get_item_counts(&query)
                    .await
                    .expect("counts")
                    .movie_count,
                1,
                "and the counts agree with what the user can browse"
            );
        }
    }

    /// `/Items/Filters` and `/Years` are confined the same way.
    ///
    /// These are the lists a client renders as the filter dialog, so an
    /// unscoped facet is a choice the user can pick that then returns nothing.
    #[tokio::test]
    async fn the_filter_facets_see_only_the_user_s_libraries() {
        let db = test_db().await;
        let repository = repo(&db);
        let (first, _second, user_id, user) = two_libraries(&db).await;
        seed_item_genre(&db, Uuid::from_u128(0x9803), "Adventure").await;
        seed_item_genre(&db, Uuid::from_u128(0x9804), "Mystery").await;
        set_production_year(&db, Uuid::from_u128(0x9803), 1999).await;
        set_production_year(&db, Uuid::from_u128(0x9804), 2018).await;

        let query = InternalItemsQuery {
            user: Some(user.clone()),
            ..InternalItemsQuery::default()
        };
        let filters = repository
            .get_query_filters_legacy(&query)
            .await
            .expect("filters");
        assert_eq!(filters.genres, ["Adventure", "Mystery"]);
        assert_eq!(
            repository.get_distinct_years(&query).await.expect("years"),
            [1999, 2018]
        );

        sqlx::query(r#"UPDATE "Permissions" SET "Value" = 0 WHERE "UserId" = ?1 AND "Kind" = ?2"#)
            .bind(guid_to_db(user_id))
            .bind(i32::from(PermissionKind::EnableAllFolders))
            .execute(db.writer())
            .await
            .expect("revoke");
        set_folder_preference(&db, user_id, PreferenceKind::EnabledFolders, &[first]).await;

        let filters = repository
            .get_query_filters_legacy(&query)
            .await
            .expect("filters");
        assert_eq!(
            filters.genres,
            ["Adventure"],
            "the hidden library's genre is no longer offered"
        );
        assert_eq!(
            repository.get_distinct_years(&query).await.expect("years"),
            [1999],
            "nor its year"
        );
    }

    /// A `UserView` resolves through its `DisplayParentId` — C#
    /// `GetTopParentIdsForQuery`'s second arm.
    ///
    /// This is the shape a real 10.11.8 database ships: its Playlists view
    /// carries no `ParentId` and no children of its own, only a
    /// `DisplayParentId` pointing at the actual `PlaylistsFolder`. Without the
    /// hop, browsing that view finds nothing.
    #[tokio::test]
    async fn a_user_view_resolves_through_its_display_parent() {
        let db = test_db().await;
        let repository = repo(&db);
        let view = Uuid::from_u128(0x9E01);
        let folder = Uuid::from_u128(0x9E02);
        let playlist = Uuid::from_u128(0x9E03);

        seed_item_with_data(
            &db,
            view,
            BaseItemKind::UserView,
            "Playlists",
            &format!(
                r#"{{"ViewType":"playlists","DisplayParentId":"{}"}}"#,
                folder.simple()
            ),
        )
        .await;
        seed_named_item(&db, folder, BaseItemKind::PlaylistsFolder, "Playlists").await;
        seed_top_parented_item(&db, playlist, BaseItemKind::Playlist, "Road Trip", folder).await;

        let found = repository
            .get_item_list(&InternalItemsQuery {
                parent_id: view,
                recursive: true,
                ..InternalItemsQuery::default()
            })
            .await
            .expect("browse");
        assert!(
            found.iter().any(|r| r.name.as_deref() == Some("Road Trip")),
            "the view's display parent is the scope, got {:?}",
            found
                .iter()
                .filter_map(|r| r.name.as_deref())
                .collect::<Vec<_>>()
        );
    }

    /// A Live TV view stands for itself (the first arm), and a view that
    /// resolves to nothing matches nothing rather than widening to the whole
    /// server — upstream substitutes a fresh guid for exactly that reason.
    #[tokio::test]
    async fn a_live_tv_view_is_its_own_scope_and_a_dangling_one_matches_nothing() {
        let db = test_db().await;
        let repository = repo(&db);
        let live_tv = Uuid::from_u128(0x9F01);
        let programme = Uuid::from_u128(0x9F02);
        let dangling = Uuid::from_u128(0x9F03);
        let elsewhere = Uuid::from_u128(0x9F04);

        seed_item_with_data(
            &db,
            live_tv,
            BaseItemKind::UserView,
            "Live TV",
            r#"{"ViewType":"livetv"}"#,
        )
        .await;
        seed_top_parented_item(&db, programme, BaseItemKind::Movie, "On Live TV", live_tv).await;
        // A view whose display parent is not there at all.
        seed_item_with_data(
            &db,
            dangling,
            BaseItemKind::UserView,
            "Ghost",
            r#"{"ViewType":"movies","DisplayParentId":"ffffffffffffffffffffffffffffffff"}"#,
        )
        .await;
        seed_named_item(&db, elsewhere, BaseItemKind::Movie, "Somewhere Else").await;

        let browse = |parent: Uuid| {
            let repository = &repository;
            async move {
                repository
                    .get_item_list(&InternalItemsQuery {
                        parent_id: parent,
                        recursive: true,
                        ..InternalItemsQuery::default()
                    })
                    .await
                    .expect("browse")
                    .iter()
                    .filter_map(|r| r.name.clone())
                    .collect::<Vec<String>>()
            }
        };
        assert_eq!(browse(live_tv).await, ["On Live TV"]);
        assert!(
            browse(dangling).await.is_empty(),
            "a dangling view matches nothing, not everything"
        );
    }

    /// A view the user has GROUPED libraries into stands for those libraries —
    /// C# `GetTopParentIdsForQuery`'s fifth arm.
    ///
    /// Only reachable for a user whose `GroupedFolders` preference is
    /// non-empty, which is why neither the default install nor the adoption
    /// oracle can see it: the view had been resolving to nothing, so browsing a
    /// combined view returned an empty library instead of the union.
    #[tokio::test]
    async fn a_grouping_view_stands_for_the_libraries_grouped_into_it() {
        let db = test_db().await;
        let repository = repo(&db);
        let (first, second, user_id, user) = two_libraries(&db).await;
        set_collection_type(&db, first, "movies").await;
        set_collection_type(&db, second, "movies").await;

        // A `movies` view with no pointer of its own — the shape the arm is for.
        let view = Uuid::from_u128(0x9F81);
        seed_item_with_data(
            &db,
            view,
            BaseItemKind::UserView,
            "Movies",
            r#"{"ViewType":"movies"}"#,
        )
        .await;

        let browse = |parent: Uuid, user: UserEntity| {
            let repository = &repository;
            async move {
                let mut names: Vec<String> = repository
                    .get_item_list(&InternalItemsQuery {
                        parent_id: parent,
                        recursive: true,
                        user: Some(user),
                        include_item_types: vec![BaseItemKind::Movie],
                        ..InternalItemsQuery::default()
                    })
                    .await
                    .expect("browse")
                    .into_iter()
                    .filter_map(|r| r.name)
                    .collect();
                names.sort();
                names
            }
        };

        // With no `GroupedFolders` preference the view groups nothing, so it
        // resolves to nothing — which is upstream's answer too.
        assert!(
            browse(view, user.clone()).await.is_empty(),
            "an ungrouped view stands for no library"
        );

        // Group only the first library into it.
        set_folder_preference(&db, user_id, PreferenceKind::GroupedFolders, &[first]).await;
        assert_eq!(
            browse(view, user.clone()).await,
            ["In First"],
            "the view now stands for the library grouped into it"
        );

        // Group both.
        set_folder_preference(
            &db,
            user_id,
            PreferenceKind::GroupedFolders,
            &[first, second],
        )
        .await;
        assert_eq!(
            browse(view, user.clone()).await,
            ["In First", "In Second"],
            "…and for both once both are grouped"
        );

        // A library of a DIFFERENT type is not pulled into a movies view.
        set_collection_type(&db, second, "music").await;
        assert_eq!(
            browse(view, user).await,
            ["In First"],
            "a music library does not group into a movies view"
        );
    }

    /// `BlockedMediaFolders` hides exactly the libraries it names.
    #[tokio::test]
    async fn a_blocked_library_is_hidden_even_from_a_user_who_may_see_everything() {
        let db = test_db().await;
        let repository = repo(&db);
        let (first, _second, user_id, user) = two_libraries(&db).await;
        set_folder_preference(&db, user_id, PreferenceKind::BlockedMediaFolders, &[first]).await;

        assert_eq!(
            unscoped_names(&repository, &user).await,
            ["In Second"],
            "the blocked library is gone, the other stays"
        );
    }

    /// A user who can see no library sees nothing — not everything.
    ///
    /// C# guards the empty scope with a fresh guid precisely so the query
    /// matches nothing; without it, "no libraries" would read as "no filter".
    #[tokio::test]
    async fn a_user_with_no_libraries_sees_nothing_rather_than_everything() {
        let db = test_db().await;
        let repository = repo(&db);
        let (first, second, user_id, user) = two_libraries(&db).await;
        set_folder_preference(
            &db,
            user_id,
            PreferenceKind::BlockedMediaFolders,
            &[first, second],
        )
        .await;

        assert!(unscoped_names(&repository, &user).await.is_empty());
    }

    /// Browsing a library on an adopted Jellyfin database.
    ///
    /// Nothing there carries a `CollectionFolder`'s id as `ParentId` — measured
    /// on a real 10.11.8 library, 0 rows of 40,610 — so the view has to be
    /// translated into the physical folders named in its `Data` blob or the
    /// browse comes back empty. This is the shape the Ferrofin-native fixtures
    /// cannot produce, which is why the bug was invisible to a green suite.
    #[tokio::test]
    async fn browsing_a_jellyfin_collection_folder_finds_the_physical_folder_s_children() {
        let db = test_db().await;
        let repository = repo(&db);
        let view = Uuid::from_u128(0x9001);
        let physical = Uuid::from_u128(0x9002);
        let episode = Uuid::from_u128(0x9003);

        seed_item_with_data(
            &db,
            view,
            BaseItemKind::CollectionFolder,
            "TV",
            &collection_folder_data(physical),
        )
        .await;
        seed_named_item(&db, physical, BaseItemKind::Folder, "TV").await;
        seed_child_item(&db, episode, BaseItemKind::Episode, "Pilot", physical).await;

        let filter = InternalItemsQuery {
            parent_id: view,
            ..InternalItemsQuery::default()
        };
        let found = repository.get_item_list(&filter).await.expect("children");
        assert_eq!(found.len(), 1, "the view's children hang off its folder");
        assert_eq!(found[0].id, guid_to_db(episode));
    }

    /// A *recursive* browse of a library is scoped by its physical folders'
    /// `TopParentId`, not by the ancestor closure — C#
    /// `SetTopParentIdsOrAncestors`. The two differ by exactly the folder row
    /// itself, which the closure misses: measured against a live 10.11.8 on the
    /// same 40,610-item database, 6,988 rows by top parent vs 6,987 by closure.
    #[tokio::test]
    async fn a_recursive_browse_of_a_library_is_scoped_by_its_physical_folders() {
        let db = test_db().await;
        let repository = repo(&db);
        let view = Uuid::from_u128(0x9A01);
        let physical = Uuid::from_u128(0x9A02);
        let episode = Uuid::from_u128(0x9A03);

        seed_item_with_data(
            &db,
            view,
            BaseItemKind::CollectionFolder,
            "TV",
            &collection_folder_data(physical),
        )
        .await;
        seed_named_item(&db, physical, BaseItemKind::Folder, "TV").await;
        // Top-parented but NOT in the view's ancestor closure — only the
        // translated scope reaches it.
        seed_top_parented_item(&db, episode, BaseItemKind::Episode, "Pilot", physical).await;

        let found = repository
            .get_item_list(&InternalItemsQuery {
                parent_id: view,
                recursive: true,
                ..InternalItemsQuery::default()
            })
            .await
            .expect("recursive browse");
        let names: Vec<&str> = found.iter().filter_map(|r| r.name.as_deref()).collect();
        assert!(
            names.contains(&"Pilot"),
            "the library's physical folder is the scope, got {names:?}"
        );
    }

    /// The delete cascade asks for a parent's physical children, recursively —
    /// and must NOT be widened to the library's whole top-parent scope, or
    /// "delete this folder's contents" becomes "delete the library".
    #[tokio::test]
    async fn a_physical_children_query_is_never_widened_to_the_library() {
        let db = test_db().await;
        let repository = repo(&db);
        let view = Uuid::from_u128(0x9B01);
        let physical = Uuid::from_u128(0x9B02);
        let elsewhere = Uuid::from_u128(0x9B03);

        seed_item_with_data(
            &db,
            view,
            BaseItemKind::CollectionFolder,
            "TV",
            &collection_folder_data(physical),
        )
        .await;
        seed_named_item(&db, physical, BaseItemKind::Folder, "TV").await;
        seed_top_parented_item(&db, elsewhere, BaseItemKind::Episode, "Pilot", physical).await;

        let found = repository
            .get_item_list(&InternalItemsQuery {
                parent_id: view,
                recursive: true,
                physical_children_only: true,
                ..InternalItemsQuery::default()
            })
            .await
            .expect("physical children");
        assert!(
            found.is_empty(),
            "nothing is a physical child of the collection folder itself, got {:?}",
            found
                .iter()
                .filter_map(|r| r.name.as_deref())
                .collect::<Vec<_>>()
        );
    }

    /// Two versions of one film share a `PresentationUniqueKey`, so a browse
    /// lists the title once — C# `ApplyGroupingFilter` — and the row it lists
    /// is the *primary* version, not whichever cut sorts first.
    ///
    /// The whole point of the merge is that the alternate stops showing up on
    /// its own, so this drives the real `merge_versions` end to end: the
    /// grouping replaced a blunt `PrimaryVersionId IS NULL` predicate that used
    /// to hide alternates, and if the two rows do not actually land in one
    /// group, merging silently stops working.
    #[tokio::test]
    async fn merged_versions_collapse_to_the_primary_row() {
        let db = test_db().await;
        let repository = repo(&db);
        let library = Uuid::from_u128(0x9C01);
        let cut_a = Uuid::from_u128(0x9C02);
        let cut_b = Uuid::from_u128(0x9C03);
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "Movies").await;
        seed_top_parented_item(&db, cut_a, BaseItemKind::Movie, "Blade Runner", library).await;
        seed_top_parented_item(
            &db,
            cut_b,
            BaseItemKind::Movie,
            "Blade Runner (Director's Cut)",
            library,
        )
        .await;
        // `seed_*` writes a deliberately minimal row straight to the table, so
        // the two go through the real writer first — that is what stamps the
        // presentation key (`kinds::presentation_unique_key`), and a scanned
        // library has been through it.
        let persistence =
            crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone());
        for id in [cut_a, cut_b] {
            let row = crate::test_support::fetch_item(&db, id).await;
            persistence
                .save_items(std::slice::from_ref(&row))
                .await
                .expect("write through the production writer");
        }
        // Merged through `LibraryManager::merge_versions` itself, not through
        // the writer it happens to call: on a FIRST merge that method touches
        // only the alternates (the primary's `PrimaryVersionId` is already
        // null, so it is skipped), and a test that stamps the primary by hand
        // would hide exactly the case where the two rows fail to meet.
        merge_versions_over(&db, &[cut_a, cut_b]).await;
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0x9C05)).await;

        let filter = InternalItemsQuery {
            user: Some(user.clone()),
            recursive: true,
            include_item_types: vec![BaseItemKind::Movie],
            ..InternalItemsQuery::default()
        };
        // Which of the two `merge_versions` elected is its business (widest
        // video stream, then id order), so the expectation is read back rather
        // than assumed — what matters is that the browse lists exactly that
        // row.
        let mut elected_name = None;
        for id in [cut_a, cut_b] {
            let row = crate::test_support::fetch_item(&db, id).await;
            if row.primary_version_id.is_none() {
                elected_name = row.name;
            }
        }
        let elected_name = elected_name.expect("one of the two is the primary");

        let found = repository.get_item_list(&filter).await.expect("browse");
        assert_eq!(
            found
                .iter()
                .filter_map(|r| r.name.as_deref())
                .collect::<Vec<_>>(),
            [elected_name.as_str()],
            "one row per title, and it is the primary version"
        );
        // The total that labels a *page* counts the same grouped rows
        // (`GetItems` counts `dbQuery` after `ApplyGroupingFilter`).
        let paged = repository
            .get_items(&InternalItemsQuery {
                limit: Some(10),
                enable_total_record_count: true,
                ..filter.clone()
            })
            .await
            .expect("page");
        assert_eq!(paged.total_record_count, 1);
    }

    /// Resume surfaces the version that was actually PLAYED, not the primary.
    ///
    /// Deliberate divergence from 10.11.8, which has this bug: the resume
    /// predicate keeps alternate versions on purpose, and then the presentation
    /// grouping collapsed the surfaced one back onto the primary — whose user
    /// data has no playback position, so the row came back with no progress on
    /// it. Upstream's released `EnableGroupByPresentationUniqueKey` now returns
    /// early for a resumable query, with the same reasoning.
    #[tokio::test]
    async fn resume_returns_the_version_that_was_played() {
        let db = test_db().await;
        let repository = repo(&db);
        let library = Uuid::from_u128(0x9C41);
        let primary = Uuid::from_u128(0x9C42);
        let alternate = Uuid::from_u128(0x9C43);
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "Movies").await;
        seed_top_parented_item(&db, primary, BaseItemKind::Movie, "Blade Runner", library).await;
        seed_top_parented_item(
            &db,
            alternate,
            BaseItemKind::Movie,
            "Blade Runner 4K",
            library,
        )
        .await;
        let persistence =
            crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone());
        for id in [primary, alternate] {
            let row = crate::test_support::fetch_item(&db, id).await;
            persistence
                .save_items(std::slice::from_ref(&row))
                .await
                .expect("write");
        }
        merge_versions_over(&db, &[primary, alternate]).await;

        // The user watched half of the ALTERNATE — whichever row the merge left
        // pointing at the other.
        let mut alternate_id = None;
        for id in [primary, alternate] {
            if crate::test_support::fetch_item(&db, id)
                .await
                .primary_version_id
                .is_some()
            {
                alternate_id = Some(id);
            }
        }
        let alternate_id = alternate_id.expect("one of the two is the alternate");

        // BOTH carry progress. That is what makes this discriminating: with one
        // resumable row the grouping has nothing to collapse and the bug is
        // invisible, which is exactly how it survived.
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0x9C45)).await;
        for id in [primary, alternate] {
            set_playback_position(&db, id, &user.id, 42_000_000).await;
        }

        let resumed = repository
            .get_item_list(&InternalItemsQuery {
                user: Some(user),
                recursive: true,
                is_resumable: Some(true),
                ..InternalItemsQuery::default()
            })
            .await
            .expect("resume");
        let ids: Vec<String> = resumed.iter().map(|r| r.id.clone()).collect();
        assert!(
            ids.contains(&guid_to_db(alternate_id)),
            "the played alternate must be surfaced, not collapsed onto the primary (got {ids:?})"
        );
        assert_eq!(
            ids.len(),
            2,
            "and both versions with progress are resumable"
        );
    }

    /// The grouping *gate* — C# `EnableGroupByPresentationUniqueKey`, which
    /// needs a user and either no kind filter or one of the six kinds that can
    /// have versions.
    ///
    /// Probed with two rows that merely SHARE a key and have no
    /// `PrimaryVersionId`: an alternate version would be dropped by the
    /// ungrouped path's own predicate, so it could not tell the two states
    /// apart.
    #[tokio::test]
    async fn grouping_needs_a_user_and_a_versionable_kind() {
        let db = test_db().await;
        let repository = repo(&db);
        let library = Uuid::from_u128(0x9D01);
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "Movies").await;
        // Two rows per kind, each pair sharing one key. `Audio` is outside the
        // six kinds that can have versions, so it is the gate's negative side.
        for (i, kind) in [BaseItemKind::Movie, BaseItemKind::Audio]
            .into_iter()
            .enumerate()
        {
            for j in 0..2u128 {
                let id = Uuid::from_u128(0x9D02 + (i as u128) * 8 + j);
                seed_top_parented_item(&db, id, kind, &format!("{kind:?} {j}"), library).await;
                set_presentation_key(&db, id, &format!("shared-{i}")).await;
            }
        }
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0x9D05)).await;
        let grouped = InternalItemsQuery {
            user: Some(user),
            recursive: true,
            include_item_types: vec![BaseItemKind::Movie],
            ..InternalItemsQuery::default()
        };

        assert_eq!(
            repository
                .get_item_list(&grouped)
                .await
                .expect("browse")
                .len(),
            1,
            "one row per key"
        );
        assert_eq!(
            repository
                .get_item_list(&InternalItemsQuery {
                    user: None,
                    ..grouped.clone()
                })
                .await
                .expect("browse")
                .len(),
            2,
            "without a user nothing is grouped"
        );
        assert_eq!(
            repository
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![BaseItemKind::Audio],
                    ..grouped.clone()
                })
                .await
                .expect("browse")
                .len(),
            2,
            "a kind list with none of the six turns grouping off"
        );
        // …but upstream's rule is `Contains(Episode) || Contains(Video) || …`,
        // so a list that merely INCLUDES one of the six keeps it on.
        assert_eq!(
            repository
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![BaseItemKind::Audio, BaseItemKind::Movie],
                    ..grouped.clone()
                })
                .await
                .expect("browse")
                .len(),
            2,
            "grouping stays on and collapses BOTH pairs to their key"
        );
        // `/Items/Counts` reports ROWS, not titles: C# `GetCount` runs
        // `TranslateQuery` without `ApplyGroupingFilter`.
        assert_eq!(
            count_over(&db, &grouped).await,
            2,
            "the counts endpoint is ungrouped"
        );
    }

    /// The `TopParentId` half — what `Latest` scopes by.
    ///
    /// A Jellyfin row carries the *physical folder* as its `TopParentId`
    /// (measured: 0 of 40,610 rows carry a collection folder), so scoping by
    /// the view finds nothing until it is translated.
    #[tokio::test]
    async fn a_latest_query_scoped_to_a_jellyfin_view_finds_its_folder_s_items() {
        let db = test_db().await;
        let repository = repo(&db);
        let view = Uuid::from_u128(0x9401);
        let physical = Uuid::from_u128(0x9402);
        let episode = Uuid::from_u128(0x9403);

        seed_item_with_data(
            &db,
            view,
            BaseItemKind::CollectionFolder,
            "TV",
            &collection_folder_data(physical),
        )
        .await;
        seed_named_item(&db, physical, BaseItemKind::Folder, "TV").await;
        seed_top_parented_item(&db, episode, BaseItemKind::Episode, "Pilot", physical).await;

        let filter = InternalItemsQuery {
            top_parent_ids: vec![view],
            ..InternalItemsQuery::default()
        };
        let found = repository.get_item_list(&filter).await.expect("latest");
        assert_eq!(found.len(), 1, "the view resolves to its physical folder");
        assert_eq!(found[0].id, guid_to_db(episode));
    }

    /// A scope that is not a collection folder keeps its own id.
    ///
    /// Upstream resolves `TopParentIds` per id; replacing the whole set
    /// whenever one entry expanded would silently drop the others — the Live TV
    /// and Playlists `UserView`s that sit alongside the libraries in the same
    /// query.
    #[tokio::test]
    async fn a_mixed_scope_keeps_the_ids_that_are_not_collection_folders() {
        let db = test_db().await;
        let repository = repo(&db);
        let view = Uuid::from_u128(0x9501);
        let physical = Uuid::from_u128(0x9502);
        let from_library = Uuid::from_u128(0x9503);
        let other_view = Uuid::from_u128(0x9504);
        let from_other = Uuid::from_u128(0x9505);

        seed_item_with_data(
            &db,
            view,
            BaseItemKind::CollectionFolder,
            "TV",
            &collection_folder_data(physical),
        )
        .await;
        seed_named_item(&db, physical, BaseItemKind::Folder, "TV").await;
        seed_top_parented_item(&db, from_library, BaseItemKind::Episode, "Pilot", physical).await;
        // A `UserView` — no `Data`, and its own id is the top parent.
        seed_named_item(&db, other_view, BaseItemKind::UserView, "Live TV").await;
        seed_top_parented_item(&db, from_other, BaseItemKind::TvChannel, "BBC", other_view).await;

        let filter = InternalItemsQuery {
            top_parent_ids: vec![view, other_view],
            ..InternalItemsQuery::default()
        };
        let found = repository
            .get_item_list(&filter)
            .await
            .expect("both scopes");
        let mut ids: Vec<&str> = found.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        let mut want = vec![guid_to_db(from_library), guid_to_db(from_other)];
        want.sort_unstable();
        assert_eq!(ids, want, "the unexpanded scope must survive the expansion");
    }

    /// The translation must not disturb a Ferrofin-written database, where a
    /// collection folder has no `Data` blob and owns its children directly.
    #[tokio::test]
    async fn browsing_a_ferrofin_collection_folder_is_unchanged() {
        let db = test_db().await;
        let repository = repo(&db);
        let view = Uuid::from_u128(0x9101);
        let episode = Uuid::from_u128(0x9102);

        seed_named_item(&db, view, BaseItemKind::CollectionFolder, "TV").await;
        seed_child_item(&db, episode, BaseItemKind::Episode, "Pilot", view).await;

        let filter = InternalItemsQuery {
            parent_id: view,
            ..InternalItemsQuery::default()
        };
        let found = repository.get_item_list(&filter).await.expect("children");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, guid_to_db(episode));
    }

    /// Deleting a library must keep meaning "the rows it owns".
    ///
    /// `physical_children_only` is the delete-cascade path; widening it to the
    /// library's physical folders would turn removing a view into removing the
    /// media under it. Two things stop that — the guard in `resolve_views` and
    /// the fact that `translate_query`'s `physical_children_only` branch never
    /// reads `parent_physical_folder_ids` — so this passes even with the guard
    /// removed. It is here to fail if the *branch* ever starts reading the
    /// field, which is the change that would actually be dangerous.
    #[tokio::test]
    async fn the_delete_cascade_query_is_never_widened_to_physical_folders() {
        let db = test_db().await;
        let repository = repo(&db);
        let view = Uuid::from_u128(0x9201);
        let physical = Uuid::from_u128(0x9202);
        let episode = Uuid::from_u128(0x9203);

        seed_item_with_data(
            &db,
            view,
            BaseItemKind::CollectionFolder,
            "TV",
            &collection_folder_data(physical),
        )
        .await;
        seed_named_item(&db, physical, BaseItemKind::Folder, "TV").await;
        seed_child_item(&db, episode, BaseItemKind::Episode, "Pilot", physical).await;

        let filter = InternalItemsQuery {
            parent_id: view,
            physical_children_only: true,
            ..InternalItemsQuery::default()
        };
        assert!(
            repository
                .get_item_list(&filter)
                .await
                .expect("owned rows")
                .is_empty(),
            "the episode belongs to the physical folder, not the view"
        );
    }

    /// Genres on an adopted Jellyfin database.
    ///
    /// `ItemValues.ItemValueId` there is a synthetic guid unrelated to any item
    /// — 0 of 3,279 matched a `BaseItems.Id` on the real library — so the
    /// by-name row is found by name, as C# `GetItemValues` does
    /// (`itemValuesQuery.Contains(e.CleanName)`).
    #[tokio::test]
    async fn genres_resolve_by_name_when_the_by_name_row_is_not_keyed_by_item_value_id() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0x9301);
        // An id that is emphatically not the ItemValueId the value gets.
        let genre_row = Uuid::from_u128(0x9302);

        seed_named_item(&db, movie, BaseItemKind::Movie, "A Drama Film").await;
        seed_item_genre(&db, movie, "Drama").await;
        // Replace the scanner's by-name row with one keyed the Jellyfin way:
        // an unrelated id, found only through `CleanName`.
        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Type" LIKE '%Genre'"#)
            .execute(db.writer())
            .await
            .expect("drop the ferrofin-keyed genre row");
        seed_named_item(&db, genre_row, BaseItemKind::Genre, "Drama").await;
        set_clean_name(&db, genre_row, "Drama").await;

        let genres = repository
            .get_genres(&InternalItemsQuery::default())
            .await
            .expect("genres");
        assert_eq!(genres.items.len(), 1, "the genre is found by name");
        assert_eq!(genres.items[0].item.id, guid_to_db(genre_row));
        assert_eq!(genres.items[0].counts.item_count, 1);
    }

    /// The representative collapse: two by-name rows that share a
    /// `PresentationUniqueKey` are ONE row on the wire, and the survivor is the
    /// lowest id.
    ///
    /// Ported from `GetItemValues`'s `GroupBy(e => e.PresentationUniqueKey)`,
    /// which BOTH trees perform (master `BaseItemRepository.ByName.cs:213-224`
    /// = `g.Min(e => e.Id)`; v10.11.8 `BaseItemRepository.cs:1313-1316` =
    /// `g.FirstOrDefault()`). Ferrofin listed both rows, so a merged film's
    /// alternate and a multi-library artist folder each showed up twice.
    #[tokio::test]
    async fn by_name_rows_sharing_a_presentation_key_collapse_to_the_lowest_id() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0x9A01);
        let lo = Uuid::from_u128(0x9A02);
        let hi = Uuid::from_u128(0x9A03);

        seed_named_item(&db, movie, BaseItemKind::Movie, "A Drama Film").await;
        seed_item_genre(&db, movie, "Drama").await;
        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Type" LIKE '%Genre'"#)
            .execute(db.writer())
            .await
            .expect("drop the scanner row");
        for id in [lo, hi] {
            seed_named_item(&db, id, BaseItemKind::Genre, "Drama").await;
            set_clean_name(&db, id, "Drama").await;
            sqlx::query(
                r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = 'Genre-Drama'
                       WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .expect("share the key");
        }

        let q = InternalItemsQuery {
            enable_total_record_count: true,
            limit: Some(50),
            ..InternalItemsQuery::default()
        };
        let genres = repository.get_genres(&q).await.expect("genres");
        assert_eq!(genres.items.len(), 1, "the two rows are one group");
        assert_eq!(
            genres.items[0].item.id,
            guid_to_db(lo),
            "Min(Id) is the representative"
        );
        assert_eq!(
            genres.total_record_count, 1,
            "the total counts GROUPS, not rows — otherwise paging promises rows it cannot serve"
        );
    }

    /// …and rows with NO key collapse together, because that is what SQLite's
    /// `GROUP BY` and EF's `GroupBy` both do with NULL — the very behaviour
    /// that makes a live Jellyfin list four music genres where its database
    /// holds eight.
    ///
    /// Reproducing it faithfully is only half the job: Ferrofin must also write
    /// the keys upstream writes, or it reproduces the collapse WITHOUT the keys
    /// that save most rows from it. That is
    /// `backfill_missing_presentation_keys`, and this test is the reason it is
    /// not optional.
    #[tokio::test]
    async fn unkeyed_by_name_rows_collapse_the_way_upstream_collapses_them() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0x9B01);
        let other = Uuid::from_u128(0x9B02);
        let drama = Uuid::from_u128(0x9B03);
        let comedy = Uuid::from_u128(0x9B04);

        seed_named_item(&db, movie, BaseItemKind::Movie, "A Drama Film").await;
        seed_item_genre(&db, movie, "Drama").await;
        seed_named_item(&db, other, BaseItemKind::Movie, "A Comedy Film").await;
        seed_item_genre(&db, other, "Comedy").await;
        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Type" LIKE '%Genre'"#)
            .execute(db.writer())
            .await
            .expect("drop the scanner rows");
        for (id, name) in [(drama, "Drama"), (comedy, "Comedy")] {
            seed_named_item(&db, id, BaseItemKind::Genre, name).await;
            set_clean_name(&db, id, name).await;
            sqlx::query(r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = NULL WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .execute(db.writer())
                .await
                .expect("unkey");
        }

        let genres = repository
            .get_genres(&InternalItemsQuery::default())
            .await
            .expect("genres");
        assert_eq!(
            genres.items.len(),
            1,
            "two DIFFERENT unkeyed genres are one NULL group — upstream's own behaviour"
        );

        // …and once the keys are repaired, both names come back.
        assert_eq!(
            crate::item_persistence_service::backfill_missing_presentation_keys(&db)
                .await
                .expect("backfill"),
            2
        );
        let genres = repository
            .get_genres(&InternalItemsQuery::default())
            .await
            .expect("genres");
        assert_eq!(genres.items.len(), 2, "keyed rows are their own groups");
    }

    /// Matching by name alone is not enough — the row's *kind* is the other
    /// half of the C# filter.
    ///
    /// Without it, an ordinary item that happens to be named after a genre is
    /// returned as one. On the real adopted library that turned 25 genres into
    /// 35: four episodes, a folder and a collection folder joined the list.
    #[tokio::test]
    async fn an_item_merely_named_after_a_genre_is_not_a_genre() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0x9601);
        let impostor = Uuid::from_u128(0x9602);

        seed_named_item(&db, movie, BaseItemKind::Movie, "Some Film").await;
        seed_item_genre(&db, movie, "Drama").await;
        // A movie literally called "Drama": same `CleanName` as the genre.
        seed_named_item(&db, impostor, BaseItemKind::Movie, "Drama").await;
        set_clean_name(&db, impostor, "Drama").await;

        let genres = repository
            .get_genres(&InternalItemsQuery::default())
            .await
            .expect("genres");
        assert_eq!(genres.items.len(), 1, "only the Genre row is a genre");
        assert!(
            genres
                .items
                .iter()
                .all(|g| g.item.id != guid_to_db(impostor)),
            "a Movie named after a genre must not be listed as one"
        );
    }

    /// A music library's genres browse, on a Ferrofin-written database.
    ///
    /// `/Genres` and `/MusicGenres` share one `ItemValueType` and are told apart
    /// only by the by-name row's kind, so the scanner has to materialize a
    /// `MusicGenre` row for a music item's genre — otherwise selecting on that
    /// kind (which is what upstream does) leaves the tab empty.
    #[tokio::test]
    async fn a_music_item_s_genre_is_browsable_under_music_genres() {
        let db = test_db().await;
        let repository = repo(&db);
        let song = Uuid::from_u128(0x9701);

        seed_named_item(&db, song, BaseItemKind::Audio, "A Song").await;
        seed_item_genre(&db, song, "Metal").await;

        let music = repository
            .get_music_genres(&InternalItemsQuery::default())
            .await
            .expect("music genres");
        assert_eq!(music.items.len(), 1, "the music genre is browsable");
        assert_eq!(music.items[0].item.name.as_deref(), Some("Metal"));
        assert_eq!(music.items[0].counts.item_count, 1);

        // …and it is a MusicGenre row, not the Genre one borrowed.
        assert_eq!(
            music.items[0].item.type_,
            stored_type_name(BaseItemKind::MusicGenre).expect("kind is known")
        );
        // The plain genres browse stays empty — not because music items are
        // excluded from the aggregate (upstream excludes nothing: both
        // `GetGenres` and `GetMusicGenres` are the same `GetItemValues` call,
        // `BaseItemRepository.ByName.cs:44-52`), but because a music owner
        // materializes ONLY a `MusicGenre` row, so there is no `Genre` row of
        // that name to join.
        assert!(
            repository
                .get_genres(&InternalItemsQuery::default())
                .await
                .expect("genres")
                .items
                .is_empty()
        );
    }

    /// A `MusicGenre` row whose value is carried only by NON-music items still
    /// lists on `/MusicGenres`.
    ///
    /// This is the shape a `GET /MusicGenres/Action` lazy create leaves behind
    /// on a movie library: the `Action` value belongs to a Movie, and
    /// `CreateItemByName<MusicGenre>` mints a `MusicGenre` row for it. Upstream
    /// lists it, because `GetMusicGenres` restricts the *return kind* and never
    /// the contributing items. Ferrofin used to pass the music types as an
    /// `include_content_types` filter, which dropped the row entirely.
    #[tokio::test]
    async fn a_music_genre_row_lists_even_when_only_a_movie_carries_the_value() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0x9801);
        let music_genre_row = Uuid::from_u128(0x9802);

        seed_named_item(&db, movie, BaseItemKind::Movie, "An Action Film").await;
        seed_item_genre(&db, movie, "Action").await;
        // What `CreateItemByName<MusicGenre>("Action")` leaves behind.
        seed_named_item(&db, music_genre_row, BaseItemKind::MusicGenre, "Action").await;
        set_clean_name(&db, music_genre_row, "Action").await;

        let music = repository
            .get_music_genres(&InternalItemsQuery::default())
            .await
            .expect("music genres");
        assert_eq!(
            music.items.len(),
            1,
            "the MusicGenre row lists even though only a Movie carries the value"
        );
        assert_eq!(music.items[0].item.id, guid_to_db(music_genre_row));
        // …and the plain Genre row for the same value still lists on /Genres.
        let genres = repository
            .get_genres(&InternalItemsQuery::default())
            .await
            .expect("genres");
        assert_eq!(genres.items.len(), 1);
        assert_eq!(genres.items[0].item.name.as_deref(), Some("Action"));
    }

    /// `includeItemTypes` fills the per-kind counts PER by-name row, master's
    /// `BuildItemCountsByCleanName` (`BaseItemRepository.ByName.cs:283-338`).
    ///
    /// v10.11.8 instead hands every row the same uncorrelated totals
    /// (`BaseItemRepository.cs:1368-1392`, whose own comment reads "TODO: This
    /// is bad refactor!"), which is why a 10.11.8 oracle reports the same
    /// `SongCount` for every genre. Ferrofin follows master.
    #[tokio::test]
    async fn per_kind_counts_are_computed_per_by_name_row() {
        let db = test_db().await;
        let repository = repo(&db);
        let rock_song = Uuid::from_u128(0x9901);
        let jazz_song = Uuid::from_u128(0x9902);
        let jazz_album = Uuid::from_u128(0x9903);
        let rock_movie = Uuid::from_u128(0x9904);

        seed_named_item(&db, rock_song, BaseItemKind::Audio, "Rock Song").await;
        seed_item_genre(&db, rock_song, "Rock").await;
        seed_named_item(&db, jazz_song, BaseItemKind::Audio, "Jazz Song").await;
        seed_item_genre(&db, jazz_song, "Jazz").await;
        seed_named_item(&db, jazz_album, BaseItemKind::MusicAlbum, "Jazz Album").await;
        seed_item_genre(&db, jazz_album, "Jazz").await;
        // A movie carrying "Rock" — it must NOT be counted as a song.
        seed_named_item(&db, rock_movie, BaseItemKind::Movie, "Rock The Film").await;
        seed_item_genre(&db, rock_movie, "Rock").await;

        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Audio, BaseItemKind::MusicAlbum],
            ..InternalItemsQuery::default()
        };
        let music = repository
            .get_music_genres(&query)
            .await
            .expect("music genres");
        let by_name = |name: &str| {
            music
                .items
                .iter()
                .find(|i| i.item.name.as_deref() == Some(name))
                .unwrap_or_else(|| panic!("{name} listed"))
                .counts
        };
        assert_eq!(by_name("Rock").song_count, 1);
        assert_eq!(by_name("Rock").album_count, 0);
        assert_eq!(by_name("Rock").movie_count, 0, "movies were not requested");
        assert_eq!(by_name("Jazz").song_count, 1);
        assert_eq!(
            by_name("Jazz").album_count,
            1,
            "per-CleanName, not one global total shared by every row"
        );
    }

    /// With no `includeItemTypes` the per-kind counts stay zero, because
    /// `GetItemValues` builds them only inside `if (filter.IncludeItemTypes
    /// .Length > 0)` on both trees — and the API layer only projects them
    /// then too.
    #[tokio::test]
    async fn per_kind_counts_are_not_computed_without_include_item_types() {
        let db = test_db().await;
        let repository = repo(&db);
        let song = Uuid::from_u128(0x9A01);
        seed_named_item(&db, song, BaseItemKind::Audio, "A Song").await;
        seed_item_genre(&db, song, "Rock").await;

        let music = repository
            .get_music_genres(&InternalItemsQuery::default())
            .await
            .expect("music genres");
        assert_eq!(music.items.len(), 1);
        assert_eq!(music.items[0].counts.song_count, 0);
        assert_eq!(
            music.items[0].counts.item_count, 1,
            "the value aggregate itself is unchanged"
        );
    }
}
