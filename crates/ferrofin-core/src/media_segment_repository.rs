//! The `MediaSegments` writes of
//! [`FerrofinMediaSegmentManager`](crate::media_segment_manager::FerrofinMediaSegmentManager)
//! that replace or clear a provider's rows, and the SQL of its per-item reads.
//!
//! Every per-item statement seeks `FerrofinIX_MediaSegments_ItemId_StartTicks`
//! (migration 0052); without it each one scanned the whole table (gl-1409).

use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use crate::db_error::db_err;

/// An item's rows in playback order: the index gives the order, so there is no sort.
pub(crate) const ITEM_ROWS: &str =
    r#"SELECT * FROM "MediaSegments" WHERE "ItemId" = ?1 ORDER BY "StartTicks""#;

/// How many rows an item has, counted in the index alone.
pub(crate) const ITEM_ROW_COUNT: &str =
    r#"SELECT COUNT(*) FROM "MediaSegments" WHERE "ItemId" = ?1"#;

/// The DELETE [`replace_rows`] runs per namespace, with `"Type" = ?3` when `typed`.
fn replace_delete_sql(typed: bool) -> String {
    let mut delete = String::from(
        r#"DELETE FROM "MediaSegments" WHERE "ItemId" = ?1 AND "SegmentProviderId" = ?2"#,
    );
    if typed {
        delete.push_str(r#" AND "Type" = ?3"#);
    }
    delete
}

/// Deletes `item`'s rows stored under each of `namespaces` (only those of
/// `type_filter`, when given) and inserts `segments` under `provider_id`, in one
/// write transaction, so a failure or a crash part-way leaves the previous rows
/// and a reader never sees the item with that provider's segments half replaced.
///
/// # Errors
///
/// [`ServiceError::invalid_input`] when a segment ends before it starts (nothing
/// is written), or the database error, after which nothing has changed.
pub(crate) async fn replace_rows(
    db: &Database,
    item: Uuid,
    namespaces: &[String],
    type_filter: Option<MediaSegmentType>,
    provider_id: &str,
    segments: &[MediaSegmentDto],
) -> Result<(), ServiceError> {
    debug_assert!(segments.iter().all(|segment| segment.item_id == item));
    // The producer entry points also check before publishing their cache; this one guards
    // `run_provider` and `create_stored`.
    if segments
        .iter()
        .any(|segment| segment.end_ticks < segment.start_ticks)
    {
        return Err(ServiceError::invalid_input(
            "segment end precedes its start",
        ));
    }
    let delete = replace_delete_sql(type_filter.is_some());
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    for namespace in namespaces {
        let mut query = sqlx::query(sqlx::AssertSqlSafe(delete.clone()))
            .bind(guid_to_db(item))
            .bind(namespace);
        if let Some(kind) = type_filter {
            query = query.bind(kind.json_value());
        }
        query.execute(&mut *tx).await.map_err(db_err)?;
    }
    for segment in segments {
        sqlx::query(
            r#"INSERT INTO "MediaSegments"
               ("Id", "EndTicks", "ItemId", "SegmentProviderId", "StartTicks", "Type")
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
        )
        .bind(guid_to_db(segment.id))
        .bind(segment.end_ticks)
        .bind(guid_to_db(segment.item_id))
        .bind(provider_id)
        .bind(segment.start_ticks)
        .bind(segment.type_.json_value())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    tx.commit().await.map_err(db_err)
}

/// Deletes every item's rows stored under each of `namespaces` (only those of
/// `type_filter`, when given), in one write transaction, so a failure part-way
/// leaves every namespace's rows (gl-1401). Returns the item of each deleted row.
///
/// # Errors
///
/// The database error, or a stored item id that is not a GUID, after which
/// nothing has changed.
pub(crate) async fn clear_rows(
    db: &Database,
    namespaces: &[String],
    type_filter: Option<MediaSegmentType>,
) -> Result<Vec<Uuid>, ServiceError> {
    let mut delete = String::from(r#"DELETE FROM "MediaSegments" WHERE "SegmentProviderId" = ?1"#);
    if type_filter.is_some() {
        delete.push_str(r#" AND "Type" = ?2"#);
    }
    delete.push_str(r#" RETURNING "ItemId""#);
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let mut items = Vec::new();
    for namespace in namespaces {
        let mut query =
            sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(delete.clone())).bind(namespace);
        if let Some(kind) = type_filter {
            query = query.bind(kind.json_value());
        }
        for item in query.fetch_all(&mut *tx).await.map_err(db_err)? {
            items.push(
                Uuid::parse_str(&item).map_err(|error| ServiceError::backend(error.to_string()))?,
            );
        }
    }
    tx.commit().await.map_err(db_err)?;
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::{ITEM_ROW_COUNT, ITEM_ROWS, replace_delete_sql};
    use crate::test_support::test_db;

    const INDEX: &str = "FerrofinIX_MediaSegments_ItemId_StartTicks (ItemId=?)";

    /// `EXPLAIN QUERY PLAN` for `sql`, one `detail` per step.
    async fn query_plan(db: &ferrofin_db::Database, sql: &str) -> Vec<String> {
        sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {sql}"
        )))
        .fetch_all(db.pool())
        .await
        .expect("explain query plan")
        .into_iter()
        .map(|(_, _, _, detail)| detail)
        .collect()
    }

    /// `GET /MediaSegments/{itemId}` and `PlaybackInfo`'s `HasSegments` read one
    /// item's rows at every playback start, and a provider's replace deletes them
    /// inside the writer's transaction: each seeks the item on the index (the read
    /// in index order, the count in the index alone), never scanning the table or
    /// sorting.
    #[tokio::test]
    async fn per_item_statements_seek_the_item_index() {
        let db = test_db().await;
        let seek = format!("SEARCH MediaSegments USING INDEX {INDEX}");
        let count = format!("SEARCH MediaSegments USING COVERING INDEX {INDEX}");
        for (sql, plan) in [
            (ITEM_ROWS.to_owned(), &seek),
            (ITEM_ROW_COUNT.to_owned(), &count),
            (replace_delete_sql(false), &seek),
            (replace_delete_sql(true), &seek),
        ] {
            assert_eq!(query_plan(&db, &sql).await, [plan.as_str()], "{sql}");
        }
    }
}
