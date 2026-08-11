//! The C# `User` OOP behavior as free functions over the `hermit-db` rows.
//!
//! Jellyfin's `User` entity carries navigation collections (`Permissions`,
//! `Preferences`, `AccessSchedules`) and a pile of extension methods
//! (`HasPermission`, `GetPreference`, `SetPermission`, `SetPreference`,
//! `AddDefaultPermissions`, `IsParentalScheduleAllowed`, …) in
//! `Jellyfin.Data.UserEntityExtensions`. The port keeps `UserEntity` a flat
//! one-to-one table mirror (Wave 3) and moves that behavior here, backed by the
//! sibling `Permissions` / `Preferences` / `AccessSchedules` tables — the same
//! "OOP hierarchy becomes free functions over the row" rule the item `kinds`
//! module applies.
//!
//! Every helper takes a [`sqlx`] executor so callers can run them inside the
//! caller's transaction. `Guid` identity is the hyphenated `UserEntity::id`
//! string, matching the stored `UserId` foreign keys.

use hermit_db::enums::{PermissionKind, PreferenceKind};
use sqlx::sqlite::SqlitePool;
use sqlx::{Sqlite, SqliteExecutor};

use crate::db_error::db_err;
use hermit_traits::error::ServiceError;

/// The delimiter list-valued preferences are stored with (C#
/// `UserEntityExtensions.Delimiter`). The values are `Guid`s or tags, neither of
/// which contains a comma.
const PREFERENCE_DELIMITER: char = ',';

/// The default permission seed applied to every newly created user (C#
/// `AddDefaultPermissions`). Ordered exactly as upstream so a migrated database
/// row-for-row matches Jellyfin.
const DEFAULT_PERMISSIONS: &[(PermissionKind, bool)] = &[
    (PermissionKind::IsAdministrator, false),
    (PermissionKind::IsDisabled, false),
    (PermissionKind::IsHidden, true),
    (PermissionKind::EnableAllChannels, true),
    (PermissionKind::EnableAllDevices, true),
    (PermissionKind::EnableAllFolders, true),
    (PermissionKind::EnableContentDeletion, false),
    (PermissionKind::EnableContentDownloading, true),
    (PermissionKind::EnableMediaConversion, true),
    (PermissionKind::EnableMediaPlayback, true),
    (PermissionKind::EnablePlaybackRemuxing, true),
    (PermissionKind::EnablePublicSharing, true),
    (PermissionKind::EnableRemoteAccess, true),
    (PermissionKind::EnableSyncTranscoding, true),
    (PermissionKind::EnableAudioPlaybackTranscoding, true),
    (PermissionKind::EnableLiveTvAccess, true),
    (PermissionKind::EnableLiveTvManagement, false),
    (PermissionKind::EnableSharedDeviceControl, true),
    (PermissionKind::EnableVideoPlaybackTranscoding, true),
    (PermissionKind::ForceRemoteSourceTranscoding, false),
    (PermissionKind::EnableRemoteControlOfOtherUsers, false),
    (PermissionKind::EnableCollectionManagement, false),
    (PermissionKind::EnableSubtitleManagement, false),
    (PermissionKind::EnableLyricManagement, false),
];

/// Every [`PreferenceKind`] variant, seeded empty on user creation (C#
/// `AddDefaultPreferences` iterates `Enum.GetValues<PreferenceKind>()`).
const ALL_PREFERENCE_KINDS: &[PreferenceKind] = &[
    PreferenceKind::BlockedTags,
    PreferenceKind::BlockedChannels,
    PreferenceKind::BlockedMediaFolders,
    PreferenceKind::EnabledDevices,
    PreferenceKind::EnabledChannels,
    PreferenceKind::EnabledFolders,
    PreferenceKind::EnableContentDeletionFromFolders,
    PreferenceKind::LatestItemExcludes,
    PreferenceKind::MyMediaExcludes,
    PreferenceKind::GroupedFolders,
    PreferenceKind::BlockUnratedItems,
    PreferenceKind::OrderedViews,
    PreferenceKind::AllowedTags,
];

/// Whether the user has the given permission (C# `HasPermission`).
///
/// A missing `Permissions` row means the permission is unset, which reads as
/// `false` — matching `Permissions.FirstOrDefault(...)?.Value ?? false`.
///
/// # Errors
/// Returns [`ServiceError::Db`] if the query fails.
pub async fn has_permission<'e, E>(
    executor: E,
    user_id: &str,
    kind: PermissionKind,
) -> Result<bool, ServiceError>
where
    E: SqliteExecutor<'e>,
{
    let value: Option<bool> = sqlx::query_scalar(
        r#"SELECT "Value" FROM "Permissions" WHERE "UserId" = ?1 AND "Kind" = ?2"#,
    )
    .bind(user_id)
    .bind(i32::from(kind))
    .fetch_optional(executor)
    .await
    .map_err(db_err)?;
    Ok(value.unwrap_or(false))
}

/// Sets a permission to `value`, inserting the row when absent (C#
/// `SetPermission`).
///
/// `Permissions` has no unique index on `(UserId, Kind)`, so this mirrors the
/// C# "find then update-or-insert" as an `UPDATE` followed by a conditional
/// `INSERT` (each on its own pooled connection — the pair is idempotent, so it
/// needs no surrounding transaction).
///
/// # Errors
/// Returns [`ServiceError::Db`] if either statement fails.
pub async fn set_permission(
    pool: &SqlitePool,
    user_id: &str,
    kind: PermissionKind,
    value: bool,
) -> Result<(), ServiceError> {
    sqlx::query(r#"UPDATE "Permissions" SET "Value" = ?3 WHERE "UserId" = ?1 AND "Kind" = ?2"#)
        .bind(user_id)
        .bind(i32::from(kind))
        .bind(value)
        .execute(pool)
        .await
        .map_err(db_err)?;

    sqlx::query(
        r#"INSERT INTO "Permissions" ("Kind", "RowVersion", "UserId", "Value")
           SELECT ?2, 1, ?1, ?3
           WHERE NOT EXISTS (
               SELECT 1 FROM "Permissions" WHERE "UserId" = ?1 AND "Kind" = ?2
           )"#,
    )
    .bind(user_id)
    .bind(i32::from(kind))
    .bind(value)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Reads a list-valued preference, split on the delimiter (C# `GetPreference`).
/// An absent or empty value yields an empty list.
///
/// # Errors
/// Returns [`ServiceError::Db`] if the query fails.
pub async fn get_preference<'e, E>(
    executor: E,
    user_id: &str,
    kind: PreferenceKind,
) -> Result<Vec<String>, ServiceError>
where
    E: SqliteExecutor<'e>,
{
    let value: Option<String> = sqlx::query_scalar(
        r#"SELECT "Value" FROM "Preferences" WHERE "UserId" = ?1 AND "Kind" = ?2"#,
    )
    .bind(user_id)
    .bind(i32::from(kind))
    .fetch_optional(executor)
    .await
    .map_err(db_err)?;
    Ok(match value {
        Some(v) if !v.is_empty() => v.split(PREFERENCE_DELIMITER).map(str::to_owned).collect(),
        _ => Vec::new(),
    })
}

/// Writes a list-valued preference, joining `values` with the delimiter (C#
/// `SetPreference`). Inserts the row when absent, updates it otherwise.
///
/// # Errors
/// Returns [`ServiceError::Db`] if either statement fails.
pub async fn set_preference(
    pool: &SqlitePool,
    user_id: &str,
    kind: PreferenceKind,
    values: &[String],
) -> Result<(), ServiceError> {
    let joined = values.join(&PREFERENCE_DELIMITER.to_string());

    sqlx::query(r#"UPDATE "Preferences" SET "Value" = ?3 WHERE "UserId" = ?1 AND "Kind" = ?2"#)
        .bind(user_id)
        .bind(i32::from(kind))
        .bind(&joined)
        .execute(pool)
        .await
        .map_err(db_err)?;

    sqlx::query(
        r#"INSERT INTO "Preferences" ("Kind", "RowVersion", "UserId", "Value")
           SELECT ?2, 1, ?1, ?3
           WHERE NOT EXISTS (
               SELECT 1 FROM "Preferences" WHERE "UserId" = ?1 AND "Kind" = ?2
           )"#,
    )
    .bind(user_id)
    .bind(i32::from(kind))
    .bind(&joined)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Whether a list-valued preference contains `needle`, case-insensitively (the
/// comparison C# `CanAccessDevice` uses for `EnabledDevices`).
///
/// # Errors
/// Returns [`ServiceError::Db`] if the query fails.
pub async fn preference_contains<'e, E>(
    executor: E,
    user_id: &str,
    kind: PreferenceKind,
    needle: &str,
) -> Result<bool, ServiceError>
where
    E: SqliteExecutor<'e>,
{
    let values = get_preference(executor, user_id, kind).await?;
    Ok(values.iter().any(|v| v.eq_ignore_ascii_case(needle)))
}

/// Whether the user is currently within an access schedule, or has none (C#
/// `IsParentalScheduleAllowed`).
///
/// The schedule stores fractional local hours per day-of-week; a user with no
/// schedules is always allowed. Day-of-week matching uses the
/// [`DynamicDayOfWeek`](hermit_model::users::DynamicDayOfWeek) grouping
/// (`Everyday` / `Weekday` / `Weekend`) exactly as upstream.
///
/// # Errors
/// Returns [`ServiceError::Db`] if the query fails.
pub async fn is_parental_schedule_allowed<'e, E>(
    executor: E,
    user_id: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Result<bool, ServiceError>
where
    E: SqliteExecutor<'e>,
{
    use chrono::{Datelike as _, Timelike as _};

    let schedules: Vec<(i32, f64, f64)> = sqlx::query_as(
        r#"SELECT "DayOfWeek", "StartHour", "EndHour" FROM "AccessSchedules" WHERE "UserId" = ?1"#,
    )
    .bind(user_id)
    .fetch_all(executor)
    .await
    .map_err(db_err)?;

    if schedules.is_empty() {
        return Ok(true);
    }

    let hour =
        f64::from(now.hour()) + f64::from(now.minute()) / 60.0 + f64::from(now.second()) / 3600.0;
    let weekday = now.date_naive().weekday();

    Ok(schedules
        .into_iter()
        .any(|(day, start, end)| day_matches(day, weekday) && hour >= start && hour <= end))
}

/// Whether a stored [`DynamicDayOfWeek`](hermit_model::users::DynamicDayOfWeek)
/// discriminant covers `weekday` (C# `DynamicDayOfWeek.Contains`).
///
/// Discriminants `0..=6` are `Sunday..Saturday`, `7` is `Everyday`, `8` is
/// `Weekday` (Mon–Fri), and `9` is `Weekend` (Sat/Sun).
fn day_matches(day: i32, weekday: chrono::Weekday) -> bool {
    use chrono::Weekday;
    match day {
        7 => true,
        8 => !matches!(weekday, Weekday::Sat | Weekday::Sun),
        9 => matches!(weekday, Weekday::Sat | Weekday::Sun),
        // 0 = Sunday … 6 = Saturday (C# System.DayOfWeek order).
        d @ 0..=6 => {
            let target = [
                Weekday::Sun,
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
                Weekday::Sat,
            ][usize::try_from(d).unwrap_or(0)];
            weekday == target
        }
        _ => false,
    }
}

/// Seeds the default permissions and (empty) preferences for a freshly created
/// user (C# `AddDefaultPermissions` + `AddDefaultPreferences`).
///
/// Runs inside the caller's transaction so a half-seeded user is never
/// committed.
///
/// # Errors
/// Returns [`ServiceError::Db`] if any insert fails.
pub async fn seed_defaults(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    user_id: &str,
) -> Result<(), ServiceError> {
    for (kind, value) in DEFAULT_PERMISSIONS {
        sqlx::query(
            r#"INSERT INTO "Permissions" ("Kind", "RowVersion", "UserId", "Value")
               VALUES (?1, 1, ?2, ?3)"#,
        )
        .bind(i32::from(*kind))
        .bind(user_id)
        .bind(*value)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
    }

    for kind in ALL_PREFERENCE_KINDS {
        sqlx::query(
            r#"INSERT INTO "Preferences" ("Kind", "RowVersion", "UserId", "Value")
               VALUES (?1, 1, ?2, '')"#,
        )
        .bind(i32::from(*kind))
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
    }

    Ok(())
}

/// Sets a permission during first-run bootstrap, inside the caller's
/// transaction (used by [`crate::user_manager`]'s `initialize`, where the whole
/// user is created atomically). Runs the same `UPDATE`-then-conditional-`INSERT`
/// as [`set_permission`] against the transaction connection.
///
/// # Errors
/// Returns [`ServiceError::Db`] if either statement fails.
pub async fn set_permission_tx(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    user_id: &str,
    kind: PermissionKind,
    value: bool,
) -> Result<(), ServiceError> {
    sqlx::query(r#"UPDATE "Permissions" SET "Value" = ?3 WHERE "UserId" = ?1 AND "Kind" = ?2"#)
        .bind(user_id)
        .bind(i32::from(kind))
        .bind(value)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;

    sqlx::query(
        r#"INSERT INTO "Permissions" ("Kind", "RowVersion", "UserId", "Value")
           SELECT ?2, 1, ?1, ?3
           WHERE NOT EXISTS (
               SELECT 1 FROM "Permissions" WHERE "UserId" = ?1 AND "Kind" = ?2
           )"#,
    )
    .bind(user_id)
    .bind(i32::from(kind))
    .bind(value)
    .execute(&mut **tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{seed_user, test_db};
    use hermit_db::store::guid_to_db;
    use uuid::Uuid;

    #[tokio::test]
    async fn default_permissions_read_back() {
        let db = test_db().await;
        let id = Uuid::from_u128(1);
        seed_user(&db, id).await;
        let mut tx = db.writer().begin().await.expect("begin");
        seed_defaults(&mut tx, &guid_to_db(id)).await.expect("seed");
        tx.commit().await.expect("commit");

        assert!(
            has_permission(db.pool(), &guid_to_db(id), PermissionKind::EnableAllDevices)
                .await
                .expect("has perm")
        );
        assert!(
            !has_permission(db.pool(), &guid_to_db(id), PermissionKind::IsAdministrator)
                .await
                .expect("has perm")
        );
    }

    #[tokio::test]
    async fn set_permission_inserts_then_updates() {
        let db = test_db().await;
        let id = Uuid::from_u128(2);
        seed_user(&db, id).await;

        set_permission(
            db.pool(),
            &guid_to_db(id),
            PermissionKind::IsAdministrator,
            true,
        )
        .await
        .expect("insert");
        assert!(
            has_permission(db.pool(), &guid_to_db(id), PermissionKind::IsAdministrator)
                .await
                .expect("read")
        );

        set_permission(
            db.pool(),
            &guid_to_db(id),
            PermissionKind::IsAdministrator,
            false,
        )
        .await
        .expect("update");
        assert!(
            !has_permission(db.pool(), &guid_to_db(id), PermissionKind::IsAdministrator)
                .await
                .expect("read")
        );
    }

    #[tokio::test]
    async fn missing_permission_is_false() {
        let db = test_db().await;
        let id = Uuid::from_u128(3);
        seed_user(&db, id).await;
        assert!(
            !has_permission(db.pool(), &guid_to_db(id), PermissionKind::EnableAllDevices)
                .await
                .expect("read")
        );
    }

    #[tokio::test]
    async fn no_schedules_is_always_allowed() {
        let db = test_db().await;
        let id = Uuid::from_u128(4);
        seed_user(&db, id).await;
        assert!(
            is_parental_schedule_allowed(db.pool(), &guid_to_db(id), chrono::Local::now())
                .await
                .expect("allowed")
        );
    }
}
