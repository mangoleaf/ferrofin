//! `SessionController` — session listing, remote commands, capabilities, logout.
//!
//! Ports Jellyfin's `SessionController`:
//!
//! - `GET  /Sessions` — list sessions visible to the caller.
//! - `POST /Sessions/{sessionId}/Viewing` — browse a client to an item.
//! - `POST /Sessions/{sessionId}/Playing` — play items on a client.
//! - `POST /Sessions/{sessionId}/Playing/{command}` — issue a playstate command.
//! - `POST /Sessions/{sessionId}/System/{command}` — issue a system command.
//! - `POST /Sessions/{sessionId}/Command/{command}` — issue a general command.
//! - `POST /Sessions/{sessionId}/Command` — issue a full general command.
//! - `POST /Sessions/{sessionId}/Message` — display a message on a client.
//! - `POST`/`DELETE /Sessions/{sessionId}/User/{userId}` — add / remove a guest.
//! - `POST /Sessions/Capabilities` — post capability flags for a device.
//! - `POST /Sessions/Capabilities/Full` — post the full capability object.
//! - `POST /Sessions/Viewing` — report the item a session is now viewing.
//! - `POST /Sessions/Logout` — report the caller's session ended.
//! - `GET  /Auth/Providers` — list authentication providers.
//! - `GET  /Auth/PasswordResetProviders` — list password-reset providers.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::{ClientCapabilitiesDto, NameIdPair, SessionInfoDto};
use ferrofin_model::secret::Secret;
use ferrofin_model::session::{
    BrowseRequest, ClientCapabilities, GeneralCommand, GeneralCommandType, MessageCommand,
    PlayCommand, PlayRequest, PlaystateCommand, PlaystateRequest,
};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::query_parse::{parse_csv_enums, parse_csv_uuids};
use crate::handlers::session_ctx::{current_session, current_session_id};
use crate::state::AppState;

/// Query parameters for `GET /Sessions`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetSessionsQuery {
    /// Filter to sessions a given user may remote-control.
    #[serde(default)]
    controllable_by_user_id: Option<Uuid>,
    /// Filter by device id.
    #[serde(default)]
    device_id: Option<String>,
    /// Filter to sessions active in the last n seconds.
    #[serde(default)]
    active_within_seconds: Option<i32>,
}

/// `GET /Sessions` — the sessions visible to the caller.
///
/// Port of `SessionController.GetSessions`.
#[utoipa::path(
    get,
    path = "/Sessions",
    responses((status = 200, description = "List of sessions (Vec<SessionInfoDto>)")),
    tag = "ferrofin"
)]
async fn get_sessions(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<GetSessionsQuery>,
) -> Result<Json<Vec<SessionInfoDto>>, ApiError> {
    let sessions = state
        .sessions
        .get_sessions(
            auth.user_id(),
            query.device_id.as_deref(),
            query.active_within_seconds,
            query.controllable_by_user_id,
            auth.is_api_key,
        )
        .await?;
    Ok(Json(sessions))
}

/// Query parameters for `POST /Sessions/{sessionId}/Viewing`.
///
/// The `item*` prefix mirrors the vendored query-parameter names (`itemType` /
/// `itemId` / `itemName`), so the shared prefix is intentional.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_field_names)]
struct DisplayContentQuery {
    /// The type of item to browse to.
    item_type: BaseItemKind,
    /// The id of the item.
    item_id: String,
    /// The name of the item.
    item_name: String,
}

/// `POST /Sessions/{sessionId}/Viewing` — browse a client to an item.
///
/// Port of `SessionController.DisplayContent`.
#[utoipa::path(
    post,
    path = "/Sessions/{sessionId}/Viewing",
    params(("sessionId" = String, Path, description = "The session id")),
    responses((status = 204, description = "Instruction sent to session")),
    tag = "ferrofin"
)]
async fn display_content(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(session_id): Path<String>,
    Query(query): Query<DisplayContentQuery>,
) -> Result<StatusCode, ApiError> {
    let command = BrowseRequest {
        item_id: Some(query.item_id),
        item_name: Some(query.item_name),
        item_type: query.item_type,
    };
    // Browse is a general command carrying the browse payload as arguments (C#
    // `SendBrowseCommand` builds a `DisplayContent` general command).
    let mut arguments = HashMap::new();
    arguments.insert("ItemType".to_owned(), enum_token(&command.item_type));
    if let Some(id) = &command.item_id {
        arguments.insert("ItemId".to_owned(), id.clone());
    }
    if let Some(name) = &command.item_name {
        arguments.insert("ItemName".to_owned(), name.clone());
    }
    let controlling = current_session(&state, &auth).await?;
    let general = GeneralCommand {
        name: GeneralCommandType::DisplayContent,
        controlling_user_id: controlling.user_id,
        arguments,
    };
    let controlling_id = session_id_of(&controlling)?;
    state
        .sessions
        .send_general_command(&controlling_id, &session_id, &general)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `POST /Sessions/{sessionId}/Playing`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlayQuery {
    /// The type of play command (`PlayNow`/`PlayNext`/`PlayLast`).
    play_command: PlayCommand,
    /// Comma-delimited item ids to play.
    item_ids: String,
    #[serde(default)]
    start_position_ticks: Option<i64>,
    #[serde(default)]
    media_source_id: Option<String>,
    #[serde(default)]
    audio_stream_index: Option<i32>,
    #[serde(default)]
    subtitle_stream_index: Option<i32>,
    #[serde(default)]
    start_index: Option<i32>,
}

/// `POST /Sessions/{sessionId}/Playing` — play items on a client.
///
/// Port of `SessionController.Play`.
#[utoipa::path(
    post,
    path = "/Sessions/{sessionId}/Playing",
    params(("sessionId" = String, Path, description = "The session id")),
    responses((status = 204, description = "Instruction sent to session")),
    tag = "ferrofin"
)]
async fn play(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(session_id): Path<String>,
    Query(query): Query<PlayQuery>,
) -> Result<StatusCode, ApiError> {
    let play_request = PlayRequest {
        item_ids: parse_csv_uuids(Some(&query.item_ids))?,
        start_position_ticks: query.start_position_ticks,
        play_command: query.play_command,
        media_source_id: query.media_source_id,
        audio_stream_index: query.audio_stream_index,
        subtitle_stream_index: query.subtitle_stream_index,
        start_index: query.start_index,
        ..PlayRequest::default()
    };
    let controlling_id = current_session_id(&state, &auth).await?;
    state
        .sessions
        .send_play_command(&controlling_id, &session_id, &play_request)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `POST /Sessions/{sessionId}/Playing/{command}`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaystateQuery {
    #[serde(default)]
    seek_position_ticks: Option<i64>,
    #[serde(default)]
    controlling_user_id: Option<String>,
}

/// `POST /Sessions/{sessionId}/Playing/{command}` — issue a playstate command.
///
/// Port of `SessionController.SendPlaystateCommand`.
#[utoipa::path(
    post,
    path = "/Sessions/{sessionId}/Playing/{command}",
    params(
        ("sessionId" = String, Path, description = "The session id"),
        ("command" = String, Path, description = "The playstate command")
    ),
    responses((status = 204, description = "Playstate command sent to session")),
    tag = "ferrofin"
)]
async fn send_playstate_command(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((session_id, command)): Path<(String, PlaystateCommand)>,
    Query(query): Query<PlaystateQuery>,
) -> Result<StatusCode, ApiError> {
    let request = PlaystateRequest {
        command,
        controlling_user_id: query.controlling_user_id,
        seek_position_ticks: query.seek_position_ticks,
    };
    let controlling_id = current_session_id(&state, &auth).await?;
    state
        .sessions
        .send_playstate_command(&controlling_id, &session_id, &request)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Sessions/{sessionId}/System/{command}` — issue a system command.
///
/// Port of `SessionController.SendSystemCommand` (a general command whose name is
/// the system command).
#[utoipa::path(
    post,
    path = "/Sessions/{sessionId}/System/{command}",
    params(
        ("sessionId" = String, Path, description = "The session id"),
        ("command" = String, Path, description = "The system command")
    ),
    responses((status = 204, description = "System command sent to session")),
    tag = "ferrofin"
)]
async fn send_system_command(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((session_id, command)): Path<(String, GeneralCommandType)>,
) -> Result<StatusCode, ApiError> {
    send_named_command(&state, &auth, &session_id, command).await
}

/// `POST /Sessions/{sessionId}/Command/{command}` — issue a general command.
///
/// Port of `SessionController.SendGeneralCommand`.
#[utoipa::path(
    post,
    path = "/Sessions/{sessionId}/Command/{command}",
    params(
        ("sessionId" = String, Path, description = "The session id"),
        ("command" = String, Path, description = "The general command")
    ),
    responses((status = 204, description = "General command sent to session")),
    tag = "ferrofin"
)]
async fn send_general_command(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((session_id, command)): Path<(String, GeneralCommandType)>,
) -> Result<StatusCode, ApiError> {
    send_named_command(&state, &auth, &session_id, command).await
}

/// `POST /Sessions/{sessionId}/Command` — issue a full general command.
///
/// Port of `SessionController.SendFullGeneralCommand`.
#[utoipa::path(
    post,
    path = "/Sessions/{sessionId}/Command",
    params(("sessionId" = String, Path, description = "The session id")),
    request_body = GeneralCommand,
    responses((status = 204, description = "Full general command sent to session")),
    tag = "ferrofin"
)]
async fn send_full_general_command(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(session_id): Path<String>,
    Json(mut command): Json<GeneralCommand>,
) -> Result<StatusCode, ApiError> {
    let controlling = current_session(&state, &auth).await?;
    command.controlling_user_id = controlling.user_id;
    let controlling_id = session_id_of(&controlling)?;
    state
        .sessions
        .send_general_command(&controlling_id, &session_id, &command)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Sessions/{sessionId}/Message` — display a message on a client.
///
/// Port of `SessionController.SendMessageCommand`.
#[utoipa::path(
    post,
    path = "/Sessions/{sessionId}/Message",
    params(("sessionId" = String, Path, description = "The session id")),
    request_body = MessageCommand,
    responses((status = 204, description = "Message sent")),
    tag = "ferrofin"
)]
async fn send_message_command(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(session_id): Path<String>,
    Json(mut command): Json<MessageCommand>,
) -> Result<StatusCode, ApiError> {
    // C# defaults a blank header to "Message from Server".
    if command.header.as_deref().is_none_or(str::is_empty) {
        command.header = Some("Message from Server".to_owned());
    }
    let controlling_id = current_session_id(&state, &auth).await?;
    state
        .sessions
        .send_message_command(&controlling_id, &session_id, &command)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Sessions/{sessionId}/User/{userId}` — add a guest user to a session.
///
/// Port of `SessionController.AddUserToSession`.
#[utoipa::path(
    post,
    path = "/Sessions/{sessionId}/User/{userId}",
    params(
        ("sessionId" = String, Path, description = "The session id"),
        ("userId" = String, Path, description = "The user id")
    ),
    responses((status = 204, description = "User added to session")),
    tag = "ferrofin"
)]
async fn add_user_to_session(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((session_id, user_id)): Path<(String, Uuid)>,
) -> Result<StatusCode, ApiError> {
    state
        .sessions
        .add_additional_user(&session_id, user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /Sessions/{sessionId}/User/{userId}` — remove a guest user.
///
/// Port of `SessionController.RemoveUserFromSession`.
#[utoipa::path(
    delete,
    path = "/Sessions/{sessionId}/User/{userId}",
    params(
        ("sessionId" = String, Path, description = "The session id"),
        ("userId" = String, Path, description = "The user id")
    ),
    responses((status = 204, description = "User removed from session")),
    tag = "ferrofin"
)]
async fn remove_user_from_session(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((session_id, user_id)): Path<(String, Uuid)>,
) -> Result<StatusCode, ApiError> {
    state
        .sessions
        .remove_additional_user(&session_id, user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `POST /Sessions/Capabilities`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CapabilitiesQuery {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    playable_media_types: Option<String>,
    #[serde(default)]
    supported_commands: Option<String>,
    #[serde(default)]
    supports_media_control: Option<bool>,
    #[serde(default)]
    supports_persistent_identifier: Option<bool>,
}

/// `POST /Sessions/Capabilities` — post capability flags for a device.
///
/// Port of `SessionController.PostCapabilities`.
#[utoipa::path(
    post,
    path = "/Sessions/Capabilities",
    responses((status = 204, description = "Capabilities posted")),
    tag = "ferrofin"
)]
async fn post_capabilities(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<CapabilitiesQuery>,
) -> Result<StatusCode, ApiError> {
    let id = match query.id {
        Some(id) if !id.is_empty() => id,
        _ => current_session_id(&state, &auth).await?,
    };
    let playable_media_types: Vec<MediaType> =
        parse_csv_enums(query.playable_media_types.as_deref())?;
    let supported_commands: Vec<GeneralCommandType> =
        parse_csv_enums(query.supported_commands.as_deref())?;
    let capabilities = ClientCapabilities {
        playable_media_types,
        supported_commands,
        supports_media_control: query.supports_media_control.unwrap_or(false),
        supports_persistent_identifier: query.supports_persistent_identifier.unwrap_or(true),
        ..ClientCapabilities::default()
    };
    state
        .sessions
        .report_capabilities(&id, &capabilities)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `POST /Sessions/Capabilities/Full`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FullCapabilitiesQuery {
    #[serde(default)]
    id: Option<String>,
}

/// `POST /Sessions/Capabilities/Full` — post the full capability object.
///
/// Port of `SessionController.PostFullCapabilities`.
#[utoipa::path(
    post,
    path = "/Sessions/Capabilities/Full",
    request_body = ClientCapabilitiesDto,
    responses((status = 204, description = "Capabilities updated")),
    tag = "ferrofin"
)]
async fn post_full_capabilities(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<FullCapabilitiesQuery>,
    Json(capabilities): Json<ClientCapabilitiesDto>,
) -> Result<StatusCode, ApiError> {
    let id = match query.id {
        Some(id) if !id.is_empty() => id,
        _ => current_session_id(&state, &auth).await?,
    };
    state
        .sessions
        .report_capabilities(&id, &capabilities.to_client_capabilities())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `POST /Sessions/Viewing`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReportViewingQuery {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    item_id: Option<String>,
}

/// `POST /Sessions/Viewing` — report the item a session is now viewing.
///
/// Port of `SessionController.ReportViewing`.
#[utoipa::path(
    post,
    path = "/Sessions/Viewing",
    responses((status = 204, description = "Session reported to server")),
    tag = "ferrofin"
)]
async fn report_viewing(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ReportViewingQuery>,
) -> Result<StatusCode, ApiError> {
    let session = match query.session_id {
        Some(id) if !id.is_empty() => id,
        _ => current_session_id(&state, &auth).await?,
    };
    state
        .sessions
        .report_now_viewing_item(&session, query.item_id.as_deref().unwrap_or_default())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Sessions/Logout` — report the caller's session ended.
///
/// Port of `SessionController.ReportSessionEnded` (logs out the caller's token).
#[utoipa::path(
    post,
    path = "/Sessions/Logout",
    responses((status = 204, description = "Session end reported to server")),
    tag = "ferrofin"
)]
async fn report_session_ended(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
) -> Result<StatusCode, ApiError> {
    let token = auth
        .token
        .as_ref()
        .map(Secret::expose)
        .ok_or_else(|| ApiError::Unauthorized("no access token".to_owned()))?;
    state.sessions.logout(token).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Auth/Providers` — the authentication providers.
///
/// Port of `SessionController.GetAuthProviders`.
#[utoipa::path(
    get,
    path = "/Auth/Providers",
    responses((status = 200, description = "Auth providers (Vec<NameIdPair>)")),
    tag = "ferrofin"
)]
async fn get_auth_providers(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<Vec<NameIdPair>>, ApiError> {
    let providers = state.users.get_authentication_providers().await?;
    Ok(Json(providers))
}

/// `GET /Auth/PasswordResetProviders` — the password-reset providers.
///
/// Port of `SessionController.GetPasswordResetProviders`.
#[utoipa::path(
    get,
    path = "/Auth/PasswordResetProviders",
    responses((status = 200, description = "Password reset providers (Vec<NameIdPair>)")),
    tag = "ferrofin"
)]
async fn get_password_reset_providers(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<Vec<NameIdPair>>, ApiError> {
    let providers = state.users.get_password_reset_providers().await?;
    Ok(Json(providers))
}

/// Sends a named (argument-less) general command, stamping the caller's user id
/// as the controlling user (C# `SendSystemCommand` / `SendGeneralCommand`).
async fn send_named_command(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    session_id: &str,
    command: GeneralCommandType,
) -> Result<StatusCode, ApiError> {
    let controlling = current_session(state, auth).await?;
    let general = GeneralCommand {
        name: command,
        controlling_user_id: controlling.user_id,
        arguments: HashMap::new(),
    };
    let controlling_id = session_id_of(&controlling)?;
    state
        .sessions
        .send_general_command(&controlling_id, session_id, &general)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The id of the caller's own session, or a `404` if it has none.
fn session_id_of(session: &SessionInfoDto) -> Result<String, ApiError> {
    session
        .id
        .clone()
        .ok_or_else(|| ApiError::NotFound("Session not found.".to_owned()))
}

/// The PascalCase serde token for an enum value (used as a command argument).
fn enum_token<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Sessions", get(get_sessions))
        .route("/Sessions/{sessionId}/Viewing", post(display_content))
        .route("/Sessions/{sessionId}/Playing", post(play))
        .route(
            "/Sessions/{sessionId}/Playing/{command}",
            post(send_playstate_command),
        )
        .route(
            "/Sessions/{sessionId}/System/{command}",
            post(send_system_command),
        )
        .route(
            "/Sessions/{sessionId}/Command/{command}",
            post(send_general_command),
        )
        .route(
            "/Sessions/{sessionId}/Command",
            post(send_full_general_command),
        )
        .route("/Sessions/{sessionId}/Message", post(send_message_command))
        .route(
            "/Sessions/{sessionId}/User/{userId}",
            post(add_user_to_session).delete(remove_user_from_session),
        )
        .route("/Sessions/Capabilities", post(post_capabilities))
        .route("/Sessions/Capabilities/Full", post(post_full_capabilities))
        .route("/Sessions/Viewing", post(report_viewing))
        .route("/Sessions/Logout", post(report_session_ended))
        .route("/Auth/Providers", get(get_auth_providers))
        .route(
            "/Auth/PasswordResetProviders",
            get(get_password_reset_providers),
        )
}
