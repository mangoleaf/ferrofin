//! `QuickConnectController` — the Quick Connect pairing flow.
//!
//! Ports Jellyfin's `QuickConnectController`:
//!
//! - `GET  /QuickConnect/Enabled` — whether Quick Connect is active.
//! - `POST /QuickConnect/Initiate` — start a new pairing request.
//! - `GET  /QuickConnect/Connect` — poll a request's status by its secret.
//! - `POST /QuickConnect/Authorize` — authorize a pending request by its code.
//!
//! All four delegate to the [`QuickConnect`](ferrofin_traits::security::QuickConnect)
//! manager. `Initiate` reads the caller's parsed [`AuthorizationInfo`]; `Authorize`
//! is behind `[Authorize]` and targets `userId` (or the caller when omitted).

use axum::extract::{Query, State};
use axum::http::request::Parts;
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::quick_connect::QuickConnectResult;
use ferrofin_model::secret::Secret;
use ferrofin_traits::options::AuthorizationInfo;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// Query parameters for `GET /QuickConnect/Connect` (the request secret).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectQuery {
    /// The secret returned by the initiate endpoint.
    #[serde(default)]
    secret: Secret,
}

/// Query parameters for `POST /QuickConnect/Authorize` (code + optional user).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizeQuery {
    /// The user-facing code to authorize.
    #[serde(default)]
    code: String,
    /// The user to authorize as; defaults to the caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// Reads the client identity parsed by the auth-context middleware into this
/// request's [`AuthorizationInfo`] extension.
fn auth_info(parts: &Parts) -> AuthorizationInfo {
    parts
        .extensions
        .get::<AuthorizationInfo>()
        .cloned()
        .unwrap_or_default()
}

/// `GET /QuickConnect/Enabled` — whether Quick Connect is active.
///
/// Port of `QuickConnectController.GetQuickConnectEnabled`.
#[utoipa::path(
    get,
    path = "/QuickConnect/Enabled",
    responses((status = 200, description = "Quick Connect enabled state", body = bool)),
    tag = "ferrofin"
)]
async fn get_enabled(State(state): State<AppState>) -> Result<Json<bool>, ApiError> {
    Ok(Json(state.quick_connect.is_enabled().await?))
}

/// `POST /QuickConnect/Initiate` — start a new pairing request.
///
/// Port of `QuickConnectController.InitiateQuickConnect`: reads the caller's
/// parsed authorization info and opens a new request.
#[utoipa::path(
    post,
    path = "/QuickConnect/Initiate",
    responses(
        (status = 200, description = "Quick Connect request created", body = QuickConnectResult),
        (status = 401, description = "Quick Connect is disabled")
    ),
    tag = "ferrofin"
)]
async fn initiate(
    State(state): State<AppState>,
    parts: Parts,
) -> Result<Json<QuickConnectResult>, ApiError> {
    let auth = auth_info(&parts);
    Ok(Json(state.quick_connect.try_connect(&auth).await?))
}

/// `GET /QuickConnect/Connect` — poll a request's status by its secret.
///
/// Port of `QuickConnectController.GetQuickConnectState`.
#[utoipa::path(
    get,
    path = "/QuickConnect/Connect",
    params(("secret" = String, Query, description = "The request secret")),
    responses(
        (status = 200, description = "Quick Connect result returned", body = QuickConnectResult),
        (status = 404, description = "Unknown secret")
    ),
    tag = "ferrofin"
)]
async fn connect(
    State(state): State<AppState>,
    Query(query): Query<ConnectQuery>,
) -> Result<Json<QuickConnectResult>, ApiError> {
    Ok(Json(
        state
            .quick_connect
            .check_request_status(query.secret.expose())
            .await?,
    ))
}

/// `POST /QuickConnect/Authorize` — authorize a pending request by its code.
///
/// Port of `QuickConnectController.AuthorizeQuickConnect`: authorizes the request
/// for `userId` (or the caller when omitted).
#[utoipa::path(
    post,
    path = "/QuickConnect/Authorize",
    params(
        ("code" = String, Query, description = "The code to authorize"),
        ("userId" = Option<String>, Query, description = "The user to authorize as")
    ),
    responses(
        (status = 200, description = "Authorization result", body = bool),
        (status = 403, description = "Unknown user id")
    ),
    tag = "ferrofin"
)]
async fn authorize(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<AuthorizeQuery>,
) -> Result<Json<bool>, ApiError> {
    let user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    Ok(Json(
        state
            .quick_connect
            .authorize_request(user_id, &query.code)
            .await?,
    ))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/QuickConnect/Enabled", get(get_enabled))
        .route("/QuickConnect/Initiate", post(initiate))
        .route("/QuickConnect/Connect", get(connect))
        .route("/QuickConnect/Authorize", post(authorize))
}
