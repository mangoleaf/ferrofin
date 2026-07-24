//! `MergeVersionsController` — bulk merge/split of duplicate video versions.
//!
//! Ports the `MergeVersions` plugin's library-wide operations onto the core
//! [`LibraryManager`](hermit_traits::library::LibraryManager) version-group seam
//! (`PrimaryVersionId` pointer) that already backs `POST /Videos/MergeVersions`:
//! - `POST /MergeVersions/MergeMovies` — merge every duplicate movie (by `Tmdb` id)
//! - `POST /MergeVersions/SplitMovies` — split every merged movie apart
//! - `POST /MergeVersions/MergeEpisodes` — merge every duplicate episode
//! - `POST /MergeVersions/SplitEpisodes` — split every merged episode apart
//!
//! The plugin's routes take no parameters: each scans the whole library. The
//! grouping/merge logic lives in the manager (see its `merge_all_*` /
//! `split_all_*` methods); these handlers are the thin `[Authorize]` seam and
//! return `204 No Content` on success.

use axum::http::StatusCode;
use axum::routing::post;
use axum::{Router, extract::State};

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// `POST /MergeVersions/MergeMovies` — merge every duplicate movie version.
///
/// Port of `MergeVersionsController.MergeMoviesRequest`: delegates to
/// [`LibraryManager::merge_all_movie_versions`](hermit_traits::library::LibraryManager::merge_all_movie_versions).
#[utoipa::path(
    post,
    path = "/MergeVersions/MergeMovies",
    responses(
        (status = 204, description = "Library scan and merge completed"),
        (status = 401, description = "Unauthorized")
    ),
    tag = "hermit"
)]
async fn merge_movies(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<StatusCode, ApiError> {
    state.library.merge_all_movie_versions().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /MergeVersions/SplitMovies` — split every merged movie apart.
///
/// Port of `MergeVersionsController.SplitMoviesRequest`: delegates to
/// [`LibraryManager::split_all_movie_versions`](hermit_traits::library::LibraryManager::split_all_movie_versions).
#[utoipa::path(
    post,
    path = "/MergeVersions/SplitMovies",
    responses(
        (status = 204, description = "Library scan and split completed"),
        (status = 401, description = "Unauthorized")
    ),
    tag = "hermit"
)]
async fn split_movies(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<StatusCode, ApiError> {
    state.library.split_all_movie_versions().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /MergeVersions/MergeEpisodes` — merge every duplicate episode version.
///
/// Port of `MergeVersionsController.MergeEpisodesRequestAsync`: delegates to
/// [`LibraryManager::merge_all_episode_versions`](hermit_traits::library::LibraryManager::merge_all_episode_versions).
#[utoipa::path(
    post,
    path = "/MergeVersions/MergeEpisodes",
    responses(
        (status = 204, description = "Library scan and merge completed"),
        (status = 401, description = "Unauthorized")
    ),
    tag = "hermit"
)]
async fn merge_episodes(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<StatusCode, ApiError> {
    state.library.merge_all_episode_versions().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /MergeVersions/SplitEpisodes` — split every merged episode apart.
///
/// Port of `MergeVersionsController.SplitEpisodesRequestAsync`: delegates to
/// [`LibraryManager::split_all_episode_versions`](hermit_traits::library::LibraryManager::split_all_episode_versions).
#[utoipa::path(
    post,
    path = "/MergeVersions/SplitEpisodes",
    responses(
        (status = 204, description = "Library scan and split completed"),
        (status = 401, description = "Unauthorized")
    ),
    tag = "hermit"
)]
async fn split_episodes(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<StatusCode, ApiError> {
    state.library.split_all_episode_versions().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/MergeVersions/MergeMovies", post(merge_movies))
        .route("/MergeVersions/SplitMovies", post(split_movies))
        .route("/MergeVersions/MergeEpisodes", post(merge_episodes))
        .route("/MergeVersions/SplitEpisodes", post(split_episodes))
}
