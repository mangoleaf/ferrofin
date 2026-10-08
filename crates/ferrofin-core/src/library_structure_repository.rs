//! Preserve media written by the old collection-folder-parented scanner.
//!
//! D1 scans store media under physical location folders. A library may be removed
//! before its first D1 scan, so convert its existing hierarchy transactionally
//! before deleting the collection folder. Only hierarchy columns change: metadata,
//! media ids, owned extras and user data survive without a provider refresh.

use std::path::Path;

use ferrofin_db::{Database, store::guid_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use crate::{
    db_error::db_err,
    item_type_lookup::{IdDerivation, derive_item_id_with, stored_type_name},
};

#[derive(sqlx::FromRow)]
struct Child {
    id: String,
    path: Option<String>,
}

/// Convert only trees still directly parented by this collection folder.
/// A missing/malformed location description stops the removal instead of
/// cascading through media whose physical parent cannot be established.
pub(crate) async fn preserve_children(
    db: &Database,
    collection: Uuid,
    aggregate: Uuid,
    mode: &IdDerivation,
) -> Result<(), ServiceError> {
    let cf = guid_to_db(collection);
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let children: Vec<Child> = sqlx::query_as(
        r#"SELECT "Id" AS id, "Path" AS path FROM "BaseItems" WHERE "ParentId" = ?"#,
    )
    .bind(&cf)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    if children.is_empty() {
        return Ok(());
    }
    let (path, data): (Option<String>, Option<String>) =
        sqlx::query_as(r#"SELECT "Path", "Data" FROM "BaseItems" WHERE "Id" = ?"#)
            .bind(&cf)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
    let data: serde_json::Value = data
        .as_deref()
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default();
    let locations: Vec<&str> = data["PhysicalLocationsList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|p| !p.is_empty() && Some(*p) != path.as_deref())
        .collect();
    let aggregate = guid_to_db(aggregate);
    for child in children {
        // A pathless virtual folder belongs to the same physical location as
        // its descendants. UNION also terminates on a corrupt parent cycle.
        let descendants: Vec<Child> = sqlx::query_as(
            r#"WITH RECURSIVE tree(id) AS (
                SELECT "Id" FROM "BaseItems" WHERE "Id" = ?
                UNION SELECT b."Id" FROM "BaseItems" b JOIN tree t ON b."ParentId" = t.id
            ) SELECT b."Id" AS id, b."Path" AS path FROM "BaseItems" b JOIN tree t ON b."Id" = t.id"#,
        ).bind(&child.id).fetch_all(&mut *tx).await.map_err(db_err)?;
        let physical_path = child
            .path
            .as_deref()
            .or_else(|| descendants.iter().find_map(|row| row.path.as_deref()));
        let location = physical_path
            .and_then(|path| {
                locations
                    .iter()
                    .copied()
                    .filter(|location| Path::new(path).starts_with(location))
                    .max_by_key(|location| location.len())
            })
            .ok_or_else(|| {
                ServiceError::backend(format!(
                    "cannot preserve library item {}: no matching physical location",
                    child.id
                ))
            })?;
        let location = if location == "/" {
            location
        } else {
            location.trim_end_matches('/')
        };
        let id = derive_item_id_with(mode, BaseItemKind::Folder, location)
            .ok_or_else(|| ServiceError::backend("cannot derive the library location id"))?;
        let folder = guid_to_db(id);
        ensure_location(&mut tx, id, &folder, &aggregate, location).await?;
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ? WHERE "Id" = ?"#)
            .bind(if child.id == folder {
                &aggregate
            } else {
                &folder
            })
            .bind(&child.id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for row in descendants {
            sqlx::query(
                r#"UPDATE "BaseItems" SET "TopParentId" = ? WHERE "Id" = ? AND "TopParentId" = ?"#,
            )
            .bind(&folder)
            .bind(&row.id)
            .bind(&cf)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            if row.id == folder {
                continue;
            }
            sqlx::query(r#"INSERT OR IGNORE INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?, ?), (?, ?)"#)
                .bind(&row.id).bind(&folder).bind(&row.id).bind(&aggregate)
                .execute(&mut *tx).await.map_err(db_err)?;
        }
    }
    tx.commit().await.map_err(db_err)
}

/// Provision the physical parent inside the same transaction as its children.
async fn ensure_location(
    connection: &mut sqlx::SqliteConnection,
    id: Uuid,
    folder: &str,
    aggregate: &str,
    location: &str,
) -> Result<(), ServiceError> {
    sqlx::query(
            r#"INSERT INTO "BaseItems" (
                "Id", "Type", "Name", "Path", "ParentId", "TopParentId", "PresentationUniqueKey",
                "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem"
            ) VALUES (?, ?, ?, ?, ?, ?, ?, 1, 0, 0, 0, 0, 0, 0) ON CONFLICT("Id") DO NOTHING"#,
        ).bind(folder).bind(stored_type_name(BaseItemKind::Folder).unwrap_or_default())
            .bind(Path::new(location).file_name().and_then(|v| v.to_str()).unwrap_or(location))
            .bind(location).bind(aggregate).bind(folder).bind(id.simple().to_string())
            .execute(&mut *connection).await.map_err(db_err)?;
    sqlx::query(r#"INSERT OR IGNORE INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?, ?)"#)
        .bind(folder)
        .bind(aggregate)
        .execute(&mut *connection)
        .await
        .map_err(db_err)?;
    Ok(())
}
