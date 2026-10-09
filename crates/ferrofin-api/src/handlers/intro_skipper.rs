//! Intro Skipper extension controllers — the plugin API surface a Jellyfin
//! client (and the plugin's own dashboard pages) call to read/write skip
//! timestamps, drive re-scans and inject the skip-button CSS.
//!
//! Ports the routes of the upstream Intro Skipper plugin's five controllers
//! (`SkipIntro`, `SegmentEditor`, `SkipButtonCss`, `Troubleshooting`,
//! `Visualization`) plus the `FileTransformation` registration hook it depends
//! on. In Jellyfin these live in a dynamically-loaded plugin; Ferrofin compiles
//! the extension in, so the routes are served here over the same managers the
//! rest of the API uses.
//!
//! **Data-model mapping.** As upstream, the plugin keeps its own segment tier
//! (detected and user-provided timestamps, season state, disabled episodes —
//! Ferrofin-owned tables behind [`ferrofin_traits::intro_skipper`]) plus an
//! on-disk fingerprint cache. A "timestamp" read/write here is a read/write of
//! that tier; what clients play against is published into the core
//! `MediaSegments` table under Jellyfin's provider id for the plugin (the MD5
//! of `intro skipper`), with the plugin `AnalysisMode` mapped onto
//! [`MediaSegmentType`]:
//!
//! | `AnalysisMode` | [`MediaSegmentType`] |
//! |----------------|----------------------|
//! | Introduction   | `Intro`              |
//! | Credits        | `Outro`              |
//! | Preview        | `Preview`            |
//! | Recap          | `Recap`              |
//! | Commercial     | `Commercial`         |
//!
//! One route is thinner than upstream because the backing subsystem does not
//! exist in Ferrofin, documented at its handler:
//! `FileTransformation/RegisterTransformation` (no web-asset pipeline to hook).

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::intro_skipper::{AnalysisMode, AnalyzerAction};
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::intro_skipper::{self as intro_store, StoredSegment};
use ferrofin_traits::media_segments::MediaSegmentManager;
use ferrofin_traits::options::InternalItemsQuery;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{RequireAdmin, RequireAuth};
use crate::error::ApiError;
use crate::extract::{JsonBody, Query};
use crate::state::AppState;

/// The compiled-in Intro Skipper extension id (mirrors `EXTENSION_ID` in
/// `ferrofin-extensions` — the upstream plugin's GUID, which the plugin's own
/// dashboard app and client integrations hardcode).
const INTRO_SKIPPER_ID: Uuid = Uuid::from_u128(0xc83d_86bb_a1e0_4c35_a113_e210_1cf4_ee6b);
/// One second expressed in the 100-nanosecond ticks used by segment storage.
const TICKS_PER_SECOND: f64 = 10_000_000.0;

/// The skip-button CSS `@import` the plugin injects into server branding.
const IMPORT_STRING: &str = r#"@import url("https://cdn.jsdelivr.net/gh/intro-skipper/intro-skipper-css@main/skip-button.min.css");"#;

// ---------------------------------------------------------------------------
// Mode ↔ segment-type ↔ name helpers
// ---------------------------------------------------------------------------

/// The kind a stored item names (its full CLR type name).
fn kind_of(item: &BaseItemEntity) -> Option<BaseItemKind> {
    BaseItemKind::from_stored_type_name(&item.type_)
}

/// Whether an item is an Episode or Movie — the kinds the plugin's timestamp
/// routes accept.
fn is_episode_or_movie(item: &BaseItemEntity) -> bool {
    matches!(
        kind_of(item),
        Some(BaseItemKind::Episode | BaseItemKind::Movie)
    )
}

#[allow(clippy::cast_precision_loss)]
fn ticks_to_secs(ticks: i64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND
}

// ---------------------------------------------------------------------------
// Wire DTOs (local to these handlers — the plugin's contract types)
// ---------------------------------------------------------------------------

/// A single skippable region, in seconds. Mirrors the plugin `Segment`; `Valid`
/// is the computed `End > 0` flag it exposes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Segment {
    #[serde(default, with = "ferrofin_model::json::guid")]
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
    #[serde(with = "ferrofin_model::json::guid")]
    id: Uuid,
    name: String,
}

/// Query for `POST /MediaSegmentsApi/{itemId}` (`[Required] string providerId`).
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
    #[serde(default)]
    mode: Option<AnalysisMode>,
    /// Also erase the cached fingerprints (`bool eraseCache = false`).
    #[serde(default)]
    erase_cache: Option<bool>,
}

/// Query for the season/movie erase: `bool eraseCache = false`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EraseSeasonQuery {
    #[serde(default)]
    erase_cache: Option<bool>,
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Publishes `items`' segments when `UpdateMediaSegments` is on
/// (`MediaSegmentRefreshService.RefreshAsync`, `suppressErrors: true`: a
/// failure is logged and the request still succeeds — the tier already
/// changed, and the next refresh repairs the published rows).
async fn publish(state: &AppState, items: &[Uuid]) {
    if state
        .intro_skipper_analysis
        .settings()
        .await
        .update_media_segments
    {
        for &item_id in items {
            if let Err(err) = intro_store::refresh(
                state.intro_skipper.as_ref(),
                state.media_segments.as_ref(),
                item_id,
            )
            .await
            {
                tracing::error!(%err, %item_id, "intro skipper: publishing media segments failed");
            }
        }
    }
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

/// The item's timestamps from the plugin's tier, one per mode — the
/// earliest-starting (`Plugin.GetTimestampsAsync`).
async fn timestamps_for(state: &AppState, item_id: Uuid) -> Result<TimeStamps, ApiError> {
    let segments = state.intro_skipper.segments(item_id).await?;
    let mut ts = TimeStamps::default();
    for (mode, s) in intro_store::timestamps(&segments) {
        let seg = Segment::output(item_id, s.start, s.end);
        match mode {
            AnalysisMode::Introduction => ts.introduction = Some(seg),
            AnalysisMode::Credits => ts.credits = Some(seg),
            AnalysisMode::Recap => ts.recap = Some(seg),
            AnalysisMode::Preview => ts.preview = Some(seg),
            AnalysisMode::Commercial => ts.commercial = Some(seg),
            AnalysisMode::Unrecognized(_) => {}
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
    RequireAdmin(_auth): RequireAdmin,
    Path(id): Path<Uuid>,
    JsonBody(timestamps): JsonBody<TimeStamps>,
) -> Result<StatusCode, ApiError> {
    let Some(item) = state.library.get_item_by_id(id).await? else {
        return Err(ApiError::NotFound(format!("item {id}")));
    };
    if !is_episode_or_movie(&item) {
        return Err(ApiError::NotFound(format!(
            "item {id} is not an episode/movie"
        )));
    }

    // `UpdateTimestampsAsync`: each valid (`End > 0`) mode is stored as
    // user-provided — analysis never replaces it — then published.
    let modes = [
        (AnalysisMode::Introduction, timestamps.introduction),
        (AnalysisMode::Credits, timestamps.credits),
        (AnalysisMode::Recap, timestamps.recap),
        (AnalysisMode::Preview, timestamps.preview),
        (AnalysisMode::Commercial, timestamps.commercial),
    ];
    for (mode, segment) in modes {
        let Some(seg) = segment.filter(|s| s.end > 0.0) else {
            continue;
        };
        state
            .intro_skipper
            .update_timestamp(StoredSegment {
                item_id: id,
                mode,
                start: seg.start,
                end: seg.end,
                is_user_provided: true,
                config_hash: String::new(),
            })
            .await?;
    }
    publish(&state, &[id]).await;

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
    if !is_episode_or_movie(&item) {
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
) -> Result<Json<std::collections::BTreeMap<AnalysisMode, Segment>>, ApiError> {
    let segments = state.intro_skipper.segments(id).await?;
    let out = intro_store::timestamps(&segments)
        .into_iter()
        .map(|(mode, s)| (mode, Segment::output(id, s.start, s.end)))
        .collect();
    Ok(Json(out))
}

/// `POST /Intros/EraseTimestamps` — erase every stored segment of one mode.
///
/// Port of `SkipIntroController.ResetIntroTimestamps`: `[FromQuery]
/// AnalysisMode mode` is non-nullable, so an absent one is `Introduction` and
/// an undefined one a 400. One deliberate divergence: upstream leaves the
/// mode's published `MediaSegments` rows until the next analysis republishes,
/// so an erased intro keeps showing its skip button; here they go too (when
/// `UpdateMediaSegments` is on).
/// `eraseCache` also erases the mode's cached fingerprints (Introduction and
/// Credits only, as upstream).
async fn erase_timestamps(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(query): Query<EraseQuery>,
) -> Result<StatusCode, ApiError> {
    let mode = query.mode.unwrap_or(AnalysisMode::Introduction);
    let Some(kind) = intro_store::segment_type(mode) else {
        return Err(ApiError::BadRequest(format!(
            "mode: The value '{}' is invalid.",
            mode.value()
        )));
    };
    state.intro_skipper.delete_mode(mode).await?;
    if query.erase_cache == Some(true)
        && matches!(mode, AnalysisMode::Introduction | AnalysisMode::Credits)
    {
        state
            .intro_skipper_analysis
            .erase_cache(None, Some(mode))
            .await?;
    }
    if state
        .intro_skipper_analysis
        .settings()
        .await
        .update_media_segments
    {
        state
            .media_segments
            .delete_all_provider_segments(&intro_store::provider_id(), Some(kind))
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Intros/RebuildDatabase` — rebuild the plugin's tier.
///
/// Port of `IntroSkipperDbContext.RebuildDatabaseAsync`: upstream backs up its
/// rows, recreates its database and restores only valid segments (`End > 0`).
/// Ferrofin's tables are migration-managed, so the observable effect is the
/// prune of invalid segments. Not published, as upstream does not refresh.
async fn rebuild_database(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
) -> Result<StatusCode, ApiError> {
    let pruned = state.intro_skipper.prune_invalid().await?;
    tracing::info!(pruned, "intro skipper: database rebuilt");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// SegmentEditor controller
// ---------------------------------------------------------------------------

/// `GET /MediaSegmentsApi` — plugin metadata (version).
async fn segment_editor_metadata(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "version": plugin_version(&state).await }))
}

/// `POST /MediaSegmentsApi/{itemId}` — create/replace a segment for an item.
///
/// Port of `SegmentEditorController.CreateSegmentAsync`: the segment is stored
/// as user-provided in the plugin's tier (`UpdateTimestampAsync`), then
/// `MediaSegmentEditorService.CreateOrReplaceSegmentAsync` writes it under the
/// plugin's provider id — replacing the item's other segments of that type
/// (any provider), or, for a Commercial, skipping an identical one. The
/// `providerId` query is `[Required]` (400 when absent) but not used for the
/// write.
async fn create_segment(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
    Query(query): Query<CreateSegmentQuery>,
    JsonBody(segment): JsonBody<SegmentInput>,
) -> Result<StatusCode, ApiError> {
    if query.provider_id.is_none() {
        return Err(ApiError::BadRequest("providerId is required".to_owned()));
    }
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }
    let Some(mode) = intro_store::segment_mode(segment.type_) else {
        return Err(ApiError::BadRequest(format!(
            "segment type {:?} has no analysis mode",
            segment.type_
        )));
    };
    state
        .intro_skipper
        .update_timestamp(StoredSegment {
            item_id,
            mode,
            start: ticks_to_secs(segment.start_ticks),
            end: ticks_to_secs(segment.end_ticks),
            is_user_provided: true,
            config_hash: String::new(),
        })
        .await?;
    publish_edit(state.media_segments.as_ref(), item_id, &segment).await?;
    Ok(StatusCode::OK)
}

/// `MediaSegmentEditorService._itemLocks`: one lock per edited item, kept for
/// the process's lifetime.
// ponytail: never evicted, as upstream ("re-add eviction if touched item count
// becomes measurable") — one small entry per item ever edited by hand.
fn item_lock(item_id: Uuid) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    type Locks = std::collections::HashMap<Uuid, std::sync::Arc<tokio::sync::Mutex<()>>>;
    static LOCKS: std::sync::LazyLock<std::sync::Mutex<Locks>> =
        std::sync::LazyLock::new(Default::default);
    let mut locks = LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::sync::Arc::clone(locks.entry(item_id).or_default())
}

/// `MediaSegmentEditorService.CreateOrReplaceSegmentAsync`, under the item's
/// lock so two simultaneous edits publish one segment: a Commercial identical
/// to one already published is skipped; any other type replaces the item's
/// segments of that type (any provider); then the edit is published under the
/// plugin's provider id.
async fn publish_edit(
    segments: &dyn MediaSegmentManager,
    item_id: Uuid,
    segment: &SegmentInput,
) -> Result<(), ApiError> {
    let lock = item_lock(item_id);
    let _held = lock.lock().await;
    let existing = segments
        .get_segments(item_id, Some(&[segment.type_]), false)
        .await?;
    if segment.type_ == MediaSegmentType::Commercial {
        if existing
            .iter()
            .any(|e| e.start_ticks == segment.start_ticks && e.end_ticks == segment.end_ticks)
        {
            return Ok(());
        }
    } else {
        for e in existing {
            // Upstream logs a failed delete and carries on with the others.
            if let Err(err) = segments.delete_segment(e.id).await {
                tracing::warn!(%err, segment_id = %e.id, "intro skipper: could not delete a replaced segment");
            }
        }
    }
    let dto = MediaSegmentDto {
        id: Uuid::nil(),
        item_id,
        type_: segment.type_,
        start_ticks: segment.start_ticks,
        end_ticks: segment.end_ticks,
    };
    segments
        .create_segment(&dto, &intro_store::provider_id())
        .await?;
    Ok(())
}

/// Query for `DELETE /MediaSegmentsApi/{segmentId}` (both `[Required]`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteSegmentQuery {
    #[serde(default)]
    item_id: Option<Uuid>,
    #[serde(default, rename = "type")]
    type_: Option<String>,
}

/// `DELETE /MediaSegmentsApi/{segmentId}` — delete one segment.
///
/// Port of `SegmentEditorController.DeleteSegmentAsync`: the matching row of
/// the plugin's tier goes first (by type, and by the published segment's
/// bounds when it exists; a Commercial needs them), then the published
/// segment.
/// Then the episode leaves the mode's analysed list (`RemoveEpisodeIdAsync`),
/// so the next analysis treats it as not analysed.
async fn delete_segment(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(segment_id): Path<Uuid>,
    Query(query): Query<DeleteSegmentQuery>,
) -> Result<StatusCode, ApiError> {
    let (Some(item_id), Some(type_)) = (query.item_id, query.type_) else {
        return Err(ApiError::BadRequest(
            "itemId and type are required".to_owned(),
        ));
    };
    let mode = match type_.to_ascii_lowercase().as_str() {
        "intro" => AnalysisMode::Introduction,
        "recap" => AnalysisMode::Recap,
        "preview" => AnalysisMode::Preview,
        "outro" | "credits" => AnalysisMode::Credits,
        "commercial" => AnalysisMode::Commercial,
        _ => {
            return Err(ApiError::BadRequest(format!(
                "Unknown segment type '{type_}'"
            )));
        }
    };
    let existing = state
        .media_segments
        .get_segments(item_id, None, false)
        .await?
        .into_iter()
        .find(|s| s.id == segment_id);
    let range = existing
        .as_ref()
        .map(|s| (ticks_to_secs(s.start_ticks), ticks_to_secs(s.end_ticks)));
    if range.is_none() && mode == AnalysisMode::Commercial {
        return Err(ApiError::NotFound(format!("segment {segment_id}")));
    }
    // The tier row goes first; if the published delete then fails, it is put
    // back (with its user-provided flag), as upstream rolls back.
    let removed: Vec<StoredSegment> = state
        .intro_skipper
        .segments(item_id)
        .await?
        .into_iter()
        .filter(|s| {
            s.mode == mode
                && range.is_none_or(|(start, end)| {
                    (s.start - start).abs() <= intro_store::SEGMENT_COMPARISON_EPSILON
                        && (s.end - end).abs() <= intro_store::SEGMENT_COMPARISON_EPSILON
                })
        })
        .collect();
    state
        .intro_skipper
        .delete_timestamp(item_id, mode, range)
        .await?;
    if let Err(err) = state.media_segments.delete_segment(segment_id).await {
        for segment in removed {
            if let Err(restore) = state.intro_skipper.update_timestamp(segment).await {
                tracing::error!(%restore, %item_id, "intro skipper: could not restore a segment after a failed delete");
            }
        }
        return Err(err.into());
    }
    // An episode's season, else the item itself (a movie is its own season).
    let season_id = state
        .library
        .get_item_by_id(item_id)
        .await?
        .and_then(|item| {
            (kind_of(&item) == Some(BaseItemKind::Episode))
                .then(|| {
                    item.season_id
                        .as_deref()
                        .and_then(|id| Uuid::parse_str(id).ok())
                })
                .flatten()
        })
        .unwrap_or(item_id);
    state
        .intro_skipper
        .remove_episode_ids(Some(season_id), Some(mode), &[item_id])
        .await?;
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

/// Byte offset of the last ASCII-case-insensitive occurrence of `needle`.
///
/// Searches `haystack`'s own bytes so the offset stays valid for slicing it.
/// Lowercasing a copy first would not: `str::to_lowercase` is Unicode-aware and
/// not byte-length preserving (`İ` U+0130 is 2 bytes and lowercases to 3, `ẞ`
/// U+1E9E is 3 and lowercases to 2), so offsets taken from the copy drift and
/// can land mid-character in the original — a wrong insertion point or a panic.
/// `needle` is ASCII (`@import`), and .NET's `StringComparison.OrdinalIgnoreCase`
/// folds nothing outside ASCII onto an ASCII needle, so ASCII folding matches
/// upstream exactly. A match implies `haystack[i]` is ASCII, hence a boundary.
fn rfind_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let (hay, ndl) = (haystack.as_bytes(), needle.as_bytes());
    let last_start = hay.len().checked_sub(ndl.len())?;
    (0..=last_start)
        .rev()
        .find(|&i| hay[i..i + ndl.len()].eq_ignore_ascii_case(ndl))
}

/// Inserts the `@import` after the last existing `@import`, else prepends it.
/// Port of `InjectImport`.
fn inject_import(css: &str) -> String {
    if let Some(last_import) = rfind_ascii_case_insensitive(css, "@import") {
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
    RequireAdmin(_auth): RequireAdmin,
) -> Result<StatusCode, ApiError> {
    let delay = state
        .intro_skipper_analysis
        .settings()
        .await
        .skip_button_hide_delay;
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
    RequireAdmin(_auth): RequireAdmin,
) -> Result<StatusCode, ApiError> {
    let delay = state
        .intro_skipper_analysis
        .settings()
        .await
        .skip_button_hide_delay;
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
    RequireAdmin(_auth): RequireAdmin,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "version": plugin_version(&state).await }))
}

/// `GET /IntroSkipper/SupportBundle` — a plain-text Markdown troubleshooting
/// bundle (`TroubleshootingController.GetSupportBundle`): the versions and
/// platform, then the analysis's own report (queue contents, warnings, the
/// server ffmpeg's capability checks).
async fn support_bundle(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
) -> String {
    let version = plugin_version(&state).await;
    // `ApplicationVersionString`: the version `/System/Info` reports.
    let server = state
        .system
        .get_public_system_info(&ferrofin_traits::net::RequestContext::default())
        .await
        .ok()
        .and_then(|info| info.version)
        .unwrap_or_default();
    format!(
        "* Jellyfin version: {server} (Ferrofin {ferrofin})\n\
         * Plugin version: {version}\n\
         * Runs on: {os}\n\
         {analysis}",
        ferrofin = env!("CARGO_PKG_VERSION"),
        os = operating_system(),
        analysis = state.intro_skipper_analysis.support_bundle().await,
    )
}

/// .NET's `RuntimeInformation.OSDescription` on Linux: the kernel's
/// `uname -srv`.
fn os_description() -> String {
    let read = |name: &str| {
        std::fs::read_to_string(format!("/proc/sys/kernel/{name}"))
            .map(|value| value.trim().to_owned())
            .unwrap_or_default()
    };
    format!(
        "{} {} {}",
        read("ostype"),
        read("osrelease"),
        read("version")
    )
    .trim()
    .to_owned()
}

/// `Helper.OperatingSystem.DetermineOperatingSystem`.
fn operating_system() -> String {
    match std::env::consts::OS {
        "windows" => "Windows".to_owned(),
        "macos" => "macOS".to_owned(),
        "linux" => {
            let docker = ["/.dockerenv", "/run/.containerenv"]
                .iter()
                .any(|marker| std::path::Path::new(marker).exists());
            if !docker {
                return os_description();
            }
            if std::env::var_os("ATTACHED_DEVICES_PERMS").is_some() {
                "LinuxServer.io image (Docker)".to_owned()
            } else if std::env::var_os("WEBUI_PORTS").is_some() {
                "hotio image (Docker)".to_owned()
            } else {
                "Linux (Docker)".to_owned()
            }
        }
        _ => "Unknown".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Visualization controller
// ---------------------------------------------------------------------------

/// `GET /Intros/ScanStatus` — whether an analysis pass is running
/// (`ScheduledTaskSemaphore.IsBusy`: the scheduled detection, the Media
/// Segment Scan or a season rescan).
async fn scan_status(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
) -> Result<Json<ScanStatusResponse>, ApiError> {
    Ok(Json(ScanStatusResponse {
        is_running: state.intro_skipper_analysis.is_running(),
    }))
}

/// Projects a season's episode rows into the visualization list.
///
/// A row whose stored `Id` is not a Guid is **dropped** rather than listed under
/// the nil GUID: `Id` is the key the visualization page sends straight back to
/// fetch that episode's segments, and the nil one resolves to nothing. Upstream
/// reads a `Guid` column here and can only ever emit real ids.
fn episode_visualizations(episodes: Vec<BaseItemEntity>) -> Vec<EpisodeVisualization> {
    episodes
        .into_iter()
        .filter_map(|e| {
            let id = Uuid::parse_str(&e.id).ok()?;
            Some(EpisodeVisualization {
                id,
                name: e.name.unwrap_or_default(),
            })
        })
        .collect()
}

/// `GET /Intros/Show/{SeriesId}/{SeasonId}` — the episodes of a season.
async fn get_season_episodes(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path((_series_id, season_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<EpisodeVisualization>>, ApiError> {
    let episodes = season_episodes(&state, season_id).await?;
    if episodes.is_empty() {
        return Err(ApiError::NotFound(format!(
            "season {season_id} has no episodes"
        )));
    }
    Ok(Json(episode_visualizations(episodes)))
}

/// `DELETE /Intros/Show/{SeriesId}/{SeasonId}` — erase the season's Intro
/// Skipper segments. Port of `VisualizationController.EraseSeasonAsync`.
async fn erase_season(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path((_series_id, season_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<EraseSeasonQuery>,
) -> Result<StatusCode, ApiError> {
    let episodes = season_episodes(&state, season_id).await?;
    if episodes.is_empty() {
        return Err(ApiError::NotFound(format!(
            "season {season_id} has no episodes"
        )));
    }
    let ids: Vec<Uuid> = episodes
        .iter()
        .filter_map(|e| Uuid::parse_str(&e.id).ok())
        .collect();
    erase_items(&state, season_id, &ids, query.erase_cache == Some(true)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `EraseSeasonAsync`'s body: every segment of `ids` goes, user-provided too,
/// the cache with them when asked, then the (now empty) set is republished.
/// The season's analysed-episode lists are emptied, so it is analysed afresh.
async fn erase_items(
    state: &AppState,
    season_id: Uuid,
    ids: &[Uuid],
    erase_cache: bool,
) -> Result<(), ApiError> {
    state.intro_skipper.delete_items(ids).await?;
    state
        .intro_skipper
        .clear_episode_ids(season_id, None)
        .await?;
    if erase_cache {
        state
            .intro_skipper_analysis
            .erase_cache(Some(ids), None)
            .await?;
    }
    publish(state, ids).await;
    Ok(())
}

/// `DELETE /Intros/Show/{MovieId}` — erase a movie's Intro Skipper segments.
///
/// Not an upstream route: the plugin's dashboard erases a movie with this
/// single-id form, which upstream never serves (its button fails). Owner
/// decision D9 serves it: `EraseSeasonAsync` with the movie as the one item,
/// as the plugin queues a movie under its own id. 404 unless the id is a Movie.
async fn erase_movie(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(movie_id): Path<Uuid>,
    Query(query): Query<EraseSeasonQuery>,
) -> Result<StatusCode, ApiError> {
    let is_movie = state
        .library
        .get_item_by_id(movie_id)
        .await?
        .is_some_and(|item| kind_of(&item) == Some(BaseItemKind::Movie));
    if !is_movie {
        return Err(ApiError::NotFound(format!("movie {movie_id}")));
    }
    erase_items(
        &state,
        movie_id,
        &[movie_id],
        query.erase_cache == Some(true),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The episode ids a JSON array carries, in Jellyfin's `"N"` form.
#[derive(Debug, Serialize)]
#[serde(transparent)]
struct GuidList(#[serde(with = "ferrofin_model::json::guid::vec")] Vec<Uuid>);

/// `GET /Intros/DisabledEpisodes/{SeasonId}` — the season's episodes excluded
/// from media-segment output.
///
/// Port of `VisualizationController.GetDisabledEpisodes` →
/// `Plugin.GetMediaSegmentExcludedEpisodeIdsAsync` (no existence check: an
/// unknown season has none).
async fn get_disabled_episodes(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(season_id): Path<Uuid>,
) -> Result<Json<GuidList>, ApiError> {
    let mut ids: Vec<Uuid> = state
        .intro_skipper
        .excluded_episodes(season_id)
        .await?
        .into_iter()
        .collect();
    ids.sort_unstable();
    Ok(Json(GuidList(ids)))
}

/// The body of `POST /Intros/DisabledEpisodes/Update`
/// (`UpdateEpisodeMediaSegmentRequest`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UpdateEpisodeMediaSegmentRequest {
    /// The season.
    #[serde(default, with = "ferrofin_model::json::guid")]
    season_id: Uuid,
    /// The episode.
    #[serde(default, with = "ferrofin_model::json::guid")]
    episode_id: Uuid,
    /// Exclude (`true`) or include it again.
    #[serde(default)]
    disabled: bool,
}

/// `POST /Intros/DisabledEpisodes/Update` — exclude or re-include an episode's
/// media-segment output.
///
/// Port of `VisualizationController.UpdateDisabledEpisode`: 404 unless the
/// episode belongs to the season (`IsEpisodeInSeason`'s library arm: an
/// `Episode` whose `SeasonId` is it); the flag is stored, then the episode's
/// published segments are removed or republished
/// (`RemoveIntroSkipperSegmentsAsync` / `RefreshAsync` — not gated on
/// `UpdateMediaSegments` upstream either). The stored segments stay, so
/// re-including restores them without re-analysis.
async fn update_disabled_episode(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    JsonBody(request): JsonBody<UpdateEpisodeMediaSegmentRequest>,
) -> Result<StatusCode, ApiError> {
    let in_season = state
        .library
        .get_item_by_id(request.episode_id)
        .await?
        .is_some_and(|item| {
            kind_of(&item) == Some(BaseItemKind::Episode)
                && item
                    .season_id
                    .as_deref()
                    .and_then(|id| Uuid::parse_str(id).ok())
                    == Some(request.season_id)
        });
    if !in_season {
        return Err(ApiError::NotFound(format!(
            "episode {} in season {}",
            request.episode_id, request.season_id
        )));
    }
    state
        .intro_skipper
        .set_excluded(request.season_id, request.episode_id, request.disabled)
        .await?;
    if request.disabled {
        intro_store::remove_published(state.media_segments.as_ref(), request.episode_id).await?;
    } else {
        intro_store::refresh(
            state.intro_skipper.as_ref(),
            state.media_segments.as_ref(),
            request.episode_id,
        )
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `ClearExcludedTimestampsResponse` (PascalCase, as the dashboard reads it).
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct ClearExcludedTimestampsResponse {
    affected_items: u64,
    removed_segments: u64,
    removed_cache_entries: u64,
}

/// `POST /Intros/ExcludedTimestamps/Clear` — clear the data of items the
/// exclusion policy now matches.
///
/// Port of `VisualizationController.ClearExcludedTimestampsAsync`: their
/// stored segments and cache go and they are republished; a failure is a 500,
/// as upstream answers.
async fn clear_excluded_timestamps(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
) -> Result<Json<ClearExcludedTimestampsResponse>, ApiError> {
    match state.intro_skipper_analysis.clear_excluded().await {
        Ok(cleared) => Ok(Json(ClearExcludedTimestampsResponse {
            affected_items: cleared.affected_items,
            removed_segments: cleared.removed_segments,
            removed_cache_entries: cleared.removed_cache_entries,
        })),
        Err(err) => {
            tracing::error!(%err, "intro skipper: failed to clear excluded timestamp data");
            Err(ApiError::Service(ServiceError::backend(
                "An unexpected error occurred while clearing excluded timestamp data.",
            )))
        }
    }
}

/// `GET /Intros/AnalyzerActions/{SeasonId}` — the per-mode analyzer actions for
/// a season.
///
/// Port of `VisualizationController.GetAnalyzerAction` →
/// `Plugin.GetAllAnalyzerActionsAsync`: every mode in declaration order, the
/// stored action or `Default`. Elevated, as the whole controller is upstream.
/// 404 unless the id is a season with episodes or a movie — upstream's
/// `QueuedMediaItems.ContainsKey`, whose queue keys a movie by its own id.
async fn get_analyzer_actions(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(season_id): Path<Uuid>,
) -> Result<Json<std::collections::BTreeMap<AnalysisMode, AnalyzerAction>>, ApiError> {
    if season_episodes(&state, season_id).await?.is_empty()
        && !state
            .library
            .get_item_by_id(season_id)
            .await?
            .is_some_and(|item| kind_of(&item) == Some(BaseItemKind::Movie))
    {
        return Err(ApiError::NotFound(format!("season {season_id}")));
    }
    let stored = state.intro_skipper.analyzer_actions(season_id).await?;
    Ok(Json(
        AnalysisMode::ALL
            .into_iter()
            .map(|mode| (mode, stored.get(&mode).copied().unwrap_or_default()))
            .collect(),
    ))
}

/// The body of `POST /Intros/AnalyzerActions/UpdateSeason`. Port of
/// `IntroSkipper.Data.UpdateAnalyzerActionsRequest`, bound by the MVC binder
/// upstream (`VisualizationController.UpdateAnalyzerActions([FromBody] …)`), so
/// it takes an object only and its names and enum values bind ignoring case.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UpdateAnalyzerActionsRequest {
    /// The season. `null` is the empty id, as Jellyfin's `JsonGuidConverter` reads it.
    #[serde(default, with = "ferrofin_model::json::guid")]
    id: Uuid,
    /// The action per analysis mode.
    #[serde(default)]
    analyzer_actions: HashMap<AnalysisMode, AnalyzerAction>,
}

/// `POST /Intros/AnalyzerActions/UpdateSeason` — store per-season analyzer
/// actions.
///
/// Port of `VisualizationController.UpdateAnalyzerActions` →
/// `Plugin.SetAnalyzerActionAsync`: each named mode's action is replaced, the
/// others kept; detection honours them (a mode set to `None` is not analysed
/// for the season). Elevated, as the whole controller is upstream.
async fn update_analyzer_actions(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    JsonBody(request): JsonBody<UpdateAnalyzerActionsRequest>,
) -> Result<StatusCode, ApiError> {
    let actions: Vec<(AnalysisMode, AnalyzerAction)> =
        request.analyzer_actions.into_iter().collect();
    state
        .intro_skipper
        .set_analyzer_actions(request.id, &actions)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Intros/ScanSeason/{SeriesId}/{SeasonId}` — rescan one season.
///
/// Port of `VisualizationController.ScanSeason`: 409 when a pass is already
/// running; otherwise, in the background, the season's segments and cache are
/// erased and only that season is analysed again (202).
/// 404 while the plugin is disabled, as the route of an unloaded plugin.
async fn scan_season(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path((_series_id, season_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    if state.intro_skipper_analysis.rescan(season_id).await? {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ApiError::Conflict(
            "A scan is already in progress.".to_owned(),
        ))
    }
}

// ---------------------------------------------------------------------------
// FileTransformation hook
// ---------------------------------------------------------------------------

/// The File Transformation plugin's registration body
/// (`TransformationRegistrationPayload` upstream, camelCase on the wire).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct TransformationRegistration {
    /// The registering plugin's id.
    #[serde(with = "ferrofin_model::json::guid")]
    id: Uuid,
    /// The exact web-root-relative path or regex to transform.
    file_name_pattern: String,
    /// The HTTP callback the pipeline POSTs `{"contents": …}` to.
    transformation_endpoint: String,
    /// A .NET named-pipe callback (unsupported — .NET-process-specific).
    transformation_pipe: Option<String>,
    /// A .NET assembly-reflection callback (unsupported — compiled-in
    /// extensions register natively through the service instead).
    callback_assembly: Option<String>,
}

/// `POST /FileTransformation/RegisterTransformation` — register a web-file
/// transformation.
///
/// Port of `FileTransformationController.RegisterTransformation`: registers an
/// HTTP-callback transformation with the pipeline the static `/web` mount
/// consults. The .NET-specific callback forms (assembly reflection, named
/// pipes) cannot exist in a Rust process; a registration carrying only those is
/// still `200` (upstream always is) but logged, since it can never fire.
///
/// **Elevation is required**, as upstream requires it
/// (`[Authorize(Policy = Policies.RequiresElevation)]` on
/// `FileTransformationController.RegisterTransformation`). This port previously
/// took a bare [`RequireAuth`], which was both a divergence from upstream and
/// the reachable end of an unbounded registry: a registration is keyed by an id
/// **and** a pattern the caller supplies, nothing sweeps it, and each accepted
/// one retains the caller's strings for the life of the process — measured at
/// +157 MB of RssAnon over 150 requests carrying 1 MB of strings each. The
/// registry is capped independently in `ferrofin-extensions`; this gate is what
/// keeps a guest profile or a stolen playback token away from it at all, and it
/// belongs here for the same reason `plugins::require_admin` does — registering
/// a callback that rewrites the JavaScript served to every browser is staging
/// code, not editing metadata.
async fn register_transformation(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    JsonBody(payload): JsonBody<TransformationRegistration>,
) -> Result<StatusCode, ApiError> {
    let Some(service) = state.file_transformations.as_ref() else {
        tracing::warn!("file-transformation registration dropped: pipeline not wired");
        return Ok(StatusCode::OK);
    };
    if payload.transformation_endpoint.is_empty() {
        tracing::warn!(
            id = %payload.id,
            pattern = payload.file_name_pattern,
            assembly = payload.callback_assembly.as_deref().unwrap_or_default(),
            pipe = payload.transformation_pipe.as_deref().unwrap_or_default(),
            "file-transformation registration has no HTTP endpoint; \
             .NET assembly/pipe callbacks are unsupported in Ferrofin"
        );
        return Ok(StatusCode::OK);
    }
    service
        .add_endpoint_transformation(
            payload.id,
            &payload.file_name_pattern,
            &payload.transformation_endpoint,
        )
        .await;
    Ok(StatusCode::OK)
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
            "/Intros/DisabledEpisodes/{SeasonId}",
            get(get_disabled_episodes),
        )
        .route(
            "/Intros/DisabledEpisodes/Update",
            post(update_disabled_episode),
        )
        .route(
            "/Intros/ExcludedTimestamps/Clear",
            post(clear_excluded_timestamps),
        )
        .route(
            "/Intros/Show/{SeriesId}/{SeasonId}",
            get(get_season_episodes).delete(erase_season),
        )
        .route("/Intros/Show/{MovieId}", axum::routing::delete(erase_movie))
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

crate::query::query_parameters! {
    CreateSegmentQuery {} => [];
    DeleteSegmentQuery {} => [];
    EraseSeasonQuery {} => [];
    EraseQuery {} => [];
}

#[cfg(test)]
mod tests {
    use super::{
        IMPORT_STRING, episode_visualizations, find_skip_duration_span, inject_import,
        ticks_to_secs, update_duration_value,
    };
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use uuid::Uuid;

    #[test]
    fn items_are_told_apart_by_their_stored_type_name() {
        use ferrofin_model::data::BaseItemKind;
        let item = |kind: BaseItemKind| BaseItemEntity {
            type_: kind.stored_type_name().expect("stored name").to_owned(),
            ..BaseItemEntity::default()
        };
        assert_eq!(
            super::kind_of(&item(BaseItemKind::Movie)),
            Some(BaseItemKind::Movie)
        );
        assert!(super::is_episode_or_movie(&item(BaseItemKind::Episode)));
        assert!(super::is_episode_or_movie(&item(BaseItemKind::Movie)));
        assert!(!super::is_episode_or_movie(&item(BaseItemKind::Season)));
    }

    #[test]
    fn episode_visualizations_drop_rows_whose_id_is_not_a_guid() {
        let good = Uuid::from_u128(0x5EA5);
        let episodes = vec![
            BaseItemEntity {
                id: good.to_string(),
                name: Some("S01E01".to_owned()),
                ..BaseItemEntity::default()
            },
            BaseItemEntity {
                id: "not-a-guid".to_owned(),
                name: Some("S01E02".to_owned()),
                ..BaseItemEntity::default()
            },
        ];
        let out = episode_visualizations(episodes);
        // The malformed row is skipped, not published as the nil GUID — an id the
        // page would send back to a segments lookup that can never match.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, good);
        assert_eq!(out[0].name, "S01E01");
        assert!(!out.iter().any(|e| e.id.is_nil()));
    }

    #[test]
    fn episode_visualizations_default_a_missing_name_to_empty() {
        let out = episode_visualizations(vec![BaseItemEntity {
            id: Uuid::from_u128(1).to_string(),
            name: None,
            ..BaseItemEntity::default()
        }]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "");
    }

    #[test]
    fn ticks_convert_to_seconds() {
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

    /// Non-ASCII branding CSS must not shift the `@import` search offset.
    /// `str::to_lowercase` is not byte-length preserving, so an offset taken
    /// from a lowercased copy and used on the original either lands mid-char
    /// (panic) or points somewhere else entirely (import injected into the
    /// wrong place, where a browser drops it).
    #[test]
    fn inject_import_handles_non_ascii_css() {
        // `ẞ` (U+1E9E, 3 bytes) lowercases to `ß` (2 bytes), so a lowercased-copy
        // offset points one byte *before* the real `@` — here into the middle of
        // the 2-byte `é`, which is a slicing panic, not merely a wrong answer.
        let shrinking = "/* ẞ */é@import url(\"a.css\");\n.x{}";
        let out = inject_import(shrinking);
        let theirs = out.find("a.css").expect("existing import kept");
        let ours = out.find(IMPORT_STRING).expect("our import injected");
        assert!(theirs < ours, "our import follows the existing one");
        assert!(
            out.starts_with("/* ẞ */é@import url(\"a.css\");\n"),
            "existing rules are preserved verbatim: {out:?}"
        );

        // `İ` (U+0130, 2 bytes) lowercases to 3 bytes: the offset drifts
        // forward, past the existing import's `;` and into the next rule.
        let growing = format!(
            "/* {} */\n@import url(\"a.css\");\n.x {{ color: red; }}\n",
            "İ".repeat(25)
        );
        let out = inject_import(&growing);
        let theirs = out.find("a.css").expect("existing import kept");
        let ours = out.find(IMPORT_STRING).expect("our import injected");
        let rule = out.find(".x {").expect("following rule kept");
        assert!(theirs < ours, "our import follows the existing one");
        assert!(
            ours < rule,
            "our import stays at top level, not inside the next rule: {out:?}"
        );
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

#[cfg(test)]
mod numeric_contract_tests {
    #[test]
    fn analyzer_enums_accept_numbers_in_values_and_dictionary_keys() {
        use super::{AnalysisMode as Mode, AnalyzerAction as Action, UpdateAnalyzerActionsRequest};
        let body: UpdateAnalyzerActionsRequest = crate::extract::deserialize_mvc(
            r#"{"AnalyzerActions":{"0":"2","Credits":3,"999":-1}}"#,
        )
        .unwrap();
        assert_eq!(
            body.analyzer_actions[&Mode::Introduction],
            Action::Chromaprint
        );
        assert_eq!(body.analyzer_actions[&Mode::Credits], Action::BlackFrame);
        assert_eq!(
            body.analyzer_actions[&Mode::Unrecognized(999)],
            Action::Unrecognized(-1)
        );
        for bad in [
            r#"{"AnalyzerActions":{"Introduction":2.0}}"#,
            r#"{"AnalyzerActions":{"no-such-mode":2}}"#,
            r#"{"AnalyzerActions":{"0":"no-such-action"}}"#,
        ] {
            assert!(crate::extract::deserialize_mvc::<UpdateAnalyzerActionsRequest>(bad).is_err());
        }
    }
    #[test]
    fn segment_contract_numbers_accept_quoted_values() {
        assert_eq!(
            crate::extract::contract_numbers::check_model(
                "Segment",
                super::Segment::output(uuid::Uuid::nil(), 0.0, 0.0)
            ),
            2
        );
    }
}

#[cfg(test)]
mod edit_lock_tests {
    use super::{SegmentInput, publish_edit};
    use async_trait::async_trait;
    use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::media_segments::{MediaSegmentManager, MediaSegmentProviderInfo};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    /// Segments in memory; each read yields after taking its snapshot, as a
    /// real store awaits there, so a concurrent edit reads the same snapshot.
    #[derive(Default)]
    struct YieldingSegments(Mutex<Vec<MediaSegmentDto>>);

    #[async_trait]
    impl MediaSegmentManager for YieldingSegments {
        async fn is_type_supported(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn create_segment(
            &self,
            segment: &MediaSegmentDto,
            _provider: &str,
        ) -> Result<MediaSegmentDto, ServiceError> {
            let mut rows = self.0.lock().expect("lock");
            let created = MediaSegmentDto {
                id: Uuid::from_u128(rows.len() as u128 + 1),
                ..segment.clone()
            };
            rows.push(created.clone());
            Ok(created)
        }
        async fn delete_segment(&self, segment_id: Uuid) -> Result<(), ServiceError> {
            self.0.lock().expect("lock").retain(|s| s.id != segment_id);
            Ok(())
        }
        async fn delete_segments(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_provider_segments(
            &self,
            _item_id: Uuid,
            _provider_id: &str,
            _type_filter: Option<MediaSegmentType>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_segments(
            &self,
            item_id: Uuid,
            types: Option<&[MediaSegmentType]>,
            _by_provider: bool,
        ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
            let found = self
                .0
                .lock()
                .expect("lock")
                .iter()
                .filter(|s| s.item_id == item_id && types.is_none_or(|t| t.contains(&s.type_)))
                .cloned()
                .collect();
            tokio::task::yield_now().await;
            Ok(found)
        }
        async fn has_segments(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(false)
        }
        async fn get_supported_providers(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<MediaSegmentProviderInfo>, ServiceError> {
            Ok(Vec::new())
        }
    }

    /// `MediaSegmentEditorService`'s per-item lock: two simultaneous edits of
    /// one item's Intro publish one segment, the later one.
    #[tokio::test]
    async fn simultaneous_edits_of_an_item_publish_one_segment() {
        let segments = Arc::new(YieldingSegments::default());
        let item = Uuid::from_u128(0x5d);
        let edit = |end_ticks| SegmentInput {
            type_: MediaSegmentType::Intro,
            start_ticks: 0,
            end_ticks,
        };
        let (first, second) = (edit(10), edit(20));
        let (a, b) = tokio::join!(
            publish_edit(segments.as_ref(), item, &first),
            publish_edit(segments.as_ref(), item, &second),
        );
        a.expect("first");
        b.expect("second");
        let rows = segments.0.lock().expect("lock").clone();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].end_ticks, 20);
    }
}
