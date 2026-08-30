//! `UserController` — authentication, user CRUD, policy/configuration, and the
//! current-user endpoint.
//!
//! Ports Jellyfin's `UserController` surface that is backed by portable logic:
//!
//! - `POST /Users/AuthenticateByName` — authenticate by username + password.
//! - `POST /Users/AuthenticateWithQuickConnect` — authenticate a Quick Connect
//!   secret that has already been authorized.
//! - `GET  /Users` — list users (optionally filtered by hidden/disabled).
//! - `GET  /Users/Public` — the login-screen-visible subset.
//! - `GET  /Users/{userId}` — a single user by id.
//! - `DELETE /Users/{userId}` — delete a user (revoking its tokens first).
//! - `POST /Users/New` — create a user (optionally with a password).
//! - `POST /Users` — update a user's name/configuration.
//! - `POST /Users/{userId}/Policy` — update a user's policy.
//! - `POST /Users/Configuration` — update a user's configuration.
//! - `POST /Users/Password` — set / reset a user's password.
//! - `POST /Users/ForgotPassword` (+ `/Pin`) — the forgot-password flow.
//! - `GET  /Users/Me` — the authenticated caller's [`UserDto`].
//!
//! Port notes:
//! - `AuthenticateNewSession` returns an [`AuthenticationResult`] in C#; the
//!   ported [`SessionManager::authenticate_new_session`] returns an
//!   [`AuthenticationResultData`] (session DTO + minted access token), which the
//!   handler assembles into the wire [`AuthenticationResult`] — the client gets a
//!   real `AccessToken` to present on subsequent requests.
//! - The `[Authorize(Policy = …)]` gates (`RequiresElevation` /
//!   `IgnoreParentalControl`) are not enforced by a policy middleware here; the
//!   in-body admin guards the C# controller itself applies (last-admin,
//!   disable-admin, "may this caller update this user") are ported faithfully via
//!   [`is_administrator`].
//! - `ServerId` (a client-cosmetic display field) is left unset: sourcing it
//!   needs `IApplicationHost.SystemId`, which is not exposed at this layer.
//! - The `PlaylistManager.RemovePlaylistsAsync` cleanup on delete is deferred
//!   (no playlist manager at this layer); token revocation + user deletion run.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::configuration::UserConfiguration;
use ferrofin_model::dto::UserDto;
use ferrofin_model::secret::Secret;
use ferrofin_model::session::AuthenticationResult;
use ferrofin_model::users::{
    ForgotPasswordAction, ForgotPasswordResult, PinRedeemResult, UserPolicy,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::session::{AuthenticationRequest, AuthenticationResultData};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{RequireAdmin, RequireAuth};
use crate::error::ApiError;
use crate::extract::JsonBody;
use crate::handlers::items::user_uuid;
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
    pw: Option<Secret>,
}

/// The `AuthenticateWithQuickConnect` request body (a Quick Connect secret).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QuickConnectDto {
    /// The Quick Connect secret returned by the initiate endpoint.
    #[serde(default)]
    secret: Secret,
}

/// The `POST /Users/New` request body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CreateUserByName {
    /// The new user's name.
    name: String,
    /// The optional initial password.
    #[serde(default)]
    password: Option<Secret>,
}

/// The `POST /Users/Password` request body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UpdateUserPassword {
    /// The current plaintext password (C# `CurrentPw`).
    #[serde(default, rename = "CurrentPw")]
    current_pw: Option<Secret>,
    /// The new plaintext password (C# `NewPw`).
    #[serde(default, rename = "NewPw")]
    new_pw: Option<Secret>,
    /// Whether to reset (clear) the password instead of changing it.
    #[serde(default)]
    reset_password: bool,
}

/// The `POST /Users/ForgotPassword` request body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ForgotPasswordDto {
    /// The username whose password should be reset.
    #[serde(default)]
    entered_username: String,
}

/// The `POST /Users/ForgotPassword/Pin` request body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ForgotPasswordPinDto {
    /// The entered pin.
    #[serde(default)]
    pin: String,
}

/// Filename prefix for a pending password-reset record under the data dir
/// (`passwordreset-<userId>.json`). Port of C#
/// `DefaultPasswordResetProvider._passwordResetFileBase`.
const PASSWORD_RESET_PREFIX: &str = "passwordreset-";

/// How long an issued forgot-password pin stays valid. Port of C#
/// `DefaultPasswordResetProvider` (30 minutes).
const PIN_TTL_MINUTES: i64 = 30;

/// The on-disk record of an in-progress forgot-password reset.
///
/// Port of C# `DefaultPasswordResetProvider.SerializablePasswordReset`, written
/// to `{data}/passwordreset-<userId>.json` when a pin is issued and consumed by
/// the `/Pin` redemption.
#[derive(Debug, serde::Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SerializablePasswordReset {
    /// When the pin expires.
    expiration_date: chrono::DateTime<chrono::Utc>,
    /// The issued pin (dash-grouped uppercase hex, e.g. `1A-2B-3C-4D`).
    pin: String,
    /// The absolute path of this record (echoed to the client as `PinFile`).
    pin_file: String,
    /// The username this reset belongs to.
    user_name: String,
}

/// Generates a fresh reset pin from 4 random bytes, formatted like C#'s
/// `BitConverter.ToString` (`1A-2B-3C-4D`).
///
/// ponytail: seeds from a v4 UUID's random bytes instead of pulling in a
/// dedicated RNG crate — 32 bits of entropy over a 30-minute window is ample for
/// a single-use, admin-visible reset pin.
fn generate_reset_pin() -> String {
    let b = Uuid::new_v4().into_bytes();
    format!("{:02X}-{:02X}-{:02X}-{:02X}", b[0], b[1], b[2], b[3])
}

/// Normalizes a pin for comparison: strip the grouping dashes, uppercase.
fn normalize_pin(pin: &str) -> String {
    pin.replace('-', "").to_ascii_uppercase()
}

/// Issues a reset pin for a user, writing the record to
/// `{data_dir}/passwordreset-<user_id>.json`. Returns the `PinCode` result to
/// send the client and the plaintext pin (for the server log). Port of
/// `DefaultPasswordResetProvider.StartForgotPassword`.
fn issue_reset_pin(
    data_dir: &std::path::Path,
    user_id: &str,
    user_name: &str,
) -> Result<(ForgotPasswordResult, String), ServiceError> {
    let pin = generate_reset_pin();
    let expiration = chrono::Utc::now() + chrono::Duration::minutes(PIN_TTL_MINUTES);
    let pin_file = data_dir.join(format!("{PASSWORD_RESET_PREFIX}{user_id}.json"));
    let record = SerializablePasswordReset {
        expiration_date: expiration,
        pin: pin.clone(),
        pin_file: pin_file.to_string_lossy().into_owned(),
        user_name: user_name.to_owned(),
    };
    std::fs::create_dir_all(data_dir)
        .map_err(|e| ServiceError::backend(format!("create data dir: {e}")))?;
    let bytes = serde_json::to_vec(&record)
        .map_err(|e| ServiceError::backend(format!("serialize reset record: {e}")))?;
    std::fs::write(&pin_file, bytes)
        .map_err(|e| ServiceError::backend(format!("write reset record: {e}")))?;
    Ok((
        ForgotPasswordResult {
            action: ForgotPasswordAction::PinCode,
            pin_file: Some(record.pin_file),
            pin_expiration_date: Some(expiration),
        },
        pin,
    ))
}

/// Scans `data_dir` for pending reset records: deletes expired ones, and for
/// each whose pin matches `entered_pin` (dashes/case ignored) returns its
/// `(user_name, pin)` and deletes the record. An empty pin matches nothing. Port
/// of `DefaultPasswordResetProvider.RedeemPasswordResetPin`.
fn redeem_reset_pins(
    data_dir: &std::path::Path,
    entered_pin: &str,
) -> Result<Vec<(String, String)>, ServiceError> {
    let entered = normalize_pin(entered_pin);
    let mut matches = Vec::new();

    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        // No data dir yet ⇒ no pending resets.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(matches),
        Err(e) => return Err(ServiceError::backend(format!("read data dir: {e}"))),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name_is_reset = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(PASSWORD_RESET_PREFIX));
        let is_json = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"));
        if !name_is_reset || !is_json {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<SerializablePasswordReset>(&bytes) else {
            continue;
        };

        if record.expiration_date < chrono::Utc::now() {
            let _ = std::fs::remove_file(&path);
        } else if !entered.is_empty() && normalize_pin(&record.pin) == entered {
            matches.push((record.user_name, record.pin));
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(matches)
}

/// Query parameters carrying an optional `userId` (the update/password routes).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserIdQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// Query parameters for `GET /Users` (hidden/disabled filters).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetUsersQuery {
    /// Filter to users whose `IsHidden` matches, when set.
    #[serde(default)]
    is_hidden: Option<bool>,
    /// Filter to users whose `IsDisabled` matches, when set.
    #[serde(default)]
    is_disabled: Option<bool>,
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

/// Whether the given user is an administrator.
///
/// Reuses [`UserManager::get_user_dto`](ferrofin_traits::library::UserManager::get_user_dto)
/// (whose policy projection reads the `Permissions` table) rather than adding a
/// dedicated permission accessor, matching C# `user.HasPermission(IsAdministrator)`.
pub(crate) async fn is_administrator(
    state: &AppState,
    user: &UserEntity,
) -> Result<bool, ApiError> {
    Ok(user_policy(state, user)
        .await?
        .is_some_and(|p| p.is_administrator))
}

/// The given user's effective policy, as the `Permissions` table projects it.
///
/// The one read path every permission-gated extractor goes through, so a policy
/// check can never disagree with what `GET /Users/{id}` reports.
pub(crate) async fn user_policy(
    state: &AppState,
    user: &UserEntity,
) -> Result<Option<ferrofin_model::users::UserPolicy>, ApiError> {
    Ok(state.users.get_user_dto(user, None).await?.policy)
}

/// Ports C# `RequestHelpers.AssertCanUpdateUser`: the caller may update `target`
/// when the caller is an administrator, or when the caller *is* the target.
async fn assert_can_update_user(
    state: &AppState,
    auth: &AuthorizationInfo,
    target: &UserEntity,
) -> Result<(), ApiError> {
    let caller_id = auth.user_id();
    // `auth.user_id()` is the nil GUID for a caller with no user (an API key),
    // and C# compares against `User.Id`, a real `Guid` that is never empty. So
    // the self-update branch requires a real caller *and* a target whose stored
    // id parses — degrading an unparseable target id to nil would make it
    // compare equal to a userless caller and grant the update.
    if !caller_id.is_nil() && Uuid::parse_str(&target.id).is_ok_and(|id| id == caller_id) {
        return Ok(());
    }
    if let Some(caller) = &auth.user
        && is_administrator(state, caller).await?
    {
        return Ok(());
    }
    Err(ApiError::Forbidden("user update not allowed".to_owned()))
}

/// Writes a dashboard activity-log entry (best-effort — the feed must never
/// fail the request that produced it). Names/types mirror upstream's
/// `Jellyfin.Server.Implementations.Events.Consumers` loggers.
async fn log_activity(state: &AppState, entry: ferrofin_traits::activity::ActivityLogCreate) {
    let _ = state.activity.create_entry(entry).await;
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
    // without bound (a `ferrofin-model` DTO defect); inlining it here overflows
    // the OpenAPI generator. Documented by description until that is fixed.
    responses((status = 200, description = "User authenticated (AuthenticationResult)")),
    tag = "ferrofin"
)]
async fn authenticate_by_name(
    State(state): State<AppState>,
    parts: Parts,
    JsonBody(body): JsonBody<AuthenticateByNameRequest>,
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
    let username = request.username.clone().unwrap_or_default();
    let result = match state.sessions.authenticate_new_session(&request).await {
        Ok(result) => result,
        Err(e) => {
            // Port of `AuthenticationFailedLogger`: failed logins land in the
            // dashboard's Alerts feed (no user id, Error severity).
            log_activity(
                &state,
                ferrofin_traits::activity::ActivityLogCreate {
                    name: format!("Failed login attempt from {username}"),
                    type_: "AuthenticationFailed".to_owned(),
                    severity: ferrofin_model::activity::LogLevel::Error,
                    ..Default::default()
                },
            )
            .await;
            return Err(e.into());
        }
    };
    // Port of `AuthenticationSucceededLogger`.
    log_activity(
        &state,
        ferrofin_traits::activity::ActivityLogCreate {
            name: format!("{username} successfully authenticated"),
            type_: "AuthenticationSucceeded".to_owned(),
            user_id: Some(result.session.user_id),
            ..Default::default()
        },
    )
    .await;
    Ok(Json(authentication_result(&state, result).await))
}

/// `POST /Users/AuthenticateWithQuickConnect` — finish a Quick Connect login.
///
/// Port of `UserController.AuthenticateWithQuickConnect`: exchanges an authorized
/// Quick Connect secret for a session.
#[utoipa::path(
    post,
    path = "/Users/AuthenticateWithQuickConnect",
    responses((status = 200, description = "User authenticated (AuthenticationResult)")),
    tag = "ferrofin"
)]
async fn authenticate_with_quick_connect(
    State(state): State<AppState>,
    parts: Parts,
    JsonBody(body): JsonBody<QuickConnectDto>,
) -> Result<Json<AuthenticationResult>, ApiError> {
    // Validate the secret (it has already been authorized on another device),
    // then open a session for that user *directly* — no password — so the client
    // receives a real `AccessToken` instead of thinking it logged in with none.
    let session = state
        .quick_connect
        .get_authorized_request(body.secret.expose())
        .await?;
    let auth = auth_info(&parts);
    let request = AuthenticationRequest {
        username: None,
        user_id: Some(session.user_id),
        password: None,
        app: auth.client,
        app_version: auth.version,
        device_id: auth.device_id,
        device_name: auth.device,
        remote_endpoint: None,
    };
    let result = state.sessions.authenticate_direct(&request).await?;
    Ok(Json(authentication_result(&state, result).await))
}

/// Assembles the [`AuthenticationResult`] wire body from an authenticated
/// session and its minted access token.
///
/// The token is echoed back to the client as `AccessToken` so subsequent
/// requests can authenticate; a genuinely empty token (e.g. the Quick Connect
/// seam that does not carry one) collapses to `None`.
async fn authentication_result(
    state: &AppState,
    result: AuthenticationResultData,
) -> AuthenticationResult {
    let AuthenticationResultData {
        session,
        access_token,
    } = result;
    // Build the full User DTO exactly as `GET /Users/Me` does — real policy,
    // configuration, and `ServerId`. jellyfin-web caches `result.User` from login
    // and drives the whole UI off it, so a bare id/name DTO with a default
    // (all-false) policy and null `ServerId` locks the client out of its own
    // libraries/dashboard and throws `getApiClient(null)`. Fall back to the bare
    // DTO only if the row somehow can't be reloaded post-auth.
    let user = match load_user(state, session.user_id).await {
        Ok(entity) => state.users.get_user_dto(&entity, None).await.ok(),
        Err(_) => None,
    }
    .unwrap_or_else(|| UserDto {
        id: session.user_id,
        name: session.user_name.clone(),
        ..UserDto::default()
    });
    let server_id = session.server_id.clone();
    AuthenticationResult {
        user: Some(user),
        session_info: Some(session),
        access_token: (!access_token.expose().is_empty()).then_some(access_token),
        server_id,
    }
}

/// `GET /Users` — list users, optionally filtered by hidden/disabled.
///
/// Port of `UserController.GetUsers`: applies the hidden/disabled filters, sorts
/// by username, and projects each to a [`UserDto`].
#[utoipa::path(
    get,
    path = "/Users",
    params(
        ("isHidden" = Option<bool>, Query, description = "Filter by hidden status"),
        ("isDisabled" = Option<bool>, Query, description = "Filter by disabled status")
    ),
    responses((status = 200, description = "Users returned", body = [UserDto])),
    tag = "ferrofin"
)]
async fn get_users(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<GetUsersQuery>,
) -> Result<Json<Vec<UserDto>>, ApiError> {
    let dtos = filtered_user_dtos(&state, query.is_hidden, query.is_disabled).await?;
    Ok(Json(dtos))
}

/// `GET /Users/Public` — the login-screen-visible users.
///
/// Port of `UserController.GetPublicUsers`: only non-hidden, non-disabled users
/// are returned, regardless of the startup-wizard state (Jellyfin excludes hidden
/// and disabled users in both wizard states). The device/network narrowing the C#
/// applies needs a network manager (deferred at this layer), so it is not applied.
#[utoipa::path(
    get,
    path = "/Users/Public",
    responses((status = 200, description = "Public users returned", body = [UserDto])),
    tag = "ferrofin"
)]
async fn get_public_users(State(state): State<AppState>) -> Result<Json<Vec<UserDto>>, ApiError> {
    let dtos = filtered_user_dtos(&state, Some(false), Some(false)).await?;
    Ok(Json(dtos))
}

/// Lists users matching the optional hidden/disabled filters, ordered by name and
/// projected to [`UserDto`]s (the shared body of `GetUsers`/`GetPublicUsers`).
async fn filtered_user_dtos(
    state: &AppState,
    is_hidden: Option<bool>,
    is_disabled: Option<bool>,
) -> Result<Vec<UserDto>, ApiError> {
    let mut users = state.users.get_users().await?;
    users.sort_by(|a, b| a.username.cmp(&b.username));

    let mut dtos = Vec::with_capacity(users.len());
    for user in &users {
        let dto = state.users.get_user_dto(user, None).await?;
        let policy = dto.policy.as_ref();
        if let Some(want) = is_hidden
            && policy.is_some_and(|p| p.is_hidden) != want
        {
            continue;
        }
        if let Some(want) = is_disabled
            && policy.is_some_and(|p| p.is_disabled) != want
        {
            continue;
        }
        dtos.push(dto);
    }
    Ok(dtos)
}

/// `GET /Users/{userId}` — a single user by id.
///
/// Port of `UserController.GetUserById`: a missing user is a `404`.
#[utoipa::path(
    get,
    path = "/Users/{userId}",
    params(("userId" = String, Path, description = "The user id")),
    responses(
        (status = 200, description = "User returned", body = UserDto),
        (status = 404, description = "User not found")
    ),
    tag = "ferrofin"
)]
async fn get_user_by_id(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(user_id): Path<Uuid>,
) -> Result<Json<UserDto>, ApiError> {
    let user = load_user(&state, user_id).await?;
    Ok(Json(state.users.get_user_dto(&user, None).await?))
}

/// `DELETE /Users/{userId}` — delete a user.
///
/// Port of `UserController.DeleteUser`: revokes the user's tokens, then deletes
/// the user. (The playlist cleanup is deferred — see the module note.)
#[utoipa::path(
    delete,
    path = "/Users/{userId}",
    params(("userId" = String, Path, description = "The user id")),
    responses(
        (status = 204, description = "User deleted"),
        (status = 404, description = "User not found")
    ),
    tag = "ferrofin"
)]
async fn delete_user(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(user_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let user = load_user(&state, user_id).await?;
    state.sessions.revoke_user_tokens(user_id, "").await?;
    state.users.delete_user(user_id).await?;
    // Port of `UserDeletedLogger`.
    log_activity(
        &state,
        ferrofin_traits::activity::ActivityLogCreate {
            name: format!("User {} has been deleted", user.username),
            type_: "UserDeleted".to_owned(),
            user_id: Some(user_id),
            ..Default::default()
        },
    )
    .await;
    // Tell the deleted user's still-open clients (their sessions outlive the
    // token revocation until they disconnect) — port of `UserDeleted`.
    if let Ok(data) = serde_json::to_string(&user_id.simple().to_string()) {
        let _ = state
            .sessions
            .send_message_to_user_sessions(
                &[user_id],
                ferrofin_model::session::SessionMessageType::UserDeleted,
                &data,
            )
            .await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Pushes a `UserUpdated` message carrying the refreshed [`UserDto`] to the
/// user's sessions, so open clients pick up name/configuration/policy changes
/// without re-login (port of Jellyfin's `ServerEventNotifier.OnUserUpdated`).
/// Best-effort: a delivery failure must not fail the mutating request.
async fn notify_user_updated(state: &AppState, user_id: Uuid) {
    let Ok(user) = load_user(state, user_id).await else {
        return;
    };
    let Ok(dto) = state.users.get_user_dto(&user, None).await else {
        return;
    };
    if let Ok(data) = serde_json::to_string(&dto) {
        let _ = state
            .sessions
            .send_message_to_user_sessions(
                &[user_id],
                ferrofin_model::session::SessionMessageType::UserUpdated,
                &data,
            )
            .await;
    }
}

/// `POST /Users/New` — create a user.
///
/// Port of `UserController.CreateUserByName`: creates the user, sets the initial
/// password when supplied, and returns the projected [`UserDto`].
#[utoipa::path(
    post,
    path = "/Users/New",
    responses((status = 200, description = "User created", body = UserDto)),
    tag = "ferrofin"
)]
async fn create_user_by_name(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    JsonBody(body): JsonBody<CreateUserByName>,
) -> Result<Json<UserDto>, ApiError> {
    let new_user = state.users.create_user(&body.name).await?;
    let id = user_uuid(&new_user)?;
    if let Some(password) = &body.password {
        state.users.change_password(id, password.expose()).await?;
    }
    // Reload so the DTO reflects the password just set.
    let reloaded = load_user(&state, id).await?;
    // Port of `UserCreatedLogger`.
    log_activity(
        &state,
        ferrofin_traits::activity::ActivityLogCreate {
            name: format!("User {} has been created", reloaded.username),
            type_: "UserCreated".to_owned(),
            user_id: Some(id),
            ..Default::default()
        },
    )
    .await;
    Ok(Json(state.users.get_user_dto(&reloaded, None).await?))
}

/// `POST /Users` — update a user's name and configuration.
///
/// Port of `UserController.UpdateUser`: renames the user when the name changed,
/// then persists the supplied configuration. `userId` defaults to the caller.
#[utoipa::path(
    post,
    path = "/Users",
    params(("userId" = Option<String>, Query, description = "The user id (defaults to caller)")),
    responses(
        (status = 204, description = "User updated"),
        (status = 403, description = "Update not allowed"),
        (status = 404, description = "User not found")
    ),
    tag = "ferrofin"
)]
async fn update_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UserIdQuery>,
    JsonBody(body): JsonBody<UserDto>,
) -> Result<StatusCode, ApiError> {
    let user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    let user = load_user(&state, user_id).await?;
    assert_can_update_user(&state, &auth, &user).await?;

    if let Some(new_name) = &body.name
        && new_name != &user.username
    {
        state
            .users
            .rename_user(user_id, &user.username, new_name)
            .await?;
    }
    if let Some(config) = &body.configuration {
        state.users.update_configuration(user_id, config).await?;
    }
    notify_user_updated(&state, user_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Users/{userId}/Policy` — update a user's policy.
///
/// Port of `UserController.UpdateUserPolicy`, including the guards: the last
/// administrator may not be demoted, an administrator may not be disabled, and
/// the last enabled user may not be disabled. On a successful disable, the
/// user's tokens are revoked.
#[utoipa::path(
    post,
    path = "/Users/{userId}/Policy",
    params(("userId" = String, Path, description = "The user id")),
    responses(
        (status = 204, description = "Policy updated"),
        (status = 403, description = "Policy update forbidden"),
        (status = 404, description = "User not found")
    ),
    tag = "ferrofin"
)]
async fn update_user_policy(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(user_id): Path<Uuid>,
    JsonBody(new_policy): JsonBody<UserPolicy>,
) -> Result<StatusCode, ApiError> {
    let user = load_user(&state, user_id).await?;
    let was_admin = is_administrator(&state, &user).await?;

    // If removing admin access, there must remain at least one administrator.
    if !new_policy.is_administrator && was_admin && admin_count(&state).await? == 1 {
        return Err(ApiError::Forbidden(
            "there must be at least one user in the system with administrative access".to_owned(),
        ));
    }

    // Administrators cannot be disabled.
    if new_policy.is_disabled && was_admin {
        return Err(ApiError::Forbidden(
            "administrators cannot be disabled".to_owned(),
        ));
    }

    // Disabling a currently-enabled user: at least one enabled user must remain.
    let currently_disabled = state
        .users
        .get_user_dto(&user, None)
        .await?
        .policy
        .is_some_and(|p| p.is_disabled);
    let mut revoke_on_disable = false;
    if new_policy.is_disabled && !currently_disabled {
        if enabled_count(&state).await? == 1 {
            return Err(ApiError::Forbidden(
                "there must be at least one enabled user in the system".to_owned(),
            ));
        }
        revoke_on_disable = true;
    }

    if revoke_on_disable {
        state.sessions.revoke_user_tokens(user_id, "").await?;
    }
    state.users.update_policy(user_id, &new_policy).await?;
    notify_user_updated(&state, user_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Users/Configuration` — update a user's configuration.
///
/// Port of `UserController.UpdateUserConfiguration`. `userId` defaults to the
/// caller; the caller must be permitted to update the target user.
#[utoipa::path(
    post,
    path = "/Users/Configuration",
    params(("userId" = Option<String>, Query, description = "The user id (defaults to caller)")),
    responses(
        (status = 204, description = "Configuration updated"),
        (status = 403, description = "Update not allowed"),
        (status = 404, description = "User not found")
    ),
    tag = "ferrofin"
)]
async fn update_user_configuration(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UserIdQuery>,
    JsonBody(config): JsonBody<UserConfiguration>,
) -> Result<StatusCode, ApiError> {
    let user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    let user = load_user(&state, user_id).await?;
    assert_can_update_user(&state, &auth, &user).await?;
    state.users.update_configuration(user_id, &config).await?;
    notify_user_updated(&state, user_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Users/Password` — set or reset a user's password.
///
/// Port of `UserController.UpdateUserPassword`: resets the password when
/// `ResetPassword` is set; otherwise (for a non-admin caller, or a caller
/// updating their own account) verifies the current password before changing it,
/// then revokes the user's other tokens.
#[utoipa::path(
    post,
    path = "/Users/Password",
    params(("userId" = Option<String>, Query, description = "The user id (defaults to caller)")),
    responses(
        (status = 204, description = "Password updated"),
        (status = 403, description = "Update not allowed or invalid credentials"),
        (status = 404, description = "User not found")
    ),
    tag = "ferrofin"
)]
async fn update_user_password(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UserIdQuery>,
    JsonBody(body): JsonBody<UpdateUserPassword>,
) -> Result<StatusCode, ApiError> {
    let user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    let user = load_user(&state, user_id).await?;
    assert_can_update_user(&state, &auth, &user).await?;

    if body.reset_password {
        state.users.reset_password(user_id).await?;
        log_password_changed(&state, user_id, &user.username).await;
        return Ok(StatusCode::NO_CONTENT);
    }

    // A non-admin caller (or one changing their own explicit-userId account)
    // must prove the current password.
    let caller_is_admin = match &auth.user {
        Some(caller) => is_administrator(&state, caller).await?,
        None => false,
    };
    let updating_self = query.user_id.is_some_and(|id| id == auth.user_id());
    if !caller_is_admin || updating_self {
        let ok = state
            .users
            .authenticate_user(
                &user.username,
                body.current_pw.as_ref().map_or("", Secret::expose),
                "",
                false,
            )
            .await?;
        if ok.is_none() {
            return Err(ApiError::Forbidden(
                "invalid user or password entered".to_owned(),
            ));
        }
    }

    state
        .users
        .change_password(user_id, body.new_pw.as_ref().map_or("", Secret::expose))
        .await?;
    state.sessions.revoke_user_tokens(user_id, "").await?;
    log_password_changed(&state, user_id, &user.username).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Records the `UserPasswordChanged` activity entry (port of
/// `UserPasswordChangedLogger`).
async fn log_password_changed(state: &AppState, user_id: Uuid, username: &str) {
    log_activity(
        state,
        ferrofin_traits::activity::ActivityLogCreate {
            name: format!("Password has been changed for user {username}"),
            type_: "UserPasswordChanged".to_owned(),
            user_id: Some(user_id),
            ..Default::default()
        },
    )
    .await;
}

/// `POST /Users/ForgotPassword` — begin the forgot-password flow.
///
/// Port of `UserController.ForgotPassword` → `UserManager.StartForgotPassword` →
/// the built-in `DefaultPasswordResetProvider`: when the entered username matches
/// a user, a random pin (valid [`PIN_TTL_MINUTES`]) is written to
/// `{data}/passwordreset-<userId>.json` and its path returned as `PinFile` with
/// action [`ForgotPasswordAction::PinCode`]. The pin is also logged (there is no
/// e-mail provider — the admin reads it from the server log, matching Jellyfin).
/// An unknown / blank username yields [`ForgotPasswordAction::ContactAdmin`] so
/// the endpoint never discloses whether an account exists.
#[utoipa::path(
    post,
    path = "/Users/ForgotPassword",
    responses((status = 200, description = "Forgot-password process started", body = ForgotPasswordResult)),
    tag = "ferrofin"
)]
async fn forgot_password(
    State(state): State<AppState>,
    JsonBody(body): JsonBody<ForgotPasswordDto>,
) -> Result<Json<ForgotPasswordResult>, ApiError> {
    let contact_admin = || {
        Ok(Json(ForgotPasswordResult {
            action: ForgotPasswordAction::ContactAdmin,
            pin_file: None,
            pin_expiration_date: None,
        }))
    };

    let username = body.entered_username.trim();
    if username.is_empty() {
        return contact_admin();
    }
    let Some(user) = state.users.get_user_by_name(username).await? else {
        return contact_admin();
    };

    let dir = std::path::PathBuf::from(state.config.application_paths().data_path());
    let (result, pin) = issue_reset_pin(&dir, &user.id, &user.username)?;
    // No e-mail provider: surface the pin in the log so the admin can relay it.
    tracing::info!(user = %user.username, pin = %pin, "forgot-password pin issued");
    Ok(Json(result))
}

/// `POST /Users/ForgotPassword/Pin` — redeem a forgot-password pin.
///
/// Port of `UserController.ForgotPasswordPin` →
/// `DefaultPasswordResetProvider.RedeemPasswordResetPin`: scans the pending
/// `passwordreset-*.json` records, deleting expired ones. When one matches the
/// entered pin (dashes/case ignored) its user's password is set to the pin (so
/// they can log in and change it) and the record deleted. Reports the set of
/// usernames reset.
#[utoipa::path(
    post,
    path = "/Users/ForgotPassword/Pin",
    responses((status = 200, description = "Pin redemption result", body = PinRedeemResult)),
    tag = "ferrofin"
)]
async fn forgot_password_pin(
    State(state): State<AppState>,
    JsonBody(body): JsonBody<ForgotPasswordPinDto>,
) -> Result<Json<PinRedeemResult>, ApiError> {
    let dir = std::path::PathBuf::from(state.config.application_paths().data_path());
    let mut users_reset = Vec::new();
    for (user_name, pin) in redeem_reset_pins(&dir, &body.pin)? {
        // Set the matched user's password to the pin so they can log in and
        // change it (C# `_userManager.ChangePassword(resetUser, pin)`).
        if let Some(user) = state.users.get_user_by_name(&user_name).await?
            && let Ok(user_id) = Uuid::parse_str(&user.id)
        {
            state.users.change_password(user_id, &pin).await?;
            users_reset.push(user.username);
        }
    }
    Ok(Json(PinRedeemResult {
        success: !users_reset.is_empty(),
        users_reset,
    }))
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
    tag = "ferrofin"
)]
async fn get_current_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
) -> Result<Json<UserDto>, ApiError> {
    if auth.user_id().is_nil() {
        return Err(ApiError::BadRequest("no user for token".to_owned()));
    }
    // The auth layer already loaded this row — no re-fetch.
    let user = auth
        .user
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("no user for token".to_owned()))?;
    Ok(Json(state.users.get_user_dto(user, None).await?))
}

/// Loads a user by id, mapping the absent case to a `404`.
async fn load_user(state: &AppState, user_id: Uuid) -> Result<UserEntity, ApiError> {
    state
        .users
        .get_user_by_id(user_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("user not found".to_owned()))
}

/// Counts the administrators in the system (for the policy guards).
async fn admin_count(state: &AppState) -> Result<usize, ApiError> {
    let users = state.users.get_users().await?;
    let mut count = 0;
    for user in &users {
        if is_administrator(state, user).await? {
            count += 1;
        }
    }
    Ok(count)
}

/// Counts the enabled (non-disabled) users in the system (for the policy guards).
async fn enabled_count(state: &AppState) -> Result<usize, ApiError> {
    let users = state.users.get_users().await?;
    let mut count = 0;
    for user in &users {
        let disabled = state
            .users
            .get_user_dto(user, None)
            .await?
            .policy
            .is_some_and(|p| p.is_disabled);
        if !disabled {
            count += 1;
        }
    }
    Ok(count)
}

/// `POST /Users/{userId}` — path-scoped form of `POST /Users` (update user).
///
/// Upstream keeps these `/Users/{userId}/…` forms `[Obsolete]` and hidden from
/// the OpenAPI doc but still serves them, and jellyfin-web's bundled
/// `jellyfin-apiclient` still calls them. Each injects the path `userId` into
/// the query and forwards to the query-scoped handler.
async fn update_user_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path(user_id): Path<Uuid>,
    body: JsonBody<UserDto>,
) -> Result<StatusCode, ApiError> {
    let query = UserIdQuery {
        user_id: Some(user_id),
    };
    update_user(state, auth, Query(query), body).await
}

/// `POST /Users/{userId}/Configuration` — path-scoped form of
/// `POST /Users/Configuration`.
async fn update_user_configuration_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path(user_id): Path<Uuid>,
    config: JsonBody<UserConfiguration>,
) -> Result<StatusCode, ApiError> {
    let query = UserIdQuery {
        user_id: Some(user_id),
    };
    update_user_configuration(state, auth, Query(query), config).await
}

/// `POST /Users/{userId}/Password` — path-scoped form of `POST /Users/Password`.
async fn update_user_password_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path(user_id): Path<Uuid>,
    body: JsonBody<UpdateUserPassword>,
) -> Result<StatusCode, ApiError> {
    let query = UserIdQuery {
        user_id: Some(user_id),
    };
    update_user_password(state, auth, Query(query), body).await
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Users/AuthenticateByName", post(authenticate_by_name))
        .route(
            "/Users/AuthenticateWithQuickConnect",
            post(authenticate_with_quick_connect),
        )
        .route("/Users", get(get_users).post(update_user))
        .route("/Users/Public", get(get_public_users))
        .route(
            "/Users/{userId}",
            get(get_user_by_id)
                .delete(delete_user)
                .post(update_user_for_user),
        )
        .route("/Users/New", post(create_user_by_name))
        .route("/Users/{userId}/Policy", post(update_user_policy))
        .route(
            "/Users/{userId}/Configuration",
            post(update_user_configuration_for_user),
        )
        .route(
            "/Users/{userId}/Password",
            post(update_user_password_for_user),
        )
        .route("/Users/Configuration", post(update_user_configuration))
        .route("/Users/Password", post(update_user_password))
        .route("/Users/ForgotPassword", post(forgot_password))
        .route("/Users/ForgotPassword/Pin", post(forgot_password_pin))
        .route("/Users/Me", get(get_current_user))
}

#[cfg(test)]
mod tests {
    use super::{
        PASSWORD_RESET_PREFIX, SerializablePasswordReset, generate_reset_pin, issue_reset_pin,
        normalize_pin, redeem_reset_pins,
    };

    #[test]
    fn reset_pin_format_and_normalization() {
        let pin = generate_reset_pin();
        // `XX-XX-XX-XX`: 8 hex digits in four dash-separated pairs.
        let parts: Vec<&str> = pin.split('-').collect();
        assert_eq!(parts.len(), 4);
        assert!(
            parts
                .iter()
                .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
        );
        // Normalization strips dashes and uppercases, so a lowercased/spaced-out
        // entry still matches.
        assert_eq!(
            normalize_pin(&pin),
            normalize_pin(&pin.to_ascii_lowercase())
        );
        assert_eq!(normalize_pin("1a-2b"), "1A2B");
    }

    #[test]
    fn issue_then_redeem_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let (result, pin) = issue_reset_pin(dir.path(), "user-1", "alice").unwrap();
        assert_eq!(result.action, super::ForgotPasswordAction::PinCode);
        assert!(result.pin_file.is_some());
        assert!(result.pin_expiration_date.is_some());
        // The record file exists.
        let file = dir
            .path()
            .join(format!("{PASSWORD_RESET_PREFIX}user-1.json"));
        assert!(file.exists());

        // A wrong pin matches nothing and leaves the record in place.
        assert!(
            redeem_reset_pins(dir.path(), "FF-FF-FF-FF")
                .unwrap()
                .is_empty()
        );
        assert!(file.exists());
        // An empty pin never matches.
        assert!(redeem_reset_pins(dir.path(), "").unwrap().is_empty());

        // The right pin (dashes/case ignored) returns the user + consumes the file.
        let matched = redeem_reset_pins(dir.path(), &pin.to_ascii_lowercase()).unwrap();
        assert_eq!(matched, vec![("alice".to_owned(), pin)]);
        assert!(!file.exists());
    }

    #[test]
    fn expired_record_is_deleted_and_never_matches() {
        let dir = tempfile::tempdir().unwrap();
        let record = SerializablePasswordReset {
            expiration_date: chrono::Utc::now() - chrono::Duration::minutes(1),
            pin: "AA-BB-CC-DD".to_owned(),
            pin_file: String::new(),
            user_name: "bob".to_owned(),
        };
        let file = dir
            .path()
            .join(format!("{PASSWORD_RESET_PREFIX}user-2.json"));
        std::fs::write(&file, serde_json::to_vec(&record).unwrap()).unwrap();

        // Even with the correct pin, an expired record yields no match and is purged.
        assert!(
            redeem_reset_pins(dir.path(), "AA-BB-CC-DD")
                .unwrap()
                .is_empty()
        );
        assert!(!file.exists());
    }

    #[test]
    fn redeem_on_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(
            redeem_reset_pins(&missing, "AA-BB-CC-DD")
                .unwrap()
                .is_empty()
        );
    }
}
