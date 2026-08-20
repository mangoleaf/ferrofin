//! [`FerrofinSearchManager`] — the concrete [`SearchManager`].
//!
//! Port of `Emby.Server.Implementations.Library.SearchManager` (the object-safe
//! subset). The C# manager fans a term out across registered search providers,
//! item names, people, genres, studios, and artists, then ranks the union. The
//! provider *registry* is dropped (registration is a composition-root concern);
//! what remains is the item-backed search: translate the [`SearchQuery`] into an
//! [`InternalItemsQuery`] with a name-contains predicate, run it through the
//! injected [`ItemRepository`], and map the rows to [`SearchHint`]s.
//!
//! Relevance ([`SearchResult::score`]) uses the same tiering the C# code applies:
//! an exact (case-insensitive) name match outranks a prefix match, which outranks
//! a substring match. Fuzzy provider scoring is out of scope for this seam.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::querying::QueryResult;
use ferrofin_model::search::{SearchHint, SearchQuery};
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{SearchManager, SearchResult};
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
}

impl std::fmt::Debug for FerrofinSearchManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinSearchManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinSearchManager {
    /// Creates a search manager over the injected item repository.
    #[must_use]
    pub fn new(items: Arc<dyn ItemRepository>) -> Self {
        Self { items }
    }

    /// Translates a [`SearchQuery`] into the item query that backs it.
    ///
    /// The C# search fans out one query per enabled category (media, people,
    /// genres, studios, artists) and unions the ranked results. This port issues a
    /// single name-scoped query: when the caller restricts
    /// [`SearchQuery::include_item_types`] that restriction is honored; otherwise
    /// the query matches every kind (the category `include_*` flags then act as an
    /// *additional* exclusion of the by-name kinds the caller turned off).
    fn to_item_query(query: &SearchQuery) -> InternalItemsQuery {
        let mut exclude = query.exclude_item_types.clone();
        // A disabled category excludes its by-name kinds from the union.
        if !query.include_genres {
            exclude.push(BaseItemKind::Genre);
            exclude.push(BaseItemKind::MusicGenre);
        }
        if !query.include_studios {
            exclude.push(BaseItemKind::Studio);
        }
        if !query.include_artists {
            exclude.push(BaseItemKind::MusicArtist);
        }
        if !query.include_people {
            exclude.push(BaseItemKind::Person);
        }

        InternalItemsQuery {
            name_contains: if query.search_term.is_empty() {
                None
            } else {
                Some(query.search_term.clone())
            },
            include_item_types: query.include_item_types.clone(),
            exclude_item_types: exclude,
            media_types: query.media_types.clone(),
            parent_id: query.parent_id.unwrap_or_default(),
            start_index: query.start_index,
            limit: query.limit,
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
        self.items.get_item_list(&Self::to_item_query(query)).await
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

/// Maps an item row to a [`SearchHint`] carrying the term that matched, or
/// [`None`] when the row id is not a stored `Guid`.
///
/// A hint is pure navigation: the client turns `Id`/`ItemId` straight into an
/// `/Items/{id}` request. Emitting the nil GUID would hand it a hit that 404s,
/// so an unparseable id drops the hint — the same choice `get_search_results`
/// below already makes.
fn to_hint(item: &BaseItemEntity, matched_term: &str) -> Option<SearchHint> {
    let id = Uuid::parse_str(&item.id).ok()?;
    let kind = kind_from_type_name(&item.type_).unwrap_or(BaseItemKind::Folder);
    Some(SearchHint {
        item_id: id,
        id,
        name: item.name.clone(),
        matched_term: Some(matched_term.to_owned()),
        index_number: item.index_number.and_then(|n| i32::try_from(n).ok()),
        production_year: item.production_year.and_then(|y| i32::try_from(y).ok()),
        parent_index_number: item.parent_index_number.and_then(|n| i32::try_from(n).ok()),
        primary_image_tag: None,
        thumb_image_tag: None,
        thumb_image_item_id: None,
        backdrop_image_tag: None,
        backdrop_image_item_id: None,
        type_: kind,
        is_folder: Some(item.is_folder),
        run_time_ticks: item.run_time_ticks,
        media_type: parse_media_type(item.media_type.as_deref()),
        start_date: item.start_date,
        end_date: item.end_date,
        series: item.series_name.clone(),
        status: None,
        album: item.album.clone(),
        album_id: None,
        album_artist: None,
        artists: split_multi(item.artists.as_deref()),
        song_count: None,
        episode_count: None,
        channel_id: None,
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
        let items = self.matching_items(query).await?;
        let mut scored: Vec<(f32, SearchHint)> = items
            .iter()
            .filter_map(|item| {
                let name = item.name.as_deref().unwrap_or_default();
                let s = score(name, &query.search_term)?;
                Some((s, to_hint(item, &query.search_term)?))
            })
            .collect();
        // Highest score first; ties keep query order (stable sort).
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let total = i32::try_from(scored.len()).unwrap_or(i32::MAX);
        let hints: Vec<SearchHint> = scored.into_iter().map(|(_, h)| h).collect();
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
    use crate::test_support::{seed_named_item, seed_named_item_raw_id, set_clean_name, test_db};
    use ferrofin_db::Database;

    fn manager(db: &Database) -> FerrofinSearchManager {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        FerrofinSearchManager::new(Arc::new(FerrofinItemRepository::new(db.clone(), lookup)))
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

    /// A `BaseItems` row whose stored `Id` is not a `Guid` must not surface as a
    /// hint carrying the nil GUID: the client would turn `Id`/`ItemId` straight
    /// into an `/Items/00000000-…` request that 404s. It is dropped instead —
    /// and dropped from `TotalRecordCount` too, matching `get_search_results`.
    #[tokio::test]
    async fn hint_with_a_non_guid_row_id_is_dropped_not_emitted_as_nil() {
        let db = test_db().await;
        let good = Uuid::from_u128(0x201);
        seed_named_item(&db, good, BaseItemKind::Movie, "Stalker").await;
        set_clean_name(&db, good, "Stalker").await;
        // A row the schema permits (`Id` is plain `TEXT`) but no writer emits.
        seed_named_item_raw_id(&db, "not-a-guid", BaseItemKind::Movie, "Stalker 2").await;

        let query = SearchQuery {
            search_term: "stalker".to_owned(),
            ..Default::default()
        };
        let result = manager(&db).get_search_hints(&query).await.expect("hints");

        assert!(
            result
                .items
                .iter()
                .all(|h| !h.id.is_nil() && !h.item_id.is_nil()),
            "no hint may carry the nil GUID"
        );
        assert_eq!(
            result.items.len(),
            1,
            "only the row with a parseable id is hinted"
        );
        assert_eq!(result.items[0].id, good);
        assert_eq!(
            result.total_record_count, 1,
            "the dropped row must not be counted either"
        );
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
