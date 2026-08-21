//! `ApiKeyController` — list, create, and revoke long-lived server API keys.
//!
//! Ports the elevation-gated `ApiKeyController` (`[Route("Auth")]`) routes:
//! - `GET /Auth/Keys` — every stored key as an [`AuthenticationInfo`].
//! - `POST /Auth/Keys` — create a key named by the `app` query parameter.
//! - `DELETE /Auth/Keys/{key}` — revoke the key with the given access token.
//!
//! Backing store note (the batch's open question): keys live in their own
//! `ApiKeys` table via the [`ApiKeyManager`](ferrofin_traits::security::ApiKeyManager)
//! trait — a dedicated key repository, matching Jellyfin's
//! `AuthenticationManager` over `dbContext.ApiKeys` (distinct from device-session
//! tokens issued by the session manager).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use ferrofin_model::querying::QueryResult;
use ferrofin_model::security::AuthenticationInfo;

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::state::AppState;

/// Query parameters for `POST /Auth/Keys` — the app name for the new key.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateKeyQuery {
    /// Name of the app using the authentication key.
    #[serde(default)]
    app: Option<String>,
}

/// `GET /Auth/Keys` — every stored API key.
///
/// Port of `ApiKeyController.GetKeys`: returns all keys wrapped in a
/// [`QueryResult`] (the C# `new QueryResult<AuthenticationInfo>(keys)`).
#[utoipa::path(
    get,
    path = "/Auth/Keys",
    responses((status = 200, description = "Api keys retrieved", body = QueryResult<AuthenticationInfo>)),
    tag = "ferrofin"
)]
async fn get_keys(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
) -> Result<Json<QueryResult<AuthenticationInfo>>, ApiError> {
    let keys = state.api_keys.get_api_keys().await?;
    Ok(Json(QueryResult::from_items(keys)))
}

/// `POST /Auth/Keys` — create a new API key.
///
/// Port of `ApiKeyController.CreateKey`: creates a key named by the required
/// `app` parameter and returns `204 No Content`.
#[utoipa::path(
    post,
    path = "/Auth/Keys",
    params(("app" = String, Query, description = "Name of the app using the authentication key")),
    responses((status = 204, description = "Api key created")),
    tag = "ferrofin"
)]
async fn create_key(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(query): Query<CreateKeyQuery>,
) -> Result<StatusCode, ApiError> {
    let app = match query.app.as_deref() {
        Some(app) if !app.is_empty() => app,
        _ => return Err(ApiError::BadRequest("missing required 'app'".to_owned())),
    };
    state.api_keys.create_api_key(app).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /Auth/Keys/{key}` — revoke an API key by its access token.
///
/// Port of `ApiKeyController.RevokeKey`: deletes the key with the given token
/// (a no-op when none matches) and returns `204 No Content`.
#[utoipa::path(
    delete,
    path = "/Auth/Keys/{key}",
    params(("key" = String, Path, description = "The access token to delete")),
    responses((status = 204, description = "Api key deleted")),
    tag = "ferrofin"
)]
async fn revoke_key(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(key): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.api_keys.delete_api_key(&key).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Auth/Keys", get(get_keys).post(create_key))
        .route("/Auth/Keys/{key}", delete(revoke_key))
}
