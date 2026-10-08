//! Request-local evaluation of Jellyfin's `IsVisible` and `IsVisibleStandalone`.
//!
//! Raw item reads remain user-less. This service loads policy and the reachable
//! hierarchy once for a batch, without applying browse-query filters to IDs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::{PermissionKind, PreferenceKind};
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::localization::LocalizationManager;
use ferrofin_traits::persistence::ItemRepository;
use serde_json::Value;
use uuid::Uuid;

use crate::item_type_lookup::kind_from_type_name;
use crate::item_visibility_repository as repository;
use crate::text_util::get_clean_value;

/// The collaborators needed to evaluate item access. Results are never cached
/// across requests, so library, sharing and policy changes apply immediately.
pub struct ItemVisibility {
    db: Database,
    items: Arc<dyn ItemRepository>,
    localization: Arc<dyn LocalizationManager>,
    data_path: String,
    paths: crate::virtual_paths::VirtualPathExpander,
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
        }
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
        let context = self.load(items, Some(user)).await?;
        items
            .iter()
            .map(|item| {
                let item = context.rows.get(&row_id(item)).unwrap_or(item);
                if standalone {
                    context.standalone(item)
                } else {
                    context.visible(item, false)
                }
            })
            .collect()
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
        if items.is_empty() {
            return Ok(());
        }
        let context = self.load(items, None).await?;
        for item in items {
            let row = context.rows.get(&row_id(item)).unwrap_or(item);
            let score = context.rating_score(row)?;
            item.inherited_parental_rating_value = score.map(|score| i64::from(score.score));
            item.inherited_parental_rating_sub_value =
                score.and_then(|score| score.sub_score.map(i64::from));
        }
        Ok(())
    }

    async fn load_hierarchy(
        &self,
        rows: &mut HashMap<Uuid, BaseItemEntity>,
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
            let boxsets: Vec<_> = fresh
                .iter()
                .filter(|id| kind(&rows[id]) == BaseItemKind::BoxSet)
                .copied()
                .collect();
            for (parent, child) in repository::linked_children(&self.db, &boxsets).await? {
                if let (Some(parent), Some(child)) = (uuid(Some(&parent)), uuid(Some(&child))) {
                    links.entry(parent).or_default().push(child);
                }
            }
            let mut wanted = HashSet::new();
            for id in fresh {
                examined.insert(id);
                let row = &rows[&id];
                for next in [
                    row.parent_id.as_deref(),
                    row.owner_id.as_deref(),
                    row.channel_id.as_deref(),
                    row.season_id.as_deref(),
                    row.series_id.as_deref(),
                ]
                .into_iter()
                .filter_map(uuid)
                .chain(links.get(&id).into_iter().flatten().copied())
                {
                    if requested.insert(next) {
                        wanted.insert(next);
                    }
                }
            }
            if wanted.is_empty() {
                break;
            }
            for row in self
                .items
                .retrieve_items(&wanted.into_iter().collect::<Vec<_>>())
                .await?
            {
                rows.insert(row_id(&row), row);
            }
        }
        Ok(links)
    }

    async fn load<'a>(
        &'a self,
        items: &[BaseItemEntity],
        user: Option<&'a UserEntity>,
    ) -> Result<Context<'a>, ServiceError> {
        let (permissions, preferences) = if let Some(user) = user {
            (
                repository::permissions(&self.db, &user.id).await?,
                repository::preferences(&self.db, &user.id).await?,
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let libraries = repository::libraries(&self.db).await?;
        let library_ids: Vec<Uuid> = libraries.iter().map(row_id).collect();
        let mut rows: HashMap<Uuid, BaseItemEntity> = libraries
            .into_iter()
            .chain(items.iter().cloned())
            .map(|row| (row_id(&row), row))
            .collect();
        let links = self.load_hierarchy(&mut rows).await?;
        for row in rows.values_mut() {
            row.path = self.paths.expand_opt(row.path.as_deref());
        }
        let mut options = HashMap::new();
        // Read the existing options without GetVirtualFolders, which also heals
        // library rows and would turn an authorization read into a mutation.
        for id in &library_ids {
            if let Some(path) = rows[id].path.as_deref() {
                options.insert(
                    *id,
                    crate::virtual_folder_manager::FerrofinVirtualFolderManager::read_options(
                        std::path::Path::new(path),
                    )
                    .await,
                );
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
            options,
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
    rows: HashMap<Uuid, BaseItemEntity>,
    library_ids: Vec<Uuid>,
    links: HashMap<Uuid, Vec<Uuid>>,
    options: HashMap<Uuid, ferrofin_model::configuration::LibraryOptions>,
    playlists: HashMap<Uuid, bool>,
    permissions: HashMap<i32, bool>,
    preferences: HashMap<i32, Vec<String>>,
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
        let blocked = self.preference(blocked);
        if !blocked.is_empty() {
            return !blocked
                .iter()
                .filter_map(|v| uuid(Some(v)))
                .any(|v| v == id);
        }
        self.permissions
            .get(&i32::from(all))
            .copied()
            .unwrap_or(false)
            || self
                .preference(enabled)
                .iter()
                .filter_map(|v| uuid(Some(v)))
                .any(|v| v == id)
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
            let Some(parent) = uuid(next).and_then(|id| self.rows.get(&id)) else {
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
            let parent = uuid(current.parent_id.as_deref()).and_then(|id| self.rows.get(&id));
            if parent.is_some_and(|r| kind(r) == BaseItemKind::AggregateFolder) {
                break;
            }
            let next = parent
                .or_else(|| uuid(current.owner_id.as_deref()).and_then(|id| self.rows.get(&id)));
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
                    || self.options.get(id).is_some_and(|opts| {
                        opts.path_infos.iter().any(|p| {
                            self.service
                                .paths
                                .expand(&p.path)
                                .eq_ignore_ascii_case(path)
                        })
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
            if let Some(allowed) = self.playlists.get(&id) {
                return Ok(*allowed);
            }
            let value = data(item);
            return Ok(value
                .get("OpenAccess")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || uuid(value.get("OwnerUserId").and_then(Value::as_str)) == uuid(Some(&user.id))
                || value
                    .get("Shares")
                    .and_then(Value::as_array)
                    .is_some_and(|shares| {
                        shares.iter().any(|share| {
                            uuid(share.get("UserId").and_then(Value::as_str))
                                == uuid(Some(&user.id))
                        })
                    }));
        }
        if kind(item) == BaseItemKind::CollectionFolder
            && (!self.folder_allowed(id, false)
                || self.options.get(&id).is_some_and(|o| !o.enabled))
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
                    if let Some(row) = self.rows.get(&child) {
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
                if let Some(row) = self.rows.get(&library) {
                    accessible |= self.visible(row, false)?;
                }
            }
            if !accessible {
                return Ok(false);
            }
            if user.max_parental_rating_score.is_some() {
                let mut any = false;
                let mut present = false;
                for child in children.iter().filter_map(|id| self.rows.get(id)) {
                    present = true;
                    any |= self.parental(child, true)?;
                }
                if present && !any {
                    return Ok(false);
                }
            }
        }
        Ok(true)
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
                .rows
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
                .chain(libraries.iter().filter_map(|id| self.rows.get(id)))
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
        let parents = self.chain(item, false)?;
        let libraries = self.libraries(item)?;
        let display = self.chain(item, true)?;
        let rating = display
            .iter()
            .find_map(|row| nonempty(row.custom_rating.as_deref()))
            .or_else(|| {
                display
                    .iter()
                    .find_map(|row| nonempty(row.official_rating.as_deref()))
            });
        let country = parents
            .iter()
            .find_map(|row| nonempty(row.preferred_metadata_country_code.as_deref()))
            .or_else(|| {
                libraries
                    .iter()
                    .filter_map(|id| self.rows.get(id))
                    .find_map(|row| nonempty(row.preferred_metadata_country_code.as_deref()))
            })
            .or_else(|| {
                libraries
                    .iter()
                    .filter_map(|id| self.options.get(id))
                    .find_map(|opts| nonempty(opts.metadata_country_code.as_deref()))
            });
        Ok(rating.and_then(|r| self.service.localization.get_rating_score(r, country)))
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
        let category = match kind {
            BaseItemKind::Movie | BaseItemKind::BoxSet => "Movie",
            BaseItemKind::Series | BaseItemKind::Episode => "Series",
            BaseItemKind::Trailer => "Trailer",
            BaseItemKind::Book | BaseItemKind::AudioBook => "Book",
            BaseItemKind::MusicVideo | BaseItemKind::MusicAlbum | BaseItemKind::MusicArtist => {
                "Music"
            }
            BaseItemKind::Audio if uuid(item.channel_id.as_deref()).is_none() => "Music",
            BaseItemKind::LiveTvChannel | BaseItemKind::TvChannel => "LiveTvChannel",
            BaseItemKind::LiveTvProgram => "LiveTvProgram",
            _ if uuid(item.channel_id.as_deref()).is_some() || kind == BaseItemKind::Channel => {
                "ChannelContent"
            }
            _ => "Other",
        };
        self.preference(PreferenceKind::BlockUnratedItems)
            .iter()
            .any(|v| v == category)
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
                self.db.pool(),
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
        set_permission(
            f.db.pool(),
            &f.user.id,
            PermissionKind::EnableAllFolders,
            false,
        )
        .await
        .expect("permission");
        f.pref(PreferenceKind::EnabledFolders, &[&allowed.id]).await;
        assert!(!f.visible(&movie).await);
        assert!(!f.visible(&extra).await);
        // Blocked folders replace the allow-list, even when EnableAllFolders is false.
        f.pref(PreferenceKind::BlockedMediaFolders, &[&allowed.id])
            .await;
        assert!(f.visible(&movie).await);
        f.pref(PreferenceKind::BlockedMediaFolders, &[&hidden.id])
            .await;
        set_permission(
            f.db.pool(),
            &f.user.id,
            PermissionKind::IsAdministrator,
            true,
        )
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
            set_permission(
                f.db.pool(),
                &f.user.id,
                PermissionKind::IsAdministrator,
                admin,
            )
            .await
            .expect("admin");
            for (all, selected, blocked, expected) in [
                (true, false, None, true),
                (false, false, None, false),
                (false, true, None, true),
                (true, true, Some(&channel.id), false),
                (false, false, Some(&other), true),
            ] {
                set_permission(
                    f.db.pool(),
                    &f.user.id,
                    PermissionKind::EnableAllChannels,
                    all,
                )
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
        let mut context = f
            .service
            .load(std::slice::from_ref(&item), Some(&f.user))
            .await
            .expect("context");
        context.options.insert(
            row_id(&library),
            ferrofin_model::configuration::LibraryOptions {
                enabled: false,
                ..Default::default()
            },
        );
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
