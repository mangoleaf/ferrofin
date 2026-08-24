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

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::DtoOptions;
use ferrofin_traits::persistence::ItemWithCounts;

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
    /// Restrict to items the caller has (not) favourited.
    #[serde(default)]
    pub is_favorite: Option<bool>,
    /// Comma-delimited `ItemFilter` flags (`IsFavorite`, …).
    #[serde(default)]
    pub filters: Option<String>,
    /// Comma-delimited [`ItemFields`](ferrofin_model::querying::ItemFields) to
    /// populate on each DTO. Absent/empty ⇒ the base DTO, matching Jellyfin's
    /// `new DtoOptions { Fields = fields }`.
    #[serde(default)]
    pub fields: Option<String>,
    /// Whether image information is populated (C# default `true`).
    #[serde(default)]
    pub enable_images: Option<bool>,
    /// The maximum number of images to return, per image type.
    #[serde(default)]
    pub image_type_limit: Option<i32>,
    /// Comma-delimited [`ImageType`](ferrofin_model::entities::ImageType) set to
    /// populate. Empty ⇒ every type, as upstream.
    #[serde(default)]
    pub enable_image_types: Option<String>,
    /// Whether user data is populated. Ignored by the controllers that pin it
    /// (Genres / `MusicGenres` hardcode `false` upstream).
    #[serde(default)]
    pub enable_user_data: Option<bool>,
}

/// Builds the projection options a by-name browse hands to the DTO service.
///
/// Mirrors C# `new DtoOptions { Fields = fields }.AddAdditionalDtoOptions(
/// enableImages, enableUserData, imageTypeLimit, enableImageTypes)`:
/// `enable_images` defaults on, `image_type_limit`/`enable_user_data` only
/// override when the caller sent them, and an empty `enableImageTypes` leaves
/// the full type set in place.
///
/// `enable_user_data` is a parameter rather than read off the query because the
/// controllers disagree: `GenresController`/`MusicGenresController` pass a
/// literal `false` (so upstream never emits a `UserData` block on those rows),
/// while Artists/Studios/Persons/Years forward the caller's value.
pub(crate) fn additional_dto_options(
    fields: Option<&str>,
    enable_images: Option<bool>,
    enable_user_data: Option<bool>,
    image_type_limit: Option<i32>,
    enable_image_types: Option<&str>,
) -> DtoOptions {
    let mut options = DtoOptions {
        fields: crate::handlers::query_parse::parse_csv_enums_lenient(fields),
        enable_images: enable_images.unwrap_or(true),
        ..DtoOptions::default()
    };
    if let Some(limit) = image_type_limit {
        options.image_type_limit = limit;
    }
    if let Some(enabled) = enable_user_data {
        options.enable_user_data = enabled;
    }
    let requested: Vec<ferrofin_model::entities::ImageType> =
        crate::handlers::query_parse::parse_csv_enums_lenient(enable_image_types);
    if !requested.is_empty() {
        options.image_types = requested;
    }
    options
}

impl ByNameListQuery {
    /// This browse's [`DtoOptions`], with `enable_user_data` supplied by the
    /// caller — see [`additional_dto_options`] for why it is not read off the
    /// query.
    pub(crate) fn dto_options(&self, enable_user_data: Option<bool>) -> DtoOptions {
        additional_dto_options(
            self.fields.as_deref(),
            self.enable_images,
            enable_user_data,
            self.image_type_limit,
            self.enable_image_types.as_deref(),
        )
    }

    /// Builds the [`InternalItemsQuery`](ferrofin_traits::options::InternalItemsQuery)
    /// the manager runs, translating the shared name/paging filters and the
    /// resolved user. The `parent_id` localizes the browse the way Jellyfin's
    /// `AncestorIds`/`ItemIds` split does (folders scope by ancestor, non-folders
    /// by item); here it always scopes by ancestor, the common case.
    pub(crate) fn base_query(
        &self,
        user: Option<UserEntity>,
    ) -> ferrofin_traits::options::InternalItemsQuery {
        let mut ancestor_ids = Vec::new();
        if let Some(parent) = self.parent_id {
            ancestor_ids.push(parent);
        }
        // C# folds `filters ∋ IsFavorite` onto the tri-state when `isFavorite`
        // is absent (ArtistsController/GenresController do this same dance).
        let filters = crate::handlers::query_parse::parse_csv_enums_lenient::<
            ferrofin_model::querying::ItemFilter,
        >(self.filters.as_deref());
        let is_favorite = self.is_favorite.or_else(|| {
            filters
                .contains(&ferrofin_model::querying::ItemFilter::IsFavorite)
                .then_some(true)
        });
        // Scope the aggregate to the requested kinds (C# sets IncludeItemTypes
        // on the inner query): the Movies "Genres" tab must list only genres
        // carried by movies, not by every item under the parent. Lenient parse,
        // matching upstream's tolerant comma-delimited model binder.
        let include_item_types = crate::handlers::query_parse::parse_csv_enums_lenient::<
            ferrofin_model::data::BaseItemKind,
        >(self.include_item_types.as_deref());
        ferrofin_traits::options::InternalItemsQuery {
            user,
            is_favorite,
            include_item_types,
            start_index: self.start_index,
            limit: self.limit,
            search_term: self.search_term.clone(),
            name_starts_with_or_greater: self.name_starts_with_or_greater.clone(),
            name_starts_with: self.name_starts_with.clone(),
            name_less_than: self.name_less_than.clone(),
            ancestor_ids,
            enable_total_record_count: self.enable_total_record_count.unwrap_or(true),
            ..ferrofin_traits::options::InternalItemsQuery::default()
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
/// Strips `ItemCounts` from a by-name **list**'s options.
///
/// C# `DtoService.GetItemByNameDto` only stamps the ten count fields when
/// `ItemFields.ItemCounts` is requested AND `taggedItems` is non-empty:
///
/// ```csharp
/// if (options.ContainsField(ItemFields.ItemCounts)
///     && taggedItems is not null && taggedItems.Count != 0)
/// { SetItemByNameInfo(item, dto, taggedItems); }
/// ```
///
/// Every by-name LIST passes `null` (Genres/MusicGenres/Studios/Artists via
/// `RequestHelpers.CreateQueryResult`, and Persons) or an empty list (Years),
/// so upstream never emits counts from `ItemCounts` on a list — only from the
/// separate `includeItemTypes` overwrite block.
///
/// Ferrofin routes these through `get_base_item_dtos`, which runs
/// `name_counts_batch` on sight of `ItemCounts` and stamps all ten. That was
/// unreachable while the list options hardcoded "no fields"; honouring the
/// caller's `fields` makes it reachable, so the C# precondition has to be
/// enforced here instead.
fn without_item_counts(options: &DtoOptions) -> DtoOptions {
    let mut trimmed = options.clone();
    trimmed
        .fields
        .retain(|f| *f != ferrofin_model::querying::ItemFields::ItemCounts);
    trimmed
}

pub(crate) async fn project_item_rows(
    state: &AppState,
    result: QueryResult<ferrofin_db::entities::base_items::BaseItemEntity>,
    options: &DtoOptions,
    user: Option<&UserEntity>,
) -> Result<QueryResult<BaseItemDto>, ApiError> {
    let start_index = Some(result.start_index);
    let total = Some(result.total_record_count);
    // With no pre-supplied tagged ids and no `ItemCounts` field requested (the
    // by-name list options), `get_item_by_name_dto` reduces to `build_dto` — the
    // same thing `get_base_item_dtos` does, but batching the per-item image and
    // user-data loads into two queries instead of 2×N. Same rows, same order.
    let options = without_item_counts(options);
    let dtos = state
        .dto
        .get_base_item_dtos(&result.items, &options, user, None, true)
        .await?;
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
    // Split the count-carrying aggregates into rows + counts; batch-build the
    // DTOs (two prefetch queries instead of an N+1 loop), then fold the
    // aggregated counts back on by index — order is preserved by the builder.
    let (items, counts): (Vec<_>, Vec<_>) = result
        .items
        .into_iter()
        .map(|iwc| (iwc.item, iwc.counts))
        .unzip();
    let options = without_item_counts(options);
    let mut dtos = state
        .dto
        .get_base_item_dtos(&items, &options, user, None, true)
        .await?;
    if include_item_types {
        for (dto, counts) in dtos.iter_mut().zip(counts.iter()) {
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
    }
    Ok(QueryResult::new(start_index, total, dtos))
}

#[cfg(test)]
mod tests {
    use super::{ByNameListQuery, additional_dto_options};
    use ferrofin_model::entities::ImageType;
    use ferrofin_model::querying::ItemFields;

    #[test]
    fn defaults_match_the_parameterless_csharp_call() {
        // `new DtoOptions { Fields = [] }.AddAdditionalDtoOptions(null, null, null, [])`
        // leaves images on, the type set complete and the limit unbounded.
        let o = additional_dto_options(None, None, None, None, None);
        assert!(o.fields.is_empty());
        assert!(o.enable_images);
        assert!(o.enable_user_data);
        assert_eq!(o.image_type_limit, i32::MAX);
        assert_eq!(o.image_types.len(), 13);
    }

    #[test]
    fn every_toggle_is_applied() {
        let o = additional_dto_options(
            Some("Overview,Path"),
            Some(false),
            Some(false),
            Some(2),
            Some("Primary,Thumb"),
        );
        assert!(o.contains_field(ItemFields::Overview));
        assert!(o.contains_field(ItemFields::Path));
        assert!(!o.contains_field(ItemFields::Genres));
        assert!(!o.enable_images);
        assert!(!o.enable_user_data);
        assert_eq!(o.image_type_limit, 2);
        assert_eq!(o.image_types, vec![ImageType::Primary, ImageType::Thumb]);
    }

    #[test]
    fn caller_supplied_user_data_flag_overrides_the_query() {
        // Genres/MusicGenres pass `Some(false)` regardless of what arrived.
        let query = ByNameListQuery {
            enable_user_data: Some(true),
            ..ByNameListQuery::default()
        };
        assert!(!query.dto_options(Some(false)).enable_user_data);
        assert!(query.dto_options(query.enable_user_data).enable_user_data);
    }
}
