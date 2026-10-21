//! Persistence operations for accounts: authentication and the profile image.

use ferrofin_db::entities::users::ImageInfoEntity;
use ferrofin_db::store::datetime_to_db;
use sqlx::{SqliteExecutor, SqlitePool};

/// Changes only the selected provider, preserving concurrent preference edits.
pub(super) async fn set_authentication_provider(
    pool: &SqlitePool,
    user_id: &str,
    provider_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(r#"UPDATE "Users" SET "AuthenticationProviderId" = ?2 WHERE "Id" = ?1"#)
        .bind(user_id)
        .bind(provider_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Stores a user's password hash (`None` for a password-less user), bumping
/// `RowVersion` as an EF save of the entity does.
pub(super) async fn set_password(
    pool: &SqlitePool,
    user_id: &str,
    hash: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"UPDATE "Users" SET "Password" = ?2, "RowVersion" = "RowVersion" + 1
           WHERE "Id" = ?1"#,
    )
    .bind(user_id)
    .bind(hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// A user's `ImageInfos` row, or `None` when they have no profile image.
pub(super) async fn profile_image<'e, E>(
    executor: E,
    user_id: &str,
) -> Result<Option<ImageInfoEntity>, sqlx::Error>
where
    E: SqliteExecutor<'e>,
{
    sqlx::query_as(r#"SELECT * FROM "ImageInfos" WHERE "UserId" = ?1 LIMIT 1"#)
        .bind(user_id)
        .fetch_optional(executor)
        .await
}

/// Replaces a user's `ImageInfos` row with one for `path` (one image per user:
/// `UserId` is unique).
pub(super) async fn replace_profile_image(
    conn: &mut sqlx::SqliteConnection,
    user_id: &str,
    path: &str,
) -> Result<(), sqlx::Error> {
    delete_profile_image(&mut *conn, user_id).await?;
    sqlx::query(
        r#"INSERT INTO "ImageInfos" ("LastModified", "Path", "UserId") VALUES (?1, ?2, ?3)"#,
    )
    .bind(datetime_to_db(chrono::Utc::now()))
    .bind(path)
    .bind(user_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Deletes a user's `ImageInfos` row, returning the path it held.
pub(super) async fn delete_profile_image<'e, E>(
    executor: E,
    user_id: &str,
) -> Result<Option<String>, sqlx::Error>
where
    E: SqliteExecutor<'e>,
{
    sqlx::query_scalar(r#"DELETE FROM "ImageInfos" WHERE "UserId" = ?1 RETURNING "Path""#)
        .bind(user_id)
        .fetch_optional(executor)
        .await
}

/// Points a user's `ImageInfos` row at `path`.
pub(super) async fn set_profile_image_path<'e, E>(
    executor: E,
    user_id: &str,
    path: &str,
) -> Result<(), sqlx::Error>
where
    E: SqliteExecutor<'e>,
{
    sqlx::query(r#"UPDATE "ImageInfos" SET "Path" = ?2 WHERE "UserId" = ?1"#)
        .bind(user_id)
        .bind(path)
        .execute(executor)
        .await?;
    Ok(())
}
