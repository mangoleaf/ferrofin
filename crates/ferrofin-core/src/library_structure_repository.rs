//! Preserve media written by the old collection-folder-parented scanner.
//!
//! D1 scans store media under physical location folders. A library may be removed
//! before its first D1 scan, so convert its existing hierarchy transactionally
//! before deleting the collection folder. Only hierarchy columns change: metadata,
//! media ids, owned extras and user data survive without a provider refresh.

use std::path::Path;

use chrono::{DateTime, Utc};
use ferrofin_db::{
    Database,
    store::{datetime_to_db, guid_to_db},
};
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use crate::{
    db_error::db_err,
    item_persistence_service::move_user_data,
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
        let now = Utc::now();
        ensure_location(&mut tx, id, Some(&aggregate), location, (now, now)).await?;
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

/// Provision the physical parent inside the same transaction as its children:
/// the row `plan_location_folder` keeps for a library location, its own top
/// parent, with no parent when the aggregate root is not there yet. An existing
/// row stays as it is. `dates` are its `DateCreated` and `DateModified`.
async fn ensure_location(
    connection: &mut sqlx::SqliteConnection,
    id: Uuid,
    aggregate: Option<&str>,
    location: &str,
    dates: (DateTime<Utc>, DateTime<Utc>),
) -> Result<(), ServiceError> {
    let folder = guid_to_db(id);
    let name = Path::new(location)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or(location);
    sqlx::query(
            r#"INSERT INTO "BaseItems" (
                "Id", "Type", "Name", "SortName", "CleanName", "Path", "ParentId", "TopParentId",
                "PresentationUniqueKey", "DateCreated", "DateModified", "DateLastSaved",
                "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem"
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?1, ?8, ?9, ?10, ?11, 1, 0, 0, 0, 0, 0, 0)
            ON CONFLICT("Id") DO NOTHING"#,
        ).bind(&folder).bind(stored_type_name(BaseItemKind::Folder).unwrap_or_default())
            .bind(name)
            .bind(crate::kinds::saved_sort_name(BaseItemKind::Folder, name, None))
            .bind(crate::text_util::get_clean_value(name))
            .bind(location).bind(aggregate).bind(id.simple().to_string())
            .bind(datetime_to_db(dates.0)).bind(datetime_to_db(dates.1))
            .bind(datetime_to_db(Utc::now()))
            .execute(&mut *connection).await.map_err(db_err)?;
    if let Some(aggregate) = aggregate {
        sqlx::query(
            r#"INSERT OR IGNORE INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?, ?)"#,
        )
        .bind(&folder)
        .bind(aggregate)
        .execute(&mut *connection)
        .await
        .map_err(db_err)?;
    }
    Ok(())
}

/// A `Folder` row kept for a library location that no scan has reached yet
/// (`ValidateTopLibraryFolders`, which `AddVirtualFolder` runs before its
/// background scan), stored at `location`. A row already under `id` stays.
pub(crate) async fn insert_location_folder(
    db: &Database,
    id: Uuid,
    aggregate: Option<Uuid>,
    location: &str,
    dates: (DateTime<Utc>, DateTime<Utc>),
) -> Result<(), ServiceError> {
    let aggregate = aggregate.map(guid_to_db);
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    ensure_location(&mut tx, id, aggregate.as_deref(), location, dates).await?;
    tx.commit().await.map_err(db_err)
}

/// A folder row as a library lookup needs it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct FolderRow {
    /// The stored id.
    #[sqlx(rename = "Id")]
    pub(crate) id: String,
    /// The stored path, which an adopted row keeps in its token form.
    #[sqlx(rename = "Path")]
    pub(crate) path: Option<String>,
    /// The stored name.
    #[sqlx(rename = "Name")]
    pub(crate) name: Option<String>,
}

/// `RootFolder.Children.OfType<Folder>()`: the aggregate root's child folders
/// in `SortName` order, the order `Folder.GetCachedChildren` reads them in.
/// A `CollectionFolder` is no physical child of the root: one stored there is
/// left out, so it can never be taken for a library's location.
pub(crate) async fn root_child_folders(
    db: &Database,
    aggregate: Uuid,
) -> Result<Vec<FolderRow>, ServiceError> {
    sqlx::query_as(
        r#"SELECT "Id", "Path", "Name" FROM "BaseItems"
           WHERE "ParentId" = ?1 AND "IsFolder" = 1 AND "Type" <> ?2
           ORDER BY "SortName", rowid"#,
    )
    .bind(guid_to_db(aggregate))
    .bind(stored_type_name(BaseItemKind::CollectionFolder).unwrap_or_default())
    .fetch_all(db.pool())
    .await
    .map_err(db_err)
}

/// The oldest `kind` row stored at `path` or at its `%AppDataPath%` spelling
/// `token`.
pub(crate) async fn folder_at(
    db: &Database,
    kind: BaseItemKind,
    path: &str,
    token: &str,
) -> Result<Option<FolderRow>, ServiceError> {
    sqlx::query_as(
        r#"SELECT "Id", "Path", "Name" FROM "BaseItems"
           WHERE "Path" IN (?1, ?2) AND "Type" = ?3 ORDER BY rowid LIMIT 1"#,
    )
    .bind(path)
    .bind(token)
    .bind(stored_type_name(kind).unwrap_or_default())
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)
}

/// The `kind` row stored under `id`.
pub(crate) async fn folder_of_kind(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
) -> Result<Option<FolderRow>, ServiceError> {
    sqlx::query_as(
        r#"SELECT "Id", "Path", "Name" FROM "BaseItems" WHERE "Id" = ?1 AND "Type" = ?2"#,
    )
    .bind(guid_to_db(id))
    .bind(stored_type_name(kind).unwrap_or_default())
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)
}

/// Records `ancestors` for `item`: `GetAncestorIds`, its parents up to the root
/// and the libraries it belongs to. An ancestor with no row yet is skipped, as
/// a key to it could not be stored.
pub(crate) async fn add_ancestors_in(
    connection: &mut sqlx::SqliteConnection,
    item: Uuid,
    ancestors: &[Uuid],
) -> Result<(), ServiceError> {
    for ancestor in ancestors {
        sqlx::query(
            r#"INSERT OR IGNORE INTO "AncestorIds" ("ItemId", "ParentItemId")
               SELECT ?1, "Id" FROM "BaseItems" WHERE "Id" = ?2"#,
        )
        .bind(guid_to_db(item))
        .bind(guid_to_db(*ancestor))
        .execute(&mut *connection)
        .await
        .map_err(db_err)?;
    }
    Ok(())
}

/// What [`merge_into_location`] moves a retired container's content onto.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LocationTarget {
    /// The `Folder` row of the library's location, which takes the children.
    pub(crate) folder: Uuid,
    /// The aggregate root, an ancestor of everything under the location.
    pub(crate) aggregate: Uuid,
    /// The library's `CollectionFolder`, which takes the row's own user data
    /// and images.
    pub(crate) library: Uuid,
}

/// Moves what hangs off the retired container `old` onto a library's location
/// and deletes it, in one transaction: a failure anywhere leaves every row as
/// it was. Returns how many items moved.
///
/// Everything is re-aimed before the `DELETE`, so its `ON DELETE CASCADE` keys
/// have nothing left to take: a child's `ParentId` and `TopParentId`, and for
/// each item below `old`, its ancestors become the location, the root and the
/// library, as `GetAncestorIds` gives them to an item of that library. A
/// child that carries no ancestor row, which a box set an older Ferrofin
/// created does not, gets them too. An image of a type the library has no
/// image of moves to it; one it already has goes with `old`.
pub(crate) async fn merge_into_location(
    db: &Database,
    old: Uuid,
    target: LocationTarget,
) -> Result<u64, ServiceError> {
    let old_db = guid_to_db(old);
    let (folder, library) = (guid_to_db(target.folder), guid_to_db(target.library));
    // A boot repair: the previous version may be writing (CONTRIBUTING.md).
    let mut tx = db
        .writer()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(db_err)?;
    for ancestor in [target.folder, target.aggregate, target.library] {
        sqlx::query(
            r#"INSERT OR IGNORE INTO "AncestorIds" ("ItemId", "ParentItemId")
               SELECT below."Id", ?2 FROM (
                   SELECT "Id" FROM "BaseItems" WHERE "ParentId" = ?1
                   UNION SELECT "ItemId" FROM "AncestorIds" WHERE "ParentItemId" = ?1
               ) below WHERE below."Id" <> ?2"#,
        )
        .bind(&old_db)
        .bind(guid_to_db(ancestor))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    let moved = sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "ParentId" = ?1"#)
        .bind(&old_db)
        .bind(&folder)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
    sqlx::query(r#"UPDATE "BaseItems" SET "TopParentId" = ?2 WHERE "TopParentId" = ?1"#)
        .bind(&old_db)
        .bind(&folder)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    sqlx::query(r#"DELETE FROM "AncestorIds" WHERE "ParentItemId" = ?1 OR "ItemId" = ?1"#)
        .bind(&old_db)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    move_user_data(&mut tx, &old_db, &library).await?;
    sqlx::query(
        r#"UPDATE "BaseItemImageInfos" SET "ItemId" = ?2
           WHERE "ItemId" = ?1 AND "ImageType" NOT IN
               (SELECT "ImageType" FROM "BaseItemImageInfos" WHERE "ItemId" = ?2)"#,
    )
    .bind(&old_db)
    .bind(&library)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(&old_db)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    db.invalidate_libraries();
    Ok(moved)
}

/// Test-only: the ancestors stored for `item`, sorted.
#[cfg(test)]
pub(crate) async fn ancestors_of(db: &Database, item: Uuid) -> Vec<String> {
    sqlx::query_scalar(r#"SELECT "ParentItemId" FROM "AncestorIds" WHERE "ItemId" = ?1 ORDER BY 1"#)
        .bind(guid_to_db(item))
        .fetch_all(db.pool())
        .await
        .expect("ancestors")
}

/// Test-only: makes every delete of a `BaseItems` row fail while `on`.
#[cfg(test)]
pub(crate) async fn fail_item_deletes(db: &Database, on: bool) {
    let sql = if on {
        r#"CREATE TRIGGER "fail_delete" BEFORE DELETE ON "BaseItems"
           BEGIN SELECT RAISE(ABORT, 'disk full'); END"#
    } else {
        r#"DROP TRIGGER "fail_delete""#
    };
    sqlx::query(sql)
        .execute(db.writer())
        .await
        .expect("trigger");
}

/// Test-only: the items `user` has user data on.
#[cfg(test)]
pub(crate) async fn user_data_items(db: &Database, user: Uuid) -> Vec<String> {
    sqlx::query_scalar(r#"SELECT "ItemId" FROM "UserData" WHERE "UserId" = ?1 ORDER BY 1"#)
        .bind(guid_to_db(user))
        .fetch_all(db.pool())
        .await
        .expect("user data")
}
