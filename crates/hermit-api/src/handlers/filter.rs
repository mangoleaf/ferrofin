//! `FilterController` — the query-filter facets under `/Items/Filters*`.
//!
//! Ports:
//!
//! - `GET /Items/Filters`  — the legacy flat-string facets ([`QueryFiltersLegacy`]:
//!   genres, tags, official ratings, years) aggregated over a parent's items.
//! - `GET /Items/Filters2` — the richer facets ([`QueryFilters`]: genre
//!   name/id pairs plus audio/subtitle languages) for a query.
//!
//! Both endpoints scope to an optional parent and honor the `includeItemTypes`
//! restriction (a lone `Trailer`/`Program` type skips the parent lookup, exactly
//! as Jellyfin does). The genre facet dispatches to the music-genre aggregate for
//! a music-only type set and to the plain-genre aggregate otherwise; the language
//! facets read the matching items' media-stream languages. The localization
//! embellishment of a language's display name (Jellyfin's
//! `ILocalizationManager.FindLanguageInfo`) is not applied here — the localization
//! manager is not part of `AppState` — so a language's `Name` is its code.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use hermit_model::data::{BaseItemKind, MediaType};
use hermit_model::dto::{NameGuidPair, NameValuePair};
use hermit_model::entities::MediaStreamType;
use hermit_model::querying::{QueryFilters, QueryFiltersLegacy};
use hermit_traits::options::InternalItemsQuery;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user_opt;
use crate::handlers::query_parse::parse_csv_enums;
use crate::state::AppState;

/// The query parameters honoured by `GET /Items/Filters`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FiltersLegacyQuery {
    /// The target user; scopes visibility when present.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// Localizes the aggregation to a specific parent item/folder.
    #[serde(default)]
    parent_id: Option<Uuid>,
    /// Comma-delimited [`BaseItemKind`] set to include.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Comma-delimited [`MediaType`] set to include.
    #[serde(default)]
    media_types: Option<String>,
}

/// Whether the (single) included type is `Trailer` or `Program`, which Jellyfin
/// treats specially by *not* resolving a parent item.
fn is_trailer_or_program(types: &[BaseItemKind]) -> bool {
    types.len() == 1 && matches!(types[0], BaseItemKind::Trailer | BaseItemKind::Program)
}

/// `GET /Items/Filters` — the legacy flat-string filter facets.
///
/// Port of `FilterController.GetQueryFiltersLegacy`. The facets are aggregated
/// from the matching items' distinct genre/tag/rating/year values.
#[utoipa::path(
    get,
    path = "/Items/Filters",
    responses((status = 200, description = "Legacy filters returned", body = QueryFiltersLegacy)),
    tag = "hermit"
)]
async fn get_query_filters_legacy(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<FiltersLegacyQuery>,
) -> Result<Json<QueryFiltersLegacy>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let include_item_types: Vec<BaseItemKind> =
        parse_csv_enums(query.include_item_types.as_deref())?;
    let media_types: Vec<MediaType> = parse_csv_enums(query.media_types.as_deref())?;

    // A lone Trailer/Program type set skips the parent lookup; otherwise a parent
    // that is not a folder yields empty filters (the C# `is not Folder` guard).
    // Without the OOP folder tree we always aggregate over the parent's subtree.
    let mut ancestor_ids = Vec::new();
    if !is_trailer_or_program(&include_item_types)
        && let Some(parent) = query.parent_id
    {
        ancestor_ids.push(parent);
    }

    let internal = InternalItemsQuery {
        user,
        media_types,
        include_item_types,
        recursive: true,
        enable_total_record_count: false,
        ancestor_ids,
        ..InternalItemsQuery::default()
    };
    let filters = state.library.get_query_filters_legacy(&internal).await?;
    Ok(Json(filters))
}

/// The query parameters honoured by `GET /Items/Filters2`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
struct FiltersQuery {
    /// The target user; scopes visibility when present.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// Localizes the aggregation to a specific parent item/folder.
    #[serde(default)]
    parent_id: Option<Uuid>,
    /// Comma-delimited [`BaseItemKind`] set to include.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Optional live-tv "is airing" filter.
    #[serde(default)]
    is_airing: Option<bool>,
    /// Optional live-tv "is movie" filter.
    #[serde(default)]
    is_movie: Option<bool>,
    /// Optional live-tv "is sports" filter.
    #[serde(default)]
    is_sports: Option<bool>,
    /// Optional live-tv "is kids" filter.
    #[serde(default)]
    is_kids: Option<bool>,
    /// Optional live-tv "is news" filter.
    #[serde(default)]
    is_news: Option<bool>,
    /// Optional live-tv "is series" filter.
    #[serde(default)]
    is_series: Option<bool>,
    /// Whether to aggregate recursively (defaults `true`).
    #[serde(default)]
    recursive: Option<bool>,
}

/// Whether the type set is one of the four music kinds Jellyfin routes to the
/// music-genre aggregate.
fn is_music_type_set(types: &[BaseItemKind]) -> bool {
    types.len() == 1
        && matches!(
            types[0],
            BaseItemKind::MusicAlbum
                | BaseItemKind::MusicVideo
                | BaseItemKind::MusicArtist
                | BaseItemKind::Audio
        )
}

/// Whether the type set contains any of the video kinds that carry
/// audio/subtitle language facets (Movie/Series/Season/Episode).
fn has_language_facets(types: &[BaseItemKind]) -> bool {
    types.iter().any(|t| {
        matches!(
            t,
            BaseItemKind::Movie
                | BaseItemKind::Series
                | BaseItemKind::Season
                | BaseItemKind::Episode
        )
    })
}

/// Maps distinct language codes to sorted [`NameValuePair`]s. Jellyfin embellishes
/// the name via localization; without the localization manager the name is the
/// code itself.
fn language_pairs(mut languages: Vec<String>) -> Vec<NameValuePair> {
    languages.sort_unstable();
    languages
        .into_iter()
        .map(|language| NameValuePair::new(language.clone(), language))
        .collect()
}

/// `GET /Items/Filters2` — the richer genre + language filter facets.
///
/// Port of `FilterController.GetQueryFilters`. The genre facet routes to the
/// music-genre aggregate for a music-only type set, and the audio/subtitle
/// language facets are read for a video type set.
#[utoipa::path(
    get,
    path = "/Items/Filters2",
    responses((status = 200, description = "Filters returned", body = QueryFilters)),
    tag = "hermit"
)]
async fn get_query_filters(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<FiltersQuery>,
) -> Result<Json<QueryFilters>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let include_item_types: Vec<BaseItemKind> =
        parse_csv_enums(query.include_item_types.as_deref())?;

    // Trailer/Program skip the parent; otherwise the parent scopes the aggregate.
    let mut ancestor_ids = Vec::new();
    if !is_trailer_or_program(&include_item_types)
        && (query.recursive.unwrap_or(true))
        && let Some(parent) = query.parent_id
    {
        ancestor_ids.push(parent);
    }

    let base = InternalItemsQuery {
        user: user.clone(),
        include_item_types: include_item_types.clone(),
        is_airing: query.is_airing,
        is_movie: query.is_movie,
        is_sports: query.is_sports,
        is_kids: query.is_kids,
        is_news: query.is_news,
        is_series: query.is_series,
        ancestor_ids: ancestor_ids.clone(),
        ..InternalItemsQuery::default()
    };

    let mut filters = QueryFilters::default();

    // Genre facet: music-genre aggregate for a music-only type set, else genres.
    let genre_result = if is_music_type_set(&include_item_types) {
        state.library.get_music_genres(&base).await?
    } else {
        state.library.get_genres(&base).await?
    };
    filters.genres = genre_result
        .items
        .into_iter()
        .map(|iwc| NameGuidPair {
            name: iwc.item.name.clone(),
            id: Uuid::parse_str(&iwc.item.id).unwrap_or_else(|_| Uuid::nil()),
        })
        .collect();

    // Language facets apply only to the video type set. Streams join on episodes,
    // so a Series/Season set is widened to include Episode (as in C#), and owned
    // items are included since alternative versions may carry other languages.
    if has_language_facets(&include_item_types) {
        let mut language_types = include_item_types.clone();
        if (language_types.contains(&BaseItemKind::Series)
            || language_types.contains(&BaseItemKind::Season))
            && !language_types.contains(&BaseItemKind::Episode)
        {
            language_types.push(BaseItemKind::Episode);
        }
        let language_query = InternalItemsQuery {
            include_owned_items: true,
            include_item_types: language_types,
            ..base.clone()
        };
        filters.audio_languages = language_pairs(
            state
                .library
                .get_media_stream_languages(MediaStreamType::Audio, &language_query)
                .await?,
        );
        filters.subtitle_languages = language_pairs(
            state
                .library
                .get_media_stream_languages(MediaStreamType::Subtitle, &language_query)
                .await?,
        );
    }

    Ok(Json(filters))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Items/Filters", get(get_query_filters_legacy))
        .route("/Items/Filters2", get(get_query_filters))
}
