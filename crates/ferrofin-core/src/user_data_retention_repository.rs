//! Source snapshots supplement the compatible UserData placeholder rows.
//! Snapshot aliases are consumed together, so returning to a previous path
//! cannot revive an obsolete GUID alias. Every operation uses its caller's
//! transaction, including the compatible mirror's cleanup.

use ferrofin_traits::error::ServiceError;
use sqlx::SqliteConnection;

use crate::db_error::db_err;
use crate::translate_query::PLACEHOLDER_ID;

fn json<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, ServiceError> {
    serde_json::to_string(value).map_err(|e| ServiceError::Backend(e.to_string()))
}

pub(crate) async fn capture(
    tx: &mut SqliteConnection,
    ids: &[String],
    retained_at: &str,
) -> Result<(), ServiceError> {
    let snapshots: Vec<(i64, String, String)> = sqlx::query_as(
        r#"WITH ranked AS (
            SELECT *, ROW_NUMBER() OVER (
                PARTITION BY "ItemId", "UserId"
                ORDER BY ("CustomDataKey" = lower("ItemId")) DESC,
                    "LastPlayedDate" DESC, "PlayCount" DESC, "CustomDataKey"
            ) AS priority FROM "UserData"
            WHERE "ItemId" IN (SELECT value FROM json_each(?1)) AND "ItemId" <> ?2
        )
        INSERT INTO "FerrofinUserDataRetentionSnapshots" (
            "ItemId", "UserId", "CustomDataKey", "AudioStreamIndex", "IsFavorite",
            "LastPlayedDate", "Likes", "PlayCount", "PlaybackPositionTicks",
            "Played", "Rating", "RetentionDate", "SubtitleStreamIndex")
        SELECT "ItemId", "UserId", "CustomDataKey", "AudioStreamIndex", "IsFavorite",
            "LastPlayedDate", "Likes", "PlayCount", "PlaybackPositionTicks",
            "Played", "Rating", ?3, "SubtitleStreamIndex"
        FROM ranked WHERE priority = 1
        RETURNING "SnapshotId", "ItemId", "UserId""#,
    )
    .bind(json(ids)?)
    .bind(PLACEHOLDER_ID)
    .bind(retained_at)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let mut sources: Vec<String> = snapshots.iter().map(|s| s.1.clone()).collect();
    sources.sort_unstable();
    sources.dedup();
    let identities: std::collections::HashMap<_, _> =
        crate::user_data_key_repository::load_keys_for_items(tx, &sources)
            .await?
            .into_iter()
            .collect();
    for (snapshot, item, user) in snapshots {
        sqlx::query(
            r#"INSERT INTO "FerrofinUserDataRetentionKeys" ("SnapshotId", "Kind", "Key")
            SELECT ?1, 'mirror', "CustomDataKey" FROM "UserData"
            WHERE "ItemId" = ?2 AND "UserId" = ?3"#,
        )
        .bind(snapshot)
        .bind(&item)
        .bind(user)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if let Some(keys) = identities.get(&item) {
            sqlx::query(
                r#"INSERT OR IGNORE INTO "FerrofinUserDataRetentionKeys" ("SnapshotId", "Kind", "Key")
                SELECT ?1, 'identity', value FROM json_each(?2)"#,
            )
            .bind(snapshot)
            .bind(json(keys)?)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
    }
    Ok(())
}

pub(crate) async fn recover(
    tx: &mut SqliteConnection,
    item: &str,
    keys: &[String],
) -> Result<(), ServiceError> {
    let keys = json(keys)?;
    let candidates: Vec<i64> = sqlx::query_scalar(
        r#"SELECT DISTINCT s."SnapshotId" FROM "FerrofinUserDataRetentionSnapshots" s
        JOIN "FerrofinUserDataRetentionKeys" k USING ("SnapshotId")
        WHERE k."Kind" = 'identity' AND k."Key" IN (SELECT value FROM json_each(?1))
        AND NOT EXISTS (SELECT 1 FROM "UserData" current
            WHERE current."ItemId" = ?2 AND current."UserId" = s."UserId")"#,
    )
    .bind(&keys)
    .bind(item)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    if candidates.is_empty() {
        return Ok(());
    }
    sqlx::query(
        r#"WITH ranked AS MATERIALIZED (
            SELECT *, ROW_NUMBER() OVER (PARTITION BY "UserId"
                ORDER BY "RetentionDate" DESC, "SnapshotId" DESC) AS priority
            FROM "FerrofinUserDataRetentionSnapshots" s
            WHERE "SnapshotId" IN (SELECT value FROM json_each(?1))
        )
        INSERT INTO "UserData" (
            "ItemId", "UserId", "CustomDataKey", "AudioStreamIndex", "IsFavorite",
            "LastPlayedDate", "Likes", "PlayCount", "PlaybackPositionTicks",
            "Played", "Rating", "RetentionDate", "SubtitleStreamIndex")
        SELECT ?2, "UserId", keys.value, "AudioStreamIndex", "IsFavorite",
            "LastPlayedDate", "Likes", "PlayCount", "PlaybackPositionTicks",
            "Played", "Rating", NULL, "SubtitleStreamIndex"
        FROM ranked CROSS JOIN json_each(?3) keys WHERE priority = 1
        ON CONFLICT("ItemId", "UserId", "CustomDataKey") DO NOTHING"#,
    )
    .bind(json(&candidates)?)
    .bind(item)
    .bind(keys)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    consume(tx, &candidates).await
}

async fn consume(tx: &mut SqliteConnection, snapshots: &[i64]) -> Result<(), ServiceError> {
    let snapshots = json(snapshots)?;
    // A colliding snapshot may still own the same compatible mirror key.
    // Leave that row shadowed until its last owning snapshot is consumed.
    sqlx::query(
        r#"DELETE FROM "UserData" AS ud WHERE ud."ItemId" = ?1
        AND EXISTS (SELECT 1 FROM "FerrofinUserDataRetentionSnapshots" s
            JOIN "FerrofinUserDataRetentionKeys" k USING ("SnapshotId")
            WHERE k."Kind" = 'mirror' AND k."Key" = ud."CustomDataKey"
            AND s."UserId" = ud."UserId" AND s."SnapshotId" IN (SELECT value FROM json_each(?2)))
        AND NOT EXISTS (SELECT 1 FROM "FerrofinUserDataRetentionSnapshots" s
            JOIN "FerrofinUserDataRetentionKeys" k USING ("SnapshotId")
            WHERE k."Kind" = 'mirror' AND k."Key" = ud."CustomDataKey"
            AND s."UserId" = ud."UserId" AND s."SnapshotId" NOT IN (SELECT value FROM json_each(?2)))"#,
    )
    .bind(PLACEHOLDER_ID)
    .bind(&snapshots)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    sqlx::query(
        r#"DELETE FROM "FerrofinUserDataRetentionSnapshots"
        WHERE "SnapshotId" IN (SELECT value FROM json_each(?1))"#,
    )
    .bind(snapshots)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

pub(crate) async fn purge_expired(
    db: &ferrofin_db::Database,
    cutoff: &str,
) -> Result<u64, ServiceError> {
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let snapshots: Vec<i64> = sqlx::query_scalar(
        r#"SELECT "SnapshotId" FROM "FerrofinUserDataRetentionSnapshots" WHERE "RetentionDate" < ?1"#,
    )
    .bind(cutoff)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    consume(&mut tx, &snapshots).await?;
    let deleted =
        sqlx::query(r#"DELETE FROM "UserData" WHERE "ItemId" = ?1 AND "RetentionDate" < ?2"#)
            .bind(PLACEHOLDER_ID)
            .bind(cutoff)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    Ok(deleted.rows_affected() + u64::try_from(snapshots.len()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{seed_item, seed_user, seed_user_data, test_db};
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::persistence::ItemPersistenceService;
    use uuid::Uuid;

    #[tokio::test]
    async fn retention_snapshots_expire_and_user_deletion_removes_the_remainder() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        seed_user(&db, user).await;
        let old = Uuid::new_v4();
        let recent = Uuid::new_v4();
        for item in [old, recent] {
            seed_item(&db, item, BaseItemKind::Movie).await;
            seed_user_data(&db, user, item, true, None).await;
        }
        crate::FerrofinItemPersistenceService::new(db.clone())
            .delete_items(&[old, recent])
            .await
            .unwrap();
        sqlx::query(
            r#"UPDATE "FerrofinUserDataRetentionSnapshots"
            SET "RetentionDate" = '2000-01-01 00:00:00.0000000' WHERE "ItemId" = ?1"#,
        )
        .bind(guid_to_db(old))
        .execute(db.writer())
        .await
        .unwrap();
        purge_expired(&db, "2001-01-01 00:00:00.0000000")
            .await
            .unwrap();
        let owners: Vec<String> =
            sqlx::query_scalar(r#"SELECT "ItemId" FROM "FerrofinUserDataRetentionSnapshots""#)
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(owners, vec![guid_to_db(recent)]);
        let keys: Vec<String> = sqlx::query_scalar(r#"SELECT "CustomDataKey" FROM "UserData""#)
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(keys, vec![recent.to_string()]);
        sqlx::query(r#"DELETE FROM "Users" WHERE "Id" = ?1"#)
            .bind(guid_to_db(user))
            .execute(db.writer())
            .await
            .unwrap();
        let remaining: i64 = sqlx::query_scalar(
            r#"SELECT
            (SELECT COUNT(*) FROM "FerrofinUserDataRetentionSnapshots") +
            (SELECT COUNT(*) FROM "FerrofinUserDataRetentionKeys") +
            (SELECT COUNT(*) FROM "UserData")"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(remaining, 0);
    }
}
