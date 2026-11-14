//! The `MediaSegments` writes of
//! [`FerrofinMediaSegmentManager`](crate::media_segment_manager::FerrofinMediaSegmentManager)
//! that replace or clear a provider's rows.

use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use crate::db_error::db_err;

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
    let mut delete = String::from(
        r#"DELETE FROM "MediaSegments" WHERE "ItemId" = ?1 AND "SegmentProviderId" = ?2"#,
    );
    if type_filter.is_some() {
        delete.push_str(r#" AND "Type" = ?3"#);
    }
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
