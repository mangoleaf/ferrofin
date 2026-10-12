//! Persistence operations for account authentication.

use sqlx::SqlitePool;

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
