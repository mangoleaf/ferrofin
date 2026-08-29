//! `LibraryController` similar-items surface (the non-`/Shows` aliases).
//!
//! Ports Jellyfin's `LibraryController.GetSimilarItems`, which serves one handler
//! body across six routes: `Albums`, `Artists`, `Items`, `Movies`, `Trailers`,
//! and `Shows`. The `Shows` alias already lives in [`super::tv_shows`]; this
//! module registers the other five, all delegating to the same
//! [`SimilarItemsManager`](ferrofin_traits::library::SimilarItemsManager) seam.
//!
//! The C# body resolves the seed item (a nil id falls back to the root folder),
//! short-circuits `Episode`/by-name-non-artist seeds to an empty result — the
//! manager's `get_similar_items` applies that guard (`kinds::supports_similarity`)
//! before any provider runs — builds `DtoOptions` from `fields`, asks the
//! similar-items manager for the ranked rows, and returns a
//! `QueryResult<BaseItemDto>` with `startIndex = 0` and `totalRecordCount` equal
//! to the number of rows. The transform is identical for every alias — only the
//! route path differs — so one handler backs all five.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::DtoOptions;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user_opt;
use crate::handlers::query_parse::{parse_csv_enums_lenient, parse_csv_uuids};
use crate::state::AppState;

/// The query parameters honoured by `GET /{kind}/{itemId}/Similar`.
///
/// Mirrors the `GetSimilarItems` C# signature: `excludeArtistIds` (comma-delimited
/// GUIDs), the optional target `userId`, a result `limit`, and the comma-delimited
/// `fields` list that shapes the returned DTOs.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimilarParams {
    /// Comma-delimited artist ids to exclude from the results.
    #[serde(default)]
    exclude_artist_ids: Option<String>,
    /// The target user; scopes visibility and attaches user data when present.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Comma-delimited additional [`ItemFields`](ferrofin_model::querying::ItemFields).
    #[serde(default)]
    fields: Option<String>,
}

/// Resolves the seed id the C# controller would use.
///
/// Port of the `LibraryController.GetSimilarItems` first two lines: a nil
/// `itemId` is not an error but a fallback to the root folder
/// (`itemId.IsEmpty() ? (user is null ? RootFolder : GetUserRootFolder()) :
/// GetItemById(itemId, user)`). A non-nil id passes through untouched and its
/// existence is checked by the manager, which reports the miss so the handler
/// can `404`.
///
/// C#'s two roots (`AggregateFolder` without a user, `UserRootFolder` with one)
/// collapse to one here: neither is a kind any similarity provider serves, so
/// both answer with the same empty `QueryResult`, and Ferrofin only persists the
/// user root. `Ok(None)` — a nil id on a server with no root row at all — takes
/// that same empty answer.
pub(crate) async fn resolve_similar_seed(
    state: &AppState,
    item_id: Uuid,
) -> Result<Option<Uuid>, ApiError> {
    if !item_id.is_nil() {
        return Ok(Some(item_id));
    }
    let root = state.library.get_user_root_folder().await?;
    Ok(root.and_then(|row| Uuid::parse_str(&row.id).ok()))
}

/// Shared body of every `/{kind}/{itemId}/Similar` route.
///
/// Port of `LibraryController.GetSimilarItems`. Resolves the (optional) user,
/// parses `excludeArtistIds`/`fields`, and delegates to the
/// [`SimilarItemsManager`](ferrofin_traits::library::SimilarItemsManager), whose
/// `get_similar_items` answers empty for an `Episode` or a by-name seed other
/// than a `MusicArtist` — the C# `if (item is Episode || (item is IItemByName
/// && item is not MusicArtist)) return new QueryResult()` guard — and otherwise
/// runs the providers with the user's access, the per-kind played filter and
/// `excludeArtistIds` (honoured by the music providers only, as in C#).
/// Returns the ranked rows projected to [`BaseItemDto`] with `startIndex = 0`.
async fn similar_items(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    item_id: Uuid,
    query: SimilarParams,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(state, auth, query.user_id).await?;
    let user_id = user.as_ref().and_then(|u| Uuid::parse_str(&u.id).ok());
    let exclude_artist_ids = parse_csv_uuids(query.exclude_artist_ids.as_deref())?;
    let options = DtoOptions {
        // Lenient: clients send deprecated ItemFields; Jellyfin drops unknowns.
        fields: parse_csv_enums_lenient(query.fields.as_deref()),
        ..DtoOptions::default()
    };

    // C# resolves the seed BEFORE any provider runs: a nil id is the documented
    // root-folder fallback (`itemId.IsEmpty() ? (user is null ? RootFolder :
    // GetUserRootFolder()) : GetItemById(itemId, user)`), and a seed that does
    // not resolve is a `404`, not an empty page.
    let Some(seed_id) = resolve_similar_seed(state, item_id).await? else {
        return Ok(Json(QueryResult::new(Some(0), Some(0), Vec::new())));
    };
    let items = state
        .similar_items
        .get_similar_items(seed_id, &exclude_artist_ids, user_id, &options, query.limit)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let total = i32::try_from(items.len()).unwrap_or(i32::MAX);
    let dtos = state
        .dto
        .get_base_item_dtos(&items, &options, user.as_ref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(Some(0), Some(total), dtos)))
}

/// Builds a similar-items handler for one `/{kind}/{itemId}/Similar` route.
///
/// Each alias (`Albums`, `Artists`, `Items`, `Movies`, `Trailers`) has an
/// identical handler; this macro emits one per route so they share the single
/// [`similar_items`] body instead of copying it, matching the C# multi-`[HttpGet]`
/// fan-in onto one method.
macro_rules! similar_handler {
    ($name:ident, $path:literal) => {
        #[doc = concat!("`GET ", $path, "` — items similar to the seed.\n\nPort of the `", $path, "` alias of `LibraryController.GetSimilarItems`.")]
        #[utoipa::path(
            get,
            path = $path,
            params(("itemId" = String, Path, description = "The item id")),
            responses(
                (status = 200, description = "Similar items returned (QueryResult<BaseItemDto>)"),
                (status = 404, description = "Item not found")
            ),
            tag = "ferrofin"
        )]
        async fn $name(
            State(state): State<AppState>,
            RequireAuth(auth): RequireAuth,
            Path(item_id): Path<Uuid>,
            Query(query): Query<SimilarParams>,
        ) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
            similar_items(&state, &auth, item_id, query).await
        }
    };
}

similar_handler!(get_similar_albums, "/Albums/{itemId}/Similar");
similar_handler!(get_similar_artists, "/Artists/{itemId}/Similar");
similar_handler!(get_similar_generic_items, "/Items/{itemId}/Similar");
similar_handler!(get_similar_movies, "/Movies/{itemId}/Similar");
similar_handler!(get_similar_trailers, "/Trailers/{itemId}/Similar");

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Albums/{itemId}/Similar", get(get_similar_albums))
        .route("/Artists/{itemId}/Similar", get(get_similar_artists))
        .route("/Items/{itemId}/Similar", get(get_similar_generic_items))
        .route("/Movies/{itemId}/Similar", get(get_similar_movies))
        .route("/Trailers/{itemId}/Similar", get(get_similar_trailers))
}
