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
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use ferrofin_model::querying::QueryResult;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::query_parse::parse_csv_enums;
use crate::state::AppState;

/// Collects every `includeSegmentTypes` value from the raw query pairs.
///
/// ASP.NET's collection binder accepts the parameter **repeated**
/// (`?includeSegmentTypes=Intro&includeSegmentTypes=Outro` — what the
/// jellyfin SDK sends) as well as comma-delimited; a typed
/// `Query<Option<String>>` rejects the repeated form as a duplicate field
/// with `400`, which broke every playback's segment fetch (no skip button).
fn include_segment_types(pairs: &[(String, String)]) -> Option<String> {
    let joined = pairs
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("includeSegmentTypes"))
        .map(|(_, v)| v.as_str())
        .collect::<Vec<_>>()
        .join(",");
    (!joined.is_empty()).then_some(joined)
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
    tag = "ferrofin"
)]
async fn get_item_segments(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Json<QueryResult<MediaSegmentDto>>, ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }

    let raw_types = include_segment_types(&pairs);
    let types: Vec<MediaSegmentType> = parse_csv_enums(raw_types.as_deref())?;
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

/// Query parameters for `DELETE /MediaSegments/Provider/{providerId}`.
#[derive(Debug, Default, serde::Deserialize)]
struct ProviderEraseQuery {
    /// Optional single segment type to limit the erase to.
    #[serde(default, rename = "type")]
    type_: Option<String>,
}

/// `DELETE /MediaSegments/Provider/{providerId}` — erases every segment a provider
/// wrote, optionally limited to one type. Backs a provider's bulk "erase
/// timestamps" tool (e.g. Intro Skipper). Not a Jellyfin contract route; additive.
async fn erase_provider_segments(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(provider_id): Path<String>,
    Query(query): Query<ProviderEraseQuery>,
) -> Result<StatusCode, ApiError> {
    let type_filter = match query.type_.as_deref() {
        Some(raw) => Some(
            parse_csv_enums::<MediaSegmentType>(Some(raw))?
                .into_iter()
                .next()
                .ok_or_else(|| ApiError::BadRequest(format!("invalid segment type {raw:?}")))?,
        ),
        None => None,
    };
    state
        .media_segments
        .delete_all_provider_segments(&provider_id, type_filter)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/MediaSegments/{itemId}", get(get_item_segments))
        .route(
            "/MediaSegments/Provider/{providerId}",
            delete(erase_provider_segments),
        )
}
