//! `DisplayPreferencesController` — per-user, per-client display settings.
//!
//! Ports `GET`/`POST /DisplayPreferences/{displayPreferencesId}`.
//!
//! The `displayPreferencesId` is either a `Guid` or an arbitrary string; when it
//! is not a `Guid` the C# hashes it with MD5 to a stable id
//! (`displayPreferencesId.GetMD5()`). Both are reproduced here.
//!
//! The GET action assembles a [`DisplayPreferencesDto`] from the flat
//! display-preferences row, the item sort/index preferences, and the custom
//! key/value preferences, folding the row's scalar fields (chromecast version,
//! skip lengths, overlay flag, theme, TV home) into `CustomPrefs` under the
//! well-known keys the web client expects. The POST action reverses that: it
//! parses those keys back onto the row, persists the item preferences and the
//! remaining custom preferences, and saves the row.
//!
//! Departure: Jellyfin also (de)serializes the row's `HomeSections` children as
//! `homesection{n}` custom prefs. The `hermit-traits` display-preferences seam
//! returns the *flat* row (home sections are a separate concern loaded by the
//! DTO layer), so home-section round-tripping is deferred with that seam; the
//! scalar and item/custom preferences are fully ported.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use hermit_common::extensions::get_md5;
use hermit_db::entities::display_preferences::{
    DisplayPreferencesEntity, ItemDisplayPreferencesEntity,
};
use hermit_model::dto::{DisplayPreferencesDto, ScrollDirection, SortOrder};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// The default skip length (ms) the web client uses when the custom pref is
/// absent (C# `? … : 15000`).
const DEFAULT_SKIP_LENGTH_MS: i32 = 15000;

/// Query parameters for the display-preferences routes.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisplayPreferencesParams {
    /// Optional user id; defaults to the authenticated caller.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The required client name.
    #[serde(default)]
    client: Option<String>,
}

/// Resolves a `displayPreferencesId` into an item id: parse it as a `Guid`, else
/// hash it with MD5 (C# `Guid.TryParse … else GetMD5()`).
fn resolve_item_id(raw: &str) -> Uuid {
    Uuid::parse_str(raw).unwrap_or_else(|_| get_md5(raw))
}

/// Maps a stored `ScrollDirection` discriminant to the DTO enum
/// (`0` → Horizontal, else Vertical).
fn scroll_direction_from_i32(value: i32) -> ScrollDirection {
    if value == 1 {
        ScrollDirection::Vertical
    } else {
        ScrollDirection::Horizontal
    }
}

/// Maps a DTO `ScrollDirection` to its stored discriminant.
fn scroll_direction_to_i32(dir: ScrollDirection) -> i32 {
    match dir {
        ScrollDirection::Horizontal => 0,
        ScrollDirection::Vertical => 1,
    }
}

/// Maps a stored `SortOrder` discriminant to the DTO enum
/// (`0` → Ascending, else Descending).
fn sort_order_from_i32(value: i32) -> SortOrder {
    if value == 1 {
        SortOrder::Descending
    } else {
        SortOrder::Ascending
    }
}

/// Maps a DTO `SortOrder` to its stored discriminant.
fn sort_order_to_i32(order: SortOrder) -> i32 {
    match order {
        SortOrder::Ascending => 0,
        SortOrder::Descending => 1,
    }
}

/// The `IndexingKind` display name for a stored discriminant (C# `IndexBy?.ToString()`).
fn index_by_name(value: Option<i32>) -> Option<String> {
    match value {
        Some(0) => Some("PremiereDate".to_owned()),
        Some(1) => Some("ProductionYear".to_owned()),
        Some(2) => Some("CommunityRating".to_owned()),
        _ => None,
    }
}

/// Parses an `IndexingKind` display name back to its stored discriminant.
fn index_by_from_name(name: Option<&str>) -> Option<i32> {
    match name.map(str::to_ascii_lowercase) {
        Some(n) if n == "premieredate" => Some(0),
        Some(n) if n == "productionyear" => Some(1),
        Some(n) if n == "communityrating" => Some(2),
        _ => None,
    }
}

/// `GET /DisplayPreferences/{displayPreferencesId}` — the assembled DTO.
///
/// Port of `DisplayPreferencesController.GetDisplayPreferences`.
#[utoipa::path(
    get,
    path = "/DisplayPreferences/{displayPreferencesId}",
    params(
        ("displayPreferencesId" = String, Path, description = "Display preferences id."),
        ("userId" = Option<String>, Query, description = "User id."),
        ("client" = String, Query, description = "Client.")
    ),
    responses((status = 200, description = "Display preferences retrieved", body = DisplayPreferencesDto)),
    tag = "hermit"
)]
async fn get_display_preferences(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(display_preferences_id): Path<String>,
    Query(params): Query<DisplayPreferencesParams>,
) -> Result<Json<DisplayPreferencesDto>, ApiError> {
    let user_id = params.user_id.unwrap_or_else(|| auth.user_id());
    let client = params
        .client
        .filter(|c| !c.is_empty())
        .ok_or_else(|| ApiError::BadRequest("missing required 'client'".to_owned()))?;
    let item_id = resolve_item_id(&display_preferences_id);

    let prefs = state
        .display_preferences
        .get_display_preferences(user_id, item_id, &client)
        .await?;
    let item_prefs = state
        .display_preferences
        .get_item_display_preferences(user_id, item_id, &client)
        .await?;
    let custom = state
        .display_preferences
        .list_custom_item_display_preferences(user_id, item_id, &client)
        .await?;

    let mut custom_prefs: HashMap<String, Option<String>> = HashMap::new();
    // Scalar row fields the web client reads out of CustomPrefs.
    custom_prefs.insert(
        "chromecastVersion".to_owned(),
        Some(chromecast_name(prefs.chromecast_version)),
    );
    custom_prefs.insert(
        "skipForwardLength".to_owned(),
        Some(prefs.skip_forward_length.to_string()),
    );
    custom_prefs.insert(
        "skipBackLength".to_owned(),
        Some(prefs.skip_backward_length.to_string()),
    );
    custom_prefs.insert(
        "enableNextVideoInfoOverlay".to_owned(),
        // C# `bool.ToString()` is capitalized ("True"/"False"), which Jellyfin
        // writes verbatim into CustomPrefs — match it, not Rust's "true"/"false".
        Some(
            if prefs.enable_next_video_info_overlay {
                "True"
            } else {
                "False"
            }
            .to_owned(),
        ),
    );
    // Jellyfin writes the raw (nullable) TvHome/DashboardTheme — unset ⇒ JSON null,
    // not an empty string.
    custom_prefs.insert("tvhome".to_owned(), prefs.tv_home.clone());
    custom_prefs.insert("dashboardTheme".to_owned(), prefs.dashboard_theme.clone());
    // Stored custom preferences (do not overwrite the scalar keys above).
    for (key, value) in custom {
        custom_prefs.entry(key).or_insert(value);
    }

    let dto = DisplayPreferencesDto {
        id: Some(prefs.item_id.clone()),
        view_type: None,
        sort_by: Some(item_prefs.sort_by.clone()),
        index_by: index_by_name(prefs.index_by),
        remember_indexing: item_prefs.remember_indexing,
        primary_image_height: 250,
        primary_image_width: 250,
        custom_prefs,
        scroll_direction: scroll_direction_from_i32(prefs.scroll_direction),
        show_backdrop: prefs.show_backdrop,
        remember_sorting: item_prefs.remember_sorting,
        sort_order: sort_order_from_i32(item_prefs.sort_order),
        show_sidebar: prefs.show_sidebar,
        client: Some(prefs.client.clone()),
    };
    Ok(Json(dto))
}

/// `POST /DisplayPreferences/{displayPreferencesId}` — persist the DTO.
///
/// Port of `DisplayPreferencesController.UpdateDisplayPreferences`.
#[utoipa::path(
    post,
    path = "/DisplayPreferences/{displayPreferencesId}",
    params(
        ("displayPreferencesId" = String, Path, description = "Display preferences id."),
        ("userId" = Option<String>, Query, description = "User id."),
        ("client" = String, Query, description = "Client.")
    ),
    request_body = DisplayPreferencesDto,
    responses((status = 204, description = "Display preferences updated")),
    tag = "hermit"
)]
async fn update_display_preferences(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(display_preferences_id): Path<String>,
    Query(params): Query<DisplayPreferencesParams>,
    Json(mut dto): Json<DisplayPreferencesDto>,
) -> Result<StatusCode, ApiError> {
    let user_id = params.user_id.unwrap_or_else(|| auth.user_id());
    let client = params
        .client
        .filter(|c| !c.is_empty())
        .ok_or_else(|| ApiError::BadRequest("missing required 'client'".to_owned()))?;
    let item_id = resolve_item_id(&display_preferences_id);

    let mut prefs: DisplayPreferencesEntity = state
        .display_preferences
        .get_display_preferences(user_id, item_id, &client)
        .await?;

    prefs.index_by = index_by_from_name(dto.index_by.as_deref());
    prefs.show_backdrop = dto.show_backdrop;
    prefs.show_sidebar = dto.show_sidebar;
    prefs.scroll_direction = scroll_direction_to_i32(dto.scroll_direction);

    // Pull the scalar row fields out of CustomPrefs (removing each, as the C#
    // does, so they are not persisted as arbitrary custom prefs).
    prefs.chromecast_version = take_pref(&mut dto.custom_prefs, "chromecastVersion")
        .and_then(|v| chromecast_from_name(&v))
        .unwrap_or(0);
    prefs.enable_next_video_info_overlay =
        take_pref(&mut dto.custom_prefs, "enableNextVideoInfoOverlay")
            // Case-insensitive like C# `bool.Parse`: the client echoes back the
            // "True"/"False" we send, which Rust's `parse::<bool>` rejects.
            // Default true when absent/empty/garbage; only an explicit "false" is false.
            .is_none_or(|v| v.is_empty() || !v.eq_ignore_ascii_case("false"));
    prefs.skip_backward_length = take_pref(&mut dto.custom_prefs, "skipBackLength")
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(DEFAULT_SKIP_LENGTH_MS);
    prefs.skip_forward_length = take_pref(&mut dto.custom_prefs, "skipForwardLength")
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(DEFAULT_SKIP_LENGTH_MS);
    prefs.dashboard_theme =
        Some(take_pref(&mut dto.custom_prefs, "dashboardTheme").unwrap_or_default());
    prefs.tv_home = Some(take_pref(&mut dto.custom_prefs, "tvhome").unwrap_or_default());

    // Drop the home-section and landing keys: home-section persistence is
    // deferred with the flat display-preferences seam (see the module docs).
    dto.custom_prefs
        .retain(|k, _| !k.to_ascii_lowercase().starts_with("homesection"));

    let mut item_prefs: ItemDisplayPreferencesEntity = state
        .display_preferences
        .get_item_display_preferences(user_id, item_id, &client)
        .await?;
    item_prefs.sort_by = dto.sort_by.clone().unwrap_or_else(|| "SortName".to_owned());
    item_prefs.sort_order = sort_order_to_i32(dto.sort_order);
    item_prefs.remember_indexing = dto.remember_indexing;
    item_prefs.remember_sorting = dto.remember_sorting;

    state
        .display_preferences
        .set_custom_item_display_preferences(user_id, item_id, &client, &dto.custom_prefs)
        .await?;
    state
        .display_preferences
        .update_item_display_preferences(&item_prefs)
        .await?;
    state
        .display_preferences
        .update_display_preferences(&prefs)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes a custom-preference key, returning its value if present + non-null.
fn take_pref(prefs: &mut HashMap<String, Option<String>>, key: &str) -> Option<String> {
    prefs.remove(key).flatten()
}

/// The `ChromecastVersion` display name for a stored discriminant.
fn chromecast_name(value: i32) -> String {
    if value == 1 {
        "unstable".to_owned()
    } else {
        "stable".to_owned()
    }
}

/// Parses a `ChromecastVersion` name back to its stored discriminant.
fn chromecast_from_name(name: &str) -> Option<i32> {
    match name.to_ascii_lowercase().as_str() {
        "stable" => Some(0),
        "unstable" => Some(1),
        _ => None,
    }
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/DisplayPreferences/{displayPreferencesId}",
        get(get_display_preferences).post(update_display_preferences),
    )
}
