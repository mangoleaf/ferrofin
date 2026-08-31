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
//! The row's `HomeSection` children round-trip through the same `CustomPrefs`
//! map as `homesection{n}` keys: the GET emits one per stored section, and the
//! POST parses them back, substituting the C# per-order default when a value
//! does not name a `HomeSectionType`. They are loaded and rewritten through the
//! display-preferences seam's `list_home_sections`/`set_home_sections`, which
//! stand in for EF's `.Include(HomeSections)` eager-load.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_common::extensions::get_md5;
use ferrofin_db::entities::display_preferences::{
    DisplayPreferencesEntity, ItemDisplayPreferencesEntity,
};
use ferrofin_db::enums::{HomeSectionType, ViewType};
use ferrofin_model::dto::{DisplayPreferencesDto, ScrollDirection, SortOrder};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::JsonBody;
use crate::handlers::items::effective_user_id;
use crate::state::AppState;

/// The rewind skip length (ms) stored when the `skipBackLength` custom pref is
/// absent or empty (v10.11.8 `DisplayPreferencesController.cs`, `… : 10000`).
///
/// Upstream v12.0-rc3 unified both skip lengths to `15000`; Ferrofin pins the
/// 10.11.8 contract, so the two values stay distinct here.
const DEFAULT_SKIP_BACK_LENGTH_MS: i32 = 10_000;

/// The fast-forward skip length (ms) stored when the `skipForwardLength` custom
/// pref is absent or empty (v10.11.8 `DisplayPreferencesController.cs`,
/// `… : 30000`). See [`DEFAULT_SKIP_BACK_LENGTH_MS`].
const DEFAULT_SKIP_FORWARD_LENGTH_MS: i32 = 30_000;

/// Query parameters for the display-preferences routes.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisplayPreferencesParams {
    /// Optional user id; defaults to the authenticated caller.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
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
    tag = "ferrofin"
)]
async fn get_display_preferences(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(display_preferences_id): Path<String>,
    Query(params): Query<DisplayPreferencesParams>,
) -> Result<Json<DisplayPreferencesDto>, ApiError> {
    // C# `userId = RequestHelpers.GetUserId(User, userId)` — a named user other
    // than the caller requires the administrator role, else `403`.
    let user_id = effective_user_id(&state, &auth, params.user_id).await?;
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
    // The row's home sections, as `homesection{order}` = the lowercased
    // `HomeSectionType` name. Written before the scalar keys and before the
    // stored custom prefs, matching the C# order (the stored-prefs merge is a
    // `TryAdd`, so anything emitted here wins).
    for section in state
        .display_preferences
        .list_home_sections(prefs.id)
        .await?
    {
        custom_prefs.insert(
            format!("homesection{}", section.order),
            Some(home_section_name(section.type_)),
        );
    }
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
        // C# returns `displayPreferences.ItemId.ToString()` — the lowercase
        // hyphenated `Guid` form. The stored column is `guid_to_db`'s uppercase
        // form, so it has to be normalized on the way out.
        id: Some(prefs.item_id.to_ascii_lowercase()),
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
    tag = "ferrofin"
)]
async fn update_display_preferences(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(display_preferences_id): Path<String>,
    Query(params): Query<DisplayPreferencesParams>,
    JsonBody(mut dto): JsonBody<DisplayPreferencesDto>,
) -> Result<StatusCode, ApiError> {
    // C# `userId = RequestHelpers.GetUserId(User, userId)` — a named user other
    // than the caller requires the administrator role, else `403`.
    let user_id = effective_user_id(&state, &auth, params.user_id).await?;
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
        .unwrap_or(DEFAULT_SKIP_BACK_LENGTH_MS);
    prefs.skip_forward_length = take_pref(&mut dto.custom_prefs, "skipForwardLength")
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(DEFAULT_SKIP_FORWARD_LENGTH_MS);
    // C# `TryGetValue(key, out var v) ? v : string.Empty` over a
    // `Dictionary<string, string?>`: an absent key stores the empty string, but a
    // key present with an explicit JSON `null` stores null — and the GET path
    // hands that null straight back (see above), so the distinction is
    // client-visible.
    prefs.dashboard_theme = take_nullable_pref(&mut dto.custom_prefs, "dashboardTheme");
    prefs.tv_home = take_nullable_pref(&mut dto.custom_prefs, "tvhome");

    // Home sections: every `homesection{order}` key is consumed off CustomPrefs
    // and turned into a `HomeSection` row. A value that does not name a
    // `HomeSectionType` falls back to the per-order default, and an order at or
    // beyond that table falls back to `None` — C#
    // `type = order < 8 ? defaults[order] : HomeSectionType.None;`.
    let mut home_sections: Vec<(i32, i32)> = Vec::new();
    let home_section_keys: Vec<String> = dto
        .custom_prefs
        .keys()
        .filter(|k| {
            k.len() > "homesection".len()
                && k[.."homesection".len()].eq_ignore_ascii_case("homesection")
        })
        .cloned()
        .collect();
    for key in home_section_keys {
        let value = dto.custom_prefs.remove(&key).flatten();
        // C# `int.Parse` throws on a non-numeric suffix; Ferrofin drops the key
        // instead of 500-ing, which is the same observable state for a client
        // that never sends one.
        let Ok(order) = key["homesection".len()..].parse::<i32>() else {
            continue;
        };
        let type_ = value
            .as_deref()
            .and_then(home_section_from_name)
            .unwrap_or_else(|| {
                usize::try_from(order)
                    .ok()
                    .and_then(|o| HOME_SECTION_DEFAULTS.get(o).copied())
                    .unwrap_or(0)
            });
        home_sections.push((order, type_));
    }

    // `landing-*` keys naming something that is not a `ViewType` are dropped
    // rather than persisted (C# logs and removes them).
    dto.custom_prefs.retain(|key, value| {
        if key.len() <= "landing-".len()
            || !key[.."landing-".len()].eq_ignore_ascii_case("landing-")
        {
            return true;
        }
        let valid = value.as_deref().is_some_and(is_view_type_name);
        if !valid {
            tracing::error!(landing_screen_option = ?value, "Invalid ViewType");
        }
        valid
    });

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
    // Flushed last, mirroring EF saving the row's cleared-and-rebuilt
    // `HomeSections` collection as part of `UpdateDisplayPreferences`.
    state
        .display_preferences
        .set_home_sections(prefs.id, &home_sections)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes a custom-preference key, returning its value if present + non-null.
fn take_pref(prefs: &mut HashMap<String, Option<String>>, key: &str) -> Option<String> {
    prefs.remove(key).flatten()
}

/// Removes a custom-preference key whose stored column is nullable, preserving
/// the present-but-null case: absent ⇒ `Some("")`, present-and-null ⇒ `None`,
/// present ⇒ the value (C# `TryGetValue(…, out var v) ? v : string.Empty`).
fn take_nullable_pref(prefs: &mut HashMap<String, Option<String>>, key: &str) -> Option<String> {
    prefs.remove(key).unwrap_or_else(|| Some(String::new()))
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

/// The per-`Order` fallback home-section types, verbatim from the C#
/// `HomeSectionType[] defaults` array in
/// `DisplayPreferencesController.UpdateDisplayPreferences`. Indexed by the
/// section's order; an order at or beyond the array uses `None`.
const HOME_SECTION_DEFAULTS: [i32; 8] = [
    1, // SmallLibraryTiles
    4, // Resume
    5, // ResumeAudio
    9, // ResumeBook
    8, // LiveTv
    7, // NextUp
    6, // LatestMedia
    0, // None
];

/// The `HomeSectionType` name for a stored discriminant — C#
/// `homeSection.Type.ToString().ToLowerInvariant()`.
fn home_section_name(value: i32) -> String {
    HomeSectionType::try_from(value)
        .map_or_else(|_| "none".to_owned(), |t| format!("{t:?}").to_lowercase())
}

/// Parses a `HomeSectionType` name back to its stored discriminant, case
/// insensitively (C# `Enum.TryParse<HomeSectionType>(value, true, out _)`).
fn home_section_from_name(name: &str) -> Option<i32> {
    HOME_SECTION_TYPES
        .iter()
        .find(|t| format!("{t:?}").eq_ignore_ascii_case(name))
        .map(|t| i32::from(*t))
}

/// Every `HomeSectionType` variant, so a name lookup can enumerate them (the
/// `db_enum!` macro generates only the numeric conversions).
const HOME_SECTION_TYPES: [HomeSectionType; 10] = [
    HomeSectionType::None,
    HomeSectionType::SmallLibraryTiles,
    HomeSectionType::LibraryButtons,
    HomeSectionType::ActiveRecordings,
    HomeSectionType::Resume,
    HomeSectionType::ResumeAudio,
    HomeSectionType::LatestMedia,
    HomeSectionType::NextUp,
    HomeSectionType::LiveTv,
    HomeSectionType::ResumeBook,
];

/// Whether `name` parses as a `ViewType`, case insensitively (C#
/// `Enum.TryParse<ViewType>(value, true, out _)`).
fn is_view_type_name(name: &str) -> bool {
    VIEW_TYPES
        .iter()
        .any(|v| format!("{v:?}").eq_ignore_ascii_case(name))
}

/// Every `ViewType` variant, for the `landing-*` validity check.
const VIEW_TYPES: [ViewType; 26] = [
    ViewType::Albums,
    ViewType::AlbumArtists,
    ViewType::Artists,
    ViewType::Channels,
    ViewType::Collections,
    ViewType::Episodes,
    ViewType::Favorites,
    ViewType::Genres,
    ViewType::Guide,
    ViewType::Movies,
    ViewType::Networks,
    ViewType::Playlists,
    ViewType::Programs,
    ViewType::Recordings,
    ViewType::Schedule,
    ViewType::Series,
    ViewType::Shows,
    ViewType::Songs,
    ViewType::Suggestions,
    ViewType::Trailers,
    ViewType::Upcoming,
    ViewType::Authors,
    ViewType::Books,
    ViewType::Folders,
    ViewType::Mixed,
    ViewType::Photos,
];

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/DisplayPreferences/{displayPreferencesId}",
        get(get_display_preferences).post(update_display_preferences),
    )
}
