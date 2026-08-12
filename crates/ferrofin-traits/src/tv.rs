//! TV-series manager trait — the "Next Up" episode queue.
//!
//! Port of `MediaBrowser.Controller.TV.ITVSeriesManager`.
//!
//! Port rules applied:
//! - `ITVSeriesManager` exposes only the two `GetNextUp` overloads; they
//!   collapse to a single [`TvSeriesManager::get_next_up`]. The overload that
//!   takes an explicit `BaseItem[] parentsFolders` is folded into the query's
//!   optional `parent_id`.
//! - The `NextUpQuery` C# parameter (a `MediaBrowser.Model.Querying` type whose
//!   `required User User` and identity fields are service-internal) is ported as
//!   the local [`NextUpQuery`] param struct, with `User`/`Guid` fields becoming
//!   [`uuid::Uuid`] and `DateTime` becoming [`chrono::DateTime<Utc>`](chrono::DateTime).
//! - The result is DTO-shaped, so it reuses `QueryResult<BaseItemDto>` from
//!   `ferrofin-model` rather than the C# `QueryResult<BaseItem>` domain form.
//! - `DtoOptions` is reused from [`crate::options`].
//! - Synchronous C# methods become `async fn -> Result` (the impl paginates the
//!   database).
//!
//! The trait is object-safe and carries a `_assert_object_safe_*` assertion.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::entities::ImageType;
use ferrofin_model::querying::QueryResult;
use uuid::Uuid;

use crate::error::ServiceError;
use crate::options::DtoOptions;

/// The parameters of a "Next Up" query.
///
/// Port of `MediaBrowser.Model.Querying.NextUpQuery`. The `required User User`
/// becomes `user_id`; the nullable `Guid` fields become `Option<Uuid>`; the
/// `NextUpDateCutoff` `DateTime` becomes an [`Option`]al UTC timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NextUpQuery {
    /// The user the queue is computed for.
    pub user_id: Uuid,
    /// Restrict to items under this parent, if set.
    pub parent_id: Option<Uuid>,
    /// Restrict to a single series, if set.
    pub series_id: Option<Uuid>,
    /// The zero-based index of the first item to return.
    pub start_index: Option<i32>,
    /// The maximum number of items to return.
    pub limit: Option<i32>,
    /// The image types to populate on returned items.
    pub enable_image_types: Vec<ImageType>,
    /// Whether to compute the total record count.
    pub enable_total_record_count: bool,
    /// Only consider episodes aired on or after this cutoff, if set.
    pub next_up_date_cutoff: Option<DateTime<Utc>>,
    /// Whether to include resumable (partially-watched) episodes.
    pub enable_resumable: bool,
    /// Whether to include already-watched episodes (rewatching).
    pub enable_rewatching: bool,
}

/// Computes the "Next Up" episode queue for a user's TV series.
///
/// Port of `ITVSeriesManager`.
#[async_trait]
pub trait TvSeriesManager: Send + Sync {
    /// Gets the next-up episodes matching the query.
    async fn get_next_up(
        &self,
        query: &NextUpQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError>;
}

fn _assert_object_safe_tv_series_manager(_: &dyn TvSeriesManager) {}

#[cfg(test)]
mod tests {
    use super::NextUpQuery;

    #[test]
    fn next_up_query_default_is_empty() {
        let q = NextUpQuery::default();
        assert!(q.parent_id.is_none());
        assert!(q.series_id.is_none());
        assert!(q.limit.is_none());
        assert!(q.enable_image_types.is_empty());
        assert!(q.next_up_date_cutoff.is_none());
        assert!(!q.enable_rewatching);
    }
}
