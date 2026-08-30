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
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::querying::QueryResult;
use ferrofin_model::search::{SearchHint, SearchQuery};
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
        Self { items, users }
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
    let name_lc = name.to_lowercase();
    let term_lc = term.to_lowercase();
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

/// Maps an item row to a [`SearchHint`].
///
/// Port of `SearchController.GetSearchHintResult`, whose emission rules the
/// wire format depends on: `MatchedTerm` comes from `SearchHintInfo`, which
/// `SearchEngine` never populates, so it stays null and is dropped from the
/// JSON; `IsFolder` is only assigned `true` (a non-folder leaves it null);
/// and `ChannelId` is copied from the item's non-nullable `Guid`, so it is
/// always present — the all-zero id when the item has no channel.
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
        is_folder: item.is_folder.then_some(true),
        run_time_ticks: item.run_time_ticks,
        media_type: parse_media_type(item.media_type.as_deref()),
        start_date: item.start_date,
        end_date: item.end_date,
        series: item.series_name.clone(),
        status: None,
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
    use crate::test_support::{seed_named_item, set_clean_name, test_db};
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
        let mgr = manager(&db);

        let result = mgr
            .get_search_hints(&SearchQuery {
                search_term: "action".to_owned(),
                ..Default::default()
            })
            .await
            .expect("hints");
        assert_eq!(result.items.len(), 2);

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
        // `if (item.IsFolder) result.IsFolder = true;` — and nothing else.
        assert_eq!(by_kind(BaseItemKind::Movie).is_folder, None);
        assert_eq!(by_kind(BaseItemKind::MusicArtist).is_folder, Some(true));
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
