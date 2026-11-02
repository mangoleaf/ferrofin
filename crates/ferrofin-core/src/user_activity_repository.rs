//! The `Users.LastActivityDate` stamp that session activity writes
//! (`SessionManager.LogSessionActivity`, `SessionManager.cs:262-275`).

use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::store::datetime_to_db;
use ferrofin_traits::error::ServiceError;

use crate::db_error::db_err;

/// Stamps the user `user_id` (its stored id) as active at `at`, unless its
/// stored `LastActivityDate` is already later, and bumps `RowVersion` as
/// upstream's `UpdateUserAsync` save does.
///
/// # Errors
///
/// A storage failure writing the row.
pub(crate) async fn stamp_last_activity(
    db: &Database,
    user_id: &str,
    at: DateTime<Utc>,
) -> Result<(), ServiceError> {
    sqlx::query(
        r#"UPDATE "Users" SET "LastActivityDate" = ?2, "RowVersion" = "RowVersion" + 1
           WHERE "Id" = ?1 AND ("LastActivityDate" IS NULL OR "LastActivityDate" < ?2)"#,
    )
    .bind(user_id)
    .bind(datetime_to_db(at))
    .execute(db.writer())
    .await
    .map_err(db_err)?;
    Ok(())
}
