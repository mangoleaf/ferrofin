//! The `AccessSchedules` table — a user's allowed viewing windows.
//!
//! Port of the `User.AccessSchedules` navigation `UserManager.UpdatePolicyAsync`
//! clears and refills, and `GetUserDto` reads back.

use ferrofin_model::users::{AccessSchedule, DynamicDayOfWeek};
use ferrofin_traits::error::ServiceError;
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

use crate::db_error::db_err;

/// Reads a user's access schedules.
///
/// `user_id` is the stored `Guid` text used for the lookup; `uid` is that same
/// id already parsed by the caller, so the emitted `AccessSchedule.UserId`
/// cannot silently become the nil GUID.
///
/// # Errors
///
/// Returns a backend error when the query fails.
pub async fn list(
    pool: &SqlitePool,
    user_id: &str,
    uid: Uuid,
) -> Result<Vec<AccessSchedule>, ServiceError> {
    let rows: Vec<(i64, i32, f64, f64)> = sqlx::query_as(
        r#"SELECT "Id", "DayOfWeek", "StartHour", "EndHour"
           FROM "AccessSchedules" WHERE "UserId" = ?1"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .map(|(id, day, start, end)| AccessSchedule {
            id: i32::try_from(id).unwrap_or(i32::MAX),
            user_id: uid,
            day_of_week: day_from_i32(day),
            start_hour: start,
            end_hour: end,
        })
        .collect())
}

/// `user.AccessSchedules.Clear()` + re-add: replaces a user's schedule rows in
/// one transaction.
///
/// # Errors
///
/// Returns a backend error when a statement fails.
pub async fn replace(
    pool: &SqlitePool,
    user_id: &str,
    schedules: &[AccessSchedule],
) -> Result<(), ServiceError> {
    let mut tx = pool.begin().await.map_err(db_err)?;
    replace_tx(&mut tx, user_id, schedules).await?;
    tx.commit().await.map_err(db_err)
}

/// [`replace`] inside a caller-owned transaction (the policy update writes the
/// user's columns, permissions, schedules and preferences as one unit).
///
/// # Errors
///
/// Returns a backend error when a statement fails.
pub async fn replace_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: &str,
    schedules: &[AccessSchedule],
) -> Result<(), ServiceError> {
    sqlx::query(r#"DELETE FROM "AccessSchedules" WHERE "UserId" = ?1"#)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
    for schedule in schedules {
        sqlx::query(
            r#"INSERT INTO "AccessSchedules" ("DayOfWeek", "StartHour", "EndHour", "UserId")
               VALUES (?1, ?2, ?3, ?4)"#,
        )
        .bind(day_to_i32(schedule.day_of_week))
        .bind(schedule.start_hour)
        .bind(schedule.end_hour)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
    }
    Ok(())
}

/// Maps a stored `DayOfWeek` discriminant to its [`DynamicDayOfWeek`].
#[must_use]
pub fn day_from_i32(value: i32) -> DynamicDayOfWeek {
    match value {
        1 => DynamicDayOfWeek::Monday,
        2 => DynamicDayOfWeek::Tuesday,
        3 => DynamicDayOfWeek::Wednesday,
        4 => DynamicDayOfWeek::Thursday,
        5 => DynamicDayOfWeek::Friday,
        6 => DynamicDayOfWeek::Saturday,
        7 => DynamicDayOfWeek::Everyday,
        8 => DynamicDayOfWeek::Weekday,
        9 => DynamicDayOfWeek::Weekend,
        _ => DynamicDayOfWeek::Sunday,
    }
}

/// The stored `DayOfWeek` discriminant of a [`DynamicDayOfWeek`] (the inverse
/// of [`day_from_i32`]).
#[must_use]
pub fn day_to_i32(day: DynamicDayOfWeek) -> i32 {
    match day {
        DynamicDayOfWeek::Sunday => 0,
        DynamicDayOfWeek::Monday => 1,
        DynamicDayOfWeek::Tuesday => 2,
        DynamicDayOfWeek::Wednesday => 3,
        DynamicDayOfWeek::Thursday => 4,
        DynamicDayOfWeek::Friday => 5,
        DynamicDayOfWeek::Saturday => 6,
        DynamicDayOfWeek::Everyday => 7,
        DynamicDayOfWeek::Weekday => 8,
        DynamicDayOfWeek::Weekend => 9,
    }
}

#[cfg(test)]
mod tests {
    use super::{day_from_i32, day_to_i32};
    use ferrofin_model::users::DynamicDayOfWeek;

    #[test]
    fn day_discriminants_round_trip() {
        for (i, day) in [
            DynamicDayOfWeek::Sunday,
            DynamicDayOfWeek::Monday,
            DynamicDayOfWeek::Tuesday,
            DynamicDayOfWeek::Wednesday,
            DynamicDayOfWeek::Thursday,
            DynamicDayOfWeek::Friday,
            DynamicDayOfWeek::Saturday,
            DynamicDayOfWeek::Everyday,
            DynamicDayOfWeek::Weekday,
            DynamicDayOfWeek::Weekend,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(day_to_i32(day), i32::try_from(i).unwrap());
            assert_eq!(day_from_i32(day_to_i32(day)), day);
        }
    }
}
