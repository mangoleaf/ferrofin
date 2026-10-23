//! [`FerrofinSearchManager`] — the concrete [`SearchManager`].
//!
//! Port of `Emby.Server.Implementations.Library.SearchEngine` (10.11.8). The C#
//! engine does **no** ranking of its own: it folds the `include*` category
//! toggles into one `IncludeItemTypes`/`ExcludeItemTypes` pair, hands the term
//! to the repository as `InternalItemsQuery.SearchTerm`, and lets
//! `BaseItemRepository.ApplyOrder` rank by match quality *inside the SQL* — so
//! the query's `LIMIT` keeps the best matches. This port does the same: the
//! category folding lives here, the relevance `ORDER BY` lives in
//! [`translate_query`](crate::translate_query), and paging is applied to the
//! ranked window afterwards exactly as `SearchEngine.GetSearchHints` does.
//!
//! [`SearchManager::get_search_results`] has no 10.11.8 analogue (it is a
//! Ferrofin-side convenience returning ids + a coarse score); it keeps the
//! exact/prefix/substring tiering below.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::SortOrder;
use ferrofin_model::entities::ImageType;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::querying::QueryResult;
use ferrofin_model::search::{SearchHint, SearchQuery};
use ferrofin_traits::drawing::ImageProcessor;
use ferrofin_traits::options::ItemImageInfo;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{SearchManager, SearchResult, UserManager};
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;

use crate::item_type_lookup::kind_from_type_name;

/// Score for an exact, case-insensitive name match.
const SCORE_EXACT: f32 = 3.0;
/// Score for a prefix (starts-with) match.
const SCORE_PREFIX: f32 = 2.0;
/// Score for a substring match anywhere in the name.
const SCORE_SUBSTRING: f32 = 1.0;

/// The concrete search manager.
#[derive(Clone)]
pub struct FerrofinSearchManager {
    items: Arc<dyn ItemRepository>,
    users: Arc<dyn UserManager>,
    images: Option<Arc<dyn ImageProcessor>>,
}

impl std::fmt::Debug for FerrofinSearchManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinSearchManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinSearchManager {
    /// Creates a search manager over the injected item repository and user
    /// manager.
    ///
    /// The user manager is **not** optional: C# `SearchEngine` holds an
    /// `IUserManager` and resolves `query.UserId` into the `User` it hands to
    /// `new InternalItemsQuery(user)`. Without it the search runs unscoped —
    /// no `TopParentIds` restriction and no visibility filtering — which leaks
    /// rows the caller cannot browse.
    #[must_use]
    pub fn new(items: Arc<dyn ItemRepository>, users: Arc<dyn UserManager>) -> Self {
        Self {
            items,
            users,
            images: None,
        }
    }

    /// Attaches the image processor the hints' artwork tags come from
    /// (`SearchController`'s `IImageProcessor`). Without one, a hint carries
    /// no image tags.
    #[must_use]
    pub fn with_image_processor(mut self, images: Arc<dyn ImageProcessor>) -> Self {
        self.images = Some(images);
        self
    }

    /// Translates a [`SearchQuery`] into the item query that backs it.
    ///
    /// Line-for-line port of `SearchEngine.GetSearchHints`'s query building.
    /// Each category toggle is a two-sided rule: enabled *and* compatible with
    /// the caller's `IncludeItemTypes` means the kind is only force-*included*
    /// when media itself is off (a media search already covers it), while a
    /// disabled — or incompatible — category force-*excludes* the kind.
    /// `Year`/`Folder`/`CollectionFolder` are always excluded, and a non-empty
    /// include set wins outright: C# clears the exclusions and the media types.
    ///
    /// Note what is *not* set: no `start_index` (upstream pages the ranked
    /// window after the query, not with `OFFSET`) and `search_term` rather than
    /// `name_contains`, because only `search_term` earns the relevance
    /// `ORDER BY`.
    fn to_item_query(
        query: &SearchQuery,
        user: Option<ferrofin_db::entities::users::UserEntity>,
    ) -> InternalItemsQuery {
        /// C# `SearchEngine.AddIfMissing`.
        fn add_if_missing(list: &mut Vec<BaseItemKind>, kind: BaseItemKind) {
            if !list.contains(&kind) {
                list.push(kind);
            }
        }

        let mut exclude = query.exclude_item_types.clone();
        let mut include = query.include_item_types.clone();

        exclude.push(BaseItemKind::Year);
        exclude.push(BaseItemKind::Folder);

        // One `category` block per C# `if (query.IncludeX && (…)) … else …`.
        // `probe` is the kind the C# condition tests for membership; `kinds` is
        // what the branch then adds — they differ only for genres, where the
        // test is `Contains(Genre)` but both `Genre` and `MusicGenre` move.
        let mut category = |enabled: bool, probe: BaseItemKind, kinds: &[BaseItemKind]| {
            if enabled && (include.is_empty() || include.contains(&probe)) {
                if !query.include_media {
                    for kind in kinds {
                        add_if_missing(&mut include, *kind);
                    }
                }
            } else {
                for kind in kinds {
                    add_if_missing(&mut exclude, *kind);
                }
            }
        };
        category(
            query.include_genres,
            BaseItemKind::Genre,
            &[BaseItemKind::Genre, BaseItemKind::MusicGenre],
        );
        category(
            query.include_people,
            BaseItemKind::Person,
            &[BaseItemKind::Person],
        );
        category(
            query.include_studios,
            BaseItemKind::Studio,
            &[BaseItemKind::Studio],
        );
        category(
            query.include_artists,
            BaseItemKind::MusicArtist,
            &[BaseItemKind::MusicArtist],
        );

        add_if_missing(&mut exclude, BaseItemKind::CollectionFolder);
        add_if_missing(&mut exclude, BaseItemKind::Folder);

        let mut media_types = query.media_types.clone();
        if !include.is_empty() {
            exclude.clear();
            media_types.clear();
        }

        InternalItemsQuery {
            // C# `new InternalItemsQuery(user)`. This is what applies
            // `TopParentIds` (the user's enabled libraries) and the visibility
            // filters; an unscoped search returns rows — the `UserRootFolder`
            // among them — that `/Items` correctly hides from the same user.
            user,
            search_term: if query.search_term.is_empty() {
                None
            } else {
                Some(query.search_term.clone())
            },
            include_item_types: include,
            exclude_item_types: exclude,
            media_types,
            include_items_by_name: Some(query.parent_id.is_none()),
            parent_id: query.parent_id.unwrap_or_default(),
            limit: query.limit,
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            is_movie: query.is_movie,
            is_series: query.is_series,
            is_news: query.is_news,
            is_kids: query.is_kids,
            is_sports: query.is_sports,
            recursive: true,
            ..Default::default()
        }
    }

    /// Runs the item query behind the search and returns the matching rows.
    async fn matching_items(
        &self,
        query: &SearchQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // C# `if (!query.UserId.IsEmpty()) user = _userManager.GetUserById(...)`.
        let user = if query.user_id.is_nil() {
            None
        } else {
            self.users.get_user_by_id(query.user_id).await?
        };
        self.items
            .get_item_list(&Self::to_item_query(query, user))
            .await
    }

    /// `ChannelName`: the name of the item a hint's `ChannelId` names (C#
    /// `_libraryManager.GetItemById(item.ChannelId)`), read once per page.
    async fn attach_channel_names(&self, hints: &mut [SearchHint]) -> Result<(), ServiceError> {
        let mut ids: Vec<Uuid> = hints
            .iter()
            .filter_map(|hint| hint.channel_id.filter(|id| !id.is_nil()))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(());
        }
        let names: std::collections::HashMap<Uuid, Option<String>> = self
            .items
            .retrieve_items(&ids)
            .await?
            .into_iter()
            .filter_map(|row| Uuid::parse_str(&row.id).ok().map(|id| (id, row.name)))
            .collect();
        for hint in hints {
            if let Some(name) = hint.channel_id.and_then(|id| names.get(&id)) {
                hint.channel_name.clone_from(name);
            }
        }
        Ok(())
    }

    /// The hints' artwork (`GetSearchHintResult` with `SetThumbImageInfo` and
    /// `SetBackdropImageInfo`): the page's ancestors are read one generation
    /// per batch and every image row in one more read, never a query per hint.
    async fn attach_artwork(
        &self,
        hints: &mut [SearchHint],
        items: &[BaseItemEntity],
    ) -> Result<(), ServiceError> {
        let Some(processor) = self.images.as_deref() else {
            return Ok(());
        };
        let rows: std::collections::HashMap<Uuid, &BaseItemEntity> = items
            .iter()
            .filter_map(|row| Uuid::parse_str(&row.id).ok().map(|id| (id, row)))
            .collect();
        let page_rows: Vec<&BaseItemEntity> = hints
            .iter()
            .filter_map(|h| rows.get(&h.id).copied())
            .collect();
        let ancestors = self.ancestors(&page_rows).await?;
        let mut ids: Vec<Uuid> = hints
            .iter()
            .map(|h| h.id)
            .chain(ancestors.keys().copied())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let images = self.items.get_image_infos_batch(&ids).await?;
        for hint in hints.iter_mut() {
            let parents = rows
                .get(&hint.id)
                .map(|row| parent_chain(row, &ancestors))
                .unwrap_or_default();
            fill_artwork(processor, hint, &parents, &images).await;
        }
        Ok(())
    }

    /// Every ancestor of `items` by id (`BaseItem.GetParents()`: the parent,
    /// its parent, and so on up the `ParentId` chain), one batch per generation.
    async fn ancestors(
        &self,
        items: &[&BaseItemEntity],
    ) -> Result<std::collections::HashMap<Uuid, BaseItemEntity>, ServiceError> {
        let mut known = std::collections::HashMap::new();
        let mut wanted: Vec<Uuid> = items.iter().filter_map(|row| parent_of(row)).collect();
        loop {
            wanted.sort_unstable();
            wanted.dedup();
            wanted.retain(|id| !known.contains_key(id));
            if wanted.is_empty() {
                return Ok(known);
            }
            let rows = self.items.retrieve_items(&wanted).await?;
            wanted.clear();
            for row in rows {
                if let Ok(id) = Uuid::parse_str(&row.id) {
                    wanted.extend(parent_of(&row));
                    known.insert(id, row);
                }
            }
        }
    }

    /// The `MusicAlbum` parents of the `Audio` rows in `items`, by album id.
    ///
    /// C# reads `song.AlbumEntity` (the song's parent `MusicAlbum`) per hint;
    /// one batched read over the page's distinct parents does the same work
    /// without a query per row.
    async fn album_parents(
        &self,
        items: &[BaseItemEntity],
    ) -> Result<std::collections::HashMap<Uuid, BaseItemEntity>, ServiceError> {
        let mut parent_ids: Vec<Uuid> = Vec::new();
        for item in items {
            if !matches!(
                kind_from_type_name(&item.type_),
                Some(BaseItemKind::Audio | BaseItemKind::AudioBook)
            ) {
                continue;
            }
            if let Some(parent) = item
                .parent_id
                .as_deref()
                .and_then(|raw| Uuid::parse_str(raw).ok())
                && !parent_ids.contains(&parent)
            {
                parent_ids.push(parent);
            }
        }
        if parent_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = self
            .items
            .get_item_list(&InternalItemsQuery {
                item_ids: parent_ids,
                include_item_types: vec![BaseItemKind::MusicAlbum],
                ..Default::default()
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| Uuid::parse_str(&row.id).ok().map(|id| (id, row)))
            .collect())
    }
}

/// The relevance score of `name` against the search `term` (both compared
/// case-insensitively), or `None` when the name does not match at all.
fn score(name: &str, term: &str) -> Option<f32> {
    if term.is_empty() {
        return Some(SCORE_SUBSTRING);
    }
    let name_lc = ferrofin_util::string_extensions::lower_invariant(name);
    let term_lc = ferrofin_util::string_extensions::lower_invariant(term);
    if name_lc == term_lc {
        Some(SCORE_EXACT)
    } else if name_lc.starts_with(&term_lc) {
        Some(SCORE_PREFIX)
    } else if name_lc.contains(&term_lc) {
        Some(SCORE_SUBSTRING)
    } else {
        None
    }
}

/// A row's parent id, when it has one.
fn parent_of(row: &BaseItemEntity) -> Option<Uuid> {
    row.parent_id
        .as_deref()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .filter(|id| !id.is_nil())
}

/// `item.GetParents()` as `(id, kind)`, nearest first, from the rows
/// [`FerrofinSearchManager::ancestors`] read. A cycle ends the walk.
fn parent_chain(
    item: &BaseItemEntity,
    ancestors: &std::collections::HashMap<Uuid, BaseItemEntity>,
) -> Vec<(Uuid, BaseItemKind)> {
    let mut chain: Vec<(Uuid, BaseItemKind)> = Vec::new();
    let mut next = parent_of(item);
    while let Some(id) = next {
        let Some(row) = ancestors.get(&id) else { break };
        if chain.iter().any(|(seen, _)| *seen == id) {
            break;
        }
        chain.push((
            id,
            kind_from_type_name(&row.type_).unwrap_or(BaseItemKind::Folder),
        ));
        next = parent_of(row);
    }
    chain
}

/// The first image of `kind` an item has (`GetImageInfo(type, 0)`).
fn first_image(
    images: &std::collections::HashMap<Uuid, Vec<ItemImageInfo>>,
    item: Uuid,
    kind: ImageType,
) -> Option<&ItemImageInfo> {
    images
        .get(&item)?
        .iter()
        .find(|image| image.image_type == kind)
}

/// The image half of `GetSearchHintResult`: the Primary tag with that image's
/// aspect ratio; the Thumb of the item, else (for an Episode) its first Series
/// with one, else its first parent with one (`SetThumbImageInfo`); the
/// Backdrop of the item, else its first parent with one
/// (`SetBackdropImageInfo`). An image the processor cannot tag is skipped.
async fn fill_artwork(
    processor: &dyn ImageProcessor,
    hint: &mut SearchHint,
    parents: &[(Uuid, BaseItemKind)],
    images: &std::collections::HashMap<Uuid, Vec<ItemImageInfo>>,
) {
    let tag = |item: Uuid, image: &ItemImageInfo| {
        let image = image.clone();
        async move {
            processor
                .get_image_cache_tag(item, &image)
                .await
                .ok()
                .flatten()
        }
    };
    if let Some(primary) = first_image(images, hint.id, ImageType::Primary)
        && let Some(primary_tag) = tag(hint.id, primary).await
    {
        hint.primary_image_tag = Some(primary_tag);
        let own = images.get(&hint.id).map_or(&[][..], Vec::as_slice);
        hint.primary_image_aspect_ratio =
            crate::dto_service::primary_image_aspect_ratio(processor, hint.id, hint.type_, own)
                .await;
    }
    let with = |kind: ImageType| {
        move |(id, _): &&(Uuid, BaseItemKind)| first_image(images, *id, kind).is_some()
    };
    let thumb_owner = if first_image(images, hint.id, ImageType::Thumb).is_some() {
        Some(hint.id)
    } else {
        (hint.type_ == BaseItemKind::Episode)
            .then(|| {
                parents
                    .iter()
                    .filter(|(_, kind)| *kind == BaseItemKind::Series)
                    .find(with(ImageType::Thumb))
            })
            .flatten()
            .or_else(|| parents.iter().find(with(ImageType::Thumb)))
            .map(|(id, _)| *id)
    };
    if let Some(owner) = thumb_owner
        && let Some(image) = first_image(images, owner, ImageType::Thumb)
        && let Some(thumb_tag) = tag(owner, image).await
    {
        hint.thumb_image_tag = Some(thumb_tag);
        hint.thumb_image_item_id = Some(owner.simple().to_string());
    }
    let backdrop_owner = if first_image(images, hint.id, ImageType::Backdrop).is_some() {
        Some(hint.id)
    } else {
        parents
            .iter()
            .find(with(ImageType::Backdrop))
            .map(|(id, _)| *id)
    };
    if let Some(owner) = backdrop_owner
        && let Some(image) = first_image(images, owner, ImageType::Backdrop)
        && let Some(backdrop_tag) = tag(owner, image).await
    {
        hint.backdrop_image_tag = Some(backdrop_tag);
        hint.backdrop_image_item_id = Some(owner.simple().to_string());
    }
}

/// Maps an item row to a [`SearchHint`].
///
/// Port of `SearchController.GetSearchHintResult`, whose emission rules the
/// wire format depends on: `MatchedTerm` comes from `SearchHintInfo`, which
/// `SearchEngine` never populates, so it stays null and is dropped from the
/// JSON; `IsFolder` is only assigned `true` (a non-folder leaves it null);
/// and `ChannelId` is copied from the item's non-nullable `Guid`, so it is
/// always present — the all-zero id when the item has no channel. The artwork
/// and `ChannelName` need other rows and are attached to the page afterwards.
fn to_hint(
    item: &BaseItemEntity,
    albums: &std::collections::HashMap<Uuid, BaseItemEntity>,
) -> Option<SearchHint> {
    // A hint is pure navigation: the client turns `Id`/`ItemId` straight into an
    // `/Items/{id}` request, so emitting the nil GUID would hand it a hit that
    // 404s. An unparseable id drops the hint — the same choice
    // `get_search_results` below already makes.
    let id = Uuid::parse_str(&item.id).ok()?;
    let kind = kind_from_type_name(&item.type_).unwrap_or(BaseItemKind::Folder);
    // C# `switch (item)`: the music fields are set ONLY on the `MusicAlbum` and
    // `Audio` arms, so a genre/person/movie hint carries no `Artists` at all.
    // `AudioBook` matches the earlier `IHasSeries` arm in C# (it implements
    // `IHasSeries`), so it never reaches the `Audio` arm.
    let (artists, album_artist, album, album_id) = match kind {
        BaseItemKind::MusicAlbum => (
            split_multi(item.artists.as_deref()),
            first_multi(item.album_artists.as_deref()),
            None,
            None,
        ),
        BaseItemKind::Audio => {
            // `song.AlbumEntity` — the parent `MusicAlbum` names the album and
            // supplies `AlbumId`; C# falls back to the song's own `Album` tag
            // when the parent is not an album.
            let parent = item
                .parent_id
                .as_deref()
                .and_then(|raw| Uuid::parse_str(raw).ok())
                .and_then(|pid| albums.get(&pid).map(|row| (pid, row)));
            match parent {
                Some((pid, row)) => (
                    split_multi(item.artists.as_deref()),
                    first_multi(item.album_artists.as_deref()),
                    row.name.clone(),
                    Some(pid),
                ),
                None => (
                    split_multi(item.artists.as_deref()),
                    first_multi(item.album_artists.as_deref()),
                    item.album.clone(),
                    None,
                ),
            }
        }
        _ => (Vec::new(), None, None, None),
    };
    Some(SearchHint {
        item_id: id,
        id,
        name: item.name.clone(),
        matched_term: None,
        index_number: item.index_number.and_then(|n| i32::try_from(n).ok()),
        production_year: item.production_year.and_then(|y| i32::try_from(y).ok()),
        parent_index_number: item.parent_index_number.and_then(|n| i32::try_from(n).ok()),
        primary_image_tag: None,
        thumb_image_tag: None,
        thumb_image_item_id: None,
        backdrop_image_tag: None,
        backdrop_image_item_id: None,
        type_: kind,
        is_folder: crate::dto_service::runtime_is_folder(item).then_some(true),
        run_time_ticks: item.run_time_ticks,
        media_type: parse_media_type(item.media_type.as_deref()),
        // `case LiveTvProgram program: result.StartDate = program.StartDate`.
        start_date: if kind == BaseItemKind::LiveTvProgram {
            item.start_date
        } else {
            None
        },
        end_date: item.end_date,
        series: item.series_name.clone(),
        // `case Series series: result.Status = series.Status.Value.ToString()`.
        status: if kind == BaseItemKind::Series {
            crate::item_data::read_series_status(item.data.as_deref())
        } else {
            None
        },
        album,
        album_id,
        album_artist,
        artists,
        song_count: None,
        episode_count: None,
        channel_id: Some(
            item.channel_id
                .as_deref()
                .and_then(|raw| Uuid::parse_str(raw).ok())
                .unwrap_or_else(Uuid::nil),
        ),
        channel_name: None,
        primary_image_aspect_ratio: None,
    })
}

/// Parses a stored `MediaType` string into the enum, defaulting to
/// [`MediaType::Unknown`] for a missing/unrecognized value.
fn parse_media_type(stored: Option<&str>) -> MediaType {
    match stored {
        Some("Video") => MediaType::Video,
        Some("Audio") => MediaType::Audio,
        Some("Photo") => MediaType::Photo,
        Some("Book") => MediaType::Book,
        _ => MediaType::Unknown,
    }
}

/// The first value of a stored pipe-delimited multi-value column, if any —
/// C# `song.AlbumArtists?.FirstOrDefault()` / `album.AlbumArtist`.
fn first_multi(stored: Option<&str>) -> Option<String> {
    split_multi(stored).into_iter().next()
}

/// Splits a stored pipe-delimited multi-value column (`artists`, …) into a list,
/// dropping empties. Jellyfin stores these joined by `|`.
fn split_multi(stored: Option<&str>) -> Vec<String> {
    stored
        .map(|s| {
            s.split('|')
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait]
impl SearchManager for FerrofinSearchManager {
    async fn get_search_hints(
        &self,
        query: &SearchQuery,
    ) -> Result<QueryResult<SearchHint>, ServiceError> {
        // The repository already returned the rows in relevance order, capped at
        // `limit`. C# `SearchEngine.GetSearchHints` counts *that* window (its
        // `TotalRecordCount` is the size of the ranked page, not of the whole
        // match set) and only then applies `StartIndex`/`Limit` to it.
        let items = self.matching_items(query).await?;
        let albums = self.album_parents(&items).await?;
        let mut hints: Vec<SearchHint> = items
            .iter()
            .filter_map(|item| to_hint(item, &albums))
            .collect();
        let total = i32::try_from(hints.len()).unwrap_or(i32::MAX);

        if let Some(start) = query.start_index {
            let start = usize::try_from(start).unwrap_or(0).min(hints.len());
            hints.drain(..start);
        }
        if let Some(limit) = query.limit {
            hints.truncate(usize::try_from(limit).unwrap_or(0));
        }
        self.attach_channel_names(&mut hints).await?;
        self.attach_artwork(&mut hints, &items).await?;

        Ok(QueryResult::new(query.start_index, Some(total), hints))
    }

    async fn get_search_results(
        &self,
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, ServiceError> {
        let items = self.matching_items(query).await?;
        let mut results: Vec<SearchResult> = items
            .iter()
            .filter_map(|item| {
                let name = item.name.as_deref().unwrap_or_default();
                let s = score(name, &query.search_term)?;
                let id = Uuid::parse_str(&item.id).ok()?;
                Some(SearchResult {
                    item_id: id,
                    score: s,
                })
            })
            .collect();
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_repository::FerrofinItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::test_support::{
        FakeImages, image_info, save_item, seed_images, seed_named_item, set_clean_name, test_db,
    };
    use ferrofin_db::Database;

    /// `to_hint` must DROP a row whose stored `Id` is not a `Guid` rather than
    /// emit one carrying the nil GUID: a hint is pure navigation, so the client
    /// turns `Id`/`ItemId` straight into an `/Items/00000000-…` request that
    /// 404s. Upstream cannot reach this state (`Item.Id` is a `Guid`), and no
    /// Ferrofin writer emits a non-GUID either — the column is plain `TEXT`, so
    /// this pins the shape, not a reachable bug.
    ///
    /// Deliberately exercises `to_hint` directly. Going through
    /// `get_search_hints` would NOT discriminate: matching moved into SQL on
    /// `CleanName`, so a hand-seeded row without one is filtered out before it
    /// ever reaches this function, and the assertion would pass either way.
    #[test]
    fn to_hint_drops_a_row_whose_id_is_not_a_guid() {
        let mut ok = ferrofin_db::entities::base_items::BaseItemEntity {
            id: Uuid::from_u128(0x201).to_string().to_uppercase(),
            type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
            name: Some("Stalker".to_owned()),
            ..Default::default()
        };
        let no_albums = std::collections::HashMap::new();
        assert!(
            to_hint(&ok, &no_albums).is_some(),
            "a parseable id yields a hint"
        );

        ok.id = "not-a-guid".to_owned();
        assert!(
            to_hint(&ok, &no_albums).is_none(),
            "an unparseable id must be dropped, never emitted as the nil GUID"
        );
    }

    /// `SearchController.GetSearchHintResult`'s `switch (item)`: the music
    /// fields are set only on the `MusicAlbum` and `Audio` arms, and an `Audio`
    /// row takes `Album`/`AlbumId` from its parent `MusicAlbum`.
    #[test]
    fn hint_music_fields_follow_the_csharp_switch() {
        use ferrofin_db::entities::base_items::BaseItemEntity;

        let album_id = Uuid::from_u128(0x301);
        let album = BaseItemEntity {
            id: album_id.to_string().to_uppercase(),
            type_: "MediaBrowser.Controller.Entities.Audio.MusicAlbum".to_owned(),
            name: Some("Album 01".to_owned()),
            artists: Some("Artist 03".to_owned()),
            album_artists: Some("Artist 03".to_owned()),
            ..Default::default()
        };
        let mut albums = std::collections::HashMap::new();
        albums.insert(album_id, album.clone());

        let hint = to_hint(&album, &albums).expect("album hint");
        assert_eq!(hint.artists, vec!["Artist 03".to_owned()]);
        assert_eq!(hint.album_artist.as_deref(), Some("Artist 03"));
        assert_eq!(hint.album, None);
        assert_eq!(hint.album_id, None);

        let song = BaseItemEntity {
            id: Uuid::from_u128(0x302).to_string().to_uppercase(),
            type_: "MediaBrowser.Controller.Entities.Audio.Audio".to_owned(),
            name: Some("Track 01".to_owned()),
            parent_id: Some(album_id.to_string().to_uppercase()),
            artists: Some("Artist 03".to_owned()),
            album_artists: Some("Artist 03".to_owned()),
            ..Default::default()
        };
        let hint = to_hint(&song, &albums).expect("song hint");
        assert_eq!(hint.artists, vec!["Artist 03".to_owned()]);
        assert_eq!(hint.album_artist.as_deref(), Some("Artist 03"));
        assert_eq!(hint.album.as_deref(), Some("Album 01"));
        assert_eq!(hint.album_id, Some(album_id));

        // A non-music row falls through the switch: no Artists, no Album.
        let genre = BaseItemEntity {
            id: Uuid::from_u128(0x303).to_string().to_uppercase(),
            type_: "MediaBrowser.Controller.Entities.Genre".to_owned(),
            name: Some("Ambient".to_owned()),
            artists: Some("leaked".to_owned()),
            album: Some("leaked".to_owned()),
            ..Default::default()
        };
        let hint = to_hint(&genre, &albums).expect("genre hint");
        assert!(hint.artists.is_empty());
        assert_eq!(hint.album, None);
        assert_eq!(hint.is_folder, None, "Genre : BaseItem, not Folder");
    }

    fn manager(db: &Database) -> FerrofinSearchManager {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        FerrofinSearchManager::new(
            Arc::new(FerrofinItemRepository::new(db.clone(), lookup)),
            Arc::new(crate::user_manager::FerrofinUserManager::new(db.clone())),
        )
    }

    /// `GetParents()` as the artwork walk reads it: nearest first, ending at a
    /// parent with no row and at a cycle (upstream would loop forever).
    #[test]
    fn parent_chain_stops_at_a_missing_row_and_at_a_cycle() {
        let row = |id: u128, parent: Option<u128>| BaseItemEntity {
            id: Uuid::from_u128(id).to_string().to_uppercase(),
            type_: "MediaBrowser.Controller.Entities.Folder".to_owned(),
            parent_id: parent.map(|p| Uuid::from_u128(p).to_string().to_uppercase()),
            ..Default::default()
        };
        let ancestors: std::collections::HashMap<Uuid, BaseItemEntity> = [
            row(0xA, Some(0xB)),
            row(0xB, Some(0xA)),
            row(0xC, Some(0xD)),
        ]
        .into_iter()
        .map(|r| (Uuid::parse_str(&r.id).expect("id"), r))
        .collect();
        let ids = |item: BaseItemEntity| -> Vec<u128> {
            parent_chain(&item, &ancestors)
                .into_iter()
                .map(|(id, _)| id.as_u128())
                .collect()
        };
        assert_eq!(
            ids(row(0x1, Some(0xA))),
            [0xA, 0xB],
            "a cycle ends the walk"
        );
        assert_eq!(
            ids(row(0x2, Some(0xC))),
            [0xC],
            "a parent with no row ends it"
        );
        assert!(ids(row(0x3, None)).is_empty());
    }

    /// The hint-artwork fixture: a series (with a status, a Thumb and a
    /// Backdrop), its season (a Thumb of its own), an episode of it (a
    /// Primary), a movie airing on a channel, and that channel. Returns their
    /// ids in that order.
    async fn seed_hint_artwork(db: &Database) -> [Uuid; 5] {
        let [series, season, episode, movie, channel] =
            [0x501, 0x502, 0x503, 0x504, 0x505].map(|n: u128| Uuid::from_u128(n));
        let row = |id: Uuid, type_: &str, name: &str, parent: Option<Uuid>| BaseItemEntity {
            id: id.to_string().to_uppercase(),
            type_: format!("MediaBrowser.Controller.Entities.{type_}"),
            name: Some(name.to_owned()),
            clean_name: Some(name.to_lowercase()),
            parent_id: parent.map(|p| p.to_string().to_uppercase()),
            ..Default::default()
        };
        save_item(
            db,
            &BaseItemEntity {
                is_folder: true,
                data: Some(r#"{"Status":"Ended"}"#.to_owned()),
                ..row(series, "TV.Series", "Show", None)
            },
        )
        .await;
        save_item(
            db,
            &BaseItemEntity {
                is_folder: true,
                ..row(season, "TV.Season", "Season 1", Some(series))
            },
        )
        .await;
        save_item(db, &row(episode, "TV.Episode", "Pilot", Some(season))).await;
        save_item(
            db,
            &BaseItemEntity {
                start_date: Some(chrono::Utc::now()),
                channel_id: Some(channel.to_string().to_uppercase()),
                ..row(movie, "Movies.Movie", "Pilot Light", None)
            },
        )
        .await;
        save_item(
            db,
            &BaseItemEntity {
                type_: "MediaBrowser.Controller.Channels.Channel".to_owned(),
                ..row(channel, "", "Chan", None)
            },
        )
        .await;
        let seed = |id: Uuid, images: Vec<(ImageType, &str)>| {
            let images: Vec<_> = images
                .into_iter()
                .map(|(kind, path)| image_info(kind, path, None))
                .collect();
            let db = db.clone();
            async move { seed_images(&db, id, &images).await }
        };
        seed(
            series,
            vec![
                (ImageType::Thumb, "/img/series-thumb.jpg"),
                (ImageType::Backdrop, "/img/series-backdrop.jpg"),
            ],
        )
        .await;
        seed(season, vec![(ImageType::Thumb, "/img/season-thumb.jpg")]).await;
        seed(episode, vec![(ImageType::Primary, "/img/episode.jpg")]).await;
        [series, season, episode, movie, channel]
    }

    /// `SearchController.GetSearchHintResult`: a hint carries its Primary tag
    /// and that image's aspect ratio, a Thumb and a Backdrop from the item or
    /// else its first ancestor with one (for an Episode, a Series first), the
    /// series status and the channel name; only a program carries `StartDate`.
    #[tokio::test]
    async fn hints_carry_artwork_status_and_channel_as_upstream_does() {
        let db = test_db().await;
        let [series, _season, episode, movie, _channel] = seed_hint_artwork(&db).await;
        let mgr = manager(&db).with_image_processor(Arc::new(FakeImages));
        let hints = |term: &str| {
            let query = SearchQuery {
                search_term: term.to_owned(),
                ..Default::default()
            };
            let mgr = &mgr;
            async move { mgr.get_search_hints(&query).await.expect("hints").items }
        };

        let pilot = hints("pilot").await;
        let hit = |id: Uuid| pilot.iter().find(|h| h.id == id).expect("hint");
        let ep = hit(episode);
        assert_eq!(
            ep.primary_image_tag.as_deref(),
            Some("tag:/img/episode.jpg")
        );
        assert_eq!(
            ep.primary_image_aspect_ratio,
            Some(2.0),
            "the fake's 400x200"
        );
        assert_eq!(
            (
                ep.thumb_image_tag.as_deref(),
                ep.thumb_image_item_id.as_deref()
            ),
            (
                Some("tag:/img/series-thumb.jpg"),
                Some(series.simple().to_string().as_str())
            ),
            "the Series' thumb, not the nearer Season's"
        );
        assert_eq!(
            (
                ep.backdrop_image_tag.as_deref(),
                ep.backdrop_image_item_id.as_deref()
            ),
            (
                Some("tag:/img/series-backdrop.jpg"),
                Some(series.simple().to_string().as_str())
            )
        );
        let film = hit(movie);
        assert_eq!(film.primary_image_tag, None);
        assert_eq!(film.primary_image_aspect_ratio, None);
        assert_eq!(film.start_date, None, "StartDate is a program's only");
        assert_eq!(film.channel_name.as_deref(), Some("Chan"));

        let show = hints("show").await;
        assert_eq!(show[0].status.as_deref(), Some("Ended"));
    }

    #[rstest::rstest]
    #[case("ΟΣ", "οσ")]
    #[case("Élodie", "élodie")]
    #[case("𐐀", "𐐨")]
    fn unicode_search_score(#[case] name: &str, #[case] term: &str) {
        assert_eq!(score(name, term), Some(SCORE_EXACT));
    }

    #[test]
    fn score_tiers_exact_prefix_substring() {
        assert_eq!(score("The Matrix", "the matrix"), Some(SCORE_EXACT));
        assert_eq!(score("The Matrix", "the"), Some(SCORE_PREFIX));
        assert_eq!(score("The Matrix", "atri"), Some(SCORE_SUBSTRING));
        assert_eq!(score("The Matrix", "zzz"), None);
    }

    #[tokio::test]
    async fn hints_are_ranked_by_relevance() {
        let db = test_db().await;
        // Ids avoid 1 (the query translator's placeholder row id).
        let exact = Uuid::from_u128(0x101);
        let sub = Uuid::from_u128(0x102);
        seed_named_item(&db, exact, BaseItemKind::Movie, "Matrix").await;
        seed_named_item(&db, sub, BaseItemKind::Movie, "The Matrix Reloaded").await;
        set_clean_name(&db, exact, "Matrix").await;
        set_clean_name(&db, sub, "The Matrix Reloaded").await;
        let mgr = manager(&db);

        let query = SearchQuery {
            search_term: "matrix".to_owned(),
            ..Default::default()
        };
        let result = mgr.get_search_hints(&query).await.expect("hints");
        assert_eq!(result.items.len(), 2);
        // "Matrix" (exact) ranks above "The Matrix Reloaded" (substring).
        assert_eq!(result.items[0].name.as_deref(), Some("Matrix"));
    }

    /// Seeds one row and gives it the `CleanName` the scanner would have written.
    async fn seed(db: &Database, id: u128, kind: BaseItemKind, name: &str) {
        let id = Uuid::from_u128(id);
        seed_named_item(db, id, kind, name).await;
        set_clean_name(db, id, name).await;
    }

    /// The four relevance tiers, seeded in an order that is deliberately the
    /// *reverse* of the ranked one. Every helper row leaves `SortName` NULL, so
    /// SQLite's fallback ordering is insertion order — which is what a
    /// rank-after-the-fact implementation would surface.
    async fn seed_relevance_tiers(db: &Database) {
        seed(db, 0x201, BaseItemKind::Movie, "Zebra Action").await; // contains
        seed(db, 0x202, BaseItemKind::Movie, "Actionable").await; // prefix
        seed(db, 0x203, BaseItemKind::Movie, "Action Figures").await; // word prefix
        seed(db, 0x204, BaseItemKind::Movie, "Action").await; // exact
    }

    #[tokio::test]
    async fn hints_rank_exact_then_word_prefix_then_prefix_then_contains() {
        let db = test_db().await;
        seed_relevance_tiers(&db).await;
        let mgr = manager(&db);

        let result = mgr
            .get_search_hints(&SearchQuery {
                search_term: "action".to_owned(),
                ..Default::default()
            })
            .await
            .expect("hints");

        let names: Vec<_> = result
            .items
            .iter()
            .map(|h| h.name.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(
            names,
            ["Action", "Action Figures", "Actionable", "Zebra Action"]
        );
    }

    #[tokio::test]
    async fn limit_keeps_the_best_matches_not_the_first_rows() {
        // The regression this guards: ranking after a `LIMIT` returns whichever
        // rows the database happened to hand back, so the two best matches here
        // ("Action", "Action Figures") never reach the client at all.
        let db = test_db().await;
        seed_relevance_tiers(&db).await;
        let mgr = manager(&db);

        let result = mgr
            .get_search_hints(&SearchQuery {
                search_term: "action".to_owned(),
                limit: Some(2),
                ..Default::default()
            })
            .await
            .expect("hints");

        let names: Vec<_> = result
            .items
            .iter()
            .map(|h| h.name.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(names, ["Action", "Action Figures"]);
        // C# counts the ranked window, so the total tracks the limited page.
        assert_eq!(result.total_record_count, 2);
    }

    #[tokio::test]
    async fn start_index_pages_the_ranked_window() {
        let db = test_db().await;
        seed_relevance_tiers(&db).await;
        let mgr = manager(&db);

        let result = mgr
            .get_search_hints(&SearchQuery {
                search_term: "action".to_owned(),
                start_index: Some(1),
                ..Default::default()
            })
            .await
            .expect("hints");

        let names: Vec<_> = result
            .items
            .iter()
            .map(|h| h.name.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(names, ["Action Figures", "Actionable", "Zebra Action"]);
        // `TotalRecordCount` is counted before `StartIndex` trims the window.
        assert_eq!(result.total_record_count, 4);
    }

    #[tokio::test]
    async fn a_row_the_sql_matched_is_never_dropped_by_a_second_opinion() {
        // `CleanName` is diacritic-folded, so the row the WHERE clause matched
        // exactly is one whose raw `Name` does not contain the term at all.
        // Re-deciding the match on `Name` after the query drops it — and takes
        // the reported total down with it.
        let db = test_db().await;
        seed(&db, 0x205, BaseItemKind::Movie, "Áction").await;
        let mgr = manager(&db);

        let result = mgr
            .get_search_hints(&SearchQuery {
                search_term: "action".to_owned(),
                ..Default::default()
            })
            .await
            .expect("hints");

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].name.as_deref(), Some("Áction"));
        assert_eq!(result.total_record_count, 1);
    }

    #[tokio::test]
    async fn folder_kinds_are_never_hinted() {
        let db = test_db().await;
        seed(&db, 0x301, BaseItemKind::Movie, "Action").await;
        seed(&db, 0x302, BaseItemKind::CollectionFolder, "Action Library").await;
        seed(&db, 0x303, BaseItemKind::Folder, "Action Folder").await;
        seed(&db, 0x304, BaseItemKind::Year, "Action Year").await;
        let mgr = manager(&db);

        let result = mgr
            .get_search_hints(&SearchQuery {
                search_term: "action".to_owned(),
                ..Default::default()
            })
            .await
            .expect("hints");

        let kinds: Vec<_> = result.items.iter().map(|h| h.type_).collect();
        assert_eq!(kinds, [BaseItemKind::Movie]);
    }

    #[tokio::test]
    async fn a_disabled_category_drops_its_by_name_kind() {
        let db = test_db().await;
        seed(&db, 0x401, BaseItemKind::Movie, "Action").await;
        seed(&db, 0x402, BaseItemKind::Genre, "Action").await;
        let mgr = manager(&db);

        let with_genres = mgr
            .get_search_hints(&SearchQuery {
                search_term: "action".to_owned(),
                ..Default::default()
            })
            .await
            .expect("hints");
        assert_eq!(with_genres.items.len(), 2);

        let without_genres = mgr
            .get_search_hints(&SearchQuery {
                search_term: "action".to_owned(),
                include_genres: false,
                ..Default::default()
            })
            .await
            .expect("hints");
        let kinds: Vec<_> = without_genres.items.iter().map(|h| h.type_).collect();
        assert_eq!(kinds, [BaseItemKind::Movie]);
    }

    #[tokio::test]
    async fn hint_emission_matches_the_controllers_null_rules() {
        use crate::test_support::seed_folder_item;

        let db = test_db().await;
        seed(&db, 0x501, BaseItemKind::Movie, "Action").await;
        let artist = Uuid::from_u128(0x502);
        seed_folder_item(&db, artist, BaseItemKind::MusicArtist, "Action", None).await;
        set_clean_name(&db, artist, "Action").await;
        let series = Uuid::from_u128(0x503);
        seed_folder_item(&db, series, BaseItemKind::Series, "Action", None).await;
        set_clean_name(&db, series, "Action").await;
        let mgr = manager(&db);

        let result = mgr
            .get_search_hints(&SearchQuery {
                search_term: "action".to_owned(),
                ..Default::default()
            })
            .await
            .expect("hints");
        assert_eq!(result.items.len(), 3);

        for hint in &result.items {
            // `SearchEngine` never fills `SearchHintInfo.MatchedTerm`, so the
            // controller writes null and the field leaves the wire entirely.
            assert_eq!(hint.matched_term, None, "{:?}", hint.type_);
            // `ChannelId` is a non-nullable Guid upstream: always serialized.
            assert_eq!(hint.channel_id, Some(Uuid::nil()), "{:?}", hint.type_);
        }

        let by_kind = |kind: BaseItemKind| {
            result
                .items
                .iter()
                .find(|h| h.type_ == kind)
                .unwrap_or_else(|| panic!("no {kind:?} hint"))
        };
        // `if (item.IsFolder) result.IsFolder = true;` — and nothing else. A
        // parentless artist is accessed by name, so not a folder
        // (`MusicArtist.cs:30-33`), even stored as one by an earlier build.
        assert_eq!(by_kind(BaseItemKind::Movie).is_folder, None);
        assert_eq!(by_kind(BaseItemKind::MusicArtist).is_folder, None);
        assert_eq!(by_kind(BaseItemKind::Series).is_folder, Some(true));
    }

    #[tokio::test]
    async fn results_carry_id_and_score() {
        let db = test_db().await;
        let id = Uuid::from_u128(5);
        seed_named_item(&db, id, BaseItemKind::Movie, "Solaris").await;
        set_clean_name(&db, id, "Solaris").await;
        let mgr = manager(&db);

        let query = SearchQuery {
            search_term: "solaris".to_owned(),
            ..Default::default()
        };
        let results = mgr.get_search_results(&query).await.expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].item_id, Uuid::from_u128(5));
        assert!((results[0].score - SCORE_EXACT).abs() < f32::EPSILON);
    }
}
