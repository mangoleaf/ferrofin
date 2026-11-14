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
//!   projecting it with `DtoService::get_base_item_dto`.
//!
//! This module holds the pieces those handlers share so each controller file is
//! just its route wiring plus the manager call that differs — extracting the
//! reuse rather than copying it into six files.

use std::sync::Arc;

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::{AuthorizationInfo, DtoOptions};
use ferrofin_traits::persistence::ItemWithCounts;

use crate::error::ApiError;
use crate::handlers::items::require_scope_visible;
use crate::handlers::query_parse::{parse_csv_i32, parse_order_by};
use crate::state::AppState;

/// The query of the by-name **list** endpoints: what `GET /Artists` and
/// `GET /Artists/AlbumArtists` bind, and what [`ValueListQuery`] fills for
/// Genres, `MusicGenres` and Studios with only the parameters they declare.
/// It feeds [`base_query`].
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ByNameListQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    pub parent_id: Option<uuid::Uuid>,
    /// When non-empty, the aggregated counts are folded onto each DTO.
    #[serde(default)]
    pub include_item_types: Option<String>,
    /// Comma-delimited `BaseItemKind`s the inner content query excludes.
    #[serde(default)]
    pub exclude_item_types: Option<String>,
    /// Comma-delimited media types the inner content query is restricted to.
    #[serde(default)]
    pub media_types: Option<String>,
    /// Comma-delimited sort keys (C# `sortBy`), paired with [`Self::sort_order`].
    #[serde(default)]
    pub sort_by: Option<String>,
    /// Comma-delimited sort directions (C# `sortOrder`).
    #[serde(default)]
    pub sort_order: Option<String>,
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
    /// Whether user data is populated. Genres and `MusicGenres` hardcode
    /// `false` upstream and do not bind it ([`ValueListQuery::genres`]).
    #[serde(default)]
    pub enable_user_data: Option<bool>,
    /// Pipe-delimited genre names the by-name row itself must carry
    /// (`ArtistsController` only; v12 `ByName.cs:191`).
    #[serde(default)]
    pub genres: Option<String>,
    /// Comma-delimited genre item ids the by-name row must reference.
    #[serde(default)]
    pub genre_ids: Option<String>,
    /// Pipe-delimited official ratings of the by-name row.
    #[serde(default)]
    pub official_ratings: Option<String>,
    /// Pipe-delimited tags of the by-name row.
    #[serde(default)]
    pub tags: Option<String>,
    /// Comma-delimited production years of the by-name row.
    #[serde(default)]
    pub years: Option<String>,
    /// Comma-delimited studio item ids the by-name row must reference.
    #[serde(default)]
    pub studio_ids: Option<String>,
    /// Pipe-delimited studio **names** (`ArtistsController` only). Non-empty,
    /// they replace [`Self::studio_ids`] with the named studios' ids — see
    /// [`Self::artists_query`]. The Genres, `MusicGenres` and Studios
    /// controllers declare no such parameter: [`ValueListQuery`] never sets it.
    #[serde(default)]
    pub studios: Option<String>,
    /// `[FromQuery] double? minCommunityRating`, which only the
    /// `ArtistsController` actions declare: bound raw ([`ValueListQuery`]
    /// never sets it), and checked by
    /// [`Self::check_min_community_rating`] on the Artists routes. A valid value
    /// is never read: `GetItemValues` copies it onto neither filter.
    #[serde(default)]
    pub min_community_rating: Option<String>,
}

/// The query `GET /Genres`, `GET /MusicGenres` and `GET /Studios` bind: only
/// the parameters their v12.4 actions declare (`GenresController.GetGenres`,
/// `MusicGenresController.GetMusicGenres`, `StudiosController.GetStudios`).
/// ASP.NET never binds any other, so a value there, valid or not, changes
/// nothing. `sortBy`/`sortOrder` (Genres and `MusicGenres` only) and
/// `enableUserData` (Studios only) bind raw; [`Self::genres`] and
/// [`Self::studios`] keep the ones each action declares.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ValueListQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<uuid::Uuid>,
    /// The index of the first record to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// A search term the name must match.
    #[serde(default)]
    search_term: Option<String>,
    /// Names sorted at or after this value.
    #[serde(default)]
    name_starts_with_or_greater: Option<String>,
    /// Names starting with this value.
    #[serde(default)]
    name_starts_with: Option<String>,
    /// Names sorted before this value.
    #[serde(default)]
    name_less_than: Option<String>,
    /// The parent item the browse is scoped to.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    parent_id: Option<uuid::Uuid>,
    /// Comma-delimited `BaseItemKind`s the inner content query includes.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Comma-delimited `BaseItemKind`s the inner content query excludes.
    #[serde(default)]
    exclude_item_types: Option<String>,
    /// Comma-delimited sort keys; Genres and `MusicGenres` only.
    #[serde(default)]
    sort_by: Option<String>,
    /// Comma-delimited sort directions; Genres and `MusicGenres` only.
    #[serde(default)]
    sort_order: Option<String>,
    /// Whether a total record count is requested.
    #[serde(default)]
    enable_total_record_count: Option<bool>,
    /// Restrict to items the caller has (not) favourited.
    #[serde(default)]
    is_favorite: Option<bool>,
    /// Comma-delimited fields to populate on each DTO.
    #[serde(default)]
    fields: Option<String>,
    /// Whether image information is populated.
    #[serde(default)]
    enable_images: Option<bool>,
    /// The maximum number of images to return, per image type.
    #[serde(default)]
    image_type_limit: Option<i32>,
    /// Comma-delimited image types to populate.
    #[serde(default)]
    enable_image_types: Option<String>,
    /// `bool? enableUserData`, which only `GetStudios` declares: raw here,
    /// converted by [`Self::studios`].
    #[serde(default)]
    enable_user_data: Option<String>,
}

impl ValueListQuery {
    /// The Genres and `MusicGenres` browse: sorted as asked, `enableUserData`
    /// not bound (the controllers pass a literal `false`).
    pub(crate) fn genres(self) -> ByNameListQuery {
        self.into_list(true, None)
    }

    /// The Studios browse: no `sortBy`/`sortOrder`, and `enableUserData`
    /// converted as the query binder converts a `bool?` (blank is absent).
    ///
    /// # Errors
    ///
    /// `400` for an `enableUserData` that is not a boolean.
    pub(crate) fn studios(self) -> Result<ByNameListQuery, ApiError> {
        let enable_user_data = match self.enable_user_data.as_deref() {
            None => None,
            Some(raw) => <Option<bool> as serde::Deserialize>::deserialize(
                crate::query::value::Value::nullable(raw),
            )
            .map_err(|_| {
                ApiError::BadRequest(format!(
                    "The value '{raw}' is not valid for enableUserData."
                ))
            })?,
        };
        Ok(self.into_list(false, enable_user_data))
    }

    /// The shared by-name query with only the declared parameters set.
    fn into_list(self, sorted: bool, enable_user_data: Option<bool>) -> ByNameListQuery {
        ByNameListQuery {
            user_id: self.user_id,
            start_index: self.start_index,
            limit: self.limit,
            search_term: self.search_term,
            name_starts_with_or_greater: self.name_starts_with_or_greater,
            name_starts_with: self.name_starts_with,
            name_less_than: self.name_less_than,
            parent_id: self.parent_id,
            include_item_types: self.include_item_types,
            exclude_item_types: self.exclude_item_types,
            sort_by: self.sort_by.filter(|_| sorted),
            sort_order: self.sort_order.filter(|_| sorted),
            enable_total_record_count: self.enable_total_record_count,
            is_favorite: self.is_favorite,
            fields: self.fields,
            enable_images: self.enable_images,
            image_type_limit: self.image_type_limit,
            enable_image_types: self.enable_image_types,
            enable_user_data,
            ..ByNameListQuery::default()
        }
    }
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
    /// The `400` the query binder answers on `/Artists` and
    /// `/Artists/AlbumArtists` for a `minCommunityRating` that does not convert
    /// to `double?`. It binds as the query binder binds one (blank is absent),
    /// only later, as `ImageQuery`'s `imageIndex` does.
    pub(crate) fn check_min_community_rating(&self) -> Result<(), ApiError> {
        let Some(raw) = self.min_community_rating.as_deref() else {
            return Ok(());
        };
        <Option<f64> as serde::Deserialize>::deserialize(crate::query::value::Value::nullable(raw))
            .map(drop)
            .map_err(|_| {
                ApiError::BadRequest(format!(
                    "The value '{raw}' is not valid for minCommunityRating."
                ))
            })
    }

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
    ///
    /// # Errors
    ///
    /// A malformed id list or year list, or a contradictory `filters` pair, is
    /// a `400` — ASP.NET's binder and C# `ApplyFilters` reject them the same way.
    pub(crate) fn base_query(
        &self,
        user: Option<Arc<UserEntity>>,
    ) -> Result<ferrofin_traits::options::InternalItemsQuery, ApiError> {
        let mut ancestor_ids = Vec::new();
        if let Some(parent) = self.parent_id {
            ancestor_ids.push(parent);
        }
        // C# `query.ApplyFilters(filters)` runs after the explicit `isFavorite`
        // is set, so a `filters ∋ IsFavorite` wins over `isFavorite=false`
        // (`ArtistsController.cs:203`); the other flags land on their tri-states.
        let filters = crate::handlers::query_parse::parse_csv_enums_lenient::<
            ferrofin_model::querying::ItemFilter,
        >(self.filters.as_deref());
        let genre_ids = crate::handlers::query_parse::parse_csv_uuids(self.genre_ids.as_deref())?;
        let studio_ids = crate::handlers::query_parse::parse_csv_uuids(self.studio_ids.as_deref())?;
        let years = parse_csv_i32(self.years.as_deref())?;
        // Scope the aggregate to the requested kinds (C# sets IncludeItemTypes
        // on the inner query): the Movies "Genres" tab must list only genres
        // carried by movies, not by every item under the parent. Lenient parse,
        // matching upstream's tolerant comma-delimited model binder.
        let include_item_types = crate::handlers::query_parse::parse_csv_enums_lenient::<
            ferrofin_model::data::BaseItemKind,
        >(self.include_item_types.as_deref());
        let exclude_item_types = crate::handlers::query_parse::parse_csv_enums_lenient::<
            ferrofin_model::data::BaseItemKind,
        >(self.exclude_item_types.as_deref());
        // `GetItemValues` copies `MediaTypes` onto the inner content query too.
        let media_types = crate::handlers::query_parse::parse_csv_enums_lenient::<
            ferrofin_model::data::MediaType,
        >(self.media_types.as_deref());
        let mut query = ferrofin_traits::options::InternalItemsQuery {
            user,
            is_favorite: self.is_favorite,
            // The repository computes per-kind counts only when `ItemCounts`
            // is requested (v12 `ByName.cs:251`), and the controller adds it
            // when a type filter is set — see [`Self::should_include_item_types`].
            dto_options: Arc::new(DtoOptions {
                fields: self.requested_fields(),
                ..DtoOptions::default()
            }),
            include_item_types,
            exclude_item_types,
            media_types,
            // C# `MusicGenresController` (and every by-name sibling) passes
            // `OrderBy = RequestHelpers.GetOrderBy(sortBy, sortOrder)`, which
            // `ApplyOrder` then applies to the OUTER by-name query. Dropping it
            // made `sortOrder=Descending` a silent no-op.
            order_by: parse_order_by(self.sort_by.as_deref(), self.sort_order.as_deref()),
            start_index: self.start_index,
            limit: self.limit,
            search_term: self.search_term.clone(),
            name_starts_with_or_greater: self.name_starts_with_or_greater.clone(),
            name_starts_with: self.name_starts_with.clone(),
            name_less_than: self.name_less_than.clone(),
            // The rest of C#'s `outerQueryFilter` (`ByName.cs:177-196`): the
            // by-name row's own genres, tags, ratings, years and referenced
            // studios/genres.
            genres: crate::handlers::query_parse::parse_pipe_strings(self.genres.as_deref()),
            genre_ids,
            official_ratings: crate::handlers::query_parse::parse_pipe_strings(
                self.official_ratings.as_deref(),
            ),
            tags: crate::handlers::query_parse::parse_pipe_strings(self.tags.as_deref()),
            years,
            studio_ids,
            ancestor_ids,
            enable_total_record_count: self.enable_total_record_count.unwrap_or(true),
            ..ferrofin_traits::options::InternalItemsQuery::default()
        };
        query
            .apply_filters(&filters)
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        Ok(query)
    }

    /// The item `parentId` names, through [`resolve_parent`].
    ///
    /// # Errors
    ///
    /// `400` for an id naming no item, `403` for one the user may not open.
    pub(crate) async fn parent(
        &self,
        state: &AppState,
        auth: &AuthorizationInfo,
        user: Option<&UserEntity>,
    ) -> Result<Option<ferrofin_db::entities::base_items::BaseItemEntity>, ApiError> {
        resolve_parent(state, auth, self.parent_id, user).await
    }

    /// [`Self::base_query`] for `GET /Artists` and `GET /Artists/AlbumArtists`,
    /// which also take `studios`: v12.2 `ArtistsController.cs:182-195`
    /// (`:336-349` for album artists) sets `query.StudioIds` to the names'
    /// studio ids, replacing any `studioIds`, when the list is non-empty.
    /// `GetItemValues` then applies `StudioIds` to the artist row itself
    /// (`ByName.cs:221`), which carries no studios, so any name empties the list.
    ///
    /// # Errors
    ///
    /// Everything [`Self::base_query`] refuses, and a failed studio lookup.
    pub(crate) async fn artists_query(
        &self,
        state: &AppState,
        user: Option<Arc<UserEntity>>,
    ) -> Result<ferrofin_traits::options::InternalItemsQuery, ApiError> {
        let mut query = self.base_query(user)?;
        if let Some(ids) = resolve_by_name_ids(
            state,
            ferrofin_model::data::BaseItemKind::Studio,
            self.studios.as_deref(),
        )
        .await?
        {
            query.studio_ids = ids;
        }
        Ok(query)
    }

    /// Whether the per-kind counts are folded onto each DTO — C#
    /// `filter.DtoOptions.ContainsField(ItemFields.ItemCounts)` after the
    /// controller's augmentation: "asking for a type filter has always
    /// implied wanting that type's counts back" (v12 `ArtistsController.cs:129-133`
    /// and its Genres/MusicGenres/Studios siblings), so a non-empty
    /// `includeItemTypes` adds `ItemCounts` to the requested fields.
    pub(crate) fn should_include_item_types(&self) -> bool {
        self.requested_fields()
            .contains(&ferrofin_model::querying::ItemFields::ItemCounts)
    }

    /// The caller's `fields`, plus `ItemCounts` when `includeItemTypes` is
    /// set — the field set the v12 by-name controllers hand the repository.
    fn requested_fields(&self) -> Vec<ferrofin_model::querying::ItemFields> {
        let mut fields: Vec<ferrofin_model::querying::ItemFields> =
            crate::handlers::query_parse::parse_csv_enums_lenient(self.fields.as_deref());
        let has_type_filter = self
            .include_item_types
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        if has_type_filter && !fields.contains(&ferrofin_model::querying::ItemFields::ItemCounts) {
            fields.push(ferrofin_model::querying::ItemFields::ItemCounts);
        }
        fields
    }
}

/// v12.2 `LibraryManager.GetParentItem` (`LibraryManager.cs:3276-3289`) for a
/// `parentId` the request sent: the item it names, or `ArgumentException`
/// ("Invalid parent id: …", or `GetItemById`'s "Guid can't be empty" for the
/// zero id), which `ExceptionMiddleware` answers `400`. No `parentId` is
/// [`None`]; the caller decides what stands for the root.
///
/// # Errors
///
/// `400` for an id naming no item, the zero id included, and `403` for one the
/// user may not open ([`require_scope_visible`]).
pub(crate) async fn resolve_parent(
    state: &AppState,
    auth: &AuthorizationInfo,
    parent_id: Option<uuid::Uuid>,
    user: Option<&UserEntity>,
) -> Result<Option<ferrofin_db::entities::base_items::BaseItemEntity>, ApiError> {
    let Some(parent_id) = parent_id else {
        return Ok(None);
    };
    if parent_id.is_nil() {
        return Err(ApiError::BadRequest("Guid can't be empty".to_owned()));
    }
    match state.library.get_item_by_id(parent_id).await? {
        Some(parent) => {
            require_scope_visible(state, auth, &parent, user).await?;
            Ok(Some(parent))
        }
        None => Err(ApiError::BadRequest(format!(
            "Invalid parent id: {parent_id}"
        ))),
    }
}

/// The single-item query parameters shared by every by-name `{name}` endpoint.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ByNameItemQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    pub user_id: Option<uuid::Uuid>,
}

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

/// Projects a page of already-resolved by-name item rows into a
/// [`QueryResult<BaseItemDto>`], as `DtoService.GetItemByNameDto` does upstream.
///
/// Used by the browses whose manager call already yields the item rows (Persons,
/// Years) rather than count-carrying [`ItemWithCounts`] aggregates.
pub(crate) async fn project_item_rows(
    state: &AppState,
    result: QueryResult<ferrofin_db::entities::base_items::BaseItemEntity>,
    options: &DtoOptions,
    user: Option<&UserEntity>,
) -> Result<QueryResult<BaseItemDto>, ApiError> {
    let start_index = Some(result.start_index);
    let total = Some(result.total_record_count);
    // With no tagged items and no `ItemCounts` field (the by-name list
    // options), C# `GetItemByNameDto` is `GetBaseItemDtoInternal`: what
    // `get_base_item_dtos` does, batching the per-item image and user-data
    // loads into two queries instead of 2×N. Same rows, same order.
    let options = without_item_counts(options);
    let dtos = state
        .dto
        .get_base_item_dtos(&result.items, &options, user, None, true)
        .await?;
    Ok(QueryResult::new(start_index, total, dtos))
}

/// `BaseItem.SlugChar` — `'-'`,
/// `MediaBrowser.Controller/Entities/BaseItem.cs:98` on v10.11.8, `:100` on
/// master (the declaration is byte-identical; only its line moved).
const SLUG_CHAR: char = '-';

/// Resolves a by-name route's `{name}` the way `GenresController.GetGenre` and
/// `MusicGenresController.GetMusicGenre` do — the only two controllers in
/// either tree that carry the slug branch (`git grep -l GetItemFromSlugName`).
///
/// A name WITHOUT the slug char goes through `LibraryManager.GetGenre`, which
/// is `CreateItemByName<T>` and therefore materializes the row.
///
/// A name WITH it is a slug: upstream `GetItemFromSlugName<T>` runs three plain
/// `GetItemList` lookups substituting `'-'` for `'&'`, then `'/'`, then `'?'`,
/// and creates NOTHING. Creating here would mint a bogus genre for every
/// hyphenated mis-spelling, which is exactly what Ferrofin used to do.
///
/// The three substitutions are the WHOLE list, deliberately. Upstream's
/// consequence — that `/Genres/Sci-Fi` answers with the empty fallback even
/// when a genre named exactly `Sci-Fi` exists, because a hyphen is only ever
/// read as a slug — is real, and Ferrofin reproduces it. An earlier cut of this
/// port added a fourth, non-creating lookup on the LITERAL name to reach that
/// row. It was removed: a fourth candidate matches NEITHER tree (checked on
/// both: `GetItemFromSlugName` is byte-identical on v10.11.8 and master and
/// stops after `'?'`), and "Ferrofin matches neither tree" is the definition of
/// the bug, not of a capability worth keeping — nothing shipped before this
/// batch reached that row either, since the pre-existing behaviour here was the
/// create-on-GET defect this batch removed. The test
/// `a_hyphenated_name_is_only_ever_read_as_a_slug` (`tests/by_name_browse.rs`)
/// pins the upstream quirk.
///
/// Returns [`None`] when nothing matches; the caller substitutes
/// [`LibraryManager::empty_by_name_item`], which is upstream's `item ??= new
/// Genre()`.
///
/// [`LibraryManager::empty_by_name_item`]: ferrofin_traits::library::LibraryManager::empty_by_name_item
pub(crate) async fn resolve_by_name_or_slug(
    state: &AppState,
    kind: ferrofin_model::data::BaseItemKind,
    name: &str,
) -> Result<Option<ferrofin_db::entities::base_items::BaseItemEntity>, ApiError> {
    if !name.contains(SLUG_CHAR) {
        return Ok(state.library.get_named_item(kind, name).await?);
    }
    for candidate in [
        name.replace(SLUG_CHAR, "&"),
        name.replace(SLUG_CHAR, "/"),
        name.replace(SLUG_CHAR, "?"),
    ] {
        if let Some(item) = state.library.find_named_item(kind, &candidate).await? {
            return Ok(Some(item));
        }
    }
    Ok(None)
}

/// The id a name filter takes for a name that resolves to no row: the empty
/// GUID, which no stored item carries. The referenced-item match it feeds
/// (`WhereReferencedItem`: the `CleanName` of the referenced rows) finds no row
/// for it, and `AlbumIds` (`ParentId IN …`) matches no item either: `ParentId`
/// is a foreign key to `BaseItems.Id`, so no row's parent is the empty GUID.
const UNRESOLVED_NAME_ID: uuid::Uuid = uuid::Uuid::nil();

/// Resolves a pipe-delimited by-name **name** filter to the ids of its
/// `…Ids` counterpart: the controllers'
/// `names.Select(n => _libraryManager.Get<Kind>(n)).Where(i => i is not null).Select(i => i!.Id)`
/// (v12.2 `ArtistsController.cs:182-195` for `studios`; `ItemsController.cs`
/// does the same for its `studios`). Upstream wraps each `GetStudio` /
/// `GetArtist` in a `try`/`catch` and drops a name whose lookup throws; this
/// resolver propagates a repository error (`?`) as an error response instead,
/// so a caller must not assume lookup errors are swallowed.
///
/// `None` when the parameter names nothing (`studios.Length == 0`): the
/// caller's `…Ids` filter stands. Otherwise one id per name, and the caller
/// REPLACES that filter with them.
///
/// `Get<Kind>` is `CreateItemByName<T>`: an unknown name gets a row minted,
/// and its fresh id is one no item references, so the name matches nothing.
/// This lookup is the non-creating
/// [`find_named_item`](ferrofin_traits::library::LibraryManager::find_named_item),
/// because a GET must not write, and a miss takes [`UNRESOLVED_NAME_ID`],
/// which matches the same nothing. A miss is never dropped: with every name
/// dropped the list would be empty, and an empty `…Ids` filter is no filter.
///
/// Writing an item value materializes its by-name row
/// (`ItemPersistenceService::save_item_values`), so a name that resolves to
/// nothing is one no item carries. The exception is an adopted database
/// holding a value whose row Jellyfin's by-name validators had not created
/// yet: upstream's minted row would match the items carrying it, a miss here
/// matches none.
///
/// `GetArtist` (`artists` on `GET /Items`) special-cases `MusicArtist`: it
/// takes an existing row by `Name`, a folder-backed artist before a by-name
/// one (`IsAccessedByName`), which a plain `CleanName` match does not order.
/// The choice cannot change an `ArtistIds` filter, which reads the `CleanName`
/// of the row it names and so matches the same items for any row of that
/// name; it would matter to an id-keyed use. Not for `albums`: see
/// [`resolve_album_ids`].
pub(crate) async fn resolve_by_name_ids(
    state: &AppState,
    kind: ferrofin_model::data::BaseItemKind,
    raw: Option<&str>,
) -> Result<Option<Vec<uuid::Uuid>>, ApiError> {
    let names = crate::handlers::query_parse::parse_pipe_strings(raw);
    if names.is_empty() {
        return Ok(None);
    }
    // One non-creating lookup per name, accepted as the cost: upstream runs one
    // `GetStudio` per name, and on the artist routes any non-empty list empties
    // the answer anyway. No cap on the count, and no skipping the lookup.
    let mut ids = Vec::with_capacity(names.len());
    for name in &names {
        let row = state.library.find_named_item(kind, name).await?;
        ids.push(
            row.and_then(|row| uuid::Uuid::parse_str(&row.id).ok())
                .unwrap_or(UNRESOLVED_NAME_ID),
        );
    }
    Ok(Some(ids))
}

/// Resolves `GET /Items`' pipe-delimited `albums` names to the ids that
/// replace `albumIds`: `albums.SelectMany(name => GetItemIds(new
/// InternalItemsQuery { IncludeItemTypes = [MusicAlbum], Name = name, Limit = 1 }))`
/// (v12.2 `ItemsController.cs:554-561`) — the first album of each name by
/// `SortName` (upstream's `ApplyOrder` default; ties fall to the engine), and
/// no row written.
///
/// `None` when the parameter names nothing. A name no album has takes
/// [`UNRESOLVED_NAME_ID`], so it matches nothing. Upstream drops it instead,
/// and a list of unknown albums then leaves `AlbumIds` empty, which is no
/// filter: `albums=<a typo>` answers the whole library there. That Jellyfin
/// bug is not ported.
pub(crate) async fn resolve_album_ids(
    state: &AppState,
    raw: Option<&str>,
) -> Result<Option<Vec<uuid::Uuid>>, ApiError> {
    let names = crate::handlers::query_parse::parse_pipe_strings(raw);
    if names.is_empty() {
        return Ok(None);
    }
    let mut ids = Vec::with_capacity(names.len());
    for name in names {
        let query = ferrofin_traits::options::InternalItemsQuery {
            include_item_types: vec![ferrofin_model::data::BaseItemKind::MusicAlbum],
            name: Some(name),
            limit: Some(1),
            ..ferrofin_traits::options::InternalItemsQuery::default()
        };
        let found = state.library.get_item_ids(&query).await?;
        ids.push(found.first().copied().unwrap_or(UNRESOLVED_NAME_ID));
    }
    Ok(Some(ids))
}

/// Projects a page of [`ItemWithCounts`] aggregates into a
/// [`QueryResult<BaseItemDto>`], mirroring `RequestHelpers.CreateQueryResult`.
///
/// Each aggregate's item row is projected as `DtoService.GetItemByNameDto`
/// projects it, without `ItemCounts` (see [`project_item_rows`]); when
/// `include_item_types` — the caller's
/// [`ByNameListQuery::should_include_item_types`] — is set, the per-kind
/// counts are copied onto the DTO's count fields (`ChildCount`,
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
        // v12 `RequestHelpers.CreateQueryResult` (`RequestHelpers.cs:166-178`):
        // the ten count fields, `ChildCount` being `ItemCounts.ItemCount`,
        // which `BuildItemCountsByCleanName` sets to `TotalItemCount()`
        // (`ByName.cs:399`) — 10.11.8 never assigned it and so always sent 0.
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
            dto.music_video_count = Some(counts.music_video_count);
        }
    }
    Ok(QueryResult::new(start_index, total, dtos))
}

crate::query::query_parameters! {
    ByNameListQuery {
        "includeItemTypes" => ',',
        "excludeItemTypes" => ',',
        "mediaTypes" => ',',
        "sortBy" => ',',
        "sortOrder" => ',',
        "filters" => ',',
        "fields" => ',',
        "enableImageTypes" => ',',
        "genres" => '|',
        "genreIds" => ',',
        "officialRatings" => '|',
        "tags" => '|',
        "years" => ',',
        "studioIds" => ',',
        "studios" => '|',
    } => [("get", "/Artists"), ("get", "/Artists/AlbumArtists")];
    ValueListQuery {
        "includeItemTypes" => ',',
        "excludeItemTypes" => ',',
        "sortBy" => ',',
        "sortOrder" => ',',
        "fields" => ',',
        "enableImageTypes" => ',',
    } => [("get", "/Genres"), ("get", "/MusicGenres"), ("get", "/Studios")];
    ByNameItemQuery {} => [("get", "/Genres/{genreName}"), ("get", "/MusicGenres/{genreName}"), ("get", "/Persons/{name}"), ("get", "/Studios/{name}"), ("get", "/Artists/{name}")];
}

#[cfg(test)]
mod tests {
    use super::{ByNameListQuery, ValueListQuery, additional_dto_options};
    use ferrofin_model::entities::ImageType;
    use ferrofin_model::querying::ItemFields;

    /// The query parameters the vendored contracts give `routes`.
    fn contract_parameters(routes: &[&str]) -> std::collections::BTreeSet<String> {
        let mut contract = std::collections::BTreeSet::new();
        for raw in [
            include_str!("../../../../contracts/jellyfin-openapi-10.11.8.json"),
            include_str!("../../../../contracts/jellyfin-openapi-12.1.0.json"),
        ] {
            let spec: serde_json::Value = serde_json::from_str(raw).expect("contract parses");
            for route in routes {
                // Jellyfin 12 drops `/MusicGenres`; 10.11.8 still lists it.
                let Some(parameters) = spec["paths"][route]["get"]["parameters"].as_array() else {
                    continue;
                };
                contract.extend(
                    parameters
                        .iter()
                        .filter(|p| p["in"] == "query")
                        .filter_map(|p| p["name"].as_str().map(str::to_owned)),
                );
            }
        }
        contract
    }

    /// Every query parameter the vendored contracts give the two artist list
    /// routes is one of [`ByNameListQuery`]'s fields, except the three
    /// `/Artists` parameters v12.4 accepts and ignores and whose binding never
    /// fails: `GetItemValues` copies `Person`, `PersonIds` and `PersonTypes`
    /// onto neither of its filters (`ByName.cs:180-233`), and a string or a
    /// comma-delimited list binds whatever is sent. serde drops an unknown key
    /// without a word, so a missing field is a filter the client asked for and
    /// never got (`studios` was one, and every artist came back), or a value
    /// the binder refuses and Ferrofin let through (`minCommunityRating`).
    #[test]
    fn by_name_list_query_binds_every_contract_parameter() {
        const UPSTREAM_NO_OPS: [&str; 3] = ["person", "personIds", "personTypes"];
        let contract = contract_parameters(&["/Artists", "/Artists/AlbumArtists"]);
        assert!(contract.contains("studios"), "contract read: {contract:?}");
        let keys = crate::query::fields::<ByNameListQuery>();
        let unbound: std::collections::BTreeSet<&str> = contract
            .iter()
            .map(String::as_str)
            .filter(|name| !keys.contains(name))
            .collect();
        assert_eq!(
            unbound,
            UPSTREAM_NO_OPS.into_iter().collect(),
            "left: the contract parameters ByNameListQuery drops; right: v12.4's no-ops. \
             A newly dropped parameter must be bound, not listed"
        );
    }

    /// [`ValueListQuery`] binds exactly the parameters the Genres,
    /// `MusicGenres` and Studios actions declare (gl-408): one it drops is a
    /// filter the client never got, one it adds is a value ASP.NET ignores
    /// that Ferrofin would act on or refuse.
    #[test]
    fn value_list_query_binds_exactly_the_contract_parameters() {
        let contract = contract_parameters(&["/Genres", "/MusicGenres", "/Studios"]);
        let keys: std::collections::BTreeSet<String> = crate::query::fields::<ValueListQuery>()
            .iter()
            .map(|k| (*k).to_owned())
            .collect();
        assert_eq!(keys, contract);
    }

    /// Studios declares no `sortBy`/`sortOrder` and Genres no
    /// `enableUserData`; only Studios converts `enableUserData`, as a `bool?`.
    #[test]
    fn each_value_list_keeps_only_what_its_action_declares() {
        let query = || ValueListQuery {
            sort_by: Some("Name".into()),
            sort_order: Some("Descending".into()),
            enable_user_data: Some("abc".into()),
            ..ValueListQuery::default()
        };
        let genres = query().genres();
        assert_eq!(genres.sort_by.as_deref(), Some("Name"));
        assert_eq!(genres.enable_user_data, None);
        assert!(query().studios().is_err(), "a bool? the binder refuses");
        let studios = ValueListQuery {
            enable_user_data: Some("False".into()),
            ..query()
        }
        .studios()
        .expect("binds");
        assert_eq!((studios.sort_by, studios.sort_order), (None, None));
        assert_eq!(studios.enable_user_data, Some(false));
        let blank = ValueListQuery {
            enable_user_data: Some(String::new()),
            ..ValueListQuery::default()
        };
        assert_eq!(blank.studios().expect("blank").enable_user_data, None);
    }

    #[test]
    fn defaults_match_the_parameterless_csharp_call() {
        // `new DtoOptions { Fields = [] }.AddAdditionalDtoOptions(null, null, null, [])`
        // leaves images on, the type set complete and the limit unbounded.
        let o = additional_dto_options(None, None, None, None, None);
        assert!(o.fields.is_empty(), "{:?}", o.fields);
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

    /// v12 `ArtistsController.cs:129-133`: a type filter implies `ItemCounts`,
    /// which is also what the repository's count gate reads off the query.
    #[test]
    fn a_type_filter_or_the_item_counts_field_requests_the_counts() {
        let plain = ByNameListQuery::default();
        assert!(!plain.should_include_item_types());
        assert!(
            !plain
                .base_query(None)
                .expect("query")
                .dto_options
                .contains_field(ItemFields::ItemCounts)
        );

        let typed = ByNameListQuery {
            include_item_types: Some("Movie".to_owned()),
            ..ByNameListQuery::default()
        };
        assert!(typed.should_include_item_types());
        let fields = typed
            .base_query(None)
            .expect("query")
            .dto_options
            .fields
            .clone();
        assert_eq!(fields, vec![ItemFields::ItemCounts]);

        let by_field = ByNameListQuery {
            fields: Some("Overview,ItemCounts".to_owned()),
            ..ByNameListQuery::default()
        };
        assert!(by_field.should_include_item_types());
        // Not added twice when both are present.
        let both = ByNameListQuery {
            include_item_types: Some("Movie".to_owned()),
            fields: Some("ItemCounts".to_owned()),
            ..ByNameListQuery::default()
        };
        assert_eq!(both.requested_fields(), vec![ItemFields::ItemCounts]);
    }

    /// The C# `outerQueryFilter` fields reach the query, `filters` lands on
    /// the tri-states (a contradictory pair is a `400`), and a malformed id
    /// list is a `400`.
    #[test]
    fn outer_filter_parameters_reach_the_query() {
        let query = ByNameListQuery {
            filters: Some("IsPlayed,IsFavorite,Likes".to_owned()),
            is_favorite: Some(false),
            genres: Some("Rock|Jazz".to_owned()),
            tags: Some("live".to_owned()),
            official_ratings: Some("PG|R".to_owned()),
            years: Some("1999, 2001".to_owned()),
            genre_ids: Some("00000000-0000-0000-0000-000000000001".to_owned()),
            studio_ids: Some("00000000000000000000000000000002".to_owned()),
            ..ByNameListQuery::default()
        };
        let internal = query.base_query(None).expect("query");
        assert_eq!(internal.is_played, Some(true));
        assert_eq!(
            internal.is_favorite,
            Some(true),
            "ApplyFilters runs after isFavorite"
        );
        assert_eq!(internal.is_liked, Some(true));
        assert_eq!(internal.genres, vec!["Rock", "Jazz"]);
        assert_eq!(internal.tags, vec!["live"]);
        assert_eq!(internal.official_ratings, vec!["PG", "R"]);
        assert_eq!(internal.years, vec![1999, 2001]);
        assert_eq!(internal.genre_ids, vec![uuid::Uuid::from_u128(1)]);
        assert_eq!(internal.studio_ids, vec![uuid::Uuid::from_u128(2)]);

        let contradictory = ByNameListQuery {
            filters: Some("IsPlayed,IsUnplayed".to_owned()),
            ..ByNameListQuery::default()
        };
        assert!(contradictory.base_query(None).is_err());
        let malformed = ByNameListQuery {
            genre_ids: Some("not-a-guid".to_owned()),
            ..ByNameListQuery::default()
        };
        assert!(malformed.base_query(None).is_err());
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
