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

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use ferrofin_model::activity::{ActivityLogEntry, LogLevel};
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::activity::{ActivityLogQuery, ActivityLogSortBy, SortOrder};
use uuid::Uuid;

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::extract::Query;
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
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    item_id: Option<Uuid>,
    /// Filter by username (substring).
    #[serde(default)]
    username: Option<String>,
    /// Filter by log severity.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::nullable_enum"
    )]
    severity: Option<LogLevel>,
    /// Comma-delimited sort keys (`SortBy=Name,Type`).
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::present_string"
    )]
    sort_by: Option<String>,
    /// Comma-delimited sort directions.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::present_string"
    )]
    sort_order: Option<String>,
}

/// Binds one `[FromQuery] T[]` member the way `ArrayModelBinder` +
/// `EnumTypeModelBinder` do: each value (repeated keys arrive comma-joined) is
/// an enum name in any case or its C# value; an empty, unknown or undefined one
/// is a 400.
///
/// One divergence: upstream binds a single `sortBy=Name,Type` with
/// `Enum.Parse`'s bitwise OR (`Type`), contradicting its own documented
/// `SortBy=Name,Type` format; here it is the documented two keys.
fn parse_enum_array<T: serde::de::DeserializeOwned>(
    key: &str,
    raw: Option<&str>,
) -> Result<Vec<T>, ApiError> {
    raw.map_or_else(
        || Ok(Vec::new()),
        |raw| {
            raw.split(',')
                .map(|value| {
                    T::deserialize(crate::query::value::Value::nullable(value)).map_err(|_| {
                        ApiError::BadRequest(format!("{key}: The value '{value}' is not valid."))
                    })
                })
                .collect()
        },
    )
}

/// `RequestHelpers.GetOrderBy`: each key takes the order at its index, the
/// rest the FIRST requested order (ascending when none). Extra orders are
/// ignored (upstream indexes past its array and fails the request).
fn build_order_by(
    sort_by: Option<&str>,
    sort_order: Option<&str>,
) -> Result<Vec<(ActivityLogSortBy, SortOrder)>, ApiError> {
    let keys: Vec<ActivityLogSortBy> = parse_enum_array("sortBy", sort_by)?;
    let orders = parse_enum_array::<ferrofin_model::dto::SortOrder>("sortOrder", sort_order)?
        .into_iter()
        .map(|order| match order {
            ferrofin_model::dto::SortOrder::Ascending => Ok(SortOrder::Ascending),
            ferrofin_model::dto::SortOrder::Descending => Ok(SortOrder::Descending),
            // `EnumTypeModelBinder` refuses a value no member has.
            ferrofin_model::dto::SortOrder::Unrecognized(value) => Err(ApiError::BadRequest(
                format!("sortOrder: The value '{value}' is invalid."),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let fallback = orders.first().copied().unwrap_or(SortOrder::Ascending);
    Ok(keys
        .into_iter()
        .enumerate()
        .map(|(i, key)| (key, orders.get(i).copied().unwrap_or(fallback)))
        .collect())
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
    let order_by = build_order_by(query.sort_by.as_deref(), query.sort_order.as_deref())?;
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
crate::query::query_parameters! {
    GetLogEntriesQuery {
        "sortBy" => ',',
        "sortOrder" => ',',
    } => [("get", "/System/ActivityLog/Entries")];
}
