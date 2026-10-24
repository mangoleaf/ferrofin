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

use ferrofin_util::string_extensions::{
    equals_ordinal_ignore_case, lower_invariant, upper_invariant,
};

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::{BaseItemEntity, BaseItemImageInfoEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::{ItemValueType, PermissionKind, PreferenceKind};
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_model::dto::ItemCounts;
use ferrofin_model::entities::ImageType;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use ferrofin_traits::options::ItemImageInfo;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{DescendantQueryRoots, InternalItemsQuery};
use ferrofin_traits::persistence::{
    FolderChild, ItemRepository, ItemTypeLookup, ItemWithCounts, PlaylistAccessColumns,
    PlaylistItemsWithAccess,
};

use crate::aggregate_folder::RootFolderIds;
use crate::db_error::{db_err, media_stream_type_disc};
use crate::item_type_lookup::stored_type_name;
use crate::kinds::is_video;
use crate::translate_query::{
    PLACEHOLDER_ID, QueryShape, append_predicates, build_latest_music_albums_query, build_query,
    media_type_name, non_blank, push_in_list, to_guid_strings,
};
use crate::user_entity_ext::{guid_preference, has_permission, live_tv_enabled_for};
use crate::virtual_paths::VirtualPathExpander;
use sqlx::{FromRow, QueryBuilder, Row, Sqlite};

#[derive(sqlx::FromRow)]
struct FolderChildRow {
    #[sqlx(flatten)]
    item: BaseItemEntity,
    #[sqlx(rename = "_FolderLinked")]
    linked: bool,
}

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
    /// Expands the `%MetadataPath%`/`%AppDataPath%` tokens an adopted Jellyfin
    /// database stores in image paths (see
    /// [`FerrofinItemRepository::with_virtual_paths`]).
    virtual_paths: VirtualPathExpander,
    /// The files Live TV is writing right now, bound late by the composition
    /// root (see [`FerrofinItemRepository::with_active_recordings`]).
    active_recordings: ActiveRecordingSlot,
}

/// What the files Live TV is recording right now are, asked when needed.
pub type ActiveRecordingSource = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<String>, ServiceError>> + Send>,
        > + Send
        + Sync,
>;

/// A late-bound [`ActiveRecordingSource`]: the Live TV manager is built after
/// the repository it must not own, so the composition root fills this once.
pub type ActiveRecordingSlot = Arc<std::sync::OnceLock<ActiveRecordingSource>>;

impl std::fmt::Debug for FerrofinItemRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinItemRepository")
            .finish_non_exhaustive()
    }
}

/// Resolves the pinned DescendantQueryHelper root sets once per logical query.
/// Source first-visit roles matter: a physical link owner discovered before a
/// later back-link never becomes a new closure root. Leaves are not loaded here.
pub(crate) async fn resolve_descendant_query(
    db: &Database,
    filter: &InternalItemsQuery,
) -> Result<Option<InternalItemsQuery>, ServiceError> {
    let Some(parent) = filter.descendant_of_id else {
        return Ok(None);
    };
    if filter
        .descendant_roots
        .as_ref()
        .is_some_and(|roots| roots.parent_id == parent)
    {
        return Ok(None);
    }
    let mut roots = DescendantQueryRoots {
        parent_id: parent,
        closure_roots: vec![parent],
        link_roots: vec![parent],
    };
    let mut visited = std::collections::HashSet::from([parent]);
    let mut frontier = vec![parent];
    while !frontier.is_empty() {
        let discovered = discover_linked_roots(db, &frontier).await?;
        frontier.clear();
        for (id, linked_folder) in discovered {
            let id =
                Uuid::parse_str(&id).map_err(|error| ServiceError::backend(error.to_string()))?;
            if visited.insert(id) {
                frontier.push(id);
                roots.link_roots.push(id);
                if linked_folder {
                    roots.closure_roots.push(id);
                }
            }
        }
    }
    let mut resolved = filter.clone();
    resolved.descendant_roots = Some(roots);
    Ok(Some(resolved))
}

/// One batched frontier query; all link kinds qualify, and only actual folders
/// become new roots. Canonical JSON IDs avoid a bind per leaf or a variable cap.
async fn discover_linked_roots(
    db: &Database,
    frontier: &[Uuid],
) -> Result<Vec<(String, bool)>, ServiceError> {
    sqlx::query_as(
        r#"WITH frontier("Id") AS (SELECT value FROM json_each(?1)),
        closure_direct("Id") AS (
            SELECT a."ItemId" FROM "AncestorIds" a
            JOIN frontier f ON f."Id" = a."ParentItemId"
        ), closure_descendants("Id") AS (
            SELECT "Id" FROM closure_direct
            UNION
            SELECT a."ItemId" FROM "AncestorIds" a
            JOIN closure_direct d ON d."Id" = a."ParentItemId"
        ), candidates("Id", "LinkedFolder") AS (
            SELECT child."Id", 1 FROM "LinkedChildren" link
            JOIN frontier f ON f."Id" = link."ParentId"
            JOIN "BaseItems" child ON child."Id" = link."ChildId"
            WHERE child."IsFolder"
            UNION ALL
            SELECT owner."Id", 0 FROM "LinkedChildren" link
            JOIN closure_descendants d ON d."Id" = link."ParentId"
            JOIN "BaseItems" owner ON owner."Id" = link."ParentId"
            WHERE owner."IsFolder"
        ) SELECT "Id", MAX("LinkedFolder") FROM candidates GROUP BY "Id""#,
    )
    .bind(serde_json::json!(to_guid_strings(frontier)).to_string())
    .fetch_all(db.pool())
    .await
    .map_err(db_err)
}

/// The columns [`FerrofinItemRepository::top_parent_ids_for_query`] reads to
/// resolve one view.
#[derive(Clone, sqlx::FromRow)]
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
    /// Persisted link presence, independent of a stale serialized Data array.
    #[sqlx(rename = "_HasLinkedChildren")]
    has_linked_children: bool,
}

impl FerrofinItemRepository {
    /// Creates a repository over the given database and kind-lookup tables.
    #[must_use]
    pub fn new(db: Database, item_type_lookup: Arc<dyn ItemTypeLookup>) -> Self {
        Self {
            db,
            item_type_lookup,
            virtual_paths: VirtualPathExpander::identity(),
            active_recordings: ActiveRecordingSlot::default(),
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

    /// Installs the server's [`VirtualPathExpander`], so image rows adopted
    /// from Jellyfin (`%MetadataPath%/library/…`) come back as real paths —
    /// the read half of `BaseItemRepository.Map`. Called once by the
    /// composition root; without it stored tokens pass through verbatim.
    #[must_use]
    pub fn with_virtual_paths(mut self, virtual_paths: VirtualPathExpander) -> Self {
        self.virtual_paths = virtual_paths;
        self
    }

    /// Installs the slot the composition root later fills with Live TV's
    /// active-recording paths (`RecordingsManager.GetActiveRecordingInfo`), which
    /// `Video.SourceType` consults to key a video under Live TV rather than its
    /// channel. Without it no video is an active recording.
    #[must_use]
    pub fn with_active_recordings(mut self, slot: ActiveRecordingSlot) -> Self {
        self.active_recordings = slot;
        self
    }

    /// The paths Live TV is recording to, empty until the source is bound.
    async fn active_recording_paths(
        &self,
    ) -> Result<std::collections::HashSet<String>, ServiceError> {
        match self.active_recordings.get() {
            Some(source) => Ok(source().await?.into_iter().collect()),
            None => Ok(std::collections::HashSet::new()),
        }
    }

    /// The C# `ItemsController.GetItems` **non-query** branch, or `None` when
    /// this request is not it.
    ///
    /// Upstream (v12.2 ItemsController.cs:592; 10.11.8 lacked the last term):
    ///
    /// ```text
    /// if ((recursive.HasValue && recursive.Value) || ids.Length != 0
    ///     || item is not UserRootFolder || query.HasFilters) { …query… }
    /// else { result = new QueryResult<BaseItem>(folder.GetChildren(user, true)); }
    /// ```
    ///
    /// `GetChildren` builds its OWN `InternalItemsQuery`, so the request's sort
    /// and paging are discarded — measured against 10.11.8 on the batch pair,
    /// `sortBy=SortName&sortOrder=Descending`, `sortBy=DateCreated`,
    /// `limit=2&startIndex=1`, `includeItemTypes=Movie` (on 10.11.8 only; a
    /// filter now takes the query branch) and a bare `/Items` (no `parentId` at all —
    /// `LibraryManager.GetParentItem(null, userId)` returns the user root) each
    /// answered with the identical seven root rows in the identical order.
    ///
    /// Ferrofin used to answer that bare `/Items` with the WHOLE server (554
    /// rows against Jellyfin's 7) and to sort the concatenated playlists folder
    /// inline instead of appending it.
    ///
    /// Jellyfin 12 reaches `GetChildren` only for an UNFILTERED request: its
    /// `GetItems` runs a filtered one (`HasFilters`, such as the
    /// `includeItemTypes=Movie` above) recursively, so the `/Items` handler
    /// no longer sets this flag for one. Internal callers (resume, Next Up)
    /// still pass item types with it on purpose.
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
        let mut children = crate::query_restrictions::resolve(&self.db, &children)
            .await?
            .unwrap_or(children);
        // BaseItem.IsVisibleViaTags exempts the immediate children of the user
        // root from the allow-list. Blocked tags still hide a root library.
        children.include_inherited_tags.clear();
        let mut rows = self.fetch_rows(&children, QueryShape::FullRows).await?;
        // `AddChildrenFromCollection` keeps only `e.IsVisible(user)`
        // (Folder.cs:1414-1417). `Folder.IsVisible` (Folder.cs:231-253) is the
        // blocked-/enabled-folders check, and it applies to `ICollectionFolder
        // && not BasePluginFolder` — so a `CollectionFolder` is filtered, while
        // a `UserView` (not an `ICollectionFolder`) and the playlists folder (a
        // `BasePluginFolder`) always pass.
        if let Some(user) = filter.user.as_ref() {
            let mut enabled = Vec::with_capacity(rows.len());
            for row in rows {
                if Some(row.type_.as_str()) != stored_type_name(BaseItemKind::CollectionFolder)
                    || library_enabled(row.path.as_deref()).await
                {
                    enabled.push(row);
                }
            }
            rows = enabled;
            let hidden = self.hidden_media_folders(user).await?;
            if !hidden.is_empty() {
                rows.retain(|row| {
                    Some(row.type_.as_str()) != stored_type_name(BaseItemKind::CollectionFolder)
                        || Uuid::parse_str(&row.id).is_ok_and(|id| !hidden.contains(&id))
                });
            }
        }
        Ok(Some(rows))
    }

    /// `UserViewBuilder.GetUserItems(folders)` starts from visible root children,
    /// even for a recursive request, then applies GetResult's filters/sort/page.
    /// The root-controller shortcut deliberately ignores those query options and
    /// cannot be used as the final folders-view result.
    async fn folders_view_result(
        &self,
        filter: &InternalItemsQuery,
        parent: Option<&ViewRow>,
    ) -> Result<Option<QueryResult<BaseItemEntity>>, ServiceError> {
        // Folder.GetItems bypasses GetItemsInternal/UserViewBuilder when IDs
        // are explicit. Preserve that direct repository query and parent scope.
        if filter.parent_id.is_nil() || filter.physical_children_only || !filter.item_ids.is_empty()
        {
            return Ok(None);
        }
        let Some(parent) = parent else {
            return Ok(None);
        };
        if !crate::user_view_manager::is_folders_view_fields(
            &parent.type_name,
            parent.path.as_deref(),
            parent.data.as_deref(),
        ) {
            return Ok(None);
        }
        let root_query = InternalItemsQuery {
            user: filter.user.clone(),
            parent_id: self.roots.map_or_else(Uuid::nil, |roots| roots.user_root),
            user_root_children: true,
            ..Default::default()
        };
        // This helper uses fetch_rows for the real user root. Box this edge of
        // the mutually recursive async calls; the root is never a folders view.
        let children = Box::pin(self.user_root_children(&root_query))
            .await?
            .unwrap_or_default();
        // GetResult does not sort without an explicit OrderBy. Keep the root
        // helper's order (including appended plugin folders) in that case.
        let rows = self.filter_folder_children_inner(children, filter).await?;
        let mut rows = self.sort_folder_children_inner(rows, filter).await?;
        // Adjacency includes the selected item, runs after sorting, and precedes
        // paging. A missing selected item has no neighbours in this result set.
        if let Some(adjacent) = filter.adjacent_to.filter(|id| !id.is_nil()) {
            rows = adjacent_window(rows, |row| Uuid::parse_str(&row.id).ok() == Some(adjacent));
        }
        let total = i32::try_from(rows.len()).unwrap_or(i32::MAX);
        Ok(Some(QueryResult::new(
            Some(filter.start_index.unwrap_or(0)),
            Some(total),
            page(rows, filter),
        )))
    }

    async fn sort_folder_children_inner(
        &self,
        mut rows: Vec<BaseItemEntity>,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        use ferrofin_model::dto::SortOrder;
        use ferrofin_model::live_tv::ItemSortBy;
        if rows.is_empty() || filter.order_by.is_empty() {
            return Ok(rows);
        }
        if filter.order_by.iter().all(|(by, _)| {
            matches!(
                by,
                ItemSortBy::Name
                    | ItemSortBy::SortName
                    | ItemSortBy::Default
                    | ItemSortBy::Unrecognized(_)
            )
        }) {
            rows.sort_by(|a, b| {
                for (by, direction) in &filter.order_by {
                    let ordering = match by {
                        ItemSortBy::Name => {
                            folder_ordinal_compare(a.name.as_deref(), b.name.as_deref())
                        }
                        ItemSortBy::SortName => folder_ordinal_compare(
                            Some(&folder_sort_name(a)),
                            Some(&folder_sort_name(b)),
                        ),
                        _ => continue,
                    };
                    let ordering = if *direction == SortOrder::Descending {
                        ordering.reverse()
                    } else {
                        ordering
                    };
                    if !ordering.is_eq() {
                        return ordering;
                    }
                }
                std::cmp::Ordering::Equal
            });
            return Ok(rows);
        }
        // Keep the existing consumer for other sort fields. Its broader
        // comparator parity is a separate query review, not this display flag.
        let matching = InternalItemsQuery {
            user: filter.user.clone(),
            user_preferences_loaded: true,
            item_ids: rows
                .iter()
                .filter_map(|row| Uuid::parse_str(&row.id).ok())
                .collect(),
            order_by: filter.order_by.clone(),
            group_by_presentation_unique_key: false,
            include_owned_items: true,
            ..Default::default()
        };
        self.fetch_rows_resolved(&matching, QueryShape::FullRows)
            .await
    }

    /// The in-memory GetResult whitelist. Ordinary query translation has
    /// intentionally different semantics and cannot implement this consumer.
    // Independent source whitelist filters are kept together for review.
    #[allow(clippy::too_many_lines)]
    async fn filter_folder_children_inner(
        &self,
        mut rows: Vec<BaseItemEntity>,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let collation = folder_name_collator(filter)?;
        rows.retain(|row| folder_child_matches_fields(row, filter, collation));
        if rows.is_empty() {
            return Ok(rows);
        }
        if !filter.studio_ids.is_empty() || !filter.genre_ids.is_empty() {
            let ids: Vec<_> = filter
                .studio_ids
                .iter()
                .chain(&filter.genre_ids)
                .copied()
                .collect();
            let named = self.retrieve_items(&ids).await?;
            rows.retain(|row| {
                let matches = |ids: &[Uuid], values: Option<&str>| {
                    ids.is_empty()
                        || named.iter().any(|reference| {
                            Uuid::parse_str(&reference.id).is_ok_and(|id| ids.contains(&id))
                                && reference
                                    .name
                                    .as_deref()
                                    .is_some_and(|name| folder_values_contain(values, name))
                        })
                };
                matches(&filter.studio_ids, row.studios.as_deref())
                    && matches(&filter.genre_ids, row.genres.as_deref())
            });
        }
        if filter.has_imdb_id.is_some()
            || filter.has_tmdb_id.is_some()
            || filter.has_tvdb_id.is_some()
        {
            let ids = folder_row_ids_json(&rows)?;
            let providers: Vec<(String, String, String)> = sqlx::query_as(
                r#"SELECT "ItemId", "ProviderId", "ProviderValue" FROM "BaseItemProviders"
                   WHERE "ItemId" IN (SELECT value FROM json_each(?1))"#,
            )
            .bind(ids)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
            let mut present: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
            for (id, provider, value) in providers {
                if !value.is_empty() {
                    present
                        .entry(id)
                        .or_default()
                        .insert(upper_invariant(&provider));
                }
            }
            rows.retain(|row| {
                [
                    (filter.has_imdb_id, "IMDB"),
                    (filter.has_tmdb_id, "TMDB"),
                    (filter.has_tvdb_id, "TVDB"),
                ]
                .into_iter()
                .all(|(requested, key)| {
                    requested.is_none_or(|wanted| {
                        present
                            .get(&row.id)
                            .is_some_and(|providers| providers.contains(key))
                            == wanted
                    })
                })
            });
        }
        if !filter.image_types.is_empty() {
            let ids = folder_row_ids_json(&rows)?;
            let image_types: Vec<(String, i32)> = sqlx::query_as(
                r#"SELECT "ItemId", "ImageType" FROM "BaseItemImageInfos"
                   WHERE "ImageType" <> 10 AND "ItemId" IN (SELECT value FROM json_each(?1))
                   UNION ALL SELECT "ItemId", 10 FROM "Chapters"
                   WHERE "ChapterIndex" = 0 AND "ImagePath" IS NOT NULL AND "ImagePath" <> ''
                     AND "ItemId" IN (SELECT value FROM json_each(?1))"#,
            )
            .bind(ids)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
            let mut present: HashMap<String, std::collections::HashSet<i32>> = HashMap::new();
            for (id, kind) in image_types {
                present.entry(id).or_default().insert(kind);
            }
            rows.retain(|row| {
                present.get(&row.id).is_some_and(|images| {
                    filter
                        .image_types
                        .iter()
                        .any(|kind| images.contains(&kind.json_value()))
                })
            });
        }
        let own_requested = filter.is_liked.is_some()
            || filter.is_favorite_or_liked.is_some()
            || filter.is_favorite.is_some()
            || filter.is_resumable.is_some()
            || (filter.is_played.is_some() && rows.iter().any(|row| !row.is_folder));
        if own_requested && !rows.is_empty() {
            let user = filter.user_id().ok_or_else(|| {
                ServiceError::invalid_input("folder user-data filtering requires a user")
            })?;
            let own = self.folder_own_user_data(&rows, user).await?;
            rows.retain(|row| {
                let value = own.get(&row.id);
                // UserItemData.Likes is computed from Rating; the nullable
                // persisted Likes column is not this source property.
                let liked = value
                    .and_then(|value| value.rating)
                    .map(|rating| rating >= 6.5);
                let favorite = value.is_some_and(|value| value.is_favorite);
                filter.is_liked.is_none_or(|wanted| liked == Some(wanted))
                    && filter
                        .is_favorite_or_liked
                        .is_none_or(|wanted| (favorite || liked.unwrap_or(false)) == wanted)
                    && filter.is_favorite.is_none_or(|wanted| favorite == wanted)
                    && filter.is_resumable.is_none_or(|wanted| {
                        value.is_some_and(|value| value.playback_position_ticks > 0) == wanted
                    })
                    && (row.is_folder
                        || filter
                            .is_played
                            .is_none_or(|wanted| value.is_some_and(|value| value.played) == wanted))
            });
        }
        if let (Some(played), Some(user)) = (filter.is_played, filter.user.as_ref()) {
            use ferrofin_traits::persistence::ItemCountService as _;
            let ids: Vec<_> = rows
                .iter()
                .filter(|row| row.is_folder)
                .filter_map(|row| Uuid::parse_str(&row.id).ok())
                .collect();
            if !ids.is_empty() {
                let counts = crate::FerrofinItemCountService::new(self.db.clone())
                    .get_played_and_total_count_batch(&ids, user)
                    .await?;
                rows.retain(|row| {
                    !row.is_folder
                        || Uuid::parse_str(&row.id).is_ok_and(|id| {
                            let count = counts.get(&id).copied().unwrap_or_default();
                            (count.played >= count.total) == played
                        })
                });
            }
        }
        Ok(rows)
    }

    /// Resolve the source's first current key, then its attached-row fallback.
    /// Batch queries avoid introducing a read per immediate child.
    pub(crate) async fn folder_own_user_data(
        &self,
        rows: &[BaseItemEntity],
        user: Uuid,
    ) -> Result<HashMap<String, ferrofin_db::entities::playback::UserDataEntity>, ServiceError>
    {
        use ferrofin_db::entities::playback::UserDataEntity;
        let ids: Vec<_> = rows.iter().map(|row| row.id.clone()).collect();
        let mut connection = self.db.pool().acquire().await.map_err(db_err)?;
        let keys: HashMap<_, _> =
            crate::user_data_key_repository::load_keys_for_items(&mut connection, &ids)
                .await?
                .into_iter()
                .collect();
        let candidates: Vec<UserDataEntity> = sqlx::query_as(
            r#"SELECT * FROM "UserData" WHERE "UserId" = ?1
               AND "ItemId" IN (SELECT value FROM json_each(?2)) ORDER BY "CustomDataKey""#,
        )
        .bind(guid_to_db(user))
        .bind(folder_row_ids_json(rows)?)
        .fetch_all(&mut *connection)
        .await
        .map_err(db_err)?;
        let mut by_item: HashMap<String, Vec<UserDataEntity>> = HashMap::new();
        for candidate in candidates {
            by_item
                .entry(candidate.item_id.clone())
                .or_default()
                .push(candidate);
        }
        // `Video.SourceType` is `LiveTV` while its file is being recorded, which
        // takes the item out of the channel arm below. Asked only when a video
        // could be keyed by its channel.
        let active = if rows.iter().any(|row| {
            row.external_id.as_ref().is_some_and(|id| !id.is_empty())
                && row.channel_id.is_some()
                && row.path.is_some()
                && crate::item_type_lookup::kind_from_type_name(&row.type_).is_some_and(is_video)
        }) {
            self.active_recording_paths().await?
        } else {
            std::collections::HashSet::new()
        };
        let mut selected = HashMap::new();
        for row in rows {
            let Some(candidates) = by_item.remove(&row.id) else {
                continue;
            };
            let mut current = keys.get(&row.id).cloned().unwrap_or_default();
            // SourceType is a derived, JsonIgnore property: Channel's kind
            // always wins; LiveTV kinds suppress base ChannelId; ordinary items
            // derive it from the actual nonempty ChannelId, never a Data token.
            let channel = match crate::item_type_lookup::kind_from_type_name(&row.type_) {
                Some(BaseItemKind::Channel) => true,
                Some(
                    BaseItemKind::LiveTvChannel
                    | BaseItemKind::TvChannel
                    | BaseItemKind::LiveTvProgram
                    | BaseItemKind::TvProgram,
                ) => false,
                _ => row
                    .channel_id
                    .as_deref()
                    .is_some_and(|id| Uuid::parse_str(id).is_ok_and(|id| !id.is_nil())),
            };
            let recording = row.path.as_ref().is_some_and(|path| active.contains(path))
                && crate::item_type_lookup::kind_from_type_name(&row.type_).is_some_and(is_video);
            if channel
                && !recording
                && let Some(external) = row.external_id.as_ref().filter(|value| !value.is_empty())
            {
                // Derived classes prepend their provider keys ahead of BaseItem;
                // the channel key is immediately before the own-Guid last key.
                current.insert(current.len().saturating_sub(1), external.clone());
            }
            let preferred = current.iter().find_map(|key| {
                candidates
                    .iter()
                    .position(|value| value.custom_data_key == *key)
            });
            if let Some(value) = candidates.into_iter().nth(preferred.unwrap_or(0)) {
                selected.insert(row.id.clone(), value);
            }
        }
        Ok(selected)
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

    async fn query_view_row(db: &Database, id: Uuid) -> Result<Option<ViewRow>, ServiceError> {
        sqlx::query_as(
            r#"SELECT "Type", "ParentId", "Path", "Data", "TopParentId",
            EXISTS (SELECT 1 FROM "LinkedChildren" WHERE "ParentId" = ?1) AS "_HasLinkedChildren"
            FROM "BaseItems" WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .fetch_optional(db.pool())
        .await
        .map_err(db_err)
    }

    async fn query_parent_row(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Option<ViewRow>, ServiceError> {
        if filter.parent_id.is_nil()
            || filter.physical_children_only
            || self.roots.is_some_and(|roots| {
                filter.parent_id == roots.aggregate
                    || (!filter.recursive && filter.parent_id == roots.user_root)
            })
        {
            return Ok(None);
        }
        Self::query_view_row(&self.db, filter.parent_id).await
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
    /// Ferrofin's scan stores the same shape (a `Folder` row per location,
    /// named by `PhysicalFolderIds`), but rows an older Ferrofin scan saved
    /// hang off the collection folder itself (`ParentId` and `TopParentId`)
    /// until a scan saves them again, so a library's rows carry either: a view
    /// is translated to itself AND its physical folders
    /// ([`library_top_parents_by_view`]), never to the folders alone.
    async fn resolve_views(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Option<InternalItemsQuery>, ServiceError> {
        let parent = self.query_parent_row(filter).await?;
        self.resolve_views_with_parent(filter, parent.as_ref())
            .await
    }

    async fn resolve_views_with_parent(
        &self,
        filter: &InternalItemsQuery,
        parent: Option<&ViewRow>,
    ) -> Result<Option<InternalItemsQuery>, ServiceError> {
        let preferences = crate::query_restrictions::resolve(&self.db, filter).await?;
        let prepared = preferences.as_ref().unwrap_or(filter);
        let linked = Self::linked_container_scope(prepared, parent);
        let prepared = linked.as_ref().unwrap_or(prepared);
        let descendants = resolve_descendant_query(&self.db, prepared).await?;
        let prepared = descendants.as_ref().unwrap_or(prepared);
        if linked.is_some() {
            // AddUserToQuery still grants the user's libraries after Parent is
            // cleared; DescendantOfId is not one of its confinement guards.
            return Ok(Some(
                scope_to_user_libraries_prepared(&self.db, prepared)
                    .await?
                    .unwrap_or_else(|| prepared.clone()),
            ));
        }
        Ok(self
            .resolve_views_prepared(prepared, parent)
            .await?
            .or(descendants)
            .or(preferences))
    }

    /// LibraryManager.SetTopParentIdsOrAncestors selects the descendant
    /// helper only for a linked BoxSet/Playlist, then clears the parent scope.
    fn linked_container_scope(
        filter: &InternalItemsQuery,
        parent: Option<&ViewRow>,
    ) -> Option<InternalItemsQuery> {
        if !filter.recursive || filter.physical_children_only || filter.parent_id.is_nil() {
            return None;
        }
        let parent = parent?;
        let kind = crate::item_type_lookup::kind_from_type_name(&parent.type_name);
        if !parent.has_linked_children
            || !matches!(kind, Some(BaseItemKind::BoxSet | BaseItemKind::Playlist))
        {
            return None;
        }
        let mut resolved = filter.clone();
        resolved.descendant_of_id = Some(filter.parent_id);
        resolved.set_parent(None);
        Some(resolved)
    }

    async fn resolve_views_prepared(
        &self,
        filter: &InternalItemsQuery,
        parent: Option<&ViewRow>,
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
            && let Some(mut folders) = Self::top_parent_ids_for_query(
                &self.db,
                filter.parent_id,
                filter.user.as_ref(),
                0,
                parent.cloned(),
            )
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
            // intersect to the library's direct children: an adopted row hangs
            // off a physical folder, and only a row a Ferrofin scan saved
            // carries the collection folder as its parent.
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
            let folders = parent
                .filter(|row| {
                    Some(row.type_name.as_str()) == stored_type_name(BaseItemKind::CollectionFolder)
                })
                .and_then(|row| row.data.as_deref())
                .map(parse_physical_folder_ids)
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
            // contributes its physical folders — and itself, which is what
            // the rows a Ferrofin scan saved carry
            // ([`library_top_parents_by_view`]); anything else — a `UserView`
            // like Live TV or Playlists, or any id on a Ferrofin database,
            // where the view IS the top parent — contributes itself. Replacing
            // the whole set whenever *one* id expanded would silently drop the
            // scopes that did not.
            let by_view = library_top_parents_by_view(&self.db, &filter.top_parent_ids).await?;
            let mut expanded = Vec::with_capacity(filter.top_parent_ids.len());
            let changed = !by_view.is_empty();
            for id in &filter.top_parent_ids {
                match by_view.get(id) {
                    Some(folders) => expanded.extend(folders.iter().copied()),
                    None => expanded.push(*id),
                }
            }
            // A caller that already named a library's physical folders
            // beside it (the pruning's scope) gets each once.
            let mut seen = std::collections::HashSet::with_capacity(expanded.len());
            expanded.retain(|id| seen.insert(*id));
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
    /// not all `ICollectionFolder`/`UserView`). Native collection folders use
    /// their own id as the top-parent scope.
    /// `Some(vec![])` is different and deliberate: a *view* that resolves to
    /// nothing, which upstream turns into a match-nothing scope rather than letting
    /// the query widen to every library.
    ///
    /// The arms, in upstream's order:
    ///
    /// 1. a Live TV view stands for itself;
    /// 2. a `DisplayParentId` is followed (and a dangling one resolves to nothing);
    /// 3. so is a `ParentId`;
    /// 4. a view with neither stands for the libraries the user grouped into it,
    ///    when its type groups at all ([`Self::grouped_library_ids`]), and for
    ///    nothing otherwise;
    /// 5. a `CollectionFolder` becomes its `PhysicalFolderIds`, and itself
    ///    ([`library_top_parents_by_view`]).
    ///
    /// Both of the views a real 10.11.8 database carries take one of the first two
    /// arms: its Live TV view is `ViewType: livetv`, and its Playlists view carries
    /// a `DisplayParentId` pointing at the real `PlaylistsFolder`.
    async fn top_parent_ids_for_query(
        db: &Database,
        id: Uuid,
        user: Option<&UserEntity>,
        depth: u8,
        row: Option<ViewRow>,
    ) -> Result<Option<Vec<Uuid>>, ServiceError> {
        // A `DisplayParentId`/`ParentId` chain is data, so it can be a cycle.
        // Upstream recurses without a guard and would stack-overflow; refusing to
        // follow a long chain is the same answer for every shape that is not a bug.
        if depth > 8 {
            return Ok(Some(Vec::new()));
        }
        let row = match row {
            Some(row) => Some(row),
            None => Self::query_view_row(db, id).await?,
        };
        let Some(ViewRow {
            type_name,
            parent_id,
            path,
            data,
            top_parent_id,
            ..
        }) = row
        else {
            return Ok(None);
        };
        let kind = crate::item_type_lookup::kind_from_type_name(&type_name);
        if kind == Some(BaseItemKind::CollectionFolder) {
            // The physical folders AND the collection folder itself: an
            // adopted row carries the first until a scan saves it with the
            // second ([`library_top_parents_by_view`]).
            let mut folders = data
                .as_deref()
                .map(parse_physical_folder_ids)
                .unwrap_or_default();
            folders.retain(|folder| *folder != id);
            folders.insert(0, id);
            return Ok(Some(folders));
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
                Box::pin(Self::top_parent_ids_for_query(
                    db,
                    next,
                    user,
                    depth + 1,
                    None,
                ))
                .await?
                .unwrap_or_default(),
            ));
        }

        let view_type = crate::user_view_repository::view_type_of(data.as_deref());
        Box::pin(Self::grouped_library_ids(
            db,
            view_type.as_deref(),
            user,
            depth,
        ))
        .await
    }

    /// The libraries a user has GROUPED into this view — C#
    /// `GetTopParentIdsForQuery`'s grouping arm, reached by a `UserView` that has
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
        view_type: Option<&str>,
        user: Option<&UserEntity>,
        depth: u8,
    ) -> Result<Option<Vec<Uuid>>, ServiceError> {
        let Some(user) = user else {
            return Ok(Some(Vec::new()));
        };
        let mut scope = Vec::new();
        for folder in grouped_libraries(db, user, view_type).await? {
            match Box::pin(Self::top_parent_ids_for_query(
                db,
                folder,
                Some(user),
                depth + 1,
                None,
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
    /// library, which upstream treats as matching any view. A mixed library
    /// (`mixed.collection`) is untyped, as upstream parses it.
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
            })
            .filter(|collection_type| collection_type != "mixed"))
    }

    /// Runs a translated query in the requested shape, returning full rows.
    async fn fetch_rows(
        &self,
        filter: &InternalItemsQuery,
        shape: QueryShape,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let parent = self.query_parent_row(filter).await?;
        if let Some(result) = self.folders_view_result(filter, parent.as_ref()).await? {
            return Ok(result.items);
        }
        let resolved = self
            .resolve_views_with_parent(filter, parent.as_ref())
            .await?;
        let filter = resolved.as_ref().unwrap_or(filter);
        // The window is whole rows whatever the shape: every caller asks for
        // `FullRows`, the one shape this function's row type can carry.
        if let Some(window) = self.adjacent_ids_resolved(filter).await? {
            return Ok(sql_page(self.rows_in_order(&window).await?, filter));
        }
        self.fetch_rows_resolved(filter, shape).await
    }

    /// The SQL path's `AdjacentTo` (v12.2 `ApplyAdjacencyFilter`, which
    /// `GetItems`, `GetItemList` and `GetItemIdsList` run after the grouping
    /// filter and before paging): the ids of the item and its neighbours
    /// within this filter's own unpaged, ordered result, or `None` when the
    /// filter asks for no adjacency. The filter's views must be resolved.
    async fn adjacent_ids_resolved(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Option<Vec<Uuid>>, ServiceError> {
        let Some(adjacent) = filter.adjacent_to.filter(|id| !id.is_nil()) else {
            return Ok(None);
        };
        let unpaged = InternalItemsQuery {
            start_index: None,
            limit: None,
            ..filter.clone()
        };
        let mut qb = build_query(&unpaged, QueryShape::IdsOnly);
        let ids: Vec<Uuid> = qb
            .build_query_scalar::<String>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?
            .iter()
            .filter_map(|id| Uuid::parse_str(id).ok())
            .collect();
        Ok(Some(adjacent_window(ids, |id| *id == adjacent)))
    }

    /// The rows of an adjacency window, in the window's order. Upstream
    /// re-reads the window by id alone and orders it again; keeping the first
    /// pass's order is the same for every deterministic sort, and keeps a
    /// `Random` or linked-order window as the pass that chose it.
    async fn rows_in_order(&self, ids: &[Uuid]) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let mut rows = self.retrieve_items(ids).await?;
        rows.sort_by_key(|row| {
            Uuid::parse_str(&row.id)
                .ok()
                .and_then(|id| ids.iter().position(|window_id| *window_id == id))
        });
        Ok(rows)
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
        let parent = self.query_parent_row(filter).await?;
        if let Some(result) = self.folders_view_result(filter, parent.as_ref()).await? {
            return Ok(result
                .items
                .into_iter()
                .map(|row| (row.id, row.clean_name))
                .collect());
        }
        let resolved = self
            .resolve_views_with_parent(filter, parent.as_ref())
            .await?;
        let filter = resolved.as_ref().unwrap_or(filter);
        let mut qb = build_query(filter, QueryShape::IdAndCleanName);
        qb.build_query_as::<(String, Option<String>)>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
    }

    /// Runs a translated query returning only the id column.
    async fn fetch_ids(&self, filter: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        let parent = self.query_parent_row(filter).await?;
        if let Some(result) = self.folders_view_result(filter, parent.as_ref()).await? {
            return Ok(result
                .items
                .iter()
                .filter_map(|row| Uuid::parse_str(&row.id).ok())
                .collect());
        }
        let resolved = self
            .resolve_views_with_parent(filter, parent.as_ref())
            .await?;
        let filter = resolved.as_ref().unwrap_or(filter);
        if let Some(window) = self.adjacent_ids_resolved(filter).await? {
            return Ok(sql_page(window, filter));
        }
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

        let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
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

    /// Total for a by-name page that came back empty — `limit=0`, or an offset
    /// past the last row — where the page query's window total has no row to
    /// ride on. Accepts the SAME pre-computed scope the page query built, so
    /// the filter shape can never drift.
    async fn count_by_name_total(
        &self,
        scope: &ByNameScope<'_>,
        return_type: &str,
        filter: &InternalItemsQuery,
        outer: &InternalItemsQuery,
    ) -> Result<i32, ServiceError> {
        // `representativeIds.Count` (ByName.cs:227-230), not a row count:
        // upstream counts the COLLAPSED set, so the total has to be the number
        // of `PresentationUniqueKey` GROUPS the filtered rows form — the same
        // grouping the page query applies. `COUNT(DISTINCT key)` would be
        // wrong: it drops NULL keys entirely, and the whole point of the
        // grouping is that the unkeyed rows form ONE group.
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            r#"SELECT COUNT(*) FROM (SELECT 1 FROM "BaseItems" AS bi WHERE bi."Type" = "#,
        );
        qb.push_bind(return_type.to_owned());
        push_value_exists(&mut qb, scope, InnerScope::Page);
        append_by_name_filters(&mut qb, filter, outer);
        qb.push(r#" GROUP BY bi."PresentationUniqueKey")"#);
        let count: i64 = qb
            .build_query_scalar()
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(i32::try_from(count).unwrap_or(i32::MAX))
    }

    /// Port of v12 `BuildItemCountsByCleanName`
    /// (`BaseItemRepository.ByName.cs:324-409` at v12.2): one [`ItemCounts`] per
    /// `CleanName` of the page, each field the number of in-scope content
    /// items of that kind carrying the value.
    ///
    /// The scope is the browse's minus `IncludeItemTypes` — "the counts
    /// describe everything the value is attached to, not only the types the
    /// list was filtered down to" (`:336-337`) — and, exactly as upstream,
    /// three statements rather than one: the raw per-`(value, type, series)`
    /// counts, the series each value is written on, and the episodes under
    /// those series. Each drives off an index; the single-statement form
    /// left SQLite free to scan every episode in the library (`:418-419`).
    async fn item_counts_by_clean_name(
        &self,
        scope: &ByNameScope<'_>,
        value_types: &[ItemValueType],
        clean_names: &[String],
    ) -> Result<HashMap<String, ItemCounts>, ServiceError> {
        let mut counts_by_clean_name = HashMap::new();
        if clean_names.is_empty() {
            return Ok(counts_by_clean_name);
        }
        let series_type = stored_type_name(BaseItemKind::Series).unwrap_or_default();
        let episode_type = stored_type_name(BaseItemKind::Episode).unwrap_or_default();

        // `rawCounts` (`:360-368`): grouped by `(CleanName, Type, SeriesId)`,
        // counting distinct items, so an item linked to one clean name through
        // two value rows (two spellings, or Artist and AlbumArtist) counts once.
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            r#"SELECT iv."CleanValue", ci."Type", ci."SeriesId", COUNT(DISTINCT ci."Id") FROM "#,
        );
        push_value_links(&mut qb, scope, clean_names);
        push_content_scope(&mut qb, scope, InnerScope::Counts);
        qb.push(r#" GROUP BY iv."CleanValue", ci."Type", ci."SeriesId""#);
        let raw_counts: Vec<(String, String, Option<String>, i64)> = qb
            .build_query_as()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;

        // Only studios and genres pass down from a series to its episodes; an
        // artist credit does not (`:371`).
        let inherits_to_episodes = value_types
            .iter()
            .any(|t| matches!(t, ItemValueType::Studios | ItemValueType::Genre));
        let tagged_episodes: Vec<(&str, Option<&str>, i64)> = raw_counts
            .iter()
            .filter(|(_, ty, _, _)| ty == episode_type)
            .map(|(name, _, series, count)| (name.as_str(), series.as_deref(), *count))
            .collect();
        let episode_counts: HashMap<String, i64> = if inherits_to_episodes {
            self.episode_counts_by_clean_name(scope, clean_names, &tagged_episodes)
                .await?
        } else {
            let mut direct = HashMap::new();
            for (name, _, count) in &tagged_episodes {
                *direct.entry((*name).to_owned()).or_insert(0) += count;
            }
            direct
        };

        let kind_name = |kind: BaseItemKind| stored_type_name(kind).unwrap_or_default();
        let (movie, album, artist, music_video, program, audio, trailer, box_set, book) = (
            kind_name(BaseItemKind::Movie),
            kind_name(BaseItemKind::MusicAlbum),
            kind_name(BaseItemKind::MusicArtist),
            kind_name(BaseItemKind::MusicVideo),
            kind_name(BaseItemKind::LiveTvProgram),
            kind_name(BaseItemKind::Audio),
            kind_name(BaseItemKind::Trailer),
            kind_name(BaseItemKind::BoxSet),
            kind_name(BaseItemKind::Book),
        );
        // `ItemCountBuilder.Build` (`ItemCountBuilder.cs:20-81`).
        for (name, ty, _, count) in &raw_counts {
            let counts: &mut ItemCounts = counts_by_clean_name
                .entry(name.clone())
                .or_insert_with(ItemCounts::default);
            let count = clamp_count(*count);
            let ty = ty.as_str();
            if ty == series_type {
                counts.series_count += count;
            } else if ty == movie {
                counts.movie_count += count;
            } else if ty == album {
                counts.album_count += count;
            } else if ty == artist {
                counts.artist_count += count;
            } else if ty == music_video {
                counts.music_video_count += count;
            } else if ty == program {
                counts.program_count += count;
            } else if ty == audio {
                counts.song_count += count;
            } else if ty == trailer {
                counts.trailer_count += count;
            } else if ty == box_set {
                counts.box_set_count += count;
            } else if ty == book {
                counts.book_count += count;
            }
        }
        // Episodes are counted separately: the value is usually only written
        // on the series (`:392`).
        for (name, counts) in &mut counts_by_clean_name {
            counts.episode_count = clamp_count(episode_counts.get(name).copied().unwrap_or(0));
            counts.item_count = counts.total_item_count();
        }
        // A value carried by nothing but the episodes below a tagged series has
        // no row of its own (`:397-405`).
        for (name, episode_count) in episode_counts {
            counts_by_clean_name
                .entry(name)
                .or_insert_with(|| ItemCounts {
                    episode_count: clamp_count(episode_count),
                    item_count: clamp_count(episode_count),
                    ..ItemCounts::default()
                });
        }
        Ok(counts_by_clean_name)
    }

    /// Port of v12 `BuildEpisodeCountsByCleanName`
    /// (`BaseItemRepository.ByName.cs:415-464`): every in-scope episode under a
    /// series that carries the value counts, plus the episodes that carry it
    /// themselves under a series that does not.
    async fn episode_counts_by_clean_name(
        &self,
        scope: &ByNameScope<'_>,
        clean_names: &[String],
        tagged_episodes: &[(&str, Option<&str>, i64)],
    ) -> Result<HashMap<String, i64>, ServiceError> {
        let series_type = stored_type_name(BaseItemKind::Series).unwrap_or_default();
        let episode_type = stored_type_name(BaseItemKind::Episode).unwrap_or_default();

        // `taggedSeries` (`:424-430`): the in-scope series each value is on.
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new(r#"SELECT iv."CleanValue", ci."Id" FROM "#);
        push_value_links(&mut qb, scope, clean_names);
        qb.push(r#" AND ci."Type" = "#).push_bind(series_type);
        push_content_scope(&mut qb, scope, InnerScope::Counts);
        let tagged_series: Vec<(String, String)> = qb
            .build_query_as()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;

        // `episodesPerSeries` (`:432-440`): in-scope episodes per tagged series.
        let mut series_ids: Vec<String> = tagged_series.iter().map(|(_, id)| id.clone()).collect();
        series_ids.sort_unstable();
        series_ids.dedup();
        let episodes_per_series: HashMap<String, i64> = if series_ids.is_empty() {
            HashMap::new()
        } else {
            let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
                r#"SELECT ci."SeriesId", COUNT(*) FROM "BaseItems" ci WHERE ci."Type" = "#,
            );
            qb.push_bind(episode_type)
                .push(r#" AND ci."SeriesId" IS NOT NULL AND "#);
            push_in_list(&mut qb, r#"ci."SeriesId""#, &series_ids);
            push_content_scope(&mut qb, scope, InnerScope::Counts);
            qb.push(r#" GROUP BY ci."SeriesId""#);
            qb.build_query_as::<(String, i64)>()
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?
                .into_iter()
                .collect()
        };

        let mut episode_counts: HashMap<String, i64> = HashMap::new();
        let mut series_by_clean_name: HashMap<&str, std::collections::HashSet<&str>> =
            HashMap::new();
        for (name, series_id) in &tagged_series {
            series_by_clean_name
                .entry(name.as_str())
                .or_default()
                .insert(series_id.as_str());
        }
        for (name, series) in &series_by_clean_name {
            let total: i64 = series
                .iter()
                .map(|id| episodes_per_series.get(*id).copied().unwrap_or(0))
                .sum();
            episode_counts.insert((*name).to_owned(), total);
        }
        // An episode that carries the value under a series that carries it too
        // is already counted with its series (`:451-461`).
        for (name, series_id, count) in tagged_episodes {
            if let Some(series_id) = series_id
                && series_by_clean_name
                    .get(name)
                    .is_some_and(|series| series.contains(series_id))
            {
                continue;
            }
            *episode_counts.entry((*name).to_owned()).or_insert(0) += count;
        }
        Ok(episode_counts)
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
        //
        // `None` only when the caller named no kind: a list of kinds none of
        // which has a stored name (`Program`, `Recording`, …) is `Some(empty)`
        // and matches nothing, as `TranslateQuery` does (see `push_content_scope`).
        let content_type_names: Option<Vec<String>> =
            (!filter.include_item_types.is_empty()).then(|| {
                filter
                    .include_item_types
                    .iter()
                    .filter_map(|k| stored_type_name(*k).map(str::to_owned))
                    .collect()
            });
        let exclude_type_names: Vec<String> = filter
            .exclude_item_types
            .iter()
            .filter_map(|k| stored_type_name(*k).map(str::to_owned))
            .collect();
        let media_types: Vec<String> = filter
            .media_types
            .iter()
            .copied()
            .map(media_type_name)
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
        let filter = scoped.as_ref().unwrap_or(filter);
        let top_parents = to_guid_strings(&filter.top_parent_ids);

        // v12 assigns `TotalRecordCount = representativeIds.Count` whenever
        // `EnableTotalRecordCount` (`ByName.cs:227-230`), with or without a
        // `Limit`; 10.11.8's "no limit → no total" rule
        // (`BaseItemRepository.cs:1255-1258` there) is gone.
        let want_total = filter.enable_total_record_count;
        // Two levels, because the representative collapse sits BETWEEN the
        // filtered row set and the ordering/paging (see `push_representative_rank`).
        // The outer level is where `COUNT(*) OVER()` belongs: run in the inner
        // one it would count the rows BEFORE the collapse, i.e. report a total
        // larger than the number of rows any amount of paging could ever return.
        let mut select = String::from(r"SELECT bi.*");
        if want_total {
            select.push_str(r#", COUNT(*) OVER() AS "total_count""#);
        }
        select.push_str(r" FROM (SELECT bi.*");
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(select);
        let scope = ByNameScope {
            filter,
            type_ints: &type_ints,
            content_type_names: content_type_names.as_deref(),
            exclude_type_names: &exclude_type_names,
            media_types: &media_types,
            ancestors: &ancestors,
            top_parents: &top_parents,
        };
        let outer = by_name_outer_filter(filter);
        push_representative_rank(&mut qb, return_type, &top_parents);
        // The C# outer filter is **two** clauses — `.Where(e => e.Type ==
        // returnType)` and the `ItemValuesMap … .Any()` semijoin
        // (`ByName.cs:166-175`). Both are load-bearing: `CleanName` is the
        // only link on an adopted database (where `ItemValues.ItemValueId` is
        // a synthetic guid matching no item), and `Type` is what keeps the
        // answer to *this* browse — without it a Movie named "Drama" is
        // returned by `/Genres` beside the real genre, and `/Genres` and
        // `/MusicGenres`, which query the same `ItemValueType`, stop being
        // distinguishable. It is also the leading column of
        // `IX_BaseItems_Type_CleanName`, so constraining it keeps this
        // an index seek instead of a scan of every item.
        qb.push(r#" FROM "BaseItems" AS bi WHERE bi."Type" = "#)
            .push_bind(return_type.to_owned());
        push_value_exists(&mut qb, &scope, InnerScope::Page);
        append_by_name_filters(&mut qb, filter, &outer);
        qb.push(r#") AS bi WHERE bi."rep_rank" = 1"#);
        // C# `GetItemValues` runs the same `ApplyOrder(query, filter, context)`
        // the main browse does, so the caller's `sortBy`/`sortOrder` apply and
        // the no-`sortBy` default is `OrderBy(e => e.SortName)` — not `Name` —
        // or, with a `searchTerm`, the relevance rank alone.
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
            .build_query_as::<ByNameRow>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;

        // The window total rides on the page's rows; a page with none (an
        // offset past the end, or `limit=0` — jellyfin-web's "how many?"
        // probe) still owes the collapsed count, which upstream has for free
        // because it materializes `representativeIds` before paging.
        let start_index = filter.start_index.unwrap_or(0);
        let total = if want_total {
            match rows.first() {
                Some(r) => i32::try_from(r.total_count).unwrap_or(i32::MAX),
                None => {
                    self.count_by_name_total(&scope, return_type, filter, &outer)
                        .await?
                }
            }
        } else {
            0
        };
        let counts_by_clean_name = self.page_counts(&scope, value_types, filter, &rows).await?;
        let items: Vec<ItemWithCounts> = rows
            .into_iter()
            .map(|r| ItemWithCounts {
                counts: r
                    .item
                    .clean_name
                    .as_deref()
                    .and_then(|name| counts_by_clean_name.get(name))
                    .copied()
                    .unwrap_or_default(),
                item: r.item,
            })
            .collect();
        Ok(QueryResult::new(Some(start_index), Some(total), items))
    }

    /// The per-kind counts of a by-name page — only when `ItemCounts` is
    /// requested (`ByName.cs:251`), and only for the page's names
    /// (`:253-259`). v10.11.8 (`BaseItemRepository.cs:1368-1392`, comment
    /// "TODO: This is bad refactor!") handed EVERY row the same uncorrelated
    /// totals.
    async fn page_counts(
        &self,
        scope: &ByNameScope<'_>,
        value_types: &[ItemValueType],
        filter: &InternalItemsQuery,
        rows: &[ByNameRow],
    ) -> Result<HashMap<String, ItemCounts>, ServiceError> {
        if !filter
            .dto_options
            .contains_field(ferrofin_model::querying::ItemFields::ItemCounts)
        {
            return Ok(HashMap::new());
        }
        let mut page_clean_names: Vec<String> = rows
            .iter()
            .filter_map(|r| r.item.clean_name.clone())
            .filter(|name| !name.is_empty())
            .collect();
        page_clean_names.sort_unstable();
        page_clean_names.dedup();
        // One bound parameter per name, and the page size is the caller's: a
        // by-name browse with no `limit` would otherwise push the statement past
        // SQLite's variable cap. Each name's counts are computed entirely within
        // its own chunk (the grouping key is the name), so merging is exact.
        let mut counts = HashMap::new();
        for chunk in page_clean_names.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            counts.extend(
                self.item_counts_by_clean_name(scope, value_types, chunk)
                    .await?,
            );
        }
        Ok(counts)
    }
}

/// A by-name page row. `total_count` carries the `COUNT(*) OVER()`
/// window-function total when the caller needs pagination metadata, avoiding
/// a separate COUNT round-trip.
#[derive(sqlx::FromRow)]
struct ByNameRow {
    #[sqlx(flatten)]
    item: BaseItemEntity,
    #[sqlx(default)]
    total_count: i64,
}

/// Narrows one aggregate count to the `i32` the DTO field is.
fn clamp_count(n: i64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Pushes the v12 by-name semijoin (`ByName.cs:168-175`): `AND EXISTS (…)` —
/// the by-name row's `CleanName` is carried, as one of `type_ints`, by at
/// least one content item inside the browse's scope.
///
/// Correlated on `bi."CleanName"` rather than joined to a `GROUP BY`
/// aggregate, and for a measured reason: the aggregate shape ran the whole
/// `ItemValuesMap` join for every value on the server before the by-name
/// filter saw a row (296 artists × ~20k items ≈ 5.9 M probes, 927 ms for the
/// search screen's `/Artists?searchTerm=`); the semijoin runs one indexed
/// probe per surviving by-name row — `IX_ItemValues_Type_CleanValue` seeks
/// `(Type, CleanValue)`, then the map's primary key and the content row's.
/// Shared by the page query and the total-count query so their WHERE stays
/// identical.
fn push_value_exists(qb: &mut QueryBuilder<Sqlite>, scope: &ByNameScope<'_>, inner: InnerScope) {
    qb.push(
        r#" AND EXISTS (SELECT 1 FROM "ItemValues" iv
           JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
           JOIN "BaseItems" ci ON ci."Id" = ivm."ItemId"
           WHERE iv."CleanValue" = bi."CleanName" AND "#,
    );
    push_in_list(qb, r#"iv."Type""#, scope.type_ints);
    push_content_scope(qb, scope, inner);
    qb.push(")");
}

/// Pushes the `FROM` of a value-links statement (v12 `valueLinks`,
/// `ByName.cs:314-317`): the `ItemValues` rows of `scope.type_ints` whose
/// `CleanValue` is one of `clean_names`, joined to the content rows (`ci`)
/// that carry them. Ends inside the `WHERE`, ready for more `AND` terms.
fn push_value_links(
    qb: &mut QueryBuilder<Sqlite>,
    scope: &ByNameScope<'_>,
    clean_names: &[String],
) {
    qb.push(
        r#""ItemValues" iv
           JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
           JOIN "BaseItems" ci ON ci."Id" = ivm."ItemId"
           WHERE "#,
    );
    push_in_list(qb, r#"iv."Type""#, scope.type_ints);
    qb.push(" AND ");
    push_in_list(qb, r#"iv."CleanValue""#, clean_names);
}

/// Which of v12's two inner filters a content-row predicate set is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InnerScope {
    /// The `innerQueryFilter` behind the page and its total
    /// (`ByName.cs:149-164`): kinds included and excluded, media types, the
    /// ancestors / item ids / libraries, and the Live TV flags.
    Page,
    /// The `scopeQuery` behind the per-kind counts (`:301-311`): no included
    /// kinds and no flags — "the counts describe everything the value is
    /// attached to" — but the played state and the excluded ids.
    Counts,
}

/// Pushes the content-row predicates of a by-name inner query onto the `ci`
/// alias — the fields of the [`InnerScope`] asked for, each as
/// `TranslateQuery` spells it for a browse, plus what `TranslateQuery` adds
/// to any query that does not ask for owned items: no alternate version whose
/// primary is in the same library, no owned non-extras (v12.2
/// `TranslateQuery.cs:798-811`).
#[allow(clippy::too_many_lines)] // one predicate per C# field, kept in the C# order
fn push_content_scope(qb: &mut QueryBuilder<Sqlite>, scope: &ByNameScope<'_>, inner: InnerScope) {
    crate::translate_query::append_parental_restrictions(qb, scope.filter, "ci");
    let ByNameScope {
        filter,
        content_type_names,
        exclude_type_names,
        media_types,
        ancestors,
        top_parents,
        ..
    } = scope;
    // The kinds, as `TranslateQuery` filters them (v12.2
    // `TranslateQuery.cs:282-313`): with kinds included, `WhereOneOrMany` over
    // their stored names, which matches nothing when none has one
    // (`push_in_list` writes `1 = 0`), and the excludes are ignored; with none
    // included, the excludes. The counts' `scopeQuery` carries no included
    // kinds (`ByName.cs:336-346`), so it always takes the excludes.
    //
    // `+`: a plan pin, like the one on `TopParentId` below. Unpinned, the
    // planner drives the semijoin from every content row of the kind
    // (`IX_BaseItems_Type_…`, `Type=?`) and probes the value per
    // row — `/Artists?includeItemTypes=MusicAlbum` walks 800 albums per
    // artist: 45 ms; pinned, it walks the artist's own map rows: 4 ms.
    match content_type_names {
        Some(names) if inner == InnerScope::Page => {
            qb.push(" AND ");
            push_in_list(qb, r#"+ci."Type""#, names);
        }
        _ if !exclude_type_names.is_empty() => {
            qb.push(r#" AND ci."Type" NOT IN ("#);
            let mut sep = qb.separated(", ");
            for name in *exclude_type_names {
                sep.push_bind(name.clone());
            }
            qb.push(")");
        }
        _ => {}
    }
    if !media_types.is_empty() {
        qb.push(" AND ");
        push_in_list(qb, r#"ci."MediaType""#, media_types);
    }
    if !ancestors.is_empty() {
        qb.push(r#" AND EXISTS (SELECT 1 FROM "AncestorIds" a WHERE a."ItemId" = ci."Id" AND "#);
        push_in_list(qb, r#"a."ParentItemId""#, ancestors);
        qb.push(")");
    }
    // `innerQueryFilter.ParentId = filter.ParentId` (v12.2 `ByName.cs:188`, and
    // the counts' `scopeQuery`, `:346`), which `TranslateQuery` matches as a
    // strict `ParentId` equality (`TranslateQuery.cs:320-322`): the parent's
    // direct children, never a box set's linked members.
    //
    // `+`: a plan pin, like `Type` above and `TopParentId` below. Unpinned, the
    // planner drives the semijoin from the parent's children
    // (`IX_BaseItems_ParentId`) and probes the value's map for each, per
    // candidate value: 1.1 s for 400 genres over a folder of 6,000 movies,
    // against 0.1 s probing each map row's item by its primary key.
    if !filter.parent_id.is_nil() {
        qb.push(r#" AND +ci."ParentId" = "#)
            .push_bind(guid_to_db(filter.parent_id));
    }
    if !filter.item_ids.is_empty() {
        qb.push(" AND ");
        push_in_list(qb, r#"ci."Id""#, &to_guid_strings(&filter.item_ids));
    }
    if inner == InnerScope::Page {
        // The Live TV flags (`ByName.cs:159-163`), as `TranslateQuery` spells
        // them for a browse: `IsMovie` is skipped when the kinds already say
        // movie-ish (`append_predicates`), Sports/News/Kids are tag sugar.
        if let Some(is_movie) = filter.is_movie {
            let include_all_movie_types = is_movie
                && (filter.include_item_types.is_empty()
                    || filter.include_item_types.contains(&BaseItemKind::Movie)
                    || filter.include_item_types.contains(&BaseItemKind::Trailer));
            if !include_all_movie_types {
                qb.push(r#" AND ci."IsMovie" = "#).push_bind(is_movie);
            }
        }
        if let Some(is_series) = filter.is_series {
            qb.push(r#" AND ci."IsSeries" = "#).push_bind(is_series);
        }
        for (flag, tag) in [
            (filter.is_sports, "sports"),
            (filter.is_news, "news"),
            (filter.is_kids, "kids"),
        ] {
            let Some(want) = flag else { continue };
            qb.push(if want {
                " AND EXISTS "
            } else {
                " AND NOT EXISTS "
            })
            .push(
                r#"(SELECT 1 FROM "ItemValuesMap" tm
                    JOIN "ItemValues" tv ON tv."ItemValueId" = tm."ItemValueId"
                    WHERE tm."ItemId" = ci."Id" AND tv."Type" = "#,
            )
            .push_bind(i64::from(i32::from(ItemValueType::Tags)))
            .push(r#" AND tv."CleanValue" = "#)
            .push_bind(tag.to_owned())
            .push(")");
        }
        if let Some(is_airing) = filter.is_airing {
            crate::translate_query::push_is_airing(
                qb,
                "ci",
                is_airing,
                &datetime_to_db(chrono::Utc::now()),
            );
        }
    } else {
        if !filter.exclude_item_ids.is_empty() {
            qb.push(r#" AND ci."Id" NOT IN ("#);
            let mut sep = qb.separated(", ");
            for id in to_guid_strings(&filter.exclude_item_ids) {
                sep.push_bind(id);
            }
            qb.push(")");
        }
        // The played state of the content row, as the browse spells it
        // (`scopeQuery.IsPlayed` through `TranslateQuery`).
        if let (Some(user_id), Some(want)) = (filter.user_id(), filter.is_played) {
            qb.push(if want { " AND " } else { " AND NOT " });
            crate::translate_query::push_is_played(qb, "ci", &guid_to_db(user_id), filter);
        }
    }
    // `ApplyAlternateVersionFiltering` (v12.2 `QueryBuilding.cs:472-483`): a
    // version is hidden only while its primary exists in the same library
    // (`translate_query`'s `ALTERNATE_VERSION_HIDDEN`, on the `ci` alias).
    qb.push(
        r#" AND (ci."PrimaryVersionId" IS NULL OR NOT EXISTS (SELECT 1 FROM "BaseItems" p
                 WHERE p."Id" = ci."PrimaryVersionId" AND p."TopParentId" IS ci."TopParentId"))
            AND (ci."OwnerId" IS NULL OR ci."OwnerId" = '00000000-0000-0000-0000-000000000000'
                 OR ci."ExtraType" IS NOT NULL)"#,
    );
    // The user's libraries. Upstream reaches this through `TranslateQuery` on
    // the inner item query, which `AddUserToQuery` has already confined; here
    // the caller resolves the scope and passes it down. Without it the
    // by-name tabs aggregate over the WHOLE server, so a restricted account is
    // offered a genre or a studio that `/Items` will then refuse to return —
    // a filter list that lies about what is behind it.
    //
    // `+`: a plan pin. Without it the planner — which has no `sqlite_stat1`
    // to tell it the four-value `IN` matches every item there is — takes
    // `IX_BaseItems_TopParentId_Id` as a cheap seek and drives the join from
    // `ci`, walking the whole library per value (the 927 ms above). Pinned,
    // the predicate is a filter on the row the primary key already fetched.
    if !top_parents.is_empty() {
        qb.push(" AND ");
        push_in_list(qb, r#"+ci."TopParentId""#, top_parents);
    }
}

/// The pre-computed vectors that shape a by-name inner query.
///
/// One struct so the page query, the paging-fallback count and the per-kind
/// counts cannot drift apart: they take the SAME value, and a filter added to
/// one is a filter in all of them.
struct ByNameScope<'a> {
    /// The browse's filter, for the fields that need no pre-computation
    /// (item ids, the Live TV flags, the played state).
    filter: &'a InternalItemsQuery,
    /// The `ItemValues.Type` numbers this tab aggregates.
    type_ints: &'a [i64],
    /// Stored type names of the content items that may contribute — the
    /// caller's `IncludeItemTypes` and nothing else, exactly as C# builds its
    /// `innerQueryFilter`. `None` when the caller named no kind; `Some(&[])`
    /// when it named only kinds without a stored name, which matches nothing.
    content_type_names: Option<&'a [String]>,
    /// Stored type names the content items must not be (`ExcludeItemTypes`):
    /// the page applies them only while `content_type_names` is `None`, the
    /// per-kind counts always.
    exclude_type_names: &'a [String],
    /// Stored `MediaType` names the content items are restricted to.
    media_types: &'a [String],
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
    qb: &mut QueryBuilder<Sqlite>,
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

/// The physical folders each of `ids` stands for, for those that are Jellyfin
/// collection folders — see
/// [`FerrofinItemRepository::physical_folders_by_view`], which is the doc for
/// why this translation exists at all.
///
/// A free function because the child-count service and the user views need
/// the same one: a library's `ChildCount` counts its physical folders'
/// children too, and grouping on the raw `ParentId` reports 0 for every
/// library on an adopted database Ferrofin has not scanned yet. Reads that
/// scope a library by `TopParentId` take [`library_top_parents_by_view`].
pub(crate) async fn physical_folders_by_view(
    db: &Database,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Uuid>>, ServiceError> {
    let Some(collection_folder) = stored_type_name(BaseItemKind::CollectionFolder) else {
        return Ok(HashMap::new());
    };
    let mut by_view = HashMap::new();
    // Chunked: a count batch passes a whole unpaged page, which can exceed
    // SQLite's bind limit.
    for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            // `+"Type"`: the id list is the selective side; see `append_type_filters`.
            r#"SELECT "Id", "Data" FROM "BaseItems" WHERE "Data" IS NOT NULL AND +"Type" = "#,
        );
        qb.push_bind(collection_folder).push(" AND ");
        push_in_list(&mut qb, r#""Id""#, &to_guid_strings(chunk));
        let rows: Vec<(String, String)> = qb
            .build_query_as::<(String, String)>()
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;
        by_view.extend(rows.iter().filter_map(|(id, blob)| {
            let folders = parse_physical_folder_ids(blob);
            if folders.is_empty() {
                return None;
            }
            Some((Uuid::parse_str(id).ok()?, folders))
        }));
    }
    Ok(by_view)
}

/// The `TopParentId`s the items of each of `ids` carry, for those that are
/// Jellyfin collection folders: the folder itself first, then its
/// `PhysicalFolderIds` ([`physical_folders_by_view`]). Ids that are not —
/// every id on a Ferrofin-written database — are absent from the map, and a
/// query scoped to one stays scoped to it alone.
///
/// Jellyfin stores the physical folder as a library item's `TopParentId`
/// (`BaseItem.GetTopParent`: the ancestor directly under the
/// `AggregateFolder`), and upstream reads a library through
/// `CollectionFolder.PhysicalFolderIds` (`GetTopParentIdsForQuery`). Ferrofin
/// writes the collection folder instead, so on an adopted database a
/// library's rows carry the physical folder until a scan saves them and the
/// collection folder after — both at once whenever a scan has not saved
/// every row (a first scan still running or cancelled, a row the scan does
/// not plan). Every read of "the items of this library" takes the union, or
/// the rows a scan saved vanish from the library's browse, counts, next-up
/// and pruning.
pub(crate) async fn library_top_parents_by_view(
    db: &Database,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Uuid>>, ServiceError> {
    let mut by_view = physical_folders_by_view(db, ids).await?;
    for (view, folders) in &mut by_view {
        folders.retain(|folder| folder != view);
        folders.insert(0, *view);
    }
    Ok(by_view)
}

/// `AdjacentTo` over an ordered result (v12.2 `ApplyAdjacencyFilter`,
/// `UserViewBuilder.FilterForAdjacency`): the item and its immediate
/// neighbours, in `items`' order, or nothing when the item is not in `items`.
pub(crate) fn adjacent_window<T>(items: Vec<T>, is_item: impl Fn(&T) -> bool) -> Vec<T> {
    let Some(index) = items.iter().position(is_item) else {
        return Vec::new();
    };
    let start = index.saturating_sub(1);
    items
        .into_iter()
        .skip(start)
        .take(index + 2 - start)
        .collect()
}

/// Pages an in-memory result as `UserViewBuilder.SortAndPage` does:
/// `start_index` rows skipped, then at most a positive `limit` (a limit of 0
/// or less only skips; compare [`sql_page`]).
pub(crate) fn page<T>(rows: Vec<T>, filter: &InternalItemsQuery) -> Vec<T> {
    let start = usize::try_from(filter.start_index.unwrap_or(0).max(0)).unwrap_or(usize::MAX);
    let take = filter
        .limit
        .filter(|limit| *limit > 0)
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(usize::MAX);
    rows.into_iter().skip(start).take(take).collect()
}

/// The limit half of `BaseItemRepository.PrepareFilterQuery`
/// (`BaseItemRepository.cs:135-138`), which `GetItemIdsList`, `GetItems`,
/// `GetItemList` and `GetLatestItemList` run first: a limited query whose
/// caller de-duplicates by metadata key reads 4 more rows for it to drop.
/// `None` leaves the filter as it is. Unlike upstream, which raises the
/// caller's `Limit` in place, the caller's filter is never changed.
fn grouped_by_metadata_key(filter: &InternalItemsQuery) -> Option<InternalItemsQuery> {
    if !filter.enable_group_by_metadata_key {
        return None;
    }
    let limit = filter.limit?;
    Some(InternalItemsQuery {
        limit: Some(limit.saturating_add(4)),
        ..filter.clone()
    })
}

/// Pages an in-memory result as the SQL builder's `LIMIT`/`OFFSET` would
/// (`ApplyQueryPaging`): a `limit` of 0 takes nothing, a negative one is
/// SQLite's "no limit".
fn sql_page<T>(rows: Vec<T>, filter: &InternalItemsQuery) -> Vec<T> {
    let start = usize::try_from(filter.start_index.unwrap_or(0)).unwrap_or(0);
    let take = filter
        .limit
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(usize::MAX);
    rows.into_iter().skip(start).take(take).collect()
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
   FROM "LinkedChildren" lc
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
    /// `FerrofinPlaylists.OwnerUserId` (NULL for a legacy playlist, an older
    /// build's user-less API-key create, or a `Data` naming no owner).
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
/// - `SearchTerm` ports the C# two-branch shape (v12 `TranslateQuery.cs:250-265`):
///   a term containing a wildcard character goes through `LIKE '%term%'`
///   **unescaped** (client wildcards pass through, as upstream intends); a plain
///   term is cleaned (`get_clean_value`, as v12's `GetCleanValue`) and matched
///   as a literal contains on `CleanName`.
/// - `NameStartsWith` is a prefix on the sort key. Deliberate divergence:
///   `COALESCE(SortName, Name)` because Ferrofin leaves by-name `SortName` NULL
///   (C# filters `SortName` alone, which would match nothing here).
/// - `NameStartsWithOrGreater`/`NameLessThan` are full-string comparisons of
///   `SortName` against the lowercased bound, as C# compares them.
///
/// Everything else in C#'s `outerQueryFilter` (`ByName.cs:177-196`) — played /
/// favorite / liked / locked state, tags, official ratings, years, referenced
/// studios and genres, excluded ids — is the same `TranslateQuery` a browse
/// runs, so it runs through [`append_predicates`] over `outer`, the filter
/// narrowed to exactly those fields (see [`by_name_outer_filter`]).
fn append_by_name_filters(
    qb: &mut QueryBuilder<Sqlite>,
    filter: &InternalItemsQuery,
    outer: &InternalItemsQuery,
) {
    fn non_blank(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|s| !s.is_empty())
    }
    append_predicates(qb, outer);
    if let Some(term) = non_blank(filter.search_term.as_deref()) {
        let lowered = lower_invariant(term);
        if lowered.contains(SEARCH_WILDCARD_TERMS) {
            let like = format!("%{}%", lowered.trim_matches('%'));
            qb.push(r#" AND ferrofin_lower_invariant(bi."CleanName") LIKE "#)
                .push_bind(like);
        } else {
            let like = format!(
                "%{}%",
                crate::text_util::get_clean_value(term).trim_matches('%')
            );
            qb.push(r#" AND bi."CleanName" LIKE "#).push_bind(like);
        }
    }
    if let Some(prefix) = non_blank(filter.name_starts_with.as_deref()) {
        qb.push(r#" AND ferrofin_upper_invariant(COALESCE(bi."SortName", bi."Name")) LIKE "#)
            .push_bind(format!("{}%", upper_invariant(prefix)));
    }
    // Both bounds are FULL-STRING comparisons against the lowercased parameter,
    // over `SortName` only (C# `BaseItemRepository.cs:2036-2046`:
    // `SortName!.CompareTo(param.ToLowerInvariant()) >= 0` / `< 0`). Comparing
    // only the first character, case-sensitively, made `nameLessThan=Jb` and
    // `nameStartsWithOrGreater=j` return the wrong page — and `>` instead of
    // `>=` dropped the boundary row itself.
    if let Some(boundary) = non_blank(filter.name_starts_with_or_greater.as_deref()) {
        qb.push(r#" AND ferrofin_lower_invariant(bi."SortName") >= "#)
            .push_bind(lower_invariant(boundary));
    }
    if let Some(boundary) = non_blank(filter.name_less_than.as_deref()) {
        qb.push(r#" AND ferrofin_lower_invariant(bi."SortName") < "#)
            .push_bind(lower_invariant(boundary));
    }
}

/// The C# `outerQueryFilter` (`ByName.cs:208-232` at v12.2) minus the name
/// filters [`append_by_name_filters`] spells itself: the predicates that apply
/// to the by-name row — favorite state in its own `UserData` row (C# joins
/// `UserData` on the by-name item id), its tags, ratings, years, referenced
/// items.
///
/// A fresh query rather than the caller's, so that none of the inner-query
/// fields (`IncludeItemTypes`, `MediaTypes`, `AncestorIds`, paging) reaches
/// the by-name row: a genre is not a Movie, and `includeItemTypes=Movie`
/// must narrow what carries it, not the genre row itself.
///
/// The allow list stays on the inner scope only (`IncludeInheritedTags = []`):
/// a genre, studio or artist carries none of the tags of the media it
/// describes, so on the by-name row it could only ever hide all of them.
fn by_name_outer_filter(filter: &InternalItemsQuery) -> InternalItemsQuery {
    InternalItemsQuery {
        user: filter.user.clone(),
        user_preferences_loaded: filter.user_preferences_loaded,
        user_has_content_restrictions: filter.user_has_content_restrictions,
        block_unrated_items: filter.block_unrated_items.clone(),
        exclude_inherited_tags: filter.exclude_inherited_tags.clone(),
        is_played: filter.is_played,
        is_favorite: filter.is_favorite,
        is_favorite_or_liked: filter.is_favorite_or_liked,
        is_liked: filter.is_liked,
        is_locked: filter.is_locked,
        tags: filter.tags.clone(),
        official_ratings: filter.official_ratings.clone(),
        studio_ids: filter.studio_ids.clone(),
        genre_ids: filter.genre_ids.clone(),
        genres: filter.genres.clone(),
        years: filter.years.clone(),
        name_contains: filter.name_contains.clone(),
        exclude_item_ids: filter.exclude_item_ids.clone(),
        ..InternalItemsQuery::default()
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
pub(crate) fn image_type_from_disc(disc: i32) -> ImageType {
    ImageType::from_json_value(disc)
}

/// Maps a wire [`ImageType`] back to its `BaseItemImageInfos.ImageType` integer
/// discriminant — the inverse of [`image_type_from_disc`].
///
/// The discriminants line up 1:1 with the C# `ImageType` declaration order.
pub(crate) fn image_type_to_disc(image_type: ImageType) -> i32 {
    image_type.json_value()
}

/// The image rows of the items bound to `placeholders`, each item's in the
/// order [`ItemRepository::get_image_infos`] lists them and
/// `/Images/{type}/{index}` addresses them (type, then `rowid`), which the
/// `(ItemId, ImageType)` index yields without a sort. `VACUUM` may renumber
/// this table's rowids (its key is a TEXT `Id`) but copies rows in rowid
/// order, so their relative order survives.
pub(crate) fn images_of_items_sql(placeholders: &str) -> String {
    format!(
        r#"SELECT * FROM "BaseItemImageInfos"
           WHERE "ItemId" IN ({placeholders})
           ORDER BY "ItemId", "ImageType", rowid"#
    )
}

/// Projects a persisted [`BaseItemImageInfoEntity`] row into an
/// [`ItemImageInfo`].
///
/// The stored `Blurhash` is a UTF-8 byte blob; an empty blob (or one that is not
/// valid UTF-8) becomes [`None`]. A zero/negative width or height (the "unknown"
/// sentinel) is preserved as-is; the API layer nulls those out per Jellyfin.
fn image_info_from_row(
    row: BaseItemImageInfoEntity,
    virtual_paths: &VirtualPathExpander,
) -> ItemImageInfo {
    let blur_hash = row
        .blurhash
        .filter(|b| !b.is_empty())
        .and_then(|b| String::from_utf8(b).ok());
    ItemImageInfo {
        path: virtual_paths.expand(&row.path),
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

/// Loads the user snapshot needed by repository count queries.
pub(crate) async fn query_user(
    db: &Database,
    id: Uuid,
) -> Result<Option<UserEntity>, ServiceError> {
    sqlx::query_as(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .fetch_optional(db.pool())
        .await
        .map_err(db_err)
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
/// The eight fields checked are exactly the eight the C# checks; any one of
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
    let preferences = crate::query_restrictions::resolve(db, filter).await?;
    let prepared = preferences.as_ref().unwrap_or(filter);
    Ok(scope_to_user_libraries_prepared(db, prepared)
        .await?
        .or(preferences))
}

async fn scope_to_user_libraries_prepared(
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
        && filter.item_ids.is_empty()
        && filter.owner_ids.is_empty();
    if !unscoped {
        return Ok(None);
    }

    let views = visible_views(db, user).await?;
    let by_view = library_top_parents_by_view(db, &views).await?;
    // `GetTopParentIdsForQuery` per view: a collection folder becomes its
    // physical folders plus itself (what the rows a Ferrofin scan saved
    // carry, [`library_top_parents_by_view`]), and anything else — a Live TV
    // view, or any view on a Ferrofin-written database — stands for itself.
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

/// The libraries `user` grouped into a `view_type` view: of the views the
/// user may see, those in their `GroupedFolders` preference whose
/// `CollectionType` is the view's or none (an untyped library groups into any
/// view). Empty unless `view_type` is `movies`, `tvshows` or none (an untyped
/// view), the set `UserView.IsEligibleForGrouping` accepts (`UserView.cs:22-27`);
/// an `unknown` view, which upstream excludes by name, groups nothing. An untyped
/// view therefore stands for the grouped libraries that have no type.
///
/// The set a grouped view's browse is scoped to
/// (`GetTopParentIdsForQuery`'s grouping arm) and the set its `ChildCount`
/// counts (`DtoService.GetChildCountSourceIds`, v12.2 `DtoService.cs:781-792`).
pub(crate) async fn grouped_libraries(
    db: &Database,
    user: &UserEntity,
    view_type: Option<&str>,
) -> Result<Vec<Uuid>, ServiceError> {
    if !matches!(view_type, None | Some("movies" | "tvshows")) {
        return Ok(Vec::new());
    }
    let grouped = guid_preference(db.pool(), &user.id, PreferenceKind::GroupedFolders).await?;
    if grouped.is_empty() {
        return Ok(Vec::new());
    }
    let mut libraries = Vec::new();
    for folder in visible_views(db, user).await? {
        if !grouped.contains(&folder) {
            continue;
        }
        let folder_type = FerrofinItemRepository::collection_type_of(db, folder).await?;
        if folder_type.is_some() && folder_type.as_deref() != view_type {
            continue;
        }
        libraries.push(folder);
    }
    Ok(libraries)
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
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT "Id", "Type", "Path" FROM "BaseItems" WHERE "Type" IN (?1, ?2, ?3, ?4)"#,
    )
    .bind(collection_folder)
    .bind(user_view)
    .bind(playlists_folder)
    .bind(manual_playlists_folder)
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;

    // CollectionFolder.IsVisible checks Enabled even for administrators.
    // Read the saved option on every request, as the direct visibility path
    // does, so a dashboard toggle takes effect without a scan or restart.
    let mut active = Vec::with_capacity(rows.len());
    for row in rows {
        if row.1 != collection_folder || library_enabled(row.2.as_deref()).await {
            active.push(row);
        }
    }

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

    Ok(active
        .iter()
        .filter_map(|(id, type_, _)| {
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

/// The saved CollectionFolder option, including adopted XML and constructor
/// defaults. This read does not create files or repair the database.
async fn library_enabled(path: Option<&str>) -> bool {
    let Some(path) = path else {
        return true;
    };
    crate::virtual_folder_manager::FerrofinVirtualFolderManager::read_options(std::path::Path::new(
        path,
    ))
    .await
    .enabled
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

/// ICU's root secondary strength is invariant linguistic comparison ignoring
/// case while retaining accents. Only name filters require this data.
fn folder_name_collator(
    filter: &InternalItemsQuery,
) -> Result<Option<&'static icu_collator::CollatorBorrowed<'static>>, ServiceError> {
    use icu_collator::options::{CollatorOptions, Strength};
    static COLLATOR: std::sync::OnceLock<Result<icu_collator::CollatorBorrowed<'static>, String>> =
        std::sync::OnceLock::new();
    if ![
        &filter.name_starts_with,
        &filter.name_starts_with_or_greater,
        &filter.name_less_than,
    ]
    .iter()
    .any(|value| value.as_deref().is_some_and(|value| !value.is_empty()))
    {
        return Ok(None);
    }
    COLLATOR
        .get_or_init(|| {
            let mut options = CollatorOptions::default();
            options.strength = Some(Strength::Secondary);
            icu_collator::Collator::try_new(icu_collator::CollatorPreferences::default(), options)
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map(Some)
        .map_err(|error| ServiceError::backend(format!("folder name collation: {error}")))
}

fn folder_invariant_starts_with(
    collator: &icu_collator::CollatorBorrowed<'_>,
    value: &str,
    prefix: &str,
) -> bool {
    use unicode_normalization::UnicodeNormalization as _;
    let normalized: String = value.nfc().collect();
    // Compare complete canonical combining sequences, never a base character
    // cut off from its accent. SQL LIKE's '%' and '_' remain literal here.
    (0..=normalized.len())
        .filter(|end| normalized.is_char_boundary(*end))
        .filter(|end| {
            normalized[*end..]
                .chars()
                .next()
                .is_none_or(|next| !unicode_normalization::char::is_combining_mark(next))
        })
        .any(|end| collator.compare(&normalized[..end], prefix).is_eq())
}

fn folder_nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// The pinned SortNameComparer/NameComparer use ordinal case-insensitive
/// comparison, including supplementary scalar order. Keep equal keys stable,
/// with no generic SQL tiebreaker.
pub(crate) fn folder_ordinal_compare(
    left: Option<&str>,
    right: Option<&str>,
) -> std::cmp::Ordering {
    match (left, right) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(left), Some(right)) => upper_invariant(left).cmp(&upper_invariant(right)),
    }
}

pub(crate) fn folder_sort_name(row: &BaseItemEntity) -> String {
    let kind =
        crate::item_type_lookup::kind_from_type_name(&row.type_).unwrap_or(BaseItemKind::Folder);
    // BaseItemMapper assigns SortName then ForcedSortName; that setter clears
    // the cached key. A stale stored key must never mask a nonempty override.
    if let Some(forced) = row
        .forced_sort_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        return crate::kinds::forced_sort_name_for(kind, forced);
    }
    // With no stored key, the row's own `CreateSortName`: a season among its
    // siblings by number, never by title.
    row.sort_name
        .clone()
        .or_else(|| crate::kinds::fallback_sort_name(row))
        .unwrap_or_default()
}

fn folder_values_contain(values: Option<&str>, wanted: &str) -> bool {
    values.is_some_and(|values| {
        values
            .split('|')
            .filter(|value| !value.is_empty())
            .any(|value| equals_ordinal_ignore_case(value, wanted))
    })
}

fn folder_row_ids_json(rows: &[BaseItemEntity]) -> Result<String, ServiceError> {
    serde_json::to_string(&rows.iter().map(|row| &row.id).collect::<Vec<_>>())
        .map_err(|error| ServiceError::backend(error.to_string()))
}

// Mirrors the source's explicit property whitelist, not generic SQL query
// predicates. The length reflects independent optional source filters.
#[allow(clippy::too_many_lines)]
fn folder_child_matches_fields(
    row: &BaseItemEntity,
    filter: &InternalItemsQuery,
    collator: Option<&icu_collator::CollatorBorrowed<'_>>,
) -> bool {
    if let Some(collator) = collator {
        let sort_name = folder_sort_name(row);
        if folder_nonempty(filter.name_starts_with.as_deref())
            .is_some_and(|prefix| !folder_invariant_starts_with(collator, &sort_name, prefix))
            || folder_nonempty(filter.name_starts_with_or_greater.as_deref())
                .is_some_and(|bound| collator.compare(bound, &sort_name).is_gt())
            || folder_nonempty(filter.name_less_than.as_deref())
                .is_some_and(|bound| !collator.compare(bound, &sort_name).is_gt())
        {
            return false;
        }
    }
    let kind = crate::item_type_lookup::kind_from_type_name(&row.type_);
    if (!filter.media_types.is_empty()
        && !filter
            .media_types
            .iter()
            .any(|kind| media_type_name(*kind) == row.media_type.as_deref().unwrap_or("Unknown")))
        || (!filter.include_item_types.is_empty()
            && !kind.is_some_and(|kind| filter.include_item_types.contains(&kind)))
        || kind.is_some_and(|kind| filter.exclude_item_types.contains(&kind))
        || filter
            .is_virtual_item
            .is_some_and(|wanted| row.is_virtual_item != wanted)
        || filter
            .is_folder
            .is_some_and(|wanted| row.is_folder != wanted)
        || filter
            .is_locked
            .is_some_and(|wanted| row.is_locked != wanted)
        || filter.has_overview.is_some_and(|wanted| {
            row.overview
                .as_deref()
                .is_some_and(|value| !value.is_empty())
                != wanted
        })
    {
        return false;
    }
    if let Some(wanted) = filter.has_parental_rating {
        let rating = row
            .custom_rating
            .as_deref()
            .filter(|value| !value.is_empty())
            .or(row.official_rating.as_deref());
        if rating.is_some_and(|value| !value.is_empty()) != wanted {
            return false;
        }
    }
    if (!filter.genres.is_empty()
        && !filter
            .genres
            .iter()
            .any(|value| folder_values_contain(row.genres.as_deref(), value)))
        || (!filter.tags.is_empty()
            && !filter
                .tags
                .iter()
                .any(|value| folder_values_contain(row.tags.as_deref(), value)))
        || (!filter.years.is_empty()
            && !row
                .production_year
                .is_some_and(|value| filter.years.iter().any(|year| i64::from(*year) == value)))
        || (!filter.official_ratings.is_empty()
            && !filter
                .official_ratings
                .iter()
                .any(|value| value == row.official_rating.as_deref().unwrap_or_default()))
    {
        return false;
    }
    if (!filter.item_ids.is_empty()
        && !Uuid::parse_str(&row.id).is_ok_and(|id| filter.item_ids.contains(&id)))
        || Uuid::parse_str(&row.id).is_ok_and(|id| filter.exclude_item_ids.contains(&id))
        || filter
            .min_community_rating
            .is_some_and(|min| !row.community_rating.is_some_and(|value| value >= min))
        || filter
            .min_critic_rating
            .is_some_and(|min| !row.critic_rating.is_some_and(|value| value >= min))
        || filter
            .min_index_number
            .is_some_and(|min| row.index_number.is_none_or(|value| value < i64::from(min)))
        || filter
            .min_premiere_date
            .is_some_and(|min| row.premiere_date.is_none_or(|value| value < min))
        || filter
            .max_premiere_date
            .is_some_and(|max| row.premiere_date.is_none_or(|value| value > max))
        || filter.parent_index_number.is_some_and(|wanted| {
            row.parent_index_number
                .is_some_and(|value| value != i64::from(wanted))
        })
    {
        return false;
    }
    true
}

/// One of the user's guid-list folder preferences.
async fn folder_preference(
    user_db: &Database,
    user: &UserEntity,
    kind: PreferenceKind,
) -> Result<Vec<Uuid>, ServiceError> {
    guid_preference(user_db.pool(), &user.id, kind).await
}

/// `ManagedFileSystem.SwapFiles`: exchanges the contents of two files through a
/// temporary copy of the first. A failure part-way restores the first file.
fn swap_image_files(first: &std::path::Path, second: &std::path::Path) -> std::io::Result<()> {
    let temp = first.with_file_name(format!(".ferrofin-swap-{}", Uuid::new_v4().simple()));
    std::fs::copy(first, &temp)?;
    let result = std::fs::copy(second, first)
        .and_then(|_| std::fs::rename(&temp, second))
        .inspect_err(|_| {
            // Put the first file back; the temporary copy is its only other
            // holder of the original content.
            let _ = std::fs::copy(&temp, first);
        });
    let _ = std::fs::remove_file(&temp);
    result
}

/// `!ItemImageInfo.IsLocalFile`: upstream's case-insensitive `StartsWith("http")`.
fn is_remote_image_path(path: &str) -> bool {
    path.get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http"))
}

#[async_trait]
impl ItemRepository for FerrofinItemRepository {
    async fn filter_folder_children(
        &self,
        rows: Vec<BaseItemEntity>,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.filter_folder_children_inner(rows, filter).await
    }

    async fn sort_folder_children(
        &self,
        rows: Vec<BaseItemEntity>,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.sort_folder_children_inner(rows, filter).await
    }

    async fn visible_library_ids(&self, user: &UserEntity) -> Result<Vec<Uuid>, ServiceError> {
        visible_views(&self.db, user).await
    }

    async fn build_access_filter(
        &self,
        user: &UserEntity,
    ) -> Result<Option<InternalItemsQuery>, ServiceError> {
        let mut filter = InternalItemsQuery {
            include_owned_items: true,
            ..InternalItemsQuery::default()
        };
        filter.set_user(user.clone());
        // Restrictions first: they decide whether the views lookup is needed.
        let filter = crate::query_restrictions::resolve(&self.db, &filter)
            .await?
            .unwrap_or(filter);
        if !filter.user_has_content_restrictions {
            return Ok(None);
        }
        Ok(Some(
            scope_to_user_libraries(&self.db, &filter)
                .await?
                .unwrap_or(filter),
        ))
    }

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

    async fn retrieve_items(&self, ids: &[Uuid]) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let mut rows = Vec::new();
        for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = format!(
                r#"SELECT * FROM "BaseItems" WHERE "Id" IN ({})"#,
                placeholders(chunk.len())
            );
            let mut query = sqlx::query_as::<_, BaseItemEntity>(sqlx::AssertSqlSafe(sql.as_str()));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            rows.extend(query.fetch_all(self.db.pool()).await.map_err(db_err)?);
        }
        Ok(rows)
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
        let prepared = grouped_by_metadata_key(filter);
        let filter = prepared.as_ref().unwrap_or(filter);
        // C# `ItemsController.GetItems` does NOT query when the parent is the
        // user root and the request is neither recursive nor id-scoped — see
        // [`Self::user_root_children`].
        if let Some(children) = Box::pin(self.user_root_children(filter)).await? {
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
        let parent = self.query_parent_row(filter).await?;
        if let Some(result) = self.folders_view_result(filter, parent.as_ref()).await? {
            return Ok(result);
        }
        // Resolve once for both statements below: the page and its count share
        // one scope, and asking the database twice for the same library's
        // folders is a round trip on every paged browse.
        let resolved = self
            .resolve_views_with_parent(filter, parent.as_ref())
            .await?;
        let filter = resolved.as_ref().unwrap_or(filter);
        let start_index = filter.start_index.unwrap_or(0);
        if let Some(window) = self.adjacent_ids_resolved(filter).await? {
            // `GetItems` counts the trimmed set, then pages it.
            let window_len = window.len();
            let items = sql_page(self.rows_in_order(&window).await?, filter);
            let total = if filter.enable_total_record_count
                && (filter.limit.is_some() || start_index > 0)
            {
                i32::try_from(window_len).unwrap_or(i32::MAX)
            } else {
                i32::try_from(items.len()).unwrap_or(i32::MAX) + start_index
            };
            return Ok(QueryResult::new(Some(start_index), Some(total), items));
        }
        let items = self
            .fetch_rows_resolved(filter, QueryShape::FullRows)
            .await?;
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
        let prepared = grouped_by_metadata_key(filter);
        self.fetch_ids(prepared.as_ref().unwrap_or(filter)).await
    }

    async fn get_item_list(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let prepared = grouped_by_metadata_key(filter);
        self.fetch_rows(prepared.as_ref().unwrap_or(filter), QueryShape::FullRows)
            .await
    }

    async fn get_folder_children(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<FolderChild>, ServiceError> {
        // Resolve only the physical view membership. User-specific access is
        // checked together by the Folder's visibility evaluator afterwards.
        let scope = InternalItemsQuery {
            parent_id: query.parent_id,
            parent_type: query.parent_type,
            group_by_presentation_unique_key: false,
            ..InternalItemsQuery::default()
        };
        let resolved = self.resolve_views(&scope).await?;
        let scope = resolved.as_ref().unwrap_or(&scope);
        let mut builder = crate::translate_query::build_folder_children_query(
            scope.parent_id,
            &scope.parent_physical_folder_ids,
            query.user.is_some(),
        );
        builder
            .build_query_as::<FolderChildRow>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
            .map(|rows| {
                rows.into_iter()
                    .map(|row| FolderChild {
                        item: row.item,
                        linked: row.linked,
                    })
                    .collect()
            })
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
        // Ferrofin implements two arms of v12's `BaseItemRepository.
        // GetLatestItemList`: music (`Querying.cs:147-180`) and tvshows
        // (`GetLatestTvShowItems`, `:282-540`). Every other collection type
        // returns empty and the user-view manager sends it through
        // `get_item_list`. That is WIDER than the C# early exit
        // (`Querying.cs:112-116`, which only rejects types outside
        // `{movies, tvshows, music, unknown}`): v12's `movies` and `unknown`
        // arms are an owner-scoped divergence, recorded at the branch itself
        // in `user_view_manager::items_for_latest_items`.
        // Checked BEFORE `resolve_views` so the early exit costs no query.
        if !matches!(
            collection_type,
            CollectionType::tvshows | CollectionType::music
        ) {
            return Ok(Vec::new());
        }
        // This is the path jellyfin-web's per-library "Latest in …" rows take
        // (grouping is on by default for tvshows and music), so it needs the
        // same view translation as every other browse — without it, Latest for
        // a TV or Music library on an adopted database comes back empty.
        let prepared = grouped_by_metadata_key(filter);
        let filter = prepared.as_ref().unwrap_or(filter);
        let resolved = self.resolve_views(filter).await?;
        let filter = resolved.as_ref().unwrap_or(filter);
        if collection_type == CollectionType::tvshows {
            // One Episode, Season or Series card per series, the newest
            // `limit` series — see `latest_tv_repository`. A Season or Series
            // is a folder, so the caller lists it on its own; an Episode
            // groups under its series as before.
            return crate::latest_tv_repository::latest_tv_show_items(&self.db, filter).await;
        }
        // v12's music arm: the newest `limit` MusicAlbum rows themselves,
        // one statement — which reads no track at all when the caller is
        // library-scoped, and runs the caller's track query only as the
        // ancestor fallback's inner subquery. The rows come back already
        // "grouped" — an album carries no `LatestItemsIndexContainer`, so
        // the caller's grouping pass is a no-op over them.
        let mut qb = build_latest_music_albums_query(filter);
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

    async fn get_extra_owner_ids_batch(
        &self,
        items: &[BaseItemEntity],
        grouped_series: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<Uuid>>, ServiceError> {
        crate::extra_owners_repository::get_extra_owner_ids_batch(&self.db, items, grouped_series)
            .await
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

    async fn get_merged_version_ids(&self, primary_id: Uuid) -> Result<Vec<Uuid>, ServiceError> {
        // `IX_LinkedChildren_ParentId_ChildType` for the rows, a
        // `PrimaryVersionId` scan for the pointers (as
        // `get_items_by_primary_version`).
        let ids: Vec<String> = sqlx::query_scalar(
            r#"SELECT "ChildId" FROM "LinkedChildren" WHERE "ParentId" = ?1 AND "ChildType" = 3
               UNION
               SELECT b."Id" FROM "BaseItems" b
               WHERE b."PrimaryVersionId" = ?1 AND b."Id" <> ?2
                 AND NOT (b."OwnerId" IS NOT NULL AND b."OwnerId" = ?1 AND b."ExtraType" IS NULL)
                 AND NOT EXISTS (SELECT 1 FROM "LinkedChildren" l
                                 WHERE l."ParentId" = ?1 AND l."ChildId" = b."Id"
                                   AND l."ChildType" = 2)"#,
        )
        .bind(guid_to_db(primary_id))
        .bind(PLACEHOLDER_ID)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(ids
            .iter()
            .filter_map(|id| Uuid::parse_str(id).ok())
            .collect())
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
            let mut query = sqlx::query_as::<_, BaseItemEntity>(sqlx::AssertSqlSafe(sql));
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

    async fn get_items_by_primary_version_batch_flagged(
        &self,
        primary_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<(BaseItemEntity, bool)>>, ServiceError> {
        rows_by_primary_flagged(&self.db, primary_ids).await
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
        // Within each type use insertion order, as BaseItem.ImageInfos does.
        // Random GUID order changes a newly appended backdrop's public index.
        let rows = sqlx::query_as::<_, BaseItemImageInfoEntity>(
            r#"SELECT * FROM "BaseItemImageInfos" WHERE "ItemId" = ?1
                ORDER BY "ImageType", rowid"#,
        )
        .bind(guid_to_db(item_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|row| image_info_from_row(row, &self.virtual_paths))
            .collect())
    }

    async fn get_image_infos_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<ItemImageInfo>>, ServiceError> {
        let mut map: std::collections::HashMap<Uuid, Vec<ItemImageInfo>> =
            std::collections::HashMap::with_capacity(item_ids.len());
        for chunk in item_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let mut query = sqlx::query_as::<_, BaseItemImageInfoEntity>(sqlx::AssertSqlSafe(
                images_of_items_sql(&placeholders),
            ));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for row in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Ok(item_id) = Uuid::parse_str(&row.item_id) {
                    map.entry(item_id)
                        .or_default()
                        .push(image_info_from_row(row, &self.virtual_paths));
                }
            }
        }
        Ok(map)
    }

    async fn get_image_infos_with_auto_orient(
        &self,
        item_id: Uuid,
    ) -> Result<(Vec<ItemImageInfo>, bool), ServiceError> {
        /// An image row with its owner's photo facts.
        #[derive(sqlx::FromRow)]
        struct Row {
            #[sqlx(flatten)]
            image: BaseItemImageInfoEntity,
            #[sqlx(rename = "IsPhoto")]
            is_photo: bool,
            #[sqlx(rename = "PhotoData")]
            photo_data: Option<String>,
        }
        // The image read of `get_image_infos`, plus the owner by primary key.
        let rows = sqlx::query_as::<_, Row>(
            r#"SELECT ii.*, bi."Type" = ?2 AS "IsPhoto",
                    CASE WHEN bi."Type" = ?2 THEN bi."Data" END AS "PhotoData"
                FROM "BaseItemImageInfos" ii
                JOIN "BaseItems" bi ON bi."Id" = ii."ItemId"
                WHERE ii."ItemId" = ?1
                ORDER BY ii."ImageType", ii.rowid"#,
        )
        .bind(guid_to_db(item_id))
        .bind(BaseItemKind::Photo.stored_type_name())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        // Every row has the same owner, so the first one speaks for all.
        let auto_orient = rows.first().is_some_and(|row| {
            row.is_photo
                && crate::kinds::photo_auto_orients(
                    crate::item_data::read_data_string(
                        &crate::item_data::parse_data(row.photo_data.as_deref()),
                        "Orientation",
                    )
                    .as_deref(),
                )
        });
        let images = rows
            .into_iter()
            .map(|row| image_info_from_row(row.image, &self.virtual_paths))
            .collect();
        Ok((images, auto_orient))
    }

    async fn swap_item_images(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        index1: i32,
        index2: i32,
    ) -> Result<bool, ServiceError> {
        // Load this item's rows for the requested type in the same stable order
        // get_image_infos exposes, so the caller's 0-based indices address the
        // same images the read side does.
        let disc = image_type_to_disc(image_type);
        let rows = sqlx::query_as::<_, BaseItemImageInfoEntity>(
            r#"SELECT * FROM "BaseItemImageInfos" WHERE "ItemId" = ?1 AND "ImageType" = ?2
                ORDER BY rowid"#,
        )
        .bind(guid_to_db(item_id))
        .bind(disc)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        // Out-of-range indices are a no-op — the C# `GetImageInfo` returns null and
        // SwapImagesAsync bails with "nothing to do".
        let (Ok(i1), Ok(i2)) = (usize::try_from(index1), usize::try_from(index2)) else {
            return Ok(false);
        };
        let (Some(first), Some(second)) = (rows.get(i1), rows.get(i2)) else {
            return Ok(false);
        };
        // `!info.IsLocalFile` on either side: "Not supported yet", nothing done.
        if is_remote_image_path(&first.path) || is_remote_image_path(&second.path) {
            return Ok(false);
        }

        // The same file under two rows (or one slot against itself) has nothing
        // to exchange; upstream's copy onto itself would throw.
        if first.path == second.path {
            return Ok(false);
        }
        // `SwapImagesAsync` swaps the CONTENTS of the two files — the rows keep
        // their paths — and then clears the cached dimensions and re-reads each
        // modified time from its file.
        let (path1, path2) = (first.path.clone(), second.path.clone());
        let modified = tokio::task::spawn_blocking(move || {
            swap_image_files(std::path::Path::new(&path1), std::path::Path::new(&path2))?;
            let stamp = |path: &str| {
                std::fs::metadata(path)
                    .and_then(|meta| meta.modified())
                    .map_or_else(|_| Utc::now(), chrono::DateTime::<Utc>::from)
            };
            Ok::<_, std::io::Error>((stamp(&path1), stamp(&path2)))
        })
        .await
        .map_err(|error| ServiceError::backend(error.to_string()))?
        .map_err(|error| ServiceError::backend(format!("swap image files: {error}")))?;
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        for (row, modified) in [(first, modified.0), (second, modified.1)] {
            // The blur hash described the old content, so it goes too.
            sqlx::query(
                r#"UPDATE "BaseItemImageInfos"
                    SET "Width" = 0, "Height" = 0, "Blurhash" = NULL, "DateModified" = ?2
                    WHERE "Id" = ?1"#,
            )
            .bind(&row.id)
            .bind(datetime_to_db(modified))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(true)
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
        let mut by_type = self
            .get_media_stream_languages_by_type(filter, &[stream_type])
            .await?;
        Ok(by_type.remove(&stream_type).unwrap_or_default())
    }

    async fn get_media_stream_languages_by_type(
        &self,
        filter: &InternalItemsQuery,
        stream_types: &[MediaStreamType],
    ) -> Result<std::collections::HashMap<MediaStreamType, Vec<String>>, ServiceError> {
        let mut out: std::collections::HashMap<MediaStreamType, Vec<String>> =
            stream_types.iter().map(|&t| (t, Vec::new())).collect();
        if stream_types.is_empty() {
            return Ok(out);
        }
        // `BaseItemRepository.GetMediaStreamLanguages` (v12.2 `ByName.cs:87-116`)
        // runs a fresh `InternalItemsQuery(filter.User)` carrying only the scope
        // and kind fields, after `LibraryManager` applied `AddUserToQuery` and
        // `SetTopParentOrAncestorIds` (`LibraryManager.cs:4221-4230`) — which is
        // what `resolve_views` does here, with the user's restrictions.
        let narrowed = stream_language_query(filter);
        let scoped = self.resolve_views(&narrowed).await?;
        let filter = scoped.as_ref().unwrap_or(&narrowed);
        // Map disc -> type so the rows sort back into per-type lists.
        let by_disc: std::collections::HashMap<i64, MediaStreamType> = stream_types
            .iter()
            .map(|&t| (i64::from(media_stream_type_disc(t)), t))
            .collect();
        let mut qb = stream_languages_statement(filter, by_disc.keys().copied());
        let rows: Vec<(i64, String)> = qb
            .build_query_as()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        for (disc, lang) in rows {
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
        // offering a choice that returns nothing. The views are resolved as a
        // browse resolves them (which scopes an unparented query to the
        // user's libraries): a library's children hang off its locations'
        // folders, so `parentId={library}` alone matches none of them.
        let scoped = self.resolve_views(filter).await?;
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
        // it does share that method's view resolution and user scoping, which
        // have to be applied here too.
        let scoped = self.resolve_views(filter).await?;
        self.distinct_years(scoped.as_ref().unwrap_or(filter)).await
    }

    async fn get_tag_names(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        // `BaseItemRepository.GetTagNames` (v12.2 `ByName.cs:121-147`) takes the
        // `Tags` values of the items `TranslateQuery` matches, grouped by
        // `CleanValue`, `MIN(Value)` per group, ordered by that value: the
        // legacy facet's own statement. `LibraryManager.GetTagNames` applies
        // `AddUserToQuery` and `SetTopParentOrAncestorIds` first; the view
        // resolution here applies the user's libraries as the other facets
        // do. TODO(gl-807): it does not rewrite an all-view/library ancestor
        // scope to `TopParentIds` as `SetTopParentOrAncestorIds` does.
        let scoped = self.resolve_views(filter).await?;
        self.distinct_item_values(scoped.as_ref().unwrap_or(filter), ItemValueType::Tags)
            .await
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
        let mut filter = InternalItemsQuery::default();
        filter.set_user(user.clone());
        let filter = crate::query_restrictions::resolve(&self.db, &filter)
            .await?
            .unwrap_or(filter);
        let mut qb = QueryBuilder::new(r#"SELECT NOT EXISTS (SELECT 1 FROM "BaseItems" bi "#);
        if recursive {
            qb.push(r#"JOIN "AncestorIds" a ON a."ItemId" = bi."Id" WHERE a."ParentItemId" = "#);
        } else {
            qb.push(r#"WHERE bi."ParentId" = "#);
        }
        qb.push_bind(guid_to_db(id))
            .push(r#" AND bi."IsFolder" = 0 AND bi."IsVirtualItem" = 0"#);
        crate::translate_query::append_parental_restrictions(&mut qb, &filter, "bi");
        qb.push(r#" AND NOT EXISTS (SELECT 1 FROM "UserData" ud WHERE ud."ItemId" = bi."Id" AND ud."UserId" = "#)
            .push_bind(user.id.clone()).push(r#" AND ud."Played" = 1))"#);
        let all_played: i64 = qb
            .build_query_scalar()
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

/// The distinct `(StreamType, Language)` pairs of the streams of `discs` on the
/// items `filter` matches (v12.2 `BaseItemRepository.ByName.cs:110-115`).
///
/// Upstream runs only `TranslateQuery` — no PresentationUniqueKey grouping,
/// ordering or paging — so the filter is this statement's own sub-select rather
/// than `build_query`'s grouped id list: grouping keeps one version per title and
/// would drop the languages only an alternate version carries, which is what
/// `IncludeOwnedItems` asks for. A null or empty language is `und` (C#
/// `string.IsNullOrEmpty`, so whitespace is kept as is); the DISTINCT is SQLite's
/// BINARY comparison, as EF's `Distinct()` is.
fn stream_languages_statement(
    filter: &InternalItemsQuery,
    discs: impl IntoIterator<Item = i64>,
) -> QueryBuilder<Sqlite> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        r#"SELECT DISTINCT ms."StreamType",
             CASE WHEN ms."Language" IS NULL OR ms."Language" = '' THEN 'und'
                  ELSE ms."Language" END
           FROM "MediaStreamInfos" AS ms WHERE ms."StreamType" IN ("#,
    );
    {
        let mut separated = qb.separated(", ");
        for disc in discs {
            separated.push_bind(disc);
        }
    }
    qb.push(r#") AND ms."ItemId" IN (SELECT bi."Id" FROM "BaseItems" AS bi WHERE bi."Id" <> "#);
    qb.push_bind(PLACEHOLDER_ID);
    append_predicates(&mut qb, filter);
    qb.push(")");
    qb
}

/// The query `BaseItemRepository.GetMediaStreamLanguages` actually runs: a fresh
/// `InternalItemsQuery(filter.User)` (so the user's parental and tag restrictions
/// apply) carrying only `IncludeOwnedItems`, the item-kind and media-type sets,
/// the ancestor/item/top-parent/parent scope and the live-TV `Is*` flags
/// (v12.2 `BaseItemRepository.ByName.cs:92-108`). Everything else a caller set
/// is dropped, as upstream drops it.
fn stream_language_query(filter: &InternalItemsQuery) -> InternalItemsQuery {
    let mut query = InternalItemsQuery::default();
    if let Some(user) = &filter.user {
        query.set_user(user.clone());
    }
    query.include_owned_items = filter.include_owned_items;
    query
        .exclude_item_types
        .clone_from(&filter.exclude_item_types);
    query
        .include_item_types
        .clone_from(&filter.include_item_types);
    query.media_types.clone_from(&filter.media_types);
    query.ancestor_ids.clone_from(&filter.ancestor_ids);
    query.item_ids.clone_from(&filter.item_ids);
    query.top_parent_ids.clone_from(&filter.top_parent_ids);
    query.parent_id = filter.parent_id;
    query.physical_children_only = filter.physical_children_only;
    query.is_airing = filter.is_airing;
    query.is_movie = filter.is_movie;
    query.is_sports = filter.is_sports;
    query.is_kids = filter.is_kids;
    query.is_news = filter.is_news;
    query.is_series = filter.is_series;
    query
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
        item_values_statement(filter, value_type)
            .build_query_scalar()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
    }
}

/// The statement [`FerrofinItemRepository::distinct_item_values`] runs: one
/// `MIN("Value")` per `CleanValue` of `value_type`, over the items `filter`
/// matches, ordered by that value.
///
/// `CROSS JOIN` pins the join order to iv → ivm → bi: the values of the type,
/// their map rows by the map's primary key, the item by its own. Left to
/// itself the planner — with no `sqlite_stat1` to tell it the user-scope
/// `TopParentId IN (…)` matches every item there is — pairs every value with
/// every item under the user's libraries and probes the map for each pair:
/// 38 genres × 21k items ≈ 800k probes, 820 ms for the genre facet of
/// `/Items/Filters` on the bench library; pinned, it is one probe per map row
/// of the type. `item_values_statement_walks_values_then_map_then_items` pins it.
fn item_values_statement(
    filter: &InternalItemsQuery,
    value_type: ItemValueType,
) -> QueryBuilder<Sqlite> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        r#"SELECT MIN(iv."Value") FROM "ItemValues" AS iv
           CROSS JOIN "ItemValuesMap" ivm ON ivm."ItemValueId" = iv."ItemValueId"
           CROSS JOIN "BaseItems" AS bi ON bi."Id" = ivm."ItemId"
           WHERE iv."Type" = "#,
    );
    qb.push_bind(i64::from(i32::from(value_type)));
    qb.push(r#" AND bi."Id" <> "#);
    qb.push_bind(PLACEHOLDER_ID);
    append_predicates(&mut qb, filter);
    qb.push(r#" GROUP BY iv."CleanValue" ORDER BY MIN(iv."Value")"#);
    qb
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

/// `SELECT *` of the `BaseItems` rows whose `column` is one of `ids`,
/// chunked under the bind ceiling — the version-group reads
/// ([`crate::video_versions::DbVersionReader`]).
pub(crate) async fn select_item_rows(
    db: &Database,
    column: &str,
    ids: &[Uuid],
) -> Result<Vec<BaseItemEntity>, ServiceError> {
    let mut conn = db.pool().acquire().await.map_err(db_err)?;
    select_item_rows_on(&mut conn, column, ids).await
}

/// [`select_item_rows`] on a given connection — inside a transaction, the
/// owned-video copy reads what it is about to write.
pub(crate) async fn select_item_rows_on(
    conn: &mut sqlx::SqliteConnection,
    column: &str,
    ids: &[Uuid],
) -> Result<Vec<BaseItemEntity>, ServiceError> {
    let mut rows = Vec::new();
    for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let marks = (1..=chunk.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(r#"SELECT * FROM "BaseItems" WHERE {column} IN ({marks})"#);
        let mut query = sqlx::query_as::<_, BaseItemEntity>(sqlx::AssertSqlSafe(sql));
        for id in chunk {
            query = query.bind(guid_to_db(*id));
        }
        rows.extend(query.fetch_all(&mut *conn).await.map_err(db_err)?);
    }
    Ok(rows)
}

/// A row and whether some other row names it as its `PrimaryVersionId`.
#[derive(sqlx::FromRow)]
struct FlaggedRow {
    #[sqlx(flatten)]
    row: BaseItemEntity,
    #[sqlx(rename = "HasVersions")]
    has_versions: bool,
}

/// The statement [`rows_by_primary_flagged`] runs over `n` primary ids: the
/// rows pointing at them (never the placeholder row, never one pointing at
/// itself), each with whether a row points at IT — an `EXISTS` the partial
/// `IX_BaseItems_PrimaryVersionId` answers by one seek per row.
pub(crate) fn rows_by_primary_flagged_sql(n: usize) -> String {
    format!(
        r#"SELECT b.*, EXISTS (SELECT 1 FROM "BaseItems" x
                               WHERE x."PrimaryVersionId" = b."Id" AND x."Id" <> b."Id")
                   AS "HasVersions"
           FROM "BaseItems" b
           WHERE b."PrimaryVersionId" IN ({}) AND b."Id" <> ?{} AND b."Id" <> b."PrimaryVersionId""#,
        placeholders(n),
        n + 1
    )
}

/// The rows naming each of `primary_ids` as their `PrimaryVersionId`, by
/// that id, each with whether it has versions of its own — one query per
/// chunk ([`rows_by_primary_flagged_sql`]).
pub(crate) async fn rows_by_primary_flagged(
    db: &Database,
    primary_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<(BaseItemEntity, bool)>>, ServiceError> {
    let mut map: HashMap<Uuid, Vec<(BaseItemEntity, bool)>> = HashMap::new();
    for chunk in primary_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let sql = rows_by_primary_flagged_sql(chunk.len());
        let mut query = sqlx::query_as::<_, FlaggedRow>(sqlx::AssertSqlSafe(sql));
        for id in chunk {
            query = query.bind(guid_to_db(*id));
        }
        query = query.bind(PLACEHOLDER_ID);
        for flagged in query.fetch_all(db.pool()).await.map_err(db_err)? {
            if let Some(primary) = flagged
                .row
                .primary_version_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                map.entry(primary)
                    .or_default()
                    .push((flagged.row, flagged.has_versions));
            }
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::test_support::{
        seed_child_item, seed_folder_item, seed_item, seed_item_genre, seed_item_of_series,
        seed_item_value, seed_item_with_data, seed_library_over, seed_named_item,
        seed_top_parented_item, seed_user, seed_user_data, seed_user_with_defaults, set_clean_name,
        test_db,
    };
    use ferrofin_db::Database;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::ExtraType;
    use ferrofin_traits::persistence::ItemPersistenceService;

    /// The batched image read answers each item exactly as `get_image_infos`
    /// does (by type, then insertion order), and leaves out an item with no
    /// images.
    #[tokio::test]
    async fn image_infos_batch_matches_the_per_item_read() {
        use crate::item_persistence_service::FerrofinItemPersistenceService;
        use ferrofin_model::entities::ImageType;
        use ferrofin_traits::options::ItemImageInfo;

        let db = test_db().await;
        let repo = repo(&db);
        let store = FerrofinItemPersistenceService::new(db.clone());
        let (first, second, bare) = (
            Uuid::from_u128(0x9a01),
            Uuid::from_u128(0x9a02),
            Uuid::from_u128(0x9a03),
        );
        let image = |path: &str, image_type| ItemImageInfo {
            path: path.to_owned(),
            image_type,
            date_modified: chrono::Utc::now(),
            width: 0,
            height: 0,
            blur_hash: None,
        };
        for (id, name) in [(first, "First"), (second, "Second"), (bare, "Bare")] {
            seed_named_item(&db, id, BaseItemKind::MusicAlbum, name).await;
        }
        store
            .save_item_images(
                first,
                &[
                    image("/m/a/backdrop.jpg", ImageType::Backdrop),
                    image("/m/a/folder.jpg", ImageType::Primary),
                    image("/m/a/backdrop2.jpg", ImageType::Backdrop),
                ],
            )
            .await
            .unwrap();
        store
            .save_item_images(second, &[image("/m/b/cover.jpg", ImageType::Primary)])
            .await
            .unwrap();

        let batched = repo
            .get_image_infos_batch(&[first, second, bare])
            .await
            .unwrap();
        assert_eq!(batched.len(), 2, "no entry for an item with no images");
        for id in [first, second] {
            let one = repo.get_image_infos(id).await.unwrap();
            let paths = |images: &[ItemImageInfo]| {
                images.iter().map(|i| i.path.clone()).collect::<Vec<_>>()
            };
            assert_eq!(paths(&batched[&id]), paths(&one), "{id}");
        }
    }

    /// A page's artist and music-video album links resolve through one
    /// `(Type, CleanName)` index search per chunk ([`item_ids_by_clean_name`]),
    /// never a scan of `BaseItems`.
    #[tokio::test]
    async fn item_ids_by_clean_name_seeks_the_type_clean_name_index() {
        let db = test_db().await;
        let sql = format!("EXPLAIN QUERY PLAN {}", item_ids_by_clean_name_sql(2));
        let plan: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind("MediaBrowser.Controller.Entities.Audio.MusicAlbum")
            .bind("a")
            .bind("b")
            .fetch_all(db.pool())
            .await
            .expect("plan")
            .iter()
            .map(|row| sqlx::Row::get::<String, _>(row, "detail"))
            .collect();
        assert!(
            plan.iter()
                .any(|detail| detail.starts_with("SEARCH BaseItems USING")
                    && detail.contains("Type_CleanName")
                    && detail.contains("(Type=? AND CleanName=?)")),
            "{plan:#?}"
        );
        assert!(
            !plan
                .iter()
                .any(|detail| detail.starts_with("SCAN BaseItems")),
            "{plan:#?}"
        );
    }

    /// A page's image read comes off the `(ItemId, ImageType)` index already
    /// in `rowid` order; ordering by `Id` added a temp B-tree sort.
    #[tokio::test]
    async fn page_image_read_needs_no_sort() {
        let db = test_db().await;
        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {}",
            images_of_items_sql("?1,?2")
        )))
        .fetch_all(db.pool())
        .await
        .expect("plan");
        let details: Vec<&str> = plan.iter().map(|r| r.3.as_str()).collect();
        assert!(
            details.iter().any(|d| d.contains("_ItemId_ImageType")),
            "{details:?}"
        );
        assert!(
            !details.iter().any(|d| d.contains("TEMP B-TREE")),
            "{details:?}"
        );
    }

    #[tokio::test]
    async fn plain_folder_children_keep_physical_order_then_every_link_kind() {
        use ferrofin_traits::persistence::LinkedChildrenService;
        let db = test_db().await;
        let repository = repo(&db);
        let folder = Uuid::new_v4();
        seed_folder_item(&db, folder, BaseItemKind::Folder, "Folder", None).await;
        let mut ids = Vec::new();
        for (name, physical) in [
            ("Zulu physical", true),
            ("Bravo physical", true),
            ("Alpha shortcut", false),
            ("Charlie local alternate", false),
            ("Delta linked alternate", false),
        ] {
            let id = Uuid::new_v4();
            seed_named_item(&db, id, BaseItemKind::Movie, name).await;
            let mut row = repository.retrieve_item(id).await.unwrap().unwrap();
            row.sort_name = Some(name.to_lowercase());
            if physical {
                row.parent_id = Some(guid_to_db(folder));
            } else if name == "Charlie local alternate" {
                // An existing alternate is hidden by the cached physical
                // query, then restored by its all-kind linked membership.
                row.parent_id = Some(guid_to_db(folder));
                row.primary_version_id = Some(guid_to_db(ids[0]));
            }
            crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone())
                .save_items(&[row])
                .await
                .unwrap();
            ids.push(id);
        }
        let links = crate::linked_children_service::FerrofinLinkedChildrenService::new(db.clone());
        // Link order differs from SortName; physical duplication must neither
        // move the child nor cause its access to become linked-only.
        for (index, kind) in [(0, 0), (2, 1), (4, 3), (3, 2)] {
            links
                .upsert_linked_child(folder, ids[index], kind)
                .await
                .unwrap();
        }
        let query = InternalItemsQuery {
            parent_id: folder,
            user: Some(seed_user_with_defaults(&db, Uuid::new_v4()).await),
            limit: Some(1),
            start_index: Some(99),
            include_item_types: vec![BaseItemKind::Audio],
            ..Default::default()
        };
        let rows = repository.get_folder_children(&query).await.unwrap();
        assert_eq!(
            rows.iter()
                .map(|child| child.item.name.as_deref().unwrap())
                .collect::<Vec<_>>(),
            [
                "Bravo physical",
                "Zulu physical",
                "Alpha shortcut",
                "Delta linked alternate",
                "Charlie local alternate"
            ]
        );
        assert_eq!(
            rows.iter().map(|child| child.linked).collect::<Vec<_>>(),
            [false, false, true, true, true]
        );
        let no_user = InternalItemsQuery {
            user: None,
            ..query
        };
        let rows = repository.get_folder_children(&no_user).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|child| !child.linked));
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one query fixture verifies substitution before sorting, adjacency, counts and paging"
    )]
    async fn collection_collapse_substitutes_before_count_order_page_and_letter_filters() {
        use ferrofin_model::{dto::SortOrder, live_tv::ItemSortBy};
        let db = test_db().await;
        let repository = repo(&db);
        let library = Uuid::new_v4();
        seed_folder_item(
            &db,
            library,
            BaseItemKind::CollectionFolder,
            "Library",
            None,
        )
        .await;
        let member = Uuid::new_v4();
        let series = Uuid::new_v4();
        let other = Uuid::new_v4();
        let set = Uuid::new_v4();
        let second_set = Uuid::new_v4();
        let shortcut_set = Uuid::new_v4();
        let orphan = Uuid::new_v4();
        for (id, kind, name) in [
            (member, BaseItemKind::Movie, "Zulu movie"),
            (series, BaseItemKind::Series, "Series"),
            (other, BaseItemKind::Movie, "Bravo movie"),
            (set, BaseItemKind::BoxSet, "Alpha set"),
            (second_set, BaseItemKind::BoxSet, "Charlie set"),
            (shortcut_set, BaseItemKind::BoxSet, "Shortcut set"),
            (orphan, BaseItemKind::BoxSet, "Orphan set"),
        ] {
            seed_top_parented_item(&db, id, kind, name, library).await;
            sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = ? WHERE "Id" = ?"#)
                .bind(name.to_lowercase())
                .bind(guid_to_db(id))
                .execute(db.writer())
                .await
                .unwrap();
            set_clean_name(&db, id, name).await;
        }
        // Collections can live outside the candidate library and still replace members.
        for collection in [set, second_set, shortcut_set] {
            sqlx::query(r#"UPDATE "BaseItems" SET "TopParentId" = NULL WHERE "Id" = ?"#)
                .bind(guid_to_db(collection))
                .execute(db.writer())
                .await
                .unwrap();
        }
        for (parent, child, kind, order) in [
            (set, member, 0, 0),
            (second_set, member, 0, 0),
            (set, series, 0, 1),
            (shortcut_set, other, 1, 0),
        ] {
            sqlx::query(r#"INSERT INTO "LinkedChildren" ("ParentId", "ChildId", "ChildType", "SortOrder") VALUES (?, ?, ?, ?)"#)
                .bind(guid_to_db(parent)).bind(guid_to_db(child)).bind(kind).bind(order).execute(db.writer()).await.unwrap();
        }
        let query = InternalItemsQuery {
            top_parent_ids: vec![library],
            include_item_types: vec![BaseItemKind::Movie, BaseItemKind::Series],
            collapse_box_set_items: Some(true),
            collapse_box_set_item_types: vec![BaseItemKind::Movie],
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            ..Default::default()
        };
        let names = |result: QueryResult<BaseItemEntity>| {
            result
                .items
                .into_iter()
                .map(|item| item.name.unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(repository.get_items(&query).await.unwrap()),
            ["Alpha set", "Bravo movie", "Charlie set", "Series"]
        );
        let page = InternalItemsQuery {
            start_index: Some(1),
            limit: Some(2),
            ..query.clone()
        };
        let result = repository.get_items(&page).await.unwrap();
        assert_eq!(result.total_record_count, 4);
        assert_eq!(names(result), ["Bravo movie", "Charlie set"]);
        let beyond = repository
            .get_items(&InternalItemsQuery {
                start_index: Some(99),
                limit: Some(1),
                ..query.clone()
            })
            .await
            .unwrap();
        assert_eq!(beyond.total_record_count, 4);
        assert!(beyond.items.is_empty());
        // `AdjacentTo` trims the collapsed result: a substituted set has its
        // neighbours there, and a member the set replaced has none.
        let around = |id: Uuid| InternalItemsQuery {
            adjacent_to: Some(id),
            ..query.clone()
        };
        assert_eq!(
            names(repository.get_items(&around(second_set)).await.unwrap()),
            ["Bravo movie", "Charlie set", "Series"]
        );
        assert!(
            repository
                .get_items(&around(member))
                .await
                .unwrap()
                .items
                .is_empty()
        );
        let alpha = InternalItemsQuery {
            name_starts_with: Some("A".into()),
            ..query.clone()
        };
        assert_eq!(
            names(repository.get_items(&alpha).await.unwrap()),
            ["Alpha set"],
            "recursive letter filter selects the collection name, not member name"
        );
        assert_eq!(repository.get_item_ids(&alpha).await.unwrap(), [set]);
        let clean = repository.get_item_id_clean_names(&alpha).await.unwrap();
        assert_eq!(
            clean,
            [(
                guid_to_db(set),
                Some(crate::text_util::get_clean_value("Alpha set"))
            )]
        );
        let bounds = InternalItemsQuery {
            name_starts_with_or_greater: Some("B".into()),
            name_less_than: Some("D".into()),
            ..query.clone()
        };
        assert_eq!(
            names(repository.get_items(&bounds).await.unwrap()),
            ["Bravo movie", "Charlie set"]
        );
        let typed_with_set = InternalItemsQuery {
            include_item_types: Vec::new(),
            ..query.clone()
        };
        assert!(
            names(repository.get_items(&typed_with_set).await.unwrap())
                .contains(&"Orphan set".to_owned()),
            "typed collapse preserves noncollapsible BoxSet candidates"
        );
        let all = InternalItemsQuery {
            include_item_types: Vec::new(),
            collapse_box_set_item_types: Vec::new(),
            ..query.clone()
        };
        assert_eq!(
            names(repository.get_items(&all).await.unwrap()),
            ["Alpha set", "Bravo movie", "Charlie set"],
            "empty type set collapses every linked type and omits standalone candidate BoxSets"
        );
        let mut literal_set = repository.retrieve_item(set).await.unwrap().unwrap();
        literal_set.sort_name = Some("alpha%set".into());
        crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone())
            .save_items(&[literal_set])
            .await
            .unwrap();
        assert!(
            repository
                .get_items(&InternalItemsQuery {
                    name_starts_with: Some("Alpha_".into()),
                    ..query.clone()
                })
                .await
                .unwrap()
                .items
                .is_empty(),
            "letter filtering must not treat '_' as a wildcard"
        );
        assert_eq!(
            repository
                .get_item_ids(&InternalItemsQuery {
                    name_starts_with: Some("Alpha%".into()),
                    ..query
                })
                .await
                .unwrap(),
            [set]
        );
    }

    #[rstest::rstest]
    #[case("own")]
    #[case("series")]
    #[case("ancestor")]
    #[case("top_parent")]
    #[tokio::test]
    async fn saved_tag_policy_filters_each_inherited_source(#[case] source: &str) {
        let db = test_db().await;
        let repository = repo(&db);
        let user = seed_user_with_defaults(&db, Uuid::new_v4()).await;
        let tagged = Uuid::new_v4();
        let plain = Uuid::new_v4();
        let parent = Uuid::new_v4();
        seed_named_item(&db, plain, BaseItemKind::Movie, "Plain").await;
        seed_named_item(&db, parent, BaseItemKind::Series, "Tag source").await;
        match source {
            "series" => {
                seed_item_of_series(&db, tagged, BaseItemKind::Movie, "Tagged", parent).await;
            }
            "top_parent" => {
                seed_top_parented_item(&db, tagged, BaseItemKind::Movie, "Tagged", parent).await;
            }
            _ => seed_named_item(&db, tagged, BaseItemKind::Movie, "Tagged").await,
        }
        if source == "ancestor" {
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?, ?)"#)
                .bind(guid_to_db(tagged))
                .bind(guid_to_db(parent))
                .execute(db.writer())
                .await
                .unwrap();
        }
        seed_item_value(
            &db,
            if source == "own" { tagged } else { parent },
            ItemValueType::Tags,
            "Café",
        )
        .await;
        let filter = InternalItemsQuery {
            user: Some(user.clone()),
            item_ids: vec![tagged, plain],
            group_by_presentation_unique_key: false,
            ..Default::default()
        };
        for (blocked, allowed, expected) in [
            (vec![], vec![], vec![tagged, plain]),
            (vec!["CAFE"], vec![], vec![plain]),
            (vec![], vec!["café"], vec![tagged]),
            (vec!["Café"], vec!["cafe"], vec![]),
            (vec![" "], vec!["\t"], vec![tagged, plain]),
            (vec![], vec![], vec![tagged, plain]),
        ] {
            for (kind, values) in [
                (PreferenceKind::BlockedTags, &blocked),
                (PreferenceKind::AllowedTags, &allowed),
            ] {
                crate::user_entity_ext::set_preference(
                    db.writer(),
                    &user.id,
                    kind,
                    &values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>(),
                )
                .await
                .unwrap();
            }
            for has_parental_rating in [None, Some(true)] {
                let result = repository
                    .get_items(&InternalItemsQuery {
                        has_parental_rating,
                        ..filter.clone()
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    result.total_record_count,
                    i32::try_from(expected.len()).unwrap(),
                    "{source} {blocked:?} {allowed:?}"
                );
                assert_eq!(result.items.len(), expected.len());
                for id in &expected {
                    assert!(result.items.iter().any(|row| row.id == guid_to_db(*id)));
                }
            }
        }
    }

    #[tokio::test]
    async fn person_allow_tag_exception_does_not_override_blocked_tags() {
        let db = test_db().await;
        let repository = repo(&db);
        let person = Uuid::new_v4();
        seed_named_item(&db, person, BaseItemKind::Person, "Cast member").await;
        let filter = InternalItemsQuery {
            item_ids: vec![person],
            include_inherited_tags: vec!["Safe".into()],
            ..Default::default()
        };
        assert_eq!(
            repository
                .get_items(&filter)
                .await
                .unwrap()
                .total_record_count,
            1
        );
        seed_item_value(&db, person, ItemValueType::Tags, "Private").await;
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    exclude_inherited_tags: vec!["private".into()],
                    ..filter
                })
                .await
                .unwrap()
                .total_record_count,
            0
        );
    }

    /// Plan pin for the resumable roll-up's played arm: driven from the
    /// materialized played-leaf list through the leaf's primary key, then its
    /// closure by `ItemId` — never a scan of every leaf or every closure row.
    #[tokio::test]
    async fn resumable_played_arm_drives_from_the_played_leaf_list() {
        use sqlx::{Execute, Row};
        let db = test_db().await;
        let user = seed_user(&db, Uuid::from_u128(0xA0F0)).await;
        let filter = InternalItemsQuery {
            user: Some(user),
            is_resumable: Some(true),
            ..Default::default()
        };
        let mut builder = build_query(&filter, QueryShape::FullRows);
        let mut query = builder.build();
        let args = query.take_arguments().unwrap().unwrap();
        let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
        let plan = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
            .fetch_all(db.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>();
        let pl = plan
            .iter()
            .position(|d| d == "SCAN pl")
            .unwrap_or_else(|| panic!("the played leaves drive the arm: {plan:#?}"));
        assert_eq!(
            plan[pl + 1],
            "SEARCH l USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)",
            "{plan:#?}"
        );
        assert!(
            !plan
                .iter()
                .any(|d| d.starts_with("SEARCH l USING INDEX IX_BaseItems_IsFolder")),
            "no walk over every leaf: {plan:#?}"
        );
        assert!(
            plan.iter()
                .all(|d| !d.starts_with("SCAN") || d == "SCAN pl"),
            "{plan:#?}"
        );
    }

    #[tokio::test]
    async fn tag_query_keeps_tag_value_map_and_ancestor_index_seeks() {
        use sqlx::{Execute, Row};
        let db = test_db().await;
        let filter = InternalItemsQuery {
            include_inherited_tags: vec!["safe".into()],
            exclude_inherited_tags: vec!["private".into()],
            ..Default::default()
        };
        let mut builder = build_query(&filter, QueryShape::FullRows);
        let mut query = builder.build();
        let args = query.take_arguments().unwrap().unwrap();
        let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
        let plan = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
            .fetch_all(db.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>()
            .join(" | ");
        assert_eq!(plan.matches("SEARCH tag_value USING").count(), 8, "{plan}");
        assert_eq!(plan.matches("SEARCH tag_map USING").count(), 8, "{plan}");
        // v12.2 `ItemsBelowTaggedAncestor`: each ancestor arm is one
        // uncorrelated set read by `ParentItemId`, never a per-row seek of
        // `(ItemId, ParentItemId)` for every tagged id.
        assert_eq!(
            plan.matches(
                "SEARCH tag_parent USING INDEX IX_AncestorIds_ParentItemId (ParentItemId=?)"
            )
            .count(),
            2,
            "{plan}"
        );
        assert_eq!(plan.matches("SEARCH tag_parent USING").count(), 2, "{plan}");
        let steps: Vec<&str> = plan.split(" | ").collect();
        assert!(
            steps
                .windows(2)
                .all(|w| !(w[0].starts_with("CORRELATED") && w[1].contains(" tag_"))),
            "no tag arm is a per-row subquery: {plan}"
        );
        assert!(!plan.contains("SCAN tag_"), "{plan}");
    }

    #[rstest::rstest]
    #[case(BaseItemKind::Movie, "Movie", false)]
    #[case(BaseItemKind::BoxSet, "Movie", false)]
    #[case(BaseItemKind::Series, "Series", false)]
    #[case(BaseItemKind::Season, "Series", false)]
    #[case(BaseItemKind::Episode, "Series", false)]
    #[case(BaseItemKind::Trailer, "Trailer", false)]
    #[case(BaseItemKind::Book, "Book", false)]
    #[case(BaseItemKind::AudioBook, "Book", false)]
    #[case(BaseItemKind::Audio, "Music", false)]
    #[case(BaseItemKind::MusicAlbum, "Music", false)]
    #[case(BaseItemKind::MusicVideo, "Music", false)]
    #[case(BaseItemKind::LiveTvChannel, "LiveTvChannel", false)]
    #[case(BaseItemKind::LiveTvProgram, "LiveTvProgram", false)]
    #[case(BaseItemKind::Video, "ChannelContent", true)]
    #[case(BaseItemKind::Audio, "ChannelContent", true)]
    #[tokio::test]
    async fn saved_unrated_policy_filters_old_rows_and_explicit_ids(
        #[case] kind: BaseItemKind,
        #[case] category: &str,
        #[case] channel: bool,
    ) {
        use ferrofin_db::enums::PreferenceKind;
        let db = test_db().await;
        let repository = repo(&db);
        let user = seed_user_with_defaults(&db, Uuid::new_v4()).await;
        let unrated = Uuid::new_v4();
        let rated = Uuid::new_v4();
        for (id, score) in [(unrated, None), (rated, Some(0_i64))] {
            seed_named_item(&db, id, kind, "Rating category").await;
            sqlx::query(r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue" = ?, "UnratedType" = NULL, "ChannelId" = ? WHERE "Id" = ?"#)
                .bind(score).bind(channel.then(|| guid_to_db(Uuid::new_v4()))).bind(guid_to_db(id)).execute(db.writer()).await.unwrap();
        }
        let query = InternalItemsQuery {
            user: Some(user.clone()),
            item_ids: vec![unrated, rated],
            group_by_presentation_unique_key: false,
            ..Default::default()
        };
        for (blocked, count) in [
            (Vec::new(), 2),
            (vec![category.to_owned()], 1),
            (vec!["Other".to_owned()], 2),
            (Vec::new(), 2),
        ] {
            crate::user_entity_ext::set_preference(
                db.writer(),
                &user.id,
                PreferenceKind::BlockUnratedItems,
                &blocked,
            )
            .await
            .unwrap();
            let result = repository.get_items(&query).await.unwrap();
            assert_eq!(result.total_record_count, count, "{blocked:?}");
            assert_eq!(result.items.len(), usize::try_from(count).unwrap());
            if count == 1 {
                let selector = InternalItemsQuery {
                    has_parental_rating: Some(true),
                    ..query.clone()
                };
                assert_eq!(
                    repository
                        .get_items(&selector)
                        .await
                        .unwrap()
                        .total_record_count,
                    2,
                    "TranslateQuery's explicit HasParentalRating selector skips BlockUnratedItems"
                );
            }
            assert!(
                result.items.iter().any(|item| item.id == guid_to_db(rated)),
                "a score of zero is rated"
            );
        }
    }

    #[tokio::test]
    async fn parental_rating_hides_facets_backed_only_by_restricted_media() {
        let db = test_db().await;
        let repository = repo(&db);
        let mut user = seed_user_with_defaults(&db, Uuid::new_v4()).await;
        user.max_parental_rating_score = Some(12);
        let mut movies = Vec::new();
        for (name, rating) in [("Child", 10), ("Adult", 18)] {
            let id = Uuid::new_v4();
            seed_named_item(&db, id, BaseItemKind::Movie, name).await;
            sqlx::query(
                r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue" = ? WHERE "Id" = ?"#,
            )
            .bind(rating)
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .unwrap();
            seed_item_genre(&db, id, name).await;
            movies.push(id);
        }
        seed_library_over(&db, &movies).await;
        let query = InternalItemsQuery {
            user: Some(user),
            ..Default::default()
        };
        let genres = repository.get_genres(&query).await.unwrap();
        assert_eq!(genres.total_record_count, 1);
        let query = InternalItemsQuery {
            include_items_by_name: Some(true),
            include_item_types: vec![BaseItemKind::Genre],
            ..query
        };
        let found = repository.get_item_list(&query).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name.as_deref(), Some("Child"));
    }

    #[tokio::test]
    async fn hidden_episode_progress_does_not_make_its_series_resumable() {
        let db = test_db().await;
        let repository = repo(&db);
        let uid = Uuid::new_v4();
        let mut user = seed_user_with_defaults(&db, uid).await;
        let series = Uuid::new_v4();
        let episode = Uuid::new_v4();
        seed_folder_item(&db, series, BaseItemKind::Series, "Series", None).await;
        seed_child_item(&db, episode, BaseItemKind::Episode, "Episode", series).await;
        seed_library_over(&db, &[series, episode]).await;
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?, ?)"#)
            .bind(guid_to_db(episode))
            .bind(guid_to_db(series))
            .execute(db.writer())
            .await
            .unwrap();
        sqlx::query(r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue" = 18 WHERE "Id" = ?"#)
            .bind(guid_to_db(episode))
            .execute(db.writer())
            .await
            .unwrap();
        seed_user_data(&db, uid, episode, false, None).await;
        sqlx::query(r#"UPDATE "UserData" SET "PlaybackPositionTicks" = 100 WHERE "ItemId" = ?"#)
            .bind(guid_to_db(episode))
            .execute(db.writer())
            .await
            .unwrap();
        for (limit, count) in [(Some(12), 0), (Some(18), 1), (None, 1)] {
            user.max_parental_rating_score = limit;
            let query = InternalItemsQuery {
                user: Some(user.clone()),
                include_item_types: vec![BaseItemKind::Series],
                is_resumable: Some(true),
                ..Default::default()
            };
            assert_eq!(repository.get_item_list(&query).await.unwrap().len(), count);
        }
    }

    /// The artist id filters over one artist's albums (TranslateQuery.cs:
    /// 617-650): `ArtistIds` is any credit, `AlbumArtistIds` the albums the
    /// artist leads (the artist page's Albums), `ContributingArtistIds` the
    /// albums crediting the artist under someone else's name (Appears On) and
    /// `ExcludeArtistIds` every album not crediting the artist.
    #[tokio::test]
    async fn artist_id_filters_split_an_artists_albums() {
        async fn albums(
            repository: &FerrofinItemRepository,
            filter: InternalItemsQuery,
        ) -> Vec<String> {
            let rows = repository
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![BaseItemKind::MusicAlbum],
                    ..filter
                })
                .await
                .unwrap();
            let mut names: Vec<String> = rows.into_iter().filter_map(|row| row.name).collect();
            names.sort();
            names
        }

        let db = test_db().await;
        let repository = repo(&db);
        let (alpha, beta) = (Uuid::from_u128(0xAA01), Uuid::from_u128(0xAA02));
        for (id, name) in [(alpha, "Alpha"), (beta, "Beta")] {
            seed_named_item(&db, id, BaseItemKind::MusicArtist, name).await;
            set_clean_name(&db, id, &name.to_lowercase()).await;
        }
        for (id, name, album_artist, artists) in [
            (0xAA10, "Own", "Alpha", &["Alpha"][..]),
            (0xAA11, "Featured", "Beta", &["Beta", "Alpha"][..]),
            (0xAA12, "Other", "Beta", &["Beta"][..]),
        ] {
            let album = Uuid::from_u128(id);
            seed_named_item(&db, album, BaseItemKind::MusicAlbum, name).await;
            seed_item_value(&db, album, ItemValueType::AlbumArtist, album_artist).await;
            for artist in artists {
                seed_item_value(&db, album, ItemValueType::Artist, artist).await;
            }
        }

        let by = |ids: fn(&mut InternalItemsQuery) -> &mut Vec<Uuid>, artist: Uuid| {
            let mut filter = InternalItemsQuery::default();
            ids(&mut filter).push(artist);
            filter
        };
        assert_eq!(
            albums(&repository, InternalItemsQuery::default()).await,
            ["Featured", "Other", "Own"]
        );
        assert_eq!(
            albums(&repository, by(|q| &mut q.artist_ids, alpha)).await,
            ["Featured", "Own"]
        );
        assert_eq!(
            albums(&repository, by(|q| &mut q.album_artist_ids, alpha)).await,
            ["Own"]
        );
        assert_eq!(
            albums(&repository, by(|q| &mut q.contributing_artist_ids, alpha)).await,
            ["Featured"]
        );
        assert_eq!(
            albums(&repository, by(|q| &mut q.exclude_artist_ids, alpha)).await,
            ["Other"]
        );
        // Beta leads every album crediting Beta, so it appears on none.
        assert!(
            albums(&repository, by(|q| &mut q.contributing_artist_ids, beta))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn parental_rating_filters_lists_counts_and_linked_containers() {
        use ferrofin_traits::persistence::LinkedChildrenService as _;
        let db = test_db().await;
        let repository = repo(&db);
        let mut user = seed_user_with_defaults(&db, Uuid::from_u128(0xAB10)).await;
        user.max_parental_rating_score = Some(10);
        user.max_parental_rating_sub_score = Some(1);
        let ids: Vec<_> = (0xAB20..0xAB25).map(Uuid::from_u128).collect();
        for (id, score, subscore) in [
            (ids[0], Some(9_i64), Some(9_i64)),
            (ids[1], Some(10), Some(1)),
            (ids[2], Some(10), Some(2)),
            (ids[3], Some(17), None),
            (ids[4], None, None),
        ] {
            seed_named_item(&db, id, BaseItemKind::Movie, "Synthetic rating case").await;
            sqlx::query(
                r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue"=?,
                "InheritedParentalRatingSubValue"=? WHERE "Id"=?"#,
            )
            .bind(score)
            .bind(subscore)
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .unwrap();
        }
        seed_library_over(&db, &ids).await;
        let query = InternalItemsQuery {
            user: Some(user.clone()),
            include_item_types: vec![BaseItemKind::Movie],
            ..Default::default()
        };
        let rows = repository.get_items(&query).await.unwrap();
        assert_eq!(rows.total_record_count, 3);
        for id in [ids[0], ids[1], ids[4]] {
            assert!(rows.items.iter().any(|row| row.id == guid_to_db(id)));
        }
        let collection = Uuid::from_u128(0xAB30);
        seed_named_item(
            &db,
            collection,
            BaseItemKind::BoxSet,
            "Synthetic restricted collection",
        )
        .await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "TopParentId" =
            (SELECT "TopParentId" FROM "BaseItems" WHERE "Id"=?) WHERE "Id"=?"#,
        )
        .bind(guid_to_db(ids[0]))
        .bind(guid_to_db(collection))
        .execute(db.writer())
        .await
        .unwrap();
        let links = crate::FerrofinLinkedChildrenService::new(db.clone());
        links
            .upsert_linked_child(collection, ids[3], 0)
            .await
            .unwrap();
        let query = InternalItemsQuery {
            user: Some(user),
            include_item_types: vec![BaseItemKind::BoxSet],
            ..Default::default()
        };
        assert!(repository.get_item_list(&query).await.unwrap().is_empty());
        links
            .upsert_linked_child(collection, ids[0], 0)
            .await
            .unwrap();
        assert_eq!(repository.get_item_list(&query).await.unwrap().len(), 1);
    }

    /// v12.2 `BuildMaxParentalRatingFilter` (45d7760a14,
    /// `QueryBuilding.cs:663-689`): the item's own rating, where it has one,
    /// must be within the limit, AND an item with Manual links must have a
    /// member within it. A rated collection whose members are all over the
    /// limit is hidden; an unrated video is not judged by its alternate-version
    /// links, which do not make it a container.
    #[rstest::rstest]
    #[case::rated_collection_of_blocked_members(BaseItemKind::BoxSet, Some(9), 0, &[17], false)]
    #[case::rated_collection_with_an_allowed_member(BaseItemKind::BoxSet, Some(9), 0, &[17, 9], true)]
    #[case::over_limit_collection_with_an_allowed_member(BaseItemKind::BoxSet, Some(17), 0, &[9], false)]
    #[case::unrated_collection_of_blocked_members(BaseItemKind::BoxSet, None, 0, &[17], false)]
    #[case::unrated_video_with_a_blocked_version(BaseItemKind::Movie, None, 2, &[17], true)]
    #[case::unrated_video_with_a_blocked_linked_version(BaseItemKind::Movie, None, 3, &[17], true)]
    #[tokio::test]
    async fn max_parental_rating_gates_containers_on_manual_members_only(
        #[case] kind: BaseItemKind,
        #[case] own: Option<i64>,
        #[case] link_type: i32,
        #[case] members: &[i64],
        #[case] visible: bool,
    ) {
        use ferrofin_traits::persistence::LinkedChildrenService as _;
        let db = test_db().await;
        let repository = repo(&db);
        let mut user = seed_user_with_defaults(&db, Uuid::from_u128(0xAB40)).await;
        user.max_parental_rating_score = Some(10);
        let container = Uuid::from_u128(0xAB41);
        seed_named_item(&db, container, kind, "Container").await;
        let mut ids = vec![container];
        let links = crate::FerrofinLinkedChildrenService::new(db.clone());
        for (n, score) in members.iter().enumerate() {
            let member = Uuid::from_u128(0xAB50 + n as u128);
            seed_named_item(&db, member, BaseItemKind::Movie, "Member").await;
            links
                .upsert_linked_child(container, member, link_type)
                .await
                .unwrap();
            ids.push(member);
            sqlx::query(
                r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue" = ? WHERE "Id" = ?"#,
            )
            .bind(score)
            .bind(guid_to_db(member))
            .execute(db.writer())
            .await
            .unwrap();
        }
        sqlx::query(r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue" = ? WHERE "Id" = ?"#)
            .bind(own)
            .bind(guid_to_db(container))
            .execute(db.writer())
            .await
            .unwrap();
        seed_library_over(&db, &ids).await;

        let listed = repository
            .get_item_list(&InternalItemsQuery {
                user: Some(user),
                item_ids: vec![container],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(listed.len(), usize::from(visible));
    }

    /// Plan pin for the max-parental-rating container rule: both Manual-link
    /// probes seek `IX_LinkedChildren_ParentId_ChildType` on both columns, and
    /// the member's rating is a primary-key probe — never a scan.
    #[tokio::test]
    async fn max_parental_rating_container_probes_keep_their_index_seeks() {
        use sqlx::{Execute, Row};
        let db = test_db().await;
        let filter = InternalItemsQuery {
            max_parental_rating: Some(ferrofin_model::entities_media::ParentalRatingScore {
                score: 10,
                sub_score: None,
            }),
            ..Default::default()
        };
        let mut builder = build_query(&filter, QueryShape::FullRows);
        let mut query = builder.build();
        let args = query.take_arguments().unwrap().unwrap();
        let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
        let plan = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
            .fetch_all(db.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>()
            .join(" | ");
        assert_eq!(
            plan.matches("USING COVERING INDEX IX_LinkedChildren_ParentId_ChildType (ParentId=? AND ChildType=?)")
                .count()
                + plan
                    .matches("USING INDEX IX_LinkedChildren_ParentId_ChildType (ParentId=? AND ChildType=?)")
                    .count(),
            2,
            "{plan}"
        );
        assert!(
            plan.contains("SEARCH rated USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)"),
            "{plan}"
        );
        assert!(
            !plan.contains("SCAN lc") && !plan.contains("SCAN rated"),
            "{plan}"
        );
    }

    #[rstest::rstest]
    // Pinned EF SQLite lowercases the bound with .NET ToLowerInvariant, but
    // SQLite lower(SortName) changes ASCII only. The EF oracle confirms these.
    #[case("Élodie", "él", false)]
    #[case("ΟΣ", "οσ", false)]
    #[case("𐐀 Star", "𐐨", false)]
    #[case("ı", "I", false)]
    #[case("ß", "ss", false)]
    #[case("élodie", "ÉL", true)]
    #[case("οσ", "ΟΣ", true)]
    #[case("𐐨 Star", "𐐀", true)]
    #[case("a%b", "a%", true)]
    #[case("axb", "a%", false)]
    #[case("a_b", "a_", true)]
    #[case("axb", "a_", false)]
    #[tokio::test]
    async fn unicode_name_prefix(
        #[case] stored: &str,
        #[case] prefix: &str,
        #[case] matches: bool,
    ) {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Person, stored).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = ?1 WHERE "Id" = ?2"#)
            .bind(stored)
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .unwrap();
        let result = repo(&db)
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Person],
                name_starts_with: Some(prefix.to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result.iter().any(|row| row.id == guid_to_db(id)), matches);
    }

    /// Independent EF SQLite 10.0.11 measurements of the pinned expressions,
    /// including post-collapse names, literal prefixes, nulls and range bounds.
    #[tokio::test]
    async fn repository_name_ranges_match_the_measured_ef_sqlite_oracle() {
        use ferrofin_traits::persistence::LinkedChildrenService as _;
        let reference: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/dashboard_name_range_reference.json"
        ))
        .unwrap();
        let db = test_db().await;
        let repository = repo(&db);
        let links = crate::FerrofinLinkedChildrenService::new(db.clone());
        let cases = reference["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 83);
        for case in cases {
            let person = Uuid::new_v4();
            let child = Uuid::new_v4();
            let collection = Uuid::new_v4();
            for (id, kind, name) in [
                (person, BaseItemKind::Person, "Person"),
                (child, BaseItemKind::Movie, "Unrelated child name"),
                (collection, BaseItemKind::BoxSet, "Collection"),
            ] {
                seed_named_item(&db, id, kind, name).await;
                sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = ? WHERE "Id" = ?"#)
                    .bind(if id == child {
                        Some("child-name-must-not-control-collection-filter")
                    } else {
                        case["Stored"].as_str()
                    })
                    .bind(guid_to_db(id))
                    .execute(db.writer())
                    .await
                    .unwrap();
            }
            links
                .upsert_linked_child(collection, child, 0)
                .await
                .unwrap();
            let string = |key: &str| case[key].as_str().map(str::to_owned);
            for (shape, id, expected_id, kind, collapse) in [
                ("ordinary", person, person, BaseItemKind::Person, false),
                (
                    "collapsed-all",
                    child,
                    collection,
                    BaseItemKind::Movie,
                    true,
                ),
            ] {
                let result = repository
                    .get_items(&InternalItemsQuery {
                        item_ids: vec![id],
                        include_item_types: vec![kind],
                        collapse_box_set_items: Some(collapse),
                        // A full singleton page also executes the count query.
                        limit: Some(1),
                        name_starts_with: string("prefix"),
                        name_starts_with_or_greater: string("greater"),
                        name_less_than: string("less"),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                let matches = case[shape].as_bool().unwrap();
                assert_eq!(
                    result.items.len(),
                    usize::from(matches),
                    "{} {shape}",
                    case["Name"]
                );
                assert_eq!(result.total_record_count, i32::from(matches));
                if matches {
                    assert_eq!(result.items[0].id, guid_to_db(expected_id));
                }
            }
        }
    }

    #[rstest::rstest]
    #[case("Élodie", "élodie")]
    #[case("ΟΣ", "οσ")]
    #[case("𐐀 Star", "𐐨 star")]
    #[tokio::test]
    async fn unicode_original_title_search(#[case] stored: &str, #[case] term: &str) {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "Unrelated title").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "OriginalTitle" = ?1 WHERE "Id" = ?2"#)
            .bind(stored)
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .unwrap();
        let result = repo(&db)
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Movie],
                search_term: Some(term.to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, guid_to_db(id));
    }

    /// Fixtures persist the actual folder flag and ancestor rows. Raw ParentId
    /// alone intentionally does not make a recursive descendant.
    async fn seed_linked_descendant_item(
        db: &Database,
        id: Uuid,
        kind: BaseItemKind,
        name: &str,
        parent: Option<Uuid>,
        ancestors: &[Uuid],
    ) {
        if matches!(
            kind,
            BaseItemKind::Folder
                | BaseItemKind::BoxSet
                | BaseItemKind::Playlist
                | BaseItemKind::Series
                | BaseItemKind::Season
        ) {
            seed_folder_item(db, id, kind, name, parent).await;
        } else if let Some(parent) = parent {
            seed_child_item(db, id, kind, name, parent).await;
        } else {
            seed_named_item(db, id, kind, name).await;
        }
        let mut row = crate::test_support::fetch_item(db, id).await;
        row.sort_name = Some(lower_invariant(name));
        let persistence = crate::FerrofinItemPersistenceService::new(db.clone());
        persistence.save_items(&[row]).await.unwrap();
        persistence.set_ancestors(id, ancestors).await.unwrap();
    }

    /// v12.2 `ApplyAdjacencyFilter`: on the SQL path `AdjacentTo` trims the
    /// query's own ordered result to the item and its neighbours before paging,
    /// for `GetItems`, `GetItemList` and `GetItemIdsList` alike.
    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one fixture walks every adjacency case across the three query methods"
    )]
    async fn adjacent_to_trims_a_recursive_sql_query_to_the_item_and_its_neighbours() {
        use ferrofin_model::{dto::SortOrder, live_tv::ItemSortBy};
        let db = test_db().await;
        let repository = repo(&db);
        let root = Uuid::new_v4();
        seed_linked_descendant_item(&db, root, BaseItemKind::Folder, "Root", None, &[]).await;
        let [alpha, bravo, charlie, delta] = std::array::from_fn(|_| Uuid::new_v4());
        for (id, name) in [
            (alpha, "Alpha"),
            (bravo, "Bravo"),
            (charlie, "Charlie"),
            (delta, "Delta"),
        ] {
            seed_linked_descendant_item(&db, id, BaseItemKind::Movie, name, Some(root), &[root])
                .await;
        }
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            ..linked_descendant_query(root)
        };
        let ids = |result: &[BaseItemEntity]| -> Vec<String> {
            result.iter().map(|row| row.id.clone()).collect()
        };
        let adjacent = |id: Uuid| InternalItemsQuery {
            adjacent_to: Some(id),
            ..query.clone()
        };

        let middle = repository.get_items(&adjacent(bravo)).await.unwrap();
        assert_eq!(
            ids(&middle.items),
            [alpha, bravo, charlie].map(guid_to_db),
            "previous, the item, next"
        );
        assert_eq!(middle.total_record_count, 3);
        let first = repository.get_items(&adjacent(alpha)).await.unwrap();
        assert_eq!(ids(&first.items), [alpha, bravo].map(guid_to_db));
        let missing = repository
            .get_items(&adjacent(Uuid::new_v4()))
            .await
            .unwrap();
        assert!(missing.items.is_empty());
        assert_eq!(missing.total_record_count, 0);
        // Paging applies to the trimmed set, and the total counts it.
        let paged = repository
            .get_items(&InternalItemsQuery {
                start_index: Some(1),
                limit: Some(1),
                ..adjacent(charlie)
            })
            .await
            .unwrap();
        assert_eq!(ids(&paged.items), [guid_to_db(charlie)]);
        assert_eq!(paged.total_record_count, 3);
        // Neighbours follow the order the query asked for.
        let descending = repository
            .get_item_list(&InternalItemsQuery {
                order_by: vec![(ItemSortBy::SortName, SortOrder::Descending)],
                ..adjacent(bravo)
            })
            .await
            .unwrap();
        assert_eq!(ids(&descending), [charlie, bravo, alpha].map(guid_to_db));
        assert_eq!(
            ids(&repository.get_item_list(&adjacent(delta)).await.unwrap()),
            [charlie, delta].map(guid_to_db)
        );
        assert_eq!(
            repository.get_item_ids(&adjacent(bravo)).await.unwrap(),
            [alpha, bravo, charlie]
        );
        // A nil id is no adjacency request at all.
        assert_eq!(
            repository
                .get_items(&adjacent(Uuid::nil()))
                .await
                .unwrap()
                .items
                .len(),
            4
        );

        // The trim runs after grouping: two versions sharing a presentation
        // key are one row, so the hidden version has no window at all.
        sqlx::query(
            r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = 'shared' WHERE "Id" IN (?, ?)"#,
        )
        .bind(guid_to_db(alpha))
        .bind(guid_to_db(bravo))
        .execute(db.writer())
        .await
        .unwrap();
        let grouped = InternalItemsQuery {
            user: Some(seed_user_with_defaults(&db, Uuid::new_v4()).await),
            group_by_presentation_unique_key: true,
            ..query.clone()
        };
        assert!(crate::translate_query::group_by_presentation_unique_key(
            &grouped
        ));
        let shown = repository.get_item_ids(&grouped).await.unwrap();
        assert_eq!(shown.len(), 3, "{shown:?}");
        let (kept, hidden) = if shown.contains(&alpha) {
            (alpha, bravo)
        } else {
            (bravo, alpha)
        };
        let around_kept = repository
            .get_items(&InternalItemsQuery {
                adjacent_to: Some(kept),
                ..grouped.clone()
            })
            .await
            .unwrap();
        assert_eq!(ids(&around_kept.items), [kept, charlie].map(guid_to_db));
        let around_hidden = repository
            .get_items(&InternalItemsQuery {
                adjacent_to: Some(hidden),
                ..grouped
            })
            .await
            .unwrap();
        assert!(around_hidden.items.is_empty());
    }

    async fn seed_descendant_link(db: &Database, parent: Uuid, child: Uuid, kind: i32) {
        use ferrofin_traits::persistence::LinkedChildrenService as _;
        crate::FerrofinLinkedChildrenService::new(db.clone())
            .upsert_linked_child(parent, child, kind)
            .await
            .unwrap();
    }

    async fn linked_descendant_names(
        repository: &FerrofinItemRepository,
        query: &InternalItemsQuery,
    ) -> Vec<String> {
        let rows = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            repository.get_item_list(query),
        )
        .await
        .unwrap()
        .unwrap();
        let mut names: Vec<String> = rows.into_iter().filter_map(|row| row.name).collect();
        names.sort();
        names
    }

    fn linked_descendant_query(parent: Uuid) -> InternalItemsQuery {
        use ferrofin_model::{dto::SortOrder, live_tv::ItemSortBy};
        InternalItemsQuery {
            parent_id: parent,
            recursive: true,
            group_by_presentation_unique_key: false,
            collapse_box_set_items: Some(false),
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            ..Default::default()
        }
    }

    #[rstest::rstest]
    #[case(BaseItemKind::BoxSet)]
    #[case(BaseItemKind::Playlist)]
    #[tokio::test]
    async fn linked_container_descendants_filter_count_and_page_after_membership(
        #[case] kind: BaseItemKind,
    ) {
        let db = test_db().await;
        let repository = repo(&db);
        let [root, movie, series, season, episode, physical, orphan] =
            std::array::from_fn(|_| Uuid::new_v4());
        for (id, item_kind, name, parent, ancestors) in [
            (root, kind, "Collection", None, vec![]),
            (movie, BaseItemKind::Movie, "Alpha Movie", None, vec![]),
            (series, BaseItemKind::Series, "Bravo Series", None, vec![]),
            (
                season,
                BaseItemKind::Season,
                "Season",
                Some(series),
                vec![series],
            ),
            (
                episode,
                BaseItemKind::Episode,
                "Charlie Episode",
                Some(season),
                vec![series, season],
            ),
            (
                physical,
                BaseItemKind::Movie,
                "Delta Physical",
                Some(root),
                vec![root],
            ),
            (
                orphan,
                BaseItemKind::Movie,
                "Orphan Parent Only",
                Some(root),
                vec![],
            ),
        ] {
            seed_linked_descendant_item(&db, id, item_kind, name, parent, &ancestors).await;
        }
        seed_descendant_link(&db, root, movie, 0).await;
        seed_descendant_link(&db, root, series, 1).await;
        // Duplicate physical + linked membership remains one returned row.
        seed_descendant_link(&db, root, physical, 3).await;
        let query = InternalItemsQuery {
            include_item_types: vec![
                BaseItemKind::Movie,
                BaseItemKind::Series,
                BaseItemKind::Episode,
            ],
            limit: Some(1),
            ..linked_descendant_query(root)
        };
        let first = repository.get_items(&query).await.unwrap();
        assert_eq!(first.total_record_count, 4);
        assert_eq!(first.items[0].id, guid_to_db(movie));
        let second = repository
            .get_items(&InternalItemsQuery {
                start_index: Some(2),
                ..query.clone()
            })
            .await
            .unwrap();
        assert_eq!(second.total_record_count, 4);
        assert_eq!(second.items[0].id, guid_to_db(episode));
        let filtered = repository
            .get_items(&InternalItemsQuery {
                name_starts_with: Some("delta".to_owned()),
                ..query
            })
            .await
            .unwrap();
        assert_eq!(filtered.total_record_count, 1);
        assert_eq!(filtered.items[0].id, guid_to_db(physical));
        let ids = repository
            .get_item_ids(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Episode],
                ..linked_descendant_query(root)
            })
            .await
            .unwrap();
        assert_eq!(ids, [episode]);
    }

    #[tokio::test]
    async fn linked_container_cycles_all_child_kinds_and_nonfolder_boundaries() {
        let db = test_db().await;
        let repository = repo(&db);
        let [root, nested, movie, video, alternate, extra] =
            std::array::from_fn(|_| Uuid::new_v4());
        for (id, kind, name, parent, ancestors) in [
            (root, BaseItemKind::BoxSet, "Root", None, vec![]),
            (nested, BaseItemKind::Playlist, "Nested", None, vec![]),
            (movie, BaseItemKind::Movie, "Alpha Movie", None, vec![]),
            (video, BaseItemKind::Video, "Bravo Video", None, vec![]),
            (
                alternate,
                BaseItemKind::Movie,
                "Hidden Alternate",
                None,
                vec![],
            ),
            (
                extra,
                BaseItemKind::Video,
                "Hidden Owned Extra",
                Some(movie),
                vec![movie],
            ),
        ] {
            seed_linked_descendant_item(&db, id, kind, name, parent, &ancestors).await;
        }
        for (parent, child, kind) in [
            (root, nested, 2),
            (nested, root, 1),
            (nested, movie, 3),
            (root, video, 0),
            (movie, alternate, 2),
        ] {
            seed_descendant_link(&db, parent, child, kind).await;
        }
        assert_eq!(
            linked_descendant_names(&repository, &linked_descendant_query(root)).await,
            ["Alpha Movie", "Bravo Video", "Nested"]
        );
        let derived = repository
            .resolve_views(&linked_descendant_query(root))
            .await
            .unwrap()
            .unwrap();
        let roots = derived.descendant_roots.as_ref().unwrap();
        assert_eq!(roots.closure_roots.len(), 2);
        assert_eq!(roots.link_roots.len(), 2);
        assert!(!roots.link_roots.contains(&movie));
        assert_eq!(derived.parent_id, Uuid::nil());
        assert!(derived.parent_type.is_none());
    }

    /// The source keeps first-visit roles even with sparse adopted ancestry:
    /// a physical owner later linked back never gains its uncovered closure.
    #[tokio::test]
    async fn linked_container_physical_owners_keep_first_visit_closure_roles() {
        let db = test_db().await;
        let repository = repo(&db);
        let [root, bridge, owner, marker, later, member, uncovered] =
            std::array::from_fn(|_| Uuid::new_v4());
        for (id, kind, name, parent, ancestors) in [
            (root, BaseItemKind::BoxSet, "Root", None, vec![]),
            (
                bridge,
                BaseItemKind::Folder,
                "Bridge",
                Some(root),
                vec![root],
            ),
            (
                owner,
                BaseItemKind::Folder,
                "Owner",
                Some(bridge),
                vec![bridge],
            ),
            (marker, BaseItemKind::Movie, "Marker", None, vec![]),
            (later, BaseItemKind::Playlist, "Later", None, vec![]),
            (member, BaseItemKind::Movie, "Member", None, vec![]),
            (
                uncovered,
                BaseItemKind::Movie,
                "Uncovered Physical",
                Some(owner),
                vec![owner],
            ),
        ] {
            seed_linked_descendant_item(&db, id, kind, name, parent, &ancestors).await;
        }
        for (parent, child, kind) in [
            (root, marker, 0),
            (owner, later, 0),
            (owner, member, 0),
            (later, owner, 0),
        ] {
            seed_descendant_link(&db, parent, child, kind).await;
        }
        assert_eq!(
            linked_descendant_names(&repository, &linked_descendant_query(root)).await,
            ["Bridge", "Later", "Marker", "Member", "Owner"]
        );
        let query = repository
            .resolve_views(&linked_descendant_query(root))
            .await
            .unwrap()
            .unwrap();
        let roots = query.descendant_roots.unwrap();
        assert!(roots.link_roots.contains(&owner));
        assert!(!roots.closure_roots.contains(&owner));
        assert!(roots.closure_roots.contains(&later));
    }

    #[rstest::rstest]
    #[case(BaseItemKind::BoxSet)]
    #[case(BaseItemKind::Playlist)]
    #[tokio::test]
    async fn linked_container_empty_missing_and_physical_only_scopes_stay_narrow(
        #[case] kind: BaseItemKind,
    ) {
        let db = test_db().await;
        let repository = repo(&db);
        let [root, physical, orphan, linked, ordinary, ordinary_child] =
            std::array::from_fn(|_| Uuid::new_v4());
        for (id, item_kind, name, parent, ancestors) in [
            (root, kind, "Root", None, vec![]),
            (
                physical,
                BaseItemKind::Movie,
                "Physical",
                Some(root),
                vec![root],
            ),
            (
                orphan,
                BaseItemKind::Movie,
                "Parent Only",
                Some(root),
                vec![],
            ),
            (linked, BaseItemKind::Movie, "Linked", None, vec![]),
            (ordinary, BaseItemKind::Folder, "Ordinary", None, vec![]),
            (
                ordinary_child,
                BaseItemKind::Movie,
                "Ordinary Physical",
                Some(ordinary),
                vec![ordinary],
            ),
        ] {
            seed_linked_descendant_item(&db, id, item_kind, name, parent, &ancestors).await;
        }
        assert_eq!(
            linked_descendant_names(&repository, &linked_descendant_query(root)).await,
            ["Physical"]
        );
        assert!(
            linked_descendant_names(&repository, &linked_descendant_query(Uuid::new_v4()))
                .await
                .is_empty()
        );
        seed_descendant_link(&db, root, linked, 0).await;
        seed_descendant_link(&db, ordinary, linked, 0).await;
        assert_eq!(
            linked_descendant_names(&repository, &linked_descendant_query(ordinary)).await,
            ["Ordinary Physical"]
        );
        assert_eq!(
            linked_descendant_names(
                &repository,
                &InternalItemsQuery {
                    recursive: false,
                    physical_children_only: true,
                    ..linked_descendant_query(root)
                }
            )
            .await,
            ["Parent Only", "Physical"]
        );
    }

    #[allow(clippy::too_many_lines)] // one source graph and its membership/count/page boundary assertions
    #[tokio::test]
    async fn linked_container_visibility_is_applied_to_results_not_intermediate_folders() {
        let db = test_db().await;
        let repository = repo(&db);
        let (allowed, denied, user_id, user) = two_libraries(&db).await;
        let [root, intermediate, visible, played, blocked, outside] =
            std::array::from_fn(|_| Uuid::new_v4());
        for (id, kind, name, parent, ancestors) in [
            (root, BaseItemKind::BoxSet, "Root", None, vec![]),
            (
                intermediate,
                BaseItemKind::Series,
                "Hidden Intermediate",
                None,
                vec![],
            ),
            (
                visible,
                BaseItemKind::Movie,
                "Alpha Visible",
                Some(intermediate),
                vec![intermediate],
            ),
            (
                played,
                BaseItemKind::Movie,
                "Bravo Played",
                Some(intermediate),
                vec![intermediate],
            ),
            (
                blocked,
                BaseItemKind::Movie,
                "Charlie Blocked",
                None,
                vec![],
            ),
            (outside, BaseItemKind::Movie, "Delta Outside", None, vec![]),
        ] {
            seed_linked_descendant_item(&db, id, kind, name, parent, &ancestors).await;
            let mut row = crate::test_support::fetch_item(&db, id).await;
            row.top_parent_id = Some(guid_to_db(if id == outside { denied } else { allowed }));
            row.is_virtual_item = id == intermediate;
            crate::FerrofinItemPersistenceService::new(db.clone())
                .save_items(&[row])
                .await
                .unwrap();
        }
        for id in [intermediate, blocked, outside] {
            seed_descendant_link(&db, root, id, 0).await;
        }
        seed_user_data(&db, user_id, played, true, None).await;
        seed_item_value(&db, blocked, ItemValueType::Tags, "blocked-selection").await;
        crate::user_entity_ext::set_preference(
            db.pool(),
            &user.id,
            PreferenceKind::BlockedTags,
            &["blocked-selection".to_owned()],
        )
        .await
        .unwrap();
        crate::user_entity_ext::set_permission(
            db.pool(),
            &user.id,
            PermissionKind::EnableAllFolders,
            false,
        )
        .await
        .unwrap();
        set_folder_preference(&db, user_id, PreferenceKind::EnabledFolders, &[allowed]).await;
        let query = InternalItemsQuery {
            user: Some(user),
            include_item_types: vec![BaseItemKind::Movie],
            is_virtual_item: Some(false),
            is_played: Some(false),
            limit: Some(1),
            ..linked_descendant_query(root)
        };
        let result = repository.get_items(&query).await.unwrap();
        assert_eq!(result.total_record_count, 1);
        assert_eq!(result.items[0].id, guid_to_db(visible));
        let resolved = repository.resolve_views(&query).await.unwrap().unwrap();
        assert_eq!(resolved.top_parent_ids, [allowed]);
        assert!(
            resolved
                .descendant_roots
                .as_ref()
                .unwrap()
                .link_roots
                .contains(&intermediate)
        );
        assert_eq!(count_over(&db, &resolved).await, 1);
        let confined = repository
            .get_items(&InternalItemsQuery {
                ancestor_ids: vec![intermediate],
                ..query
            })
            .await
            .unwrap();
        assert_eq!(confined.total_record_count, 1);
        assert_eq!(confined.items[0].id, guid_to_db(visible));
    }

    #[tokio::test]
    async fn explicit_descendant_seed_can_be_nonfolder_and_counts_reuse_root_resolution() {
        use ferrofin_traits::persistence::ItemCountService as _;
        let db = test_db().await;
        let repository = repo(&db);
        let [root, movie, alternate] = std::array::from_fn(|_| Uuid::new_v4());
        for (id, name) in [
            (root, "Seed Movie"),
            (movie, "Member Movie"),
            (alternate, "Alternate Movie"),
        ] {
            seed_named_item(&db, id, BaseItemKind::Movie, name).await;
        }
        seed_descendant_link(&db, root, movie, 3).await;
        seed_descendant_link(&db, movie, alternate, 2).await;
        let query = InternalItemsQuery {
            parent_id: Uuid::nil(),
            descendant_of_id: Some(root),
            ..linked_descendant_query(root)
        };
        assert_eq!(
            linked_descendant_names(&repository, &query).await,
            ["Member Movie"]
        );
        let count = crate::FerrofinItemCountService::new(db.clone());
        assert_eq!(count.get_count(&query).await.unwrap(), 1);
        assert_eq!(count.get_item_counts(&query).await.unwrap().movie_count, 1);
        let resolved = resolve_descendant_query(&db, &query)
            .await
            .unwrap()
            .unwrap();
        assert!(
            resolve_descendant_query(&db, &resolved)
                .await
                .unwrap()
                .is_none()
        );
        // Changing the logical criterion invalidates an old internal snapshot.
        let changed = InternalItemsQuery {
            descendant_of_id: Some(movie),
            ..resolved
        };
        assert_eq!(
            linked_descendant_names(&repository, &changed).await,
            ["Alternate Movie"]
        );
    }

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

    /// `PrepareFilterQuery`: a limited query grouped by metadata key reads 4
    /// rows past its limit on every list entry point; one without a limit, or
    /// without the flag, reads what it asked for. A counted page still totals
    /// every match.
    #[rstest::rstest]
    #[case::grouped(Some(2), true, 6)]
    #[case::not_grouped(Some(2), false, 2)]
    #[case::grouped_but_unlimited(None, true, 10)]
    #[tokio::test]
    async fn grouping_by_metadata_key_reads_four_rows_past_the_limit(
        #[case] limit: Option<i32>,
        #[case] grouped: bool,
        #[case] rows: usize,
    ) {
        let db = test_db().await;
        let repository = repo(&db);
        for i in 0..10u128 {
            seed_named_item(
                &db,
                Uuid::from_u128(0x7050 + i),
                BaseItemKind::Movie,
                &format!("Film {i:02}"),
            )
            .await;
        }
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            limit,
            enable_group_by_metadata_key: grouped,
            ..InternalItemsQuery::default()
        };

        let list = repository.get_item_list(&query).await.expect("list");
        let ids = repository.get_item_ids(&query).await.expect("ids");
        let page = repository
            .get_items(&InternalItemsQuery {
                enable_total_record_count: true,
                ..query.clone()
            })
            .await
            .expect("page");
        assert_eq!(page.total_record_count, 10, "the total counts every match");
        assert_eq!(
            (list.len(), ids.len(), page.items.len()),
            (rows, rows, rows)
        );
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

    /// The flagged version read seeks a `PrimaryVersionId` index twice —
    /// for the rows pointing at the page's ids, and for each row's "does
    /// anything point at it" flag — and scans nothing: the flag costs one
    /// index seek per version row, never a pass over `BaseItems`. (The
    /// planner takes Jellyfin's `IX_BaseItems_PrimaryVersionId` where the
    /// schema has it, Ferrofin's partial one otherwise; either seeks.) It
    /// answers the flag too: a version with a version of its own, and one
    /// without.
    #[tokio::test]
    async fn the_flagged_version_read_seeks_the_partial_index() {
        let db = test_db().await;
        let mut query = sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {}",
            rows_by_primary_flagged_sql(2)
        )));
        for _ in 0..3 {
            query = query.bind("00000000-0000-0000-0000-000000000001");
        }
        let plan: Vec<String> = query
            .fetch_all(db.pool())
            .await
            .expect("explain query plan")
            .into_iter()
            .map(|(_, _, _, detail)| detail)
            .collect();
        let seeks = |alias: &str| {
            plan.iter().any(|d| {
                d.starts_with(&format!("SEARCH {alias} USING"))
                    && d.contains("_PrimaryVersionId (PrimaryVersionId=?)")
            })
        };
        assert!(seeks("b"), "the rows by their pointer, got: {plan:?}");
        assert!(seeks("x"), "the flag by the same index, got: {plan:?}");
        assert!(
            !plan.iter().any(|d| d.starts_with("SCAN")),
            "no scan, got: {plan:?}"
        );

        let (primary, merged, local) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for id in [primary, merged, local] {
            seed_item(&db, id, BaseItemKind::Movie).await;
        }
        for (id, to) in [(merged, primary), (local, merged)] {
            sqlx::query(r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .bind(guid_to_db(to))
                .execute(db.writer())
                .await
                .unwrap();
        }
        let flagged = rows_by_primary_flagged(&db, &[primary, merged])
            .await
            .unwrap();
        let flag = |under: Uuid| -> Vec<bool> { flagged[&under].iter().map(|(_, f)| *f).collect() };
        assert_eq!(flag(primary), vec![true], "the merged version has one");
        assert_eq!(flag(merged), vec![false], "the local version has none");
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
                .bind(lower_invariant(name))
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
    async fn recursive_parent_uses_library_scope_and_folder_ancestors() {
        let db = test_db().await;
        let repository = repo(&db);
        // library ─ series ─ episode. The episode is a direct child of the series,
        // not of the library. Persist both its library scope and ancestor closure,
        // as the scanner does; library queries use TopParentId, folder queries
        // use the ancestor closure.
        let library = Uuid::from_u128(0xB001);
        let series = Uuid::from_u128(0xB002);
        let episode = Uuid::from_u128(0xB003);
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "TV").await;
        seed_named_item(&db, series, BaseItemKind::Series, "Show").await;
        seed_named_item(&db, episode, BaseItemKind::Episode, "Pilot").await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "ParentId" = ?2, "TopParentId" = ?3 WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(series))
        .bind(guid_to_db(library))
        .bind(guid_to_db(library))
        .execute(db.writer())
        .await
        .expect("series parent");
        sqlx::query(
            r#"UPDATE "BaseItems" SET "ParentId" = ?2, "TopParentId" = ?3 WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(episode))
        .bind(guid_to_db(series))
        .bind(guid_to_db(library))
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

        // The library and its series both reach the episode recursively.
        for parent_id in [library, series] {
            let recursive = InternalItemsQuery {
                parent_id,
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
        sqlx::query(r#"DELETE FROM "LinkedChildren" WHERE "ParentId" = ?1 AND "ChildId" = ?2"#)
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
    async fn get_image_infos_expands_jellyfin_virtual_paths() {
        let db = test_db().await;
        let item = Uuid::from_u128(0x9003);
        seed_named_item(&db, item, BaseItemKind::Series, "Adopted").await;
        // What an adopted Jellyfin row holds: the metadata dir as a token.
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
                ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                VALUES (?1, NULL, NULL, 0, 4, ?2, '%MetadataPath%/library/9c/9c3f/thumb.jpg', 0)"#,
        )
        .bind(guid_to_db(Uuid::from_u128(0x9103)))
        .bind(guid_to_db(item))
        .execute(db.writer())
        .await
        .expect("insert thumb");

        // Without an expander the token passes through verbatim.
        let raw = repo(&db).get_image_infos(item).await.expect("images");
        assert_eq!(raw[0].path, "%MetadataPath%/library/9c/9c3f/thumb.jpg");

        // With the server's paths it is the file the copied metadata/ holds.
        let root = tempfile::tempdir().expect("tempdir");
        let paths = crate::app_paths::test_paths(root.path());
        let repository =
            repo(&db).with_virtual_paths(VirtualPathExpander::from_paths(Arc::clone(&paths)));
        let images = repository.get_image_infos(item).await.expect("images");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].image_type, ImageType::Logo);
        assert_eq!(
            images[0].path,
            format!(
                "{}/library/9c/9c3f/thumb.jpg",
                ferrofin_traits::system::ServerApplicationPaths::internal_metadata_path(&*paths)
            )
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
    async fn swap_item_images_exchanges_the_files_and_keeps_the_rows() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0x9200);
        seed_named_item(&db, item, BaseItemKind::Movie, "Reorder Me").await;
        let dir = tempfile::tempdir().unwrap();

        // Three backdrops (type 2), addressed by index 0/1/2 in insertion order.
        let paths: Vec<String> = ["a.jpg", "b.jpg", "c.jpg"]
            .iter()
            .map(|name| {
                let path = dir.path().join(name);
                std::fs::write(&path, name.to_uppercase()).unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        for (n, path) in paths.iter().enumerate() {
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos"
                    ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                    VALUES (?1, X'6162', NULL, 1080, 2, ?2, ?3, 1920)"#,
            )
            .bind(guid_to_db(Uuid::from_u128(0x9210 + n as u128)))
            .bind(guid_to_db(item))
            .bind(path)
            .execute(db.writer())
            .await
            .expect("insert backdrop");
        }

        // Swap index 0 with index 2.
        assert!(
            repository
                .swap_item_images(item, ImageType::Backdrop, 0, 2)
                .await
                .expect("swap")
        );

        // `SwapImagesAsync` swaps the FILES: each row keeps its path, the
        // contents trade places, and no temporary copy is left behind.
        assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), "C.JPG");
        assert_eq!(std::fs::read_to_string(&paths[1]).unwrap(), "B.JPG");
        assert_eq!(std::fs::read_to_string(&paths[2]).unwrap(), "A.JPG");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
        let images = repository.get_image_infos(item).await.expect("images");
        assert_eq!(
            images.iter().map(|i| i.path.clone()).collect::<Vec<_>>(),
            paths
        );
        // The two swapped rows lose their cached size and blur hash and take
        // the file's modified time; the other is untouched.
        for (index, swapped) in [(0, true), (1, false), (2, true)] {
            assert_eq!(images[index].width, if swapped { 0 } else { 1920 });
            assert_eq!(images[index].height, if swapped { 0 } else { 1080 });
            assert_eq!(images[index].blur_hash.is_none(), swapped, "{index}");
            let recent = (Utc::now() - images[index].date_modified)
                .num_seconds()
                .abs()
                < 60;
            assert_eq!(recent, swapped, "{index}");
        }
    }

    #[tokio::test]
    async fn swap_item_images_out_of_range_index_is_noop() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0x9300);
        seed_named_item(&db, item, BaseItemKind::Movie, "One Backdrop").await;
        let dir = tempfile::tempdir().unwrap();
        let only = dir.path().join("only.jpg");
        std::fs::write(&only, "ONLY").unwrap();
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
                ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                VALUES (?1, NULL, NULL, 1080, 2, ?2, ?3, 1920)"#,
        )
        .bind(guid_to_db(Uuid::from_u128(0x9310)))
        .bind(guid_to_db(item))
        .bind(only.to_string_lossy().as_ref())
        .execute(db.writer())
        .await
        .expect("insert backdrop");

        // Index 5 does not exist — a faithful no-op, and the row is untouched.
        // Neither does a slot swap with itself: upstream's copy onto the same
        // file would throw, so nothing is exchanged or stamped.
        for pair in [(0, 5), (5, 0), (-1, 0), (0, 0)] {
            assert!(
                !repository
                    .swap_item_images(item, ImageType::Backdrop, pair.0, pair.1)
                    .await
                    .expect("noop swap"),
                "{pair:?}"
            );
        }
        let images = repository.get_image_infos(item).await.expect("images");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].width, 1920);
        assert_eq!(std::fs::read_to_string(&only).unwrap(), "ONLY");
    }

    /// A row whose file is gone fails the swap, as `File.Copy` throws, and the
    /// rows and the other file stay as they were.
    #[tokio::test]
    async fn swap_item_images_fails_cleanly_when_a_file_is_missing() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0x9350);
        seed_named_item(&db, item, BaseItemKind::Movie, "Broken").await;
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.jpg");
        std::fs::write(&present, "PRESENT").unwrap();
        for (n, path) in [(0_u128, &present), (1, &dir.path().join("gone.jpg"))] {
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos"
                    ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                    VALUES (?1, NULL, NULL, 1080, 2, ?2, ?3, 1920)"#,
            )
            .bind(guid_to_db(Uuid::from_u128(0x9360 + n)))
            .bind(guid_to_db(item))
            .bind(path.to_string_lossy().as_ref())
            .execute(db.writer())
            .await
            .expect("insert backdrop");
        }
        assert!(
            repository
                .swap_item_images(item, ImageType::Backdrop, 0, 1)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&present).unwrap(), "PRESENT");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let images = repository.get_image_infos(item).await.expect("images");
        assert!(images.iter().all(|image| image.width == 1920));
    }

    #[tokio::test]
    async fn swap_item_images_ignores_remote_urls() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0x9400);
        seed_named_item(&db, item, BaseItemKind::Movie, "Remote Backdrop").await;
        for (n, path) in [(0u128, "/local.jpg"), (1, "HTTPS://example.test/b.jpg")] {
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos"
                    ("Id", "Blurhash", "DateModified", "Height", "ImageType", "ItemId", "Path", "Width")
                    VALUES (?1, NULL, NULL, 1080, 2, ?2, ?3, 1920)"#,
            )
            .bind(guid_to_db(Uuid::from_u128(0x9410 + n)))
            .bind(guid_to_db(item))
            .bind(path)
            .execute(db.writer())
            .await
            .expect("insert backdrop");
        }
        // `!IsLocalFile` on either side: "Not supported yet", nothing changes.
        assert!(
            !repository
                .swap_item_images(item, ImageType::Backdrop, 0, 1)
                .await
                .expect("remote swap")
        );
        let images = repository.get_image_infos(item).await.expect("images");
        assert_eq!(images[0].path, "/local.jpg");
        assert_eq!(images[0].width, 1920);
        assert_eq!(images[1].path, "HTTPS://example.test/b.jpg");
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

        // Counts are computed only when `ItemCounts` is requested
        // (`ByName.cs:251`): a query with an explicit empty field set — what
        // `/Items/Filters2` sends — pays nothing for them, and the default
        // `DtoOptions` (every field, as C#'s parameterless constructor) has it.
        let genres = repository
            .get_genres(&without_fields(InternalItemsQuery::default()))
            .await
            .expect("genres");
        assert_eq!(genres.items.len(), 1);
        assert_eq!(genres.items[0].counts, ItemCounts::default());
        let genres = repository
            .get_genres(&InternalItemsQuery::default())
            .await
            .expect("genres with counts");
        assert_eq!(genres.items[0].counts.movie_count, 1);
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

    /// Port of v12.2 `BaseItemRepositoryAllowedTagsByNameTests` (53f08961cf):
    /// a genre, studio or artist carries none of the tags of the media it
    /// describes, so the allow list is not applied to the by-name row
    /// (`ByName.cs:229-232`); reachability is the inner scope's, so a genre
    /// written only on a by-name item stays hidden.
    #[tokio::test]
    async fn by_name_lists_follow_the_allowed_tags_of_the_media() {
        let db = test_db().await;
        let repository = repo(&db);
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0xA0D0)).await;
        let ids: Vec<_> = (0xA0D1..0xA0D6).map(Uuid::from_u128).collect();
        let (movie, song, blocked_movie, blocked_artist) = (ids[0], ids[1], ids[2], ids[3]);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Allowed Movie").await;
        seed_named_item(&db, song, BaseItemKind::Audio, "Allowed Song").await;
        seed_named_item(&db, blocked_movie, BaseItemKind::Movie, "Untagged Movie").await;
        seed_named_item(&db, blocked_artist, BaseItemKind::MusicArtist, "Mingus").await;
        seed_library_over(&db, &ids[..4]).await;
        for (item, value_type, value) in [
            (movie, ItemValueType::Tags, "kids"),
            (movie, ItemValueType::Genre, "Comedy"),
            (movie, ItemValueType::Studios, "Pixar"),
            (song, ItemValueType::Tags, "kids"),
            (song, ItemValueType::Artist, "Raffi"),
            (blocked_movie, ItemValueType::Genre, "Horror"),
            (blocked_artist, ItemValueType::Genre, "Jazz"),
        ] {
            seed_item_value(&db, item, value_type, value).await;
        }
        let names = |result: QueryResult<ItemWithCounts>| -> Vec<String> {
            result
                .items
                .into_iter()
                .filter_map(|row| row.item.name)
                .collect()
        };
        let query = InternalItemsQuery {
            user: Some(user.clone()),
            ..Default::default()
        };

        assert_eq!(
            names(repository.get_genres(&query).await.unwrap()),
            ["Comedy", "Horror"]
        );

        crate::user_entity_ext::set_preference(
            db.writer(),
            &user.id,
            PreferenceKind::AllowedTags,
            &["kids".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(
            names(repository.get_genres(&query).await.unwrap()),
            ["Comedy"]
        );
        assert_eq!(
            names(repository.get_studios(&query).await.unwrap()),
            ["Pixar"]
        );
        assert_eq!(
            names(repository.get_all_artists(&query).await.unwrap()),
            ["Raffi"]
        );
    }

    /// v12.2 `ApplyAlternateVersionFiltering` (911edfb64d) on the by-name
    /// inner queries: a version is hidden only while its primary sits in the
    /// same library, so a genre carried by a version in another library than
    /// its primary is listed and counted, and one in the primary's library is
    /// not. The by-name adaptation of `BaseItemRepositoryGroupingTests`
    /// `GetItemList_LibraryWithoutThePrimaryOfTheGroup_KeepsTheVersionVisible`.
    #[tokio::test]
    async fn by_name_lists_count_a_version_in_another_library_than_its_primary() {
        let db = test_db().await;
        let repository = repo(&db);
        let (library_a, library_b) = (Uuid::from_u128(0xA0E1), Uuid::from_u128(0xA0E2));
        let primary = Uuid::from_u128(0xA0E3);
        let (same_library, other_library) = (Uuid::from_u128(0xA0E4), Uuid::from_u128(0xA0E5));
        seed_top_parented_item(&db, primary, BaseItemKind::Movie, "Primary", library_a).await;
        seed_top_parented_item(&db, same_library, BaseItemKind::Movie, "Version", library_a).await;
        seed_top_parented_item(
            &db,
            other_library,
            BaseItemKind::Movie,
            "Version",
            library_b,
        )
        .await;
        for (version, genre) in [(same_library, "Hidden"), (other_library, "Shown")] {
            sqlx::query(r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?1 WHERE "Id" = ?2"#)
                .bind(guid_to_db(primary))
                .bind(guid_to_db(version))
                .execute(db.writer())
                .await
                .unwrap();
            seed_item_genre(&db, version, genre).await;
        }

        let genres = repository
            .get_genres(&InternalItemsQuery::default())
            .await
            .expect("genres");
        assert_eq!(genres.items.len(), 1);
        assert_eq!(genres.items[0].item.name.as_deref(), Some("Shown"));
        assert_eq!(genres.items[0].counts.movie_count, 1);
        assert_eq!(genres.total_record_count, 1);
    }

    /// Port of v12.2 `BaseItemRepositoryByNameItemCountsTests`
    /// `GetAllArtists_AlbumCreditedAsArtistAndAlbumArtist_CountsTheAlbumOnce`:
    /// `rawCounts` counts distinct items (`ByName.cs:366-368`), so an album
    /// credited under both artist value types counts once.
    #[tokio::test]
    async fn all_artists_count_an_album_credited_as_artist_and_album_artist_once() {
        let db = test_db().await;
        let repository = repo(&db);
        let album = Uuid::from_u128(0xA0C1);
        seed_named_item(&db, album, BaseItemKind::MusicAlbum, "Album").await;
        seed_item_value(&db, album, ItemValueType::Artist, "Abba").await;
        seed_item_value(&db, album, ItemValueType::AlbumArtist, "Abba").await;

        let artists = repository
            .get_all_artists(&InternalItemsQuery::default())
            .await
            .expect("all artists");
        assert_eq!(artists.items.len(), 1);
        assert_eq!(artists.items[0].counts.album_count, 1);
        assert_eq!(artists.items[0].counts.item_count, 1);
    }

    /// Port of v12.2 `GetAlbumArtists_TwoValueRowsCleaningToOneName_CountsTheAlbumOnce`
    /// and `GetArtists_TwoValueRowsCleaningToOneName_CountsTheAlbumOnce`: two
    /// value rows whose names clean to one `CleanValue` link one album, which
    /// counts once.
    #[tokio::test]
    async fn artists_count_an_album_under_two_spellings_of_one_name_once() {
        for value_type in [ItemValueType::AlbumArtist, ItemValueType::Artist] {
            let db = test_db().await;
            let repository = repo(&db);
            let album = Uuid::from_u128(0xA0C2);
            seed_named_item(&db, album, BaseItemKind::MusicAlbum, "Album").await;
            seed_item_value(&db, album, value_type, "Abba").await;
            seed_item_value(&db, album, value_type, "ABBA").await;

            let query = InternalItemsQuery::default();
            let artists = if value_type == ItemValueType::Artist {
                repository.get_artists(&query).await
            } else {
                repository.get_album_artists(&query).await
            }
            .expect("artists");
            assert_eq!(artists.items.len(), 1, "{value_type:?}");
            assert_eq!(artists.items[0].counts.album_count, 1, "{value_type:?}");
            assert_eq!(artists.items[0].counts.item_count, 1, "{value_type:?}");
        }
    }

    /// Port of v12.2 `GetGenres_TaggedBookOrBoxSet_CountsIt`: `ItemCountBuilder`
    /// counts books and box sets (`ItemCountBuilder.cs:68-75`), so they reach
    /// `ItemCount`.
    #[rstest::rstest]
    #[case::book(BaseItemKind::Book)]
    #[case::box_set(BaseItemKind::BoxSet)]
    #[tokio::test]
    async fn genres_count_a_tagged_book_or_box_set(#[case] kind: BaseItemKind) {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0xA0C3);
        seed_named_item(&db, item, kind, "Tagged").await;
        seed_item_genre(&db, item, "Drama").await;

        let genres = repository
            .get_genres(&InternalItemsQuery::default())
            .await
            .expect("genres");
        assert_eq!(genres.items.len(), 1);
        let counts = &genres.items[0].counts;
        let kind_count = if kind == BaseItemKind::Book {
            counts.book_count
        } else {
            counts.box_set_count
        };
        assert_eq!(kind_count, 1);
        assert_eq!(counts.item_count, 1);
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

    /// The by-name inner query's type filter is `TranslateQuery`'s (v12.2
    /// `TranslateQuery.cs:282-313`): named kinds that have no stored type name
    /// (the Live TV `Program`, `Recording`, `ChannelFolderItem`) still leave a
    /// `Type IN ()` that matches nothing, and the excluded kinds count only
    /// while no kind is included. The per-kind counts keep the excludes either
    /// way: their `scopeQuery` carries `ExcludeItemTypes` and no includes
    /// (`ByName.cs:336-346`).
    #[rstest::rstest]
    #[case::program(BaseItemKind::Program)]
    #[case::recording(BaseItemKind::Recording)]
    #[case::channel_folder_item(BaseItemKind::ChannelFolderItem)]
    #[tokio::test]
    async fn by_name_type_filter_follows_translate_query(#[case] unmapped: BaseItemKind) {
        assert_eq!(
            stored_type_name(unmapped),
            None,
            "{unmapped:?} has a stored name"
        );
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0xA0D1);
        seed_named_item(&db, movie, BaseItemKind::Movie, "One").await;
        seed_item_genre(&db, movie, "Drama").await;
        seed_item_value(&db, movie, ItemValueType::Studios, "Acme").await;
        let album = Uuid::from_u128(0xA0D2);
        seed_named_item(&db, album, BaseItemKind::MusicAlbum, "Album").await;
        seed_item_value(&db, album, ItemValueType::Artist, "Band").await;
        seed_item_value(&db, album, ItemValueType::AlbumArtist, "Band").await;

        let names = |result: QueryResult<ItemWithCounts>| -> (Vec<String>, i32) {
            let total = result.total_record_count;
            let names = result
                .items
                .into_iter()
                .filter_map(|i| i.item.name)
                .collect();
            (names, total)
        };
        let unmapped_only = InternalItemsQuery {
            include_item_types: vec![unmapped],
            enable_total_record_count: true,
            ..Default::default()
        };
        // `limit=0` takes the separate total statement (`count_by_name_total`).
        let probe = InternalItemsQuery {
            limit: Some(0),
            ..unmapped_only.clone()
        };
        for (route, result) in [
            ("genres", repository.get_genres(&unmapped_only).await),
            ("studios", repository.get_studios(&unmapped_only).await),
            ("artists", repository.get_artists(&unmapped_only).await),
            (
                "album artists",
                repository.get_album_artists(&unmapped_only).await,
            ),
            ("genres limit=0", repository.get_genres(&probe).await),
        ] {
            assert_eq!(names(result.expect(route)), (vec![], 0), "{route}");
        }
        // An unmapped kind beside a mapped one: the mapped one still counts.
        let mixed = InternalItemsQuery {
            include_item_types: vec![unmapped, BaseItemKind::Movie],
            ..unmapped_only.clone()
        };
        let mixed = repository.get_genres(&mixed).await.expect("mixed");
        assert_eq!(names(mixed), (vec!["Drama".to_owned()], 1));

        // Included and excluded at once: the include wins on the page, so the
        // value is listed, and the counts still drop the excluded kind.
        let both = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            exclude_item_types: vec![BaseItemKind::Movie],
            enable_total_record_count: true,
            ..Default::default()
        };
        let genres = repository.get_genres(&both).await.expect("both");
        assert_eq!(genres.total_record_count, 1);
        assert_eq!(genres.items.len(), 1);
        assert_eq!(genres.items[0].item.name.as_deref(), Some("Drama"));
        assert_eq!(genres.items[0].counts.movie_count, 0);
        let studios = repository.get_studios(&both).await.expect("studios both");
        assert_eq!(names(studios), (vec!["Acme".to_owned()], 1));
        // Excluded alone, the kind is gone from the page too.
        let excluded = InternalItemsQuery {
            exclude_item_types: vec![BaseItemKind::Movie],
            enable_total_record_count: true,
            ..Default::default()
        };
        let excluded = repository.get_genres(&excluded).await.expect("excluded");
        assert_eq!(names(excluded), (vec![], 0));
    }

    /// `studioIds` matches through the referenced row's `CleanName` (v12.2
    /// `WhereReferencedItem`), so an id that names no row, the API's stand-in
    /// for a `studios` name with no studio, matches nothing, on an item browse
    /// as on a by-name list. On `/Artists` the filter lands on the artist row
    /// itself (`ByName.cs:221`), which carries no studios: even a real studio
    /// empties the list.
    #[tokio::test]
    async fn studio_ids_naming_no_row_match_nothing() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0xA0E1);
        seed_named_item(&db, movie, BaseItemKind::Movie, "One").await;
        seed_item_value(&db, movie, ItemValueType::Studios, "Acme").await;
        let album = Uuid::from_u128(0xA0E2);
        seed_named_item(&db, album, BaseItemKind::MusicAlbum, "Album").await;
        seed_item_value(&db, album, ItemValueType::Artist, "Band").await;
        let studios = repository
            .get_studios(&InternalItemsQuery::default())
            .await
            .expect("studios");
        let acme = Uuid::parse_str(&studios.items[0].item.id).expect("studio id");

        let with_studios = |ids: Vec<Uuid>| InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            studio_ids: ids,
            ..Default::default()
        };
        let movies = |result: QueryResult<BaseItemEntity>| -> Vec<String> {
            result.items.into_iter().filter_map(|i| i.name).collect()
        };
        let real = repository.get_items(&with_studios(vec![acme])).await;
        assert_eq!(movies(real.expect("real studio")), ["One"]);
        let unknown = repository.get_items(&with_studios(vec![Uuid::nil()])).await;
        assert!(movies(unknown.expect("unknown studio")).is_empty());

        let all = repository
            .get_artists(&InternalItemsQuery::default())
            .await
            .expect("artists");
        assert_eq!(all.items.len(), 1);
        for ids in [vec![acme], vec![Uuid::nil()], vec![acme, Uuid::nil()]] {
            let query = InternalItemsQuery {
                studio_ids: ids.clone(),
                ..Default::default()
            };
            let artists = repository.get_artists(&query).await.expect("artists");
            assert!(artists.items.is_empty(), "{ids:?}");
        }
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

        // nameStartsWith is a prefix filter on the by-name row. An unpaged
        // response still carries the total: v12 assigns
        // `representativeIds.Count` whenever `EnableTotalRecordCount`
        // (`ByName.cs:227-230`) — 10.11.8's "no limit → no total" is gone.
        let starts = InternalItemsQuery {
            name_starts_with: Some("A".to_owned()),
            ..Default::default()
        };
        let a = repository.get_genres(&starts).await.expect("starts");
        let a_names: Vec<_> = a.items.iter().filter_map(|i| i.item.name.clone()).collect();
        assert_eq!(a_names, vec!["Action", "Adventure"]);
        assert_eq!(a.total_record_count, 2);

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
        let search = with_item_counts(InternalItemsQuery {
            search_term: Some("ram".to_owned()),
            ..Default::default()
        });
        let s = repository.get_genres(&search).await.expect("search");
        assert_eq!(s.items.len(), 1);
        assert_eq!(s.items[0].item.name.as_deref(), Some("Drama"));
        assert_eq!(s.items[0].counts.movie_count, 2);
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

    /// Gives the named rows a `SortName`/`CleanName` (the lowercased name) and
    /// the `DateCreated` of their position in `oldest_first`, so a sort by
    /// `DateCreated` and a sort by name disagree.
    async fn date_rows_by_name(db: &Database, oldest_first: &[&str]) {
        for (day, name) in (1..).zip(oldest_first) {
            sqlx::query(
                r#"UPDATE "BaseItems" SET "SortName" = lower("Name"), "CleanName" = lower("Name"),
                   "DateCreated" = ?1 WHERE "Name" = ?2"#,
            )
            .bind(format!("2020-01-{day:02} 00:00:00"))
            .bind(*name)
            .execute(db.writer())
            .await
            .expect("date row");
        }
    }

    /// The by-name search order is v12.2 `ApplyOrder`'s: relevance, then the
    /// caller's `sortBy`. 10.11.8 put `SortName, Name` between the two, so
    /// `/Genres?searchTerm=…&sortBy=DateCreated` came back in name order within
    /// each relevance band.
    #[tokio::test]
    async fn by_name_search_orders_equal_relevance_hits_by_the_callers_sort() {
        use ferrofin_model::dto::SortOrder;
        use ferrofin_model::live_tv::ItemSortBy;

        let db = test_db().await;
        let movie = Uuid::from_u128(0xA101);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Carrier").await;
        // Name order is Alpha, Bravo, Charlie; creation order is the reverse.
        let oldest_first = ["Blade Charlie", "Blade Bravo", "Blade Alpha"];
        for genre in oldest_first {
            seed_item_genre(&db, movie, genre).await;
        }
        date_rows_by_name(&db, &oldest_first).await;

        let genres = repo(&db)
            .get_genres(&InternalItemsQuery {
                search_term: Some("blade".to_owned()),
                order_by: vec![(ItemSortBy::DateCreated, SortOrder::Ascending)],
                ..Default::default()
            })
            .await
            .expect("genres");
        let names: Vec<_> = genres
            .items
            .iter()
            .filter_map(|i| i.item.name.as_deref())
            .collect();
        assert_eq!(names, oldest_first);
    }

    /// The same v12.2 rule on the item query (`/UserItems/Resume?searchTerm=…`
    /// orders by `DatePlayed` this way; `/Items` leads its keys with `SortName`
    /// in the handler): the relevance band leads, the caller's key orders
    /// within it.
    #[tokio::test]
    async fn item_search_orders_equal_relevance_hits_by_the_callers_sort() {
        use ferrofin_model::dto::SortOrder;
        use ferrofin_model::live_tv::ItemSortBy;

        let db = test_db().await;
        // "Blade" is the exact match (band 0) and stays first whatever the
        // sort; the other three share the word-prefix band.
        let oldest_first = ["Blade Alpha", "Blade Bravo", "Blade", "Blade Charlie"];
        for (n, name) in (0xA201_u128..).zip(oldest_first) {
            seed_named_item(&db, Uuid::from_u128(n), BaseItemKind::Movie, name).await;
        }
        date_rows_by_name(&db, &oldest_first).await;

        let found = repo(&db)
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Movie],
                search_term: Some("blade".to_owned()),
                order_by: vec![(ItemSortBy::DateCreated, SortOrder::Descending)],
                ..Default::default()
            })
            .await
            .expect("items");
        let names: Vec<_> = found.iter().filter_map(|i| i.name.as_deref()).collect();
        assert_eq!(
            names,
            ["Blade", "Blade Charlie", "Blade Bravo", "Blade Alpha"]
        );
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

        // No matching items → empty.
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

    /// Stores one stream row (`StreamType` discriminant, `Language`).
    async fn insert_stream(
        db: &Database,
        item: Uuid,
        index: i64,
        stream_type: MediaStreamType,
        language: Option<&str>,
    ) {
        sqlx::query(
            r#"INSERT INTO "MediaStreamInfos"
               ("ItemId", "StreamIndex", "IsDefault", "IsExternal", "IsForced",
                "StreamType", "Language")
               VALUES (?1, ?2, 0, 0, 0, ?3, ?4)"#,
        )
        .bind(guid_to_db(item))
        .bind(index)
        .bind(i64::from(media_stream_type_disc(stream_type)))
        .bind(language)
        .execute(db.writer())
        .await
        .expect("insert stream");
    }

    const LANG_MOVIES: Uuid = Uuid::from_u128(0xB101);
    const LANG_OTHER: Uuid = Uuid::from_u128(0xB102);
    const LANG_PRIMARY: Uuid = Uuid::from_u128(0xB103);
    const LANG_ALTERNATE: Uuid = Uuid::from_u128(0xB104);
    const LANG_ELSEWHERE: Uuid = Uuid::from_u128(0xB105);
    const LANG_USER: Uuid = Uuid::from_u128(0xB106);

    /// Two libraries. In the first, a movie merged with an alternate version
    /// the way the scanner stores versions (shared `PresentationUniqueKey`,
    /// the alternate's `PrimaryVersionId` naming the primary): the primary is
    /// English, the alternate French. The second library holds a Japanese movie.
    /// Upstream's `_versionedMovie` / `_alternateVersion` fixture
    /// (`BaseItemRepositoryStreamFilterTests.cs:33-36`).
    async fn versioned_language_library(db: &Database) -> UserEntity {
        seed_named_item(db, LANG_MOVIES, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(db, LANG_OTHER, BaseItemKind::CollectionFolder, "Other").await;
        for (id, name, library) in [
            (LANG_PRIMARY, "Merged", LANG_MOVIES),
            (LANG_ALTERNATE, "Merged", LANG_MOVIES),
            (LANG_ELSEWHERE, "Elsewhere", LANG_OTHER),
        ] {
            seed_top_parented_item(db, id, BaseItemKind::Movie, name, library).await;
        }
        for id in [LANG_PRIMARY, LANG_ALTERNATE] {
            sqlx::query(
                r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = 'merged' WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .expect("presentation key");
        }
        sqlx::query(r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?1 WHERE "Id" = ?2"#)
            .bind(guid_to_db(LANG_PRIMARY))
            .bind(guid_to_db(LANG_ALTERNATE))
            .execute(db.writer())
            .await
            .expect("primary version");
        insert_stream(db, LANG_PRIMARY, 0, MediaStreamType::Audio, Some("eng")).await;
        insert_stream(db, LANG_ALTERNATE, 0, MediaStreamType::Audio, Some("fre")).await;
        insert_stream(db, LANG_ELSEWHERE, 0, MediaStreamType::Audio, Some("jpn")).await;
        seed_user_with_defaults(db, LANG_USER).await
    }

    /// `GET /Items/Filters2`'s movie stream query for `user`.
    fn movie_stream_query(user: UserEntity, include_owned_items: bool) -> InternalItemsQuery {
        let mut query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            include_owned_items,
            ..InternalItemsQuery::default()
        };
        query.set_user(user);
        query
    }

    async fn sorted_audio(
        repository: &FerrofinItemRepository,
        query: &InternalItemsQuery,
    ) -> Vec<String> {
        let mut languages = repository
            .get_media_stream_languages(query, MediaStreamType::Audio)
            .await
            .expect("languages");
        languages.sort();
        languages
    }

    /// Both requested types come back from one call, and a requested type with
    /// no streams still has its (empty) entry.
    #[tokio::test]
    async fn media_stream_languages_by_type_answers_every_requested_type() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0xB201);
        seed_item(&db, item, BaseItemKind::Movie).await;
        insert_stream(&db, item, 0, MediaStreamType::Audio, Some("eng")).await;
        insert_stream(&db, item, 1, MediaStreamType::Subtitle, Some("fra")).await;
        insert_stream(&db, item, 2, MediaStreamType::Subtitle, None).await;

        let mut by_type = repository
            .get_media_stream_languages_by_type(
                &InternalItemsQuery::default(),
                &[
                    MediaStreamType::Audio,
                    MediaStreamType::Subtitle,
                    MediaStreamType::Lyric,
                ],
            )
            .await
            .expect("languages");
        let mut subtitles = by_type
            .remove(&MediaStreamType::Subtitle)
            .expect("subtitles");
        subtitles.sort();
        assert_eq!(
            by_type.remove(&MediaStreamType::Audio),
            Some(vec!["eng".to_owned()])
        );
        assert_eq!(subtitles, ["fra", "und"]);
        assert_eq!(by_type.remove(&MediaStreamType::Lyric), Some(Vec::new()));
        assert!(by_type.is_empty());
    }

    /// `/Items/Filters2?recursive=false` under a folder that is neither a view
    /// nor a library gives each facet query the folder as `Parent`, which
    /// upstream's `TranslateQuery` matches as a strict `ParentId` equality
    /// (v12.2 `TranslateQuery.cs:320-322`; the by-name `innerQueryFilter`
    /// carries it, `ByName.cs:188`). So the genre, music-genre and
    /// stream-language facets count the folder's direct children only: not a
    /// grandchild, and under a box set not its linked members.
    #[tokio::test]
    async fn filter_facets_under_a_parent_count_its_direct_children_only() {
        let db = test_db().await;
        let repository = repo(&db);
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0xA1D0)).await;
        let ids: Vec<_> = (0xA1D1..0xA1D9).map(Uuid::from_u128).collect();
        let (folder, sub, box_set) = (ids[0], ids[1], ids[2]);
        let (child, grandchild, elsewhere) = (ids[3], ids[4], ids[5]);
        let (song, grand_song) = (ids[6], ids[7]);
        seed_folder_item(&db, folder, BaseItemKind::Folder, "Kids", None).await;
        seed_folder_item(&db, sub, BaseItemKind::Folder, "Classics", Some(folder)).await;
        seed_folder_item(&db, box_set, BaseItemKind::BoxSet, "Set", None).await;
        seed_child_item(&db, child, BaseItemKind::Movie, "Film A", folder).await;
        seed_child_item(&db, grandchild, BaseItemKind::Movie, "Film B", sub).await;
        seed_named_item(&db, elsewhere, BaseItemKind::Movie, "Other Film").await;
        seed_child_item(&db, song, BaseItemKind::Audio, "Song", folder).await;
        seed_child_item(&db, grand_song, BaseItemKind::Audio, "Old Song", sub).await;
        seed_library_over(&db, &ids).await;
        for (item, genre) in [
            (child, "Animation"),
            (grandchild, "Western"),
            (elsewhere, "Drama"),
            (elsewhere, "Animation"),
            (song, "Jazz"),
            (grand_song, "Blues"),
        ] {
            seed_item_value(&db, item, ItemValueType::Genre, genre).await;
        }
        for (item, language) in [(child, "eng"), (grandchild, "fra"), (elsewhere, "jpn")] {
            insert_stream(&db, item, 0, MediaStreamType::Audio, Some(language)).await;
        }
        sqlx::query(
            r#"INSERT INTO "LinkedChildren" ("ParentId", "ChildId", "ChildType", "SortOrder")
               VALUES (?, ?, 0, 0)"#,
        )
        .bind(guid_to_db(box_set))
        .bind(guid_to_db(elsewhere))
        .execute(db.writer())
        .await
        .expect("link the set's member");

        let under = |parent: Uuid, kind: BaseItemKind| InternalItemsQuery {
            user: Some(user.clone()),
            include_item_types: vec![kind],
            parent_id: parent,
            physical_children_only: true,
            ..InternalItemsQuery::default()
        };
        let names = |result: QueryResult<ItemWithCounts>| -> Vec<String> {
            result
                .items
                .into_iter()
                .filter_map(|row| row.item.name)
                .collect()
        };
        let audio = |query: InternalItemsQuery| {
            let repository = &repository;
            async move {
                repository
                    .get_media_stream_languages(&query, MediaStreamType::Audio)
                    .await
                    .expect("languages")
            }
        };

        let movies = under(folder, BaseItemKind::Movie);
        let genres = repository.get_genres(&movies).await.expect("genres");
        // The per-kind counts carry the same parent (`scopeQuery`, `:346`).
        assert_eq!(
            genres
                .items
                .iter()
                .map(|row| row.counts.movie_count)
                .collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(names(genres), ["Animation"]);
        assert_eq!(audio(movies).await, ["eng"]);
        assert_eq!(
            names(
                repository
                    .get_music_genres(&under(folder, BaseItemKind::Audio))
                    .await
                    .expect("music genres")
            ),
            ["Jazz"]
        );
        let set = under(box_set, BaseItemKind::Movie);
        assert!(
            names(repository.get_genres(&set).await.expect("genres")).is_empty(),
            "a box set's linked member is not its child"
        );
        assert!(audio(set).await.is_empty());
    }

    /// Upstream's `BaseItemRepositoryItemValueTests.GetTagNames_GroupsAndFiltersItemValues`
    /// (Jellyfin 12.1, `ca06ccfa13`), the oracle for `/Items/Filters2`'s tags:
    /// one name per clean value (its smallest spelling), only `Tags` (not
    /// `InheritedTags` or a genre), only from the requested kinds.
    #[tokio::test]
    async fn tag_names_group_by_clean_value_and_keep_only_tags_of_the_kinds() {
        let db = test_db().await;
        let repository = repo(&db);
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0xA3D0)).await;
        let ids: Vec<_> = (0xA3D1..0xA3D4).map(Uuid::from_u128).collect();
        let (movie, other_movie, audio) = (ids[0], ids[1], ids[2]);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Movie").await;
        seed_named_item(&db, other_movie, BaseItemKind::Movie, "Other Movie").await;
        seed_named_item(&db, audio, BaseItemKind::Audio, "Excluded Audio").await;
        seed_library_over(&db, &ids).await;
        for (item, value_type, value) in [
            (movie, ItemValueType::Tags, "Alpha"),
            (movie, ItemValueType::Tags, "alpha"),
            (other_movie, ItemValueType::Tags, "Beta"),
            (movie, ItemValueType::InheritedTags, "Inherited"),
            (movie, ItemValueType::Genre, "Genre Leak"),
            (audio, ItemValueType::Tags, "Excluded Tag"),
        ] {
            seed_item_value(&db, item, value_type, value).await;
        }

        let tags = repository
            .get_tag_names(&InternalItemsQuery {
                user: Some(user),
                include_item_types: vec![BaseItemKind::Movie],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("tag names");
        assert_eq!(tags, ["Alpha", "Beta"]);
    }

    /// The tag facet honours the scope `/Items/Filters2` hands it: a parent's
    /// descendants (`AncestorIds`) or its direct children (a strict parent),
    /// and an unscoped query is confined to the libraries the user may open.
    #[tokio::test]
    async fn tag_names_follow_the_facet_scope() {
        let db = test_db().await;
        let repository = repo(&db);
        let ids: Vec<_> = (0xA5D1..0xA5D8).map(Uuid::from_u128).collect();
        let (folder, sub, child, grandchild, elsewhere) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
        let (hidden_library, hidden) = (ids[5], ids[6]);
        seed_folder_item(&db, folder, BaseItemKind::Folder, "Kids", None).await;
        seed_folder_item(&db, sub, BaseItemKind::Folder, "Classics", Some(folder)).await;
        seed_child_item(&db, child, BaseItemKind::Movie, "Film A", folder).await;
        seed_child_item(&db, grandchild, BaseItemKind::Movie, "Film B", sub).await;
        seed_named_item(&db, elsewhere, BaseItemKind::Movie, "Other Film").await;
        let library = seed_library_over(&db, &ids[..5]).await;
        seed_named_item(
            &db,
            hidden_library,
            BaseItemKind::CollectionFolder,
            "Hidden",
        )
        .await;
        seed_top_parented_item(
            &db,
            hidden,
            BaseItemKind::Movie,
            "Hidden Film",
            hidden_library,
        )
        .await;
        for (item, ancestor) in [
            (sub, folder),
            (child, folder),
            (grandchild, sub),
            (grandchild, folder),
        ] {
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?, ?)"#)
                .bind(guid_to_db(item))
                .bind(guid_to_db(ancestor))
                .execute(db.writer())
                .await
                .expect("ancestor row");
        }
        for (item, tag) in [
            (child, "Near"),
            (grandchild, "Deep"),
            (elsewhere, "Far"),
            (hidden, "Hidden"),
        ] {
            seed_item_value(&db, item, ItemValueType::Tags, tag).await;
        }
        let user_id = Uuid::from_u128(0xA5D0);
        let user = seed_user_with_defaults(&db, user_id).await;
        crate::user_entity_ext::set_permission(
            db.pool(),
            &user.id,
            PermissionKind::EnableAllFolders,
            false,
        )
        .await
        .expect("restrict the user");
        set_folder_preference(&db, user_id, PreferenceKind::EnabledFolders, &[library]).await;

        let base = InternalItemsQuery {
            user: Some(user),
            include_item_types: vec![BaseItemKind::Movie],
            ..InternalItemsQuery::default()
        };
        let tags = |query: InternalItemsQuery| {
            let repository = &repository;
            async move { repository.get_tag_names(&query).await.expect("tag names") }
        };
        assert_eq!(tags(base.clone()).await, ["Deep", "Far", "Near"]);
        assert_eq!(
            tags(InternalItemsQuery {
                ancestor_ids: vec![folder],
                ..base.clone()
            })
            .await,
            ["Deep", "Near"]
        );
        assert_eq!(
            tags(InternalItemsQuery {
                parent_id: folder,
                physical_children_only: true,
                ..base
            })
            .await,
            ["Near"]
        );
    }

    /// Plan pin for the tag facet (`/Items/Filters2`; the legacy facet set's
    /// genres and tags run the same statement): the `CROSS JOIN` order holds,
    /// the type's values first, then their map rows, then each item by a point
    /// probe on its `Id` (the primary key, or under a user scope
    /// `IX_BaseItems_Type_TopParentId_Id` ending in `Id=?`).
    #[tokio::test]
    async fn item_values_statement_walks_values_then_map_then_items() {
        use sqlx::{Execute, Row};
        let db = test_db().await;
        let libraries = [Uuid::from_u128(0xA4D1), Uuid::from_u128(0xA4D2)];
        // Unscoped, under a strict parent, and a user-scoped statement
        // (`TopParentId IN (…)`) with an ancestor scope.
        for (parent, top_parent_ids, ancestor_ids) in [
            (Uuid::nil(), vec![], vec![]),
            (Uuid::from_u128(0xA4D0), vec![], vec![]),
            (Uuid::nil(), libraries.to_vec(), vec![libraries[0]]),
        ] {
            let filter = InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Movie],
                parent_id: parent,
                physical_children_only: !parent.is_nil(),
                top_parent_ids,
                ancestor_ids,
                ..InternalItemsQuery::default()
            };
            let mut qb = item_values_statement(&filter, ItemValueType::Tags);
            let mut query = qb.build();
            let args = query.take_arguments().expect("arguments").expect("binds");
            let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
            let plan = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .fetch_all(db.pool())
                .await
                .expect("plan")
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect::<Vec<_>>();
            let at = |prefix: &str| {
                plan.iter()
                    .position(|d| d.starts_with(prefix))
                    .unwrap_or_else(|| panic!("no {prefix}: {plan:#?}"))
            };
            let (iv, ivm, bi) = (at("SEARCH iv "), at("SEARCH ivm "), at("SEARCH bi "));
            assert!(iv < ivm && ivm < bi, "{plan:#?}");
            // A point probe by the map row's item: the primary key, or under a
            // user scope `IX_BaseItems_Type_TopParentId_Id` ending in `Id=?` (a
            // `TopParentId=?` range of the library's items would not do).
            assert!(
                plan[bi].ends_with("(Id=?)") || plan[bi].ends_with(" AND Id=?)"),
                "{plan:#?}"
            );
            assert!(!plan.iter().any(|d| d.starts_with("SCAN ")), "{plan:#?}");
        }
    }

    /// Plan pin for the by-name semijoin under a parent (`/Items/Filters2?recursive=false`
    /// under a folder): each of the value's map rows reaches its item by the
    /// item's primary key, with or without a kind filter. Driven from the
    /// parent's children instead (`IX_BaseItems_ParentId`), every candidate
    /// value walked the folder's whole child list.
    #[tokio::test]
    async fn by_name_semijoin_under_a_parent_probes_items_by_id() {
        use sqlx::{Execute, Row};
        let db = test_db().await;
        let filter = InternalItemsQuery {
            parent_id: Uuid::from_u128(0xA2D0),
            ..InternalItemsQuery::default()
        };
        let type_ints = [i64::from(i32::from(ItemValueType::Genre))];
        let movie = [stored_type_name(BaseItemKind::Movie)
            .expect("stored")
            .to_owned()];
        for content_type_names in [Some(&movie[..]), None] {
            let scope = ByNameScope {
                filter: &filter,
                type_ints: &type_ints,
                content_type_names,
                exclude_type_names: &[],
                media_types: &[],
                ancestors: &[],
                top_parents: &[],
            };
            let mut qb: QueryBuilder<Sqlite> =
                QueryBuilder::new(r#"SELECT bi."Id" FROM "BaseItems" AS bi WHERE bi."Type" = "#);
            qb.push_bind(stored_type_name(BaseItemKind::Genre).expect("stored"));
            push_value_exists(&mut qb, &scope, InnerScope::Page);
            let mut query = qb.build();
            let args = query.take_arguments().expect("arguments").expect("binds");
            let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
            let plan = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .fetch_all(db.pool())
                .await
                .expect("plan")
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect::<Vec<_>>();
            assert!(
                plan.iter()
                    .any(|d| d == "SEARCH ci USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)"),
                "{plan:#?}"
            );
            assert!(
                !plan.iter().any(|d| d.contains("IX_BaseItems_ParentId")),
                "{plan:#?}"
            );
        }
    }

    /// The grouping regression: with a user set, the `build_query` id list
    /// grouped the version pair by `PresentationUniqueKey` and kept only the
    /// primary, so the alternate's French never reached the facet. Upstream
    /// runs `TranslateQuery` alone and `IncludeOwnedItems` admits the alternate.
    #[tokio::test]
    async fn media_stream_languages_include_an_alternate_versions_language() {
        let db = test_db().await;
        let repository = repo(&db);
        let user = versioned_language_library(&db).await;
        assert_eq!(
            sorted_audio(&repository, &movie_stream_query(user, true)).await,
            ["eng", "fre", "jpn"]
        );
    }

    /// Without `IncludeOwnedItems` the alternate version stays hidden behind
    /// its primary (`ApplyAlternateVersionFiltering`): the flag is what admits it.
    #[tokio::test]
    async fn media_stream_languages_hide_the_alternate_without_owned_items() {
        let db = test_db().await;
        let repository = repo(&db);
        let user = versioned_language_library(&db).await;
        assert_eq!(
            sorted_audio(&repository, &movie_stream_query(user, false)).await,
            ["eng", "jpn"]
        );
    }

    /// `AddUserToQuery`: a library the user cannot see contributes nothing.
    #[tokio::test]
    async fn media_stream_languages_skip_a_library_the_user_cannot_see() {
        let db = test_db().await;
        let repository = repo(&db);
        let user = versioned_language_library(&db).await;
        set_folder_preference(
            &db,
            LANG_USER,
            PreferenceKind::BlockedMediaFolders,
            &[LANG_OTHER],
        )
        .await;
        assert_eq!(
            sorted_audio(&repository, &movie_stream_query(user, true)).await,
            ["eng", "fre"]
        );
    }

    /// Only fields upstream copies into its fresh query count: a caller's
    /// `IsVirtualItem` / `Years` / paging do not narrow the facet.
    #[tokio::test]
    async fn media_stream_languages_ignore_fields_upstream_does_not_copy() {
        let db = test_db().await;
        let repository = repo(&db);
        let user = versioned_language_library(&db).await;
        let query = InternalItemsQuery {
            years: vec![1900],
            is_virtual_item: Some(true),
            limit: Some(1),
            ..movie_stream_query(user, true)
        };
        assert_eq!(
            sorted_audio(&repository, &query).await,
            ["eng", "fre", "jpn"]
        );
    }

    /// C# `string.IsNullOrEmpty`: only NULL and `""` become `und`; whitespace
    /// is a language of its own. The DISTINCT is BINARY, so case variants stay
    /// apart.
    #[tokio::test]
    async fn media_stream_languages_map_only_null_and_empty_to_und() {
        let db = test_db().await;
        let repository = repo(&db);
        let item = Uuid::from_u128(0xB301);
        seed_item(&db, item, BaseItemKind::Movie).await;
        for (index, language) in [
            (0, Some(" ")),
            (1, Some("ENG")),
            (2, Some("eng")),
            (3, Some("")),
            (4, None),
            (5, Some("eng")),
        ] {
            insert_stream(&db, item, index, MediaStreamType::Audio, language).await;
        }
        assert_eq!(
            sorted_audio(&repository, &InternalItemsQuery::default()).await,
            [" ", "ENG", "eng", "und"]
        );
    }

    /// The facet statement reaches `MediaStreamInfos` through an index seek,
    /// never a full scan of the stream table, and `BaseItems` through the
    /// user-scoped top-parent index.
    #[tokio::test]
    async fn media_stream_languages_plan_seeks_the_stream_index() {
        use sqlx::{Execute, Row};
        let db = test_db().await;
        let repository = repo(&db);
        let user = versioned_language_library(&db).await;
        let narrowed = stream_language_query(&movie_stream_query(user, true));
        let resolved = repository.resolve_views(&narrowed).await.expect("resolve");
        let filter = resolved.as_ref().unwrap_or(&narrowed);
        assert!(!filter.top_parent_ids.is_empty(), "user scope applied");
        let discs = [MediaStreamType::Audio, MediaStreamType::Subtitle]
            .map(|t| i64::from(media_stream_type_disc(t)));
        let mut builder = stream_languages_statement(filter, discs);
        let mut query = builder.build();
        let args = query.take_arguments().unwrap().unwrap();
        let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
        let plan = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
            .fetch_all(db.pool())
            .await
            .unwrap()
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>()
            .join(" | ");
        // The stream probe is the covering `(StreamType, ItemId, Language, …)`
        // index, seeking each matched id; the item set is an index seek too.
        assert!(
            plan.contains(
                "SEARCH ms USING COVERING INDEX \
                 IX_MediaStreamInfos_StreamType_ItemId_Language_IsExternal"
            ),
            "{plan}"
        );
        assert!(plan.contains("SEARCH bi USING"), "{plan}");
        assert!(!plan.contains("SCAN"), "{plan}");
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
        crate::user_entity_ext::set_preference(
            db.writer(),
            &user.id,
            ferrofin_db::enums::PreferenceKind::BlockUnratedItems,
            &["Series".to_owned()],
        )
        .await
        .unwrap();
        assert!(
            repository
                .get_is_played(&user, parent, false)
                .await
                .unwrap(),
            "hidden unrated episodes do not keep a folder unplayed"
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

    /// Only `tvshows` and `music` take a dedicated path here; everything else
    /// returns nothing and the user-view manager routes it through
    /// `get_item_list`. `books` is the C# early exit; `movies` is NOT — v12
    /// has a movies arm, deliberately not ported (the owner-scoped divergence
    /// recorded in `user_view_manager::items_for_latest_items`), so this case
    /// also pins that decision.
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

    /// A season's id and its episodes' `(id, DateCreated)`.
    type ShowSeason<'a> = (Uuid, &'a [(Uuid, chrono::DateTime<Utc>)]);

    /// A show as a scan stores it: the `Series` row, one `Season` row per
    /// season, and its episodes, each carrying `SeriesName`, `SeriesId` and
    /// `SeasonId`, created at the given instants.
    async fn seed_latest_show(db: &Database, series: Uuid, name: &str, seasons: &[ShowSeason<'_>]) {
        let persistence =
            crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone());
        let mut rows = vec![BaseItemEntity {
            id: guid_to_db(series),
            type_: stored_type_name(BaseItemKind::Series)
                .unwrap_or_default()
                .to_owned(),
            name: Some(name.to_owned()),
            sort_name: Some(name.to_owned()),
            is_folder: true,
            date_created: Some(day(1)),
            ..BaseItemEntity::default()
        }];
        for (n, (season, episodes)) in seasons.iter().enumerate() {
            rows.push(BaseItemEntity {
                id: guid_to_db(*season),
                type_: stored_type_name(BaseItemKind::Season)
                    .unwrap_or_default()
                    .to_owned(),
                name: Some(format!("Season {}", n + 1)),
                series_id: Some(guid_to_db(series)),
                series_name: Some(name.to_owned()),
                is_folder: true,
                date_created: Some(day(1)),
                ..BaseItemEntity::default()
            });
            for (e, (episode, created)) in episodes.iter().enumerate() {
                rows.push(BaseItemEntity {
                    id: guid_to_db(*episode),
                    type_: stored_type_name(BaseItemKind::Episode)
                        .unwrap_or_default()
                        .to_owned(),
                    name: Some(format!("{name} S{}E{}", n + 1, e + 1)),
                    sort_name: Some(format!("{name} s{}e{}", n + 1, e + 1)),
                    series_id: Some(guid_to_db(series)),
                    season_id: Some(guid_to_db(*season)),
                    series_name: Some(name.to_owned()),
                    date_created: Some(*created),
                    media_type: Some("Video".to_owned()),
                    ..BaseItemEntity::default()
                });
            }
        }
        persistence.save_items(&rows).await.expect("seed show");
    }

    fn hour(day_of_month: u32, hour: u32) -> chrono::DateTime<Utc> {
        day(day_of_month) + chrono::Duration::hours(i64::from(hour))
    }

    /// The jellyfin-web "Latest in TV" filter, as `GetItemsForLatestItems`
    /// builds it for a grouped tvshows request.
    fn latest_tv_filter(limit: i32) -> InternalItemsQuery {
        InternalItemsQuery {
            is_folder: Some(false),
            is_virtual_item: Some(false),
            media_types: vec![ferrofin_model::data::MediaType::Video],
            limit: Some(limit),
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
        }
    }

    /// v12 `GetLatestTvShowItems` (`BaseItemRepository.Querying.cs:282-540`):
    /// the newest `limit` series by their newest episode, and for each one
    /// card chosen from the episodes added within 24 hours of that newest one:
    ///
    /// - one recent episode → the **Episode**;
    /// - several in one season of a multi-season series → the **Season**;
    /// - several in the only season → the **Series**;
    /// - recent episodes across seasons → the **Series**.
    ///
    /// The cards come back newest first. An older episode outside the window
    /// does not count toward its series' recent additions.
    #[tokio::test]
    async fn latest_tv_items_pick_the_episode_season_or_series_card() {
        let db = test_db().await;
        let repository = repo(&db);
        let id = |n: u128| Uuid::from_u128(0xD100 + n);
        // Show A: two seasons, one new episode in season 2 beside an older
        // one there, outside the window → the Episode. (Were it the season's
        // only episode, the whole season would be new: the Season.)
        seed_latest_show(
            &db,
            id(0x10),
            "Alpha",
            &[
                (id(0x11), &[(id(0x12), day(2))]),
                (id(0x13), &[(id(0x15), day(3)), (id(0x14), hour(9, 6))]),
            ],
        )
        .await;
        // Show B: two seasons, three new episodes in season 2 → the Season.
        seed_latest_show(
            &db,
            id(0x20),
            "Bravo",
            &[
                (id(0x21), &[(id(0x22), day(2))]),
                (
                    id(0x23),
                    &[
                        (id(0x24), hour(9, 1)),
                        (id(0x25), hour(9, 2)),
                        (id(0x26), hour(9, 3)),
                    ],
                ),
            ],
        )
        .await;
        // Show C: one season, two new episodes → the Series.
        seed_latest_show(
            &db,
            id(0x30),
            "Charlie",
            &[(id(0x31), &[(id(0x32), hour(8, 4)), (id(0x33), hour(8, 5))])],
        )
        .await;
        // Show D: new episodes in two seasons → the Series.
        seed_latest_show(
            &db,
            id(0x40),
            "Delta",
            &[
                (id(0x41), &[(id(0x42), hour(7, 1))]),
                (id(0x43), &[(id(0x44), hour(7, 2))]),
            ],
        )
        .await;

        let rows = repository
            .get_latest_item_list(&latest_tv_filter(16), CollectionType::tvshows)
            .await
            .expect("latest tvshows");
        let cards: Vec<(Uuid, String)> = rows
            .iter()
            .map(|r| {
                (
                    Uuid::parse_str(&r.id).unwrap(),
                    r.type_.rsplit('.').next().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            cards,
            [
                (id(0x14), "Episode".to_owned()),
                (id(0x23), "Season".to_owned()),
                (id(0x30), "Series".to_owned()),
                (id(0x40), "Series".to_owned()),
            ]
        );

        // `limit` caps the series considered, newest first.
        let top_two = repository
            .get_latest_item_list(&latest_tv_filter(2), CollectionType::tvshows)
            .await
            .expect("latest tvshows, two");
        let ids: Vec<Uuid> = top_two
            .iter()
            .map(|r| Uuid::parse_str(&r.id).unwrap())
            .collect();
        assert_eq!(ids, [id(0x14), id(0x23)]);

        // No series at all (`limit` 0) answers nothing.
        let none = repository
            .get_latest_item_list(&latest_tv_filter(0), CollectionType::tvshows)
            .await
            .expect("latest tvshows, none");
        assert!(none.is_empty());
    }

    /// A Season card the user's parental limit hides is replaced by the
    /// series' newest episode (`GetLatestTvShowItems` step 5b,
    /// `Querying.cs:458-490`): a matching episode does not make its container
    /// visible.
    #[tokio::test]
    async fn latest_tv_items_show_the_episode_when_the_season_is_hidden() {
        let db = test_db().await;
        let repository = repo(&db);
        let id = |n: u128| Uuid::from_u128(0xD200 + n);
        seed_latest_show(
            &db,
            id(0x10),
            "Bravo",
            &[
                (id(0x11), &[(id(0x12), day(2))]),
                (id(0x13), &[(id(0x14), hour(9, 1)), (id(0x15), hour(9, 2))]),
            ],
        )
        .await;
        // The season is rated above the limit; its episodes carry no rating.
        sqlx::query(
            r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue" = 18 WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id(0x13)))
        .execute(db.writer())
        .await
        .expect("rate the season");
        let card = |rows: Vec<BaseItemEntity>| -> Vec<(Uuid, String)> {
            rows.iter()
                .map(|r| {
                    (
                        Uuid::parse_str(&r.id).unwrap(),
                        r.type_.rsplit('.').next().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        };

        let open = repository
            .get_latest_item_list(&latest_tv_filter(16), CollectionType::tvshows)
            .await
            .expect("latest tvshows");
        assert_eq!(card(open), [(id(0x13), "Season".to_owned())]);

        let limited = InternalItemsQuery {
            max_parental_rating: Some(ferrofin_model::entities_media::ParentalRatingScore {
                score: 13,
                sub_score: None,
            }),
            ..latest_tv_filter(16)
        };
        let hidden = repository
            .get_latest_item_list(&limited, CollectionType::tvshows)
            .await
            .expect("latest tvshows, limited");
        assert_eq!(card(hidden), [(id(0x15), "Episode".to_owned())]);
    }

    /// Seeds a `MusicAlbum` row the way the scanner writes one — the library it
    /// belongs to in `TopParentId`, `IsFolder` set — plus one track under it, so
    /// a test can tell "the newest albums" (what v12 answers) apart from
    /// "the albums holding the newest tracks" (what 10.11.8 answered).
    async fn seed_latest_album(
        db: &Database,
        id: Uuid,
        name: &str,
        top_parent: Option<Uuid>,
        created: chrono::DateTime<Utc>,
    ) {
        let persistence =
            crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone());
        persistence
            .save_items(&[BaseItemEntity {
                id: guid_to_db(id),
                type_: stored_type_name(BaseItemKind::MusicAlbum)
                    .unwrap_or_default()
                    .to_owned(),
                name: Some(name.to_owned()),
                sort_name: Some(name.to_owned()),
                is_folder: true,
                top_parent_id: top_parent.map(guid_to_db),
                date_created: Some(created),
                ..BaseItemEntity::default()
            }])
            .await
            .expect("seed latest album");
    }

    /// The music library id the album tests scope to.
    fn music_library() -> Uuid {
        Uuid::from_u128(0xD2FF)
    }

    /// v12's music arm returns the newest ALBUMS, ordered `DateCreated DESC`,
    /// and `limit` caps albums. The three albums here are seeded with tracks
    /// whose dates run the OTHER way, which is exactly the case the 10.11.8
    /// grouped-threshold query got right and v12 deliberately changed: the
    /// answer follows the album rows, not the tracks.
    #[rstest::rstest]
    #[case(3, vec![0xD201, 0xD202, 0xD203])]
    #[case(2, vec![0xD201, 0xD202])]
    #[case(1, vec![0xD201])]
    // `Take(0)` upstream — no albums, not "every album".
    #[case(0, vec![])]
    #[tokio::test]
    async fn latest_item_list_returns_newest_albums_for_music(
        #[case] limit: i32,
        #[case] expected: Vec<u128>,
    ) {
        let db = test_db().await;
        let repository = repo(&db);
        let library = music_library();
        // Album X newest (day 8) … Album Z oldest (day 4).
        for (id, name, created, track_day) in [
            (0xD201_u128, "Album X", day(8), 4_u32),
            (0xD202, "Album Y", day(6), 5),
            (0xD203, "Album Z", day(4), 8),
        ] {
            seed_latest_album(&db, Uuid::from_u128(id), name, Some(library), created).await;
            // A track whose own DateCreated contradicts its album's ordering.
            seed_latest_row(
                &db,
                Uuid::from_u128(id + 0x1000),
                BaseItemKind::Audio,
                &format!("{name} t1"),
                name,
                day(track_day),
            )
            .await;
        }

        let filter = InternalItemsQuery {
            include_item_types: vec![],
            media_types: vec![ferrofin_model::data::MediaType::Audio],
            is_folder: Some(false),
            is_virtual_item: Some(false),
            limit: Some(limit),
            top_parent_ids: vec![library],
            order_by: vec![(
                ferrofin_model::live_tv::ItemSortBy::DateCreated,
                ferrofin_model::dto::SortOrder::Descending,
            )],
            ..InternalItemsQuery::default()
        };
        let rows = repository
            .get_latest_item_list(&filter, CollectionType::music)
            .await
            .expect("latest music albums");
        let ids: Vec<Uuid> = rows
            .iter()
            .filter_map(|r| Uuid::parse_str(&r.id).ok())
            .collect();
        assert_eq!(
            ids,
            expected
                .into_iter()
                .map(Uuid::from_u128)
                .collect::<Vec<_>>(),
            "the newest {limit} albums, in DateCreated order"
        );
        assert!(
            rows.iter()
                .all(|r| r.type_.ends_with("MusicAlbum") && r.is_folder),
            "the music arm returns album rows, not tracks"
        );
    }

    /// `GetLatestItemList` runs `PrepareFilterQuery` too: grouped by metadata
    /// key, a limit of 1 reads 5 albums.
    #[rstest::rstest]
    #[case::grouped(true, 5)]
    #[case::not_grouped(false, 1)]
    #[tokio::test]
    async fn latest_albums_grouped_by_metadata_key_read_four_past_the_limit(
        #[case] grouped: bool,
        #[case] albums: usize,
    ) {
        let db = test_db().await;
        let repository = repo(&db);
        let library = music_library();
        for i in 0..6_u32 {
            let id = 0xD301_u128 + u128::from(i);
            let name = format!("Album {i}");
            seed_latest_album(&db, Uuid::from_u128(id), &name, Some(library), day(i + 1)).await;
            seed_latest_row(
                &db,
                Uuid::from_u128(id + 0x1000),
                BaseItemKind::Audio,
                &format!("{name} t1"),
                &name,
                day(i + 1),
            )
            .await;
        }
        let filter = InternalItemsQuery {
            media_types: vec![ferrofin_model::data::MediaType::Audio],
            is_folder: Some(false),
            is_virtual_item: Some(false),
            limit: Some(1),
            top_parent_ids: vec![library],
            order_by: vec![(
                ferrofin_model::live_tv::ItemSortBy::DateCreated,
                ferrofin_model::dto::SortOrder::Descending,
            )],
            enable_group_by_metadata_key: grouped,
            ..InternalItemsQuery::default()
        };

        let rows = repository
            .get_latest_item_list(&filter, CollectionType::music)
            .await
            .expect("latest music albums");
        assert_eq!(rows.len(), albums);
    }

    /// `Id DESC` is the tiebreaker on equal `DateCreated`
    /// (`OrderByDescending(DateCreated).ThenByDescending(Id)`), and a virtual
    /// album — an NFO-declared release with no files — never counts as new.
    #[tokio::test]
    async fn latest_music_albums_break_date_ties_by_id_and_skip_virtual_albums() {
        let db = test_db().await;
        let repository = repo(&db);
        let library = music_library();
        let (low, high) = (Uuid::from_u128(0xD211), Uuid::from_u128(0xD212));
        seed_latest_album(&db, low, "Album low id", Some(library), day(5)).await;
        seed_latest_album(&db, high, "Album high id", Some(library), day(5)).await;

        // Same date, but virtual: excluded by `!album.IsVirtualItem`.
        let virt = Uuid::from_u128(0xD213);
        seed_latest_album(&db, virt, "Album virtual", Some(library), day(9)).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "IsVirtualItem" = 1 WHERE "Id" = ?1"#)
            .bind(guid_to_db(virt))
            .execute(db.writer())
            .await
            .expect("mark virtual");

        // Same date again, but in another library: out of scope.
        seed_latest_album(
            &db,
            Uuid::from_u128(0xD214),
            "Album elsewhere",
            Some(Uuid::from_u128(0xD2FE)),
            day(9),
        )
        .await;

        let rows = repository
            .get_latest_item_list(
                &InternalItemsQuery {
                    limit: Some(10),
                    top_parent_ids: vec![library],
                    ..InternalItemsQuery::default()
                },
                CollectionType::music,
            )
            .await
            .expect("latest music albums");
        let ids: Vec<Uuid> = rows
            .iter()
            .filter_map(|r| Uuid::parse_str(&r.id).ok())
            .collect();
        assert_eq!(
            ids,
            vec![high, low],
            "the higher id wins the DateCreated tie"
        );
    }

    /// A library with no albums answers empty rather than widening: the top
    /// parent scope is a real predicate, not a hint.
    #[tokio::test]
    async fn latest_music_albums_are_empty_for_a_library_with_none() {
        let db = test_db().await;
        let repository = repo(&db);
        seed_latest_album(
            &db,
            Uuid::from_u128(0xD221),
            "Album X",
            Some(music_library()),
            day(8),
        )
        .await;

        let rows = repository
            .get_latest_item_list(
                &InternalItemsQuery {
                    limit: Some(16),
                    top_parent_ids: vec![Uuid::from_u128(0xD2FD)],
                    ..InternalItemsQuery::default()
                },
                CollectionType::music,
            )
            .await
            .expect("latest music albums");
        assert!(rows.is_empty(), "another library's albums must not leak in");
    }

    /// The ancestor-scoped fallback (`Querying.cs:159-167`): with no top parent
    /// ids the albums come from the `AncestorIds` parents of the rows the
    /// caller's own query matches — so an artist-folder parent still answers
    /// with albums.
    #[tokio::test]
    async fn latest_music_albums_fall_back_to_the_ancestor_closure() {
        let db = test_db().await;
        let repository = repo(&db);
        let (album, other_album) = (Uuid::from_u128(0xD231), Uuid::from_u128(0xD232));
        seed_latest_album(&db, album, "Album X", None, day(3)).await;
        seed_latest_album(&db, other_album, "Album Y", None, day(9)).await;

        // One track, under `album` only — `other_album` has no matching row and
        // so must not come back despite its newer date.
        let track = Uuid::from_u128(0xD233);
        seed_latest_row(&db, track, BaseItemKind::Audio, "X t1", "Album X", day(1)).await;
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(track))
            .bind(guid_to_db(album))
            .execute(db.writer())
            .await
            .expect("seed ancestor");

        let rows = repository
            .get_latest_item_list(
                &InternalItemsQuery {
                    limit: Some(16),
                    ancestor_ids: vec![album],
                    ..InternalItemsQuery::default()
                },
                CollectionType::music,
            )
            .await
            .expect("latest music albums");
        let ids: Vec<Uuid> = rows
            .iter()
            .filter_map(|r| Uuid::parse_str(&r.id).ok())
            .collect();
        assert_eq!(ids, vec![album], "only the album above a matching track");
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
            .bind(extra.json_value())
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

        // OwnerIds already scopes the query, even for an adopted extra with
        // no TopParentId. C# AddUserToQuery must not replace it with libraries.
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0xE003)).await;
        let scoped = InternalItemsQuery {
            user: Some(user),
            ..query.clone()
        };
        let res = repository
            .get_item_list(&scoped)
            .await
            .expect("user extras");
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

    /// A query asking for exactly `ItemCounts` (the by-name controllers add
    /// it whenever `includeItemTypes` is set).
    fn with_item_counts(mut query: InternalItemsQuery) -> InternalItemsQuery {
        query.dto_options.fields = vec![ferrofin_model::querying::ItemFields::ItemCounts];
        query
    }

    /// A query with an explicit empty field set (`/Items/Filters2`'s shape).
    fn without_fields(mut query: InternalItemsQuery) -> InternalItemsQuery {
        query.dto_options.fields = Vec::new();
        query
    }

    /// v12 `GetItemValues` assigns `TotalRecordCount = representativeIds.Count`
    /// whenever `EnableTotalRecordCount` (`ByName.cs:227-230`), with or
    /// without a `Limit` — `limit=0` is jellyfin-web's "how many?" probe, and
    /// 10.11.8 answered it with 0.
    #[tokio::test]
    async fn by_name_total_is_the_collapsed_count_regardless_of_limit() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0xA0F5);
        seed_named_item(&db, movie, BaseItemKind::Movie, "One").await;
        for g in ["Action", "Adventure", "Comedy", "Drama", "Horror"] {
            seed_item_genre(&db, movie, g).await;
        }

        let limit_zero = InternalItemsQuery {
            limit: Some(0),
            ..Default::default()
        };
        let r = repository.get_genres(&limit_zero).await.expect("limit 0");
        assert!(r.items.is_empty());
        assert_eq!(r.total_record_count, 5);

        let no_limit = InternalItemsQuery::default();
        let r = repository.get_genres(&no_limit).await.expect("no limit");
        assert_eq!(r.items.len(), 5);
        assert_eq!(r.total_record_count, 5);

        let disabled = InternalItemsQuery {
            enable_total_record_count: false,
            ..Default::default()
        };
        let r = repository.get_genres(&disabled).await.expect("disabled");
        assert_eq!(r.items.len(), 5);
        assert_eq!(
            r.total_record_count, 0,
            "never assigned upstream → 0 on the wire"
        );
    }

    /// Port of v12 `BuildItemCountsByCleanName` (`ByName.cs:287-464`): the
    /// counts describe everything the value is attached to (not only the
    /// `IncludeItemTypes` the list was narrowed to), `ItemCount` is their
    /// sum, and — for studios and genres only — every episode under a tagged
    /// series counts, while an episode tagged under an untagged series counts
    /// once for itself.
    #[tokio::test]
    async fn by_name_counts_describe_everything_the_value_is_attached_to() {
        let db = test_db().await;
        let repository = repo(&db);

        let movie = Uuid::from_u128(0xA301);
        seed_named_item(&db, movie, BaseItemKind::Movie, "A Drama Film").await;
        seed_item_genre(&db, movie, "Drama").await;
        // A tagged series with three untagged episodes …
        let tagged_series = Uuid::from_u128(0xA302);
        seed_named_item(&db, tagged_series, BaseItemKind::Series, "Tagged Show").await;
        seed_item_genre(&db, tagged_series, "Drama").await;
        for n in 0..3u128 {
            let ep = Uuid::from_u128(0xA310 + n);
            seed_item_of_series(&db, ep, BaseItemKind::Episode, "Ep", tagged_series).await;
        }
        // … one of which is ALSO tagged itself (counted with its series, not twice) …
        seed_item_genre(&db, Uuid::from_u128(0xA310), "Drama").await;
        // … and an untagged series with one tagged episode (counts for itself).
        let plain_series = Uuid::from_u128(0xA303);
        seed_named_item(&db, plain_series, BaseItemKind::Series, "Plain Show").await;
        let stray = Uuid::from_u128(0xA320);
        seed_item_of_series(&db, stray, BaseItemKind::Episode, "Stray", plain_series).await;
        seed_item_genre(&db, stray, "Drama").await;
        // A song carrying the same genre: `SongCount`, and part of the total.
        let song = Uuid::from_u128(0xA330);
        seed_named_item(&db, song, BaseItemKind::Audio, "A Song").await;
        seed_item_genre(&db, song, "Drama").await;

        // Narrowed to movies: the list is the genres movies carry, but the
        // counts are the whole picture (`ByName.cs:299-311`).
        let movies_only = with_item_counts(InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            ..Default::default()
        });
        let got = repository.get_genres(&movies_only).await.expect("counts");
        assert_eq!(got.items.len(), 1);
        let counts = got.items[0].counts;
        assert_eq!(counts.movie_count, 1);
        assert_eq!(counts.series_count, 1);
        assert_eq!(counts.song_count, 1);
        assert_eq!(
            counts.episode_count, 4,
            "3 under the tagged series + the stray"
        );
        assert_eq!(counts.item_count, 7);

        // An artist credit does not pass down to episodes (`:341-342`): the
        // same shape under `AlbumArtist` (the value type that materializes a
        // browsable `MusicArtist` row) counts only the directly tagged episodes.
        seed_item_value(&db, tagged_series, ItemValueType::AlbumArtist, "Someone").await;
        seed_item_value(
            &db,
            Uuid::from_u128(0xA310),
            ItemValueType::AlbumArtist,
            "Someone",
        )
        .await;
        seed_item_value(&db, stray, ItemValueType::AlbumArtist, "Someone").await;
        let artists = repository
            .get_album_artists(&with_item_counts(InternalItemsQuery::default()))
            .await
            .expect("album artists");
        assert_eq!(artists.items.len(), 1);
        assert_eq!(artists.items[0].counts.series_count, 1);
        assert_eq!(artists.items[0].counts.episode_count, 2);
        assert_eq!(artists.items[0].counts.item_count, 3);
    }

    /// The two inner filters differ (`ByName.cs:149-164` vs `:301-311`): the
    /// page's semijoin carries the Live TV flags (`IsMovie`, …) and the count
    /// scope the played state and excluded ids.
    #[tokio::test]
    async fn by_name_inner_scopes_carry_the_flags_and_the_played_state() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0xA601);
        let program = Uuid::from_u128(0xA602);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Film").await;
        seed_named_item(&db, program, BaseItemKind::LiveTvProgram, "Airing").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "IsMovie" = 1 WHERE "Id" = ?1"#)
            .bind(guid_to_db(program))
            .execute(db.writer())
            .await
            .expect("flag the program");
        seed_item_genre(&db, movie, "Drama").await;
        seed_item_genre(&db, program, "News").await;
        seed_library_over(&db, &[movie, program]).await;
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0xA610)).await;
        seed_user_data(&db, Uuid::from_u128(0xA610), movie, true, None).await;

        let names = |r: QueryResult<ItemWithCounts>| -> Vec<String> {
            r.items.into_iter().filter_map(|i| i.item.name).collect()
        };
        // `IsMovie` with a Program-only kind filter reaches the content row;
        // with no kind filter v12 skips the predicate (`include_all_movie_types`).
        let programs = InternalItemsQuery {
            user: Some(user.clone()),
            include_item_types: vec![BaseItemKind::LiveTvProgram],
            is_movie: Some(true),
            ..Default::default()
        };
        assert_eq!(
            names(repository.get_genres(&programs).await.expect("programs")),
            vec!["News"]
        );
        let unfiltered = InternalItemsQuery {
            user: Some(user.clone()),
            is_movie: Some(true),
            ..Default::default()
        };
        assert_eq!(
            names(
                repository
                    .get_genres(&unfiltered)
                    .await
                    .expect("unfiltered")
            ),
            vec!["Drama", "News"]
        );

        // `IsPlayed` scopes the COUNTS (`scopeQuery`, `:310`), not the list.
        let played = with_item_counts(InternalItemsQuery {
            user: Some(user.clone()),
            is_played: Some(false),
            ..Default::default()
        });
        let got = repository.get_genres(&played).await.expect("played");
        let drama = got
            .items
            .iter()
            .find(|i| i.item.name.as_deref() == Some("Drama"))
            .expect("the genre row is listed regardless");
        assert_eq!(drama.counts.movie_count, 0, "the one carrier is played");
        let news = got
            .items
            .iter()
            .find(|i| i.item.name.as_deref() == Some("News"))
            .expect("listed");
        assert_eq!(news.counts.program_count, 1);
        assert_eq!(news.counts.item_count, 1);
    }

    /// C#'s `outerQueryFilter` (`ByName.cs:177-196`) applies to the by-name
    /// row itself — its own year, rating, tags and played state — never to
    /// the content that carries the value.
    #[tokio::test]
    async fn by_name_outer_filters_apply_to_the_by_name_row() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0xA501);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Film").await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "ProductionYear" = 1999, "OfficialRating" = 'R' WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(movie))
        .execute(db.writer())
        .await
        .expect("movie metadata");
        for g in ["Action", "Drama"] {
            seed_item_genre(&db, movie, g).await;
        }
        // Only the "Drama" genre ROW carries the year, the rating and a tag.
        let drama: String = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems" WHERE "Name" = 'Drama' AND "Type" = 'MediaBrowser.Controller.Entities.Genre'"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("genre row");
        sqlx::query(
            r#"UPDATE "BaseItems" SET "ProductionYear" = 2001, "OfficialRating" = 'PG' WHERE "Id" = ?1"#,
        )
        .bind(&drama)
        .execute(db.writer())
        .await
        .expect("genre metadata");
        let drama_id = Uuid::parse_str(&drama).expect("guid");
        seed_item_value(&db, drama_id, ItemValueType::Tags, "featured").await;

        let names = |r: QueryResult<ItemWithCounts>| -> Vec<String> {
            r.items.into_iter().filter_map(|i| i.item.name).collect()
        };
        let by_year = InternalItemsQuery {
            years: vec![2001],
            ..Default::default()
        };
        assert_eq!(
            names(repository.get_genres(&by_year).await.expect("years")),
            vec!["Drama"]
        );
        let movie_year = InternalItemsQuery {
            years: vec![1999],
            ..Default::default()
        };
        assert!(
            names(
                repository
                    .get_genres(&movie_year)
                    .await
                    .expect("movie year")
            )
            .is_empty(),
            "the movie's year is not the genre's"
        );
        let by_rating = InternalItemsQuery {
            official_ratings: vec!["PG".to_owned()],
            ..Default::default()
        };
        assert_eq!(
            names(repository.get_genres(&by_rating).await.expect("rating")),
            vec!["Drama"]
        );
        let by_tag = InternalItemsQuery {
            tags: vec!["featured".to_owned()],
            ..Default::default()
        };
        assert_eq!(
            names(repository.get_genres(&by_tag).await.expect("tag")),
            vec!["Drama"]
        );
    }

    /// The inner content query carries the caller's `ExcludeItemTypes` and
    /// `MediaTypes` (`ByName.cs:151-153`) and, like every `TranslateQuery`
    /// that does not ask for owned items, ignores alternate versions.
    #[tokio::test]
    async fn by_name_inner_query_honours_exclude_types_media_types_and_alternates() {
        let db = test_db().await;
        let repository = repo(&db);
        let movie = Uuid::from_u128(0xA401);
        seed_named_item(&db, movie, BaseItemKind::Movie, "Film").await;
        seed_item_genre(&db, movie, "Action").await;
        let series = Uuid::from_u128(0xA402);
        seed_named_item(&db, series, BaseItemKind::Series, "Show").await;
        seed_item_genre(&db, series, "Drama").await;
        // A movie is `Video`; a series row carries `Unknown`, as Jellyfin writes it.
        for (id, media) in [(movie, "Video"), (series, "Unknown")] {
            sqlx::query(r#"UPDATE "BaseItems" SET "MediaType" = ?1 WHERE "Id" = ?2"#)
                .bind(media)
                .bind(guid_to_db(id))
                .execute(db.writer())
                .await
                .expect("set media type");
        }
        // An alternate version is the only carrier of "Cut": it contributes
        // nothing (`TranslateQuery.cs:796-807`).
        let alternate = Uuid::from_u128(0xA403);
        seed_named_item(&db, alternate, BaseItemKind::Movie, "Film 4K").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?1 WHERE "Id" = ?2"#)
            .bind(guid_to_db(movie))
            .bind(guid_to_db(alternate))
            .execute(db.writer())
            .await
            .expect("link alternate");
        seed_item_genre(&db, alternate, "Cut").await;

        let names = |r: QueryResult<ItemWithCounts>| -> Vec<String> {
            r.items.into_iter().filter_map(|i| i.item.name).collect()
        };
        let all = repository
            .get_genres(&InternalItemsQuery::default())
            .await
            .expect("all");
        assert_eq!(names(all), vec!["Action", "Drama"]);

        let no_movies = InternalItemsQuery {
            exclude_item_types: vec![BaseItemKind::Movie],
            ..Default::default()
        };
        assert_eq!(
            names(repository.get_genres(&no_movies).await.expect("exclude")),
            vec!["Drama"]
        );

        let video_only = InternalItemsQuery {
            media_types: vec![ferrofin_model::data::MediaType::Video],
            ..Default::default()
        };
        assert_eq!(
            names(repository.get_genres(&video_only).await.expect("media")),
            vec!["Action"]
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

    /// `PersonsController.BuildAccessFilter`: no filter for a user nothing is
    /// hidden from (`HasContentRestrictions`); otherwise the user's resolved
    /// restrictions, scoped to their libraries, with owned items admitted.
    #[tokio::test]
    async fn the_access_filter_exists_only_for_a_user_with_content_restrictions() {
        let db = test_db().await;
        let repository = repo(&db);
        let (first, second, user_id, user) = two_libraries(&db).await;
        assert_eq!(repository.build_access_filter(&user).await.unwrap(), None);

        let user_key = guid_to_db(user_id);
        let tags = |values: &[&str]| values.iter().map(|&v| v.to_owned()).collect::<Vec<_>>();
        crate::user_entity_ext::set_preference(
            db.pool(),
            &user_key,
            PreferenceKind::BlockedTags,
            &tags(&["gore"]),
        )
        .await
        .expect("block tag");
        let filter = repository
            .build_access_filter(&user)
            .await
            .unwrap()
            .expect("a blocked tag restricts");
        assert!(filter.user_has_content_restrictions && filter.include_owned_items);
        assert_eq!(filter.exclude_inherited_tags, ["gore"]);
        let mut scope = filter.top_parent_ids.clone();
        scope.sort();
        assert_eq!(scope, [first, second]);

        // Library access alone is a restriction too.
        crate::user_entity_ext::set_preference(
            db.pool(),
            &user_key,
            PreferenceKind::BlockedTags,
            &tags(&[]),
        )
        .await
        .expect("clear tag");
        sqlx::query(r#"UPDATE "Permissions" SET "Value" = 0 WHERE "UserId" = ?1 AND "Kind" = ?2"#)
            .bind(&user_key)
            .bind(i32::from(PermissionKind::EnableAllFolders))
            .execute(db.writer())
            .await
            .expect("revoke");
        set_folder_preference(&db, user_id, PreferenceKind::EnabledFolders, &[first]).await;
        let filter = repository
            .build_access_filter(&user)
            .await
            .unwrap()
            .expect("library access restricts");
        assert!(filter.exclude_inherited_tags.is_empty());
        assert_eq!(filter.top_parent_ids, [first]);
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

        // `AdjacentTo` trims the view's resolved result like any other.
        for (id, name) in [
            (Uuid::from_u128(0x9E04), "Alpha Mix"),
            (Uuid::from_u128(0x9E05), "Bravo Beats"),
            (Uuid::from_u128(0x9E06), "Zed Tunes"),
        ] {
            seed_top_parented_item(&db, id, BaseItemKind::Playlist, name, folder).await;
        }
        sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = lower("Name")"#)
            .execute(db.writer())
            .await
            .unwrap();
        let around = repository
            .get_item_list(&InternalItemsQuery {
                parent_id: view,
                recursive: true,
                include_item_types: vec![BaseItemKind::Playlist],
                order_by: vec![(
                    ferrofin_model::live_tv::ItemSortBy::SortName,
                    ferrofin_model::dto::SortOrder::Ascending,
                )],
                adjacent_to: Some(playlist),
                ..InternalItemsQuery::default()
            })
            .await
            .expect("adjacent browse");
        assert_eq!(
            around
                .iter()
                .filter_map(|r| r.name.as_deref())
                .collect::<Vec<_>>(),
            ["Bravo Beats", "Road Trip", "Zed Tunes"]
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

    /// An untyped view groups too. Upstream's `GetTopParentIdsForQuery` groups
    /// when `ViewType != unknown && IsEligibleForGrouping(ViewType)`, whose set
    /// is `{ movies, tvshows, null }` (`UserView.cs:22-27`), and its
    /// `CollectionType is null || == ViewType` filter then keeps only the
    /// grouped libraries that have no type. An `unknown` view still groups
    /// nothing.
    #[tokio::test]
    async fn an_untyped_view_stands_for_the_untyped_libraries_grouped_into_it() {
        let db = test_db().await;
        let repository = repo(&db);
        let (untyped, movies, user_id, user) = two_libraries(&db).await;
        set_collection_type(&db, movies, "movies").await;
        set_folder_preference(
            &db,
            user_id,
            PreferenceKind::GroupedFolders,
            &[untyped, movies],
        )
        .await;
        let untyped_view = Uuid::from_u128(0x9F82);
        seed_item_with_data(&db, untyped_view, BaseItemKind::UserView, "Media", "{}").await;
        // `unknown` by name, and by its discriminant (0), as an adopted blob
        // written without the enum converter stores it.
        let unknown_views = [Uuid::from_u128(0x9F83), Uuid::from_u128(0x9F84)];
        for (view, data) in unknown_views
            .iter()
            .zip([r#"{"ViewType":"unknown"}"#, r#"{"ViewType":0}"#])
        {
            seed_item_with_data(&db, *view, BaseItemKind::UserView, "Unknown", data).await;
        }
        let browse = |parent: Uuid| {
            let repository = &repository;
            let user = user.clone();
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

        assert_eq!(
            browse(untyped_view).await,
            ["In First"],
            "an untyped view stands for the grouped untyped library only"
        );
        for view in unknown_views {
            assert!(
                browse(view).await.is_empty(),
                "an unknown view groups nothing"
            );
        }
    }

    /// A view the user has GROUPED libraries into stands for those libraries —
    /// C# `GetTopParentIdsForQuery`'s grouping arm.
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

    /// An adopted library whose rows a Ferrofin scan has saved only some of
    /// (a first scan running or cancelled, or a row it does not plan): the
    /// adopted rows still hang off the physical folder (Jellyfin's
    /// `ParentId`/`TopParentId`), the saved ones off the collection folder.
    /// Every by-library read sees both — the recursive browse, the
    /// `TopParentIds` scope `Latest` and the similar-items query use, the
    /// direct children, and a user's unscoped query — or the saved rows
    /// vanish from the library (measured on the bench corpus before the fix:
    /// every library browsed empty after the first scan).
    #[tokio::test]
    async fn an_adopted_library_holds_the_rows_a_scan_saved_and_the_ones_it_did_not() {
        use crate::test_support::{fetch_item, save_item};
        let db = test_db().await;
        let repository = repo(&db);
        let view = Uuid::from_u128(0x9C01);
        let physical = Uuid::from_u128(0x9C02);
        let adopted = Uuid::from_u128(0x9C03);
        let saved = Uuid::from_u128(0x9C04);
        let other_view = Uuid::from_u128(0x9C05);
        let elsewhere = Uuid::from_u128(0x9C06);

        seed_item_with_data(
            &db,
            view,
            BaseItemKind::CollectionFolder,
            "Movies",
            &collection_folder_data(physical),
        )
        .await;
        seed_named_item(&db, physical, BaseItemKind::Folder, "movies").await;
        seed_named_item(&db, other_view, BaseItemKind::CollectionFolder, "Other").await;
        for (id, name, top) in [
            (adopted, "Adopted", physical),
            (saved, "Saved", view),
            (elsewhere, "Elsewhere", other_view),
        ] {
            seed_top_parented_item(&db, id, BaseItemKind::Movie, name, top).await;
            let mut row = fetch_item(&db, id).await;
            row.parent_id = Some(guid_to_db(top));
            save_item(&db, &row).await;
        }
        let user = crate::test_support::seed_user_with_defaults(&db, Uuid::from_u128(0x9C07)).await;
        let names = |filter: InternalItemsQuery| {
            let repository = &repository;
            async move {
                let mut names: Vec<String> = repository
                    .get_item_list(&InternalItemsQuery {
                        include_item_types: vec![BaseItemKind::Movie],
                        ..filter
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

        assert_eq!(
            names(InternalItemsQuery {
                parent_id: view,
                recursive: true,
                ..InternalItemsQuery::default()
            })
            .await,
            ["Adopted", "Saved"],
            "the recursive browse"
        );
        assert_eq!(
            names(InternalItemsQuery {
                top_parent_ids: vec![view],
                ..InternalItemsQuery::default()
            })
            .await,
            ["Adopted", "Saved"],
            "the TopParentIds scope"
        );
        assert_eq!(
            names(InternalItemsQuery {
                parent_id: view,
                ..InternalItemsQuery::default()
            })
            .await,
            ["Adopted", "Saved"],
            "the direct children"
        );
        assert_eq!(
            unscoped_names(&repository, &user).await,
            ["Adopted", "Elsewhere", "Saved"],
            "a user's unscoped query, scoped to their libraries"
        );
        // Naming the physical folder beside the library (the pruning's
        // scope) reads each row once.
        assert_eq!(
            names(InternalItemsQuery {
                top_parent_ids: vec![view, physical],
                ..InternalItemsQuery::default()
            })
            .await,
            ["Adopted", "Saved"],
            "the library and its physical folder, named together"
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

    /// Which row stands for a presentation-key group when the primary-version
    /// preference does not decide: C# `ApplyGroupingFilter` takes
    /// `g.Where(e => e.PrimaryVersionId == null).Min(e => (Guid?)e.Id) ??
    /// g.Min(e => (Guid?)e.Id)`, so the smallest id among the primaries, and
    /// the smallest id overall in a group with none.
    ///
    /// Each group's larger id is written first, so a pick that falls back on
    /// SQLite's scan order (the first row reaching the `MIN`) lands on it.
    #[tokio::test]
    async fn a_presentation_group_is_represented_by_its_smallest_primary_id() {
        let db = test_db().await;
        let repository = repo(&db);
        let library = Uuid::from_u128(0x9C11);
        let other_library = Uuid::from_u128(0x9C12);
        // Two primaries share a key: two seasons of one series with the same
        // number.
        let (tie_big, tie_small) = (Uuid::from_u128(0x9C1B), Uuid::from_u128(0x9C1A));
        // The keyless group: every row without a key is one group.
        let (keyless_big, keyless_small) = (Uuid::from_u128(0x9C4B), Uuid::from_u128(0x9C4A));
        // Alternates whose primary is gone: none of the group is a primary.
        let (orphan_big, orphan_small) = (Uuid::from_u128(0x9C2B), Uuid::from_u128(0x9C2A));
        let gone = Uuid::from_u128(0x9C2F);
        // The primary wins over a smaller alternate. The alternate is in another
        // library, where it is listed as an item of its own (an alternate beside
        // its primary is hidden before grouping), so the two meet in one group.
        let (primary, alternate) = (Uuid::from_u128(0x9C3B), Uuid::from_u128(0x9C3A));
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, other_library, BaseItemKind::CollectionFolder, "More").await;
        for (id, key, primary_version, top_parent) in [
            (tie_big, Some("tie"), None, library),
            (tie_small, Some("tie"), None, library),
            (keyless_big, None, None, library),
            (keyless_small, None, None, library),
            (orphan_big, Some("orphans"), Some(gone), library),
            (orphan_small, Some("orphans"), Some(gone), library),
            (primary, Some("mixed"), None, library),
            (alternate, Some("mixed"), Some(primary), other_library),
        ] {
            let name = key.unwrap_or("keyless");
            seed_top_parented_item(&db, id, BaseItemKind::Movie, name, top_parent).await;
            sqlx::query(
                r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = ?2, "PrimaryVersionId" = ?3 WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .bind(key)
            .bind(primary_version.map(guid_to_db))
            .execute(db.writer())
            .await
            .expect("set the key and the primary version");
        }
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0x9C05)).await;

        let mut found: Vec<String> = repository
            .get_item_list(&InternalItemsQuery {
                user: Some(user),
                recursive: true,
                include_item_types: vec![BaseItemKind::Movie],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("browse")
            .iter()
            .map(|row| row.id.clone())
            .collect();
        found.sort();
        assert_eq!(
            found,
            [tie_small, orphan_small, primary, keyless_small].map(guid_to_db)
        );
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
        // invisible, which is exactly how it survived. The alternate was
        // played more recently.
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0x9C45)).await;
        for id in [primary, alternate] {
            set_playback_position(&db, id, &user.id, 42_000_000).await;
        }
        let date_progress = |id: Uuid, date: &'static str| {
            let db = db.clone();
            async move {
                sqlx::query(r#"UPDATE "UserData" SET "LastPlayedDate" = ?2 WHERE "ItemId" = ?1"#)
                    .bind(guid_to_db(id))
                    .bind(date)
                    .execute(db.writer())
                    .await
                    .expect("date the progress");
            }
        };
        let other = if alternate_id == primary {
            alternate
        } else {
            primary
        };
        date_progress(other, "2026-01-01 00:00:00.0000000").await;
        date_progress(alternate_id, "2026-01-02 00:00:00.0000000").await;

        let query = InternalItemsQuery {
            user: Some(user.clone()),
            recursive: true,
            is_resumable: Some(true),
            ..InternalItemsQuery::default()
        };
        let resumed = repository.get_item_list(&query).await.expect("resume");
        let ids: Vec<String> = resumed.iter().map(|r| r.id.clone()).collect();
        // v12 `TranslateQuery.cs:586-602`: "When several versions of the same
        // item are in progress, keep only the most recently played one".
        assert_eq!(
            ids,
            vec![guid_to_db(alternate_id)],
            "the played alternate must be surfaced, not collapsed onto the primary"
        );

        // A tie on the date (both NULL here) falls to the lower id.
        sqlx::query(r#"UPDATE "UserData" SET "LastPlayedDate" = NULL"#)
            .execute(db.writer())
            .await
            .expect("clear dates");
        let resumed = repository.get_item_list(&query).await.expect("resume");
        let ids: Vec<String> = resumed.iter().map(|r| r.id.clone()).collect();
        let lower = [primary, alternate]
            .into_iter()
            .map(guid_to_db)
            .min()
            .expect("two ids");
        assert_eq!(ids, vec![lower]);

        // `IsResumable = false` works on primaries: neither version is listed,
        // the primary because a version of it is in progress.
        let not_resumable = InternalItemsQuery {
            is_resumable: Some(false),
            ..query
        };
        let rest = repository
            .get_item_list(&not_resumable)
            .await
            .expect("not resumable");
        assert!(
            rest.iter()
                .all(|r| r.id != guid_to_db(primary) && r.id != guid_to_db(alternate)),
            "an in-progress item is not in the not-resumable set (got {rest:?})"
        );
    }

    /// v12 `OrderMapper.cs:32-48`: an item's played date is the newest of its
    /// own progress and its alternate versions' — the second arm of the
    /// `DatePlayed` ordering — while `PlayCount` / `IsFavoriteOrLiked` read
    /// the item's own first `UserData` row only (`:57-61`). `IsPlayed` /
    /// `IsUnplayed` are the played filter's predicate (`played_state_tests`).
    #[tokio::test]
    async fn date_played_spans_alternates_but_play_count_does_not() {
        use ferrofin_model::dto::SortOrder;
        use ferrofin_model::live_tv::ItemSortBy;

        let db = test_db().await;
        let repository = repo(&db);
        let library = Uuid::from_u128(0x9F01);
        let a = Uuid::from_u128(0x9F02);
        let b = Uuid::from_u128(0x9F03);
        let b_alt = Uuid::from_u128(0x9F04);
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "Movies").await;
        seed_top_parented_item(&db, a, BaseItemKind::Movie, "Alpha", library).await;
        seed_top_parented_item(&db, b, BaseItemKind::Movie, "Beta", library).await;
        seed_top_parented_item(&db, b_alt, BaseItemKind::Movie, "Beta 4K", library).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?1 WHERE "Id" = ?2"#)
            .bind(guid_to_db(b))
            .bind(guid_to_db(b_alt))
            .execute(db.writer())
            .await
            .expect("link alternate");
        set_presentation_key(&db, a, "key-a").await;
        set_presentation_key(&db, b, "key-b").await;
        set_presentation_key(&db, b_alt, "key-b").await;
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0x9F05)).await;
        // A: played once, in March. B: played once, in January — but its
        // alternate was played nine times, in June.
        for (item, count, date) in [
            (a, 1_i64, "2026-03-01 00:00:00.0000000"),
            (b, 1, "2026-01-01 00:00:00.0000000"),
            (b_alt, 9, "2026-06-01 00:00:00.0000000"),
        ] {
            sqlx::query(
                r#"INSERT INTO "UserData"
                   ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "LastPlayedDate",
                    "PlayCount", "PlaybackPositionTicks", "Played")
                   VALUES (?1, ?2, ?1, 0, ?3, ?4, 0, 1)"#,
            )
            .bind(guid_to_db(item))
            .bind(&user.id)
            .bind(date)
            .bind(count)
            .execute(db.writer())
            .await
            .expect("user data");
        }

        let names = |sort: ItemSortBy, user: UserEntity| {
            let repository = &repository;
            async move {
                repository
                    .get_item_list(&InternalItemsQuery {
                        user: Some(user),
                        recursive: true,
                        include_item_types: vec![BaseItemKind::Movie],
                        order_by: vec![(sort, SortOrder::Descending)],
                        ..InternalItemsQuery::default()
                    })
                    .await
                    .expect("sorted")
                    .into_iter()
                    .filter_map(|r| r.name)
                    .collect::<Vec<_>>()
            }
        };
        // The grouped browse lists the primaries; B's date is its alternate's.
        assert_eq!(
            names(ItemSortBy::DatePlayed, user.clone()).await,
            vec!["Beta", "Alpha"]
        );
        // PlayCount reads B's own row (1), not the alternate's (9).
        assert_eq!(
            names(ItemSortBy::PlayCount, user.clone()).await,
            vec!["Alpha", "Beta"]
        );
        // The other user-data keys build and run.
        for sort in [
            ItemSortBy::IsPlayed,
            ItemSortBy::IsUnplayed,
            ItemSortBy::IsFavoriteOrLiked,
        ] {
            assert_eq!(names(sort, user.clone()).await.len(), 2, "{sort:?}");
        }
    }

    /// v12's `IsResumable` folder rule (`TranslateQuery.cs:562-575`,
    /// `_resumableFolderKinds = [Series, Season]`): a series or season is
    /// resumable when a leaf descendant is in progress, or when it has both a
    /// played and an unplayed leaf descendant. No percentage threshold; a
    /// box set is a container and never resumable.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one fixture tree, every arm of the rule asserted on it
    async fn series_and_seasons_are_resumable_by_their_leaves() {
        let db = test_db().await;
        let repository = repo(&db);
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0x9E00)).await;
        let uid = guid_to_db(Uuid::from_u128(0x9E00));

        // Show A: S1 has one played and one unplayed episode → partially
        // watched; S2 is untouched. Show B: one episode in progress. Show C:
        // fully played. A box set links a played and an unplayed movie.
        let show_a = Uuid::from_u128(0x9E01);
        let a_s1 = Uuid::from_u128(0x9E02);
        let a_s2 = Uuid::from_u128(0x9E03);
        let a_s1e1 = Uuid::from_u128(0x9E04);
        let a_s1e2 = Uuid::from_u128(0x9E05);
        let a_s2e1 = Uuid::from_u128(0x9E06);
        let show_b = Uuid::from_u128(0x9E11);
        let b_s1 = Uuid::from_u128(0x9E12);
        let b_s1e1 = Uuid::from_u128(0x9E13);
        let show_c = Uuid::from_u128(0x9E21);
        let c_s1 = Uuid::from_u128(0x9E22);
        let c_s1e1 = Uuid::from_u128(0x9E23);
        let box_set = Uuid::from_u128(0x9E31);
        let played_movie = Uuid::from_u128(0x9E32);
        let unplayed_movie = Uuid::from_u128(0x9E33);
        for (id, name) in [(show_a, "A"), (show_b, "B"), (show_c, "C")] {
            seed_folder_item(&db, id, BaseItemKind::Series, name, None).await;
        }
        for (id, series) in [
            (a_s1, show_a),
            (a_s2, show_a),
            (b_s1, show_b),
            (c_s1, show_c),
        ] {
            seed_item_of_series(&db, id, BaseItemKind::Season, "Season", series).await;
        }
        for (id, series) in [
            (a_s1e1, show_a),
            (a_s1e2, show_a),
            (a_s2e1, show_a),
            (b_s1e1, show_b),
            (c_s1e1, show_c),
        ] {
            seed_item_of_series(&db, id, BaseItemKind::Episode, "Episode", series).await;
        }
        seed_folder_item(&db, box_set, BaseItemKind::BoxSet, "Set", None).await;
        seed_named_item(&db, played_movie, BaseItemKind::Movie, "Played").await;
        seed_named_item(&db, unplayed_movie, BaseItemKind::Movie, "Unplayed").await;
        // An unscoped query as a user is confined to that user's libraries.
        seed_library_over(
            &db,
            &[
                show_a,
                a_s1,
                a_s2,
                a_s1e1,
                a_s1e2,
                a_s2e1,
                show_b,
                b_s1,
                b_s1e1,
                show_c,
                c_s1,
                c_s1e1,
                box_set,
                played_movie,
                unplayed_movie,
            ],
        )
        .await;
        // The ancestor closure — what `BuildHasDescendantFilter` walks.
        for (item, parents) in [
            (a_s1e1, vec![a_s1, show_a]),
            (a_s1e2, vec![a_s1, show_a]),
            (a_s2e1, vec![a_s2, show_a]),
            (b_s1e1, vec![b_s1, show_b]),
            (c_s1e1, vec![c_s1, show_c]),
            (played_movie, vec![box_set]),
            (unplayed_movie, vec![box_set]),
        ] {
            for parent in parents {
                sqlx::query(
                    r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#,
                )
                .bind(guid_to_db(item))
                .bind(guid_to_db(parent))
                .execute(db.writer())
                .await
                .expect("ancestor");
            }
        }
        for (item, played, position) in [
            (a_s1e1, true, 0),
            (b_s1e1, false, 5_000),
            (c_s1e1, true, 0),
            (played_movie, true, 0),
        ] {
            sqlx::query(
                r#"INSERT INTO "UserData"
                   ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
                    "PlaybackPositionTicks", "Played")
                   VALUES (?1, ?2, ?1, 0, 1, ?3, ?4)"#,
            )
            .bind(guid_to_db(item))
            .bind(&uid)
            .bind(position)
            .bind(played)
            .execute(db.writer())
            .await
            .expect("user data");
        }

        let query = InternalItemsQuery {
            user: Some(user),
            recursive: true,
            is_resumable: Some(true),
            ..InternalItemsQuery::default()
        };
        let mut resumable: Vec<String> = repository
            .get_item_list(&query)
            .await
            .expect("resumable")
            .into_iter()
            .map(|r| r.id)
            .collect();
        resumable.sort();
        let mut expected: Vec<String> = [show_a, a_s1, show_b, b_s1, b_s1e1]
            .into_iter()
            .map(guid_to_db)
            .collect();
        expected.sort();
        assert_eq!(
            resumable, expected,
            "A + A/S1 (partially watched), B + B/S1 + the episode (in progress); \
             not A/S2 (untouched), C (fully played) or the box set"
        );

        // The complement, on the same rows.
        let not_resumable = InternalItemsQuery {
            is_resumable: Some(false),
            ..query
        };
        let rest: Vec<String> = repository
            .get_item_list(&not_resumable)
            .await
            .expect("not resumable")
            .into_iter()
            .map(|r| r.id)
            .collect();
        for id in [show_a, a_s1, show_b, b_s1, b_s1e1] {
            assert!(!rest.contains(&guid_to_db(id)), "{id} is resumable");
        }
        for id in [
            a_s2,
            show_c,
            c_s1,
            box_set,
            a_s1e1,
            c_s1e1,
            played_movie,
            unplayed_movie,
        ] {
            assert!(rest.contains(&guid_to_db(id)), "{id} is not resumable");
        }
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

    /// The DTO's by-name links pick the same survivor as the list above (the
    /// lowest id), fall back to a `Genre`/`Studio` name's `ItemValues` id when
    /// it has no row, never to `Genre`'s value id for a `MusicGenre`, and stay
    /// on the `(Type, CleanName)` / `(Type, CleanValue)` indexes.
    #[tokio::test]
    async fn by_name_link_ids_take_the_lowest_row_then_the_value_id() {
        use ferrofin_db::enums::ItemValueType;
        let db = test_db().await;
        let movie = Uuid::from_u128(0x9C01);
        let (lo, hi) = (Uuid::from_u128(0x9C02), Uuid::from_u128(0x9C03));
        seed_named_item(&db, movie, BaseItemKind::Movie, "Film").await;
        seed_item_genre(&db, movie, "Drama").await;
        crate::test_support::seed_item_value(&db, movie, ItemValueType::Studios, "Acme").await;
        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" <> ?1"#)
            .bind(guid_to_db(movie))
            .execute(db.writer())
            .await
            .expect("drop the scanner rows");
        for id in [hi, lo] {
            seed_named_item(&db, id, BaseItemKind::Genre, "Drama").await;
            set_clean_name(&db, id, "Drama").await;
        }
        let acme_value: String =
            sqlx::query_scalar(r#"SELECT "ItemValueId" FROM "ItemValues" WHERE "Value" = 'Acme'"#)
                .fetch_one(db.pool())
                .await
                .expect("value id");

        let keys = [
            (BaseItemKind::Genre, "drama".to_owned()),
            (BaseItemKind::MusicGenre, "drama".to_owned()),
            (BaseItemKind::Studio, "acme".to_owned()),
            (BaseItemKind::Studio, "nobody".to_owned()),
        ];
        let ids = by_name_ids_by_clean_name(&db, &keys).await.expect("ids");
        assert_eq!(ids[&BaseItemKind::Genre]["drama"], lo, "Min(Id) survivor");
        assert!(!ids.contains_key(&BaseItemKind::MusicGenre), "{ids:?}");
        assert_eq!(
            ids[&BaseItemKind::Studio]["acme"],
            Uuid::parse_str(&acme_value).unwrap(),
            "no row: the value id"
        );
        assert!(!ids[&BaseItemKind::Studio].contains_key("nobody"));

        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {}",
            by_name_ids_sql(1)
        )))
        .bind("t")
        .bind(2)
        .bind("c")
        .fetch_all(db.pool())
        .await
        .expect("plan");
        let has = |needle: &str| plan.iter().any(|(_, _, _, line)| line.contains(needle));
        assert!(
            has("SEARCH bi USING INDEX IX_BaseItems_Type_CleanName (Type=? AND CleanName=?)")
                && has(
                    "SEARCH iv USING INDEX IX_ItemValues_Type_CleanValue (Type=? AND CleanValue=?)"
                )
                && !has("SCAN bi")
                && !has("SCAN iv"),
            "{plan:?}"
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

    /// The per-kind counts are PER by-name row, v12's
    /// `BuildItemCountsByCleanName` (`BaseItemRepository.ByName.cs:287-413`),
    /// and they describe everything the value is attached to — the scope
    /// drops `IncludeItemTypes` (`:299-311`), so the music-genre tab narrowed
    /// to songs and albums still reports the movie that carries "Rock".
    ///
    /// v10.11.8 instead hands every row the same uncorrelated totals
    /// (`BaseItemRepository.cs:1368-1392`, whose own comment reads "TODO: This
    /// is bad refactor!"), which is why a 10.11.8 oracle reports the same
    /// `SongCount` for every genre.
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
        assert_eq!(
            by_name("Rock").movie_count,
            1,
            "the counts describe everything the value is attached to"
        );
        assert_eq!(by_name("Rock").item_count, 2);
        assert_eq!(by_name("Jazz").song_count, 1);
        assert_eq!(
            by_name("Jazz").album_count,
            1,
            "per-CleanName, not one global total shared by every row"
        );
    }

    /// Without `ItemCounts` among the requested fields the per-kind counts are
    /// not computed at all (`ByName.cs:251`, `273-282`): every count stays at
    /// its zero default, `ItemCount` included.
    #[tokio::test]
    async fn per_kind_counts_are_not_computed_without_the_item_counts_field() {
        let db = test_db().await;
        let repository = repo(&db);
        let song = Uuid::from_u128(0x9A01);
        seed_named_item(&db, song, BaseItemKind::Audio, "A Song").await;
        seed_item_genre(&db, song, "Rock").await;

        let music = repository
            .get_music_genres(&without_fields(InternalItemsQuery::default()))
            .await
            .expect("music genres");
        assert_eq!(music.items.len(), 1);
        assert_eq!(music.items[0].counts, ItemCounts::default());
    }

    /// v12 `BuildLeafIsPlayedFilter`: a version group is played when any of
    /// its versions is. The alternate is hidden from the general query, so it
    /// is the primary that must answer `IsPlayed` for the version the user
    /// actually watched.
    #[tokio::test]
    async fn is_played_counts_a_version_group_played_through_its_alternate() {
        use crate::test_support::{seed_library_over, seed_user_data, seed_user_with_defaults};
        let db = test_db().await;
        let repository = crate::test_support::item_repository_over(db.clone());
        let user = seed_user_with_defaults(&db, Uuid::from_u128(0xE4FF)).await;
        let user_id = Uuid::parse_str(&user.id).expect("user id");
        let (primary, alternate, other) = (
            Uuid::from_u128(0xE401),
            Uuid::from_u128(0xE402),
            Uuid::from_u128(0xE403),
        );
        for id in [primary, alternate, other] {
            seed_item(&db, id, BaseItemKind::Movie).await;
        }
        seed_library_over(&db, &[primary, alternate, other]).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?2 WHERE "Id" = ?1"#)
            .bind(ferrofin_db::store::guid_to_db(alternate))
            .bind(ferrofin_db::store::guid_to_db(primary))
            .execute(db.writer())
            .await
            .expect("link");
        // Only the ALTERNATE was watched.
        seed_user_data(&db, user_id, alternate, true, None).await;

        let ids = |items: Vec<ferrofin_db::entities::base_items::BaseItemEntity>| {
            let mut v: Vec<Uuid> = items
                .iter()
                .filter_map(|i| Uuid::parse_str(&i.id).ok())
                .collect();
            v.sort();
            v
        };
        let played = repository
            .get_item_list(&InternalItemsQuery {
                user: Some(user.clone()),
                include_item_types: vec![BaseItemKind::Movie],
                is_played: Some(true),
                ..Default::default()
            })
            .await
            .expect("played");
        assert_eq!(
            ids(played),
            vec![primary],
            "the primary is played through its version"
        );
        let unplayed = repository
            .get_item_list(&InternalItemsQuery {
                user: Some(user.clone()),
                include_item_types: vec![BaseItemKind::Movie],
                is_played: Some(false),
                ..Default::default()
            })
            .await
            .expect("unplayed");
        assert_eq!(ids(unplayed), vec![other]);
    }

    #[tokio::test]
    async fn disabled_library_options_filter_unscoped_queries_and_counts_live() {
        let db = test_db().await;
        let repository = repo(&db);
        let (_, second, _, user) = two_libraries(&db).await;
        let options = tempfile::tempdir().unwrap();
        crate::test_support::set_item_path(&db, second, options.path().to_str().unwrap()).await;
        let query = InternalItemsQuery {
            user: Some(user.clone()),
            recursive: true,
            include_item_types: vec![BaseItemKind::Movie],
            ..Default::default()
        };
        for (enabled, expected) in [(true, 2), (false, 1), (true, 2)] {
            std::fs::write(
                options.path().join("options.json"),
                format!(r#"{{"Enabled":{enabled}}}"#),
            )
            .unwrap();
            let page = repository.get_items(&query).await.unwrap();
            assert_eq!(page.items.len(), expected);
            assert_eq!(usize::try_from(page.total_record_count).unwrap(), expected);
            assert_eq!(
                usize::try_from(count_over(&db, &query).await).unwrap(),
                expected
            );
            assert_eq!(
                repository
                    .visible_library_ids(&user)
                    .await
                    .unwrap()
                    .contains(&second),
                enabled
            );
            // Upstream's explicit-scope exception is unchanged.
            let explicit = InternalItemsQuery {
                parent_id: second,
                ..query.clone()
            };
            assert_eq!(repository.get_item_list(&explicit).await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn disabled_adopted_library_is_hidden_from_user_root_without_writing_options() {
        use crate::aggregate_folder::RootFolderIds;
        use crate::test_support::seed_folder_item;
        let db = test_db().await;
        let (first, second, _, user) = two_libraries(&db).await;
        let roots = RootFolderIds {
            user_root: Uuid::from_u128(0xA301),
            aggregate: Uuid::from_u128(0xA302),
        };
        seed_folder_item(
            &db,
            roots.user_root,
            BaseItemKind::UserRootFolder,
            "Root",
            None,
        )
        .await;
        seed_folder_item(
            &db,
            roots.aggregate,
            BaseItemKind::AggregateFolder,
            "Physical",
            None,
        )
        .await;
        for id in [first, second] {
            sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ? WHERE "Id" = ?"#)
                .bind(guid_to_db(roots.user_root))
                .bind(guid_to_db(id))
                .execute(db.writer())
                .await
                .unwrap();
        }
        let options = tempfile::tempdir().unwrap();
        std::fs::write(
            options.path().join("options.xml"),
            "<LibraryOptions><Enabled>false</Enabled></LibraryOptions>",
        )
        .unwrap();
        crate::test_support::set_item_path(&db, second, options.path().to_str().unwrap()).await;
        let repository = repo(&db).with_root_ids(roots);
        let query = InternalItemsQuery {
            user_root_children: true,
            user: Some(user),
            ..Default::default()
        };
        let page = repository.get_items(&query).await.unwrap();
        assert_eq!(
            page.items
                .iter()
                .filter_map(|row| row.name.as_deref())
                .collect::<Vec<_>>(),
            ["First"]
        );
        assert_eq!(page.total_record_count, 1);
        assert!(!options.path().join("options.json").exists());
        let userless = repository
            .get_items(&InternalItemsQuery {
                user: None,
                ..query
            })
            .await
            .unwrap();
        assert_eq!(userless.items.len(), 2);
    }

    // One persisted root fixture verifies ordered filtering, state changes and paging together.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn folders_view_filters_sorts_pages_and_never_recurses_into_media() {
        use crate::aggregate_folder::RootFolderIds;
        use crate::test_support::seed_folder_item;
        use ferrofin_model::dto::SortOrder;
        use ferrofin_model::live_tv::ItemSortBy;
        let db = test_db().await;
        let (first, second, user_id, user) = two_libraries(&db).await;
        let roots = RootFolderIds {
            user_root: Uuid::new_v4(),
            aggregate: Uuid::new_v4(),
        };
        seed_folder_item(
            &db,
            roots.user_root,
            BaseItemKind::UserRootFolder,
            "Root",
            None,
        )
        .await;
        seed_folder_item(
            &db,
            roots.aggregate,
            BaseItemKind::AggregateFolder,
            "Physical",
            None,
        )
        .await;
        for id in [first, second] {
            sqlx::query(
                r#"UPDATE "BaseItems" SET "ParentId" = ?2, "IsFolder" = 1 WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .bind(guid_to_db(roots.user_root))
            .execute(db.writer())
            .await
            .unwrap();
        }
        let playlists = Uuid::new_v4();
        seed_folder_item(
            &db,
            playlists,
            BaseItemKind::PlaylistsFolder,
            "Playlists",
            Some(roots.aggregate),
        )
        .await;
        let view = Uuid::new_v4();
        seed_folder_item(&db, view, BaseItemKind::UserView, "Folders", None).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "Data" = '{"ViewType":"folders"}' WHERE "Id" = ?1"#)
            .bind(guid_to_db(view))
            .execute(db.writer())
            .await
            .unwrap();
        let repository = repo(&db).with_root_ids(roots);
        let query = InternalItemsQuery {
            parent_id: view,
            user: Some(user.clone()),
            recursive: true,
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            ..Default::default()
        };
        assert_eq!(
            repository
                .get_items(&query)
                .await
                .unwrap()
                .items
                .iter()
                .filter_map(|row| row.name.as_deref())
                .collect::<Vec<_>>(),
            ["First", "Playlists", "Second"]
        );
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    order_by: Vec::new(),
                    ..query.clone()
                })
                .await
                .unwrap()
                .items
                .iter()
                .filter_map(|row| row.name.as_deref())
                .collect::<Vec<_>>(),
            ["First", "Second", "Playlists"],
            "GetResult preserves GetChildren order when no sort is requested"
        );
        for recursive in [false, true] {
            let explicit = InternalItemsQuery {
                item_ids: vec![first, playlists],
                recursive,
                ..query.clone()
            };
            let result = repository.get_items(&explicit).await.unwrap();
            assert!(
                result.items.is_empty(),
                "explicit IDs bypass the folders builder and retain the empty persisted view scope"
            );
            assert_eq!(result.total_record_count, 0);
            assert!(repository.get_item_ids(&explicit).await.unwrap().is_empty());
            assert!(
                repository
                    .fetch_id_clean_names(&explicit)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        let nil_adjacent = repository
            .get_items(&InternalItemsQuery {
                adjacent_to: Some(Uuid::nil()),
                ..query.clone()
            })
            .await
            .unwrap();
        assert_eq!(
            nil_adjacent.total_record_count, 3,
            "source treats an empty adjacent GUID as absent"
        );
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    name_contains: Some("does-not-exist".to_owned()),
                    parent_index_number: Some(42),
                    ..query.clone()
                })
                .await
                .unwrap()
                .total_record_count,
            3,
            "GetResult ignores NameContains and permits a null ParentIndexNumber"
        );
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    has_parental_rating: Some(true),
                    ..query.clone()
                })
                .await
                .unwrap()
                .total_record_count,
            0
        );
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    has_parental_rating: Some(false),
                    ..query.clone()
                })
                .await
                .unwrap()
                .total_record_count,
            3
        );
        crate::test_support::seed_user_data(&db, user_id, first, false, None).await;
        sqlx::query(r#"UPDATE "UserData" SET "PlaybackPositionTicks" = 10 WHERE "UserId" = ?1 AND "ItemId" = ?2"#).bind(guid_to_db(user_id)).bind(guid_to_db(first)).execute(db.writer()).await.unwrap();
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    is_resumable: Some(true),
                    ..query.clone()
                })
                .await
                .unwrap()
                .items[0]
                .id,
            guid_to_db(first),
            "GetResult reads the root folder's own resume position rather than the SQL series/season rollup"
        );
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    is_resumable: Some(false),
                    ..query.clone()
                })
                .await
                .unwrap()
                .total_record_count,
            2
        );
        for (leaf, parent) in [
            (Uuid::from_u128(0x9803), first),
            (Uuid::from_u128(0x9804), second),
        ] {
            sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
                .bind(guid_to_db(leaf))
                .bind(guid_to_db(parent))
                .execute(db.writer())
                .await
                .unwrap();
        }
        crate::test_support::seed_user_data(&db, user_id, Uuid::from_u128(0x9803), true, None)
            .await;
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    is_played: Some(true),
                    ..query.clone()
                })
                .await
                .unwrap()
                .items
                .iter()
                .filter_map(|row| row.name.as_deref())
                .collect::<Vec<_>>(),
            ["First", "Playlists"],
            "folder played state comes from descendants; empty plugin folders count as played"
        );
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    is_played: Some(false),
                    ..query.clone()
                })
                .await
                .unwrap()
                .items[0]
                .id,
            guid_to_db(second)
        );
        let page = repository
            .get_items(&InternalItemsQuery {
                start_index: Some(1),
                limit: Some(1),
                order_by: vec![(ItemSortBy::SortName, SortOrder::Descending)],
                ..query.clone()
            })
            .await
            .unwrap();
        assert_eq!(page.total_record_count, 3);
        assert_eq!(page.items[0].id, guid_to_db(playlists));
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    start_index: Some(99),
                    limit: Some(1),
                    ..query.clone()
                })
                .await
                .unwrap()
                .total_record_count,
            3
        );
        let zero_limit = repository
            .get_items(&InternalItemsQuery {
                limit: Some(0),
                ..query.clone()
            })
            .await
            .unwrap();
        assert_eq!(
            zero_limit.items.len(),
            3,
            "GetResult applies a limit only when positive"
        );
        let filtered = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::CollectionFolder],
            name_starts_with: Some("S".to_owned()),
            ..query.clone()
        };
        assert_eq!(repository.get_item_ids(&filtered).await.unwrap(), [second]);
        assert_eq!(
            repository.get_item_list(&filtered).await.unwrap()[0].id,
            guid_to_db(second)
        );
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    include_item_types: vec![BaseItemKind::Movie],
                    ..query.clone()
                })
                .await
                .unwrap()
                .total_record_count,
            0,
            "recursive=true still means root children in this view"
        );
        let adjacent = repository
            .get_items(&InternalItemsQuery {
                adjacent_to: Some(playlists),
                start_index: Some(1),
                limit: Some(1),
                ..query.clone()
            })
            .await
            .unwrap();
        assert_eq!(adjacent.total_record_count, 3);
        assert_eq!(adjacent.items[0].id, guid_to_db(playlists));
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    adjacent_to: Some(Uuid::new_v4()),
                    ..query.clone()
                })
                .await
                .unwrap()
                .total_record_count,
            0
        );
        // AllowedTags exempts these immediate root children. Folder permissions
        // still remove the other library; plugin folders retain source visibility.
        crate::user_entity_ext::set_preference(
            db.pool(),
            &guid_to_db(user_id),
            PreferenceKind::AllowedTags,
            &["only-media".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(repository.get_items(&query).await.unwrap().items.len(), 3);
        sqlx::query(r#"UPDATE "Permissions" SET "Value" = 0 WHERE "UserId" = ?1 AND "Kind" = ?2"#)
            .bind(guid_to_db(user_id))
            .bind(i32::from(PermissionKind::EnableAllFolders))
            .execute(db.writer())
            .await
            .unwrap();
        set_folder_preference(&db, user_id, PreferenceKind::EnabledFolders, &[first]).await;
        let restricted = repository.get_items(&query).await.unwrap();
        assert_eq!(
            restricted
                .items
                .iter()
                .filter_map(|row| row.name.as_deref())
                .collect::<Vec<_>>(),
            ["First", "Playlists"]
        );
        set_folder_preference(
            &db,
            user_id,
            PreferenceKind::BlockedMediaFolders,
            &[first, second],
        )
        .await;
        assert_eq!(
            repository
                .get_items(&query)
                .await
                .unwrap()
                .items
                .iter()
                .filter_map(|row| row.name.as_deref())
                .collect::<Vec<_>>(),
            ["Playlists"]
        );
        // A filtered empty scope must not fall through to the server catalogue.
        assert_eq!(
            repository
                .get_items(&InternalItemsQuery {
                    include_item_types: vec![BaseItemKind::CollectionFolder],
                    ..query
                })
                .await
                .unwrap()
                .total_record_count,
            0
        );
    }

    /// A mixed library has no collection type upstream (`mixed.collection`
    /// parses to none), so grouped, it joins a movies view and its items are
    /// browsed there, as the views list puts it in that view.
    #[tokio::test]
    async fn a_grouped_mixed_library_is_browsed_in_a_movies_view() {
        let db = test_db().await;
        let repository = repo(&db);
        let (first, second, user_id, user) = two_libraries(&db).await;
        set_collection_type(&db, first, "movies").await;
        set_collection_type(&db, second, "mixed").await;
        let view = Uuid::from_u128(0x9F91);
        seed_item_with_data(
            &db,
            view,
            BaseItemKind::UserView,
            "Movies",
            r#"{"ViewType":"movies"}"#,
        )
        .await;
        set_folder_preference(
            &db,
            user_id,
            PreferenceKind::GroupedFolders,
            &[first, second],
        )
        .await;
        let mut names: Vec<String> = repository
            .get_item_list(&InternalItemsQuery {
                parent_id: view,
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
        assert_eq!(names, ["In First", "In Second"]);
    }
}

#[cfg(test)]
mod folder_get_result_tests {
    use super::*;
    use crate::test_support::{
        seed_folder_item, seed_named_item, seed_provider_id, seed_user_data,
        seed_user_with_defaults, test_db,
    };
    use ferrofin_model::data::MediaType;

    fn repository(db: &Database) -> FerrofinItemRepository {
        FerrofinItemRepository::new(
            db.clone(),
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
        )
    }

    async fn ordered_rows(
        repository: &FerrofinItemRepository,
        ids: &[Uuid],
    ) -> Vec<BaseItemEntity> {
        let mut rows = Vec::new();
        for id in ids {
            rows.push(repository.retrieve_item(*id).await.unwrap().unwrap());
        }
        rows
    }

    fn ids(rows: &[BaseItemEntity]) -> Vec<Uuid> {
        rows.iter()
            .map(|row| Uuid::parse_str(&row.id).unwrap())
            .collect()
    }

    #[test]
    fn literal_name_prefixes_use_case_insensitive_invariant_combining_sequences() {
        let collator = folder_name_collator(&InternalItemsQuery {
            name_starts_with: Some("x".to_owned()),
            ..Default::default()
        })
        .unwrap()
        .unwrap();
        for (name, prefix, expected) in [
            ("a%b folder", "A%B", true),
            ("axxb folder", "a%b", false),
            ("a_b folder", "A_B", true),
            ("axb folder", "a_b", false),
            ("ÉCLAIR", "é", true),
            ("e\u{301}clair", "É", true),
            ("éclair", "e", false),
            ("e\u{301}clair", "e", false),
            ("e\u{301}\u{323}clair", "e\u{301}", false),
            (" folders", " ", true),
            ("folders", " ", false),
            ("folder", "\u{ad}FOL", true),
        ] {
            assert_eq!(
                folder_invariant_starts_with(collator, name, prefix),
                expected,
                "{name:?}/{prefix:?}"
            );
        }
        let row = BaseItemEntity {
            sort_name: Some("éclair".to_owned()),
            ..Default::default()
        };
        let query = InternalItemsQuery {
            name_starts_with_or_greater: Some("e".to_owned()),
            name_less_than: Some("f".to_owned()),
            ..Default::default()
        };
        assert!(folder_child_matches_fields(
            &row,
            &query,
            folder_name_collator(&query).unwrap()
        ));
        let whitespace = InternalItemsQuery {
            name_starts_with: Some(" ".to_owned()),
            ..Default::default()
        };
        assert!(!folder_child_matches_fields(
            &row,
            &whitespace,
            folder_name_collator(&whitespace).unwrap()
        ));
    }

    #[test]
    fn source_ordinal_sort_preserves_case_ties_and_supplementary_order() {
        assert!(folder_ordinal_compare(Some("ÉCLAIR"), Some("éclair")).is_eq());
        for (left, right, expected) in [
            ("\u{10000}", "\u{e000}", std::cmp::Ordering::Greater),
            ("\u{e000}", "\u{10000}", std::cmp::Ordering::Less),
            ("\u{10000}", "\u{ffff}", std::cmp::Ordering::Greater),
            ("\u{10400}", "\u{10428}", std::cmp::Ordering::Equal),
            ("x\u{10000}", "x\u{e000}", std::cmp::Ordering::Greater),
            ("\u{10000}", "\u{10001}", std::cmp::Ordering::Less),
            ("\u{10000}", "\u{20000}", std::cmp::Ordering::Less),
        ] {
            assert_eq!(folder_ordinal_compare(Some(left), Some(right)), expected);
        }
        assert!(!folder_ordinal_compare(Some("straße"), Some("STRASSE")).is_eq());
        let mut rows = [
            BaseItemEntity {
                name: Some("second".to_owned()),
                sort_name: Some("SAME".to_owned()),
                ..Default::default()
            },
            BaseItemEntity {
                name: Some("first".to_owned()),
                sort_name: Some("same".to_owned()),
                ..Default::default()
            },
        ];
        rows.sort_by(|a, b| {
            folder_ordinal_compare(Some(&folder_sort_name(a)), Some(&folder_sort_name(b)))
        });
        assert_eq!(rows[0].name.as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn raw_named_arrays_keep_accents_and_accept_any_existing_referenced_item() {
        let db = test_db().await;
        let repo = repository(&db);
        let candidate = Uuid::new_v4();
        let accented = Uuid::new_v4();
        let cleaned = Uuid::new_v4();
        seed_folder_item(&db, candidate, BaseItemKind::Folder, "candidate", None).await;
        seed_named_item(&db, accented, BaseItemKind::Movie, "CAFÉ").await;
        seed_named_item(&db, cleaned, BaseItemKind::Genre, "cafe").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "Genres" = 'Café', "Tags" = 'ÉCLAIR', "Studios" = 'Café' WHERE "Id" = ?1"#)
            .bind(guid_to_db(candidate)).execute(db.writer()).await.unwrap();
        let rows = ordered_rows(&repo, &[candidate]).await;
        for query in [
            InternalItemsQuery {
                genres: vec!["cafe".to_owned()],
                ..Default::default()
            },
            InternalItemsQuery {
                tags: vec!["eclair".to_owned()],
                ..Default::default()
            },
            InternalItemsQuery {
                genre_ids: vec![cleaned],
                ..Default::default()
            },
            InternalItemsQuery {
                studio_ids: vec![cleaned],
                ..Default::default()
            },
            InternalItemsQuery {
                studio_ids: vec![Uuid::new_v4()],
                ..Default::default()
            },
        ] {
            assert!(
                repo.filter_folder_children(rows.clone(), &query)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        let query = InternalItemsQuery {
            genres: vec!["CAFÉ".to_owned()],
            tags: vec!["éclair".to_owned()],
            studio_ids: vec![accented],
            genre_ids: vec![accented],
            ..Default::default()
        };
        assert_eq!(
            ids(&repo.filter_folder_children(rows, &query).await.unwrap()),
            [candidate]
        );
    }

    #[tokio::test]
    async fn liked_false_excludes_unrated_and_favorite_or_liked_accepts_rating_alone() {
        let db = test_db().await;
        let repo = repository(&db);
        let user_id = Uuid::new_v4();
        let user = seed_user_with_defaults(&db, user_id).await;
        let liked = Uuid::new_v4();
        let disliked = Uuid::new_v4();
        let unrated = Uuid::new_v4();
        let favorite = Uuid::new_v4();
        for id in [liked, disliked, unrated, favorite] {
            seed_folder_item(&db, id, BaseItemKind::Folder, "folder", None).await;
            seed_user_data(&db, user_id, id, false, None).await;
        }
        sqlx::query(r#"UPDATE "UserData" SET "Rating" = 6.5, "Likes" = 0 WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(liked))
            .execute(db.writer())
            .await
            .unwrap();
        sqlx::query(r#"UPDATE "UserData" SET "Rating" = 6.4, "Likes" = 1 WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(disliked))
            .execute(db.writer())
            .await
            .unwrap();
        sqlx::query(r#"UPDATE "UserData" SET "IsFavorite" = 1 WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(favorite))
            .execute(db.writer())
            .await
            .unwrap();
        let rows = ordered_rows(&repo, &[unrated, liked, favorite, disliked]).await;
        for (query, wanted) in [
            (
                InternalItemsQuery {
                    is_liked: Some(true),
                    user: Some(user.clone()),
                    ..Default::default()
                },
                vec![liked],
            ),
            (
                InternalItemsQuery {
                    is_liked: Some(false),
                    user: Some(user.clone()),
                    ..Default::default()
                },
                vec![disliked],
            ),
            (
                InternalItemsQuery {
                    is_favorite_or_liked: Some(true),
                    user: Some(user.clone()),
                    ..Default::default()
                },
                vec![liked, favorite],
            ),
            (
                InternalItemsQuery {
                    is_favorite_or_liked: Some(false),
                    user: Some(user.clone()),
                    ..Default::default()
                },
                vec![unrated, disliked],
            ),
            (
                InternalItemsQuery {
                    is_favorite: Some(true),
                    user: Some(user.clone()),
                    ..Default::default()
                },
                vec![favorite],
            ),
        ] {
            assert_eq!(
                ids(&repo
                    .filter_folder_children(rows.clone(), &query)
                    .await
                    .unwrap()),
                wanted
            );
        }
    }

    #[tokio::test]
    async fn current_movie_provider_key_wins_over_conflicting_retired_rows() {
        let db = test_db().await;
        let repo = repository(&db);
        let user_id = Uuid::new_v4();
        let user = seed_user_with_defaults(&db, user_id).await;
        let movie = Uuid::new_v4();
        seed_named_item(&db, movie, BaseItemKind::Movie, "movie").await;
        seed_provider_id(&db, movie, "Tmdb", "123").await;
        seed_user_data(&db, user_id, movie, false, None).await;
        // Stale own-ID row says favorite/resume/played; source current TMDB row
        // says disliked/unplayed. ANY-row SQL would select the movie incorrectly.
        sqlx::query(r#"UPDATE "UserData" SET "IsFavorite" = 1, "Rating" = 10, "PlaybackPositionTicks" = 55, "Played" = 1 WHERE "ItemId" = ?1"#).bind(guid_to_db(movie)).execute(db.writer()).await.unwrap();
        sqlx::query(r#"INSERT INTO "UserData" ("ItemId","UserId","CustomDataKey","IsFavorite","PlayCount","PlaybackPositionTicks","Played","Rating") VALUES (?1,?2,'123',0,0,0,0,1)"#).bind(guid_to_db(movie)).bind(guid_to_db(user_id)).execute(db.writer()).await.unwrap();
        let rows = ordered_rows(&repo, &[movie]).await;
        for query in [
            InternalItemsQuery {
                is_favorite: Some(true),
                user: Some(user.clone()),
                ..Default::default()
            },
            InternalItemsQuery {
                is_resumable: Some(true),
                user: Some(user.clone()),
                ..Default::default()
            },
            InternalItemsQuery {
                is_played: Some(true),
                user: Some(user.clone()),
                ..Default::default()
            },
            InternalItemsQuery {
                is_liked: Some(true),
                user: Some(user.clone()),
                ..Default::default()
            },
        ] {
            assert!(
                repo.filter_folder_children(rows.clone(), &query)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(
            ids(&repo
                .filter_folder_children(
                    rows,
                    &InternalItemsQuery {
                        is_liked: Some(false),
                        user: Some(user),
                        ..Default::default()
                    }
                )
                .await
                .unwrap()),
            [movie]
        );
    }

    /// `Video.SourceType` is `LiveTV` while the file is being recorded, so the
    /// recording is keyed by its own id, not by its channel's external id; a
    /// video that is not recording (or any other kind) keeps the channel key.
    #[tokio::test]
    async fn an_active_recording_is_not_keyed_by_its_channel_external_id() {
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let user = seed_user_with_defaults(&db, user_id).await;
        let slot = ActiveRecordingSlot::default();
        let repo = repository(&db).with_active_recordings(Arc::clone(&slot));
        let mut cases = Vec::new();
        for (kind, path, wants_external) in [
            (BaseItemKind::Movie, "/rec/active.ts", false),
            (BaseItemKind::Movie, "/rec/finished.ts", true),
            (BaseItemKind::Episode, "/rec/active.ts", false),
            (BaseItemKind::Folder, "/rec/active.ts", true),
        ] {
            let id = Uuid::new_v4();
            seed_named_item(&db, id, kind, "recording").await;
            let mut entity = crate::test_support::fetch_item(&db, id).await;
            entity.path = Some(path.to_owned());
            entity.channel_id = Some(guid_to_db(Uuid::new_v4()));
            entity.external_id = Some("channel-key".to_owned());
            crate::test_support::save_item(&db, &entity).await;
            // The canonical own-id row and the channel-keyed row disagree on
            // favorite: only the key the item really has decides.
            seed_user_data(&db, user_id, id, false, None).await;
            sqlx::query(r#"INSERT INTO "UserData" ("ItemId","UserId","CustomDataKey","IsFavorite","PlayCount","PlaybackPositionTicks","Played") VALUES (?1,?2,'channel-key',1,0,0,0)"#)
                .bind(guid_to_db(id)).bind(guid_to_db(user_id)).execute(db.writer()).await.unwrap();
            cases.push((id, wants_external));
        }
        let ordered: Vec<_> = cases.iter().map(|(id, _)| *id).collect();
        let favorites = || async {
            let rows = ordered_rows(&repo, &ordered).await;
            ids(&repo
                .filter_folder_children(
                    rows,
                    &InternalItemsQuery {
                        is_favorite: Some(true),
                        user: Some(user.clone()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap())
        };
        // Nothing bound yet: no recording is active, so every item has its channel key.
        assert_eq!(favorites().await, ordered);
        slot.set(Arc::new(|| {
            Box::pin(async { Ok(vec!["/rec/active.ts".to_owned()]) })
        }))
        .ok();
        let expected: Vec<_> = cases
            .iter()
            .filter(|(_, wants_external)| *wants_external)
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(favorites().await, expected);
    }

    #[tokio::test]
    async fn adopted_channel_external_key_precedes_own_id_and_unmatched_rows_have_fallback() {
        let db = test_db().await;
        let repo = repository(&db);
        let user_id = Uuid::new_v4();
        let user = seed_user_with_defaults(&db, user_id).await;
        let channel = Uuid::new_v4();
        let fallback = Uuid::new_v4();
        for id in [channel, fallback] {
            seed_folder_item(&db, id, BaseItemKind::Folder, "folder", None).await;
            seed_user_data(&db, user_id, id, false, None).await;
        }
        let mut entity = crate::test_support::fetch_item(&db, channel).await;
        entity.external_id = Some("channel-key".to_owned());
        entity.channel_id = Some(guid_to_db(Uuid::new_v4()));
        entity.data = Some(r#"{"SourceType":0}"#.to_owned());
        crate::test_support::save_item(&db, &entity).await;
        sqlx::query(r#"UPDATE "UserData" SET "CustomDataKey" = 'retired-only', "IsFavorite" = 1 WHERE "ItemId" = ?1"#).bind(guid_to_db(fallback)).execute(db.writer()).await.unwrap();
        sqlx::query(r#"INSERT INTO "UserData" ("ItemId","UserId","CustomDataKey","IsFavorite","PlayCount","PlaybackPositionTicks","Played") VALUES (?1,?2,'channel-key',1,0,0,0)"#).bind(guid_to_db(channel)).bind(guid_to_db(user_id)).execute(db.writer()).await.unwrap();
        let rows = ordered_rows(&repo, &[fallback, channel]).await;
        assert_eq!(
            ids(&repo
                .filter_folder_children(
                    rows,
                    &InternalItemsQuery {
                        is_favorite: Some(true),
                        user: Some(user),
                        ..Default::default()
                    }
                )
                .await
                .unwrap()),
            [fallback, channel]
        );
    }

    #[tokio::test]
    async fn channel_key_selection_uses_real_source_facts_and_cannot_borrow_another_users_rows() {
        let db = test_db().await;
        let repo = repository(&db);
        let user_id = Uuid::new_v4();
        let user = seed_user_with_defaults(&db, user_id).await;
        let other_id = Uuid::new_v4();
        let other = crate::test_support::seed_named_user(&db, other_id, "other").await;
        crate::test_support::grant_default_permissions(&db, other_id).await;
        let mut cases = Vec::new();
        for (label, kind, channel_id, serialized_source, wants_external) in [
            (
                "real_channel_child",
                BaseItemKind::Folder,
                Some(Uuid::new_v4()),
                0,
                true,
            ),
            ("channel_kind", BaseItemKind::Channel, None, 0, true),
            ("stale_channel_token", BaseItemKind::Folder, None, 1, false),
            (
                "empty_channel_guid",
                BaseItemKind::Folder,
                Some(Uuid::nil()),
                1,
                false,
            ),
            (
                "live_tv_channel",
                BaseItemKind::LiveTvChannel,
                Some(Uuid::new_v4()),
                1,
                false,
            ),
            (
                "live_tv_program",
                BaseItemKind::LiveTvProgram,
                Some(Uuid::new_v4()),
                1,
                false,
            ),
        ] {
            let id = Uuid::new_v4();
            seed_named_item(&db, id, kind, label).await;
            let mut entity = crate::test_support::fetch_item(&db, id).await;
            entity.is_folder = matches!(kind, BaseItemKind::Folder | BaseItemKind::Channel);
            entity.channel_id = channel_id.map(guid_to_db);
            entity.external_id = Some("shared-channel-key".to_owned());
            entity.data = Some(serde_json::json!({"SourceType":serialized_source}).to_string());
            crate::test_support::save_item(&db, &entity).await;
            // Canonical attached own-ID and external rows disagree. Neither
            // another item with this external key nor another user can supply
            // this item's favorite value.
            for selected_user in [user_id, other_id] {
                seed_user_data(&db, selected_user, id, false, None).await;
                sqlx::query(r#"UPDATE "UserData" SET "CustomDataKey" = ?3, "IsFavorite" = 0 WHERE "ItemId" = ?1 AND "UserId" = ?2"#)
                    .bind(guid_to_db(id)).bind(guid_to_db(selected_user)).bind(id.to_string())
                    .execute(db.writer()).await.unwrap();
                sqlx::query(r#"INSERT INTO "UserData" ("ItemId","UserId","CustomDataKey","IsFavorite","PlayCount","PlaybackPositionTicks","Played") VALUES (?1,?2,'shared-channel-key',?3,0,0,0)"#)
                    .bind(guid_to_db(id)).bind(guid_to_db(selected_user)).bind(selected_user == user_id)
                    .execute(db.writer()).await.unwrap();
            }
            cases.push((id, wants_external));
        }
        let ordered: Vec<_> = cases.iter().map(|(id, _)| *id).collect();
        let rows = ordered_rows(&repo, &ordered).await;
        let selected = repo
            .filter_folder_children(
                rows.clone(),
                &InternalItemsQuery {
                    user: Some(user),
                    is_favorite: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            ids(&selected),
            cases
                .iter()
                .filter_map(|(id, external)| external.then_some(*id))
                .collect::<Vec<_>>()
        );
        assert!(
            repo.filter_folder_children(
                rows,
                &InternalItemsQuery {
                    user: Some(other),
                    is_favorite: Some(true),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .is_empty(),
            "attached rows are scoped to both item and user before key precedence"
        );
    }

    #[tokio::test]
    async fn leaf_played_uses_own_state_while_empty_folder_is_played_and_resume_can_be_played() {
        let db = test_db().await;
        let repo = repository(&db);
        let user_id = Uuid::new_v4();
        let user = seed_user_with_defaults(&db, user_id).await;
        let leaf = Uuid::new_v4();
        let empty = Uuid::new_v4();
        seed_named_item(&db, leaf, BaseItemKind::Movie, "leaf").await;
        seed_folder_item(&db, empty, BaseItemKind::Folder, "empty", None).await;
        seed_user_data(&db, user_id, leaf, false, None).await;
        let rows = ordered_rows(&repo, &[leaf, empty]).await;
        assert_eq!(
            ids(&repo
                .filter_folder_children(
                    rows.clone(),
                    &InternalItemsQuery {
                        is_played: Some(true),
                        user: Some(user.clone()),
                        ..Default::default()
                    }
                )
                .await
                .unwrap()),
            [empty]
        );
        assert_eq!(
            ids(&repo
                .filter_folder_children(
                    rows.clone(),
                    &InternalItemsQuery {
                        is_played: Some(false),
                        user: Some(user.clone()),
                        ..Default::default()
                    }
                )
                .await
                .unwrap()),
            [leaf]
        );
        sqlx::query(r#"UPDATE "UserData" SET "Played" = 1, "PlaybackPositionTicks" = 1 WHERE "ItemId" = ?1"#).bind(guid_to_db(leaf)).execute(db.writer()).await.unwrap();
        assert_eq!(
            ids(&repo
                .filter_folder_children(
                    rows,
                    &InternalItemsQuery {
                        is_resumable: Some(true),
                        user: Some(user),
                        ..Default::default()
                    }
                )
                .await
                .unwrap()),
            [leaf]
        );
    }

    #[tokio::test]
    async fn empty_provider_values_do_not_count_as_ids_and_chapter_images_use_index_zero() {
        let db = test_db().await;
        let repo = repository(&db);
        let empty = Uuid::new_v4();
        let present = Uuid::new_v4();
        for id in [empty, present] {
            seed_folder_item(&db, id, BaseItemKind::Folder, "folder", None).await;
        }
        seed_provider_id(&db, empty, "Imdb", "").await;
        seed_provider_id(&db, present, "iMdB", "tt1").await;
        for (id, index, path) in [
            (empty, 0, ""),
            (empty, 1, "later.jpg"),
            (present, 0, "first.jpg"),
        ] {
            sqlx::query(r#"INSERT INTO "Chapters" ("ItemId","ChapterIndex","ImagePath","StartPositionTicks") VALUES (?1,?2,?3,0)"#).bind(guid_to_db(id)).bind(index).bind(path).execute(db.writer()).await.unwrap();
        }
        let rows = ordered_rows(&repo, &[empty, present]).await;
        assert_eq!(
            ids(&repo
                .filter_folder_children(
                    rows.clone(),
                    &InternalItemsQuery {
                        has_imdb_id: Some(true),
                        ..Default::default()
                    }
                )
                .await
                .unwrap()),
            [present]
        );
        assert_eq!(
            ids(&repo
                .filter_folder_children(
                    rows.clone(),
                    &InternalItemsQuery {
                        has_imdb_id: Some(false),
                        has_tmdb_id: Some(false),
                        has_tvdb_id: Some(false),
                        ..Default::default()
                    }
                )
                .await
                .unwrap()),
            [empty]
        );
        assert_eq!(
            ids(&repo
                .filter_folder_children(
                    rows,
                    &InternalItemsQuery {
                        image_types: vec![ImageType::Chapter],
                        ..Default::default()
                    }
                )
                .await
                .unwrap()),
            [present]
        );
    }

    // One ordered fixture compares the source whitelist and nullable-field defaults.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn typed_filter_preserves_supplied_order_and_ignores_unlisted_request_filters() {
        let db = test_db().await;
        let repo = repository(&db);
        let later = Uuid::new_v4();
        let earlier = Uuid::new_v4();
        seed_folder_item(&db, later, BaseItemKind::Folder, "Zulu", None).await;
        seed_folder_item(&db, earlier, BaseItemKind::Folder, "Alpha", None).await;
        let rows = ordered_rows(&repo, &[later, earlier]).await;
        let query = InternalItemsQuery {
            name_contains: Some("missing".to_owned()),
            search_term: Some("missing".to_owned()),
            limit: Some(1),
            start_index: Some(100),
            parent_index_number: Some(42),
            ..Default::default()
        };
        assert_eq!(
            ids(&repo
                .filter_folder_children(rows.clone(), &query)
                .await
                .unwrap()),
            [later, earlier]
        );
        assert_eq!(
            ids(&repo
                .filter_folder_children(
                    rows.clone(),
                    &InternalItemsQuery {
                        official_ratings: vec![String::new()],
                        ..Default::default()
                    }
                )
                .await
                .unwrap()),
            [later, earlier]
        );
        assert_eq!(
            ids(&repo
                .filter_folder_children(
                    rows.clone(),
                    &InternalItemsQuery {
                        media_types: vec![MediaType::Unknown],
                        ..Default::default()
                    }
                )
                .await
                .unwrap()),
            [later, earlier]
        );
        assert!(
            repo.filter_folder_children(
                rows.clone(),
                &InternalItemsQuery {
                    media_types: vec![MediaType::Video],
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert!(
            repo.filter_folder_children(
                rows.clone(),
                &InternalItemsQuery {
                    is_virtual_item: Some(true),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert!(
            repo.filter_folder_children(
                rows.clone(),
                &InternalItemsQuery {
                    is_folder: Some(false),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert!(
            repo.filter_folder_children(
                rows.clone(),
                &InternalItemsQuery {
                    include_item_types: vec![BaseItemKind::Movie],
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert!(
            repo.filter_folder_children(
                rows.clone(),
                &InternalItemsQuery {
                    exclude_item_types: vec![BaseItemKind::Folder],
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            ids(&repo
                .filter_folder_children(
                    rows,
                    &InternalItemsQuery {
                        item_ids: vec![earlier],
                        ..Default::default()
                    }
                )
                .await
                .unwrap()),
            [earlier]
        );
    }

    #[test]
    fn numeric_date_presence_and_nullable_parent_filters_use_source_defaults() {
        let date = chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let row = BaseItemEntity {
            is_locked: true,
            overview: Some(" ".to_owned()),
            custom_rating: Some(" ".to_owned()),
            community_rating: Some(6.5),
            critic_rating: Some(70.0),
            index_number: Some(2),
            production_year: Some(2025),
            premiere_date: Some(date),
            official_rating: Some("PG".to_owned()),
            ..Default::default()
        };
        let query = InternalItemsQuery {
            is_locked: Some(true),
            has_overview: Some(true),
            has_parental_rating: Some(true),
            years: vec![2025],
            min_community_rating: Some(6.5),
            min_critic_rating: Some(70.0),
            min_index_number: Some(2),
            min_premiere_date: Some(date),
            max_premiere_date: Some(date),
            official_ratings: vec!["PG".to_owned()],
            parent_index_number: Some(99),
            ..Default::default()
        };
        assert!(folder_child_matches_fields(&row, &query, None));
        for absent in [
            BaseItemEntity {
                community_rating: None,
                ..row.clone()
            },
            BaseItemEntity {
                critic_rating: None,
                ..row.clone()
            },
            BaseItemEntity {
                index_number: None,
                ..row.clone()
            },
            BaseItemEntity {
                premiere_date: None,
                ..row.clone()
            },
            BaseItemEntity {
                production_year: None,
                ..row.clone()
            },
            BaseItemEntity {
                official_rating: Some("pg".to_owned()),
                ..row.clone()
            },
            BaseItemEntity {
                parent_index_number: Some(1),
                ..row.clone()
            },
            BaseItemEntity {
                overview: Some(String::new()),
                ..row.clone()
            },
            BaseItemEntity {
                is_locked: false,
                ..row.clone()
            },
        ] {
            assert!(!folder_child_matches_fields(&absent, &query, None));
        }
        assert!(folder_child_matches_fields(
            &row,
            &InternalItemsQuery {
                min_index_number: Some(0),
                exclude_item_ids: vec![Uuid::new_v4()],
                ..Default::default()
            },
            None
        ));
    }
    #[tokio::test]
    async fn shared_sort_keeps_no_order_and_name_ties_without_sql_name_tiebreaks() {
        use ferrofin_model::dto::SortOrder;
        use ferrofin_model::live_tv::ItemSortBy;
        let db = test_db().await;
        let repo = repository(&db);
        let rows = vec![
            BaseItemEntity {
                name: Some("Zulu".to_owned()),
                sort_name: Some("SAME".to_owned()),
                ..Default::default()
            },
            BaseItemEntity {
                name: Some("Alpha".to_owned()),
                sort_name: Some("same".to_owned()),
                ..Default::default()
            },
        ];
        assert_eq!(
            repo.sort_folder_children(rows.clone(), &InternalItemsQuery::default())
                .await
                .unwrap(),
            rows
        );
        assert_eq!(
            repo.sort_folder_children(
                rows.clone(),
                &InternalItemsQuery {
                    order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
            rows
        );
        let names = repo
            .sort_folder_children(
                rows.clone(),
                &InternalItemsQuery {
                    order_by: vec![
                        (ItemSortBy::SortName, SortOrder::Ascending),
                        (ItemSortBy::Name, SortOrder::Ascending),
                    ],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(names[0].name.as_deref(), Some("Alpha"));
        assert_eq!(
            repo.sort_folder_children(
                rows.clone(),
                &InternalItemsQuery {
                    order_by: vec![(ItemSortBy::Name, SortOrder::Descending)],
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
            rows
        );
    }
    #[tokio::test]
    async fn root_folder_preferences_apply_only_to_collection_folders_not_views_or_plugins() {
        use crate::aggregate_folder::RootFolderIds;
        let db = test_db().await;
        let user_id = Uuid::new_v4();
        let user = seed_user_with_defaults(&db, user_id).await;
        let roots = RootFolderIds {
            user_root: Uuid::new_v4(),
            aggregate: Uuid::new_v4(),
        };
        seed_folder_item(
            &db,
            roots.user_root,
            BaseItemKind::UserRootFolder,
            "root",
            None,
        )
        .await;
        seed_folder_item(
            &db,
            roots.aggregate,
            BaseItemKind::AggregateFolder,
            "physical",
            None,
        )
        .await;
        let library = Uuid::new_v4();
        let view = Uuid::new_v4();
        let plugin = Uuid::new_v4();
        seed_folder_item(
            &db,
            library,
            BaseItemKind::CollectionFolder,
            "library",
            Some(roots.user_root),
        )
        .await;
        seed_folder_item(
            &db,
            view,
            BaseItemKind::UserView,
            "view",
            Some(roots.user_root),
        )
        .await;
        seed_folder_item(
            &db,
            plugin,
            BaseItemKind::PlaylistsFolder,
            "plugin",
            Some(roots.aggregate),
        )
        .await;
        crate::user_entity_ext::set_preference(
            db.pool(),
            &guid_to_db(user_id),
            PreferenceKind::BlockedMediaFolders,
            &[guid_to_db(view), guid_to_db(plugin)],
        )
        .await
        .unwrap();
        let repository = repository(&db).with_root_ids(roots);
        let result = Box::pin(repository.user_root_children(&InternalItemsQuery {
            user: Some(user),
            parent_id: roots.user_root,
            user_root_children: true,
            ..Default::default()
        }))
        .await
        .unwrap()
        .unwrap();
        let seen = ids(&result);
        for id in [library, view, plugin] {
            assert!(seen.contains(&id));
        }
    }
}

#[cfg(test)]
mod folder_forced_sort_tests {
    use super::*;
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;

    #[tokio::test]
    async fn forced_source_key_overrides_stale_stored_key_in_filter_and_name_order() {
        let db = crate::test_support::test_db().await;
        let repo = FerrofinItemRepository::new(
            db,
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
        );
        let forced = BaseItemEntity {
            name: Some("Original title".to_owned()),
            type_: stored_type_name(BaseItemKind::Folder).unwrap().to_owned(),
            sort_name: Some("aaa retired".to_owned()),
            forced_sort_name: Some("The Spider-Man: Homecoming".to_owned()),
            is_folder: true,
            ..Default::default()
        };
        assert_eq!(folder_sort_name(&forced), "spiderman: homecoming");
        let middle = BaseItemEntity {
            name: Some("Middle".to_owned()),
            sort_name: Some("middle".to_owned()),
            forced_sort_name: None,
            ..forced.clone()
        };
        let rows = vec![forced.clone(), middle];
        let sorted = repo
            .sort_folder_children(
                rows.clone(),
                &InternalItemsQuery {
                    order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(sorted[0].name.as_deref(), Some("Middle"));
        assert_eq!(sorted[1].name.as_deref(), Some("Original title"));
        let selected = repo
            .filter_folder_children(
                rows.clone(),
                &InternalItemsQuery {
                    name_starts_with: Some("SPIDER".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(selected, [forced]);
        assert!(
            repo.filter_folder_children(
                rows,
                &InternalItemsQuery {
                    name_starts_with: Some("aaa".to_owned()),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn forced_person_preserves_source_non_alphanumeric_rule_and_empty_override_is_absent() {
        let person = BaseItemEntity {
            type_: stored_type_name(BaseItemKind::Person).unwrap().to_owned(),
            name: Some("Display".to_owned()),
            sort_name: Some("cached".to_owned()),
            forced_sort_name: Some("  The Spider-Man".to_owned()),
            ..Default::default()
        };
        assert_eq!(folder_sort_name(&person), "The Spider-Man");
        assert_eq!(
            folder_sort_name(&BaseItemEntity {
                forced_sort_name: Some(String::new()),
                ..person
            }),
            "cached"
        );
    }

    /// A child with no stored sort name is ordered by its kind's
    /// `CreateSortName`: a season by its number, not its title.
    #[test]
    fn an_unsorted_season_orders_by_its_number() {
        let season = |index: i64, name: &str| BaseItemEntity {
            type_: stored_type_name(BaseItemKind::Season).unwrap().to_owned(),
            name: Some(name.to_owned()),
            index_number: Some(index),
            ..Default::default()
        };
        assert_eq!(folder_sort_name(&season(10, "Season 10")), "0010");
        assert_eq!(folder_sort_name(&season(2, "Season 2")), "0002");
    }
}

/// The id of the item of stored type `type_name` for each clean name in
/// `cleans`: a name with no such item is absent from the map; when several
/// match, the first by sort name, then id. Serves the artist links
/// (`LinkedChildrenService.FindArtists`, `MusicArtist`) and a music video's
/// album (`SetMusicVideoProperties`, `MusicAlbum`).
///
/// # Errors
///
/// A database error.
pub(crate) async fn item_ids_by_clean_name(
    db: &Database,
    type_name: &str,
    cleans: &[&String],
) -> Result<HashMap<String, Uuid>, ServiceError> {
    let mut ids: HashMap<String, Uuid> = HashMap::new();
    for chunk in cleans.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let sql = item_ids_by_clean_name_sql(chunk.len());
        let mut query =
            sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql)).bind(type_name);
        for clean in chunk {
            query = query.bind(clean.as_str());
        }
        let rows = query
            .fetch_all(db.pool())
            .await
            .map_err(|error| ServiceError::backend(error.to_string()))?;
        for (clean, id) in rows {
            if let Ok(uuid) = Uuid::parse_str(&id) {
                ids.entry(clean).or_insert(uuid);
            }
        }
    }
    Ok(ids)
}

/// The statement behind [`item_ids_by_clean_name`] for `names` clean names,
/// after the stored type name.
fn item_ids_by_clean_name_sql(names: usize) -> String {
    let placeholders = vec!["?"; names].join(", ");
    format!(
        r#"SELECT "CleanName", "Id" FROM "BaseItems"
           WHERE "Type" = ? AND "CleanName" IN ({placeholders})
           ORDER BY "SortName", "Id""#,
    )
}

/// The statement behind [`by_name_ids_by_clean_name`] for `keys` keys of three
/// binds each (stored type name, `ItemValues.Type`, clean name).
fn by_name_ids_sql(keys: usize) -> String {
    let values = vec!["(?, ?, ?)"; keys].join(", ");
    format!(
        r#"WITH "want"("Type", "ValueType", "CleanName") AS (VALUES {values})
           SELECT w."Type", w."CleanName", COALESCE(
               (SELECT MIN(bi."Id") FROM "BaseItems" bi
                 WHERE bi."Type" = w."Type" AND bi."CleanName" = w."CleanName"),
               (SELECT iv."ItemValueId" FROM "ItemValues" iv
                 WHERE iv."Type" = w."ValueType" AND iv."CleanValue" = w."CleanName"
                 LIMIT 1))
           FROM "want" w"#
    )
}

/// The id of the by-name `Genre`/`MusicGenre`/`Studio` item for each
/// `(kind, clean name)` key — the item a `GenreItems`/`Studios` link opens
/// (C# `GetGenreId`/`GetMusicGenreId`/`GetStudioId`).
///
/// The row is found by type and clean name, as the by-name queries find it:
/// on an adopted database it carries Jellyfin's path-derived id, unrelated to
/// the `ItemValues` id. When several rows share the name the lowest id wins,
/// the survivor `GetItemValues`' `Min(Id)` collapse lists when they also share
/// a `PresentationUniqueKey`, and a listed row otherwise. A `Genre`/`Studio` name
/// with no row falls back to its `ItemValues` id, the id a scan or
/// `ItemPersistenceService::ensure_by_name_item` gives the row; a key with
/// neither is absent from the map.
///
/// # Errors
///
/// A database error.
pub(crate) async fn by_name_ids_by_clean_name(
    db: &Database,
    keys: &[(BaseItemKind, String)],
) -> Result<HashMap<BaseItemKind, HashMap<String, Uuid>>, ServiceError> {
    let mut ids: HashMap<BaseItemKind, HashMap<String, Uuid>> = HashMap::new();
    for chunk in keys.chunks(ferrofin_db::BATCH_BIND_CHUNK / 3) {
        let mut query = sqlx::query_as::<_, (String, String, Option<String>)>(sqlx::AssertSqlSafe(
            by_name_ids_sql(chunk.len()),
        ));
        for (kind, clean) in chunk {
            let value_type = match kind {
                BaseItemKind::Genre => Some(i32::from(ItemValueType::Genre)),
                BaseItemKind::Studio => Some(i32::from(ItemValueType::Studios)),
                // A `MusicGenre` shares `Genre`'s value space, whose id belongs
                // to the `Genre` row (see `music_genre_row`).
                _ => None,
            };
            query = query
                .bind(stored_type_name(*kind))
                .bind(value_type)
                .bind(clean.as_str());
        }
        for (type_name, clean, id) in query.fetch_all(db.pool()).await.map_err(db_err)? {
            if let (Some(kind), Some(Ok(id))) = (
                BaseItemKind::from_stored_type_name(&type_name),
                id.as_deref().map(Uuid::parse_str),
            ) {
                ids.entry(kind).or_default().insert(clean, id);
            }
        }
    }
    Ok(ids)
}

#[cfg(test)]
#[path = "item_repository/played_state_tests.rs"]
mod played_state_tests;

#[cfg(test)]
#[path = "item_repository/is_airing_tests.rs"]
mod is_airing_tests;
