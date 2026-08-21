//! `ActivityLogController` — paged retrieval of server activity entries.
//!
//! Ports `GET /System/ActivityLog/Entries` (elevation-gated): binds the query
//! filters into an [`ActivityLogQuery`] and returns a
//! [`QueryResult<ActivityLogEntry>`], delegating to the
//! [`ActivityManager`](ferrofin_traits::activity::ActivityManager).
//!
//! The OpenAPI contract surfaces only `startIndex`/`limit`/`minDate`/`hasUserId`
//! for this route; the handler still accepts the full C# filter/sort set (the
//! richer manager query is honoured when a client sends them).

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use ferrofin_model::activity::{ActivityLogEntry, LogLevel};
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::activity::{ActivityLogQuery, ActivityLogSortBy, SortOrder};
use uuid::Uuid;

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::state::AppState;

/// Query parameters for `GET /System/ActivityLog/Entries`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetLogEntriesQuery {
    /// The record index to start at.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// The minimum entry date (inclusive).
    #[serde(default)]
    min_date: Option<DateTime<Utc>>,
    /// The maximum entry date (inclusive).
    #[serde(default)]
    max_date: Option<DateTime<Utc>>,
    /// Keep only entries that have (or lack) a user id.
    #[serde(default)]
    has_user_id: Option<bool>,
    /// Filter by name (substring).
    #[serde(default)]
    name: Option<String>,
    /// Filter by overview (substring).
    #[serde(default)]
    overview: Option<String>,
    /// Filter by short overview (substring).
    #[serde(default)]
    short_overview: Option<String>,
    /// Filter by type (substring).
    #[serde(default, rename = "type")]
    type_: Option<String>,
    /// Filter by item id.
    #[serde(default)]
    item_id: Option<Uuid>,
    /// Filter by username (substring).
    #[serde(default)]
    username: Option<String>,
    /// Filter by log severity.
    #[serde(default)]
    severity: Option<LogLevel>,
    /// Comma-delimited sort keys (`SortBy=Name,Type`).
    #[serde(default)]
    sort_by: Option<String>,
    /// Comma-delimited sort directions.
    #[serde(default)]
    sort_order: Option<String>,
}

/// Parses one activity-log sort key name (case-insensitive).
fn parse_sort_key(value: &str) -> Option<ActivityLogSortBy> {
    match value.trim().to_ascii_lowercase().as_str() {
        "datecreated" | "date" => Some(ActivityLogSortBy::DateCreated),
        "loglevel" | "severity" => Some(ActivityLogSortBy::LogLevel),
        "id" => Some(ActivityLogSortBy::Id),
        _ => None,
    }
}

/// Parses one sort-direction token (case-insensitive); defaults to ascending.
fn parse_sort_dir(value: Option<&str>) -> SortOrder {
    match value.map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if v == "descending" || v == "desc" => SortOrder::Descending,
        _ => SortOrder::Ascending,
    }
}

/// Zips the comma-delimited sort keys with their (positionally matched)
/// directions, mirroring `RequestHelpers.GetOrderBy`.
fn build_order_by(
    sort_by: Option<&str>,
    sort_order: Option<&str>,
) -> Vec<(ActivityLogSortBy, SortOrder)> {
    let keys: Vec<&str> = sort_by.map(|s| s.split(',').collect()).unwrap_or_default();
    let dirs: Vec<&str> = sort_order
        .map(|s| s.split(',').collect())
        .unwrap_or_default();
    keys.iter()
        .enumerate()
        .filter_map(|(i, k)| {
            parse_sort_key(k).map(|key| (key, parse_sort_dir(dirs.get(i).copied())))
        })
        .collect()
}

/// `GET /System/ActivityLog/Entries` — a page of activity-log entries.
///
/// Port of `ActivityLogController.GetLogEntries`.
#[utoipa::path(
    get,
    path = "/System/ActivityLog/Entries",
    params(
        ("startIndex" = Option<i32>, Query, description = "The record index to start at."),
        ("limit" = Option<i32>, Query, description = "The maximum number of records to return."),
        ("minDate" = Option<String>, Query, description = "The minimum date."),
        ("hasUserId" = Option<bool>, Query, description = "Filter log entries if it has a user id.")
    ),
    responses((status = 200, description = "Activity log returned", body = QueryResult<ActivityLogEntry>)),
    tag = "ferrofin"
)]
async fn get_log_entries(
    State(state): State<AppState>,
    _auth: RequireAdmin,
    Query(query): Query<GetLogEntriesQuery>,
) -> Result<Json<QueryResult<ActivityLogEntry>>, ApiError> {
    let order_by = build_order_by(query.sort_by.as_deref(), query.sort_order.as_deref());
    let manager_query = ActivityLogQuery {
        start_index: query.start_index,
        limit: query.limit,
        min_date: query.min_date,
        max_date: query.max_date,
        has_user_id: query.has_user_id,
        name: query.name,
        overview: query.overview,
        short_overview: query.short_overview,
        type_: query.type_,
        item_id: query.item_id,
        username: query.username,
        severity: query.severity,
        order_by,
    };
    let result = state.activity.get_paged_result(&manager_query).await?;
    Ok(Json(result))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/System/ActivityLog/Entries", get(get_log_entries))
}
