//! Activity-log trait — paged retrieval of server activity entries.
//!
//! Port of `MediaBrowser.Model.Activity.IActivityManager`, reduced to the
//! read/query surface the `ActivityLogController` exercises (`GetPagedResultAsync`).
//! The entry-creation and cleanup members are server-side write concerns not
//! part of this batch's wire routes and are omitted; the trait can grow them
//! later without affecting handlers.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hermit_model::activity::{ActivityLogEntry, LogLevel};
use hermit_model::querying::QueryResult;
use uuid::Uuid;

use crate::error::ServiceError;

/// The sort key for an activity-log query (port of
/// `Jellyfin.Data.Enums.ActivityLogSortBy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivityLogSortBy {
    /// Sort by the entry timestamp (`DateCreated`).
    DateCreated,
    /// Sort by log severity.
    LogLevel,
    /// Sort by the surrogate id.
    Id,
}

/// The sort direction for an activity-log query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SortOrder {
    /// Ascending order.
    #[default]
    Ascending,
    /// Descending order.
    Descending,
}

/// A query against the activity log (port of
/// `Jellyfin.Data.Queries.ActivityLogQuery`).
///
/// Every field is optional; an all-`None` query returns the first page of all
/// entries. `order_by` holds `(key, direction)` pairs applied in order.
#[derive(Debug, Clone, Default)]
pub struct ActivityLogQuery {
    /// The record index to start at (drops lower-indexed records).
    pub start_index: Option<i32>,

    /// The maximum number of records to return (defaults to 100 in the impl).
    pub limit: Option<i32>,

    /// Filters to entries created on or after this instant.
    pub min_date: Option<DateTime<Utc>>,

    /// Filters to entries created on or before this instant.
    pub max_date: Option<DateTime<Utc>>,

    /// When set, keeps only entries that have (`true`) or lack (`false`) a user.
    pub has_user_id: Option<bool>,

    /// Case-insensitive `LIKE` filter on the entry name.
    pub name: Option<String>,

    /// Case-insensitive `LIKE` filter on the overview.
    pub overview: Option<String>,

    /// Case-insensitive `LIKE` filter on the short overview.
    pub short_overview: Option<String>,

    /// Case-insensitive `LIKE` filter on the entry type.
    pub type_: Option<String>,

    /// Filters to entries tagged with this item id.
    pub item_id: Option<Uuid>,

    /// Case-insensitive `LIKE` filter on the joined username.
    pub username: Option<String>,

    /// Filters to entries at this log severity.
    pub severity: Option<LogLevel>,

    /// The ordered list of `(key, direction)` sort clauses.
    pub order_by: Vec<(ActivityLogSortBy, SortOrder)>,
}

/// Manages retrieval of activity-log entries.
///
/// Port of `IActivityManager` (query surface only).
#[async_trait]
pub trait ActivityManager: Send + Sync {
    /// Returns a page of activity-log entries matching the query
    /// (C# `GetPagedResultAsync`).
    async fn get_paged_result(
        &self,
        query: &ActivityLogQuery,
    ) -> Result<QueryResult<ActivityLogEntry>, ServiceError>;
}

fn _assert_object_safe_activity_manager(_: &dyn ActivityManager) {}
