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
//! - `DELETE /Users/{userId}` — delete a user (revoking its tokens first),
//!   then hand over or delete the playlists it owned.
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
//!   ported `SessionManager::authenticate_new_session` returns an
//!   [`AuthenticationResultData`] (session DTO + minted access token), which the
//!   handler assembles into the wire [`AuthenticationResult`] — the client gets a
//!   real `AccessToken` to present on subsequent requests.
//! - The `[Authorize]` attributes are extractors: `Policy = RequiresElevation` is
//!   [`RequireAdmin`], `Policy = IgnoreParentalControl` is
//!   [`RequireAuthIgnoringSchedule`], and a plain `[Authorize]` is
//!   [`RequireAuth`]. The guards the C# controller applies itself ("may this
//!   caller update this user", the last administrator, an administrator
//!   disabled, the last enabled user) are ported too; the policy ones run
//!   inside the save
//!   ([`UserManager::update_policy_guarded`](ferrofin_traits::library::UserManager::update_policy_guarded)).
//! - `ServerId` is the server's system id: the user manager sets it on every
//!   `UserDto` it builds, and the authentication result takes it from the
//!   session.
//! - A refused login answers as upstream's `ExceptionMiddleware` does, with no
//!   reason in the body (`login_failure`).

use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::configuration::UserConfiguration;
use ferrofin_model::dto::UserDto;
use ferrofin_model::secret::Secret;
use ferrofin_model::session::AuthenticationResult;
use ferrofin_model::users::{ForgotPasswordResult, PinRedeemResult, UserPolicy};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::session::{AuthenticationRequest, AuthenticationResultData};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{
    AuthenticatedIdentity, RequireAdmin, RequireAuth, RequireAuthIgnoringSchedule,
    is_administrator_identity,
};
use crate::error::ApiError;
use crate::extract::{DotnetGuid, JsonBody, ProblemDetails, Query, RequiredString, body_parameter};
use crate::handlers::items::user_uuid;
use crate::panic_boundary::ERROR_BODY;
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
    /// The Quick Connect secret returned by the initiate endpoint. Upstream
    /// marks it `[Required]`, so the handler refuses a missing, `null` or
    /// blank one.
    #[serde(default)]
    secret: Option<Secret>,
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
    /// The username whose password should be reset (upstream's
    /// `[Required] public required string`).
    #[serde(default)]
    entered_username: RequiredString,
}

/// The `POST /Users/ForgotPassword/Pin` request body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ForgotPasswordPinDto {
    /// The entered pin (upstream's `[Required] public required string`).
    #[serde(default)]
    pin: RequiredString,
}

/// Query parameters carrying an optional `userId` (the update, password and
/// user-image write routes).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserIdQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    pub(crate) user_id: Option<Uuid>,
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
pub(crate) async fn assert_can_update_user(
    state: &AppState,
    auth: &AuthorizationInfo,
    target: &UserEntity,
) -> Result<(), ApiError> {
    let target_id = Uuid::parse_str(&target.id).ok().filter(|id| !id.is_nil());
    if target_id.is_none() {
        return Err(ApiError::Forbidden("user update not allowed".to_owned()));
    }
    // API keys carry the administrator role upstream. Validate the target
    // first so a corrupt stored id never becomes the anonymous/nil caller.
    if auth.is_api_key {
        return Ok(());
    }
    if let Some(caller) = &auth.user
        && is_administrator(state, caller).await?
    {
        return Ok(());
    }
    // All three account-update actions pass restrictUserPreferences=true to
    // RequestHelpers.AssertCanUpdateUser, including password changes/resets.
    if target_id == Some(auth.user_id()) && target.enable_user_preference_access {
        return Ok(());
    }
    Err(ApiError::Forbidden("user update not allowed".to_owned()))
}

/// Writes a dashboard activity-log entry (best-effort — the feed must never
/// fail the request that produced it, so a failure is logged instead, as
/// upstream's `EventManager` logs a failed consumer). Names/types mirror
/// upstream's `Jellyfin.Server.Implementations.Events.Consumers` loggers.
async fn log_activity(state: &AppState, entry: ferrofin_traits::activity::ActivityLogCreate) {
    let activity_type = entry.type_.clone();
    if let Err(error) = state.activity.create_entry(entry).await {
        // Warn, not error: its volume follows the request rate.
        tracing::warn!(%error, activity_type, "could not write the activity entry");
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
    // without bound (a `ferrofin-model` DTO defect); inlining it here overflows
    // the OpenAPI generator. Documented by description until that is fixed.
    responses((status = 200, description = "User authenticated (AuthenticationResult)")),
    tag = "ferrofin"
)]
async fn authenticate_by_name(
    State(state): State<AppState>,
    parts: Parts,
    JsonBody(body): JsonBody<AuthenticateByNameRequest>,
) -> Result<Response, ApiError> {
    Box::pin(authenticate(&state, &parts, body)).await
}

/// The query of `POST /Users/{userId}/Authenticate` (`[FromQuery, Required]
/// string pw`; an absent one is refused by the extractor's required table).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthenticateByIdQuery {
    /// The plaintext password.
    #[serde(default)]
    pw: Option<Secret>,
}

/// `POST /Users/{userId}/Authenticate` — authenticate by user id + password.
///
/// Port of the obsolete `UserController.AuthenticateUser`
/// (`[ApiExplorerSettings(IgnoreApi = true)]`, so absent from the OpenAPI
/// contract, but still served upstream and anonymous): 400 for a nil id, 404
/// "User not found", else exactly `AuthenticateUserByName` with that user's name.
async fn authenticate_by_id(
    State(state): State<AppState>,
    Path(DotnetGuid(user_id)): Path<DotnetGuid>,
    Query(query): Query<AuthenticateByIdQuery>,
    parts: Parts,
) -> Result<Response, ApiError> {
    // `GetUserById(Guid.Empty)` throws `ArgumentException`: a `400`.
    if user_id.is_nil() {
        return Err(ApiError::BadRequest("Guid can't be empty".to_owned()));
    }
    let Some(user) = state.users.get_user_by_id(user_id).await? else {
        return Err(ApiError::NotFound("User not found".to_owned()));
    };
    let body = AuthenticateByNameRequest {
        username: Some(user.username),
        pw: query.pw,
    };
    Box::pin(authenticate(&state, &parts, body)).await
}

/// Answers a failed login: a `401` or `403` as upstream's `ExceptionMiddleware`
/// answers an `AuthenticationException` or a `SecurityException` outside
/// Development, with the status, `text/plain` and [`ERROR_BODY`].
///
/// The reason stays out of the body, which would otherwise tell a caller which
/// usernames exist and name a disabled account. jellyfin-web and the Kotlin SDK
/// read only the status: `401` is bad credentials, `403` an account that may
/// not sign in. The core logs each refused login with its `reason`, at `warn`,
/// or at `debug` for an attempt its login backoff refused; the debug line here
/// adds the text, and is the only record of Quick Connect being off.
///
/// Every other failure is the caller's to answer as before, so a login's `400`
/// or `500` keeps the shared [`ApiError`] body.
fn login_failure(error: ServiceError) -> Result<Response, ApiError> {
    let error = ApiError::from(error);
    let status = error.status();
    if status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN {
        return Err(error);
    }
    tracing::debug!(%status, %error, "login refused");
    Ok((status, [(header::CONTENT_TYPE, "text/plain")], ERROR_BODY).into_response())
}

/// `UserController.AuthenticateUserByName`'s body, shared by both login routes.
async fn authenticate(
    state: &AppState,
    parts: &Parts,
    body: AuthenticateByNameRequest,
) -> Result<Response, ApiError> {
    let auth = auth_info(parts);
    // C# `UserController.AuthenticateUserByName` sets
    // `RemoteEndPoint = HttpContext.GetNormalizedRemoteIP().ToString()`. It ends
    // up on `SessionInfo.RemoteEndPoint`, which is what `GET /Sessions` reports
    // and what every activity-log `ShortOverview` ("IP address: …") is built
    // from — so a `None` here silently blanks both.
    let remote_ip = state.client_address(parts).to_string();
    let request = AuthenticationRequest {
        username: body.username,
        user_id: None,
        password: body.pw,
        app: auth.client,
        app_version: auth.version,
        device_id: auth.device_id,
        device_name: auth.device,
        remote_endpoint: Some(remote_ip.clone()),
    };
    let username = request.username.clone().unwrap_or_default();
    let result = match state.sessions.authenticate_new_session(&request).await {
        Ok(result) => result,
        Err(e) => {
            // Port of `AuthenticationFailedLogger`: failed logins land in the
            // dashboard's Alerts feed (no user id, Error severity).
            log_activity(
                state,
                ferrofin_traits::activity::ActivityLogCreate {
                    name: format!("Failed login attempt from {username}"),
                    type_: "AuthenticationFailed".to_owned(),
                    severity: ferrofin_model::activity::LogLevel::Error,
                    // `LabelIpAddressValue` = "IP address: {0}", filled from
                    // `AuthenticationRequestEventArgs.RemoteEndPoint`.
                    short_overview: Some(format!("IP address: {remote_ip}")),
                    ..Default::default()
                },
            )
            .await;
            return login_failure(e);
        }
    };
    // The `AuthenticationSucceeded` entry is NOT written here: C# raises it from
    // `SessionManager.AuthenticateNewSessionInternal`, *after* the awaited
    // `LogSessionActivity` that raises `SessionStarted`. Writing it in the
    // handler instead put it before the (spawned) SessionStarted row and showed
    // the dashboard's login pair backwards. The consumer lives beside the
    // SessionStarted one in the composition root.
    Ok(Json(authentication_result(state, result).await).into_response())
}

/// `POST /Users/AuthenticateWithQuickConnect` — finish a Quick Connect login.
///
/// Port of `UserController.AuthenticateWithQuickConnect`: exchanges an authorized
/// Quick Connect secret for the session and token opened when its code was
/// authorized. It never authenticates again: a second login would count the
/// authorize-time session against `MaxActiveSessions` and orphan its token.
#[utoipa::path(
    post,
    path = "/Users/AuthenticateWithQuickConnect",
    responses((status = 200, description = "User authenticated (AuthenticationResult)")),
    tag = "ferrofin"
)]
async fn authenticate_with_quick_connect(
    State(state): State<AppState>,
    JsonBody(body): JsonBody<QuickConnectDto>,
) -> Result<Response, ApiError> {
    // `[Required]` model validation runs before the action and refuses a
    // blank string (`IsNullOrWhiteSpace`) as well as a missing one.
    let Some(secret) = body.secret.filter(|s| !s.expose().trim().is_empty()) else {
        return Ok(
            ProblemDetails::member_validation("Secret", "The Secret field is required.")
                .into_response(),
        );
    };
    let result = match state
        .quick_connect
        .get_authorized_request(secret.expose())
        .await
    {
        Ok(result) => result,
        Err(e) => return login_failure(e),
    };
    Ok(Json(authentication_result(&state, result).await).into_response())
}

/// Assembles the [`AuthenticationResult`] wire body from an authenticated
/// session and its minted access token.
///
/// The token is echoed back to the client as `AccessToken` so subsequent
/// requests can authenticate.
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
    // and drives the whole UI off it: an all-false policy hides its libraries
    // and dashboard, and a null `ServerId` throws `getApiClient(null)`. Fall
    // back to a bare DTO only if the row somehow can't be reloaded post-auth:
    // the token is minted by then, and upstream always answers with it, so the
    // fallback carries the session's `ServerId` and `new UserDto()`'s policy
    // (`new UserPolicy()`) rather than failing the sign-in.
    let user = match load_user(state, session.user_id).await {
        Ok(entity) => state.users.get_user_dto(&entity, None).await.ok(),
        Err(_) => None,
    }
    .unwrap_or_else(|| UserDto {
        id: session.user_id,
        name: session.user_name.clone(),
        server_id: session.server_id.clone(),
        ..UserDto::default()
    });
    let server_id = session.server_id.clone();
    AuthenticationResult {
        user: Some(user),
        session_info: Some(session),
        access_token: Some(access_token),
        server_id,
    }
}

/// `GET /Users` — list users, optionally filtered by hidden/disabled.
///
/// Port of `UserController.GetUsers`: applies the hidden/disabled filters, sorts
/// by username, and projects each to a [`UserDto`]. [`strip_other_accounts`] goes
/// beyond upstream.
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
    RequireAuth(auth): RequireAuth,
    Query(query): Query<GetUsersQuery>,
) -> Result<Json<Vec<UserDto>>, ApiError> {
    let mut dtos = filtered_user_dtos(&state, query.is_hidden, query.is_disabled, None).await?;
    strip_other_accounts(&state, Some(&auth), &mut dtos).await?;
    Ok(Json(dtos))
}

/// `GET /Users/Public` — the login-screen-visible users.
///
/// Port of `UserController.GetPublicUsers`: only non-hidden, non-disabled users
/// are returned in both wizard states. After setup, remote callers only see
/// accounts that allow remote access, using the shared live network policy.
/// Authenticated callers also filter by their device after setup.
/// [`strip_other_accounts`] goes beyond upstream. It runs after the filters that
/// read `Policy`, and a login screen, signed out, sees neither field on any row.
#[utoipa::path(
    get,
    path = "/Users/Public",
    responses((status = 200, description = "Public users returned", body = [UserDto])),
    tag = "ferrofin"
)]
async fn get_public_users(
    State(state): State<AppState>,
    identity: Result<AuthenticatedIdentity, ApiError>,
    parts: Parts,
) -> Result<Json<Vec<UserDto>>, ApiError> {
    let setup = state
        .config
        .configuration()
        .await?
        .is_startup_wizard_completed;
    // C# reads the authenticated claim, not an anonymous DeviceId header.
    let auth = identity.ok().map(|identity| identity.0);
    let device_id = auth
        .as_ref()
        .and_then(|auth| auth.device_id.as_deref())
        .filter(|id| setup && !id.trim().is_empty());
    let mut dtos = filtered_user_dtos(&state, Some(false), Some(false), device_id).await?;
    if setup && !state.is_in_local_network(state.client_address(&parts)) {
        dtos.retain(|dto| {
            dto.policy
                .as_ref()
                .is_some_and(|policy| policy.enable_remote_access)
        });
    }
    strip_other_accounts(&state, auth.as_ref(), &mut dtos).await?;
    Ok(Json(dtos))
}

/// Clears `Policy` and `Configuration` from the rows of accounts `caller` may not
/// read them for: every row but the caller's own, unless the caller is an
/// administrator or an API key. A signed-out caller (`None`, `/Users/Public` on a
/// login screen) has no own row.
///
/// An owner decision that goes beyond Jellyfin, which strips nothing (gl-1102).
/// Both fields are nullable in the contract, and a cleared one is left out of the
/// JSON, as Jellyfin writes any null.
async fn strip_other_accounts(
    state: &AppState,
    caller: Option<&AuthorizationInfo>,
    dtos: &mut [UserDto],
) -> Result<(), ApiError> {
    if let Some(auth) = caller
        && is_administrator_identity(state, auth).await?
    {
        return Ok(());
    }
    let own_id = caller
        .map(AuthorizationInfo::user_id)
        .filter(|id| !id.is_nil());
    for dto in dtos.iter_mut().filter(|dto| Some(dto.id) != own_id) {
        dto.policy = None;
        dto.configuration = None;
    }
    Ok(())
}

/// Lists users matching the optional hidden/disabled filters, ordered by name and
/// projected to [`UserDto`]s (the shared body of `GetUsers`/`GetPublicUsers`).
async fn filtered_user_dtos(
    state: &AppState,
    is_hidden: Option<bool>,
    is_disabled: Option<bool>,
    device_id: Option<&str>,
) -> Result<Vec<UserDto>, ApiError> {
    let mut users = state.users.get_users().await?;
    sort_by_username(&mut users);

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
        if let Some(device_id) = device_id
            && !state.devices.can_access_device(user, device_id).await?
        {
            continue;
        }
        dtos.push(dto);
    }
    Ok(dtos)
}

/// Upstream's `OrderBy(u => u.Username)`, which compares by the current culture.
fn sort_by_username(users: &mut [UserEntity]) {
    let comparer =
        ferrofin_util::current_culture::comparer(&ferrofin_util::current_culture::capture()).ok();
    users.sort_by(|a, b| {
        comparer.as_ref().map_or_else(
            || a.username.cmp(&b.username),
            |comparer| comparer.compare(&a.username, &b.username),
        )
    });
}

/// `GET /Users/{userId}` — a single user by id.
///
/// Port of `UserController.GetUserById`: a nil id is a `400`, a missing user
/// a `404`. [`strip_other_accounts`] goes beyond upstream.
#[utoipa::path(
    get,
    path = "/Users/{userId}",
    params(("userId" = String, Path, description = "The user id")),
    responses(
        (status = 200, description = "User returned", body = UserDto),
        (status = 400, description = "A nil user id"),
        (status = 404, description = "User not found")
    ),
    tag = "ferrofin"
)]
async fn get_user_by_id(
    State(state): State<AppState>,
    RequireAuthIgnoringSchedule(auth): RequireAuthIgnoringSchedule,
    Path(DotnetGuid(user_id)): Path<DotnetGuid>,
) -> Result<Json<UserDto>, ApiError> {
    let user = load_user(&state, user_id).await?;
    let mut dto = state.users.get_user_dto(&user, None).await?;
    strip_other_accounts(&state, Some(&auth), std::slice::from_mut(&mut dto)).await?;
    Ok(Json(dto))
}

/// `DELETE /Users/{userId}` — delete a user.
///
/// Port of `UserController.DeleteUser`: revokes the user's tokens, deletes the
/// user, then hands over or deletes the playlists they owned
/// ([`PlaylistManager::remove_playlists`](ferrofin_traits::collections::PlaylistManager::remove_playlists)).
///
/// Upstream does the playlists between the two, before `DeleteUserAsync`,
/// whose last-user / last-admin refusal then leaves the user in place with
/// their playlists already given away or deleted. Here a refused delete
/// touches no playlist (a Jellyfin bug not ported). The deletion's activity
/// entry and `UserDeleted` message go out before the playlists, as upstream
/// raises them inside `DeleteUserAsync`, so a failed hand-over (an error
/// response, the user already deleted) cannot skip them.
#[utoipa::path(
    delete,
    path = "/Users/{userId}",
    params(("userId" = String, Path, description = "The user id")),
    responses(
        (status = 204, description = "User deleted"),
        (status = 400, description = "A nil user id"),
        (status = 404, description = "User not found")
    ),
    tag = "ferrofin"
)]
async fn delete_user(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(DotnetGuid(user_id)): Path<DotnetGuid>,
) -> Result<StatusCode, ApiError> {
    let user = load_user(&state, user_id).await?;
    state.sessions.revoke_user_tokens(user_id, "").await?;
    state.users.delete_user(user_id).await?;
    // Port of `UserDeletedLogger`, which writes
    // `new ActivityLog(…, "UserDeleted", Guid.Empty)` — deliberately NOT the
    // deleted user's id, so the entry survives as a server-level event and the
    // `hasUserId=true` filter (`ActivityLogEntryQuery.HasUserId`) excludes it.
    // `ActivityLogCreate::user_id = None` is stored as the empty guid, which is
    // the same value Jellyfin puts on the wire.
    log_activity(
        &state,
        ferrofin_traits::activity::ActivityLogCreate {
            name: format!("User {} has been deleted", user.username),
            type_: "UserDeleted".to_owned(),
            user_id: None,
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
    state.playlists.remove_playlists(user_id).await?;
    Ok(StatusCode::NO_CONTENT)
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
        (status = 400, description = "Invalid name, an incomplete policy, or a nil user id"),
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
) -> Result<Response, ApiError> {
    // Model validation walks into the nested `Policy` before the action runs.
    // A body without one gets `UserDto`'s constructor policy, whose provider
    // ids are null, so only a complete policy or `"Policy": null` passes.
    if let Some(problem) = body
        .policy
        .as_ref()
        .and_then(|policy| policy_validation(policy, "Policy."))
    {
        return Ok(problem.into_response());
    }
    let user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    let user = load_user(&state, user_id).await?;
    assert_can_update_user(&state, &auth, &user).await?;

    // C# renames whenever `updateUser.Name` differs from the stored name, so a
    // missing or null name reaches `RenameUser` too, and its
    // `ThrowIfInvalidUsername` refuses it (400) as it refuses a blank one.
    // The user manager announces the rename (`UserUpdated`, then the device
    // check) before it returns, so ahead of the configuration save.
    let new_name = body.name.as_deref().unwrap_or_default();
    if new_name != user.username {
        state
            .users
            .rename_user(user_id, &user.username, new_name)
            .await?;
    }
    // A body without `Configuration` binds `UserDto`'s constructor default, so
    // the save resets the user's settings as upstream's does; `null` (a 500
    // upstream, `UpdateConfigurationAsync` dereferences it) saves nothing.
    if let Some(config) = &body.configuration {
        state.users.update_configuration(user_id, config).await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /Users/{userId}/Policy` — update a user's policy.
///
/// Port of `UserController.UpdateUserPolicy`, including the guards: the last
/// administrator may not be demoted, an administrator may not be disabled, and
/// the last enabled user may not be disabled. The guards are read inside the
/// save's transaction ([`UserManager::update_policy_guarded`]), so concurrent
/// saves cannot both pass one. The save itself announces `UserUpdated`, which
/// pushes the user's DTO to their sessions and logs out the devices the saved
/// policy no longer allows (Jellyfin 12.1's `DeviceAccessHost`). On a
/// successful disable, the user's tokens are revoked after that.
///
/// [`UserManager::update_policy_guarded`]: ferrofin_traits::library::UserManager::update_policy_guarded
#[utoipa::path(
    post,
    path = "/Users/{userId}/Policy",
    params(("userId" = String, Path, description = "The user id")),
    responses(
        (status = 204, description = "Policy updated"),
        (status = 400, description = "A required provider id is missing or blank, or a nil user id"),
        (status = 403, description = "Policy update forbidden"),
        (status = 404, description = "User not found")
    ),
    tag = "ferrofin"
)]
async fn update_user_policy(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(DotnetGuid(user_id)): Path<DotnetGuid>,
    JsonBody(new_policy): JsonBody<UserPolicy>,
) -> Result<Response, ApiError> {
    // Model validation runs after the authorization policy and before the
    // action, so it answers ahead of the unknown user's 404.
    if let Some(problem) = policy_validation(&new_policy, "") {
        return Ok(problem.into_response());
    }
    load_user(&state, user_id).await?;
    // The guards run inside the save's transaction (`update_policy_guarded`),
    // so the tokens of a user it disabled go once it committed. Upstream
    // revokes first, which is the same once its guards have passed; here a
    // refused or failed save logs nobody out. The save's `UserUpdated` push
    // and device check have run by then, so a newly disabled user's open
    // sessions hear `UserUpdated` just before they are signed out.
    let saved = state
        .users
        .update_policy_guarded(user_id, &new_policy)
        .await?;
    if saved.newly_disabled {
        state.sessions.revoke_user_tokens(user_id, "").await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `UserPolicy`'s two `[Required(AllowEmptyStrings = false)]` members, checked
/// as ASP.NET's model validation checks them before the action runs: a member
/// the body leaves out or sends `null` (both bound as `""`) or blank
/// (`IsNullOrWhiteSpace`) is a `400` keyed `{prefix}{member}`. `prefix` is
/// `"Policy."` for the policy nested in a `UserDto` body.
fn policy_validation(policy: &UserPolicy, prefix: &str) -> Option<ProblemDetails> {
    ProblemDetails::members_validation(
        [
            (
                "AuthenticationProviderId",
                &policy.authentication_provider_id,
            ),
            (
                "PasswordResetProviderId",
                &policy.password_reset_provider_id,
            ),
        ]
        .into_iter()
        .filter(|(_, value)| value.trim().is_empty())
        .map(|(member, _)| {
            (
                format!("{prefix}{member}"),
                format!("The {member} field is required."),
            )
        }),
    )
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
        (status = 400, description = "A nil user id"),
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
    // `UpdateConfigurationAsync` raises no event, so nothing is pushed.
    state.users.update_configuration(user_id, &config).await?;
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
        (status = 400, description = "A nil user id"),
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
    let caller_is_admin = auth.is_api_key
        || match &auth.user {
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
                auth.remote_endpoint.as_deref().unwrap_or("127.0.0.1"),
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
    state
        .sessions
        .revoke_user_tokens(user_id, auth.token.as_ref().map_or("", Secret::expose))
        .await?;
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

/// `POST /Users/ForgotPassword` — begin password recovery.
///
/// Port of `UserController.ForgotPassword`: the user manager hands the request
/// to the account's saved password-reset provider
/// (`UserManager.StartForgotPasswordProcess`), the built-in PIN file for most.
/// Every caller gets the same answer, whether the name exists or not.
///
/// A missing or blank `EnteredUsername` is a `400` before anything else runs.
/// The request is local when its connection was accepted on the client's own
/// address (`HttpContext.IsLocal()`) or the client is on the local network.
/// A request from elsewhere is logged at warn with the client's address.
#[utoipa::path(
    post,
    path = "/Users/ForgotPassword",
    responses(
        (status = 200, description = "Forgot-password process started", body = ForgotPasswordResult),
        (status = 400, description = "A missing or blank EnteredUsername")
    ),
    tag = "ferrofin"
)]
async fn forgot_password(
    State(state): State<AppState>,
    parts: Parts,
    JsonBody(body): JsonBody<ForgotPasswordDto>,
) -> Result<Response, ApiError> {
    let entered_username = match body.entered_username.require(
        "Jellyfin.Api.Models.UserDtos.ForgotPasswordDto",
        "EnteredUsername",
        || body_parameter(&parts.method, parts.extensions.get()),
    ) {
        Ok(name) => name,
        Err(problem) => return Ok(problem.into_response()),
    };
    let ip = state.client_address(&parts);
    let local = crate::ip_access::is_local_connection(&parts.extensions, ip)
        || state.is_in_local_network(ip);
    if !local {
        // One line per remote request with no volume guard, as upstream's, and
        // as the IP gate logs each request it refuses.
        tracing::warn!(
            remote_ip = %ip,
            "password reset process initiated from outside the local network"
        );
    }
    Ok(Json(
        state
            .users
            .start_forgot_password_process(&entered_username, local)
            .await?,
    )
    .into_response())
}

/// `POST /Users/ForgotPassword/Pin` — redeem a recovery PIN.
///
/// Port of `UserController.ForgotPasswordPin`: each password-reset provider is
/// offered the PIN in turn (`UserManager.RedeemPasswordResetPin`). With the
/// built-in provider the PIN is case-sensitive with its dashes ignored, the
/// password becomes the PIN as entered, and the file is consumed only once the
/// password is saved. No provider redeeming it is a `404`, and a missing or
/// blank `Pin` is a `400` before any provider is asked.
#[utoipa::path(
    post,
    path = "/Users/ForgotPassword/Pin",
    responses(
        (status = 200, description = "Pin redemption result", body = PinRedeemResult),
        (status = 400, description = "A missing or blank Pin"),
        (status = 404, description = "No provider redeemed the PIN")
    ),
    tag = "ferrofin"
)]
async fn forgot_password_pin(
    State(state): State<AppState>,
    parts: Parts,
    JsonBody(body): JsonBody<ForgotPasswordPinDto>,
) -> Result<Response, ApiError> {
    let pin = match body.pin.require(
        "Jellyfin.Api.Models.UserDtos.ForgotPasswordPinDto",
        "Pin",
        || body_parameter(&parts.method, parts.extensions.get()),
    ) {
        Ok(pin) => pin,
        Err(problem) => return Ok(problem.into_response()),
    };
    Ok(Json(state.users.redeem_password_reset_pin(&pin).await?).into_response())
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

/// Loads a user by id, mapping the absent case to a `404`. A nil id is a `400`,
/// as `UserManager.GetUserById(Guid.Empty)` throws `ArgumentException`; an API
/// key's `userId ?? User.GetUserId()` is that empty id.
async fn load_user(state: &AppState, user_id: Uuid) -> Result<UserEntity, ApiError> {
    if user_id.is_nil() {
        return Err(ApiError::BadRequest("Guid can't be empty".to_owned()));
    }
    state
        .users
        .get_user_by_id(user_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("user not found".to_owned()))
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
    Path(DotnetGuid(user_id)): Path<DotnetGuid>,
    body: JsonBody<UserDto>,
) -> Result<Response, ApiError> {
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
    Path(DotnetGuid(user_id)): Path<DotnetGuid>,
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
    Path(DotnetGuid(user_id)): Path<DotnetGuid>,
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
        .route("/Users/{userId}/Authenticate", post(authenticate_by_id))
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

crate::query::query_parameters! {
    UserIdQuery {} => [("post", "/Users"), ("post", "/Users/Configuration"), ("post", "/Users/Password"), ("post", "/UserImage"), ("delete", "/UserImage")];
    GetUsersQuery {} => [("get", "/Users")];
    AuthenticateByIdQuery {} => [];
}

#[cfg(test)]
mod tests {
    use super::{ApiError, ERROR_BODY, ServiceError, StatusCode, header, login_failure};

    /// `ExceptionMiddleware`'s answer, whatever the failure says. The server's
    /// `login_failures` test drives the real refusals; a remote-access one needs
    /// a client outside the local network, so its error is pinned here.
    #[rstest::rstest]
    #[case::bad_credentials(
        ServiceError::unauthorized("Invalid username or password entered."),
        StatusCode::UNAUTHORIZED
    )]
    #[case::remote_access(
        ServiceError::Forbidden("remote access is not permitted for this user".to_owned()),
        StatusCode::FORBIDDEN
    )]
    #[tokio::test]
    async fn login_failure_answers_the_status_and_the_plain_body(
        #[case] error: ServiceError,
        #[case] status: StatusCode,
    ) {
        let response = login_failure(error).expect("a refusal is answered here");
        assert_eq!(response.status(), status);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(body, ERROR_BODY);
    }

    /// Only a `401` or `403` is a refused login; the rest keep the shared body.
    #[rstest::rstest]
    #[case::invalid_input(
        ServiceError::invalid_input("app is required"),
        "invalid input: app is required"
    )]
    #[case::not_found(
        ServiceError::not_found("unable to find request"),
        "not found: unable to find request"
    )]
    #[case::backend(ServiceError::backend("disk"), "backend error: disk")]
    fn login_failure_leaves_other_errors_to_the_caller(
        #[case] error: ServiceError,
        #[case] message: &str,
    ) {
        let error: ApiError = login_failure(error).expect_err("not a refused login");
        assert_eq!(error.to_string(), message);
    }
}
