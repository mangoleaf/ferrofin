//! [`HermitActivityManager`] — the concrete [`ActivityManager`] over `hermit-db`.
//!
//! Port of the query surface of
//! `Jellyfin.Server.Implementations.Activity.ActivityManager.GetPagedResultAsync`.
//! Entries live in the `ActivityLogs` table; the C# left-joins `Users` to expose
//! a `Username` filter, applies a set of `LIKE`/range/equality predicates, orders
//! by the requested keys, and pages the result.
//!
//! Port rules applied:
//! - EF's `LeftJoin(Users)` on `Username` becomes a SQL `LEFT JOIN "Users"` so
//!   the `username` `LIKE` predicate can filter on the joined column.
//! - `EF.Functions.Like(x, "%v%")` becomes a `LIKE '%' || ? || '%'` binding,
//!   which is case-insensitive for ASCII in SQLite (matching EF's default
//!   collation on the reference schema).
//! - `HasUserId` compares the stored `UserId` against the empty `Guid`
//!   (`00000000-0000-0000-0000-000000000000`): "has a user" means it differs.
//! - The default page size is 100 and the default ordering is `DateCreated`
//!   descending (the C# `ApplyOrdering` default), so a bare query returns the
//!   most recent entries first.
//! - `LogSeverity` is stored as the `Microsoft.Extensions.Logging.LogLevel`
//!   integer discriminant (`Trace`=0 … `None`=6); [`severity_to_int`] /
//!   [`int_to_severity`] map it to the [`LogLevel`] model.

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::activity::ActivityLogEntity;
use hermit_model::activity::{ActivityLogEntry, LogLevel};
use hermit_model::querying::QueryResult;
use hermit_traits::activity::{ActivityLogQuery, ActivityLogSortBy, ActivityManager, SortOrder};
use hermit_traits::error::ServiceError;
use uuid::Uuid;

use crate::db_error::db_err;

/// The empty `Guid` string a user-less activity entry stores in `UserId`.
const EMPTY_GUID: &str = "00000000-0000-0000-0000-000000000000";

/// The default page size when a query sets no `limit` (C# `query.Limit ?? 100`).
const DEFAULT_LIMIT: i32 = 100;

/// The concrete activity-log manager over the `ActivityLogs` table.
#[derive(Clone)]
pub struct HermitActivityManager {
    db: Database,
}

impl std::fmt::Debug for HermitActivityManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitActivityManager")
            .finish_non_exhaustive()
    }
}

impl HermitActivityManager {
    /// Creates an activity-log manager over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

/// Maps a stored `LogSeverity` discriminant to the model [`LogLevel`].
///
/// Mirrors `Microsoft.Extensions.Logging.LogLevel`; unknown values fall back to
/// `Information` (the neutral default).
#[must_use]
pub fn int_to_severity(value: i32) -> LogLevel {
    match value {
        0 => LogLevel::Trace,
        1 => LogLevel::Debug,
        3 => LogLevel::Warning,
        4 => LogLevel::Error,
        5 => LogLevel::Critical,
        6 => LogLevel::None,
        _ => LogLevel::Information,
    }
}

/// Maps a model [`LogLevel`] to its stored `LogSeverity` discriminant.
#[must_use]
pub fn severity_to_int(level: LogLevel) -> i32 {
    match level {
        LogLevel::Trace => 0,
        LogLevel::Debug => 1,
        LogLevel::Information => 2,
        LogLevel::Warning => 3,
        LogLevel::Error => 4,
        LogLevel::Critical => 5,
        LogLevel::None => 6,
    }
}

/// Projects a stored [`ActivityLogEntity`] into an [`ActivityLogEntry`] DTO.
///
/// Mirrors the C# `Select` projection: name/type/user/overviews/date/severity
/// are copied; the deprecated `UserPrimaryImageTag` stays `None`.
fn to_entry(row: ActivityLogEntity) -> ActivityLogEntry {
    #[allow(deprecated)]
    ActivityLogEntry {
        id: row.id,
        name: row.name,
        overview: row.overview,
        short_overview: row.short_overview,
        type_: row.type_,
        item_id: row.item_id,
        date: row.date_created,
        user_id: Uuid::parse_str(&row.user_id).unwrap_or_default(),
        user_primary_image_tag: None,
        severity: int_to_severity(row.log_severity),
    }
}

/// Appends the `ORDER BY` clause for the query's sort keys, defaulting to
/// `DateCreated DESC` (the C# `ApplyOrdering` fallback) when none are given.
fn order_by_clause(order_by: &[(ActivityLogSortBy, SortOrder)]) -> String {
    if order_by.is_empty() {
        return r#" ORDER BY a."DateCreated" DESC"#.to_owned();
    }
    let mut clauses = Vec::with_capacity(order_by.len());
    for (key, dir) in order_by {
        let column = match key {
            ActivityLogSortBy::DateCreated => r#"a."DateCreated""#,
            ActivityLogSortBy::LogLevel => r#"a."LogSeverity""#,
            ActivityLogSortBy::Id => r#"a."Id""#,
        };
        let direction = match dir {
            SortOrder::Ascending => "ASC",
            SortOrder::Descending => "DESC",
        };
        clauses.push(format!("{column} {direction}"));
    }
    format!(" ORDER BY {}", clauses.join(", "))
}

#[async_trait]
impl ActivityManager for HermitActivityManager {
    async fn get_paged_result(
        &self,
        query: &ActivityLogQuery,
    ) -> Result<QueryResult<ActivityLogEntry>, ServiceError> {
        // Build the shared WHERE clause (and bindings) once, then reuse it for
        // both the COUNT and the paged SELECT. Placeholders are positional (`?`).
        let mut wheres: Vec<String> = Vec::new();
        // A boxed list of string bindings, applied in order at query time.
        let mut binds: Vec<String> = Vec::new();

        if let Some(has_user_id) = query.has_user_id {
            if has_user_id {
                wheres.push(r#"a."UserId" <> ?"#.to_owned());
            } else {
                wheres.push(r#"a."UserId" = ?"#.to_owned());
            }
            binds.push(EMPTY_GUID.to_owned());
        }
        if let Some(min_date) = query.min_date {
            wheres.push(r#"a."DateCreated" >= ?"#.to_owned());
            binds.push(min_date.to_rfc3339());
        }
        if let Some(max_date) = query.max_date {
            wheres.push(r#"a."DateCreated" <= ?"#.to_owned());
            binds.push(max_date.to_rfc3339());
        }
        for (col, value) in [
            (r#"a."Name""#, &query.name),
            (r#"a."Overview""#, &query.overview),
            (r#"a."ShortOverview""#, &query.short_overview),
            (r#"a."Type""#, &query.type_),
            (r#"u."Username""#, &query.username),
        ] {
            if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
                wheres.push(format!("{col} LIKE '%' || ? || '%'"));
                binds.push(v.to_owned());
            }
        }
        if let Some(item_id) = query.item_id {
            // C# formats the item id with "N" (hyphen-free) to match storage.
            wheres.push(r#"a."ItemId" = ?"#.to_owned());
            binds.push(item_id.simple().to_string());
        }
        if let Some(severity) = query.severity {
            wheres.push(r#"a."LogSeverity" = ?"#.to_owned());
            binds.push(severity_to_int(severity).to_string());
        }

        let where_sql = if wheres.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", wheres.join(" AND "))
        };

        let count_sql = format!(
            r#"SELECT COUNT(*) FROM "ActivityLogs" a LEFT JOIN "Users" u ON a."UserId" = u."Id"{where_sql}"#
        );
        let mut count_q = sqlx::query_scalar::<_, i64>(&count_sql);
        for b in &binds {
            count_q = count_q.bind(b);
        }
        let total: i64 = count_q.fetch_one(self.db.pool()).await.map_err(db_err)?;

        let skip = query.start_index.unwrap_or(0).max(0);
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).max(0);
        let select_sql = format!(
            r#"SELECT a.* FROM "ActivityLogs" a LEFT JOIN "Users" u ON a."UserId" = u."Id"{where_sql}{order}
               LIMIT ? OFFSET ?"#,
            order = order_by_clause(&query.order_by)
        );
        let mut select_q = sqlx::query_as::<_, ActivityLogEntity>(&select_sql);
        for b in &binds {
            select_q = select_q.bind(b);
        }
        select_q = select_q.bind(i64::from(limit)).bind(i64::from(skip));
        let rows: Vec<ActivityLogEntity> =
            select_q.fetch_all(self.db.pool()).await.map_err(db_err)?;

        let items: Vec<ActivityLogEntry> = rows.into_iter().map(to_entry).collect();
        Ok(QueryResult::new(
            Some(skip),
            Some(i32::try_from(total).unwrap_or(i32::MAX)),
            items,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use hermit_db::Database;

    /// Inserts a raw activity-log row for the tests.
    async fn insert_entry(
        db: &Database,
        name: &str,
        type_: &str,
        user_id: &str,
        severity: i32,
        date: &str,
        item_id: Option<&str>,
    ) {
        sqlx::query(
            r#"INSERT INTO "ActivityLogs"
               ("DateCreated", "ItemId", "LogSeverity", "Name", "Overview",
                "RowVersion", "ShortOverview", "Type", "UserId")
               VALUES (?1, ?2, ?3, ?4, NULL, 0, NULL, ?5, ?6)"#,
        )
        .bind(date)
        .bind(item_id)
        .bind(severity)
        .bind(name)
        .bind(type_)
        .bind(user_id)
        .execute(db.pool())
        .await
        .expect("insert entry");
    }

    async fn memory_db() -> Database {
        let db = Database::connect_in_memory()
            .await
            .expect("open in-memory db");
        db.run_migrations().await.expect("run migrations");
        db
    }

    #[tokio::test]
    async fn returns_entries_ordered_newest_first() {
        let db = memory_db().await;
        insert_entry(
            &db,
            "old",
            "T1",
            "11111111-1111-1111-1111-111111111111",
            2,
            "2024-01-01T00:00:00+00:00",
            None,
        )
        .await;
        insert_entry(
            &db,
            "new",
            "T2",
            EMPTY_GUID,
            4,
            "2025-01-01T00:00:00+00:00",
            None,
        )
        .await;

        let mgr = HermitActivityManager::new(db);
        let result = mgr
            .get_paged_result(&ActivityLogQuery::default())
            .await
            .expect("query");
        assert_eq!(result.total_record_count, 2);
        assert_eq!(result.items[0].name, "new");
        assert_eq!(result.items[0].severity, LogLevel::Error);
        assert_eq!(result.items[1].name, "old");
    }

    #[tokio::test]
    async fn filters_by_has_user_id_and_min_date() {
        let db = memory_db().await;
        insert_entry(
            &db,
            "with-user",
            "T",
            "22222222-2222-2222-2222-222222222222",
            2,
            "2025-06-01T00:00:00+00:00",
            None,
        )
        .await;
        insert_entry(
            &db,
            "no-user",
            "T",
            EMPTY_GUID,
            2,
            "2025-06-02T00:00:00+00:00",
            None,
        )
        .await;

        let mgr = HermitActivityManager::new(db);

        let with_user = mgr
            .get_paged_result(&ActivityLogQuery {
                has_user_id: Some(true),
                ..Default::default()
            })
            .await
            .expect("query");
        assert_eq!(with_user.total_record_count, 1);
        assert_eq!(with_user.items[0].name, "with-user");

        let after = mgr
            .get_paged_result(&ActivityLogQuery {
                min_date: Some(Utc.with_ymd_and_hms(2025, 6, 2, 0, 0, 0).unwrap()),
                ..Default::default()
            })
            .await
            .expect("query");
        assert_eq!(after.total_record_count, 1);
        assert_eq!(after.items[0].name, "no-user");
    }

    #[tokio::test]
    async fn name_like_and_paging() {
        let db = memory_db().await;
        for i in 0..5 {
            insert_entry(
                &db,
                &format!("scan-{i}"),
                "LibraryScan",
                EMPTY_GUID,
                2,
                &format!("2025-01-0{}T00:00:00+00:00", i + 1),
                None,
            )
            .await;
        }
        insert_entry(
            &db,
            "unrelated",
            "Other",
            EMPTY_GUID,
            2,
            "2025-02-01T00:00:00+00:00",
            None,
        )
        .await;

        let mgr = HermitActivityManager::new(db);
        let page = mgr
            .get_paged_result(&ActivityLogQuery {
                name: Some("scan".to_owned()),
                start_index: Some(1),
                limit: Some(2),
                ..Default::default()
            })
            .await
            .expect("query");
        // Total counts all matches; the page holds only the requested slice.
        assert_eq!(page.total_record_count, 5);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.start_index, 1);
    }
}
