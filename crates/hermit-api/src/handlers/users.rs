//! `UserController` — authentication and the current-user endpoint.
//!
//! Ports two First-Light actions:
//!
//! - `POST /Users/AuthenticateByName` — authenticates by username + password,
//!   opening a new session via the
//!   [`SessionManager`](hermit_traits::session::SessionManager) and returning an
//!   [`AuthenticationResult`].
//! - `GET /Users/Me` — the authenticated caller's [`UserDto`].
//!
//! Port note: the C# `AuthenticateNewSession` returns an `AuthenticationResult`
//! carrying the freshly minted access token, but the ported
//! [`SessionManager::authenticate_new_session`](hermit_traits::session::SessionManager::authenticate_new_session)
//! trait returns only a [`SessionInfoDto`]. The token is therefore not available
//! at this layer; the assembled [`AuthenticationResult`] leaves `access_token`
//! unset (the session id and user identity are still returned). Widening the
//! trait to surface the token is a follow-up in the crate that owns it.

use axum::extract::State;
use axum::http::request::Parts;
use axum::routing::{get, post};
use axum::{Json, Router};
use hermit_db::entities::users::UserEntity;
use hermit_model::dto::{SessionInfoDto, UserDto};
use hermit_model::session::AuthenticationResult;
use hermit_traits::options::AuthorizationInfo;
use hermit_traits::session::AuthenticationRequest;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// The `AuthenticateByName` request body.
///
/// Port of C# `AuthenticateUserByName`: the username plus the plaintext password
/// (Jellyfin's `Pw` field). The obsolete SHA-1 `Password` field is dropped.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthenticateByNameRequest {
    /// The username to authenticate.
    #[serde(default)]
    username: Option<String>,
    /// The plaintext password (C# `Pw`).
    #[serde(default, rename = "Pw")]
    pw: Option<String>,
}

/// Reads the client identity (app/version/device) parsed by the auth-context
/// middleware into this request's [`AuthorizationInfo`] extension.
fn auth_info(parts: &Parts) -> AuthorizationInfo {
    parts
        .extensions
        .get::<AuthorizationInfo>()
        .cloned()
        .unwrap_or_default()
}

/// Projects a persisted [`UserEntity`] into the public [`UserDto`].
///
/// A minimal projection covering the fields the trait layer exposes: the id, the
/// display name, and whether a local password is set. The richer
/// policy/configuration projection lives behind the (not-yet-ported) user DTO
/// service.
fn user_dto(user: &UserEntity) -> UserDto {
    UserDto {
        id: Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil()),
        name: Some(user.username.clone()),
        has_password: Some(user.password.is_some()),
        has_configured_password: Some(user.password.is_some()),
        ..UserDto::default()
    }
}

/// `POST /Users/AuthenticateByName` — authenticate by username + password.
///
/// Port of `UserController.AuthenticateUserByName`. The app/device identity is
/// taken from the parsed authorization header; the password from the body.
#[utoipa::path(
    post,
    path = "/Users/AuthenticateByName",
    // Body schema omitted: `AuthenticationResult` transitively embeds the
    // self-referential `BaseItemDto`, whose derived `utoipa::ToSchema` recurses
    // without bound (a `hermit-model` DTO defect); inlining it here overflows
    // the OpenAPI generator. Documented by description until that is fixed.
    responses((status = 200, description = "User authenticated (AuthenticationResult)")),
    tag = "hermit"
)]
async fn authenticate_by_name(
    State(state): State<AppState>,
    parts: Parts,
    Json(body): Json<AuthenticateByNameRequest>,
) -> Result<Json<AuthenticationResult>, ApiError> {
    let auth = auth_info(&parts);
    let request = AuthenticationRequest {
        username: body.username,
        user_id: None,
        password: body.pw,
        app: auth.client,
        app_version: auth.version,
        device_id: auth.device_id,
        device_name: auth.device,
        // The remote address layer is wired at the composition root; not
        // available as a request extension here.
        remote_endpoint: None,
    };
    let session = state.sessions.authenticate_new_session(&request).await?;
    Ok(Json(authentication_result(session)))
}

/// Assembles the [`AuthenticationResult`] wire body from the opened session.
///
/// See the module note: the access token is not exposed by the trait, so
/// `access_token` is left unset.
fn authentication_result(session: SessionInfoDto) -> AuthenticationResult {
    let user = UserDto {
        id: session.user_id,
        name: session.user_name.clone(),
        ..UserDto::default()
    };
    let server_id = session.server_id.clone();
    AuthenticationResult {
        user: Some(user),
        session_info: Some(session),
        access_token: None,
        server_id,
    }
}

/// `GET /Users/Me` — the authenticated caller's user record.
///
/// Port of `UserController.GetCurrentUser`: reads the token's user id, loads the
/// [`UserEntity`], and projects it to a [`UserDto`]. A token that resolves to no
/// user is a `400`, matching the C#.
#[utoipa::path(
    get,
    path = "/Users/Me",
    responses(
        (status = 200, description = "Current user returned", body = UserDto),
        (status = 400, description = "No user for the presented token"),
        (status = 401, description = "Missing or invalid token")
    ),
    tag = "hermit"
)]
async fn get_current_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
) -> Result<Json<UserDto>, ApiError> {
    let user_id = auth.user_id();
    if user_id.is_nil() {
        return Err(ApiError::BadRequest("no user for token".to_owned()));
    }
    let user = state
        .users
        .get_user_by_id(user_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("no user for token".to_owned()))?;
    Ok(Json(user_dto(&user)))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Users/AuthenticateByName", post(authenticate_by_name))
        .route("/Users/Me", get(get_current_user))
}
