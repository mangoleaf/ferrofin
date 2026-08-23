//! `ItemUpdateController` / `ItemRefreshController` — item writes.
//!
//! Ports the portable item-write surface:
//!
//! - `POST /Items/{itemId}` — applies an edited [`BaseItemDto`] onto the stored
//!   item row and persists it.
//! - `POST /Items/{itemId}/ContentType` — sets (or clears) the configured
//!   content-type override for the item's folder in the server configuration.
//! - `POST /Items/{itemId}/Refresh` — queues a metadata/image refresh for the
//!   item at high priority.
//! - `GET /Items/{itemId}/MetadataEditor` — the reference data (parental ratings,
//!   countries, cultures, external-id descriptors, content-type options) a client
//!   needs to render the item's metadata editor.
//!
//! Faithfulness notes / deferrals: the C# `UpdateItem` cascades edits onto a
//! series' seasons/episodes and an album's tracks (and queues a provider refresh
//! when a series' display order changes). Those walks need the un-ported `Folder`
//! OOP child tree, so the portable seam applies the edit to the addressed row and
//! defers the cascades. Every scalar/collection
//! field on the row is updated faithfully.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::dto::{MetadataEditorInfo, NameGuidPair, NameValuePair};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions, RefreshPriority};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::state::AppState;

/// `POST /Items/{itemId}` — applies an edited item and persists it.
///
/// Port of `ItemUpdateController.UpdateItem` (scalar/collection subset). A
/// missing item is a `404`; on success the row is saved and the handler returns
/// `204`.
#[utoipa::path(
    post,
    path = "/Items/{itemId}",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Item updated"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
pub(crate) async fn update_item(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
    Json(request): Json<Box<UpdateItemRequest>>,
) -> Result<StatusCode, ApiError> {
    let mut item = state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let before = item.clone();
    apply_update(&mut item, &request);
    // Deliberate divergence from Jellyfin: a save that actually changes
    // metadata locks the item, because Ferrofin's library scan rebuilds rows
    // from disk and would otherwise revert the edit on its next pass — the
    // scan preserves the editable columns only for locked rows. A save that
    // changes nothing keeps the lock exactly as the checkbox sent it, so
    // un-ticking "Lock this item" (without editing fields) still unlocks.
    if !item.is_locked && editor_fields_changed(&before, &item) {
        item.is_locked = true;
    }
    state.library.update_items(&[item], None).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The editable subset of a `BaseItemDto` the metadata editor `POST`s.
///
/// jellyfin-web's editor sends the whole item, but only these fields are applied;
/// modelling just them (unknown fields are ignored) keeps the write path focused.
/// Crucially, its number inputs serialize as **strings** (`"ProductionYear": "2010"`)
/// and cleared dates as `""`, which Jellyfin's C# binder coerces but strict serde
/// rejects (a `422`). The numeric/date fields therefore use tolerant deserializers
/// that accept a string, a number, or an empty value.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct UpdateItemRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    forced_sort_name: Option<String>,
    #[serde(default)]
    original_title: Option<String>,
    #[serde(default, deserialize_with = "opt_f32")]
    critic_rating: Option<f32>,
    #[serde(default, deserialize_with = "opt_f32")]
    community_rating: Option<f32>,
    #[serde(default, deserialize_with = "opt_i32")]
    index_number: Option<i32>,
    #[serde(default, deserialize_with = "opt_i32")]
    parent_index_number: Option<i32>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    genres: Option<Vec<String>>,
    #[serde(default)]
    taglines: Option<Vec<String>>,
    #[serde(default)]
    studios: Option<Vec<NameGuidPair>>,
    #[serde(default, deserialize_with = "opt_date")]
    date_created: Option<DateTime<Utc>>,
    #[serde(default)]
    series_name: Option<String>,
    #[serde(default, deserialize_with = "opt_date")]
    end_date: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "opt_date")]
    premiere_date: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "opt_i32")]
    production_year: Option<i32>,
    #[serde(default)]
    official_rating: Option<String>,
    #[serde(default)]
    custom_rating: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    production_locations: Option<Vec<String>>,
    #[serde(default)]
    preferred_metadata_country_code: Option<String>,
    #[serde(default)]
    preferred_metadata_language: Option<String>,
    #[serde(default)]
    lock_data: Option<bool>,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    artist_items: Option<Vec<NameGuidPair>>,
    #[serde(default)]
    album_artists: Option<Vec<NameGuidPair>>,
}

/// A JSON value that is either a number or a (possibly numeric) string — the shape
/// the metadata editor emits for its number inputs.
#[derive(Deserialize)]
#[serde(untagged)]
enum NumOrStr {
    Num(f64),
    Str(String),
}

impl NumOrStr {
    /// The numeric value, or `None` for an empty/unparseable string.
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            Self::Str(s) => {
                let s = s.trim();
                (!s.is_empty()).then(|| s.parse().ok()).flatten()
            }
        }
    }
}

/// Deserializes an optional `i32` that may arrive as a number, a numeric string,
/// or an empty string (`""` → `None`).
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn opt_i32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i32>, D::Error> {
    Ok(Option::<NumOrStr>::deserialize(d)?
        .as_ref()
        .and_then(NumOrStr::as_f64)
        .map(|n| n as i32))
}

/// Deserializes an optional `f32` that may arrive as a number or a numeric string.
#[allow(clippy::cast_possible_truncation)]
fn opt_f32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
    Ok(Option::<NumOrStr>::deserialize(d)?
        .as_ref()
        .and_then(NumOrStr::as_f64)
        .map(|n| n as f32))
}

/// Deserializes an optional timestamp the way Jellyfin reads one: a cleared
/// field (`null` or `""`) is `None`, and a bare date — what jellyfin-web's
/// metadata editor sends for a date the user changed — is midnight UTC.
fn opt_date<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
    ferrofin_model::json::datetime::option::deserialize(d)
}

/// Whether any editor-owned field differs between the stored row and the
/// applied request — the auto-lock trigger.
///
/// jellyfin-web round-trips the values it did not edit through the item DTO,
/// which narrows ratings to `f32` and dates to its own serialization, so the
/// comparison normalizes both (ratings through `f32`, dates to whole seconds)
/// to keep an untouched save from reading as a change and spuriously locking.
#[allow(clippy::cast_possible_truncation)]
fn editor_fields_changed(before: &BaseItemEntity, after: &BaseItemEntity) -> bool {
    let rating = |v: Option<f64>| v.map(|x| x as f32);
    let date = |v: Option<DateTime<Utc>>| v.map(|d| d.timestamp());
    before.name != after.name
        || before.forced_sort_name != after.forced_sort_name
        || before.original_title != after.original_title
        || rating(before.critic_rating) != rating(after.critic_rating)
        || rating(before.community_rating) != rating(after.community_rating)
        || before.index_number != after.index_number
        || before.parent_index_number != after.parent_index_number
        || before.overview != after.overview
        || before.genres != after.genres
        || before.tagline != after.tagline
        || before.studios != after.studios
        || date(before.date_created) != date(after.date_created)
        || before.series_name != after.series_name
        || date(before.end_date) != date(after.end_date)
        || date(before.premiere_date) != date(after.premiere_date)
        || before.production_year != after.production_year
        || before.official_rating != after.official_rating
        || before.custom_rating != after.custom_rating
        || before.tags != after.tags
        || before.production_locations != after.production_locations
        || before.preferred_metadata_country_code != after.preferred_metadata_country_code
        || before.preferred_metadata_language != after.preferred_metadata_language
        || before.album != after.album
        || before.artists != after.artists
        || before.album_artists != after.album_artists
}

/// Applies the editable fields of `request` onto `item`. Mirrors the scalar and
/// collection assignments of C# `ItemUpdateController.UpdateItem`; the
/// series/season/album child cascades are deferred (see the module docs).
fn apply_update(item: &mut BaseItemEntity, request: &UpdateItemRequest) {
    item.name.clone_from(&request.name);
    item.forced_sort_name.clone_from(&request.forced_sort_name);
    item.original_title = non_empty(request.original_title.as_deref());
    item.critic_rating = request.critic_rating.map(f64::from);
    item.community_rating = request.community_rating.map(f64::from);
    item.index_number = request.index_number.map(i64::from);
    item.parent_index_number = request.parent_index_number.map(i64::from);
    item.overview.clone_from(&request.overview);

    if let Some(genres) = &request.genres {
        item.genres = Some(join_distinct(genres));
    }
    if let Some(taglines) = &request.taglines {
        item.tagline = taglines.first().cloned();
    }
    if let Some(studios) = &request.studios {
        let names: Vec<String> = studios
            .iter()
            .filter_map(|s| s.name.clone())
            .collect::<Vec<_>>();
        item.studios = Some(join_distinct(&names));
    }
    if let Some(created) = request.date_created {
        item.date_created = Some(created);
    }
    if let Some(series_name) = &request.series_name {
        item.series_name = Some(series_name.clone());
    }

    item.end_date = request.end_date;
    item.premiere_date = request.premiere_date;
    item.production_year = request.production_year.map(i64::from);
    item.official_rating = non_empty(request.official_rating.as_deref());
    item.custom_rating.clone_from(&request.custom_rating);

    if let Some(tags) = &request.tags {
        item.tags = Some(join_distinct(tags));
    }
    if let Some(locations) = &request.production_locations {
        item.production_locations = Some(join_distinct(locations));
    }

    item.preferred_metadata_country_code
        .clone_from(&request.preferred_metadata_country_code);
    item.preferred_metadata_language
        .clone_from(&request.preferred_metadata_language);
    item.is_locked = request.lock_data.unwrap_or(false);

    if let Some(album) = &request.album {
        item.album = Some(album.clone());
    }
    if let Some(artist_items) = &request.artist_items {
        let names: Vec<String> = artist_items
            .iter()
            .filter_map(|a| a.name.clone())
            .collect::<Vec<_>>();
        item.artists = Some(join_distinct(&names));
    }
    if let Some(album_artists) = &request.album_artists {
        let names: Vec<String> = album_artists
            .iter()
            .filter_map(|a| a.name.clone())
            .collect::<Vec<_>>();
        item.album_artists = Some(join_distinct(&names));
    }
}

/// Returns the trimmed value, or [`None`] when it is blank — mirrors the C#
/// `string.IsNullOrWhiteSpace(x) ? null : x` guards.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Joins values with `|` after a case-insensitive de-duplication, matching how
/// `ferrofin-db` stores the `Genres`/`Studios`/`Artists`/`Tags` columns and C#'s
/// `Distinct(StringComparer.OrdinalIgnoreCase)`.
fn join_distinct(values: &[String]) -> String {
    let mut seen: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if !seen.iter().any(|s| s.eq_ignore_ascii_case(value)) {
            seen.push(value.to_owned());
        }
    }
    seen.join("|")
}

/// Query parameters for `POST /Items/{itemId}/ContentType`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentTypeQuery {
    /// The content type to set; absent/blank clears the override.
    #[serde(default)]
    content_type: Option<String>,
}

/// `POST /Items/{itemId}/ContentType` — sets the folder content-type override.
///
/// Port of `ItemUpdateController.UpdateItemContentType`. A missing item is a
/// `404`; on success the server configuration's `ContentTypes` list is rewritten
/// (dropping any prior entry for this folder, adding the new one when non-blank)
/// and persisted, returning `204`.
#[utoipa::path(
    post,
    path = "/Items/{itemId}/ContentType",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Content type updated"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn update_item_content_type(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
    Query(query): Query<ContentTypeQuery>,
) -> Result<StatusCode, ApiError> {
    let item = state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let folder = containing_folder_path(item.path.as_deref());

    let mut configuration = (*state.config.configuration().await?).clone();
    configuration.content_types.retain(|pair| {
        pair.name
            .as_deref()
            .is_some_and(|name| !name.is_empty() && !name.eq_ignore_ascii_case(&folder))
    });
    if let Some(content_type) = query
        .content_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        configuration.content_types.push(NameValuePair {
            name: Some(folder),
            value: Some(content_type.to_owned()),
        });
    }
    state.config.update_configuration(&configuration).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The containing-folder path of a file path (its parent directory), or an empty
/// string when unknown. Mirrors C# `BaseItem.ContainingFolderPath`.
fn containing_folder_path(path: Option<&str>) -> String {
    let Some(path) = path else {
        return String::new();
    };
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(idx) => trimmed[..idx].to_owned(),
        None => String::new(),
    }
}

/// Query parameters for `POST /Items/{itemId}/Refresh`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshQuery {
    /// The metadata refresh mode (defaults to `None`).
    #[serde(default)]
    metadata_refresh_mode: Option<RefreshMode>,
    /// The image refresh mode (defaults to `None`).
    #[serde(default)]
    image_refresh_mode: Option<RefreshMode>,
    /// Whether to replace all metadata (only for a full refresh).
    #[serde(default)]
    replace_all_metadata: Option<bool>,
    /// Whether to replace all images (only for a full refresh).
    #[serde(default)]
    replace_all_images: Option<bool>,
    /// Whether to regenerate trickplay images (accepted; deferred subsystem).
    #[serde(default)]
    regenerate_trickplay: Option<bool>,
}

/// The wire spelling of the refresh-mode query enum. Mirrors the vendored
/// contract's `MetadataRefreshMode` (PascalCase), mapped onto the service-layer
/// [`MetadataRefreshMode`].
#[derive(Debug, Clone, Copy, serde::Deserialize)]
enum RefreshMode {
    /// Do not refresh.
    None,
    /// Validate only what is present.
    ValidationOnly,
    /// Fetch missing metadata only.
    Default,
    /// Fetch all metadata.
    FullRefresh,
}

impl From<RefreshMode> for MetadataRefreshMode {
    fn from(mode: RefreshMode) -> Self {
        match mode {
            RefreshMode::None => Self::None,
            RefreshMode::ValidationOnly => Self::ValidationOnly,
            RefreshMode::Default => Self::Default,
            RefreshMode::FullRefresh => Self::FullRefresh,
        }
    }
}

/// `POST /Items/{itemId}/Refresh` — queues a metadata/image refresh.
///
/// Port of `ItemRefreshController.RefreshItem`. A missing item is a `404`; on
/// success the refresh is queued at high priority and the handler returns `204`.
#[utoipa::path(
    post,
    path = "/Items/{itemId}/Refresh",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Refresh queued"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn refresh_item(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
    Query(query): Query<RefreshQuery>,
) -> Result<StatusCode, ApiError> {
    let item = state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;

    // Trickplay regeneration is a deferred subsystem; the flag is accepted for
    // contract parity but does not affect the queued refresh yet.
    let _ = query.regenerate_trickplay;

    // Refreshing a folder (a library's CollectionFolder, or any container) means
    // "scan its media" — the C# `ValidateChildren` path — so drive the filesystem
    // scan, scoped to the owning library. The folder is either a CollectionFolder
    // itself (jellyfin-web's per-library "Scan library" button; no TopParentId)
    // or nested inside one (series/season), whose TopParentId is that
    // CollectionFolder. A scope matching no library falls back to a full scan.
    if item.is_folder {
        let library_id = item
            .top_parent_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or(item_id);
        state.library.queue_library_scan_scoped(library_id).await?;
        return Ok(StatusCode::NO_CONTENT);
    }

    // Leaf-item metadata/image refresh goes to the provider queue: the enqueue
    // spawns a background TMDB refresh (movies/series by title; seasons/episodes
    // via their parent series) and this request 204s immediately, like the C#
    // queued refresh. Kinds with no provider (music) no-op faithfully.
    let metadata_refresh_mode = query
        .metadata_refresh_mode
        .map_or(MetadataRefreshMode::None, MetadataRefreshMode::from);
    let image_refresh_mode = query
        .image_refresh_mode
        .map_or(MetadataRefreshMode::None, MetadataRefreshMode::from);
    let options = MetadataRefreshOptions {
        metadata_refresh_mode,
        image_refresh_mode,
        replace_all_metadata: query.replace_all_metadata.unwrap_or(false),
        replace_all_images: query.replace_all_images.unwrap_or(false),
    };
    state
        .providers
        .queue_refresh(item_id, &options, RefreshPriority::High)
        .await?;
    // Re-probe the file so a metadata refresh also corrects stale media info
    // (duration, codecs, HDR/Dolby-Vision fields added since the first scan).
    // This is the media-info half of the C# refresh; best-effort, so a probe
    // failure doesn't fail the request.
    if !matches!(metadata_refresh_mode, MetadataRefreshMode::None)
        && let Err(e) = state.media_sources.refresh_media_streams(item_id).await
    {
        tracing::warn!(%item_id, error = %e, "media re-probe during refresh failed");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Registers the item-refresh + content-type routes.
///
/// The bare `POST /Items/{itemId}` route is registered by
/// [`crate::handlers::items::register`] so it shares one `MethodRouter` with the
/// `GET`/`DELETE` handlers (axum rejects a duplicate method+path across two
/// `route` calls).
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Items/{itemId}/ContentType",
            post(update_item_content_type),
        )
        .route("/Items/{itemId}/Refresh", post(refresh_item))
        .route("/Items/{itemId}/MetadataEditor", get(get_metadata_editor))
}

/// `GET /Items/{itemId}/MetadataEditor` — the item's metadata-editor descriptor.
///
/// Port of `ItemUpdateController.GetMetadataEditorInfo`: resolves the item (`404`
/// when absent), then assembles the reference data — parental ratings, countries,
/// cultures (deduped by display name, name-ordered case-insensitively via the
/// shared [`crate::handlers::localization::distinct_ordered_cultures`]), the item's external-id
/// descriptors, and the per-item content-type options.
///
/// Port note — content type: C# refines `ContentType`/`ContentTypeOptions` from
/// the folder's inherited vs configured collection type
/// (`GetInheritedContentType`/`GetConfiguredContentType`), which need the
/// un-ported collection-folder tree. The portable seam always offers the full
/// per-item option set with an unset `ContentType`; the descriptor shape and the
/// reference lists are already the final ones.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/MetadataEditor",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Metadata editor returned", body = MetadataEditorInfo),
        (status = 404, description = "Item not found"),
    ),
    tag = "ferrofin"
)]
async fn get_metadata_editor(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
) -> Result<Json<MetadataEditorInfo>, ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }

    let external_id_infos = state.providers.get_external_id_infos(item_id).await?;

    // Dedupe cultures by display name (case-insensitively) and order by it, as in
    // C#'s `DistinctBy(...).OrderBy(c => c.DisplayName)`. Shared with
    // `GET /Localization/Cultures` so both lists come back in the same order.
    let cultures =
        crate::handlers::localization::distinct_ordered_cultures(state.localization.get_cultures());

    let info = MetadataEditorInfo {
        parental_rating_options: state.localization.get_parental_ratings(),
        countries: state.localization.get_countries(),
        cultures,
        external_id_infos,
        content_type: None,
        // Jellyfin's GetMetadataEditorInfo only populates ContentTypeOptions for a collection-folder
        // whose content type is configurable; a plain library item (e.g. a Movie) gets an empty list.
        // Ferrofin doesn't model the configurable-content-type folder tree here, so a plain item — the
        // common case this endpoint serves — matches Jellyfin's empty array.
        content_type_options: Vec::new(),
    };
    Ok(Json(info))
}

#[cfg(test)]
mod tests {
    use super::{UpdateItemRequest, containing_folder_path, join_distinct, non_empty};

    #[test]
    fn update_request_accepts_editor_string_numbers_and_empty_dates() {
        // The metadata editor sends number inputs as strings and cleared dates as
        // "" — strict serde would 422; these must parse (was the save bug).
        let json = r#"{
            "Id": "ignored", "Type": "Movie", "Name": "Inception",
            "ProductionYear": "2010", "CommunityRating": "8.5", "IndexNumber": "1",
            "PremiereDate": "2010-07-16T00:00:00.0000000Z", "EndDate": "",
            "Genres": ["Action"], "Studios": [{"Name": "WB"}], "LockData": false
        }"#;
        let req: UpdateItemRequest = serde_json::from_str(json).expect("lenient parse");
        assert_eq!(req.production_year, Some(2010));
        assert_eq!(req.community_rating, Some(8.5));
        assert_eq!(req.index_number, Some(1));
        assert!(req.premiere_date.is_some());
        assert!(
            req.end_date.is_none(),
            "empty date string → None, not an error"
        );
        assert_eq!(req.name.as_deref(), Some("Inception"));
    }

    #[test]
    fn update_request_accepts_native_number_types_too() {
        let req: UpdateItemRequest =
            serde_json::from_str(r#"{"ProductionYear": 1999, "CommunityRating": 7}"#)
                .expect("numbers parse");
        assert_eq!(req.production_year, Some(1999));
        assert_eq!(req.community_rating, Some(7.0));
    }

    #[test]
    fn join_distinct_dedups_case_insensitively() {
        let values = vec![
            "Action".to_owned(),
            "action".to_owned(),
            " Sci-Fi ".to_owned(),
            String::new(),
        ];
        assert_eq!(join_distinct(&values), "Action|Sci-Fi");
    }

    #[test]
    fn non_empty_trims_and_blanks_to_none() {
        assert_eq!(non_empty(Some("  x ")), Some("x".to_owned()));
        assert_eq!(non_empty(Some("   ")), None);
        assert_eq!(non_empty(None), None);
    }

    #[test]
    fn containing_folder_is_parent_dir() {
        assert_eq!(
            containing_folder_path(Some("/media/movies/Blade/Blade.mkv")),
            "/media/movies/Blade"
        );
        assert_eq!(containing_folder_path(Some("Blade.mkv")), "");
        assert_eq!(containing_folder_path(None), "");
    }
}
