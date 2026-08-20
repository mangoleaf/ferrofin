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

use ferrofin_traits::error::ServiceError;

use crate::db_error::db_err;

/// How many ids one batched `IN (…)` carries. SQLite's default bind-variable
/// ceiling is 999; 500 keeps a wide margin and matches the chunk size the other
/// batched readers in this crate use.
const ID_CHUNK: usize = 500;

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
        for chunk in device_ids.chunks(ID_CHUNK) {
            let mut sql = String::from(r#"SELECT * FROM "DeviceOptions" WHERE "DeviceId" IN ("#);
            push_placeholders(&mut sql, chunk.len());
            sql.push(')');
            let mut query = sqlx::query_as::<_, DeviceOptionsEntity>(&sql);
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
        for chunk in user_ids.chunks(ID_CHUNK) {
            let mut sql = String::from(r#"SELECT "Id", "Username" FROM "Users" WHERE "Id" IN ("#);
            push_placeholders(&mut sql, chunk.len());
            sql.push(')');
            let mut query = sqlx::query_as::<_, (String, String)>(&sql);
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
