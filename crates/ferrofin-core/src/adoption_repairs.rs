//! One-shot boot repairs that port Jellyfin 12.0's code migrations for the
//! data a schema migration cannot compute — the routines under
//! `Jellyfin.Server/Migrations/Routines/2026*` that touch linked children,
//! extras and version groups.
//!
//! Each repair is keyed in `FerrofinMeta` (or on the adoption record) and runs
//! once per database; a second boot is a no-op. They run on every path — a
//! fresh database, an upgraded install, an adopted 10.11.8 or 12.0 database —
//! and are written so a database that is already in the 12.0 state is left
//! alone (an adopted 12.0 database went through the real routines in Jellyfin).

use std::path::Path;

use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

use uuid::Uuid;

use crate::db_error::db_err;
use crate::item_data::{import_membership, parse_data, read_linked_children};
use crate::item_persistence_service::alternate_version_child_type;
use crate::item_type_lookup::stored_type_name;

/// `LinkedChildType::LocalAlternateVersion`.
const LOCAL_ALTERNATE_VERSION: i64 = 2;
/// `LinkedChildType::LinkedAlternateVersion`.
const LINKED_ALTERNATE_VERSION: i64 = 3;
/// The placeholder item 12.0's `AddForeignKeyToOwnerId` re-points dangling
/// owners at, and `CleanupOrphanedExtras` then deletes the owned rows of.
const PLACEHOLDER_ID: &str = "00000000-0000-0000-0000-000000000001";

/// Runs every repair in this module, in upstream's order.
///
/// # Errors
/// Returns [`ServiceError`] if a repair's queries fail.
pub async fn run_all(db: &Database) -> Result<(), ServiceError> {
    let imported = import_membership_once(db).await?;
    if imported > 0 {
        tracing::info!(
            rows = imported,
            "imported playlist/collection/version membership from Data JSON"
        );
    }
    let removed = cleanup_orphaned_extras(db).await?;
    if removed > 0 {
        tracing::info!(
            items = removed,
            "removed extras whose owner no longer exists"
        );
    }
    let fixed = fix_owner_id_relationships(db).await?;
    if fixed > 0 {
        tracing::info!(items = fixed, "repaired OwnerId relationships");
    }
    let linked = backfill_alternate_version_links(db).await?;
    if linked > 0 {
        tracing::info!(
            rows = linked,
            "backfilled alternate-version links from PrimaryVersionId"
        );
    }
    let (repaired, promoted) = repair_alternate_version_links(db).await?;
    if repaired + promoted > 0 {
        tracing::info!(
            repaired,
            promoted,
            "repaired alternate-version primaries from LinkedChildren"
        );
    }
    let artists = merge_duplicate_music_artists(db).await?;
    if artists > 0 {
        tracing::info!(items = artists, "merged case-only duplicate music artists");
    }
    let people = merge_duplicate_people(db).await?;
    if people > 0 {
        tracing::info!(items = people, "merged case-only duplicate people");
    }
    let stripped = strip_embedded_linked_children(db).await?;
    if stripped > 0 {
        tracing::info!(
            items = stripped,
            "dropped dead LinkedChildren/ExtraIds keys from serialized item data"
        );
    }
    Ok(())
}

/// A `ferrofin-db` error on the bookkeeping tables, as a service error.
fn meta_err(err: impl std::fmt::Display) -> ServiceError {
    ServiceError::Backend(err.to_string())
}

/// Whether `key` still has to run.
async fn once(db: &Database, key: &str) -> Result<bool, ServiceError> {
    if db.meta_get(key).await.map_err(meta_err)?.is_some() {
        return Ok(false);
    }
    Ok(true)
}

async fn done(db: &Database, key: &str) -> Result<(), ServiceError> {
    db.meta_set(key, "1").await.map_err(meta_err)
}

/// The one-shot port of `MigrateLinkedChildren`'s data move for a database
/// **newly adopted from 10.11.8** — the only case where the `Data` JSON is the
/// truth about membership. It reads `LinkedChildren` from every playlist/box
/// set and `LocalAlternateVersions` / `LinkedAlternateVersions` from every
/// video, writes the rows, and flips the adoption record's flag in the same
/// transaction. Runs for every 10.11.x generation (10.11.8 through
/// 10.11.11). A 12.0 adoption, a Ferrofin-native database and an install
/// adopted before the record existed have no such record and are skipped.
///
/// Returns the number of rows written.
///
/// # Errors
/// Returns [`ServiceError`] if the underlying queries fail.
pub async fn import_membership_once(db: &Database) -> Result<usize, ServiceError> {
    let Some(state) = db.adoption_state().await.map_err(meta_err)? else {
        return Ok(0);
    };
    // Every 10.11.x release keeps membership only in `Data` JSON; 12.0's
    // `LinkedChildren` rows are the store and its JSON is frozen.
    if !state.generation.starts_with("10.11.") || state.membership_import_done {
        return Ok(0);
    }
    let playlist = stored_type_name(BaseItemKind::Playlist).unwrap_or_default();
    let boxset = stored_type_name(BaseItemKind::BoxSet).unwrap_or_default();
    let video_types = [
        stored_type_name(BaseItemKind::Video).unwrap_or_default(),
        stored_type_name(BaseItemKind::Movie).unwrap_or_default(),
        stored_type_name(BaseItemKind::Episode).unwrap_or_default(),
    ];
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT "Id", "Type", "Path", "Data" FROM "BaseItems"
           WHERE "Data" IS NOT NULL AND "Type" IN (?1, ?2, ?3, ?4, ?5)"#,
    )
    .bind(playlist)
    .bind(boxset)
    .bind(video_types[0])
    .bind(video_types[1])
    .bind(video_types[2])
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;

    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let mut written = 0usize;
    for (id, type_, path, data) in rows {
        let map = parse_data(data.as_deref());
        if type_ == playlist || type_ == boxset {
            if map.contains_key("LinkedChildren") {
                written += import_membership(&mut tx, &id, &map).await?;
            }
        } else {
            written += import_video_alternate_versions(&mut tx, &id, path.as_deref(), &map).await?;
        }
    }
    sqlx::query(r#"UPDATE "FerrofinAdoption" SET "MembershipImportDone" = 1"#)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    Ok(written)
}

/// `MigrateLinkedChildren.ProcessVideoAlternateVersions`: `LocalAlternateVersions`
/// (path strings → `ChildType 2`) then `LinkedAlternateVersions` (linked-child
/// objects → `ChildType 3`), one shared ordinal counter; a pair already
/// present is left alone (local beats linked), a linked version pointing at an
/// item the parent owns is skipped.
async fn import_video_alternate_versions(
    tx: &mut sqlx::SqliteConnection,
    parent_db: &str,
    _parent_path: Option<&str>,
    map: &Map<String, Value>,
) -> Result<usize, ServiceError> {
    let mut candidates: Vec<(String, i64)> = Vec::new();
    if let Some(paths) = map.get("LocalAlternateVersions").and_then(Value::as_array) {
        for path in paths.iter().filter_map(Value::as_str) {
            if path.is_empty() {
                continue;
            }
            if let Some(child) = id_by_path(tx, path).await? {
                candidates.push((child, LOCAL_ALTERNATE_VERSION));
            } else {
                tracing::warn!(
                    path,
                    parent = parent_db,
                    "could not resolve LocalAlternateVersion path"
                );
            }
        }
    }
    let mut linked_map = Map::new();
    if let Some(v) = map.get("LinkedAlternateVersions") {
        linked_map.insert("LinkedChildren".to_owned(), v.clone());
    }
    for child in read_linked_children(&linked_map) {
        let resolved = match child
            .item_id
            .as_deref()
            .and_then(|i| Uuid::parse_str(i).ok())
        {
            Some(id) => Some(guid_to_db(id)),
            None => match child.path.as_deref() {
                Some(p) => id_by_path(tx, p).await?,
                None => None,
            },
        };
        if let Some(child_db) = resolved {
            candidates.push((child_db, LINKED_ALTERNATE_VERSION));
        } else {
            tracing::warn!(
                parent = parent_db,
                "could not resolve LinkedAlternateVersion child"
            );
        }
    }
    let mut written = 0usize;
    for (child_db, child_type) in candidates {
        if child_type == LINKED_ALTERNATE_VERSION {
            let owned: Option<i64> = sqlx::query_scalar(
                r#"SELECT 1 FROM "BaseItems" WHERE "Id" = ?1 AND "OwnerId" = ?2"#,
            )
            .bind(&child_db)
            .bind(parent_db)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            if owned.is_some() {
                continue;
            }
        }
        let exists: Option<i64> = sqlx::query_scalar(
            r#"SELECT 1 FROM "LinkedChildren" WHERE "ParentId" = ?1 AND "ChildId" = ?2"#,
        )
        .bind(parent_db)
        .bind(&child_db)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        if exists.is_some() {
            continue;
        }
        let child_exists: Option<i64> =
            sqlx::query_scalar(r#"SELECT 1 FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&child_db)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        if child_exists.is_none() {
            continue;
        }
        append_link(tx, parent_db, &child_db, child_type).await?;
        written += 1;
    }
    Ok(written)
}

async fn id_by_path(
    tx: &mut sqlx::SqliteConnection,
    path: &str,
) -> Result<Option<String>, ServiceError> {
    sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Path" = ?1 ORDER BY "Id" LIMIT 1"#)
        .bind(path)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)
}

async fn append_link(
    tx: &mut sqlx::SqliteConnection,
    parent_db: &str,
    child_db: &str,
    child_type: i64,
) -> Result<(), ServiceError> {
    sqlx::query(
        r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
           VALUES (?1,
               (SELECT COALESCE(MAX("SortOrder"), -1) + 1 FROM "LinkedChildren" WHERE "ParentId" = ?1),
               ?2, ?3)"#,
    )
    .bind(parent_db)
    .bind(child_db)
    .bind(child_type)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// `CleanupOrphanedExtras`: delete every item whose `OwnerId` is the
/// placeholder — where 0032 (`AddForeignKeyToOwnerId`) re-pointed owners that
/// no longer exist. Links are cleared first; the rows' children cascade.
///
/// # Errors
/// Returns [`ServiceError`] if the underlying queries fail.
pub async fn cleanup_orphaned_extras(db: &Database) -> Result<usize, ServiceError> {
    const KEY: &str = "cleanup_orphaned_extras_v12";
    if !once(db, KEY).await? {
        return Ok(0);
    }
    let ids: Vec<String> =
        sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "OwnerId" = ?1 AND "Id" <> ?1"#)
            .bind(PLACEHOLDER_ID)
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    for id in &ids {
        sqlx::query(r#"DELETE FROM "LinkedChildren" WHERE "ParentId" = ?1 OR "ChildId" = ?1"#)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
    }
    tx.commit().await.map_err(db_err)?;
    done(db, KEY).await?;
    Ok(ids.len())
}

/// `FixIncorrectOwnerIdRelationships` steps 1–4. Step 1: items sharing a
/// `Path` are de-duplicated — the keeper has direct children, else owns
/// extras, else is not a plain `Folder`, else is the newest; the rest are
/// deleted. Step 2: a video/movie that is not an extra but has an owner that
/// is itself a video/movie, or no longer exists, loses its `OwnerId`. Step 3:
/// an orphaned extra is re-attached to the first video/movie whose path
/// starts with the extra's directory, else loses its owner. Step 4: every
/// version link's child gets `PrimaryVersionId = ParentId`.
///
/// # Errors
/// Returns [`ServiceError`] if the underlying queries fail.
pub async fn fix_owner_id_relationships(db: &Database) -> Result<usize, ServiceError> {
    const KEY: &str = "owner_id_relationships_v12";
    if !once(db, KEY).await? {
        return Ok(0);
    }
    let video = stored_type_name(BaseItemKind::Video).unwrap_or_default();
    let movie = stored_type_name(BaseItemKind::Movie).unwrap_or_default();
    let folder = stored_type_name(BaseItemKind::Folder).unwrap_or_default();
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let deleted = dedupe_paths(&mut tx, folder).await?;
    // Step 2: videos/movies (not extras) owned by a video/movie or by nothing.
    let cleared = sqlx::query(
        r#"UPDATE "BaseItems" SET "OwnerId" = NULL
           WHERE "OwnerId" IS NOT NULL
             AND ("ExtraType" IS NULL OR "ExtraType" = 0)
             AND "Type" IN (?1, ?2)
             AND (
               "OwnerId" NOT IN (SELECT "Id" FROM "BaseItems")
               OR "OwnerId" IN (SELECT "Id" FROM "BaseItems" WHERE "Type" IN (?1, ?2))
             )"#,
    )
    .bind(video)
    .bind(movie)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?
    .rows_affected();
    // Step 3: orphaned extras re-attached by directory, else unowned.
    let orphans: Vec<(String, Option<String>)> = sqlx::query_as(
        r#"SELECT "Id", "Path" FROM "BaseItems"
           WHERE "ExtraType" IS NOT NULL AND "ExtraType" <> 0 AND "OwnerId" IS NOT NULL
             AND "OwnerId" NOT IN (SELECT "Id" FROM "BaseItems")"#,
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let mut reassigned = 0usize;
    for (id, path) in orphans {
        let Some(path) = path else { continue };
        let Some(dir) = Path::new(&path)
            .parent()
            .map(|d| d.to_string_lossy().into_owned())
        else {
            continue;
        };
        let parent: Option<String> = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems"
               WHERE "Type" IN (?1, ?2) AND "Path" IS NOT NULL AND "Path" LIKE ?3 || '%'
               ORDER BY "Id" LIMIT 1"#,
        )
        .bind(video)
        .bind(movie)
        .bind(&dir)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(r#"UPDATE "BaseItems" SET "OwnerId" = ?2 WHERE "Id" = ?1"#)
            .bind(&id)
            .bind(parent)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        reassigned += 1;
    }
    // Step 4: the pointer follows the link.
    let pointed = sqlx::query(
        r#"UPDATE "BaseItems" SET "PrimaryVersionId" = (
               SELECT lc."ParentId" FROM "LinkedChildren" lc
               WHERE lc."ChildId" = "BaseItems"."Id" AND lc."ChildType" IN (2, 3)
               ORDER BY lc."ParentId", lc."SortOrder" LIMIT 1)
           WHERE "Id" IN (SELECT "ChildId" FROM "LinkedChildren" WHERE "ChildType" IN (2, 3))
             AND ("PrimaryVersionId" IS NULL OR "PrimaryVersionId" NOT IN (
               SELECT lc."ParentId" FROM "LinkedChildren" lc
               WHERE lc."ChildId" = "BaseItems"."Id" AND lc."ChildType" IN (2, 3)))"#,
    )
    .execute(&mut *tx)
    .await
    .map_err(db_err)?
    .rows_affected();
    tx.commit().await.map_err(db_err)?;
    done(db, KEY).await?;
    Ok(usize::try_from(cleared + pointed).unwrap_or(usize::MAX) + reassigned + deleted)
}

/// `FixIncorrectOwnerIdRelationships` step 1: items sharing a `Path`.
async fn dedupe_paths(
    tx: &mut sqlx::SqliteConnection,
    folder: &str,
) -> Result<usize, ServiceError> {
    // Step 1: duplicate paths. `(has_children, has_extras, not_folder, created)`
    // sorts the keeper first; the query returns rows newest-first so the
    // last tiebreak is already in order.
    let dups: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT b."Path", b."Id", b."Type", b."DateCreated" FROM "BaseItems" b
           WHERE b."Path" IS NOT NULL
             AND b."Path" IN (SELECT "Path" FROM "BaseItems" WHERE "Path" IS NOT NULL
                              GROUP BY "Path" HAVING COUNT(*) > 1)
           ORDER BY b."Path", b."DateCreated" DESC, b."Id""#,
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let mut deleted = 0usize;
    let mut by_path: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for (path, id, type_, _) in dups {
        match by_path.last_mut() {
            Some((p, rows)) if *p == path => rows.push((id, type_)),
            _ => by_path.push((path, vec![(id, type_)])),
        }
    }
    for (_, rows) in by_path {
        let mut ranked: Vec<(u8, u8, u8, usize, String)> = Vec::new();
        for (index, (id, type_)) in rows.iter().enumerate() {
            let children: i64 =
                sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "ParentId" = ?1"#)
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db_err)?;
            let extras: i64 =
                sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "OwnerId" = ?1"#)
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db_err)?;
            ranked.push((
                u8::from(children > 0),
                u8::from(extras > 0),
                u8::from(*type_ != folder),
                index,
                id.clone(),
            ));
        }
        // Highest flags win; equal flags → lowest index (newest DateCreated).
        ranked.sort_by(|a, b| (b.0, b.1, b.2, a.3).cmp(&(a.0, a.1, a.2, b.3)));
        for (_, _, _, _, id) in ranked.iter().skip(1) {
            sqlx::query(r#"DELETE FROM "LinkedChildren" WHERE "ParentId" = ?1 OR "ChildId" = ?1"#)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            sqlx::query(r#"UPDATE "BaseItems" SET "OwnerId" = NULL WHERE "OwnerId" = ?1"#)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Ferrofin modelled a version group by `PrimaryVersionId` alone; 12.0 reads
/// versions from `LinkedChildren`. Write the missing rows for every existing
/// group: local (2) when the two files share a directory, linked (3) otherwise.
/// Rows already present (an adopted 12.0 database) are left alone.
///
/// # Errors
/// Returns [`ServiceError`] if the underlying queries fail.
pub async fn backfill_alternate_version_links(db: &Database) -> Result<usize, ServiceError> {
    const KEY: &str = "alternate_version_links_v12";
    if !once(db, KEY).await? {
        return Ok(0);
    }
    let pairs: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT a."Id", a."PrimaryVersionId" FROM "BaseItems" a
           WHERE a."PrimaryVersionId" IS NOT NULL
             AND a."PrimaryVersionId" IN (SELECT "Id" FROM "BaseItems")
             AND NOT EXISTS (SELECT 1 FROM "LinkedChildren" lc
                             WHERE lc."ParentId" = a."PrimaryVersionId" AND lc."ChildId" = a."Id"
                               AND lc."ChildType" IN (2, 3))
           ORDER BY a."PrimaryVersionId", a."Id""#,
    )
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let mut written = 0usize;
    for (child, primary) in &pairs {
        let (Ok(child_id), Ok(primary_id)) = (Uuid::parse_str(child), Uuid::parse_str(primary))
        else {
            continue;
        };
        let child_type = alternate_version_child_type(&mut tx, child_id, primary_id).await?;
        append_link(&mut tx, primary, child, child_type).await?;
        written += 1;
    }
    tx.commit().await.map_err(db_err)?;
    done(db, KEY).await?;
    Ok(written)
}

/// The port of 12.1's `RepairAlternateVersionLinks`: `LinkedChildren` rows
/// of type Local(2)/Linked(3) are the truth about version groups, so every
/// child's `PrimaryVersionId` and `PresentationUniqueKey` are re-derived from
/// them. A child linked under a parent that is itself a version follows the
/// chain to the root (a loop keeps its lowest id, .NET `Guid` order, as the
/// primary); a self-link is skipped; a primary that still carries a
/// `PrimaryVersionId` of its own is promoted (`PrimaryVersionId` cleared,
/// key = its own id) unless it is owned, in which case the group stays hidden
/// until `FixIncorrectOwnerIdRelationships` repairs the owner. Runs once per
/// database (`FerrofinMeta`), after [`backfill_alternate_version_links`] has
/// written Ferrofin's own groups into `LinkedChildren`.
///
/// Returns `(repaired children, promoted primaries)`.
///
/// # Errors
/// Returns [`ServiceError`] if the underlying queries fail.
pub async fn repair_alternate_version_links(db: &Database) -> Result<(usize, usize), ServiceError> {
    const KEY: &str = "alternate_version_links_repaired_v121";
    if !once(db, KEY).await? {
        return Ok((0, 0));
    }
    let links: Vec<(String, String, i64)> = sqlx::query_as(
        r#"SELECT "ParentId", "ChildId", "ChildType" FROM "LinkedChildren"
           WHERE "ChildType" IN (2, 3) ORDER BY "ChildId", "ChildType", "ParentId""#,
    )
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    // Local(2) beats Linked(3) for a child under two parents — the ORDER BY
    // puts it first, so the first row per child wins.
    let mut primary_by_child: BTreeMap<Uuid, Uuid> = BTreeMap::new();
    for (parent, child, _) in &links {
        let (Ok(parent), Ok(child)) = (Uuid::parse_str(parent), Uuid::parse_str(child)) else {
            continue;
        };
        primary_by_child.entry(child).or_insert(parent);
    }
    for child in primary_by_child
        .iter()
        .filter(|(child, parent)| child == parent)
        .map(|(child, _)| *child)
        .collect::<Vec<_>>()
    {
        tracing::warn!(child_id = %child, "skipping alternate version linked to itself");
        primary_by_child.remove(&child);
    }
    resolve_primaries(&mut primary_by_child);

    let mut item_ids: Vec<Uuid> = primary_by_child
        .iter()
        .flat_map(|(child, primary)| [*child, *primary])
        .collect();
    item_ids.sort_unstable();
    item_ids.dedup();
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let (mut repaired, mut promoted) = (0usize, 0usize);
    for id in item_ids {
        let Some((primary_version_id, key, owner_id)) =
            sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
                r#"SELECT "PrimaryVersionId", "PresentationUniqueKey", "OwnerId"
                   FROM "BaseItems" WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
        else {
            continue;
        };
        let stored_primary = primary_version_id
            .as_deref()
            .and_then(|p| Uuid::parse_str(p).ok());
        if let Some(primary) = primary_by_child.get(&id) {
            let expected_key = primary.as_simple().to_string();
            if stored_primary == Some(*primary) && key.as_deref() == Some(expected_key.as_str()) {
                continue;
            }
            set_primary(&mut tx, id, Some(*primary), &expected_key).await?;
            repaired += 1;
        } else if stored_primary.is_some() {
            if owner_id.is_some() {
                tracing::warn!(
                    item_id = %id,
                    owner_id = ?owner_id,
                    "alternate versions are linked to an owned item; the group stays hidden until the owner is repaired"
                );
                continue;
            }
            tracing::warn!(
                item_id = %id,
                stale_primary = ?stored_primary,
                "clearing the stale primary of an item other versions are linked to"
            );
            set_primary(&mut tx, id, None, &id.as_simple().to_string()).await?;
            promoted += 1;
        }
    }
    sqlx::query(
        r#"INSERT INTO "FerrofinMeta" ("Key", "Value") VALUES (?1, '1')
           ON CONFLICT("Key") DO UPDATE SET "Value" = '1'"#,
    )
    .bind(KEY)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    Ok((repaired, promoted))
}

async fn set_primary(
    tx: &mut sqlx::SqliteConnection,
    id: Uuid,
    primary: Option<Uuid>,
    key: &str,
) -> Result<(), ServiceError> {
    sqlx::query(
        r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?2, "PresentationUniqueKey" = ?3
           WHERE "Id" = ?1"#,
    )
    .bind(guid_to_db(id))
    .bind(primary.map(guid_to_db))
    .bind(key)
    .execute(tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// `RepairAlternateVersionLinks.ResolvePrimaries`: every child maps to the
/// root of its chain; a loop keeps its smallest id (.NET `Guid` order) as the
/// primary and drops that id's own link.
fn resolve_primaries(primary_by_child: &mut BTreeMap<Uuid, Uuid>) {
    let mut resolved: BTreeMap<Uuid, Uuid> = BTreeMap::new();
    for start in primary_by_child.keys().copied().collect::<Vec<_>>() {
        if resolved.contains_key(&start) {
            continue;
        }
        let mut chain: Vec<Uuid> = Vec::new();
        let mut walked: BTreeSet<Uuid> = BTreeSet::new();
        let mut current = start;
        let primary = loop {
            if let Some(done) = resolved.get(&current) {
                break *done;
            }
            let Some(next) = primary_by_child.get(&current).copied() else {
                break current;
            };
            if !walked.insert(current) {
                let at = chain.iter().position(|c| *c == current).unwrap_or(0);
                let primary = chain[at..]
                    .iter()
                    .copied()
                    .min_by(|a, b| dotnet_guid_cmp(*a, *b))
                    .unwrap_or(current);
                tracing::warn!(
                    loop_ = ?chain[at..],
                    primary_id = %primary,
                    "alternate version links form a loop; keeping the lowest id as the primary"
                );
                primary_by_child.remove(&primary);
                break primary;
            }
            chain.push(current);
            current = next;
        };
        for version in chain {
            resolved.insert(version, primary);
        }
    }
    for (child, primary) in primary_by_child.iter_mut() {
        if let Some(root) = resolved.get(child)
            && root != child
        {
            *primary = *root;
        }
    }
}

/// `System.Guid.CompareTo`: the `int`, `short`, `short` fields as signed
/// numbers, then the eight tail bytes — not the byte order of the string.
fn dotnet_guid_cmp(a: Uuid, b: Uuid) -> std::cmp::Ordering {
    let (a1, a2, a3, a4) = a.as_fields();
    let (b1, b2, b3, b4) = b.as_fields();
    (a1.cast_signed(), a2.cast_signed(), a3.cast_signed(), a4).cmp(&(
        b1.cast_signed(),
        b2.cast_signed(),
        b3.cast_signed(),
        b4,
    ))
}

/// The port of 12.1's `StripEmbeddedLinkedChildren`: `LinkedChildren`,
/// `ExtraIds` and `SupportsExternalTransfer` are dead keys in the serialized
/// `Data` once `LinkedChildren` (the table) is the store, so they are removed
/// from every item that still carries them. Runs once, and after
/// [`import_membership_once`], which is the last reader of that JSON.
///
/// # Errors
/// Returns [`ServiceError`] if the update fails.
pub async fn strip_embedded_linked_children(db: &Database) -> Result<u64, ServiceError> {
    const KEY: &str = "strip_embedded_linked_children_v121";
    if !once(db, KEY).await? {
        return Ok(0);
    }
    let updated = sqlx::query(
        r#"UPDATE "BaseItems"
           SET "Data" = json_remove("Data", '$.LinkedChildren', '$.ExtraIds', '$.SupportsExternalTransfer')
           WHERE "Data" IS NOT NULL
             AND json_valid("Data") = 1
             AND ("Data" LIKE '%"LinkedChildren"%'
               OR "Data" LIKE '%"ExtraIds"%'
               OR "Data" LIKE '%"SupportsExternalTransfer"%')"#,
    )
    .execute(db.writer())
    .await
    .map_err(db_err)?
    .rows_affected();
    done(db, KEY).await?;
    Ok(updated)
}

/// `MergeDuplicateMusicArtists` (12.0): `MusicArtist` rows whose names differ
/// only by case are folded onto one keeper — the one with the most direct
/// children, then ancestor rows, then links, then the oldest — and every
/// reference (`ParentId`, `OwnerId`, `AncestorIds`, `LinkedChildren` both
/// ways, `UserData`, keeper's row winning any collision) is re-pointed before
/// the duplicates are deleted. Case-only means `ToLowerInvariant`: no
/// diacritic folding, no trimming.
///
/// # Errors
/// Returns [`ServiceError`] if the underlying queries fail.
pub async fn merge_duplicate_music_artists(db: &Database) -> Result<usize, ServiceError> {
    const KEY: &str = "merge_duplicate_music_artists_v12";
    if !once(db, KEY).await? {
        return Ok(0);
    }
    let type_name = stored_type_name(BaseItemKind::MusicArtist).unwrap_or_default();
    let merged = merge_case_duplicates(db, type_name, KeeperRule::Artist).await?;
    done(db, KEY).await?;
    Ok(merged)
}

/// `MergeDuplicatePeople` (12.0): the same fold for `Person` rows (keeper:
/// most user data, then links, then oldest), then the `Peoples` lookup table
/// grouped by `(lower(Name), PersonType)` — the row with the most
/// `PeopleBaseItemMap` entries (tie: lowest `Id`) keeps them, colliding
/// `(ItemId, Role)` map rows are dropped, the rest re-pointed, duplicates deleted.
///
/// # Errors
/// Returns [`ServiceError`] if the underlying queries fail.
pub async fn merge_duplicate_people(db: &Database) -> Result<usize, ServiceError> {
    const KEY: &str = "merge_duplicate_people_v12";
    if !once(db, KEY).await? {
        return Ok(0);
    }
    let type_name = stored_type_name(BaseItemKind::Person).unwrap_or_default();
    let mut merged = merge_case_duplicates(db, type_name, KeeperRule::Person).await?;
    merged += merge_peoples_rows(db).await?;
    done(db, KEY).await?;
    Ok(merged)
}

/// Which counts pick the keeper of a duplicate group.
#[derive(Clone, Copy)]
enum KeeperRule {
    /// `ChildCount desc, AncestorCount desc, LinkedCount desc, DateCreated asc`.
    Artist,
    /// `UserDataCount desc, LinkedCount desc, DateCreated asc`.
    Person,
}

/// One candidate of a duplicate group, with the counts the keeper rule reads.
struct Candidate {
    id: String,
    created: Option<String>,
    children: i64,
    ancestors: i64,
    linked: i64,
    user_data: i64,
}

async fn count(tx: &mut sqlx::SqliteConnection, sql: &str, id: &str) -> Result<i64, ServiceError> {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)
}

/// Folds every case-only duplicate group of `type_name` onto its keeper.
/// Returns the number of rows deleted.
async fn merge_case_duplicates(
    db: &Database,
    type_name: &str,
    rule: KeeperRule,
) -> Result<usize, ServiceError> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT "Id", "Name", "DateCreated" FROM "BaseItems"
           WHERE "Type" = ?1 AND "Name" IS NOT NULL ORDER BY "Id""#,
    )
    .bind(type_name)
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    let mut groups: std::collections::BTreeMap<String, Vec<(String, Option<String>)>> =
        std::collections::BTreeMap::new();
    for (id, name, created) in rows {
        groups
            .entry(name.to_lowercase())
            .or_default()
            .push((id, created));
    }
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let mut deleted = 0usize;
    for members in groups.into_values().filter(|g| g.len() > 1) {
        let mut candidates = Vec::with_capacity(members.len());
        for (id, created) in members {
            candidates.push(Candidate {
                children: count(
                    &mut tx,
                    r#"SELECT COUNT(*) FROM "BaseItems" WHERE "ParentId" = ?1"#,
                    &id,
                )
                .await?,
                ancestors: count(
                    &mut tx,
                    r#"SELECT COUNT(*) FROM "AncestorIds" WHERE "ParentItemId" = ?1"#,
                    &id,
                )
                .await?,
                linked: count(
                    &mut tx,
                    r#"SELECT COUNT(*) FROM "LinkedChildren" WHERE "ParentId" = ?1 OR "ChildId" = ?1"#,
                    &id,
                )
                .await?,
                user_data: count(
                    &mut tx,
                    r#"SELECT COUNT(*) FROM "UserData" WHERE "ItemId" = ?1"#,
                    &id,
                )
                .await?,
                id,
                created,
            });
        }
        // Descending on the counts, ascending on DateCreated (oldest wins).
        candidates.sort_by(|a, b| {
            let key = |c: &Candidate| match rule {
                KeeperRule::Artist => (c.children, c.ancestors, c.linked, 0),
                KeeperRule::Person => (c.user_data, c.linked, 0, 0),
            };
            key(b).cmp(&key(a)).then_with(|| a.created.cmp(&b.created))
        });
        let keeper = candidates[0].id.clone();
        for dup in candidates.iter().skip(1) {
            repoint_references(&mut tx, &dup.id, &keeper).await?;
            sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&dup.id)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            deleted += 1;
        }
    }
    tx.commit().await.map_err(db_err)?;
    Ok(deleted)
}

/// Re-points every reference from `dup` to `keeper`, the keeper's own rows
/// winning any collision (the exact rewrite set of both 12.0 routines).
async fn repoint_references(
    tx: &mut sqlx::SqliteConnection,
    dup: &str,
    keeper: &str,
) -> Result<(), ServiceError> {
    let statements = [
        r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "ParentId" = ?1"#,
        r#"UPDATE "BaseItems" SET "OwnerId" = ?2 WHERE "OwnerId" = ?1"#,
        r#"DELETE FROM "AncestorIds" WHERE "ParentItemId" = ?1
             AND "ItemId" IN (SELECT "ItemId" FROM "AncestorIds" WHERE "ParentItemId" = ?2)"#,
        r#"UPDATE "AncestorIds" SET "ParentItemId" = ?2 WHERE "ParentItemId" = ?1"#,
        r#"DELETE FROM "LinkedChildren" WHERE "ParentId" = ?1
             AND "ChildId" IN (SELECT "ChildId" FROM "LinkedChildren" WHERE "ParentId" = ?2)"#,
        r#"UPDATE "LinkedChildren" SET "ParentId" = ?2,
             "SortOrder" = "SortOrder" + (SELECT COALESCE(MAX("SortOrder"), -1) + 1
                                          FROM "LinkedChildren" WHERE "ParentId" = ?2)
           WHERE "ParentId" = ?1"#,
        r#"DELETE FROM "LinkedChildren" WHERE "ChildId" = ?1
             AND "ParentId" IN (SELECT "ParentId" FROM "LinkedChildren" WHERE "ChildId" = ?2)"#,
        r#"UPDATE "LinkedChildren" SET "ChildId" = ?2 WHERE "ChildId" = ?1"#,
        r#"DELETE FROM "UserData" WHERE "ItemId" = ?1
             AND EXISTS (SELECT 1 FROM "UserData" k WHERE k."ItemId" = ?2
                         AND k."UserId" = "UserData"."UserId"
                         AND k."CustomDataKey" IS "UserData"."CustomDataKey")"#,
        r#"UPDATE "UserData" SET "ItemId" = ?2 WHERE "ItemId" = ?1"#,
    ];
    for sql in statements {
        sqlx::query(sql)
            .bind(dup)
            .bind(keeper)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
    }
    Ok(())
}

/// `Peoples` rows grouped by `(lower(Name), PersonType)`: `(Id, map-row count)`.
type PeopleGroups = std::collections::BTreeMap<(String, Option<String>), Vec<(String, i64)>>;

/// `MergePeoplesRowsAsync`: the `Peoples` lookup table, grouped by
/// `(lower(Name), PersonType)`.
async fn merge_peoples_rows(db: &Database) -> Result<usize, ServiceError> {
    let rows: Vec<(String, String, Option<String>, i64)> = sqlx::query_as(
        r#"SELECT p."Id", p."Name", p."PersonType",
                  (SELECT COUNT(*) FROM "PeopleBaseItemMap" m WHERE m."PeopleId" = p."Id")
           FROM "Peoples" p ORDER BY p."Id""#,
    )
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    let mut groups: PeopleGroups = std::collections::BTreeMap::new();
    for (id, name, person_type, maps) in rows {
        groups
            .entry((name.to_lowercase(), person_type))
            .or_default()
            .push((id, maps));
    }
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let mut deleted = 0usize;
    for mut members in groups.into_values().filter(|g| g.len() > 1) {
        // Most map rows keeps them; tie → lowest Id (the input is Id-ordered).
        members.sort_by_key(|(_, maps)| std::cmp::Reverse(*maps));
        let keeper = members[0].0.clone();
        for (dup, _) in members.iter().skip(1) {
            sqlx::query(
                r#"DELETE FROM "PeopleBaseItemMap" WHERE "PeopleId" = ?1
                   AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" k WHERE k."PeopleId" = ?2
                               AND k."ItemId" = "PeopleBaseItemMap"."ItemId"
                               AND k."Role" IS "PeopleBaseItemMap"."Role")"#,
            )
            .bind(dup)
            .bind(&keeper)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            sqlx::query(r#"UPDATE "PeopleBaseItemMap" SET "PeopleId" = ?2 WHERE "PeopleId" = ?1"#)
                .bind(dup)
                .bind(&keeper)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            sqlx::query(r#"DELETE FROM "Peoples" WHERE "Id" = ?1"#)
                .bind(dup)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            deleted += 1;
        }
    }
    tx.commit().await.map_err(db_err)?;
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::data::BaseItemKind;
    use uuid::Uuid;

    use super::*;
    use crate::test_support::{seed_item, seed_named_item, test_db};

    async fn set_path(db: &Database, id: Uuid, path: &str) {
        sqlx::query(r#"UPDATE "BaseItems" SET "Path" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .bind(path)
            .execute(db.writer())
            .await
            .expect("path");
    }

    async fn set_data(db: &Database, id: Uuid, data: &str) {
        sqlx::query(r#"UPDATE "BaseItems" SET "Data" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .bind(data)
            .execute(db.writer())
            .await
            .expect("data");
    }

    async fn exists(db: &Database, id: Uuid) -> bool {
        sqlx::query_scalar::<_, i64>(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("count")
            > 0
    }

    async fn owner_of(db: &Database, id: Uuid) -> Option<String> {
        sqlx::query_scalar(r#"SELECT "OwnerId" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("owner")
    }

    async fn links(db: &Database) -> Vec<(String, String, i64, i64)> {
        sqlx::query_as(
            r#"SELECT "ParentId", "ChildId", "ChildType", "SortOrder" FROM "LinkedChildren"
               ORDER BY "ParentId", "SortOrder""#,
        )
        .fetch_all(db.pool())
        .await
        .expect("links")
    }

    /// Every 10.11.x generation keeps membership only in `Data` JSON, so the
    /// one-shot import runs for 10.11.11 exactly as for 10.11.8 (the live
    /// 10.11.11 fixture lost all 395 playlist rows when only the exact
    /// "10.11.8" name was accepted).
    #[rstest::rstest]
    #[case("10.11.8")]
    #[case("10.11.11")]
    #[tokio::test]
    async fn membership_is_imported_once_for_a_new_10_11_adoption(#[case] generation: &str) {
        let db = test_db().await;
        db.record_adoption(generation).await.expect("record");
        let (playlist, a, b, primary, alt) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        seed_named_item(&db, playlist, BaseItemKind::Playlist, "P").await;
        for id in [a, b, primary, alt] {
            seed_item(&db, id, BaseItemKind::Movie).await;
        }
        set_path(&db, b, "/m/b.mkv").await;
        set_path(&db, alt, "/m/alt.mkv").await;
        // A 10.11.8 playlist blob: one child by id, one by path, a duplicate.
        set_data(
            &db,
            playlist,
            &format!(
                r#"{{"LinkedChildren":[{{"Type":"Manual","ItemId":"{}"}},{{"Type":"Manual","Path":"/m/b.mkv"}},{{"Type":"Manual","ItemId":"{}"}}]}}"#,
                a.simple(),
                a.simple()
            ),
        )
        .await;
        // A 10.11.8 video blob: a linked alternate version by id and a local one by path.
        set_data(
            &db,
            primary,
            &format!(
                r#"{{"LocalAlternateVersions":["/m/alt.mkv"],"LinkedAlternateVersions":[{{"Type":"Manual","ItemId":"{}"}}]}}"#,
                b.simple()
            ),
        )
        .await;

        let written = import_membership_once(&db).await.expect("import");
        assert_eq!(written, 5);
        assert_eq!(
            links(&db)
                .await
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            vec![
                (guid_to_db(playlist), guid_to_db(a), 0, 0),
                (guid_to_db(playlist), guid_to_db(b), 0, 1),
                (guid_to_db(playlist), guid_to_db(a), 0, 2),
                (guid_to_db(primary), guid_to_db(alt), 2, 0),
                (guid_to_db(primary), guid_to_db(b), 3, 1),
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
        );
        assert!(
            db.adoption_state()
                .await
                .expect("state")
                .expect("record")
                .membership_import_done
        );
        // A second boot changes nothing, even after the table is emptied.
        sqlx::query(r#"DELETE FROM "LinkedChildren""#)
            .execute(db.writer())
            .await
            .expect("empty");
        assert_eq!(import_membership_once(&db).await.expect("again"), 0);
        assert!(links(&db).await.is_empty());
    }

    async fn link(db: &Database, parent: Uuid, child: Uuid, child_type: i64, order: i64) {
        sqlx::query(
            r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
               VALUES (?1, ?2, ?3, ?4)"#,
        )
        .bind(guid_to_db(parent))
        .bind(order)
        .bind(guid_to_db(child))
        .bind(child_type)
        .execute(db.writer())
        .await
        .expect("link");
    }

    async fn primary_and_key(db: &Database, id: Uuid) -> (Option<String>, Option<String>) {
        sqlx::query_as(
            r#"SELECT "PrimaryVersionId", "PresentationUniqueKey" FROM "BaseItems" WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .fetch_one(db.pool())
        .await
        .expect("row")
    }

    /// 12.1 `RepairAlternateVersionLinks`: children take their primary from
    /// `LinkedChildren` (chains resolve to the root, Local beats Linked), a
    /// self-link is ignored, a loop keeps its lowest id, a primary that is
    /// itself marked as a version is promoted unless it is owned — and the
    /// pass runs once.
    #[tokio::test]
    #[allow(clippy::many_single_char_names)] // the upstream test's own letters
    async fn alternate_version_links_repair_matches_12_1() {
        let db = test_db().await;
        let ids: Vec<Uuid> = (0x31..=0x3A).map(Uuid::from_u128).collect();
        let [p, a, b, c, d, e, q, r, owner, s] = ids[..] else {
            unreachable!()
        };
        for id in &ids {
            seed_item(&db, *id, BaseItemKind::Movie).await;
        }
        // p ← a (Local) ← b (Linked): b resolves to p through a.
        link(&db, p, a, LOCAL_ALTERNATE_VERSION, 0).await;
        link(&db, a, b, LINKED_ALTERNATE_VERSION, 0).await;
        // c links to itself: ignored.
        link(&db, c, c, LOCAL_ALTERNATE_VERSION, 0).await;
        // d ↔ e loop: the lower id (d) is the primary.
        link(&db, d, e, LINKED_ALTERNATE_VERSION, 0).await;
        link(&db, e, d, LINKED_ALTERNATE_VERSION, 0).await;
        // q is a primary (s links under it) but still carries a stale
        // PrimaryVersionId of its own: promoted. r is the same (owner links
        // under it) but owned: left alone.
        link(&db, q, s, LOCAL_ALTERNATE_VERSION, 0).await;
        for (id, stale) in [(q, c), (r, c)] {
            sqlx::query(r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .bind(guid_to_db(stale))
                .execute(db.writer())
                .await
                .expect("stale primary");
        }
        sqlx::query(r#"UPDATE "BaseItems" SET "OwnerId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(r))
            .bind(guid_to_db(owner))
            .execute(db.writer())
            .await
            .expect("owner");
        link(&db, r, owner, LOCAL_ALTERNATE_VERSION, 1).await;

        let (repaired, promoted) = repair_alternate_version_links(&db).await.expect("repair");
        let key = |id: Uuid| id.as_simple().to_string();
        assert_eq!(
            primary_and_key(&db, a).await,
            (Some(guid_to_db(p)), Some(key(p)))
        );
        assert_eq!(
            primary_and_key(&db, b).await,
            (Some(guid_to_db(p)), Some(key(p))),
            "b resolves through a to p"
        );
        assert_eq!(primary_and_key(&db, c).await.0, None, "self-link ignored");
        assert_eq!(
            primary_and_key(&db, e).await,
            (Some(guid_to_db(d)), Some(key(d))),
            "loop keeps the lowest id"
        );
        assert_eq!(primary_and_key(&db, d).await.0, None);
        assert_eq!(
            primary_and_key(&db, q).await,
            (None, Some(key(q))),
            "a primary marked as a version is promoted"
        );
        assert_eq!(
            primary_and_key(&db, r).await.0,
            Some(guid_to_db(c)),
            "an owned primary is left for the owner repair"
        );
        // a, b, e, s and r's child (owner) repaired; q promoted.
        assert_eq!((repaired, promoted), (5, 1));
        assert_eq!(
            repair_alternate_version_links(&db).await.expect("again"),
            (0, 0)
        );
    }

    /// `Guid.CompareTo` orders by the signed int/short/short fields first.
    #[test]
    fn dotnet_guid_order_is_by_field_not_by_string() {
        let low = Uuid::parse_str("7fffffff-0000-0000-0000-000000000000").expect("uuid");
        let high = Uuid::parse_str("80000000-0000-0000-0000-000000000000").expect("uuid");
        // 0x80000000 is negative as an int, so .NET sorts it first.
        assert_eq!(dotnet_guid_cmp(high, low), std::cmp::Ordering::Less);
        assert_eq!(dotnet_guid_cmp(low, low), std::cmp::Ordering::Equal);
    }

    /// 12.1 `StripEmbeddedLinkedChildren`: the three dead keys go, everything
    /// else in the blob stays, invalid JSON is untouched, and the pass runs once.
    #[tokio::test]
    async fn embedded_linked_children_keys_are_stripped_once() {
        let db = test_db().await;
        let (a, b, c) = (
            Uuid::from_u128(0x21),
            Uuid::from_u128(0x22),
            Uuid::from_u128(0x23),
        );
        seed_named_item(&db, a, BaseItemKind::Playlist, "P").await;
        seed_named_item(&db, b, BaseItemKind::Movie, "M").await;
        seed_named_item(&db, c, BaseItemKind::Movie, "N").await;
        set_data(
            &db,
            a,
            r#"{"OpenAccess":true,"LinkedChildren":[{"Type":"Manual"}],"ExtraIds":["x"],"SupportsExternalTransfer":false}"#,
        )
        .await;
        set_data(&db, b, r#"{"Overview":"kept"}"#).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "Data" = '{not json' WHERE "Id" = ?1"#)
            .bind(guid_to_db(c))
            .execute(db.writer())
            .await
            .expect("bad json");

        assert_eq!(strip_embedded_linked_children(&db).await.expect("strip"), 1);
        let data = |id: Uuid| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, String>(r#"SELECT "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
                    .bind(guid_to_db(id))
                    .fetch_one(db.pool())
                    .await
                    .expect("data")
            }
        };
        assert_eq!(data(a).await, r#"{"OpenAccess":true}"#);
        assert_eq!(data(b).await, r#"{"Overview":"kept"}"#);
        assert_eq!(data(c).await, "{not json");
        assert_eq!(strip_embedded_linked_children(&db).await.expect("again"), 0);
    }

    /// The JSON on a 12.0 database is frozen: an emptied playlist must stay
    /// empty, however much stale JSON it still carries. A Ferrofin-native
    /// database (no record) never imports either.
    #[tokio::test]
    async fn membership_is_never_imported_for_12_0_or_native_databases() {
        for record in [Some("12.0.0"), None] {
            let db = test_db().await;
            if let Some(generation) = record {
                db.record_adoption(generation).await.expect("record");
            }
            let (playlist, a) = (Uuid::new_v4(), Uuid::new_v4());
            seed_named_item(&db, playlist, BaseItemKind::Playlist, "P").await;
            seed_item(&db, a, BaseItemKind::Movie).await;
            set_data(
                &db,
                playlist,
                &format!(
                    r#"{{"LinkedChildren":[{{"Type":"Manual","ItemId":"{}"}}]}}"#,
                    a.simple()
                ),
            )
            .await;
            for _ in 0..2 {
                assert_eq!(import_membership_once(&db).await.expect("import"), 0);
                assert!(links(&db).await.is_empty(), "record {record:?}");
            }
        }
    }

    #[tokio::test]
    async fn orphaned_extras_owned_by_the_placeholder_are_deleted_once() {
        let db = test_db().await;
        let extra = Uuid::new_v4();
        seed_item(&db, extra, BaseItemKind::Trailer).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "OwnerId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(extra))
            .bind(PLACEHOLDER_ID)
            .execute(db.writer())
            .await
            .expect("own");
        assert_eq!(cleanup_orphaned_extras(&db).await.expect("cleanup"), 1);
        let left: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(extra))
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(left, 0);
        assert_eq!(cleanup_orphaned_extras(&db).await.expect("again"), 0);
    }

    #[tokio::test]
    async fn owner_id_relationships_are_repaired_like_upstream() {
        let db = test_db().await;
        let (keeper, dup, movie, owned_movie, extra, primary, alt) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        for id in [keeper, dup, movie, owned_movie, primary, alt] {
            seed_item(&db, id, BaseItemKind::Movie).await;
        }
        seed_item(&db, extra, BaseItemKind::Trailer).await;
        // Step 1: two rows share a path; the one with a child is the keeper.
        set_path(&db, keeper, "/m/same.mkv").await;
        set_path(&db, dup, "/m/same.mkv").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(extra))
            .bind(guid_to_db(keeper))
            .execute(db.writer())
            .await
            .expect("child");
        // Step 2: a movie owned by another movie is not an extra — unowned.
        sqlx::query(r#"UPDATE "BaseItems" SET "OwnerId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(owned_movie))
            .bind(guid_to_db(movie))
            .execute(db.writer())
            .await
            .expect("own");
        // Step 3: an extra whose owner is gone is re-attached by directory.
        set_path(&db, movie, "/m/film/film.mkv").await;
        set_path(&db, extra, "/m/film/trailer.mkv").await;
        let gone = Uuid::new_v4();
        seed_item(&db, gone, BaseItemKind::Movie).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "OwnerId" = ?2, "ExtraType" = 1 WHERE "Id" = ?1"#)
            .bind(guid_to_db(extra))
            .bind(guid_to_db(gone))
            .execute(db.writer())
            .await
            .expect("orphan");
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(db.writer())
            .await
            .expect("fk off");
        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(gone))
            .execute(db.writer())
            .await
            .expect("delete owner");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(db.writer())
            .await
            .expect("fk on");
        // Step 4: a version link whose child has no pointer yet.
        sqlx::query(
            r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
               VALUES (?1, 0, ?2, 3)"#,
        )
        .bind(guid_to_db(primary))
        .bind(guid_to_db(alt))
        .execute(db.writer())
        .await
        .expect("link");

        let touched = fix_owner_id_relationships(&db).await.expect("repair");
        assert!(touched >= 4, "{touched}");
        assert!(exists(&db, keeper).await && !exists(&db, dup).await);
        assert_eq!(owner_of(&db, owned_movie).await, None);
        assert_eq!(owner_of(&db, extra).await, Some(guid_to_db(movie)));
        let pointer: Option<String> =
            sqlx::query_scalar(r#"SELECT "PrimaryVersionId" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(alt))
                .fetch_one(db.pool())
                .await
                .expect("pointer");
        assert_eq!(pointer, Some(guid_to_db(primary)));
        assert_eq!(fix_owner_id_relationships(&db).await.expect("again"), 0);
    }

    #[tokio::test]
    async fn version_links_are_backfilled_from_primary_version_id_once() {
        let db = test_db().await;
        let (primary, local, linked, already) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        for id in [primary, local, linked, already] {
            seed_item(&db, id, BaseItemKind::Movie).await;
        }
        set_path(&db, primary, "/m/film/film 1080p.mkv").await;
        set_path(&db, local, "/m/film/film 2160p.mkv").await;
        set_path(&db, linked, "/m/other/film.mkv").await;
        for id in [local, linked, already] {
            sqlx::query(r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .bind(guid_to_db(primary))
                .execute(db.writer())
                .await
                .expect("pointer");
        }
        sqlx::query(
            r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
               VALUES (?1, 0, ?2, 3)"#,
        )
        .bind(guid_to_db(primary))
        .bind(guid_to_db(already))
        .execute(db.writer())
        .await
        .expect("existing");

        assert_eq!(
            backfill_alternate_version_links(&db)
                .await
                .expect("backfill"),
            2
        );
        let mut got: Vec<(String, i64)> = links(&db)
            .await
            .into_iter()
            .map(|(_, c, t, _)| (c, t))
            .collect();
        got.sort();
        let mut want = vec![
            (guid_to_db(already), 3),
            (guid_to_db(local), 2),
            (guid_to_db(linked), 3),
        ];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(
            backfill_alternate_version_links(&db).await.expect("again"),
            0
        );
    }

    #[tokio::test]
    async fn case_only_duplicate_artists_fold_onto_the_keeper_once() {
        let db = test_db().await;
        let (keeper, dup, album, listener) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        seed_named_item(&db, keeper, BaseItemKind::MusicArtist, "Gojira").await;
        seed_named_item(&db, dup, BaseItemKind::MusicArtist, "GOJIRA").await;
        seed_named_item(&db, album, BaseItemKind::MusicAlbum, "Magma").await;
        // The keeper is the one with a child.
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(album))
            .bind(guid_to_db(keeper))
            .execute(db.writer())
            .await
            .expect("child");
        crate::test_support::seed_user(&db, listener).await;
        for (item, key) in [(dup, "a"), (dup, "b"), (keeper, "a")] {
            sqlx::query(
                r#"INSERT INTO "UserData" ("UserId", "ItemId", "CustomDataKey", "Played",
                       "IsFavorite", "PlayCount", "PlaybackPositionTicks")
                   VALUES (?1, ?2, ?3, 1, 0, 1, 0)"#,
            )
            .bind(guid_to_db(listener))
            .bind(guid_to_db(item))
            .bind(key)
            .execute(db.writer())
            .await
            .expect("user data");
        }
        assert_eq!(merge_duplicate_music_artists(&db).await.expect("merge"), 1);
        assert!(exists(&db, keeper).await && !exists(&db, dup).await);
        let keys: Vec<String> = sqlx::query_scalar(
            r#"SELECT "CustomDataKey" FROM "UserData" WHERE "ItemId" = ?1 ORDER BY 1"#,
        )
        .bind(guid_to_db(keeper))
        .fetch_all(db.pool())
        .await
        .expect("keys");
        assert_eq!(
            keys,
            vec!["a".to_owned(), "b".to_owned()],
            "keeper's row wins a collision"
        );
        assert_eq!(merge_duplicate_music_artists(&db).await.expect("again"), 0);
    }

    #[tokio::test]
    async fn case_only_duplicate_people_rows_fold_their_map_entries() {
        let db = test_db().await;
        let movie = Uuid::new_v4();
        seed_item(&db, movie, BaseItemKind::Movie).await;
        let (keeper, dup) = (guid_to_db(Uuid::new_v4()), guid_to_db(Uuid::new_v4()));
        for (id, name) in [(&keeper, "Alice Parity"), (&dup, "alice parity")] {
            sqlx::query(
                r#"INSERT INTO "Peoples" ("Id", "Name", "PersonType") VALUES (?1, ?2, 'Actor')"#,
            )
            .bind(id)
            .bind(name)
            .execute(db.writer())
            .await
            .expect("person");
        }
        for (person, role) in [(&keeper, "Lead"), (&dup, "Lead"), (&dup, "Cameo")] {
            sqlx::query(
                r#"INSERT INTO "PeopleBaseItemMap" ("ItemId", "PeopleId", "Role") VALUES (?1, ?2, ?3)"#,
            )
            .bind(guid_to_db(movie))
            .bind(person)
            .bind(role)
            .execute(db.writer())
            .await
            .expect("map");
        }
        assert_eq!(merge_duplicate_people(&db).await.expect("merge"), 1);
        let roles: Vec<(String, String)> =
            sqlx::query_as(r#"SELECT "PeopleId", "Role" FROM "PeopleBaseItemMap" ORDER BY "Role""#)
                .fetch_all(db.pool())
                .await
                .expect("roles");
        // The dup had more map rows, so it is the keeper; the colliding Lead
        // row of the other one was dropped, its id is gone.
        assert_eq!(
            roles,
            vec![
                (dup.clone(), "Cameo".to_owned()),
                (dup.clone(), "Lead".to_owned())
            ]
        );
        let left: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Peoples""#)
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(left, 1);
        assert_eq!(merge_duplicate_people(&db).await.expect("again"), 0);
    }
}
