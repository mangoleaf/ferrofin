//! `MergeVersionsController` — bulk merge/split of duplicate video versions.
//!
//! The HTTP seam of the **Merge Versions extension**
//! (`ferrofin-extensions::merge_versions`, upstream
//! `jellyfin-plugin-mergeversions` 12.0). Everything else — the scans, the
//! eligibility filters, the config, the scheduled tasks, the settings page —
//! lives with the extension; these handlers are the thin `[Authorize]` seam
//! over the [`MergeVersionsManager`] trait:
//! - `POST /MergeVersions/MergeMovies` — merge every duplicate movie (by `Tmdb` id)
//! - `POST /MergeVersions/SplitMovies` — split every merged movie apart
//! - `POST /MergeVersions/MergeEpisodes` — merge every duplicate episode
//! - `POST /MergeVersions/SplitEpisodes` — split every merged episode apart
//!
//! The plugin's routes take no parameters (each scans the whole library) and
//! return `204 No Content`. While the plugin is disabled — or the extension is
//! not wired — they return `404`, the observable behavior of a Jellyfin server
//! whose disabled plugin's controller is not registered.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::routing::post;
use axum::{Router, extract::State};

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::merge_versions::MergeVersionsManager;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// Resolves the wired Merge Versions service, or the `404` an absent plugin
/// controller produces.
fn service(state: &AppState) -> Result<Arc<dyn MergeVersionsManager>, ApiError> {
    state
        .merge_versions
        .clone()
        .ok_or_else(|| ServiceError::not_found("the Merge Versions plugin is not available").into())
}

/// `POST /MergeVersions/MergeMovies` — merge every duplicate movie version.
///
/// Port of `MergeVersionsController.MergeMoviesRequest`: delegates to
/// [`MergeVersionsManager::merge_movies`].
#[utoipa::path(
    post,
    path = "/MergeVersions/MergeMovies",
    responses(
        (status = 204, description = "Library scan and merge completed"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Plugin disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn merge_movies(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<StatusCode, ApiError> {
    service(&state)?.merge_movies(None).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /MergeVersions/SplitMovies` — split every merged movie apart.
///
/// Port of `MergeVersionsController.SplitMoviesRequest`: delegates to
/// [`MergeVersionsManager::split_movies`].
#[utoipa::path(
    post,
    path = "/MergeVersions/SplitMovies",
    responses(
        (status = 204, description = "Library scan and split completed"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Plugin disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn split_movies(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<StatusCode, ApiError> {
    service(&state)?.split_movies(None).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /MergeVersions/MergeEpisodes` — merge every duplicate episode version.
///
/// Port of `MergeVersionsController.MergeEpisodesRequestAsync`: delegates to
/// [`MergeVersionsManager::merge_episodes`].
#[utoipa::path(
    post,
    path = "/MergeVersions/MergeEpisodes",
    responses(
        (status = 204, description = "Library scan and merge completed"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Plugin disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn merge_episodes(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<StatusCode, ApiError> {
    service(&state)?.merge_episodes(None).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /MergeVersions/SplitEpisodes` — split every merged episode apart.
///
/// Port of `MergeVersionsController.SplitEpisodesRequestAsync`: delegates to
/// [`MergeVersionsManager::split_episodes`].
#[utoipa::path(
    post,
    path = "/MergeVersions/SplitEpisodes",
    responses(
        (status = 204, description = "Library scan and split completed"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Plugin disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn split_episodes(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<StatusCode, ApiError> {
    service(&state)?.split_episodes(None).await?;
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
