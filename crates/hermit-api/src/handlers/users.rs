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
use hermit_db::entities::users::UserEntity;
use hermit_model::configuration::UserConfiguration;
use hermit_model::dto::UserDto;
use hermit_model::session::AuthenticationResult;
use hermit_model::users::{
    ForgotPasswordAction, ForgotPasswordResult, PinRedeemResult, UserPolicy,
};
use hermit_traits::options::AuthorizationInfo;
use hermit_traits::session::{AuthenticationRequest, AuthenticationResultData};
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

/// The `AuthenticateWithQuickConnect` request body (a Quick Connect secret).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QuickConnectDto {
    /// The Quick Connect secret returned by the initiate endpoint.
    #[serde(default)]
    secret: String,
}

/// The `POST /Users/New` request body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CreateUserByName {
    /// The new user's name.
    name: String,
    /// The optional initial password.
    #[serde(default)]
    password: Option<String>,
}

/// The `POST /Users/Password` request body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UpdateUserPassword {
    /// The current plaintext password (C# `CurrentPw`).
    #[serde(default, rename = "CurrentPw")]
    current_pw: Option<String>,
    /// The new plaintext password (C# `NewPw`).
    #[serde(default, rename = "NewPw")]
    new_pw: Option<String>,
    /// Whether to reset (clear) the password instead of changing it.
    #[serde(default)]
    reset_password: bool,
}

/// The `POST /Users/ForgotPassword` request body.
///
/// The field is accepted for wire compatibility but unused: the pluggable
/// password-reset provider is a deferred subsystem, so the handler returns the
/// default "contact admin" action without consulting the username.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ForgotPasswordDto {
    /// The username whose password should be reset.
    #[serde(default)]
    #[allow(dead_code)]
    entered_username: String,
}

/// The `POST /Users/ForgotPassword/Pin` request body.
///
/// The field is accepted for wire compatibility but unused: with the reset
/// subsystem deferred, no pin is ever issued, so redemption always reports
/// failure.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ForgotPasswordPinDto {
    /// The entered pin.
    #[serde(default)]
    #[allow(dead_code)]
    pin: String,
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
/// Reuses [`UserManager::get_user_dto`](hermit_traits::library::UserManager::get_user_dto)
/// (whose policy projection reads the `Permissions` table) rather than adding a
/// dedicated permission accessor, matching C# `user.HasPermission(IsAdministrator)`.
async fn is_administrator(state: &AppState, user: &UserEntity) -> Result<bool, ApiError> {
    Ok(state
        .users
        .get_user_dto(user, None)
        .await?
        .policy
        .is_some_and(|p| p.is_administrator))
}

/// Ports C# `RequestHelpers.AssertCanUpdateUser`: the caller may update `target`
/// when the caller is an administrator, or when the caller *is* the target.
async fn assert_can_update_user(
    state: &AppState,
    auth: &AuthorizationInfo,
    target: &UserEntity,
) -> Result<(), ApiError> {
    let caller_id = auth.user_id();
    if caller_id == Uuid::parse_str(&target.id).unwrap_or_else(|_| Uuid::nil()) {
        return Ok(());
    }
    if let Some(caller) = &auth.user
        && is_administrator(state, caller).await?
    {
        return Ok(());
    }
    Err(ApiError::Forbidden("user update not allowed".to_owned()))
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
    let result = state.sessions.authenticate_new_session(&request).await?;
    Ok(Json(authentication_result(result)))
}

/// `POST /Users/AuthenticateWithQuickConnect` — finish a Quick Connect login.
///
/// Port of `UserController.AuthenticateWithQuickConnect`: exchanges an authorized
/// Quick Connect secret for a session.
#[utoipa::path(
    post,
    path = "/Users/AuthenticateWithQuickConnect",
    responses((status = 200, description = "User authenticated (AuthenticationResult)")),
    tag = "hermit"
)]
async fn authenticate_with_quick_connect(
    State(state): State<AppState>,
    Json(body): Json<QuickConnectDto>,
) -> Result<Json<AuthenticationResult>, ApiError> {
    let session = state
        .quick_connect
        .get_authorized_request(&body.secret)
        .await?;
    // The Quick Connect trait surfaces only the session DTO (its token is not
    // carried through this seam), so `access_token` resolves to `None`.
    Ok(Json(authentication_result(AuthenticationResultData {
        session,
        access_token: String::new(),
    })))
}

/// Assembles the [`AuthenticationResult`] wire body from an authenticated
/// session and its minted access token.
///
/// The token is echoed back to the client as `AccessToken` so subsequent
/// requests can authenticate; a genuinely empty token (e.g. the Quick Connect
/// seam that does not carry one) collapses to `None`.
fn authentication_result(result: AuthenticationResultData) -> AuthenticationResult {
    let AuthenticationResultData {
        session,
        access_token,
    } = result;
    let user = UserDto {
        id: session.user_id,
        name: session.user_name.clone(),
        ..UserDto::default()
    };
    let server_id = session.server_id.clone();
    AuthenticationResult {
        user: Some(user),
        session_info: Some(session),
        access_token: (!access_token.is_empty()).then_some(access_token),
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
    tag = "hermit"
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
/// Port of `UserController.GetPublicUsers`. Before the startup wizard completes,
/// every user is returned; afterwards only non-hidden, non-disabled users are.
/// The device/network narrowing the C# applies needs a network manager (deferred
/// at this layer), so it is not applied.
#[utoipa::path(
    get,
    path = "/Users/Public",
    responses((status = 200, description = "Public users returned", body = [UserDto])),
    tag = "hermit"
)]
async fn get_public_users(State(state): State<AppState>) -> Result<Json<Vec<UserDto>>, ApiError> {
    let wizard_done = state
        .config
        .configuration()
        .await?
        .is_startup_wizard_completed;
    let dtos = if wizard_done {
        filtered_user_dtos(&state, Some(false), Some(false)).await?
    } else {
        filtered_user_dtos(&state, None, None).await?
    };
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
    tag = "hermit"
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
    tag = "hermit"
)]
async fn delete_user(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(user_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    load_user(&state, user_id).await?;
    state.sessions.revoke_user_tokens(user_id, "").await?;
    state.users.delete_user(user_id).await?;
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
    tag = "hermit"
)]
async fn create_user_by_name(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Json(body): Json<CreateUserByName>,
) -> Result<Json<UserDto>, ApiError> {
    let new_user = state.users.create_user(&body.name).await?;
    if let Some(password) = &body.password {
        let id = Uuid::parse_str(&new_user.id).unwrap_or_else(|_| Uuid::nil());
        state.users.change_password(id, password).await?;
    }
    // Reload so the DTO reflects the password just set.
    let id = Uuid::parse_str(&new_user.id).unwrap_or_else(|_| Uuid::nil());
    let reloaded = load_user(&state, id).await?;
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
    tag = "hermit"
)]
async fn update_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UserIdQuery>,
    Json(body): Json<UserDto>,
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
    tag = "hermit"
)]
async fn update_user_policy(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(user_id): Path<Uuid>,
    Json(new_policy): Json<UserPolicy>,
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
    tag = "hermit"
)]
async fn update_user_configuration(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UserIdQuery>,
    Json(config): Json<UserConfiguration>,
) -> Result<StatusCode, ApiError> {
    let user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    let user = load_user(&state, user_id).await?;
    assert_can_update_user(&state, &auth, &user).await?;
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
        (status = 403, description = "Update not allowed or invalid credentials"),
        (status = 404, description = "User not found")
    ),
    tag = "hermit"
)]
async fn update_user_password(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<UserIdQuery>,
    Json(body): Json<UpdateUserPassword>,
) -> Result<StatusCode, ApiError> {
    let user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    let user = load_user(&state, user_id).await?;
    assert_can_update_user(&state, &auth, &user).await?;

    if body.reset_password {
        state.users.reset_password(user_id).await?;
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
                body.current_pw.as_deref().unwrap_or_default(),
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
        .change_password(user_id, body.new_pw.as_deref().unwrap_or_default())
        .await?;
    state.sessions.revoke_user_tokens(user_id, "").await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Users/ForgotPassword` — begin the forgot-password flow.
///
/// Port of `UserController.ForgotPassword`. The pluggable password-reset provider
/// subsystem is deferred; the ported built-in behaviour directs the user to
/// contact the administrator (the default provider's response for a
/// non-in-network request), which is the safe, faithful default.
#[utoipa::path(
    post,
    path = "/Users/ForgotPassword",
    responses((status = 200, description = "Forgot-password process started", body = ForgotPasswordResult)),
    tag = "hermit"
)]
async fn forgot_password(
    State(_state): State<AppState>,
    Json(_body): Json<ForgotPasswordDto>,
) -> Result<Json<ForgotPasswordResult>, ApiError> {
    Ok(Json(ForgotPasswordResult {
        action: ForgotPasswordAction::ContactAdmin,
        pin_file: None,
        pin_expiration_date: None,
    }))
}

/// `POST /Users/ForgotPassword/Pin` — redeem a forgot-password pin.
///
/// Port of `UserController.ForgotPasswordPin`. With the reset-provider subsystem
/// deferred, no pin is ever issued, so redemption always reports failure — the
/// faithful outcome for a server with no active reset flow.
#[utoipa::path(
    post,
    path = "/Users/ForgotPassword/Pin",
    responses((status = 200, description = "Pin redemption result", body = PinRedeemResult)),
    tag = "hermit"
)]
async fn forgot_password_pin(
    State(_state): State<AppState>,
    Json(_body): Json<ForgotPasswordPinDto>,
) -> Result<Json<PinRedeemResult>, ApiError> {
    Ok(Json(PinRedeemResult {
        success: false,
        users_reset: Vec::new(),
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
    Ok(Json(state.users.get_user_dto(&user, None).await?))
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
        .route("/Users/{userId}", get(get_user_by_id).delete(delete_user))
        .route("/Users/New", post(create_user_by_name))
        .route("/Users/{userId}/Policy", post(update_user_policy))
        .route("/Users/Configuration", post(update_user_configuration))
        .route("/Users/Password", post(update_user_password))
        .route("/Users/ForgotPassword", post(forgot_password))
        .route("/Users/ForgotPassword/Pin", post(forgot_password_pin))
        .route("/Users/Me", get(get_current_user))
}
