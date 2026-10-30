//! Request-local evaluation of Jellyfin's `IsVisible` and `IsVisibleStandalone`.
//!
//! Raw item reads remain user-less. This service loads policy and the reachable
//! hierarchy once for a batch, without applying browse-query filters to IDs.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::{PermissionKind, PreferenceKind};
use ferrofin_db::row_cache::LibrarySet;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::localization::LocalizationManager;
use ferrofin_traits::persistence::{FolderChild, ItemRepository};
use serde_json::Value;
use uuid::Uuid;

use crate::item_type_lookup::kind_from_type_name;
use crate::item_visibility_repository as repository;
use crate::text_util::get_clean_value;

/// The kind a playlists or boxsets library only links to, from its lower-cased
/// collection type — the `itemKind` 12.2 `GetUserViews` asks
/// [`ItemVisibility::has_visible_item`] about (`UserViewManager.cs:62-73`):
/// `Playlist` for `playlists`, `BoxSet` for `boxsets`, and nothing for any
/// other type.
pub(crate) fn linked_kind(collection_type: Option<&str>) -> Option<BaseItemKind> {
    match collection_type {
        Some("playlists") => Some(BaseItemKind::Playlist),
        Some("boxsets") => Some(BaseItemKind::BoxSet),
        _ => None,
    }
}

/// The collaborators needed to evaluate item access. Results are never cached
/// across requests, so library, sharing and policy changes apply immediately.
pub struct ItemVisibility {
    db: Database,
    items: Arc<dyn ItemRepository>,
    localization: Arc<dyn LocalizationManager>,
    data_path: String,
    paths: crate::virtual_paths::VirtualPathExpander,
    /// How many times the library rows were read, for the scan's
    /// once-per-scan test.
    #[cfg(test)]
    library_loads: std::sync::atomic::AtomicUsize,
}

/// The library rows and each library's options that visibility and rating
/// evaluations read, from [`ItemVisibility::rating_libraries`]: the database's
/// cached library set ([`ferrofin_db::row_cache`]). A batch job that evaluates
/// one item at a time reads them once and passes them along.
pub(crate) struct RatingLibraries {
    set: Arc<LibrarySet>,
}

impl ItemVisibility {
    /// Creates a policy evaluator over the same database and item store as the library.
    #[must_use]
    pub fn new(
        db: Database,
        items: Arc<dyn ItemRepository>,
        localization: Arc<dyn LocalizationManager>,
        data_path: String,
    ) -> Self {
        Self {
            db,
            items,
            localization,
            data_path,
            paths: crate::virtual_paths::VirtualPathExpander::identity(),
            #[cfg(test)]
            library_loads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// How many times the library rows were read.
    #[cfg(test)]
    pub(crate) fn library_loads(&self) -> usize {
        self.library_loads.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Expands the portable data and metadata paths stored in adopted databases.
    #[must_use]
    pub fn with_virtual_paths(mut self, paths: crate::virtual_paths::VirtualPathExpander) -> Self {
        self.paths = paths;
        self
    }

    /// Evaluates rows together, preserving input order. `standalone` includes
    /// parent and collection access; false is the parent-browse `IsVisible` rule.
    ///
    /// # Errors
    /// Returns storage errors or an error for cyclic hierarchy/channel corruption.
    pub async fn visible(
        &self,
        items: &[BaseItemEntity],
        user: &UserEntity,
        standalone: bool,
    ) -> Result<Vec<bool>, ServiceError> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let libraries = self.rating_libraries().await?;
        let context = self.load(items, Some(user), &libraries.set).await?;
        items
            .iter()
            .map(|item| {
                let item = context.row(item);
                if standalone {
                    context.standalone(item)
                } else {
                    context.visible(item, false)
                }
            })
            .collect()
    }

    /// `LibraryManager.GetItemById(id, user)`: the item when `user` may see it standalone, else
    /// `None`. The item and the chain above it come in one read, so with the policy and library
    /// rows cached the check reads nothing more (gl-973).
    ///
    /// # Errors
    /// Returns storage errors or an error for cyclic hierarchy/channel corruption.
    pub async fn visible_item(
        &self,
        id: Uuid,
        user: &UserEntity,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok(self
            .visible_item_in(id, user, false)
            .await?
            .map(|(item, _)| item))
    }

    /// [`Self::visible_item`], with the rows the check read: the item's chain, item
    /// included, as stored and without the placeholder row, as
    /// `ItemRepository::retrieve_item` returns them. A caller takes a row above the item
    /// from it instead of reading that row again (gl-982).
    ///
    /// # Errors
    /// Returns storage errors or an error for cyclic hierarchy/channel corruption.
    pub async fn visible_item_with_chain(
        &self,
        id: Uuid,
        user: &UserEntity,
    ) -> Result<Option<(BaseItemEntity, Vec<BaseItemEntity>)>, ServiceError> {
        self.visible_item_in(id, user, true).await
    }

    /// [`Self::visible_item`]'s check; `keep_chain` hands the chain back too, for
    /// [`Self::visible_item_with_chain`].
    async fn visible_item_in(
        &self,
        id: Uuid,
        user: &UserEntity,
        keep_chain: bool,
    ) -> Result<Option<(BaseItemEntity, Vec<BaseItemEntity>)>, ServiceError> {
        // `ItemRepository::retrieve_item`'s rows: never the nil id or the placeholder row
        // (`ferrofin_db::PLACEHOLDER_ITEM_ID`, `00000000-0000-0000-0000-000000000001`).
        let placeholder = Uuid::from_u128(1);
        if id.is_nil() || id == placeholder {
            return Ok(None);
        }
        let mut chain = repository::reachable(&self.db, &[id]).await?;
        let Some(index) = chain.iter().position(|row| row_id(row) == id) else {
            return Ok(None);
        };
        let libraries = self.rating_libraries().await?;
        let context = self
            .load_known(
                std::slice::from_ref(&chain[index]),
                &chain,
                Some(user),
                &libraries.set,
            )
            .await?;
        let visible = context.standalone(context.row(&chain[index]))?;
        drop(context);
        if !visible {
            return Ok(None);
        }
        // The check borrowed the chain: the item is taken out of it, and copied only when the
        // chain is handed back too.
        if keep_chain {
            let item = chain[index].clone();
            chain.retain(|row| row_id(row) != placeholder);
            Ok(Some((item, chain)))
        } else {
            Ok(Some((chain.swap_remove(index), Vec::new())))
        }
    }

    /// `BaseItem.IsParentalAllowed(user, skipAllowedTagsCheck)` for each row,
    /// preserving input order: the rating, unrated and tag rules with no
    /// folder or library access check. `Person.IsVisible(user)` is this with
    /// the allowed-tags check skipped (`base.IsVisible(user, true)`).
    ///
    /// # Errors
    /// Returns storage errors or an error for cyclic hierarchy corruption.
    pub async fn parentally_allowed(
        &self,
        items: &[BaseItemEntity],
        user: &UserEntity,
        skip_allowed_tags: bool,
    ) -> Result<Vec<bool>, ServiceError> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let libraries = self.rating_libraries().await?;
        let context = self.load(items, Some(user), &libraries.set).await?;
        items
            .iter()
            .map(|item| {
                let item = context.row(item);
                context.parental(item, skip_allowed_tags)
            })
            .collect()
    }

    /// Applies Folder.GetChildren's child visibility and, for playlists and
    /// collections, GetLinkedChildren's owner/library check. Root children are
    /// supplied by the secure UserRootFolder loader; all hierarchy reads are
    /// batched in one request-local context.
    ///
    /// # Errors
    /// Propagates repository failures and hierarchy corruption.
    pub async fn folder_children(
        &self,
        children: Vec<FolderChild>,
        user: &UserEntity,
        filter_linked: bool,
        roots: &[BaseItemEntity],
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        if children.is_empty() {
            return Ok(Vec::new());
        }
        let libraries = self.rating_libraries().await?;
        let context = self
            .load_known(
                children.iter().map(|child| &child.item).chain(roots),
                &[],
                Some(user),
                &libraries.set,
            )
            .await?;
        let root_ids: HashSet<_> = roots
            .iter()
            .filter(|root| crate::kinds::is_folder(kind(root)))
            .map(row_id)
            .collect();
        let keeps = |child: &FolderChild| -> Result<bool, ServiceError> {
            let item = context.row(&child.item);
            if child.linked && filter_linked && !crate::kinds::is_item_by_name(kind(item)) {
                // BaseItem.GetOwner is a single optional owner lookup.
                let owner = uuid(item.owner_id.as_deref())
                    .and_then(|id| context.get(&id))
                    .unwrap_or(item);
                if owner
                    .path
                    .as_deref()
                    .filter(|path| !path.is_empty())
                    .is_some_and(crate::media_info_resolver::is_file_protocol)
                {
                    if !context
                        .libraries(owner)?
                        .iter()
                        .any(|id| root_ids.contains(id))
                    {
                        return Ok(false);
                    }
                } else if !context.standalone(owner)? {
                    return Ok(false);
                }
            }
            context.visible(item, false)
        };
        let kept = children
            .iter()
            .map(keeps)
            .collect::<Result<Vec<bool>, ServiceError>>()?;
        drop(context);
        // The check borrowed the children: the kept ones are moved out, not copied.
        Ok(children
            .into_iter()
            .zip(kept)
            .filter_map(|(child, kept)| kept.then_some(child.item))
            .collect())
    }

    /// `Playlist.IsVisible` for each of `playlists`, in input order, without
    /// the owner/share test the playlist manager answers itself: a shared
    /// playlist needs only 12.1's `HasParentalAllowedChild`, a playlist file
    /// `base.IsVisible(user) && HasParentalAllowedChild(user)`.
    ///
    /// # Errors
    /// Returns storage errors or an error for a cyclic hierarchy.
    pub async fn playlist_policy(
        &self,
        playlists: &[BaseItemEntity],
        user: &UserEntity,
    ) -> Result<Vec<bool>, ServiceError> {
        if playlists.is_empty() {
            return Ok(Vec::new());
        }
        let libraries = self.rating_libraries().await?;
        let context = self.load(playlists, Some(user), &libraries.set).await?;
        playlists
            .iter()
            .map(|playlist| {
                let row = context.row(playlist);
                if context.shared_playlist(row) {
                    context.has_parental_allowed_child(row, user)
                } else {
                    context.visible(row, false)
                }
            })
            .collect()
    }

    /// 12.2 `UserViewManager.HasVisibleItem` (`UserViewManager.cs:161-176`):
    /// whether `user` can see at least one `kind` item (`Playlist` or
    /// `BoxSet`) whose `TopParentId` is one of `top_parent_ids`, wherever
    /// under it the item sits — a playlist file inside a music album counts.
    /// No top parents, no item, as upstream.
    ///
    /// The caller names the top parents: those of the user's root folders
    /// (`folders.SelectMany(GetTopParentIds)`, see
    /// [`Self::has_visible_root_item`]). A probe naming none would be scoped
    /// to the user's views, which is the scope this answer decides.
    ///
    /// It reads ids only, seeking a `(Type, TopParentId)` index;
    /// `items.Any(item => item.IsVisible(user))` then checks them in doubling
    /// batches (1, 2, 4, …), so the common answer costs one row and a library
    /// of hidden ones a handful of visibility loads rather than one per item.
    ///
    /// # Errors
    /// Returns storage errors or an error for a cyclic hierarchy.
    pub async fn has_visible_item(
        &self,
        kind: BaseItemKind,
        user: &UserEntity,
        top_parent_ids: &[Uuid],
    ) -> Result<bool, ServiceError> {
        if top_parent_ids.is_empty() {
            return Ok(false);
        }
        let ids = self
            .items
            .get_item_ids(&crate::user_view_manager::visible_item_query(
                kind,
                user,
                top_parent_ids.to_vec(),
            ))
            .await?;
        let (mut start, mut size) = (0, 1);
        while start < ids.len() {
            let end = (start + size).min(ids.len());
            let rows = self.items.retrieve_items(&ids[start..end]).await?;
            if self.visible(&rows, user, false).await?.contains(&true) {
                return Ok(true);
            }
            (start, size) = (end, size * 2);
        }
        Ok(false)
    }

    /// [`Self::has_visible_item`] over the top parents of the root folders
    /// `user` can see, the libraries and the playlists folder — the question
    /// `GetUserViews` asks before it lists a playlists or boxsets library.
    ///
    /// # Errors
    /// Returns storage errors or an error for a cyclic hierarchy.
    pub async fn has_visible_root_item(
        &self,
        kind: BaseItemKind,
        user: &UserEntity,
    ) -> Result<bool, ServiceError> {
        let top_parents = crate::item_repository::root_folder_top_parents(&self.db, user).await?;
        self.has_visible_item(kind, user, &top_parents).await
    }

    /// Recomputes persisted parental scores from effective custom/official ratings
    /// and inherited metadata country, matching `BaseItem.OnMetadataChanged`.
    ///
    /// # Errors
    /// Returns storage errors or an error for a cyclic hierarchy.
    pub async fn update_rating_scores(
        &self,
        items: &mut [BaseItemEntity],
    ) -> Result<(), ServiceError> {
        self.update_rating_scores_with(items, None).await
    }

    /// [`Self::update_rating_scores`] over `libraries` read earlier with
    /// [`Self::rating_libraries`], so a batch job that scores one item at a
    /// time (the library scan) reads the libraries and their options once.
    /// Each call still reads the items' own parent chain, so an ancestor saved
    /// since `libraries` was read is seen; the library rows themselves are the
    /// snapshot `libraries` took, as the scan's library policy is.
    ///
    /// # Errors
    /// Returns storage errors or an error for a cyclic hierarchy.
    pub(crate) async fn update_rating_scores_in(
        &self,
        items: &mut [BaseItemEntity],
        libraries: &RatingLibraries,
    ) -> Result<(), ServiceError> {
        self.update_rating_scores_with(items, Some(libraries)).await
    }

    /// Reads the library rows and each library's options, which every
    /// visibility and rating evaluation needs.
    ///
    /// # Errors
    /// Returns storage errors.
    pub(crate) async fn rating_libraries(&self) -> Result<RatingLibraries, ServiceError> {
        if let Some(set) = self.db.cached_libraries() {
            return Ok(RatingLibraries { set });
        }
        // Begun before the reads: a library written meanwhile keeps this set out of the cache.
        let load = self.db.begin_library_load();
        let rows = repository::libraries(&self.db).await?;
        #[cfg(test)]
        self.library_loads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut options = HashMap::new();
        // Read the existing options without GetVirtualFolders, which also heals
        // library rows and would turn an authorization read into a mutation.
        for row in &rows {
            if let Some(path) = self.paths.expand_opt(row.path.as_deref()) {
                options.insert(
                    row_id(row),
                    crate::virtual_folder_manager::FerrofinVirtualFolderManager::read_options(
                        std::path::Path::new(&path),
                        &self.paths,
                    )
                    .await,
                );
            }
        }
        // The rows above the libraries (the user root, the root folder), where every walk ends.
        let library_ids: HashSet<Uuid> = rows.iter().map(row_id).collect();
        let above: Vec<Uuid> = rows
            .iter()
            .flat_map(references)
            .filter(|id| !library_ids.contains(id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let ancestors = if above.is_empty() {
            Vec::new()
        } else {
            repository::reachable(&self.db, &above).await?
        };
        let set = load.store(LibrarySet {
            rows,
            ancestors,
            options,
        });
        Ok(RatingLibraries { set })
    }

    async fn update_rating_scores_with(
        &self,
        items: &mut [BaseItemEntity],
        libraries: Option<&RatingLibraries>,
    ) -> Result<(), ServiceError> {
        if items.is_empty() {
            return Ok(());
        }
        let read;
        let libraries = if let Some(libraries) = libraries {
            libraries
        } else {
            read = self.rating_libraries().await?;
            &read
        };
        // Scored over the rows as passed in, then written: the context borrows them.
        let scores = {
            let context = self.load(items, None, &libraries.set).await?;
            items
                .iter()
                .map(|item| context.rating_score(context.row(item)))
                .collect::<Result<Vec<_>, ServiceError>>()?
        };
        for (item, score) in items.iter_mut().zip(scores) {
            item.unrated_type = Some(
                crate::kinds::unrated_item(kind(item), uuid(item.channel_id.as_deref()).is_some())
                    .json_name()
                    .into_owned(),
            );
            item.inherited_parental_rating_value = score.map(|score| i64::from(score.score));
            item.inherited_parental_rating_sub_value =
                score.and_then(|score| score.sub_score.map(i64::from));
        }
        Ok(())
    }

    /// [`rating_to_items`](ferrofin_providers::metadata_merge::rating_to_items)
    /// for a box set or playlist: the least restrictive of its members'
    /// ratings, scored against its `GetPreferredMetadataCountryCode()`, in
    /// link order; its own rating when no member is rated.
    ///
    /// # Errors
    /// Returns storage errors or an error for a cyclic hierarchy.
    pub async fn rating_to_items(
        &self,
        container: &BaseItemEntity,
    ) -> Result<Option<String>, ServiceError> {
        let ratings = repository::member_ratings(&self.db, row_id(container)).await?;
        let libraries = self.rating_libraries().await?;
        let context = self
            .load(std::slice::from_ref(container), None, &libraries.set)
            .await?;
        let row = context.row(container);
        let country = context.metadata_country(row)?;
        Ok(ferrofin_providers::metadata_merge::rating_to_items(
            container.official_rating.as_deref(),
            ratings.iter().map(Option::as_deref),
            |rating| self.localization.get_rating_score(rating, country),
        )
        .map(str::to_owned))
    }

    /// `playlist_links` also reads playlists' members, which only a
    /// rating-limited user's `HasParentalAllowedChild` needs.
    async fn load_hierarchy(
        &self,
        rows: &mut HashMap<Uuid, Cow<'_, BaseItemEntity>>,
        playlist_links: bool,
    ) -> Result<HashMap<Uuid, Vec<Uuid>>, ServiceError> {
        let mut examined = HashSet::new();
        let mut requested: HashSet<Uuid> = rows.keys().copied().collect();
        let mut links: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        // Each round batches all new references, including nested collections.
        // Missing references are remembered, so they cannot cause repeated reads.
        loop {
            let fresh: Vec<_> = rows
                .keys()
                .filter(|id| !examined.contains(*id))
                .copied()
                .collect();
            if fresh.is_empty() {
                break;
            }
            let containers: Vec<_> = fresh
                .iter()
                .filter(|id| match kind(&rows[id]) {
                    BaseItemKind::BoxSet => true,
                    BaseItemKind::Playlist => playlist_links,
                    _ => false,
                })
                .copied()
                .collect();
            for (parent, child) in repository::linked_children(&self.db, &containers).await? {
                if let (Some(parent), Some(child)) = (uuid(Some(&parent)), uuid(Some(&child))) {
                    links.entry(parent).or_default().push(child);
                }
            }
            let mut wanted = HashSet::new();
            for id in fresh {
                examined.insert(id);
                let row = &rows[&id];
                for next in references(row).chain(links.get(&id).into_iter().flatten().copied()) {
                    // A row the last read brought in is not asked for again.
                    if !rows.contains_key(&next) && requested.insert(next) {
                        wanted.insert(next);
                    }
                }
            }
            if wanted.is_empty() {
                break;
            }
            // The whole chain above `wanted` in one read. A reference it could not follow (a
            // missing row) is asked for once more, by itself, in the next round.
            for row in
                repository::reachable(&self.db, &wanted.into_iter().collect::<Vec<_>>()).await?
            {
                // What the caller passed (an item with unsaved changes, the cached libraries) wins.
                rows.entry(row_id(&row)).or_insert(Cow::Owned(row));
            }
        }
        Ok(links)
    }

    /// The context that evaluates `items` over `libraries` (from [`Self::rating_libraries`]).
    async fn load<'a>(
        &'a self,
        items: &'a [BaseItemEntity],
        user: Option<&'a UserEntity>,
        libraries: &'a LibrarySet,
    ) -> Result<Context<'a>, ServiceError> {
        self.load_known(items, &[], user, libraries).await
    }

    /// [`Self::load`] when the caller has read some of the hierarchy too: `known` rows are
    /// not read again. The context borrows `items`, `known` and the library set's rows; it owns
    /// the rows [`Self::load_hierarchy`] reads, and a copy of a borrowed row only when its stored
    /// path holds a virtual-path token to expand.
    async fn load_known<'a>(
        &'a self,
        items: impl IntoIterator<Item = &'a BaseItemEntity>,
        known: &'a [BaseItemEntity],
        user: Option<&'a UserEntity>,
        libraries: &'a LibrarySet,
    ) -> Result<Context<'a>, ServiceError> {
        let (permissions, preferences) = if let Some(user) = user {
            (
                repository::permissions(&self.db, &user.id).await?,
                repository::preferences(&self.db, &user.id).await?,
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let library_ids: Vec<Uuid> = libraries.rows.iter().map(row_id).collect();
        let mut rows: HashMap<Uuid, Cow<'a, BaseItemEntity>> = libraries
            .rows
            .iter()
            .chain(items)
            .map(|row| (row_id(row), Cow::Borrowed(row)))
            .collect();
        for row in known.iter().chain(&libraries.ancestors) {
            rows.entry(row_id(row)).or_insert(Cow::Borrowed(row));
        }
        let links = self
            .load_hierarchy(
                &mut rows,
                user.is_some_and(|user| user.max_parental_rating_score.is_some()),
            )
            .await?;
        // Every reader sees the expanded path; a borrowed row is copied only when it changes.
        for row in rows.values_mut() {
            let expanded = match row.path.as_deref().map(|path| self.paths.expand_cow(path)) {
                Some(Cow::Owned(path)) => Some(path),
                _ => None,
            };
            if let Some(path) = expanded {
                row.to_mut().path = Some(path);
            }
        }
        let mut playlists = HashMap::new();
        let playlist_ids: Vec<_> = rows
            .iter()
            .filter(|(_, row)| kind(row) == BaseItemKind::Playlist)
            .map(|(id, _)| *id)
            .collect();
        if let Some(user) = user {
            for (id, owner, open, shared) in
                repository::playlist_access(&self.db, &user.id, &playlist_ids).await?
            {
                if let Some(id) = uuid(Some(&id)) {
                    playlists.insert(
                        id,
                        open || shared || uuid(owner.as_deref()) == uuid(Some(&user.id)),
                    );
                }
            }
        }
        Ok(Context {
            service: self,
            user,
            rows,
            library_ids,
            links,
            libraries,
            playlists,
            permissions: permissions.into_iter().collect(),
            preferences: preferences
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        v.split(',')
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect(),
                    )
                })
                .collect(),
        })
    }
}

struct Context<'a> {
    service: &'a ItemVisibility,
    user: Option<&'a UserEntity>,
    /// Every row the checks read, by id, its path expanded: borrowed from the caller and the
    /// library set, owned when [`ItemVisibility::load_hierarchy`] read it or its path changed.
    rows: HashMap<Uuid, Cow<'a, BaseItemEntity>>,
    library_ids: Vec<Uuid>,
    links: HashMap<Uuid, Vec<Uuid>>,
    /// The library set the rows above were read with; its `options` are each library's.
    libraries: &'a LibrarySet,
    playlists: HashMap<Uuid, bool>,
    permissions: HashMap<i32, bool>,
    preferences: HashMap<i32, Vec<String>>,
}

/// The rows `row` refers to up its hierarchy: the columns [`repository::reachable`] walks.
fn references(row: &BaseItemEntity) -> impl Iterator<Item = Uuid> + '_ {
    [
        row.parent_id.as_deref(),
        row.owner_id.as_deref(),
        row.channel_id.as_deref(),
        row.season_id.as_deref(),
        row.series_id.as_deref(),
    ]
    .into_iter()
    .filter_map(uuid)
}

fn uuid(value: Option<&str>) -> Option<Uuid> {
    value
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|id| !id.is_nil())
}
fn row_id(item: &BaseItemEntity) -> Uuid {
    uuid(Some(&item.id)).unwrap_or_default()
}
fn kind(item: &BaseItemEntity) -> BaseItemKind {
    kind_from_type_name(&item.type_).unwrap_or(BaseItemKind::Folder)
}
fn data(item: &BaseItemEntity) -> Value {
    item.data
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null)
}
fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|s| !s.is_empty())
}

impl Context<'_> {
    /// The row with id `id`.
    fn get(&self, id: &Uuid) -> Option<&BaseItemEntity> {
        self.rows.get(id).map(|row| &**row)
    }

    /// The context's row for `item`'s id, path expanded; `item` itself when it has none.
    fn row<'r>(&'r self, item: &'r BaseItemEntity) -> &'r BaseItemEntity {
        self.get(&row_id(item)).unwrap_or(item)
    }

    fn preference(&self, kind: PreferenceKind) -> &[String] {
        self.preferences
            .get(&i32::from(kind))
            .map_or(&[], Vec::as_slice)
    }

    fn folder_allowed(&self, id: Uuid, channel: bool) -> bool {
        let (blocked, enabled, all) = if channel {
            (
                PreferenceKind::BlockedChannels,
                PreferenceKind::EnabledChannels,
                PermissionKind::EnableAllChannels,
            )
        } else {
            (
                PreferenceKind::BlockedMediaFolders,
                PreferenceKind::EnabledFolders,
                PermissionKind::EnableAllFolders,
            )
        };
        // `GetPreferenceValues<Guid>`: each value parsed as .NET parses a GUID,
        // and the ones that do not parse dropped, so a blocked list of only
        // those counts as empty.
        let parsed = |kind| {
            self.preference(kind)
                .iter()
                .filter_map(|v| ferrofin_util::guid_extensions::parse_dotnet_guid(v))
        };
        let mut blocked = parsed(blocked).peekable();
        if blocked.peek().is_some() {
            return !blocked.any(|v| v == id);
        }
        self.permissions
            .get(&i32::from(all))
            .copied()
            .unwrap_or(false)
            || parsed(enabled).any(|v| v == id)
    }

    fn chain<'a>(
        &'a self,
        item: &'a BaseItemEntity,
        display: bool,
    ) -> Result<Vec<&'a BaseItemEntity>, ServiceError> {
        let mut chain = vec![item];
        let mut seen = HashSet::from([row_id(item)]);
        let mut current = item;
        loop {
            let next = if display {
                match kind(current) {
                    BaseItemKind::Episode => current.season_id.as_deref(),
                    BaseItemKind::Season => current.series_id.as_deref(),
                    _ => current.parent_id.as_deref(),
                }
            } else {
                current.parent_id.as_deref()
            };
            let Some(parent) = uuid(next).and_then(|id| self.get(&id)) else {
                break;
            };
            if !seen.insert(row_id(parent)) {
                return Err(ServiceError::backend("cycle in item visibility ancestry"));
            }
            chain.push(parent);
            current = parent;
        }
        Ok(chain)
    }

    fn libraries(&self, item: &BaseItemEntity) -> Result<Vec<Uuid>, ServiceError> {
        let mut current = item;
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(row_id(current)) {
                return Err(ServiceError::backend("cycle in item visibility ownership"));
            }
            if kind(current) == BaseItemKind::CollectionFolder {
                return Ok(vec![row_id(current)]);
            }
            let parent = uuid(current.parent_id.as_deref()).and_then(|id| self.get(&id));
            if parent.is_some_and(|r| kind(r) == BaseItemKind::AggregateFolder) {
                break;
            }
            let next =
                parent.or_else(|| uuid(current.owner_id.as_deref()).and_then(|id| self.get(&id)));
            let Some(next) = next else {
                break;
            };
            current = next;
        }
        let Some(path) = nonempty(current.path.as_deref()) else {
            return Ok(Vec::new());
        };
        Ok(self
            .library_ids
            .iter()
            .copied()
            .filter(|id| {
                let row = &self.rows[id];
                row.path
                    .as_deref()
                    .is_some_and(|p| p.eq_ignore_ascii_case(path))
                    || data(row)
                        .get("PhysicalLocationsList")
                        .and_then(Value::as_array)
                        .is_some_and(|locations| {
                            locations.iter().any(|p| {
                                p.as_str().is_some_and(|p| {
                                    self.service.paths.expand(p).eq_ignore_ascii_case(path)
                                })
                            })
                        })
                    // `read_options` has already expanded these.
                    || self.libraries.options.get(id).is_some_and(|opts| {
                        opts.path_infos
                            .iter()
                            .any(|p| p.path.eq_ignore_ascii_case(path))
                    })
            })
            .collect())
    }

    fn in_data_directory(&self, item: &BaseItemEntity) -> bool {
        let Some(path) = nonempty(item.path.as_deref()) else {
            return false;
        };
        // Paths may come from an adopted Windows database; compare components,
        // not a lexical prefix (data2 must not be treated as inside data).
        let base = self
            .service
            .data_path
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase();
        !base.is_empty()
            && path
                .replace('\\', "/")
                .to_lowercase()
                .starts_with(&(base + "/"))
    }

    fn shared_playlist(&self, item: &BaseItemEntity) -> bool {
        kind(item) == BaseItemKind::Playlist
            && (self.in_data_directory(item)
                || (nonempty(item.path.as_deref()).is_none()
                    && self.playlists.contains_key(&row_id(item))))
    }

    fn modern_boxset(&self, item: &BaseItemEntity) -> bool {
        kind(item) == BaseItemKind::BoxSet
            && (nonempty(item.path.as_deref()).is_none()
                || self.in_data_directory(item)
                || self.links.get(&row_id(item)).is_some_and(|v| !v.is_empty()))
    }

    fn visible(&self, item: &BaseItemEntity, skip_allowed: bool) -> Result<bool, ServiceError> {
        let id = row_id(item);
        let user = self
            .user
            .ok_or_else(|| ServiceError::backend("missing visibility user"))?;
        if self.shared_playlist(item) {
            let shared = if let Some(allowed) = self.playlists.get(&id) {
                *allowed
            } else {
                let value = data(item);
                value
                    .get("OpenAccess")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || uuid(value.get("OwnerUserId").and_then(Value::as_str))
                        == uuid(Some(&user.id))
                    || value
                        .get("Shares")
                        .and_then(Value::as_array)
                        .is_some_and(|shares| {
                            shares.iter().any(|share| {
                                uuid(share.get("UserId").and_then(Value::as_str))
                                    == uuid(Some(&user.id))
                            })
                        })
            };
            return Ok(shared && self.has_parental_allowed_child(item, user)?);
        }
        if kind(item) == BaseItemKind::CollectionFolder
            && (!self.folder_allowed(id, false)
                || self.libraries.options.get(&id).is_some_and(|o| !o.enabled))
        {
            return Ok(false);
        }
        if kind(item) == BaseItemKind::Channel && !self.folder_allowed(id, true) {
            return Ok(false);
        }
        if !self.parental(item, skip_allowed || kind(item) == BaseItemKind::Person)? {
            return Ok(false);
        }
        if self.modern_boxset(item) {
            let children = self.links.get(&id).map_or(&[][..], Vec::as_slice);
            if children.is_empty() {
                return Ok(true);
            }
            let mut library_ids = HashSet::new();
            let value = data(item);
            if let Some(stored) = value.get("LibraryFolderIds").and_then(Value::as_array) {
                library_ids.extend(stored.iter().filter_map(|v| uuid(v.as_str())));
            } else {
                let mut queue = children.to_vec();
                let mut seen = HashSet::from([id]);
                while let Some(child) = queue.pop() {
                    if !seen.insert(child) {
                        continue;
                    }
                    if let Some(row) = self.get(&child) {
                        if kind(row) == BaseItemKind::BoxSet {
                            queue.extend(self.links.get(&child).into_iter().flatten());
                        } else {
                            library_ids.extend(self.libraries(row)?);
                        }
                    }
                }
            }
            // Upstream returns immediately when no linked library is known.
            if library_ids.is_empty() {
                return Ok(true);
            }
            let mut accessible = false;
            for library in library_ids {
                if let Some(row) = self.get(&library) {
                    accessible |= self.visible(row, false)?;
                }
            }
            // `BoxSet.IsVisible` applies the same member rule.
            if !accessible || !self.has_parental_allowed_child(item, user)? {
                return Ok(false);
            }
        }
        if kind(item) == BaseItemKind::Playlist {
            return self.has_parental_allowed_child(item, user);
        }
        Ok(true)
    }

    /// `Playlist.HasParentalAllowedChild` (v12.2 Playlist.cs:261-271): a
    /// rating-limited user sees a playlist only when it has no members or one
    /// of them passes `IsParentalAllowed(user, true)`.
    fn has_parental_allowed_child(
        &self,
        item: &BaseItemEntity,
        user: &UserEntity,
    ) -> Result<bool, ServiceError> {
        if user.max_parental_rating_score.is_none() {
            return Ok(true);
        }
        let mut members = self
            .links
            .get(&row_id(item))
            .into_iter()
            .flatten()
            .filter_map(|id| self.get(id))
            .peekable();
        if members.peek().is_none() {
            return Ok(true);
        }
        for member in members {
            if self.parental(member, true)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn standalone(&self, item: &BaseItemEntity) -> Result<bool, ServiceError> {
        if kind(item) == BaseItemKind::UserRootFolder {
            return Ok(true);
        }
        if !self.visible(item, false)? {
            return Ok(false);
        }
        if self.shared_playlist(item) || self.modern_boxset(item) {
            return Ok(true);
        }
        let chain = self.chain(item, false)?;
        for parent in chain.iter().skip(1) {
            if !self.visible(parent, true)? {
                return Ok(false);
            }
        }
        // LiveTvProgram/LiveTvChannel override SourceType to LiveTV. Their
        // ChannelId is guide metadata, not a plugin-channel access reference.
        if let Some(channel_id) = uuid(item.channel_id.as_deref())
            && !matches!(
                kind(item),
                BaseItemKind::LiveTvProgram | BaseItemKind::LiveTvChannel | BaseItemKind::TvChannel
            )
        {
            let channel = self
                .get(&channel_id)
                .ok_or_else(|| ServiceError::backend("missing item channel"))?;
            return self.visible(channel, false);
        }
        if kind(item) == BaseItemKind::Channel {
            return Ok(true);
        }
        if nonempty(chain.last().and_then(|row| row.path.as_deref())).is_none() {
            return Ok(true);
        }
        let libraries = self.libraries(item)?;
        Ok(libraries.is_empty() || libraries.iter().any(|id| self.folder_allowed(*id, false)))
    }

    fn parental(&self, item: &BaseItemEntity, skip_allowed: bool) -> Result<bool, ServiceError> {
        let parents = self.chain(item, false)?;
        let libraries = self.libraries(item)?;
        let blocked = self.preference(PreferenceKind::BlockedTags);
        let allowed = self.preference(PreferenceKind::AllowedTags);
        if !blocked.is_empty() || !allowed.is_empty() {
            let tags: HashSet<_> = parents
                .iter()
                .copied()
                .chain(libraries.iter().filter_map(|id| self.get(id)))
                .flat_map(|row| {
                    row.tags
                        .as_deref()
                        .into_iter()
                        .flat_map(|tags| tags.split('|'))
                })
                .map(get_clean_value)
                .collect();
            if blocked
                .iter()
                .filter(|t| !t.trim().is_empty())
                .any(|t| tags.contains(&get_clean_value(t)))
            {
                return Ok(false);
            }
            let parent = parents.get(1).copied().unwrap_or(item);
            let root_context = matches!(
                kind(parent),
                BaseItemKind::UserRootFolder
                    | BaseItemKind::AggregateFolder
                    | BaseItemKind::UserView
            );
            if !skip_allowed
                && !root_context
                && !allowed.is_empty()
                && !allowed
                    .iter()
                    .filter(|t| !t.trim().is_empty())
                    .any(|t| tags.contains(&get_clean_value(t)))
            {
                return Ok(false);
            }
        }
        let user = self
            .user
            .ok_or_else(|| ServiceError::backend("missing visibility user"))?;
        let score = self.rating_score(item)?;
        let Some(score) = score else {
            return Ok(!self.block_unrated(item));
        };
        let Some(max) = user.max_parental_rating_score else {
            return Ok(true);
        };
        if i64::from(score.score) != max {
            return Ok(i64::from(score.score) < max);
        }
        Ok(user
            .max_parental_rating_sub_score
            .is_none_or(|max| i64::from(score.sub_score.unwrap_or(0)) <= max))
    }

    fn rating_score(
        &self,
        item: &BaseItemEntity,
    ) -> Result<Option<ferrofin_model::entities_media::ParentalRatingScore>, ServiceError> {
        let display = self.chain(item, true)?;
        let rating = display
            .iter()
            .find_map(|row| nonempty(row.custom_rating.as_deref()))
            .or_else(|| {
                display
                    .iter()
                    .find_map(|row| nonempty(row.official_rating.as_deref()))
            });
        let country = self.metadata_country(item)?;
        Ok(rating.and_then(|r| self.service.localization.get_rating_score(r, country)))
    }

    /// `GetPreferredMetadataCountryCode()` short of the server's country,
    /// which the rating tables fall back to on `None`.
    fn metadata_country<'a>(
        &'a self,
        item: &'a BaseItemEntity,
    ) -> Result<Option<&'a str>, ServiceError> {
        let parents = self.chain(item, false)?;
        let libraries = self.libraries(item)?;
        Ok(parents
            .iter()
            .find_map(|row| nonempty(row.preferred_metadata_country_code.as_deref()))
            .or_else(|| {
                libraries
                    .iter()
                    .filter_map(|id| self.get(id))
                    .find_map(|row| nonempty(row.preferred_metadata_country_code.as_deref()))
            })
            .or_else(|| {
                libraries
                    .iter()
                    .filter_map(|id| self.libraries.options.get(id))
                    .find_map(|opts| nonempty(opts.metadata_country_code.as_deref()))
            }))
    }

    fn block_unrated(&self, item: &BaseItemEntity) -> bool {
        let kind = kind(item);
        if kind == BaseItemKind::Season {
            return false;
        }
        if !matches!(
            kind,
            BaseItemKind::Series | BaseItemKind::MusicAlbum | BaseItemKind::BoxSet
        ) && (item.is_folder
            || matches!(
                kind,
                BaseItemKind::Person
                    | BaseItemKind::Genre
                    | BaseItemKind::MusicGenre
                    | BaseItemKind::Studio
                    | BaseItemKind::Year
                    | BaseItemKind::MusicArtist
            ))
        {
            return false;
        }
        let category = crate::kinds::unrated_item(kind, uuid(item.channel_id.as_deref()).is_some());
        self.preference(PreferenceKind::BlockUnratedItems)
            .iter()
            .filter_map(|value| crate::query_restrictions::parse_unrated(value).ok())
            .any(|value| value == category)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        item_repository_over, seed_named_item, seed_user_with_defaults, test_db,
    };
    use crate::user_entity_ext::{set_permission, set_preference};
    use ferrofin_traits::persistence::{ItemPersistenceService, LinkedChildrenService};

    struct Fixture {
        db: Database,
        service: ItemVisibility,
        user: UserEntity,
    }

    impl Fixture {
        async fn new() -> Self {
            let db = test_db().await;
            // The server's caches, on (ferrofin_db::row_cache): every test below also checks that
            // the writes it makes through the persistence service and the policy setters reach
            // the next evaluation.
            db.enable_row_cache(ferrofin_db::row_cache::ROW_CACHE_TTL);
            let user = seed_user_with_defaults(&db, Uuid::from_u128(0x9000)).await;
            let service = ItemVisibility::new(
                db.clone(),
                item_repository_over(db.clone()),
                Arc::new(crate::localization_manager::LocalizationManager::new("US")),
                "/server/data".to_owned(),
            );
            Self { db, service, user }
        }

        async fn item(
            &self,
            id: u128,
            kind: BaseItemKind,
            path: Option<&str>,
            parent: Option<&BaseItemEntity>,
        ) -> BaseItemEntity {
            let id = Uuid::from_u128(id);
            seed_named_item(&self.db, id, kind, "fixture").await;
            let mut row = self
                .service
                .items
                .retrieve_item(id)
                .await
                .expect("retrieve")
                .expect("item");
            row.path = path.map(str::to_owned);
            row.parent_id = parent.map(|p| p.id.clone());
            row.is_folder = crate::kinds::is_folder(kind);
            self.save(&row).await;
            row
        }

        async fn save(&self, row: &BaseItemEntity) {
            crate::item_persistence_service::FerrofinItemPersistenceService::new(self.db.clone())
                .save_items(std::slice::from_ref(row))
                .await
                .expect("save");
        }

        async fn pref(&self, kind: PreferenceKind, values: &[&str]) {
            set_preference(
                &self.db,
                &self.user.id,
                kind,
                &values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>(),
            )
            .await
            .expect("preference");
        }

        async fn visible(&self, row: &BaseItemEntity) -> bool {
            self.service
                .visible(std::slice::from_ref(row), &self.user, true)
                .await
                .expect("visibility")[0]
        }
    }

    /// `visible_item` reads an item with the chain above it at once, and answers as the trait's
    /// lookup-then-check does: the stored row when it is visible standalone, else `None` (gl-973).
    #[tokio::test]
    async fn visible_item_answers_as_the_lookup_then_check_does() {
        let f = Fixture::new().await;
        let allowed = f
            .item(
                30,
                BaseItemKind::CollectionFolder,
                Some("/views/allowed"),
                None,
            )
            .await;
        let hidden = f
            .item(
                31,
                BaseItemKind::CollectionFolder,
                Some("/views/hidden"),
                None,
            )
            .await;
        let folder = f
            .item(32, BaseItemKind::Folder, Some("/media/a"), Some(&allowed))
            .await;
        let seen = f
            .item(
                33,
                BaseItemKind::Movie,
                Some("/media/a/seen.mkv"),
                Some(&folder),
            )
            .await;
        let unseen = f
            .item(
                34,
                BaseItemKind::Movie,
                Some("/media/b/unseen.mkv"),
                Some(&hidden),
            )
            .await;
        set_permission(&f.db, &f.user.id, PermissionKind::EnableAllFolders, false)
            .await
            .expect("permission");
        f.pref(PreferenceKind::EnabledFolders, &[&allowed.id]).await;
        for item in [&seen, &unseen, &folder, &hidden] {
            let id = row_id(item);
            let found = f.service.visible_item(id, &f.user).await.expect("lookup");
            assert_eq!(found.is_some(), f.visible(item).await, "{:?}", item.path);
            if let Some(found) = found {
                let stored = f.service.items.retrieve_item(id).await.expect("read");
                assert_eq!(Some(found), stored, "the stored row, as read");
            }
        }
        assert!(f.visible(&seen).await && !f.visible(&unseen).await);
        let missing = f
            .service
            .visible_item(Uuid::from_u128(0x9999), &f.user)
            .await;
        assert!(missing.expect("lookup").is_none());
        let placeholder = Uuid::parse_str(ferrofin_db::PLACEHOLDER_ITEM_ID).expect("id");
        assert!(
            f.service
                .visible_item(placeholder, &f.user)
                .await
                .expect("lookup")
                .is_none()
        );
    }

    /// `visible_item_with_chain` answers as `visible_item` does and hands back the rows its
    /// one read returned, each as `retrieve_item` reads it: a season's series among them, the
    /// placeholder row never (gl-982).
    #[tokio::test]
    async fn visible_item_with_chain_hands_back_the_stored_chain() {
        let f = Fixture::new().await;
        let library = f
            .item(
                40,
                BaseItemKind::CollectionFolder,
                Some("/views/shows"),
                None,
            )
            .await;
        let series = f
            .item(
                41,
                BaseItemKind::Series,
                Some("/media/show"),
                Some(&library),
            )
            .await;
        let mut season = f
            .item(
                42,
                BaseItemKind::Season,
                Some("/media/show/s1"),
                Some(&series),
            )
            .await;
        season.series_id = Some(series.id.clone());
        season.channel_id = Some(ferrofin_db::PLACEHOLDER_ITEM_ID.to_owned());
        f.save(&season).await;
        let (item, chain) = f
            .service
            .visible_item_with_chain(row_id(&season), &f.user)
            .await
            .expect("lookup")
            .expect("visible");
        assert_eq!(
            Some(&item),
            f.service
                .visible_item(row_id(&season), &f.user)
                .await
                .expect("lookup")
                .as_ref()
        );
        let walked = repository::reachable(&f.db, &[row_id(&season)])
            .await
            .expect("walk");
        assert!(
            walked.iter().any(|row| row_id(row) == Uuid::from_u128(1)),
            "the walk reaches the placeholder through ChannelId"
        );
        let mut ids: Vec<Uuid> = chain.iter().map(row_id).collect();
        ids.sort();
        assert_eq!(ids, [row_id(&library), row_id(&series), row_id(&season)]);
        for row in &chain {
            let stored = f.service.items.retrieve_item(row_id(row)).await;
            assert_eq!(Some(row), stored.expect("read").as_ref(), "as stored");
        }
        set_permission(&f.db, &f.user.id, PermissionKind::EnableAllFolders, false)
            .await
            .expect("permission");
        assert!(
            f.service
                .visible_item_with_chain(row_id(&season), &f.user)
                .await
                .expect("lookup")
                .is_none(),
            "hidden"
        );
    }

    /// With the server's caches on, any number of evaluations read the library set once, and
    /// read it again only after a library's own row is written (gl-973).
    #[tokio::test]
    async fn the_library_set_is_read_once_until_a_library_row_is_written() {
        let f = Fixture::new().await;
        let library = f
            .item(20, BaseItemKind::CollectionFolder, Some("/views/one"), None)
            .await;
        let movie = f
            .item(
                21,
                BaseItemKind::Movie,
                Some("/media/one.mkv"),
                Some(&library),
            )
            .await;
        let before = f.service.library_loads();
        for _ in 0..3 {
            assert!(f.visible(&movie).await);
        }
        assert_eq!(f.service.library_loads(), before + 1);
        f.save(&movie).await;
        f.visible(&movie).await;
        assert_eq!(
            f.service.library_loads(),
            before + 1,
            "another row: the set is kept"
        );
        f.save(&library).await;
        f.visible(&movie).await;
        assert_eq!(
            f.service.library_loads(),
            before + 2,
            "the library's row: read again"
        );
    }

    /// C# `Person.IsVisible(user)` is `base.IsVisible(user, skipAllowedTagsCheck:
    /// true)`, the parental check alone: a blocked tag on the person hides it,
    /// and an allowed-tags policy, which an untagged person cannot meet, is not
    /// applied. The full check applies both.
    #[tokio::test]
    async fn a_person_is_parentally_allowed_without_the_allowed_tags_check() {
        let f = Fixture::new().await;
        let mut hidden = f.item(0x9101, BaseItemKind::Person, None, None).await;
        hidden.tags = Some("Hidden".into());
        f.save(&hidden).await;
        let shown = f.item(0x9102, BaseItemKind::Person, None, None).await;
        f.pref(PreferenceKind::BlockedTags, &["hidden"]).await;
        f.pref(PreferenceKind::AllowedTags, &["Kids"]).await;
        let rows = [hidden, shown];
        assert_eq!(
            f.service
                .parentally_allowed(&rows, &f.user, true)
                .await
                .unwrap(),
            [false, true]
        );
        assert_eq!(
            f.service
                .parentally_allowed(&rows, &f.user, false)
                .await
                .unwrap(),
            [false, false]
        );
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one fixture contrasts linked-child ownership with visible-library membership"
    )]
    async fn linked_folder_children_use_owner_and_visible_library_membership() {
        use ferrofin_traits::persistence::FolderChild;
        let f = Fixture::new().await;
        let allowed = f
            .item(
                0x501,
                BaseItemKind::CollectionFolder,
                Some("/views/allowed"),
                None,
            )
            .await;
        let hidden = f
            .item(
                0x502,
                BaseItemKind::CollectionFolder,
                Some("/views/hidden"),
                None,
            )
            .await;
        let allowed_movie = f
            .item(
                0x503,
                BaseItemKind::Movie,
                Some("/media/allowed.mkv"),
                Some(&allowed),
            )
            .await;
        let hidden_movie = f
            .item(
                0x504,
                BaseItemKind::Movie,
                Some("/media/hidden.mkv"),
                Some(&hidden),
            )
            .await;
        let unmapped = f
            .item(0x505, BaseItemKind::Movie, Some("/outside/movie.mkv"), None)
            .await;
        let mut owned = f.item(0x506, BaseItemKind::Video, None, None).await;
        owned.owner_id = Some(hidden_movie.id.clone());
        f.save(&owned).await;
        let virtual_item = f.item(0x507, BaseItemKind::Movie, None, None).await;
        let by_name = f
            .item(0x508, BaseItemKind::Person, Some("/metadata/person"), None)
            .await;
        f.pref(PreferenceKind::BlockedMediaFolders, &[&hidden.id])
            .await;
        let rows = [
            allowed_movie.clone(),
            hidden_movie.clone(),
            unmapped,
            owned,
            virtual_item.clone(),
            by_name.clone(),
        ];
        let make = || {
            rows.iter()
                .cloned()
                .map(|item| FolderChild { item, linked: true })
                .collect()
        };
        let visible = f
            .service
            .folder_children(make(), &f.user, true, std::slice::from_ref(&allowed))
            .await
            .unwrap();
        assert_eq!(
            visible
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            [
                allowed_movie.id.as_str(),
                virtual_item.id.as_str(),
                by_name.id.as_str()
            ]
        );
        // Ordinary folders do not filter their linked children by owning
        // library. Physical children also retain this plain IsVisible rule.
        let visible = f
            .service
            .folder_children(make(), &f.user, false, &[])
            .await
            .unwrap();
        assert_eq!(visible.len(), rows.len());
        let physical = vec![FolderChild {
            item: hidden_movie.clone(),
            linked: false,
        }];
        assert_eq!(
            f.service
                .folder_children(physical, &f.user, true, &[])
                .await
                .unwrap()
                .len(),
            1
        );
        let mut blocked = allowed_movie;
        blocked.tags = Some("blocked".into());
        f.save(&blocked).await;
        f.pref(PreferenceKind::BlockedTags, &["blocked"]).await;
        let visible = f
            .service
            .folder_children(
                vec![FolderChild {
                    item: blocked,
                    linked: true,
                }],
                &f.user,
                true,
                std::slice::from_ref(&allowed),
            )
            .await
            .unwrap();
        assert!(visible.is_empty(), "{visible:?}");
    }

    #[tokio::test]
    async fn hidden_libraries_owners_and_policy_changes() {
        let f = Fixture::new().await;
        let allowed = f
            .item(
                10,
                BaseItemKind::CollectionFolder,
                Some("/views/allowed"),
                None,
            )
            .await;
        let hidden = f
            .item(
                11,
                BaseItemKind::CollectionFolder,
                Some("/views/hidden"),
                None,
            )
            .await;
        let movie = f
            .item(
                12,
                BaseItemKind::Movie,
                Some("/media/movie.mkv"),
                Some(&hidden),
            )
            .await;
        let mut extra = f
            .item(13, BaseItemKind::Video, Some("/media/extra.mkv"), None)
            .await;
        extra.owner_id = Some(movie.id.clone());
        f.save(&extra).await;
        assert!(f.visible(&movie).await);
        set_permission(&f.db, &f.user.id, PermissionKind::EnableAllFolders, false)
            .await
            .expect("permission");
        f.pref(PreferenceKind::EnabledFolders, &[&allowed.id]).await;
        assert!(!f.visible(&movie).await);
        assert!(!f.visible(&extra).await);
        // `GetPreferenceValues<Guid>` parses as .NET does: a padded or
        // parenthesized id counts, a `urn:uuid:` one does not.
        let hidden_id = Uuid::parse_str(&hidden.id).expect("id").hyphenated();
        let urn = format!("urn:uuid:{hidden_id}");
        f.pref(PreferenceKind::EnabledFolders, &[&urn]).await;
        assert!(!f.visible(&movie).await);
        let parenthesized = format!(" ({hidden_id}) ");
        f.pref(PreferenceKind::EnabledFolders, &[&parenthesized])
            .await;
        assert!(f.visible(&movie).await);
        // A blocked list whose values all fail to parse counts as empty, so
        // the allow-list still decides.
        f.pref(PreferenceKind::BlockedMediaFolders, &[&urn]).await;
        assert!(f.visible(&movie).await);
        f.pref(PreferenceKind::BlockedMediaFolders, &[]).await;
        f.pref(PreferenceKind::EnabledFolders, &[&allowed.id]).await;
        // Blocked folders replace the allow-list, even when EnableAllFolders is false.
        f.pref(PreferenceKind::BlockedMediaFolders, &[&allowed.id])
            .await;
        assert!(f.visible(&movie).await);
        f.pref(PreferenceKind::BlockedMediaFolders, &[&hidden.id])
            .await;
        set_permission(&f.db, &f.user.id, PermissionKind::IsAdministrator, true)
            .await
            .expect("admin");
        assert!(!f.visible(&movie).await);
        let root = f
            .item(14, BaseItemKind::UserRootFolder, None, Some(&hidden))
            .await;
        assert!(f.visible(&root).await);
    }

    #[tokio::test]
    async fn adopted_physical_folder_can_belong_to_two_libraries() {
        let f = Fixture::new().await;
        let root = f
            .item(20, BaseItemKind::AggregateFolder, Some("/root"), None)
            .await;
        let physical = f
            .item(21, BaseItemKind::Folder, Some("/media/shared"), Some(&root))
            .await;
        let mut a = f
            .item(22, BaseItemKind::CollectionFolder, Some("/views/a"), None)
            .await;
        a.data = Some(serde_json::json!({"PhysicalLocationsList":["/MEDIA/SHARED"]}).to_string());
        f.save(&a).await;
        let mut b = f
            .item(23, BaseItemKind::CollectionFolder, Some("/views/b"), None)
            .await;
        b.data = a.data.clone();
        f.save(&b).await;
        let movie = f
            .item(
                24,
                BaseItemKind::Movie,
                Some("/media/shared/a.mkv"),
                Some(&physical),
            )
            .await;
        f.pref(PreferenceKind::BlockedMediaFolders, &[&a.id]).await;
        assert!(f.visible(&movie).await);
        f.pref(PreferenceKind::BlockedMediaFolders, &[&a.id, &b.id])
            .await;
        assert!(!f.visible(&movie).await);
        // Items outside all collections are not denied solely for lacking membership.
        let orphan = f
            .item(25, BaseItemKind::Movie, Some("/outside/a.mkv"), None)
            .await;
        assert!(f.visible(&orphan).await);
    }

    #[tokio::test]
    async fn inherited_tags_and_parent_rating_are_checked_independently() {
        let mut f = Fixture::new().await;
        let library = f
            .item(30, BaseItemKind::CollectionFolder, Some("/library"), None)
            .await;
        let mut parent = f
            .item(
                31,
                BaseItemKind::Folder,
                Some("/library/parent"),
                Some(&library),
            )
            .await;
        let mut item = f
            .item(
                32,
                BaseItemKind::Movie,
                Some("/library/parent/item"),
                Some(&parent),
            )
            .await;
        item.tags = Some("Café".to_owned());
        f.save(&item).await;
        f.pref(PreferenceKind::AllowedTags, &["cafe"]).await;
        assert!(
            f.visible(&item).await,
            "parents skip the allowed-tag requirement"
        );
        parent.tags = Some("Private".to_owned());
        f.save(&parent).await;
        f.pref(PreferenceKind::BlockedTags, &["PRIVATE"]).await;
        assert!(
            !f.visible(&item).await,
            "a parent's blocked tag overrides an allowed child tag"
        );
        f.pref(PreferenceKind::BlockedTags, &[]).await;
        parent.official_rating = Some("R".to_owned());
        f.save(&parent).await;
        item.official_rating = Some("G".to_owned());
        f.save(&item).await;
        f.user.max_parental_rating_score = Some(13);
        assert!(
            !f.visible(&item).await,
            "a child's lower rating does not authorize its parent"
        );
        parent.official_rating = Some("PG".to_owned());
        f.save(&parent).await;
        item.official_rating = Some("R".to_owned());
        item.custom_rating = Some("G".to_owned());
        f.save(&item).await;
        assert!(f.visible(&item).await, "custom rating takes precedence");
    }

    #[rstest::rstest]
    #[case(BaseItemKind::Movie, "Movie", false)]
    #[case(BaseItemKind::Series, "Series", false)]
    #[case(BaseItemKind::Season, "Series", true)]
    #[case(BaseItemKind::MusicAlbum, "Music", false)]
    #[case(BaseItemKind::MusicArtist, "Music", true)]
    #[case(BaseItemKind::Folder, "Other", true)]
    #[case(BaseItemKind::Person, "Other", true)]
    #[case(BaseItemKind::Audio, "Music", false)]
    #[case(BaseItemKind::AudioBook, "Book", false)]
    #[case(BaseItemKind::LiveTvChannel, "LiveTvChannel", false)]
    #[tokio::test]
    async fn unrated_kind_overrides(
        #[case] kind: BaseItemKind,
        #[case] category: &str,
        #[case] visible: bool,
    ) {
        let f = Fixture::new().await;
        let item = f.item(40, kind, Some("/media/item"), None).await;
        f.pref(PreferenceKind::BlockUnratedItems, &[category]).await;
        assert_eq!(f.visible(&item).await, visible);
    }

    #[tokio::test]
    async fn private_shared_and_file_playlists() {
        let f = Fixture::new().await;
        let mut playlist = f
            .item(
                50,
                BaseItemKind::Playlist,
                Some("/server/data/playlists/private"),
                None,
            )
            .await;
        playlist.data = Some(
            serde_json::json!({"OwnerUserId":Uuid::from_u128(999),"OpenAccess":false}).to_string(),
        );
        f.save(&playlist).await;
        assert!(!f.visible(&playlist).await);
        playlist.data = Some(
            serde_json::json!({"Shares":[{"UserId":f.user.id}],"OpenAccess":false}).to_string(),
        );
        f.save(&playlist).await;
        assert!(f.visible(&playlist).await);
        playlist.data = Some(serde_json::json!({"OpenAccess":true}).to_string());
        f.save(&playlist).await;
        f.pref(PreferenceKind::AllowedTags, &["unmatched"]).await;
        assert!(
            f.visible(&playlist).await,
            "shared playlist override bypasses base parental checks"
        );
        playlist.path = Some("/server/data2/file.m3u".to_owned());
        f.save(&playlist).await;
        assert!(
            !f.visible(&playlist).await,
            "file playlists use base visibility"
        );
    }

    #[tokio::test]
    async fn metadata_scores_follow_parent_country_custom_rating_and_clear_unknowns() {
        let f = Fixture::new().await;
        let mut parent = f.item(56, BaseItemKind::Series, None, None).await;
        parent.official_rating = Some("PG".into());
        parent.preferred_metadata_country_code = Some("CA".into());
        f.save(&parent).await;
        let mut child = f.item(57, BaseItemKind::Movie, None, Some(&parent)).await;
        f.service
            .update_rating_scores(std::slice::from_mut(&mut child))
            .await
            .unwrap();
        assert_eq!(child.inherited_parental_rating_value, Some(8));
        assert_eq!(child.inherited_parental_rating_sub_value, Some(1));
        child.official_rating = Some("FSK-18".into());
        parent.custom_rating = Some("12".into());
        f.save(&parent).await;
        f.service
            .update_rating_scores(std::slice::from_mut(&mut child))
            .await
            .unwrap();
        assert_eq!(child.inherited_parental_rating_value, Some(12));
        child.custom_rating = Some("Unrated".into());
        f.service
            .update_rating_scores(std::slice::from_mut(&mut child))
            .await
            .unwrap();
        assert_eq!(child.inherited_parental_rating_value, None);
        assert_eq!(child.inherited_parental_rating_sub_value, None);
    }

    #[tokio::test]
    async fn channel_content_and_cycles_fail_closed() {
        let f = Fixture::new().await;
        let channel = f.item(60, BaseItemKind::Channel, None, None).await;
        let mut movie = f
            .item(
                61,
                BaseItemKind::Movie,
                Some("https://example.invalid/movie"),
                None,
            )
            .await;
        movie.channel_id = Some(channel.id.clone());
        f.save(&movie).await;
        f.pref(PreferenceKind::BlockedChannels, &[&channel.id])
            .await;
        assert!(!f.visible(&movie).await);
        f.pref(PreferenceKind::BlockedChannels, &[]).await;
        assert!(f.visible(&movie).await);
        movie.parent_id = Some(movie.id.clone());
        f.save(&movie).await;
        assert!(f.service.visible(&[movie], &f.user, true).await.is_err());
    }

    #[tokio::test]
    async fn channel_selection_and_legacy_block_precedence_apply_to_admins_too() {
        let f = Fixture::new().await;
        let channel = f.item(62, BaseItemKind::Channel, None, None).await;
        let other = Uuid::from_u128(64).to_string();
        let mut movie = f.item(63, BaseItemKind::Movie, None, None).await;
        movie.channel_id = Some(channel.id.clone());
        f.save(&movie).await;
        for admin in [false, true] {
            set_permission(&f.db, &f.user.id, PermissionKind::IsAdministrator, admin)
                .await
                .expect("admin");
            for (all, selected, blocked, expected) in [
                (true, false, None, true),
                (false, false, None, false),
                (false, true, None, true),
                (true, true, Some(&channel.id), false),
                (false, false, Some(&other), true),
            ] {
                set_permission(&f.db, &f.user.id, PermissionKind::EnableAllChannels, all)
                    .await
                    .expect("channels");
                let enabled = if selected {
                    vec![channel.id.as_str()]
                } else {
                    vec![]
                };
                f.pref(PreferenceKind::EnabledChannels, &enabled).await;
                f.pref(
                    PreferenceKind::BlockedChannels,
                    &blocked.map(String::as_str).into_iter().collect::<Vec<_>>(),
                )
                .await;
                assert_eq!(
                    f.visible(&channel).await,
                    expected,
                    "channel: admin={admin}, all={all}"
                );
                assert_eq!(
                    f.visible(&movie).await,
                    expected,
                    "content: admin={admin}, all={all}"
                );
            }
        }
    }

    #[tokio::test]
    async fn live_tv_program_channel_id_is_not_a_plugin_channel_reference() {
        let f = Fixture::new().await;
        let mut program = f.item(65, BaseItemKind::LiveTvProgram, None, None).await;
        program.channel_id = Some(Uuid::from_u128(66).to_string());
        program.tags = Some("safe".into());
        f.save(&program).await;
        f.pref(PreferenceKind::AllowedTags, &["safe"]).await;
        f.pref(
            PreferenceKind::BlockedChannels,
            &[program.channel_id.as_deref().unwrap()],
        )
        .await;
        assert!(f.visible(&program).await);
        f.pref(PreferenceKind::BlockUnratedItems, &["LiveTvProgram"])
            .await;
        assert!(!f.visible(&program).await);
    }

    #[tokio::test]
    async fn disabled_library_and_parent_vs_standalone() {
        let f = Fixture::new().await;
        let library = f
            .item(70, BaseItemKind::CollectionFolder, Some("/library"), None)
            .await;
        let item = f
            .item(
                71,
                BaseItemKind::Movie,
                Some("/library/item"),
                Some(&library),
            )
            .await;
        let mut libraries = (*f.service.rating_libraries().await.expect("libraries").set).clone();
        libraries.options.insert(
            row_id(&library),
            ferrofin_model::configuration::LibraryOptions {
                enabled: false,
                ..Default::default()
            },
        );
        let context = f
            .service
            .load(std::slice::from_ref(&item), Some(&f.user), &libraries)
            .await
            .expect("context");
        assert!(context.visible(&item, false).expect("own policy"));
        assert!(!context.standalone(&item).expect("parent policy"));
    }

    #[tokio::test]
    async fn boxset_checks_linked_libraries_and_children() {
        let mut f = Fixture::new().await;
        let library = f
            .item(80, BaseItemKind::CollectionFolder, Some("/library"), None)
            .await;
        let mut movie = f
            .item(
                81,
                BaseItemKind::Movie,
                Some("/library/movie"),
                Some(&library),
            )
            .await;
        movie.official_rating = Some("R".to_owned());
        f.save(&movie).await;
        let boxset = f.item(82, BaseItemKind::BoxSet, None, None).await;
        let nested = f.item(83, BaseItemKind::BoxSet, None, None).await;
        for (parent, child) in [(&boxset, &nested), (&nested, &movie)] {
            crate::FerrofinLinkedChildrenService::new(f.db.clone())
                .upsert_linked_child(row_id(parent), row_id(child), 0)
                .await
                .expect("link");
        }
        f.pref(PreferenceKind::BlockedMediaFolders, &[&library.id])
            .await;
        assert!(!f.visible(&boxset).await);
        f.pref(PreferenceKind::BlockedMediaFolders, &[]).await;
        f.user.max_parental_rating_score = Some(13);
        assert!(
            !f.visible(&nested).await,
            "all linked children exceed the limit"
        );
        f.user.max_parental_rating_score = None;
        assert!(f.visible(&nested).await);
    }

    /// `Playlist.HasParentalAllowedChild` gates every arm of `Playlist.IsVisible`
    /// for a rating-limited user: the owner's own playlist hides when every
    /// member exceeds the limit, an empty one stays visible, and a file
    /// playlist outside the data directory follows the same rule.
    #[tokio::test]
    async fn playlists_need_a_parentally_allowed_member() {
        let mut f = Fixture::new().await;
        let library = f
            .item(90, BaseItemKind::CollectionFolder, Some("/library"), None)
            .await;
        let mut members = Vec::new();
        for (id, rating) in [(91, "R"), (92, "G")] {
            let mut movie = f
                .item(
                    id,
                    BaseItemKind::Movie,
                    Some(&format!("/library/{id}")),
                    Some(&library),
                )
                .await;
            movie.official_rating = Some(rating.to_owned());
            f.save(&movie).await;
            members.push(movie);
        }
        let mut owned = f
            .item(
                93,
                BaseItemKind::Playlist,
                Some("/server/data/playlists/owned"),
                None,
            )
            .await;
        owned.data = Some(serde_json::json!({"OwnerUserId": f.user.id}).to_string());
        f.save(&owned).await;
        let file = f
            .item(
                94,
                BaseItemKind::Playlist,
                Some("/library/list.m3u"),
                Some(&library),
            )
            .await;
        f.user.max_parental_rating_score = Some(13);
        assert!(f.visible(&owned).await, "an empty playlist stays visible");
        for playlist in [&owned, &file] {
            crate::FerrofinLinkedChildrenService::new(f.db.clone())
                .upsert_linked_child(row_id(playlist), row_id(&members[0]), 0)
                .await
                .expect("link");
        }
        assert!(!f.visible(&owned).await, "every member exceeds the limit");
        assert!(!f.visible(&file).await, "file playlists apply it too");
        f.user.max_parental_rating_score = None;
        assert!(f.visible(&owned).await && f.visible(&file).await);
        f.user.max_parental_rating_score = Some(13);
        crate::FerrofinLinkedChildrenService::new(f.db.clone())
            .upsert_linked_child(row_id(&owned), row_id(&members[1]), 1)
            .await
            .expect("link");
        assert!(f.visible(&owned).await, "one allowed member is enough");
    }

    /// `playlist_policy` is `Playlist.IsVisible` minus the owner/share test,
    /// which the playlist manager answers itself: a shared playlist owned by
    /// someone else passes it, and only `HasParentalAllowedChild` can reject
    /// it; a playlist file also answers to `base.IsVisible`, its tags here.
    #[tokio::test]
    async fn playlist_policy_leaves_the_owner_check_to_the_manager() {
        let mut f = Fixture::new().await;
        let library = f
            .item(95, BaseItemKind::CollectionFolder, Some("/library"), None)
            .await;
        let mut movie = f
            .item(96, BaseItemKind::Movie, Some("/library/96"), Some(&library))
            .await;
        movie.official_rating = Some("R".to_owned());
        f.save(&movie).await;
        let mut shared = f
            .item(
                97,
                BaseItemKind::Playlist,
                Some("/server/data/playlists/theirs"),
                None,
            )
            .await;
        shared.data = Some(
            serde_json::json!({"OwnerUserId": Uuid::from_u128(999), "OpenAccess": false})
                .to_string(),
        );
        f.save(&shared).await;
        let mut file = f
            .item(
                98,
                BaseItemKind::Playlist,
                Some("/library/list.m3u"),
                Some(&library),
            )
            .await;
        assert_eq!(
            f.service
                .playlist_policy(&[shared.clone(), file.clone()], &f.user)
                .await
                .expect("policy"),
            [true, true],
            "another user's private playlist: the owner check is the manager's"
        );
        for playlist in [&shared, &file] {
            crate::FerrofinLinkedChildrenService::new(f.db.clone())
                .upsert_linked_child(row_id(playlist), row_id(&movie), 0)
                .await
                .expect("link");
        }
        f.user.max_parental_rating_score = Some(13);
        assert_eq!(
            f.service
                .playlist_policy(&[shared.clone(), file.clone()], &f.user)
                .await
                .expect("policy"),
            [false, false],
            "only an R member"
        );
        f.user.max_parental_rating_score = None;
        file.tags = Some("Private".to_owned());
        f.save(&file).await;
        f.pref(PreferenceKind::BlockedTags, &["private"]).await;
        assert_eq!(
            f.service
                .playlist_policy(&[file, shared], &f.user)
                .await
                .expect("policy"),
            [false, true],
            "a blocked tag hides the file, not the shared playlist"
        );
    }

    #[tokio::test]
    async fn adopted_playlist_tokens_and_rating_subscores() {
        let mut f = Fixture::new().await;
        let paths = Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
            "/server",
            "/server/log",
            "/server/config",
            "/server/cache",
            "/server/web",
        ));
        f.service.paths = crate::virtual_paths::VirtualPathExpander::from_paths(paths);
        let mut playlist = f
            .item(
                85,
                BaseItemKind::Playlist,
                Some("%AppDataPath%/playlists/private"),
                None,
            )
            .await;
        playlist.data = Some(
            serde_json::json!({"OpenAccess":false,"OwnerUserId":Uuid::from_u128(999)}).to_string(),
        );
        f.save(&playlist).await;
        assert!(!f.visible(&playlist).await);
        let mut movie = f.item(86, BaseItemKind::Movie, None, None).await;
        movie.official_rating = Some("PG-13".to_owned());
        f.user.max_parental_rating_score = Some(13);
        f.user.max_parental_rating_sub_score = Some(-1);
        assert!(!f.visible(&movie).await);
        f.user.max_parental_rating_sub_score = None;
        assert!(f.visible(&movie).await);
    }

    /// A layout under a server rooted at `/server` whose library row and root row store
    /// `%AppDataPath%` paths, the token Jellyfin writes for a row under its data directory (a box
    /// set's or a playlist's, in a real database), to give every path reader a token to expand:
    /// the root, the user root, the library `shared` (stored `%AppDataPath%/media/shared`), the
    /// library `other`, the physical folder `/server/data/media/shared` under the root, and a
    /// movie in it.
    async fn virtual_path_layout(f: &mut Fixture) -> [BaseItemEntity; 6] {
        let paths = Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
            "/server",
            "/server/log",
            "/server/config",
            "/server/cache",
            "/server/web",
        ));
        f.service.paths = crate::virtual_paths::VirtualPathExpander::from_paths(paths);
        let root = f
            .item(
                0xa0,
                BaseItemKind::AggregateFolder,
                Some("%AppDataPath%/root"),
                None,
            )
            .await;
        let user_root = f
            .item(
                0xa1,
                BaseItemKind::UserRootFolder,
                Some("/server/root/default"),
                Some(&root),
            )
            .await;
        let shared = f
            .item(
                0xa2,
                BaseItemKind::CollectionFolder,
                Some("%AppDataPath%/media/shared"),
                Some(&user_root),
            )
            .await;
        let other = f
            .item(
                0xa3,
                BaseItemKind::CollectionFolder,
                Some("/views/other"),
                Some(&user_root),
            )
            .await;
        let physical = f
            .item(
                0xa4,
                BaseItemKind::Folder,
                Some("/server/data/media/shared"),
                Some(&root),
            )
            .await;
        let movie = f
            .item(
                0xa5,
                BaseItemKind::Movie,
                Some("/server/data/media/shared/a.mkv"),
                Some(&physical),
            )
            .await;
        [root, user_root, shared, other, physical, movie]
    }

    /// A library row served from the cached library set, whose stored path holds
    /// `%AppDataPath%`, still decides membership on its expanded path: blocking it hides the
    /// movie, blocking the other library does not, and a linked child counts as in it
    /// (gl-1001).
    #[tokio::test]
    async fn a_cached_library_with_a_virtual_path_still_decides_membership() {
        /// How many of a box set's children `folder_children` keeps: `movie`, linked, with
        /// `root` as the user's one root folder.
        async fn linked(f: &Fixture, movie: &BaseItemEntity, root: &BaseItemEntity) -> usize {
            let child = FolderChild {
                item: movie.clone(),
                linked: true,
            };
            f.service
                .folder_children(vec![child], &f.user, true, std::slice::from_ref(root))
                .await
                .expect("children")
                .len()
        }
        let mut f = Fixture::new().await;
        let [root, _, shared, other, _, movie] = virtual_path_layout(&mut f).await;
        assert!(f.visible(&movie).await);
        // The rows handed back are the stored ones: the expansion stays inside the check.
        let (item, chain) = f
            .service
            .visible_item_with_chain(row_id(&shared), &f.user)
            .await
            .expect("lookup")
            .expect("visible");
        assert_eq!(item.path, shared.path, "the library as stored");
        let above = chain.iter().find(|row| row.id == root.id).expect("root");
        assert_eq!(above.path, root.path, "the root as stored");
        assert_eq!(linked(&f, &movie, &shared).await, 1, "in a root");
        assert_eq!(linked(&f, &movie, &other).await, 0, "in no root");
        f.pref(PreferenceKind::BlockedMediaFolders, &[&other.id])
            .await;
        assert!(f.visible(&movie).await, "another library blocked");
        f.pref(PreferenceKind::BlockedMediaFolders, &[&shared.id])
            .await;
        assert!(!f.visible(&movie).await, "its library blocked");
        let found = f.service.visible_item(row_id(&movie), &f.user).await;
        assert!(found.expect("lookup").is_none());
    }

    /// The context holds the caller's rows and the cached library set's rows themselves, not
    /// copies; a cached row whose stored path holds a token is the one copied, its path expanded,
    /// and the cached set keeps the stored path (gl-1001).
    #[tokio::test]
    async fn the_context_borrows_caller_and_cached_rows() {
        let mut f = Fixture::new().await;
        let [root, user_root, shared, other, _, movie] = virtual_path_layout(&mut f).await;
        let libraries = f.service.rating_libraries().await.expect("libraries");
        let items = [movie];
        let context = f
            .service
            .load(&items, Some(&f.user), &libraries.set)
            .await
            .expect("context");
        let held = |row: &BaseItemEntity| -> &BaseItemEntity { &context.rows[&row_id(row)] };
        let cached = |row: &BaseItemEntity| {
            context
                .libraries
                .rows
                .iter()
                .chain(&context.libraries.ancestors)
                .find(|cached| cached.id == row.id)
                .expect("cached")
        };
        let caller = &items[0];
        assert!(std::ptr::eq(held(caller), caller), "the caller's row");
        assert!(
            std::ptr::eq(held(&other), cached(&other)),
            "a cached library"
        );
        assert!(
            std::ptr::eq(held(&user_root), cached(&user_root)),
            "a cached ancestor"
        );
        for (row, expanded) in [
            (&shared, "/server/data/media/shared"),
            (&root, "/server/data/root"),
        ] {
            assert!(!std::ptr::eq(held(row), cached(row)), "{expanded}: a copy");
            assert_eq!(held(row).path.as_deref(), Some(expanded));
            assert_eq!(cached(row).path, row.path, "the stored path stays cached");
        }
        assert_eq!(
            context.libraries(&items[0]).expect("libraries"),
            [row_id(&shared)]
        );
    }

    #[tokio::test]
    async fn reading_adopted_library_options_does_not_write() {
        let f = Fixture::new().await;
        let dir = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(
            dir.path().join("options.xml"),
            "<LibraryOptions><Enabled>false</Enabled></LibraryOptions>",
        )
        .await
        .expect("xml");
        let folder = f
            .item(
                87,
                BaseItemKind::CollectionFolder,
                dir.path().to_str(),
                None,
            )
            .await;
        assert!(!f.visible(&folder).await);
        assert!(!dir.path().join("options.json").exists());
    }

    #[tokio::test]
    async fn policy_storage_errors_are_not_invisibility() {
        let f = Fixture::new().await;
        let item = f.item(90, BaseItemKind::Movie, None, None).await;
        f.db.pool().close().await;
        assert!(f.service.visible(&[item], &f.user, true).await.is_err());
    }
}
