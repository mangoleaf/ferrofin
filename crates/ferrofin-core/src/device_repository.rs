//! [`DeviceRepository`] — the raw SQL behind the device manager.
//!
//! The device listing is a fan-out: one row per registered device, each needing
//! its `DeviceOptions` row and its owning user's name. Asking per device is an
//! N+1 (`GET /Devices` ran up to five statements per device), so the lookups
//! live here as **batched** `WHERE … IN (…)` reads keyed by the same values the
//! per-row queries bound. `Guid`/`DeviceId` columns carry SQLite's BINARY
//! collation (the schema declares no `COLLATE`), so keying the returned maps by
//! the exact stored string reproduces the per-row `= ?1` match byte for byte.
//!
//! No trait: a single in-crate impl with no dependency-injection seam, used
//! only by the device manager.

use std::collections::HashMap;

use ferrofin_db::Database;
use ferrofin_db::entities::security::{DeviceEntity, DeviceOptionsEntity};
use sqlx::{QueryBuilder, Sqlite};

use ferrofin_traits::error::ServiceError;

use crate::db_error::db_err;

/// Raw-SQL data access for devices, their options, and their owners' names.
#[derive(Clone)]
pub(crate) struct DeviceRepository {
    db: Database,
}

impl DeviceRepository {
    /// Creates the repository over the database handle.
    pub(crate) fn new(db: Database) -> Self {
        Self { db }
    }

    /// Every device row, most recently active first (C# `GetDevices` ordering),
    /// with `DeviceId` as the tiebreak so equal activity stamps stay stable.
    pub(crate) async fn all_by_last_activity(&self) -> Result<Vec<DeviceEntity>, ServiceError> {
        sqlx::query_as::<_, DeviceEntity>(
            r#"SELECT * FROM "Devices" ORDER BY "DateLastActivity" DESC, "DeviceId""#,
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)
    }

    /// The device rows of `user_id` (the stored, uppercase `Guid` form) and
    /// `device_id`, each filter skipped when `None`, in `Id` order (C#
    /// `GetDevices`). Both columns are BINARY, so `=` is the exact match C#
    /// makes on the `Guid` and the ordinal device id, and the login's lookup
    /// seeks `IX_Devices_UserId_DeviceId` instead of reading every device.
    pub(crate) async fn matching(
        &self,
        user_id: Option<&str>,
        device_id: Option<&str>,
    ) -> Result<Vec<DeviceEntity>, ServiceError> {
        matching_statement(user_id, device_id)
            .build_query_as::<DeviceEntity>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
    }

    /// The `DeviceOptions` rows for `device_ids`, keyed by `DeviceId`.
    ///
    /// Batched replacement for a per-device
    /// `SELECT * FROM "DeviceOptions" WHERE "DeviceId" = ?1 LIMIT 1`: an id
    /// with no row is simply absent from the map, exactly as that query
    /// returned `None`.
    pub(crate) async fn options_for(
        &self,
        device_ids: &[&str],
    ) -> Result<HashMap<String, DeviceOptionsEntity>, ServiceError> {
        let mut out: HashMap<String, DeviceOptionsEntity> = HashMap::new();
        for chunk in ferrofin_db::padded_in_chunks(device_ids) {
            let mut sql = String::from(r#"SELECT * FROM "DeviceOptions" WHERE "DeviceId" IN ("#);
            push_placeholders(&mut sql, chunk.len());
            sql.push(')');
            let mut query = sqlx::query_as::<_, DeviceOptionsEntity>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(*id);
            }
            for row in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                out.entry(row.device_id.clone()).or_insert(row);
            }
        }
        Ok(out)
    }

    /// The `Username` of each user in `user_ids`, keyed by the stored `Id`.
    ///
    /// Batched replacement for a per-device
    /// `SELECT "Username" FROM "Users" WHERE "Id" = ?1 LIMIT 1`. A missing id
    /// is absent from the map, so the caller can still raise the
    /// "User with UserId … not found" error the per-device read raised.
    pub(crate) async fn usernames_for(
        &self,
        user_ids: &[&str],
    ) -> Result<HashMap<String, String>, ServiceError> {
        let mut out: HashMap<String, String> = HashMap::new();
        for chunk in ferrofin_db::padded_in_chunks(user_ids) {
            let mut sql = String::from(r#"SELECT "Id", "Username" FROM "Users" WHERE "Id" IN ("#);
            push_placeholders(&mut sql, chunk.len());
            sql.push(')');
            let mut query = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(*id);
            }
            for (id, name) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                out.entry(id).or_insert(name);
            }
        }
        Ok(out)
    }
}

/// The statement behind [`DeviceRepository::matching`].
fn matching_statement(user_id: Option<&str>, device_id: Option<&str>) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::new(r#"SELECT * FROM "Devices" WHERE 1"#);
    if let Some(user_id) = user_id {
        query
            .push(r#" AND "UserId" = "#)
            .push_bind(user_id.to_owned());
    }
    if let Some(device_id) = device_id {
        query
            .push(r#" AND "DeviceId" = "#)
            .push_bind(device_id.to_owned());
    }
    query.push(r#" ORDER BY "Id""#);
    query
}

/// Appends a `?, ?, …` placeholder list of length `n` to `sql`.
///
/// Anonymous `?` throughout: SQLite forbids mixing numbered and anonymous
/// placeholders in one statement, and the bind order here is positional.
fn push_placeholders(sql: &mut String, n: usize) {
    for i in 0..n {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latest_tv_repository::explain;
    use crate::test_support::test_db;

    /// Each filter keeps exactly the rows C# `GetDevices` keeps, in `Id` order:
    /// two users sharing a device id are told apart, and no filter lists all.
    #[tokio::test]
    async fn matching_keeps_the_filtered_rows_in_id_order() {
        use ferrofin_traits::devices::DeviceManager as _;
        let db = test_db().await;
        let (a, b) = (uuid::Uuid::from_u128(0xa1), uuid::Uuid::from_u128(0xb1));
        crate::test_support::seed_user(&db, a).await;
        crate::test_support::seed_named_user(&db, b, "b").await;
        let (a, b) = (
            ferrofin_db::store::guid_to_db(a),
            ferrofin_db::store::guid_to_db(b),
        );
        let devices = crate::device_manager::FerrofinDeviceManager::new(db.clone());
        for (token, user, device_id) in [
            ("t1", &a, "shared"),
            ("t2", &b, "shared"),
            ("t3", &a, "own"),
        ] {
            let now = chrono::Utc::now();
            devices
                .create_device(&DeviceEntity {
                    id: 0,
                    access_token: token.into(),
                    app_name: "app".into(),
                    app_version: "1".into(),
                    date_created: now,
                    date_last_activity: now,
                    date_modified: now,
                    device_id: device_id.into(),
                    device_name: "name".into(),
                    is_active: true,
                    user_id: user.clone(),
                })
                .await
                .expect("create");
        }
        let repo = DeviceRepository::new(db);
        for (user_id, device_id, wanted) in [
            (None, None, vec!["t1", "t2", "t3"]),
            (Some(a.as_str()), None, vec!["t1", "t3"]),
            (None, Some("shared"), vec!["t1", "t2"]),
            (Some(b.as_str()), Some("shared"), vec!["t2"]),
            (Some(b.as_str()), Some("own"), vec![]),
        ] {
            let rows = repo.matching(user_id, device_id).await.expect("query");
            let tokens: Vec<&str> = rows.iter().map(|d| d.access_token.as_str()).collect();
            assert_eq!(tokens, wanted, "{user_id:?}/{device_id:?}");
        }
    }

    /// Every filtered device lookup seeks an index; none reads the whole
    /// table (the login, token revocation and the policy re-check run them).
    #[tokio::test]
    async fn filtered_device_lookups_seek_an_index() {
        let db = test_db().await;
        for (user_id, device_id, wanted) in [
            (
                Some("U"),
                Some("D"),
                "IX_Devices_UserId_DeviceId (UserId=? AND DeviceId=?)",
            ),
            (Some("U"), None, "IX_Devices_UserId_DeviceId (UserId=?)"),
            (None, Some("D"), "(DeviceId=?)"),
        ] {
            let plan = explain(&db, matching_statement(user_id, device_id)).await;
            assert!(
                plan.starts_with("SEARCH Devices USING INDEX") && plan.contains(wanted),
                "{user_id:?}/{device_id:?}: {plan}"
            );
        }
    }
}
