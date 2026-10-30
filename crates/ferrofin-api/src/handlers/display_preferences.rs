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
//! does not parse as a `HomeSectionType` (`Enum.TryParse`: a name list or an
//! integer). They are loaded and rewritten through the
//! display-preferences seam's `list_home_sections`/`set_home_sections`, which
//! stand in for EF's `.Include(HomeSections)` eager-load.

use std::collections::HashMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_common::extensions::get_md5;
use ferrofin_db::entities::display_preferences::{
    DisplayPreferencesEntity, ItemDisplayPreferencesEntity,
};
use ferrofin_db::enums::{ChromecastVersion, HomeSectionType, IndexingKind, ViewType};
use ferrofin_model::dto::{DisplayPreferencesDto, ScrollDirection, SortOrder};
use ferrofin_util::guid_extensions::parse_dotnet_guid;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::{JsonBody, Query, RequiredRouteString, RouteParameter};
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

/// `{displayPreferencesId}`, the `[FromRoute, Required] string` of both
/// actions: a blank one is the binder's `400`, before the action.
struct DisplayPreferencesId;

impl RouteParameter for DisplayPreferencesId {
    const NAME: &'static str = "displayPreferencesId";
}

/// Resolves a `displayPreferencesId` into an item id: parse it as a `Guid`, else
/// hash it with MD5 (C# `Guid.TryParse … else GetMD5()`). `Guid.TryParse`'s
/// spellings, not `Uuid::parse_str`'s: an `urn:uuid:` id is hashed, and a
/// `(…)`, `{0x…}` or padded one is parsed.
fn resolve_item_id(raw: &str) -> Uuid {
    parse_dotnet_guid(raw).unwrap_or_else(|| get_md5(raw))
}

/// Reads a C# enum without discarding unnamed integer values.
fn scroll_direction_from_i32(value: i32) -> ScrollDirection {
    ScrollDirection::from_json_value(value)
}

/// The exact C# enum value to persist.
fn scroll_direction_to_i32(dir: ScrollDirection) -> i32 {
    dir.json_value()
}

/// Reads a C# enum without discarding unnamed integer values.
fn sort_order_from_i32(value: i32) -> SortOrder {
    SortOrder::from_json_value(value)
}

/// The exact C# enum value to persist.
fn sort_order_to_i32(order: SortOrder) -> i32 {
    order.json_value()
}

/// The `IndexingKind` display name for a stored discriminant (C#
/// `IndexBy?.ToString()`), which writes an unnamed value as its number.
fn index_by_name(value: Option<i32>) -> Option<String> {
    value.map(|v| IndexingKind::try_from(v).map_or_else(|_| v.to_string(), |k| format!("{k:?}")))
}

/// Parses an `IndexingKind` back to its stored discriminant, as C#
/// `Enum.TryParse<IndexingKind>(value, true, …) ? indexBy : null` does.
fn index_by_from_name(name: Option<&str>) -> Option<i32> {
    name.and_then(|n| enum_try_parse(n, IndexingKind::ALL))
}

/// C# `int.Parse(value, CultureInfo.InvariantCulture)` (`NumberStyles.Integer`):
/// a sign and ASCII digits, with the white space it allows around them
/// (U+0009–U+000D, U+0020) and the trailing NULs .NET ignores. `None` where
/// upstream throws (a 500 not ported).
fn parse_dotnet_int(value: &str) -> Option<i32> {
    value
        .trim_end_matches('\0')
        .trim_matches(|c| matches!(c, '\t'..='\r' | ' '))
        .parse()
        .ok()
}

/// C# `bool.Parse`: `True` or `False` in any case, ignoring white space and
/// NULs around it. `None` where upstream throws (a 500 not ported).
fn parse_dotnet_bool(value: &str) -> Option<bool> {
    let value = value.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if value.eq_ignore_ascii_case("true") {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
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
    RequiredRouteString(display_preferences_id, _): RequiredRouteString<DisplayPreferencesId>,
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
    RequiredRouteString(display_preferences_id, _): RequiredRouteString<DisplayPreferencesId>,
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
    // C# `Enum.Parse<ChromecastVersion>(value, true)` for a non-empty value,
    // else `Stable`. `Enum.Parse` keeps an unnamed integer, and throws
    // `ArgumentException` (a 400) for anything it cannot read.
    prefs.chromecast_version = match take_pref(&mut dto.custom_prefs, "chromecastVersion") {
        Some(v) if !v.is_empty() => enum_try_parse(&v, ChromecastVersion::ALL)
            .ok_or_else(|| ApiError::BadRequest(format!("Requested value '{v}' was not found.")))?,
        _ => 0,
    };
    prefs.enable_next_video_info_overlay =
        take_pref(&mut dto.custom_prefs, "enableNextVideoInfoOverlay")
            // True when absent or empty; only a value `bool.Parse` reads as
            // false is false (one it cannot read, a 500 upstream, stays true).
            .is_none_or(|v| v.is_empty() || parse_dotnet_bool(&v) != Some(false));
    prefs.skip_backward_length = take_pref(&mut dto.custom_prefs, "skipBackLength")
        .and_then(|v| parse_dotnet_int(&v))
        .unwrap_or(DEFAULT_SKIP_BACK_LENGTH_MS);
    prefs.skip_forward_length = take_pref(&mut dto.custom_prefs, "skipForwardLength")
        .and_then(|v| parse_dotnet_int(&v))
        .unwrap_or(DEFAULT_SKIP_FORWARD_LENGTH_MS);
    // C# `TryGetValue(key, out var v) ? v : string.Empty` over a
    // `Dictionary<string, string?>`: an absent key stores the empty string, but a
    // key present with an explicit JSON `null` stores null — and the GET path
    // hands that null straight back (see above), so the distinction is
    // client-visible.
    prefs.dashboard_theme = take_nullable_pref(&mut dto.custom_prefs, "dashboardTheme");
    prefs.tv_home = take_nullable_pref(&mut dto.custom_prefs, "tvhome");

    // Home sections: every `homesection{order}` key is consumed off CustomPrefs
    // and turned into a `HomeSection` row. A value that does not parse as a
    // `HomeSectionType` (`enum_try_parse`) falls back to the per-order default,
    // and an order at or beyond that table falls back to `None` — C#
    // `type = order < 8 ? defaults[order] : HomeSectionType.None;`.
    let mut home_sections: Vec<(i32, i32)> = Vec::new();
    let home_section_keys: Vec<String> = dto
        .custom_prefs
        .keys()
        // A bare `homesection` is left alone: C# matches it too and then
        // throws on `int.Parse("")`, a 500 not worth porting.
        .filter(|k| k.len() > "homesection".len() && has_prefix_ignore_case(k, "homesection"))
        .cloned()
        .collect();
    for key in home_section_keys {
        let value = dto.custom_prefs.remove(&key).flatten();
        // C# `int.Parse` throws on a non-numeric suffix; Ferrofin drops the key
        // instead of 500-ing, which is the same observable state for a client
        // that never sends one.
        let Some(order) = parse_dotnet_int(&key["homesection".len()..]) else {
            continue;
        };
        let type_ = value
            .as_deref()
            .and_then(|value| enum_try_parse(value, HomeSectionType::ALL))
            .unwrap_or_else(|| {
                usize::try_from(order)
                    .ok()
                    .and_then(|o| HOME_SECTION_DEFAULTS.get(o).copied())
                    .unwrap_or(0)
            });
        home_sections.push((order, type_));
    }

    // `landing-*` keys: an empty value (jellyfin-web's "default screen") is
    // dropped silently, one naming something that is not a `ViewType` is
    // logged and dropped (C#).
    dto.custom_prefs.retain(|key, value| {
        if !has_prefix_ignore_case(key, "landing-") {
            return true;
        }
        let Some(value) = value.as_deref().filter(|v| !v.is_empty()) else {
            return false;
        };
        let valid = is_view_type_name(value);
        if !valid {
            // C# logs this at error; it is bad client input, a warning by
            // docs/conventions/LOGGING.md.
            tracing::warn!(landing_screen_option = value, "Invalid ViewType");
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

/// The `ChromecastVersion` display name for a stored discriminant (C#
/// `ChromecastVersion.ToString().ToLowerInvariant()`), which writes an unnamed
/// value as its number.
fn chromecast_name(value: i32) -> String {
    ChromecastVersion::try_from(value)
        .map_or_else(|_| value.to_string(), |v| format!("{v:?}").to_lowercase())
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
/// `homeSection.Type.ToString().ToLowerInvariant()`, which writes an unnamed
/// value (one `Enum.TryParse` stored from an integer) as the number itself.
fn home_section_name(value: i32) -> String {
    HomeSectionType::try_from(value)
        .map_or_else(|_| value.to_string(), |t| format!("{t:?}").to_lowercase())
}

/// C# `Enum.TryParse<TEnum>(value, ignoreCase: true, out var result)` for an
/// `int`-backed enum with the members `all`: a value starting with a digit or a
/// sign is an integer of the underlying type, named or not; anything else is a
/// comma-separated list of member names, case insensitive, whose values are
/// OR'd. Surrounding whitespace is ignored; anything else is `None`.
fn enum_try_parse<T>(value: &str, all: &[T]) -> Option<i32>
where
    T: Copy + std::fmt::Debug,
    i32: From<T>,
{
    let value = value.trim();
    if value.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+') {
        return value.parse::<i32>().ok();
    }
    if value.is_empty() {
        return None;
    }
    value.split(',').try_fold(0, |acc, name| {
        all.iter()
            .find(|member| format!("{member:?}").eq_ignore_ascii_case(name.trim()))
            .map(|member| acc | i32::from(*member))
    })
}

/// Whether `value` parses as a `ViewType` ([`enum_try_parse`]).
fn is_view_type_name(value: &str) -> bool {
    enum_try_parse(value, ViewType::ALL).is_some()
}

/// Whether `key` starts with the ASCII `prefix`, ignoring case (C#
/// `StartsWith(prefix, StringComparison.OrdinalIgnoreCase)`). Never slices
/// inside a multi-byte character.
fn has_prefix_ignore_case(key: &str, prefix: &str) -> bool {
    key.get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/DisplayPreferences/{displayPreferencesId}",
        get(get_display_preferences).post(update_display_preferences),
    )
}
crate::query::query_parameters! {
    DisplayPreferencesParams {} => [("get", "/DisplayPreferences/{displayPreferencesId}"), ("post", "/DisplayPreferences/{displayPreferencesId}")];
}
