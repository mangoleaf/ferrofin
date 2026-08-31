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
//! The searches fan out over the registered fetchers (TMDB for movies/series/
//! box sets/people, TVDB for series, OMDb for movies/series/trailers,
//! MusicBrainz + TheAudioDb for albums/artists); the type-specific lookup
//! fields (album artists, song infos, …) ride along on the request. `Apply`
//! resolves the item (`404` when absent), replaces its provider ids with the
//! chosen result's and refreshes against that exact record.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::providers::{
    AlbumInfo, ArtistInfo, BookInfo, BoxSetInfo, ExternalIdInfo, ItemLookupInfo, MovieInfo,
    MusicVideoInfo, PersonLookupInfo, RemoteSearchQuery, RemoteSearchResult, SeriesInfo, SongInfo,
    TrailerInfo,
};
use ferrofin_traits::providers::{
    MetadataRefreshMode, MetadataRefreshOptions, RemoteSearchRequest,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{RequireAdmin, RequireAuth};
use crate::error::ApiError;
use crate::extract::JsonBody;
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
    RequireAdmin(_auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
) -> Result<Json<Vec<ExternalIdInfo>>, ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }
    let infos = state.providers.get_external_id_infos(item_id).await?;
    Ok(Json(infos))
}

/// The type-specific lookup fields a concrete `*Info` carries beyond the
/// shared [`ItemLookupInfo`] base — what each C# `GetSearchResults(XInfo)`
/// overload reads past the base class.
#[derive(Default)]
struct LookupExtras {
    album_artists: Vec<String>,
    artist_provider_ids: Option<std::collections::HashMap<String, String>>,
    song_infos: Vec<SongInfo>,
    artists: Vec<String>,
    series_name: Option<String>,
}

/// Splits a concrete lookup type into its shared base + its extension fields.
trait IntoLookup {
    fn into_lookup(self) -> (ItemLookupInfo, LookupExtras);
}

/// `IntoLookup` for the lookup types that add nothing beyond the base.
macro_rules! base_only_lookup {
    ($($ty:ty),* $(,)?) => {$(
        impl IntoLookup for $ty {
            fn into_lookup(self) -> (ItemLookupInfo, LookupExtras) {
                (self.base, LookupExtras::default())
            }
        }
    )*};
}
base_only_lookup!(
    MovieInfo,
    TrailerInfo,
    SeriesInfo,
    BoxSetInfo,
    PersonLookupInfo
);

impl IntoLookup for MusicVideoInfo {
    fn into_lookup(self) -> (ItemLookupInfo, LookupExtras) {
        (
            self.base,
            LookupExtras {
                artists: self.artists,
                ..LookupExtras::default()
            },
        )
    }
}

impl IntoLookup for BookInfo {
    fn into_lookup(self) -> (ItemLookupInfo, LookupExtras) {
        (
            self.base,
            LookupExtras {
                series_name: self.series_name,
                ..LookupExtras::default()
            },
        )
    }
}

impl IntoLookup for AlbumInfo {
    fn into_lookup(self) -> (ItemLookupInfo, LookupExtras) {
        (
            self.base,
            LookupExtras {
                album_artists: self.album_artists,
                artist_provider_ids: self.artist_provider_ids,
                song_infos: self.song_infos,
                ..LookupExtras::default()
            },
        )
    }
}

impl IntoLookup for ArtistInfo {
    fn into_lookup(self) -> (ItemLookupInfo, LookupExtras) {
        (
            self.base,
            LookupExtras {
                song_infos: self.song_infos,
                ..LookupExtras::default()
            },
        )
    }
}

/// Collapses a typed `RemoteSearchQuery<T>` into the object-safe
/// [`RemoteSearchRequest`] for the given item kind: the shared
/// [`ItemLookupInfo`] plus the type-specific extension fields the per-kind
/// fetchers read (album artists / artist provider ids / song infos for
/// MusicBrainz, music-video artists, a book's series name).
fn to_request<T: IntoLookup>(
    query: RemoteSearchQuery<T>,
    item_kind: BaseItemKind,
) -> RemoteSearchRequest {
    let (search_info, extras) = query
        .search_info
        .map(IntoLookup::into_lookup)
        .unwrap_or_default();
    RemoteSearchRequest {
        item_kind,
        search_info,
        item_id: query.item_id,
        search_provider_name: query.search_provider_name,
        include_disabled_providers: query.include_disabled_providers,
        album_artists: extras.album_artists,
        artist_provider_ids: extras.artist_provider_ids,
        song_infos: extras.song_infos,
        artists: extras.artists,
        series_name: extras.series_name,
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
    // `$auth` is the extractor guarding the generated route. Upstream gates
    // ONLY `RemoteSearch/Person` with `RequiresElevation` and leaves the other
    // nine remote searches on plain `[Authorize]` — asymmetric, but it is what
    // `ItemLookupController` does at v10.11.8, so it is what we do.
    (
        $(#[$meta:meta])*
        fn $fn_name:ident($info:ty, $kind:expr, $route:literal, $auth:ty)
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
            _auth: $auth,
            JsonBody(query): JsonBody<RemoteSearchQuery<$info>>,
        ) -> Result<Json<Vec<RemoteSearchResult>>, ApiError> {
            let request = to_request(query, $kind);
            run_remote_search(&state, request).await
        }
    };
}

remote_search_handler! {
    /// `POST /Items/RemoteSearch/Movie` — port of `GetMovieRemoteSearchResults`.
    fn movie_remote_search(MovieInfo, BaseItemKind::Movie, "Movie", RequireAuth)
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/Trailer` — port of `GetTrailerRemoteSearchResults`.
    fn trailer_remote_search(TrailerInfo, BaseItemKind::Trailer, "Trailer", RequireAuth)
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/MusicVideo` — port of `GetMusicVideoRemoteSearchResults`.
    fn music_video_remote_search(MusicVideoInfo, BaseItemKind::MusicVideo, "MusicVideo", RequireAuth)
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/Series` — port of `GetSeriesRemoteSearchResults`.
    fn series_remote_search(SeriesInfo, BaseItemKind::Series, "Series", RequireAuth)
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/BoxSet` — port of `GetBoxSetRemoteSearchResults`.
    fn box_set_remote_search(BoxSetInfo, BaseItemKind::BoxSet, "BoxSet", RequireAuth)
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/MusicArtist` — port of `GetMusicArtistRemoteSearchResults`.
    fn music_artist_remote_search(ArtistInfo, BaseItemKind::MusicArtist, "MusicArtist", RequireAuth)
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/MusicAlbum` — port of `GetMusicAlbumRemoteSearchResults`.
    fn music_album_remote_search(AlbumInfo, BaseItemKind::MusicAlbum, "MusicAlbum", RequireAuth)
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/Person` — port of `GetPersonRemoteSearchResults`.
    fn person_remote_search(PersonLookupInfo, BaseItemKind::Person, "Person", RequireAdmin)
}
remote_search_handler! {
    /// `POST /Items/RemoteSearch/Book` — port of `GetBookRemoteSearchResults`.
    fn book_remote_search(BookInfo, BaseItemKind::Book, "Book", RequireAuth)
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
/// when absent), then drives a `FullRefresh` (`ReplaceAllMetadata = true`,
/// `ReplaceAllImages = <query>`) carrying the chosen result — the provider
/// manager replaces the item's provider ids with the result's
/// (`item.ProviderIds = searchResult.ProviderIds`) and fetches against that
/// exact record rather than re-searching by title.
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
    RequireAdmin(_auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
    Query(query): Query<ApplyQuery>,
    JsonBody(search_result): JsonBody<RemoteSearchResult>,
) -> Result<axum::http::StatusCode, ApiError> {
    let Some(item) = state.library.get_item_by_id(item_id).await? else {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    };
    tracing::info!(
        %item_id,
        item_name = item.name.as_deref().unwrap_or_default(),
        provider_ids = ?search_result.provider_ids,
        "setting provider ids from the chosen search result"
    );

    // Full metadata + image refresh, replacing everything (the C# builds
    // `FullRefresh` for both modes with `ReplaceAllMetadata = true`), bound to
    // the chosen result (`SearchResult = searchResult`), and — the flag only
    // this endpoint sets — `RemoveOldMetadata = true`, so the previously
    // identified record's fields do not survive under the new one.
    let options = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        replace_all_images: query.replace_all_images,
        search_result: Some(search_result),
        remove_old_metadata: true,
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
