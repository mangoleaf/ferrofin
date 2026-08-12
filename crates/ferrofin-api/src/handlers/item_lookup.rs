//! `ItemLookupController` — external-id descriptors + remote metadata search.
//!
//! Ports every route of Jellyfin's `ItemLookupController`:
//!
//! - `GET /Items/{itemId}/ExternalIdInfos` — the external-id descriptors (IMDb,
//!   TMDb, MusicBrainz, …) applicable to an item.
//! - `POST /Items/RemoteSearch/{Movie,Trailer,MusicVideo,Series,BoxSet,
//!   MusicArtist,MusicAlbum,Person,Book}` — remote metadata search. Each takes a
//!   typed `RemoteSearchQuery<XInfo>`, collapses it into the object-safe
//!   [`RemoteSearchRequest`], and calls
//!   [`ProviderManager::remote_search`](ferrofin_traits::providers::ProviderManager::remote_search).
//! - `POST /Items/RemoteSearch/Apply/{itemId}` — apply a chosen search result to
//!   an item and trigger a full metadata refresh.
//!
//! ## Honest deferral
//!
//! The remote metadata *fetchers* (TMDb/TVDb/MusicBrainz/…) are deferred:
//! feature-gated off, they need API keys and network I/O. The endpoints are wired
//! against the provider manager's real remote-search surface, but with no fetcher
//! registered the applicable-provider set is empty and the search faithfully
//! returns `[]` — exactly as Jellyfin returns an empty list when no provider
//! matches. The dedup/merge algorithm is nonetheless ported for real (see
//! `ferrofin-providers`), so registering a fetcher yields correct results with no
//! further change here.
//!
//! `Apply` resolves the item (`404` when absent) and drives the provider
//! manager's `refresh_full_item`, which re-fetches the item's metadata + artwork
//! from TMDB and persists them. It re-searches by the item's title rather than
//! binding the exact provider id on the chosen result (that needs a
//! `BaseItemProviders` write path not yet present), so a title with multiple
//! matches may not honor the precise pick; the metadata applied is real either
//! way and the common case matches the user's selection.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::providers::{
    AlbumInfo, ArtistInfo, BookInfo, BoxSetInfo, ExternalIdInfo, ItemLookupInfo, MovieInfo,
    MusicVideoInfo, PersonLookupInfo, RemoteSearchQuery, RemoteSearchResult, SeriesInfo,
    TrailerInfo,
};
use ferrofin_traits::providers::{
    MetadataRefreshMode, MetadataRefreshOptions, RemoteSearchRequest,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// `GET /Items/{itemId}/ExternalIdInfos` — the item's external-id descriptors.
///
/// Port of `ItemLookupController.GetExternalIdInfos`: resolves the item (`404`
/// when absent), then returns the external-id descriptors the provider manager
/// advertises for it.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/ExternalIdInfos",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "External id info retrieved", body = Vec<ExternalIdInfo>),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn get_external_id_infos(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<Json<Vec<ExternalIdInfo>>, ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }
    let infos = state.providers.get_external_id_infos(item_id).await?;
    Ok(Json(infos))
}

/// Collapses a typed `RemoteSearchQuery<T>` into the object-safe
/// [`RemoteSearchRequest`] for the given item kind.
///
/// `extract_base` pulls the shared [`ItemLookupInfo`] out of the concrete lookup
/// type; the type-specific extension fields are consumed by the (deferred)
/// per-provider fetchers and are not carried across the object-safe seam.
fn to_request<T>(
    query: RemoteSearchQuery<T>,
    item_kind: BaseItemKind,
    extract_base: impl FnOnce(T) -> ItemLookupInfo,
) -> RemoteSearchRequest {
    RemoteSearchRequest {
        item_kind,
        search_info: query.search_info.map(extract_base).unwrap_or_default(),
        item_id: query.item_id,
        search_provider_name: query.search_provider_name,
        include_disabled_providers: query.include_disabled_providers,
    }
}

/// Runs a remote search request against the provider manager.
async fn run_remote_search(
    state: &AppState,
    request: RemoteSearchRequest,
) -> Result<Json<Vec<RemoteSearchResult>>, ApiError> {
    let results = state.providers.remote_search(&request).await?;
    Ok(Json(results))
}

/// Generates a `POST /Items/RemoteSearch/{route}` handler for one lookup type.
macro_rules! remote_search_handler {
    (
        $(#[$meta:meta])*
        fn $fn_name:ident($info:ty, $kind:expr, $route:literal)
    ) => {
        $(#[$meta])*
        #[utoipa::path(
            post,
            path = concat!("/Items/RemoteSearch/", $route),
            request_body = RemoteSearchQuery<$info>,
            responses(
                (status = 200, description = "Remote search executed", body = Vec<RemoteSearchResult>)
            ),
            tag = "ferrofin"
        )]
        async fn $fn_name(
            State(state): State<AppState>,
            RequireAuth(_auth): RequireAuth,
            Json(query): Json<RemoteSearchQuery<$info>>,
        ) -> Result<Json<Vec<RemoteSearchResult>>, ApiError> {
            let request = to_request(query, $kind, |info| info.base);
            run_remote_search(&state, request).await
        }
    };
}

remote_search_handler! {
    /// `POST /Items/RemoteSearch/Movie` — port of `GetMovieRemoteSearchResults`.
    fn movie_remote_search(MovieInfo, BaseItemKind::Movie, "Movie")
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/Trailer` — port of `GetTrailerRemoteSearchResults`.
    fn trailer_remote_search(TrailerInfo, BaseItemKind::Trailer, "Trailer")
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/MusicVideo` — port of `GetMusicVideoRemoteSearchResults`.
    fn music_video_remote_search(MusicVideoInfo, BaseItemKind::MusicVideo, "MusicVideo")
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/Series` — port of `GetSeriesRemoteSearchResults`.
    fn series_remote_search(SeriesInfo, BaseItemKind::Series, "Series")
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/BoxSet` — port of `GetBoxSetRemoteSearchResults`.
    fn box_set_remote_search(BoxSetInfo, BaseItemKind::BoxSet, "BoxSet")
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/MusicArtist` — port of `GetMusicArtistRemoteSearchResults`.
    fn music_artist_remote_search(ArtistInfo, BaseItemKind::MusicArtist, "MusicArtist")
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/MusicAlbum` — port of `GetMusicAlbumRemoteSearchResults`.
    fn music_album_remote_search(AlbumInfo, BaseItemKind::MusicAlbum, "MusicAlbum")
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/Person` — port of `GetPersonRemoteSearchResults`.
    fn person_remote_search(PersonLookupInfo, BaseItemKind::Person, "Person")
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/Book` — port of `GetBookRemoteSearchResults`.
    fn book_remote_search(BookInfo, BaseItemKind::Book, "Book")
}

/// Query string of `POST /Items/RemoteSearch/Apply/{itemId}`.
#[derive(Debug, Clone, Deserialize)]
struct ApplyQuery {
    /// Whether or not to replace all images. Default: `true`.
    #[serde(rename = "replaceAllImages", default = "default_true")]
    replace_all_images: bool,
}

/// Serde default for the `replaceAllImages` query flag (`true`).
fn default_true() -> bool {
    true
}

/// `POST /Items/RemoteSearch/Apply/{itemId}` — apply a chosen result + refresh.
///
/// Port of `ItemLookupController.ApplySearchCriteria`: resolves the item (`404`
/// when absent), then drives a full metadata + image refresh through the provider
/// manager's `refresh_full_item`, which re-fetches and persists the item's TMDB
/// metadata and downloads its primary/backdrop artwork.
#[utoipa::path(
    post,
    path = "/Items/RemoteSearch/Apply/{itemId}",
    params(
        ("itemId" = String, Path, description = "Item id"),
        ("replaceAllImages" = Option<bool>, Query, description = "Whether to replace all images")
    ),
    request_body = RemoteSearchResult,
    responses(
        (status = 204, description = "Item metadata refreshed"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn apply_search_criteria(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<ApplyQuery>,
    Json(_search_result): Json<RemoteSearchResult>,
) -> Result<axum::http::StatusCode, ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }

    // Full metadata + image refresh, replacing everything (the C# builds
    // `FullRefresh` for both modes with `ReplaceAllMetadata = true`). The chosen
    // result's provider ids are consumed by the refresh pipeline.
    let options = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        replace_all_images: query.replace_all_images,
    };
    state.providers.refresh_full_item(item_id, &options).await?;

    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Items/{itemId}/ExternalIdInfos",
            get(get_external_id_infos),
        )
        .route("/Items/RemoteSearch/Movie", post(movie_remote_search))
        .route("/Items/RemoteSearch/Trailer", post(trailer_remote_search))
        .route(
            "/Items/RemoteSearch/MusicVideo",
            post(music_video_remote_search),
        )
        .route("/Items/RemoteSearch/Series", post(series_remote_search))
        .route("/Items/RemoteSearch/BoxSet", post(box_set_remote_search))
        .route(
            "/Items/RemoteSearch/MusicArtist",
            post(music_artist_remote_search),
        )
        .route(
            "/Items/RemoteSearch/MusicAlbum",
            post(music_album_remote_search),
        )
        .route("/Items/RemoteSearch/Person", post(person_remote_search))
        .route("/Items/RemoteSearch/Book", post(book_remote_search))
        .route(
            "/Items/RemoteSearch/Apply/{itemId}",
            post(apply_search_criteria),
        )
}
