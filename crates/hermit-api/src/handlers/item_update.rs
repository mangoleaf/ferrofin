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
//! defers the cascades (logged in `brain/DEFERRED.md`). Every scalar/collection
//! field on the row is updated faithfully.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::routing::post;
use axum::{Json, Router};
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_model::dto::{BaseItemDto, MetadataEditorInfo, NameValuePair};
use hermit_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions, RefreshPriority};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// The content-type options offered by the metadata editor for a single item.
///
/// Port of `ItemUpdateController.GetContentTypeOptions(isForItem: true)`: the
/// `Inherit` (empty value) option plus the per-item collection types. The
/// `!isForItem` extras (`Books`, `MixedContent`) are omitted, matching the C#
/// call the editor makes with `isForItem = true`. Names are the English labels
/// (the C# `GetLocalizedString` pass is identity for the default culture).
fn item_content_type_options() -> Vec<NameValuePair> {
    [
        ("Inherit", ""),
        ("Movies", "movies"),
        ("Music", "music"),
        ("Shows", "tvshows"),
        ("HomeVideos", "homevideos"),
        ("MusicVideos", "musicvideos"),
        ("Photos", "photos"),
    ]
    .into_iter()
    .map(|(name, value)| NameValuePair::new(name, value))
    .collect()
}

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
    tag = "hermit"
)]
pub(crate) async fn update_item(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Json(request): Json<Box<BaseItemDto>>,
) -> Result<StatusCode, ApiError> {
    let mut item = state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    apply_update(&mut item, &request);
    state.library.update_items(&[item], None).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Applies the editable fields of `request` onto `item`. Mirrors the scalar and
/// collection assignments of C# `ItemUpdateController.UpdateItem`; the
/// series/season/album child cascades are deferred (see the module docs).
fn apply_update(item: &mut BaseItemEntity, request: &BaseItemDto) {
    item.name.clone_from(&request.name);
    item.forced_sort_name.clone_from(&request.forced_sort_name);
    item.original_title = non_empty(request.original_title.as_deref());
    item.original_language = non_empty(request.original_language.as_deref());
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
/// `hermit-db` stores the `Genres`/`Studios`/`Artists`/`Tags` columns and C#'s
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
    tag = "hermit"
)]
async fn update_item_content_type(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<ContentTypeQuery>,
) -> Result<StatusCode, ApiError> {
    let item = state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let folder = containing_folder_path(item.path.as_deref());

    let mut configuration = state.config.configuration().await?;
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
    tag = "hermit"
)]
async fn refresh_item(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<RefreshQuery>,
) -> Result<StatusCode, ApiError> {
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;

    // Trickplay regeneration is a deferred subsystem; the flag is accepted for
    // contract parity but does not affect the queued refresh yet.
    let _ = query.regenerate_trickplay;

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
/// cultures (deduped by display name, name-ordered), the item's external-id
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
    tag = "hermit"
)]
async fn get_metadata_editor(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<Json<MetadataEditorInfo>, ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }

    let external_id_infos = state.providers.get_external_id_infos(item_id).await?;

    // Dedupe cultures by display name (case-insensitively) and order by it, as in
    // C#'s `DistinctBy(...).OrderBy(c => c.DisplayName)`.
    let mut cultures = state.localization.get_cultures();
    cultures.sort_by(|a, b| {
        a.display_name
            .to_ascii_lowercase()
            .cmp(&b.display_name.to_ascii_lowercase())
    });
    cultures.dedup_by(|a, b| a.display_name.eq_ignore_ascii_case(&b.display_name));

    let info = MetadataEditorInfo {
        parental_rating_options: state.localization.get_parental_ratings(),
        countries: state.localization.get_countries(),
        cultures,
        external_id_infos,
        content_type: None,
        content_type_options: item_content_type_options(),
    };
    Ok(Json(info))
}

#[cfg(test)]
mod tests {
    use super::{containing_folder_path, join_distinct, non_empty};

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
