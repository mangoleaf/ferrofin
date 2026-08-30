//! `DevicesController` — client-device registration and per-device options.
//!
//! Ports the elevation-gated `DevicesController` routes:
//! - `GET /Devices` — the devices visible to a user (all when the caller is an
//!   admin querying without a `userId`).
//! - `GET /Devices/Info` — one device's info DTO, by device id.
//! - `GET /Devices/Options` — a device's custom-options DTO, by device id.
//! - `POST /Devices/Options` — update a device's custom name.
//! - `DELETE /Devices` — delete one or more devices, logging out their sessions.
//!
//! Every route sits behind `[Authorize(Policy = RequiresElevation)]` upstream,
//! and [`RequireAdmin`] enforces that here.
//!
//! This file previously said the elevation policy was "applied at the
//! composition root's auth layer". No such layer existed, so `GET /Devices`
//! returned every device row — each carrying a plaintext `AccessToken`,
//! including an administrator's live token — to any authenticated caller.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::dto::{DeviceInfoDto, DeviceOptionsDto};
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::devices::DeviceQuery;
use uuid::Uuid;

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::extract::JsonBody;
use crate::state::AppState;

/// Query parameters for `GET /Devices`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetDevicesQuery {
    /// Optional. Restricts the result to a single user's devices; defaults to
    /// the authenticated caller (Jellyfin's `RequestHelpers.GetUserId`).
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// Query parameters carrying a single required device `id`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceIdQuery {
    /// The device id.
    #[serde(default)]
    id: Option<String>,
}

/// Query parameters for `DELETE /Devices` — the device ids to delete.
///
/// Jellyfin binds the controller's `string[] id` from a comma-delimited value
/// (`?id=a,b`); it is parsed into the id list in the handler.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteDevicesQuery {
    /// The device ids to delete (comma-delimited).
    #[serde(default)]
    id: Option<String>,
}

/// `GET /Devices` — the devices visible to a user.
///
/// Port of `DevicesController.GetDevices`: resolves the target user (the caller
/// when `userId` is absent, per `RequestHelpers.GetUserId`) and returns that
/// user's device info DTOs.
#[utoipa::path(
    get,
    path = "/Devices",
    params(("userId" = Option<String>, Query, description = "Gets or sets the user identifier")),
    responses((status = 200, description = "Devices retrieved", body = QueryResult<DeviceInfoDto>)),
    tag = "ferrofin"
)]
async fn get_devices(
    State(state): State<AppState>,
    RequireAdmin(auth): RequireAdmin,
    Query(query): Query<GetDevicesQuery>,
) -> Result<Json<QueryResult<DeviceInfoDto>>, ApiError> {
    let user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    let devices = state.devices.get_devices_for_user(Some(user_id)).await?;
    Ok(Json(devices))
}

/// `GET /Devices/Info` — one device's info DTO.
///
/// Port of `DevicesController.GetDeviceInfo`: looks up the device by id, `404`
/// when it does not exist.
#[utoipa::path(
    get,
    path = "/Devices/Info",
    params(("id" = String, Query, description = "Device Id")),
    responses(
        (status = 200, description = "Device info retrieved", body = DeviceInfoDto),
        (status = 404, description = "Device not found")
    ),
    tag = "ferrofin"
)]
async fn get_device_info(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(query): Query<DeviceIdQuery>,
) -> Result<Json<DeviceInfoDto>, ApiError> {
    let id = require_id(query.id.as_deref())?;
    match state.devices.get_device(id).await? {
        Some(info) => Ok(Json(info)),
        None => Err(ApiError::NotFound(format!("device {id}"))),
    }
}

/// `GET /Devices/Options` — a device's custom-options DTO.
///
/// Port of `DevicesController.GetDeviceOptions`: returns the device's stored
/// options (`404` when the device has none). The manager returns the persisted
/// `DeviceOptionsEntity` row (its documented stopgap); this handler projects it
/// to the [`DeviceOptionsDto`] wire shape.
#[utoipa::path(
    get,
    path = "/Devices/Options",
    params(("id" = String, Query, description = "Device Id")),
    responses(
        (status = 200, description = "Device options retrieved", body = DeviceOptionsDto),
        (status = 404, description = "Device not found")
    ),
    tag = "ferrofin"
)]
async fn get_device_options(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(query): Query<DeviceIdQuery>,
) -> Result<Json<DeviceOptionsDto>, ApiError> {
    let id = require_id(query.id.as_deref())?;
    match state.devices.get_device_options(id).await? {
        Some(options) => Ok(Json(DeviceOptionsDto {
            id: i32::try_from(options.id).unwrap_or(i32::MAX),
            device_id: Some(options.device_id),
            custom_name: options.custom_name,
        })),
        None => Err(ApiError::NotFound(format!("device options {id}"))),
    }
}

/// `POST /Devices/Options` — update a device's custom name.
///
/// Port of `DevicesController.UpdateDeviceOptions`: upserts the device's custom
/// name (only that field of the DTO is used, matching the C# call). Returns
/// `204 No Content`.
#[utoipa::path(
    post,
    path = "/Devices/Options",
    params(("id" = String, Query, description = "Device Id")),
    request_body = DeviceOptionsDto,
    responses((status = 204, description = "Device options updated")),
    tag = "ferrofin"
)]
async fn update_device_options(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(query): Query<DeviceIdQuery>,
    JsonBody(options): JsonBody<DeviceOptionsDto>,
) -> Result<axum::http::StatusCode, ApiError> {
    let id = require_id(query.id.as_deref())?;
    state
        .devices
        .update_device_options(id, options.custom_name.as_deref())
        .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `DELETE /Devices` — delete one or more devices and log out their sessions.
///
/// Port of `DevicesController.DeleteDevice`: every requested id must resolve to
/// an existing device (`400` otherwise); each device's active sessions are then
/// logged out. Returns `204 No Content`.
#[utoipa::path(
    delete,
    path = "/Devices",
    params(("id" = String, Query, description = "Device Ids (comma-delimited)")),
    responses(
        (status = 204, description = "Device deleted"),
        (status = 400, description = "A requested device is invalid")
    ),
    tag = "ferrofin"
)]
async fn delete_devices(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(query): Query<DeleteDevicesQuery>,
) -> Result<axum::http::StatusCode, ApiError> {
    // Comma-delimited id list (Jellyfin's `string[]` query binding).
    let ids: Vec<&str> = query
        .id
        .as_deref()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // Resolve every id first; a single unknown device fails the whole request
    // (C# returns `BadRequest` when `devices.Any(f => f is null)`).
    let mut resolved = Vec::with_capacity(ids.len());
    for id in ids {
        match state.devices.get_device(id).await? {
            Some(info) => resolved.push(info),
            None => return Err(ApiError::BadRequest(format!("unknown device {id}"))),
        }
    }

    for info in resolved {
        let device_id = info.id.clone().unwrap_or_default();
        let sessions = state
            .devices
            .get_devices(&DeviceQuery {
                device_id: Some(device_id),
                ..Default::default()
            })
            .await?;
        for session in &sessions.items {
            state.sessions.logout_device(session).await?;
        }
    }

    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Requires the `id` query parameter to be present and non-empty (`400`
/// otherwise), matching the C# `[Required]` binding on the `id` argument.
fn require_id(id: Option<&str>) -> Result<&str, ApiError> {
    match id {
        Some(id) if !id.is_empty() => Ok(id),
        _ => Err(ApiError::BadRequest("missing required 'id'".to_owned())),
    }
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Devices", get(get_devices).delete(delete_devices))
        .route("/Devices/Info", get(get_device_info))
        .route(
            "/Devices/Options",
            get(get_device_options).post(update_device_options),
        )
}
