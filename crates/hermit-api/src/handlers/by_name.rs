//! Shared plumbing for the "by-name" browse controllers (Genres, `MusicGenres`,
//! Studios, Persons, Years, Artists).
//!
//! Every one of these controllers has the same two shapes:
//!
//! - A **list** endpoint returning a [`QueryResult<BaseItemDto>`] projected from
//!   the library manager's by-name aggregates ([`ItemWithCounts`]), optionally
//!   folding the aggregated counts onto each DTO when the caller asked to filter
//!   by item type (Jellyfin's `RequestHelpers.CreateQueryResult`).
//! - A **single** endpoint resolving one by-name item by its route name and
//!   projecting it with [`DtoService::get_base_item_dto`].
//!
//! This module holds the pieces those handlers share so each controller file is
//! just its route wiring plus the manager call that differs — extracting the
//! reuse rather than copying it into six files.

use hermit_db::entities::users::UserEntity;
use hermit_model::dto::BaseItemDto;
use hermit_model::querying::QueryResult;
use hermit_traits::options::DtoOptions;
use hermit_traits::persistence::ItemWithCounts;

use crate::error::ApiError;
use crate::state::AppState;

/// The paging + name-range query parameters shared by every by-name **list**
/// endpoint.
///
/// The full Jellyfin query is far wider (image/user-data toggles, sort orders,
/// media types); the remaining parameters are accepted by the per-controller
/// query structs but only these paging/name filters change which rows come back,
/// so they live here and feed [`base_query`].
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ByNameListQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    pub user_id: Option<uuid::Uuid>,
    /// The index of the first record to return.
    #[serde(default)]
    pub start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    pub limit: Option<i32>,
    /// A case-insensitive search term the name must contain.
    #[serde(default)]
    pub search_term: Option<String>,
    /// Restrict to items whose name is sorted at or after this value.
    #[serde(default)]
    pub name_starts_with_or_greater: Option<String>,
    /// Restrict to items whose name starts with this value.
    #[serde(default)]
    pub name_starts_with: Option<String>,
    /// Restrict to items whose name sorts before this value.
    #[serde(default)]
    pub name_less_than: Option<String>,
    /// Localizes the browse to a specific parent item/folder when set.
    #[serde(default)]
    pub parent_id: Option<uuid::Uuid>,
    /// When non-empty, the aggregated counts are folded onto each DTO.
    #[serde(default)]
    pub include_item_types: Option<String>,
    /// Whether a total record count is requested (defaults to `true` in C#).
    #[serde(default)]
    pub enable_total_record_count: Option<bool>,
}

impl ByNameListQuery {
    /// Builds the [`InternalItemsQuery`](hermit_traits::options::InternalItemsQuery)
    /// the manager runs, translating the shared name/paging filters and the
    /// resolved user. The `parent_id` localizes the browse the way Jellyfin's
    /// `AncestorIds`/`ItemIds` split does (folders scope by ancestor, non-folders
    /// by item); here it always scopes by ancestor, the common case.
    pub(crate) fn base_query(
        &self,
        user: Option<UserEntity>,
    ) -> hermit_traits::options::InternalItemsQuery {
        let mut ancestor_ids = Vec::new();
        if let Some(parent) = self.parent_id {
            ancestor_ids.push(parent);
        }
        hermit_traits::options::InternalItemsQuery {
            user,
            start_index: self.start_index,
            limit: self.limit,
            search_term: self.search_term.clone(),
            name_starts_with_or_greater: self.name_starts_with_or_greater.clone(),
            name_starts_with: self.name_starts_with.clone(),
            name_less_than: self.name_less_than.clone(),
            ancestor_ids,
            enable_total_record_count: self.enable_total_record_count.unwrap_or(true),
            ..hermit_traits::options::InternalItemsQuery::default()
        }
    }

    /// Whether the caller asked to filter by item type, which — per Jellyfin —
    /// means the aggregated child counts should be folded onto each DTO.
    pub(crate) fn should_include_item_types(&self) -> bool {
        self.include_item_types
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }
}

/// The single-item query parameters shared by every by-name `{name}` endpoint.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ByNameItemQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    pub user_id: Option<uuid::Uuid>,
}

/// Projects a page of already-resolved by-name item rows into a
/// [`QueryResult<BaseItemDto>`] via [`DtoService::get_item_by_name_dto`].
///
/// Used by the browses whose manager call already yields the item rows (Persons,
/// Years) rather than count-carrying [`ItemWithCounts`] aggregates.
pub(crate) async fn project_item_rows(
    state: &AppState,
    result: QueryResult<hermit_db::entities::base_items::BaseItemEntity>,
    options: &DtoOptions,
    user: Option<&UserEntity>,
) -> Result<QueryResult<BaseItemDto>, ApiError> {
    let start_index = Some(result.start_index);
    let total = Some(result.total_record_count);
    let mut dtos = Vec::with_capacity(result.items.len());
    for item in &result.items {
        let dto = state
            .dto
            .get_item_by_name_dto(item, options, None, user)
            .await?;
        dtos.push(dto);
    }
    Ok(QueryResult::new(start_index, total, dtos))
}

/// Projects a page of [`ItemWithCounts`] aggregates into a
/// [`QueryResult<BaseItemDto>`], mirroring `RequestHelpers.CreateQueryResult`.
///
/// Each aggregate's item row is projected through
/// [`DtoService::get_item_by_name_dto`]; when `include_item_types` is set the
/// aggregated counts are copied onto the DTO's count fields (`ChildCount`,
/// `ProgramCount`, …) exactly as Jellyfin does.
pub(crate) async fn project_query_result(
    state: &AppState,
    result: QueryResult<ItemWithCounts>,
    options: &DtoOptions,
    include_item_types: bool,
    user: Option<&UserEntity>,
) -> Result<QueryResult<BaseItemDto>, ApiError> {
    let start_index = Some(result.start_index);
    let total = Some(result.total_record_count);
    let mut dtos = Vec::with_capacity(result.items.len());
    for ItemWithCounts { item, counts } in result.items {
        let mut dto = state
            .dto
            .get_item_by_name_dto(&item, options, None, user)
            .await?;
        if include_item_types {
            dto.child_count = Some(counts.item_count);
            dto.program_count = Some(counts.program_count);
            dto.series_count = Some(counts.series_count);
            dto.episode_count = Some(counts.episode_count);
            dto.movie_count = Some(counts.movie_count);
            dto.trailer_count = Some(counts.trailer_count);
            dto.album_count = Some(counts.album_count);
            dto.song_count = Some(counts.song_count);
            dto.artist_count = Some(counts.artist_count);
        }
        dtos.push(dto);
    }
    Ok(QueryResult::new(start_index, total, dtos))
}
