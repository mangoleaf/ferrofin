//! `MediaSegmentsController` — read an item's media segments.
//!
//! Ports the portable read route of Jellyfin's `MediaSegmentsController`:
//! - `GET /MediaSegments/{itemId}` — the intro/outro/recap/commercial/preview
//!   segments stored for an item, optionally filtered by type.
//!
//! The plugin-owned `SegmentEditor` routes (`/MediaSegmentsApi/*`, tagged
//! `SegmentEditor` in the contract) belong to a dynamic plugin host and stay on
//! the `501` stub.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use hermit_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use hermit_model::querying::QueryResult;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::query_parse::parse_csv_enums;
use crate::state::AppState;

/// Query parameters for `GET /MediaSegments/{itemId}`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemSegmentsQuery {
    /// Optional. Comma-delimited filter of requested segment types.
    #[serde(default)]
    include_segment_types: Option<String>,
}

/// `GET /MediaSegments/{itemId}` — the item's media segments.
///
/// Port of `MediaSegmentsController.GetItemSegments`: resolves the item (`404`
/// when absent), then returns its stored segments narrowed to the requested
/// [`MediaSegmentType`]s (all types when the filter is empty), wrapped in a
/// [`QueryResult`]. The C# per-library provider filtering is a documented
/// deferral in the manager (all stored segments are returned).
#[utoipa::path(
    get,
    path = "/MediaSegments/{itemId}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("includeSegmentTypes" = Option<String>, Query, description = "Optional filter of requested segment types (comma-delimited)")
    ),
    responses(
        (status = 200, description = "Segments returned", body = QueryResult<MediaSegmentDto>),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn get_item_segments(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<ItemSegmentsQuery>,
) -> Result<Json<QueryResult<MediaSegmentDto>>, ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }

    let types: Vec<MediaSegmentType> = parse_csv_enums(query.include_segment_types.as_deref())?;
    let type_filter = if types.is_empty() {
        None
    } else {
        Some(types.as_slice())
    };

    let segments = state
        .media_segments
        .get_segments(item_id, type_filter, false)
        .await?;
    let count = i32::try_from(segments.len()).unwrap_or(i32::MAX);
    Ok(Json(QueryResult::new(Some(0), Some(count), segments)))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/MediaSegments/{itemId}", get(get_item_segments))
}
