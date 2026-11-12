//! Retention for `FerrofinPlaybackSessions`, the rows
//! [`crate::playback_metrics`] writes: the prune the Optimize Database task
//! runs, so per-user and per-device playback history expires with the activity
//! log instead of staying forever.

use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::store::datetime_to_db;
use ferrofin_traits::error::ServiceError;

use crate::db_error::db_err;

/// The prune statement; it seeks `FerrofinIX_PlaybackSessions_DecidedAt`.
/// Rows store `DecidedAt` in `datetime_to_db`'s text, which orders as time, so
/// the comparison is exact. A row written before that format (2026-08, RFC
/// 3339 with a `T`) sorts later on its own day, so it goes up to a day late.
const PRUNE: &str = r#"DELETE FROM "FerrofinPlaybackSessions" WHERE "DecidedAt" < ?1"#;

/// Deletes the rows decided before `before`; returns how many went.
pub(crate) async fn prune(db: &Database, before: DateTime<Utc>) -> Result<u64, ServiceError> {
    let result = sqlx::query(PRUNE)
        .bind(datetime_to_db(before))
        .execute(db.writer())
        .await
        .map_err(db_err)?;
    Ok(result.rows_affected())
}

/// Row helpers for the tests of the tasks that prune.
#[cfg(test)]
pub(crate) mod test_rows {
    use super::{Database, DateTime, Utc, datetime_to_db};

    /// Inserts a minimal row for `psid`, decided at `at`.
    pub(crate) async fn insert(db: &Database, psid: &str, at: DateTime<Utc>) {
        sqlx::query(
            r#"INSERT INTO "FerrofinPlaybackSessions"
               ("PlaySessionId", "ItemId", "UserId", "PlayMethod", "DecidedAt")
               VALUES (?1, 'item', 'user', 'DirectPlay', ?2)"#,
        )
        .bind(psid)
        .bind(datetime_to_db(at))
        .execute(db.writer())
        .await
        .expect("insert playback session");
    }

    /// Drops the table, so the next prune fails.
    pub(crate) async fn drop_table(db: &Database) {
        sqlx::query(r#"DROP TABLE "FerrofinPlaybackSessions""#)
            .execute(db.writer())
            .await
            .expect("drop playback sessions");
    }

    /// The `PlaySessionId`s left, sorted.
    pub(crate) async fn remaining(db: &Database) -> Vec<String> {
        sqlx::query_scalar(
            r#"SELECT "PlaySessionId" FROM "FerrofinPlaybackSessions" ORDER BY "PlaySessionId""#,
        )
        .fetch_all(db.pool())
        .await
        .expect("read playback sessions")
    }

    /// The planner-statistics tables (`sqlite_stat*`) in the database, sorted.
    /// Here, not in the optimize task's tests: `sql_boundary.rs` caps the SQL
    /// in `maintenance.rs` and exempts repository files.
    pub(crate) async fn stat_tables(db: &Database) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'sqlite_stat%' \
             ORDER BY name",
        )
        .fetch_all(db.pool())
        .await
        .expect("read sqlite_master")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;

    /// The prune seeks the `DecidedAt` index rather than scanning the table,
    /// so its write stays short however much history has built up.
    #[tokio::test]
    async fn prune_seeks_the_decided_at_index() {
        let db = test_db().await;
        let plan: Vec<String> = sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(
            format!("EXPLAIN QUERY PLAN {PRUNE}"),
        ))
        .bind("x")
        .fetch_all(db.pool())
        .await
        .expect("explain")
        .into_iter()
        .map(|(_, _, _, detail)| detail)
        .collect();
        assert!(
            plan.iter()
                .any(|d| d.starts_with("SEARCH")
                    && d.contains("FerrofinIX_PlaybackSessions_DecidedAt")),
            "{plan:?}"
        );
    }
}
