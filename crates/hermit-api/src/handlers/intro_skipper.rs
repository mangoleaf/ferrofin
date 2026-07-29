//! Intro Skipper extension controllers — the plugin API surface a Jellyfin
//! client (and the plugin's own dashboard pages) call to read/write skip
//! timestamps, drive re-scans and inject the skip-button CSS.
//!
//! Ports the routes of the upstream Intro Skipper plugin's five controllers
//! (`SkipIntro`, `SegmentEditor`, `SkipButtonCss`, `Troubleshooting`,
//! `Visualization`) plus the `FileTransformation` registration hook it depends
//! on. In Jellyfin these live in a dynamically-loaded plugin; Hermit compiles
//! the extension in, so the routes are served here over the same managers the
//! rest of the API uses.
//!
//! **Data-model mapping.** The upstream plugin keeps a private SQLite database
//! of per-mode timestamps plus an on-disk fingerprint cache. Hermit has no
//! separate plugin store: the detected boundaries live directly in the core
//! `MediaSegments` table under the provider id `IntroSkipper`. So a
//! "timestamp" read/write here is a `MediaSegments` read/write, and the plugin
//! `AnalysisMode` maps onto [`MediaSegmentType`]:
//!
//! | `AnalysisMode` | [`MediaSegmentType`] |
//! |----------------|----------------------|
//! | Introduction   | `Intro`              |
//! | Credits        | `Outro`              |
//! | Preview        | `Preview`            |
//! | Recap          | `Recap`              |
//! | Commercial     | `Commercial`         |
//!
//! Three routes are thinner than upstream because the backing subsystem does
//! not exist in Hermit — each is documented at its handler:
//! `Intros/RebuildDatabase` (no separate DB to rebuild),
//! `Intros/AnalyzerActions/UpdateSeason` (no per-season analyzer-action store;
//! detection runs off the global config) and
//! `FileTransformation/RegisterTransformation` (no web-asset pipeline to hook).

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_model::branding::BrandingOptions;
use hermit_model::data::BaseItemKind;
use hermit_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use hermit_model::tasks::TaskState;
use hermit_traits::options::InternalItemsQuery;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// The compiled-in Intro Skipper extension id (mirrors `EXTENSION_ID` in
/// `hermit-extensions`).
const INTRO_SKIPPER_ID: Uuid = Uuid::from_u128(0x1a7b_05c1_5c1b_4d0e_9f00_1247_a105_c1de);
/// The media-segment provider id the extension writes its rows under.
const PROVIDER_ID: &str = "IntroSkipper";
/// The scheduled-task key that runs a detection pass.
const DETECT_TASK_KEY: &str = "IntroSkipper.Detect";
/// One second expressed in the 100-nanosecond ticks used by segment storage.
const TICKS_PER_SECOND: f64 = 10_000_000.0;

/// The skip-button CSS `@import` the plugin injects into server branding.
const IMPORT_STRING: &str = r#"@import url("https://cdn.jsdelivr.net/gh/intro-skipper/intro-skipper-css@main/skip-button.min.css");"#;

// ---------------------------------------------------------------------------
// Mode ↔ segment-type ↔ name helpers
// ---------------------------------------------------------------------------

/// Parses an `AnalysisMode` (by name or numeric discriminant) into the stored
/// [`MediaSegmentType`]. Accepts the plugin's aliases (`intro`, `outro`).
fn mode_to_segment_type(mode: &str) -> Option<MediaSegmentType> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "introduction" | "intro" | "0" => Some(MediaSegmentType::Intro),
        "credits" | "outro" | "1" => Some(MediaSegmentType::Outro),
        "preview" | "2" => Some(MediaSegmentType::Preview),
        "recap" | "3" => Some(MediaSegmentType::Recap),
        "commercial" | "4" => Some(MediaSegmentType::Commercial),
        _ => None,
    }
}

/// The `AnalysisMode` name for a stored segment type (`None` for `Unknown`,
/// which the plugin has no mode for).
fn segment_type_mode_name(type_: MediaSegmentType) -> Option<&'static str> {
    match type_ {
        MediaSegmentType::Intro => Some("Introduction"),
        MediaSegmentType::Outro => Some("Credits"),
        MediaSegmentType::Preview => Some("Preview"),
        MediaSegmentType::Recap => Some("Recap"),
        MediaSegmentType::Commercial => Some("Commercial"),
        MediaSegmentType::Unknown => None,
    }
}

/// Whether a stored item `type_` names an Episode or Movie — the item kinds the
/// plugin's timestamp routes accept. The persisted value is the full CLR type
/// name (e.g. `MediaBrowser.Controller.Entities.TV.Episode`), so match its last
/// dotted segment.
fn is_episode_or_movie(type_name: &str) -> bool {
    matches!(
        type_name.rsplit('.').next().unwrap_or(type_name),
        "Episode" | "Movie"
    )
}

#[allow(clippy::cast_precision_loss)]
fn ticks_to_secs(ticks: i64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND
}

#[allow(clippy::cast_possible_truncation)]
fn secs_to_ticks(secs: f64) -> i64 {
    (secs * TICKS_PER_SECOND) as i64
}

// ---------------------------------------------------------------------------
// Wire DTOs (local to these handlers — the plugin's contract types)
// ---------------------------------------------------------------------------

/// A single skippable region, in seconds. Mirrors the plugin `Segment`; `Valid`
/// is the computed `End > 0` flag it exposes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Segment {
    #[serde(default)]
    episode_id: Uuid,
    start: f64,
    end: f64,
    #[serde(default)]
    valid: bool,
}

impl Segment {
    /// Builds an output segment, computing `Valid` the way the plugin does.
    fn output(episode_id: Uuid, start: f64, end: f64) -> Self {
        Self {
            episode_id,
            start,
            end,
            valid: end > 0.0,
        }
    }
}

/// The per-mode timestamp bundle for an episode (`TimeStamps` upstream).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TimeStamps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    introduction: Option<Segment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credits: Option<Segment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recap: Option<Segment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preview: Option<Segment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commercial: Option<Segment>,
}

/// Whether a detection scan is currently running (`ScanStatusResponse`). Note
/// the plugin serialises this one type as camelCase, unlike its others.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanStatusResponse {
    is_running: bool,
}

/// An episode's id and name for the visualization list (`EpisodeVisualization`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct EpisodeVisualization {
    id: Uuid,
    name: String,
}

/// Query for `POST /MediaSegmentsApi/{itemId}` — the owning provider id.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSegmentQuery {
    #[serde(default)]
    provider_id: Option<String>,
}

/// Request body for `POST /MediaSegmentsApi/{itemId}` — a segment to create. The
/// item id comes from the path, and `Id` is server-assigned, so both default;
/// only `Type`/`StartTicks`/`EndTicks` are meaningful.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SegmentInput {
    #[serde(rename = "Type")]
    type_: MediaSegmentType,
    start_ticks: i64,
    end_ticks: i64,
}

/// Query for `POST /Intros/EraseTimestamps` — the mode to erase.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EraseQuery {
    mode: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Reads the extension's `SkipbuttonHideDelay` config value (defaults to 8s).
async fn skip_hide_delay(state: &AppState) -> u64 {
    let bytes = state
        .plugins
        .get_plugin_configuration(INTRO_SKIPPER_ID)
        .await
        .unwrap_or_default();
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| {
            v.get("SkipbuttonHideDelay")
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or(8)
}

/// The extension's version string (falls back to `0.0.0.0` if unknown).
async fn plugin_version(state: &AppState) -> String {
    state
        .plugins
        .get_plugin(INTRO_SKIPPER_ID)
        .await
        .ok()
        .flatten()
        .map_or_else(|| "0.0.0.0".to_owned(), |d| d.version)
}

/// Whether a detection pass is currently running.
async fn scan_running(state: &AppState) -> Result<bool, ApiError> {
    Ok(state
        .tasks
        .get_task(DETECT_TASK_KEY)
        .await?
        .is_some_and(|t| t.state == TaskState::Running))
}

/// The episodes directly under a season (empty if the season has none).
async fn season_episodes(
    state: &AppState,
    season_id: Uuid,
) -> Result<Vec<BaseItemEntity>, ApiError> {
    let query = InternalItemsQuery {
        parent_id: season_id,
        include_item_types: vec![BaseItemKind::Episode],
        ..InternalItemsQuery::default()
    };
    Ok(state.library.get_item_list(&query).await?)
}

/// Reads an item's stored segments and folds them into a [`TimeStamps`].
async fn timestamps_for(state: &AppState, item_id: Uuid) -> Result<TimeStamps, ApiError> {
    let segments = state
        .media_segments
        .get_segments(item_id, None, false)
        .await?;
    let mut ts = TimeStamps::default();
    for s in segments {
        let seg = Segment::output(
            item_id,
            ticks_to_secs(s.start_ticks),
            ticks_to_secs(s.end_ticks),
        );
        match s.type_ {
            MediaSegmentType::Intro => ts.introduction = Some(seg),
            MediaSegmentType::Outro => ts.credits = Some(seg),
            MediaSegmentType::Recap => ts.recap = Some(seg),
            MediaSegmentType::Preview => ts.preview = Some(seg),
            MediaSegmentType::Commercial => ts.commercial = Some(seg),
            MediaSegmentType::Unknown => {}
        }
    }
    Ok(ts)
}

// ---------------------------------------------------------------------------
// SkipIntro controller
// ---------------------------------------------------------------------------

/// `POST /Episode/{Id}/Timestamps` — replace an episode/movie's user timestamps.
///
/// Port of `SkipIntroController.UpdateTimestampsAsync`: each present, valid
/// (`End > 0`) mode replaces that provider/type's stored segment. 404 when the
/// item is not an Episode or Movie.
async fn update_timestamps(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(id): Path<Uuid>,
    Json(timestamps): Json<TimeStamps>,
) -> Result<StatusCode, ApiError> {
    let Some(item) = state.library.get_item_by_id(id).await? else {
        return Err(ApiError::NotFound(format!("item {id}")));
    };
    if !is_episode_or_movie(&item.type_) {
        return Err(ApiError::NotFound(format!(
            "item {id} is not an episode/movie"
        )));
    }

    let modes = [
        (timestamps.introduction, MediaSegmentType::Intro),
        (timestamps.credits, MediaSegmentType::Outro),
        (timestamps.recap, MediaSegmentType::Recap),
        (timestamps.preview, MediaSegmentType::Preview),
        (timestamps.commercial, MediaSegmentType::Commercial),
    ];
    for (segment, type_) in modes {
        let Some(seg) = segment else { continue };
        if seg.end <= 0.0 {
            continue;
        }
        // Replace only this provider+type's rows, leaving other providers intact.
        state
            .media_segments
            .delete_provider_segments(id, PROVIDER_ID, Some(type_))
            .await?;
        let dto = MediaSegmentDto {
            id: Uuid::nil(),
            item_id: id,
            type_,
            start_ticks: secs_to_ticks(seg.start),
            end_ticks: secs_to_ticks(seg.end),
        };
        state
            .media_segments
            .create_segment(&dto, PROVIDER_ID)
            .await?;
    }

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Episode/{Id}/Timestamps` — an episode/movie's stored timestamps.
async fn get_timestamps(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<TimeStamps>, ApiError> {
    let Some(item) = state.library.get_item_by_id(id).await? else {
        return Err(ApiError::NotFound(format!("item {id}")));
    };
    if !is_episode_or_movie(&item.type_) {
        return Err(ApiError::NotFound(format!(
            "item {id} is not an episode/movie"
        )));
    }
    Ok(Json(timestamps_for(&state, id).await?))
}

/// `GET /Episode/{id}/IntroSkipperSegments` — a mode→segment dictionary of all
/// skippable regions (the shape the web skip-button script polls).
async fn get_skippable_segments(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<HashMap<String, Segment>>, ApiError> {
    let segments = state.media_segments.get_segments(id, None, false).await?;
    let mut out = HashMap::new();
    for s in segments {
        if let Some(name) = segment_type_mode_name(s.type_) {
            out.insert(
                name.to_owned(),
                Segment::output(id, ticks_to_secs(s.start_ticks), ticks_to_secs(s.end_ticks)),
            );
        }
    }
    Ok(Json(out))
}

/// `POST /Intros/EraseTimestamps` — erase every stored segment of one mode.
///
/// Port of `SkipIntroController.ResetIntroTimestamps`. `eraseCache` is accepted
/// for contract compatibility but Hermit keeps no fingerprint cache to clear.
async fn erase_timestamps(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<EraseQuery>,
) -> Result<StatusCode, ApiError> {
    let mode = query
        .mode
        .as_deref()
        .and_then(mode_to_segment_type)
        .ok_or_else(|| ApiError::BadRequest("missing or invalid 'mode'".to_owned()))?;
    state
        .media_segments
        .delete_all_provider_segments(PROVIDER_ID, Some(mode))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Intros/RebuildDatabase` — a no-op success in Hermit.
///
/// The upstream plugin rebuilds its private SQLite index here. Hermit stores
/// segments directly in the core `MediaSegments` table (there is no separate
/// index to rebuild), so this reports success without work.
// ponytail: no separate plugin DB in Hermit; nothing to rebuild.
async fn rebuild_database(
    State(_state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> StatusCode {
    StatusCode::NO_CONTENT
}

// ---------------------------------------------------------------------------
// SegmentEditor controller
// ---------------------------------------------------------------------------

/// `GET /MediaSegmentsApi` — plugin metadata (version).
async fn segment_editor_metadata(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "version": plugin_version(&state).await }))
}

/// `POST /MediaSegmentsApi/{itemId}` — create/replace a segment for an item.
async fn create_segment(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<CreateSegmentQuery>,
    Json(segment): Json<SegmentInput>,
) -> Result<StatusCode, ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }
    let provider = query.provider_id.as_deref().unwrap_or(PROVIDER_ID);
    let dto = MediaSegmentDto {
        id: Uuid::nil(),
        item_id,
        type_: segment.type_,
        start_ticks: segment.start_ticks,
        end_ticks: segment.end_ticks,
    };
    state.media_segments.create_segment(&dto, provider).await?;
    Ok(StatusCode::OK)
}

/// `DELETE /MediaSegmentsApi/{segmentId}` — delete one segment by id.
///
/// (The canonical route param is `itemId`; the value here is the segment id.
/// The `itemId`/`type` query params are accepted for contract compatibility.)
async fn delete_segment(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(segment_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    state.media_segments.delete_segment(segment_id).await?;
    Ok(StatusCode::OK)
}

// ---------------------------------------------------------------------------
// SkipButtonCss controller
// ---------------------------------------------------------------------------

/// The `:root` block the plugin appends to carry the hide-duration variable.
fn root_css(delay: u64) -> String {
    format!(":root {{\n    /* Skip button timing */\n    --skip-hide-duration: {delay}s;\n}}")
}

/// Locates the byte span of an existing `--skip-hide-duration: <n>s;` value.
/// Mirrors the plugin's `--skip-hide-duration:\s*[\d.]+s;` regex.
fn find_skip_duration_span(css: &str) -> Option<(usize, usize)> {
    const KEY: &str = "--skip-hide-duration:";
    let key_at = css.find(KEY)?;
    let after_key = key_at + KEY.len();
    let bytes = css.as_bytes();
    let mut i = after_key;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let num_start = i;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    // Require at least one digit, then a literal `s;`.
    if i == num_start || !css[i..].starts_with("s;") {
        return None;
    }
    Some((key_at, i + 2))
}

/// Updates or appends the `--skip-hide-duration` value, returning the new CSS
/// and whether anything changed. Port of `UpdateDurationValue`.
fn update_duration_value(css: &str, delay: u64) -> (String, bool) {
    let expected = format!("--skip-hide-duration: {delay}s;");
    if let Some((start, end)) = find_skip_duration_span(css) {
        if css[start..end] == expected {
            return (css.to_owned(), false);
        }
        let mut updated = String::with_capacity(css.len());
        updated.push_str(&css[..start]);
        updated.push_str(&expected);
        updated.push_str(&css[end..]);
        return (updated, true);
    }
    (format!("{css}\n{}", root_css(delay)), true)
}

/// Inserts the `@import` after the last existing `@import`, else prepends it.
/// Port of `InjectImport`.
fn inject_import(css: &str) -> String {
    if let Some(last_import) = css.to_lowercase().rfind("@import") {
        if let Some(rel_semi) = css[last_import..].find(';') {
            let mut insert_at = last_import + rel_semi + 1;
            let bytes = css.as_bytes();
            if insert_at < bytes.len() && bytes[insert_at] == b'\n' {
                insert_at += 1;
            } else if insert_at + 1 < bytes.len()
                && bytes[insert_at] == b'\r'
                && bytes[insert_at + 1] == b'\n'
            {
                insert_at += 2;
            }
            let mut out = String::with_capacity(css.len() + IMPORT_STRING.len() + 1);
            out.push_str(&css[..insert_at]);
            out.push_str(IMPORT_STRING);
            out.push('\n');
            out.push_str(&css[insert_at..]);
            return out;
        }
        return format!("{css}\n{IMPORT_STRING}");
    }
    format!("{IMPORT_STRING}\n{css}")
}

/// `POST /SkipButtonCss/InjectCss` — inject the skip-button import and duration
/// variable into server branding CSS. Port of `SkipButtonCssController.InjectCss`.
async fn inject_css(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<StatusCode, ApiError> {
    let delay = skip_hide_delay(&state).await;
    let mut branding = state.config.get_branding().await?;
    let mut css = branding.custom_css.clone().unwrap_or_default();
    let mut modified = false;

    if !css.contains(IMPORT_STRING) {
        css = inject_import(&css);
        modified = true;
    }
    let (updated, duration_modified) = update_duration_value(&css, delay);
    if duration_modified {
        css = updated;
        modified = true;
    }
    if modified {
        branding.custom_css = Some(css);
        save_branding(&state, branding).await?;
    }
    Ok(StatusCode::OK)
}

/// `POST /SkipButtonCss/UpdateSkipDuration` — refresh the duration variable if
/// it is already present (no-op otherwise). Port of `UpdateSkipDuration`.
async fn update_skip_duration(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<StatusCode, ApiError> {
    let delay = skip_hide_delay(&state).await;
    let mut branding = state.config.get_branding().await?;
    let css = branding.custom_css.clone().unwrap_or_default();
    if find_skip_duration_span(&css).is_none() {
        return Ok(StatusCode::OK);
    }
    let (updated, modified) = update_duration_value(&css, delay);
    if modified {
        branding.custom_css = Some(updated);
        save_branding(&state, branding).await?;
    }
    Ok(StatusCode::OK)
}

/// Persists branding, preserving whatever the config manager already holds for
/// the fields the CSS routes don't touch.
async fn save_branding(state: &AppState, branding: BrandingOptions) -> Result<(), ApiError> {
    state.config.update_branding(&branding).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Troubleshooting controller
// ---------------------------------------------------------------------------

/// `GET /IntroSkipper` — plugin metadata (version).
async fn troubleshooting_metadata(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "version": plugin_version(&state).await }))
}

/// Whether an `fpcalc` binary (Chromaprint fingerprinter) is on `PATH`.
fn fpcalc_available() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("fpcalc").is_file()))
}

/// `GET /IntroSkipper/SupportBundle` — a plain-text Markdown troubleshooting
/// bundle. Port of `TroubleshootingController.GetSupportBundle`, reporting the
/// facts Hermit can supply (server/plugin version, OS, fingerprinter presence).
async fn support_bundle(State(state): State<AppState>, RequireAuth(_auth): RequireAuth) -> String {
    let version = plugin_version(&state).await;
    format!(
        "* Server: Hermit {server}\n\
         * Plugin version: {version}\n\
         * Runs on: {os} ({arch})\n\
         * Chromaprint (fpcalc) available: {fpcalc}\n",
        server = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        fpcalc = fpcalc_available(),
    )
}

// ---------------------------------------------------------------------------
// Visualization controller
// ---------------------------------------------------------------------------

/// `GET /Intros/ScanStatus` — whether a detection pass is running.
async fn scan_status(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<ScanStatusResponse>, ApiError> {
    Ok(Json(ScanStatusResponse {
        is_running: scan_running(&state).await?,
    }))
}

/// `GET /Intros/Show/{SeriesId}/{SeasonId}` — the episodes of a season.
async fn get_season_episodes(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((_series_id, season_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<EpisodeVisualization>>, ApiError> {
    let episodes = season_episodes(&state, season_id).await?;
    if episodes.is_empty() {
        return Err(ApiError::NotFound(format!(
            "season {season_id} has no episodes"
        )));
    }
    let out = episodes
        .into_iter()
        .map(|e| EpisodeVisualization {
            id: Uuid::parse_str(&e.id).unwrap_or_default(),
            name: e.name.unwrap_or_default(),
        })
        .collect();
    Ok(Json(out))
}

/// `DELETE /Intros/Show/{SeriesId}/{SeasonId}` — erase the season's Intro
/// Skipper segments. Port of `VisualizationController.EraseSeasonAsync`.
async fn erase_season(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((_series_id, season_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let episodes = season_episodes(&state, season_id).await?;
    if episodes.is_empty() {
        return Err(ApiError::NotFound(format!(
            "season {season_id} has no episodes"
        )));
    }
    for episode in episodes {
        if let Ok(eid) = Uuid::parse_str(&episode.id) {
            state
                .media_segments
                .delete_provider_segments(eid, PROVIDER_ID, None)
                .await?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Intros/AnalyzerActions/{SeasonId}` — the per-mode analyzer actions for
/// a season.
///
/// Hermit runs detection off the global extension config and keeps no
/// per-season analyzer-action overrides, so every mode reports `Default`
/// (meaning "use the configured analyzer"). 404 when the season id is unknown.
async fn get_analyzer_actions(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(season_id): Path<Uuid>,
) -> Result<Json<HashMap<String, String>>, ApiError> {
    if state.library.get_item_by_id(season_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("season {season_id}")));
    }
    let actions = ["Introduction", "Credits", "Recap", "Preview", "Commercial"]
        .into_iter()
        .map(|m| (m.to_owned(), "Default".to_owned()))
        .collect();
    Ok(Json(actions))
}

/// `POST /Intros/AnalyzerActions/UpdateSeason` — accept per-season analyzer
/// actions.
///
/// Accepted for contract compatibility; Hermit's detection uses the global
/// config and does not persist per-season analyzer overrides, so this reports
/// success without storing anything.
// ponytail: no per-season analyzer-action store; detection is global-config driven.
async fn update_analyzer_actions(
    State(_state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Json(_request): Json<serde_json::Value>,
) -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `POST /Intros/ScanSeason/{SeriesId}/{SeasonId}` — start a detection pass.
///
/// Port of `VisualizationController.ScanSeason`: 409 if a scan is already
/// running, else start the detection task and return 202. Hermit's detection
/// task scans the whole library rather than a single season.
// ponytail: detection task is library-wide; per-season scoping needs task params.
async fn scan_season(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((_series_id, _season_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    if scan_running(&state).await? {
        return Err(ApiError::Conflict(
            "a scan is already in progress".to_owned(),
        ));
    }
    state.tasks.start_task(DETECT_TASK_KEY).await?;
    Ok(StatusCode::ACCEPTED)
}

// ---------------------------------------------------------------------------
// FileTransformation hook
// ---------------------------------------------------------------------------

/// `POST /FileTransformation/RegisterTransformation` — accept a transformation
/// registration.
///
/// In Jellyfin this belongs to the separate File Transformation plugin that
/// rewrites served web-client assets so the skip button can be injected. Hermit
/// serves no web client, so there is nothing to transform; the registration is
/// accepted (200) and dropped.
// ponytail: no web-asset pipeline in Hermit; accept-and-drop.
async fn register_transformation(
    RequireAuth(_auth): RequireAuth,
    Json(_payload): Json<serde_json::Value>,
) -> StatusCode {
    StatusCode::OK
}

/// Registers the Intro Skipper extension's routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Episode/{Id}/Timestamps",
            get(get_timestamps).post(update_timestamps),
        )
        .route(
            "/Episode/{Id}/IntroSkipperSegments",
            get(get_skippable_segments),
        )
        .route("/Intros/EraseTimestamps", post(erase_timestamps))
        .route("/Intros/RebuildDatabase", post(rebuild_database))
        .route("/MediaSegmentsApi", get(segment_editor_metadata))
        .route(
            "/MediaSegmentsApi/{itemId}",
            post(create_segment).delete(delete_segment),
        )
        .route("/SkipButtonCss/InjectCss", post(inject_css))
        .route(
            "/SkipButtonCss/UpdateSkipDuration",
            post(update_skip_duration),
        )
        .route("/IntroSkipper", get(troubleshooting_metadata))
        .route("/IntroSkipper/SupportBundle", get(support_bundle))
        .route(
            "/Intros/AnalyzerActions/{SeasonId}",
            get(get_analyzer_actions),
        )
        .route(
            "/Intros/AnalyzerActions/UpdateSeason",
            post(update_analyzer_actions),
        )
        .route(
            "/Intros/Show/{SeriesId}/{SeasonId}",
            get(get_season_episodes).delete(erase_season),
        )
        .route(
            "/Intros/ScanSeason/{SeriesId}/{SeasonId}",
            post(scan_season),
        )
        .route("/Intros/ScanStatus", get(scan_status))
        .route(
            "/FileTransformation/RegisterTransformation",
            post(register_transformation),
        )
}

#[cfg(test)]
mod tests {
    use super::{
        IMPORT_STRING, find_skip_duration_span, inject_import, mode_to_segment_type, secs_to_ticks,
        segment_type_mode_name, ticks_to_secs, update_duration_value,
    };
    use hermit_model::media_segments::MediaSegmentType;

    #[test]
    fn mode_round_trips_through_segment_type() {
        for (name, type_) in [
            ("Introduction", MediaSegmentType::Intro),
            ("Credits", MediaSegmentType::Outro),
            ("Preview", MediaSegmentType::Preview),
            ("Recap", MediaSegmentType::Recap),
            ("Commercial", MediaSegmentType::Commercial),
        ] {
            assert_eq!(mode_to_segment_type(name), Some(type_));
            assert_eq!(segment_type_mode_name(type_), Some(name));
        }
        // Aliases + numeric discriminants the plugin accepts.
        assert_eq!(mode_to_segment_type("intro"), Some(MediaSegmentType::Intro));
        assert_eq!(mode_to_segment_type("outro"), Some(MediaSegmentType::Outro));
        assert_eq!(
            mode_to_segment_type("4"),
            Some(MediaSegmentType::Commercial)
        );
        assert_eq!(mode_to_segment_type("nonsense"), None);
        assert_eq!(segment_type_mode_name(MediaSegmentType::Unknown), None);
    }

    #[test]
    fn ticks_seconds_round_trip() {
        assert_eq!(secs_to_ticks(1.5), 15_000_000);
        assert!((ticks_to_secs(15_000_000) - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn inject_import_prepends_when_absent_and_after_existing() {
        // No existing @import → prepended.
        let out = inject_import(".x { color: red; }");
        assert!(out.starts_with(IMPORT_STRING));
        // Existing @import → our import lands right after its semicolon.
        let existing = "@import url(\"a.css\");\n.x{}";
        let out = inject_import(existing);
        assert!(out.contains(IMPORT_STRING));
        let our = out.find(IMPORT_STRING).unwrap();
        let theirs = out.find("a.css").unwrap();
        assert!(theirs < our, "existing import stays first");
    }

    #[test]
    fn duration_value_inserts_updates_and_is_idempotent() {
        // Absent → appends a :root block.
        let (css, modified) = update_duration_value(".x{}", 8);
        assert!(modified);
        assert!(css.contains("--skip-hide-duration: 8s;"));
        // Present but different → replaced.
        let start = "--skip-hide-duration: 5s;";
        let (css2, modified) = update_duration_value(start, 8);
        assert!(modified);
        assert_eq!(css2, "--skip-hide-duration: 8s;");
        // Present and equal → unchanged.
        let (css3, modified) = update_duration_value(&css2, 8);
        assert!(!modified);
        assert_eq!(css3, css2);
        // The span finder matches whitespace variants.
        assert!(find_skip_duration_span("--skip-hide-duration:   12.5s;").is_some());
        assert!(find_skip_duration_span("--skip-hide-duration: ;").is_none());
    }
}
