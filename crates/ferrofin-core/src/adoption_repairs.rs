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
use std::collections::{BTreeMap, BTreeSet, HashMap};

use uuid::Uuid;

use ferrofin_traits::persistence::{ItemRekey, ItemUserWeight};
use ferrofin_traits::system::PathManager;

use crate::db_error::db_err;
use crate::item_data::{import_membership, parse_data, read_linked_children};
use crate::item_folders::{MovedFolders, move_item_folders};
use crate::item_persistence_service::{
    RekeyKind, alternate_version_child_type, rekey_rows, user_weights,
};
use crate::item_type_lookup::{
    IdDerivation, derive_item_id_with, kind_from_type_name, stored_type_name,
};
use crate::library_scan::rekey_source;

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
    let retyped = retype_merged_version_links(db).await?;
    if retyped > 0 {
        tracing::info!(
            rows = retyped,
            "re-typed merged version links as linked alternate versions"
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
/// is itself a video/movie, or no longer exists, loses its `OwnerId` —
/// unless it is that owner's local version or stacked part (a
/// data-preserving divergence, see the step). Step 3:
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
    // Step 2: videos/movies (not extras) owned by a video/movie or by nothing
    // — except the owner's own local versions and stacked parts (its
    // `LocalAlternateVersions` / `AdditionalParts` paths, or a `ChildType` 2
    // link), which 12.x's scanner owns by that video again on its next pass
    // (`LibraryManager.ResolveAlternateVersion`, `LibraryManager.cs:865-922`;
    // `Video.RefreshMetadataForOwnedVideo`, `Video.cs:670-691`): upstream
    // clears their owner here and its scan sets it back, Ferrofin keeps it
    // and [`rehome_local_versions`] completes the shape.
    let cleared = sqlx::query(
        r#"UPDATE "BaseItems" SET "OwnerId" = NULL
           WHERE "OwnerId" IS NOT NULL
             AND ("ExtraType" IS NULL OR "ExtraType" = 0)
             AND "Type" IN (?1, ?2)
             AND (
               "OwnerId" NOT IN (SELECT "Id" FROM "BaseItems")
               OR "OwnerId" IN (SELECT "Id" FROM "BaseItems" WHERE "Type" IN (?1, ?2))
             )
             AND NOT EXISTS (
               SELECT 1 FROM "LinkedChildren" lc
               WHERE lc."ParentId" = "BaseItems"."OwnerId" AND lc."ChildId" = "BaseItems"."Id"
                 AND lc."ChildType" = 2)
             AND NOT EXISTS (
               SELECT 1 FROM "BaseItems" o,
                    json_each(CASE WHEN json_valid(o."Data") THEN o."Data" ELSE '{}' END,
                              '$.LocalAlternateVersions') v
               WHERE o."Id" = "BaseItems"."OwnerId" AND v."value" = "BaseItems"."Path")
             AND NOT EXISTS (
               SELECT 1 FROM "BaseItems" o,
                    json_each(CASE WHEN json_valid(o."Data") THEN o."Data" ELSE '{}' END,
                              '$.AdditionalParts') v
               WHERE o."Id" = "BaseItems"."OwnerId" AND v."value" = "BaseItems"."Path")"#,
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
/// group: local (2) for a version owned by its primary (how upstream's
/// scanner stores one), linked (3) — a merge, wherever the files are —
/// otherwise ([`alternate_version_child_type`]). Rows already present (an
/// adopted 12.0 database) are left alone.
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

/// Ferrofin wrote a merge's version link as `LocalAlternateVersion` (2) when
/// the two files shared a directory; `VideosController.MergeVersions` writes
/// every merge as `LinkedAlternateVersion` (3) — it adds a
/// `LinkedAlternateVersion`, which `ItemPersistenceService.SaveItems` writes
/// as such. A Jellyfin 12.x database does hold type-2 rows — its scanner's
/// local versions, and the Merge Versions plugin's merges, which it stores
/// as owned local versions — and those are kept: a type-2 row stays when its
/// child is owned by the parent (`OwnerId`, no `ExtraType`) or is listed in
/// the parent's `LocalAlternateVersions` (a 10.11 adoption's local versions,
/// which may have no owner: `fix_owner_id_relationships` cleared it on an
/// install adopted before that repair kept it). Every other one is
/// Ferrofin's merge and becomes type 3. Runs once per database, native ones
/// included, after the passes that read version links.
///
/// Returns the number of rows re-typed.
///
/// # Errors
/// Returns [`ServiceError`] if the update fails.
pub async fn retype_merged_version_links(db: &Database) -> Result<u64, ServiceError> {
    const KEY: &str = "merged_version_links_linked_v121";
    if !once(db, KEY).await? {
        return Ok(0);
    }
    let retyped = sqlx::query(
        r#"UPDATE "LinkedChildren" SET "ChildType" = ?2
           WHERE "ChildType" = ?1
             AND NOT EXISTS (
               SELECT 1 FROM "BaseItems" c
               WHERE c."Id" = "LinkedChildren"."ChildId"
                 AND c."OwnerId" = "LinkedChildren"."ParentId" AND c."ExtraType" IS NULL)
             AND NOT EXISTS (
               SELECT 1 FROM "BaseItems" p, "BaseItems" c,
                    json_each(CASE WHEN json_valid(p."Data") THEN p."Data" ELSE '{}' END,
                              '$.LocalAlternateVersions') v
               WHERE p."Id" = "LinkedChildren"."ParentId"
                 AND c."Id" = "LinkedChildren"."ChildId"
                 AND v."value" = c."Path")"#,
    )
    .bind(LOCAL_ALTERNATE_VERSION)
    .bind(LINKED_ALTERNATE_VERSION)
    .execute(db.writer())
    .await
    .map_err(db_err)?
    .rows_affected();
    done(db, KEY).await?;
    Ok(retyped)
}

/// What [`rehome_local_versions`] changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RehomedVersions {
    /// Rows moved to the id and type of their owner's kind (a local version)
    /// or of `Video` (a stacked part).
    pub rekeyed: usize,
    /// Local alternate versions given their primary's owner, pointer, key,
    /// parent, top parent, folder state, ancestors or version link.
    pub versions: usize,
    /// Stacked parts given their video's owner, parent, top parent, folder
    /// state or ancestors.
    pub parts: usize,
}

/// The kinds whose version groups and stacks the walks plan
/// (`MovieResolver.ResolveVideos`, `MovieResolver.cs:287-317`): a movie,
/// a music video, a plain video, and an episode (`ResolveVideos<Episode>`,
/// `MovieResolver.cs:231-234`).
const GROUPED_KINDS: [BaseItemKind; 4] = [
    BaseItemKind::Movie,
    BaseItemKind::MusicVideo,
    BaseItemKind::Video,
    BaseItemKind::Episode,
];

/// One stored video [`rehome_local_versions`] reads.
#[derive(sqlx::FromRow)]
struct GroupRow {
    #[sqlx(rename = "Id")]
    id: String,
    #[sqlx(rename = "Type")]
    type_: String,
    #[sqlx(rename = "Path")]
    path: Option<String>,
    #[sqlx(rename = "Data")]
    data: Option<String>,
}

/// The data-preserving form of Jellyfin 12.0's version cleanups for a
/// database adopted from 10.11 — `MigrateLinkedChildren.
/// CleanupWrongTypeAlternateVersions` and `CleanupOrphanedAlternateVersionBaseItems`
/// (`Jellyfin.Server/Migrations/Routines/20260113120000_MigrateLinkedChildren.cs:287-350`),
/// which delete 10.11's local alternate versions stored as a plain `Video`
/// and its stacked parts, and the scan that then re-creates them
/// (`LibraryManager.ResolveAlternateVersion`, `LibraryManager.cs:865-922`;
/// `Video.RefreshMetadataForOwnedVideo`, `Video.cs:670-691`). Ferrofin keeps
/// the rows — their user data, images, streams and provider ids — and gives
/// them the shape the scan would store, so an adopted database's first scan
/// writes nothing for them (owner decision D9c):
///
/// - a local version — a row at a path in a movie's, music video's,
///   video's or episode's `LocalAlternateVersions`, or a `ChildType` 2 link's child, in
///   the primary's folder — of another type moves to the id the primary's
///   type derives for its path (the scan's re-key, with what is keyed by its
///   id and its folders; a 10.11 `Video` keeps its refresh stamp and
///   provider ids — it is the same title, [`RekeyKind::Represented`]); then
///   it takes the primary as `OwnerId` and `PrimaryVersionId`, the
///   primary's `PresentationUniqueKey` (its N-format id), `ParentId`,
///   `TopParentId`, `IsInMixedFolder` and ancestors, and is linked under it
///   as a local version (`ChildType` 2). Its name stands: the scan never
///   renames an owned version (`settle_extra_name`).
/// - a stacked part — a row at a path in a video's `AdditionalParts` — of
///   another type moves to its `Video` id as the scan's re-key does
///   ([`RekeyKind::Changed`]); then it takes the video as `OwnerId`, and the
///   video's `ParentId`, `TopParentId` and ancestors — and its
///   `IsInMixedFolder` when 10.11 stored it without a parent or it was just
///   moved (what upstream's re-created part starts with; a 12.x part keeps
///   the flag it was created with, `Video.cs:690`).
///
/// A row already stored at the new id is folded by the scan's rule
/// ([`rekey_source`]). Before it changes anything on a file-backed
/// database, the file is copied once to `<db>.pre-rehome` (as a
/// table-rebuilding migration snapshots it). A batch of moves that fails is
/// undone with its folders, its rows left for the scan, and the repair is
/// not marked done, so the next boot tries again.
///
/// It works from the stored state, not from the adoption record, so an
/// install adopted before it existed is repaired too; rows already in the
/// 12.x shape are left alone. Runs once per database (`FerrofinMeta`),
/// after [`run_all`] — it needs the item-id derivation and the folder roots
/// the composition root resolves (`art_root` is the item art root,
/// `{metadata}/library`).
///
/// # Errors
/// Returns [`ServiceError`] if the underlying queries fail.
pub async fn rehome_local_versions(
    db: &Database,
    derivation: &IdDerivation,
    art_root: Option<&Path>,
    path_manager: Option<&dyn PathManager>,
) -> Result<RehomedVersions, ServiceError> {
    const KEY: &str = "rehome_local_versions_v12";
    if !once(db, KEY).await? {
        return Ok(RehomedVersions::default());
    }
    let versions = version_pairs(db).await?;
    let parts = part_pairs(db).await?;
    if !needs_work(db, derivation, &versions, None).await?
        && !needs_work(db, derivation, &parts, Some(BaseItemKind::Video)).await?
    {
        done(db, KEY).await?;
        return Ok(RehomedVersions::default());
    }
    // It rewrites rows users made something of: the file as it was goes
    // aside first, as before a table-rebuilding migration.
    if let Some(backup) = db
        .snapshot_once(SNAPSHOT_EXTENSION)
        .await
        .map_err(meta_err)?
    {
        tracing::info!(
            backup = %backup.display(),
            "database snapshot taken before re-homing local versions and stacked parts"
        );
    }
    let folders = FolderRoots {
        art_root,
        path_manager,
    };
    let mut out = RehomedVersions::default();
    // Local versions first: a version's own stack is owned by its new id.
    let (versions, rekeyed, versions_moved) =
        rekey_to_owner_kind(db, derivation, &folders, versions, None).await?;
    out.rekeyed += rekeyed;
    out.versions = place_owned(db, &versions, true).await?;
    let parts = part_pairs(db).await?;
    let (parts, rekeyed, parts_moved) =
        rekey_to_owner_kind(db, derivation, &folders, parts, Some(BaseItemKind::Video)).await?;
    out.rekeyed += rekeyed;
    out.parts = place_owned(db, &parts, false).await?;
    // A failed move leaves its row for the scan; the repair runs again on
    // the next boot.
    if versions_moved && parts_moved {
        done(db, KEY).await?;
    }
    Ok(out)
}

/// [`rehome_local_versions`] and its log line, for the composition root.
///
/// # Errors
/// Returns [`ServiceError`] if the repair's queries fail.
pub async fn run_rehome_local_versions(
    db: &Database,
    derivation: &IdDerivation,
    art_root: Option<&Path>,
    path_manager: Option<&dyn PathManager>,
) -> Result<(), ServiceError> {
    let rehomed = rehome_local_versions(db, derivation, art_root, path_manager).await?;
    if rehomed != RehomedVersions::default() {
        tracing::info!(
            rekeyed = rehomed.rekeyed,
            versions = rehomed.versions,
            parts = rehomed.parts,
            "gave local alternate versions and stacked parts their owner's shape"
        );
    }
    Ok(())
}

/// Where a re-keyed row's id-named folders live.
struct FolderRoots<'a> {
    art_root: Option<&'a Path>,
    path_manager: Option<&'a dyn PathManager>,
}

/// One owned row and its owner, both as stored.
struct OwnedPair {
    owner: GroupRow,
    child: GroupRow,
    /// The other rows at the child's path, standing for the same file.
    others: Vec<GroupRow>,
    /// Whether the repair moved the child to a new id (or folded it into
    /// the row there): it is the owner's newly made row.
    rekeyed: bool,
}

/// The extension of the database snapshot [`rehome_local_versions`] takes
/// (`jellyfin.db.pre-rehome`).
const SNAPSHOT_EXTENSION: &str = "db.pre-rehome";

/// The stored type names of [`GROUPED_KINDS`].
fn grouped_type_names() -> Vec<&'static str> {
    GROUPED_KINDS
        .iter()
        .filter_map(|kind| stored_type_name(*kind))
        .collect()
}

/// Whether two paths are files of one folder.
fn same_folder(a: &str, b: &str) -> bool {
    Path::new(a).parent() == Path::new(b).parent()
}

/// The stored videos of [`GROUPED_KINDS`] that are no extra and whose `Data`
/// names a path list `key` (`LocalAlternateVersions`, `AdditionalParts`).
async fn listing_videos(db: &Database, key: &str) -> Result<Vec<GroupRow>, ServiceError> {
    let types = grouped_type_names();
    let rows: Vec<GroupRow> = sqlx::query_as(
        r#"SELECT "Id", "Type", "Path", "Data" FROM "BaseItems"
           WHERE "Type" IN (?1, ?2, ?3, ?4) AND "ExtraType" IS NULL AND "Path" IS NOT NULL
             AND "Data" IS NOT NULL AND instr("Data", ?5) > 0
           ORDER BY "Id""#,
    )
    .bind(types[0])
    .bind(types[1])
    .bind(types[2])
    .bind(types[3])
    .bind(format!("\"{key}\""))
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    Ok(rows)
}

/// The paths of `row`'s `Data` list `key`.
fn data_paths(row: &GroupRow, key: &str) -> Vec<String> {
    parse_data(row.data.as_deref())
        .get(key)
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The stored rows standing for the file at `path` beside `owner`: the
/// videos there that are no extra, by id.
async fn rows_at(db: &Database, path: &str, owner: &str) -> Result<Vec<GroupRow>, ServiceError> {
    let video_types = [
        stored_type_name(BaseItemKind::Movie).unwrap_or_default(),
        stored_type_name(BaseItemKind::MusicVideo).unwrap_or_default(),
        stored_type_name(BaseItemKind::Video).unwrap_or_default(),
        stored_type_name(BaseItemKind::Episode).unwrap_or_default(),
    ];
    sqlx::query_as(
        r#"SELECT "Id", "Type", "Path", "Data" FROM "BaseItems"
           WHERE "Path" = ?1 AND "Id" <> ?2 AND "ExtraType" IS NULL
             AND "Type" IN (?3, ?4, ?5, ?6)
           ORDER BY "Id""#,
    )
    .bind(path)
    .bind(owner)
    .bind(video_types[0])
    .bind(video_types[1])
    .bind(video_types[2])
    .bind(video_types[3])
    .fetch_all(db.pool())
    .await
    .map_err(db_err)
}

/// The pair of `owner` and the rows at `path`: the child is the row of
/// `child_type` (the type it is to have) when one is there, else the first.
fn pair_of(owner: &GroupRow, mut rows: Vec<GroupRow>, child_type: &str) -> Option<OwnedPair> {
    let at = rows
        .iter()
        .position(|row| row.type_ == child_type)
        .unwrap_or(0);
    if rows.is_empty() {
        return None;
    }
    let child = rows.remove(at);
    Some(OwnedPair {
        owner: GroupRow {
            id: owner.id.clone(),
            type_: owner.type_.clone(),
            path: owner.path.clone(),
            data: None,
        },
        child,
        others: rows,
        rekeyed: false,
    })
}

/// Every local version and its primary, one per path: the paths of the
/// primary's `LocalAlternateVersions`, then those of its `ChildType` 2
/// links' children — each in the primary's folder, a path counted once
/// (its first primary, by id), none at which a primary stands that is
/// itself listed here.
async fn version_pairs(db: &Database) -> Result<Vec<OwnedPair>, ServiceError> {
    let [_, local_versions] = crate::item_data::RESOLVED_VIDEO_PATH_LISTS;
    let mut primaries: BTreeMap<String, GroupRow> = BTreeMap::new();
    for owner in listing_videos(db, local_versions).await? {
        primaries.insert(owner.id.clone(), owner);
    }
    let mut wanted: Vec<(String, String)> = Vec::new();
    for owner in primaries.values() {
        for path in data_paths(owner, local_versions) {
            wanted.push((owner.id.clone(), path));
        }
    }
    let types = grouped_type_names();
    let linked: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
        r#"SELECT p."Id", p."Type", p."Path", c."Path"
           FROM "LinkedChildren" lc
           JOIN "BaseItems" p ON p."Id" = lc."ParentId"
           JOIN "BaseItems" c ON c."Id" = lc."ChildId"
           WHERE lc."ChildType" = ?1 AND p."Type" IN (?2, ?3, ?4, ?5) AND p."ExtraType" IS NULL
             AND c."ExtraType" IS NULL AND c."Path" IS NOT NULL AND p."Path" IS NOT NULL
           ORDER BY lc."ParentId", lc."SortOrder""#,
    )
    .bind(LOCAL_ALTERNATE_VERSION)
    .bind(types[0])
    .bind(types[1])
    .bind(types[2])
    .bind(types[3])
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    for (id, type_, path, child_path) in linked {
        primaries.entry(id.clone()).or_insert(GroupRow {
            id: id.clone(),
            type_,
            path,
            data: None,
        });
        wanted.push((id, child_path));
    }
    let owners: BTreeSet<String> = wanted.iter().map(|(owner, _)| owner.clone()).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut pairs: Vec<OwnedPair> = Vec::new();
    for (owner, path) in wanted {
        let Some(primary) = primaries.get(&owner) else {
            continue;
        };
        let in_folder = primary
            .path
            .as_deref()
            .is_some_and(|p| p != path && same_folder(p, &path));
        if !in_folder || seen.contains(&path) {
            continue;
        }
        let rows = rows_at(db, &path, &owner).await?;
        if rows.iter().any(|row| owners.contains(&row.id)) {
            continue;
        }
        seen.insert(path);
        pairs.extend(pair_of(primary, rows, &primary.type_));
    }
    Ok(pairs)
}

/// Every stacked part and its video, one per path: the paths of a video's
/// `AdditionalParts`, a path counted once.
async fn part_pairs(db: &Database) -> Result<Vec<OwnedPair>, ServiceError> {
    let [additional_parts, _] = crate::item_data::RESOLVED_VIDEO_PATH_LISTS;
    let video = stored_type_name(BaseItemKind::Video).unwrap_or_default();
    let mut pairs: Vec<OwnedPair> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for owner in listing_videos(db, additional_parts).await? {
        for path in data_paths(&owner, additional_parts) {
            if !seen.insert(path.clone()) {
                continue;
            }
            pairs.extend(pair_of(&owner, rows_at(db, &path, &owner.id).await?, video));
        }
    }
    Ok(pairs)
}

/// One [`rekey_rows`] call of [`rekey_to_owner_kind`]: how its moves change
/// their items, the moves, and each move's folders by target id.
type RekeyBatch = (RekeyKind, Vec<ItemRekey>, Vec<(Uuid, MovedFolders)>);

/// One child's move to its owner's kind: the id that kind derives for its
/// path, the stored type name, the path, and the rows there not at that id.
type RekeyTarget = (Uuid, &'static str, String, Vec<(Uuid, String)>);

/// Where `pair`'s file moves when it is not stored as `kind` (none: its
/// owner's type) alone at that kind's id: the id the kind derives for its
/// path, with the rows at the path standing elsewhere — as the scan's
/// re-key groups them ([`rekey_source`] picks which one moves).
fn rekey_target(
    derivation: &IdDerivation,
    pair: &OwnedPair,
    kind: Option<BaseItemKind>,
) -> Option<RekeyTarget> {
    let target_kind = kind.or_else(|| kind_from_type_name(&pair.owner.type_))?;
    let path = pair.child.path.as_deref()?;
    let type_name = stored_type_name(target_kind)?;
    let to = derive_item_id_with(derivation, target_kind, path)?;
    let others: Vec<(Uuid, String)> = std::iter::once(&pair.child)
        .chain(&pair.others)
        .filter_map(|row| {
            Uuid::parse_str(&row.id)
                .ok()
                .map(|id| (id, row.type_.clone()))
        })
        .filter(|(id, _)| *id != to)
        .collect();
    // A lone row of the kind keeps its id (an id derived otherwise is the
    // database's own derivation, not a kind change).
    let at_target = others.len() < pair.others.len() + 1;
    let lone_of_kind = !at_target && others.len() == 1 && others[0].1 == type_name;
    if others.is_empty() || lone_of_kind {
        return None;
    }
    Some((to, type_name, path.to_owned(), others))
}

/// Whether any of `pairs` has something to change: a child to move to its
/// owner's kind (`kind`, none: the owner's type), or one not in its owner's
/// shape ([`place_differs`]).
async fn needs_work(
    db: &Database,
    derivation: &IdDerivation,
    pairs: &[OwnedPair],
    kind: Option<BaseItemKind>,
) -> Result<bool, ServiceError> {
    let mut conn = db.pool().acquire().await.map_err(db_err)?;
    for pair in pairs {
        if rekey_target(derivation, pair, kind).is_some()
            || place_differs(&mut conn, pair, kind.is_none()).await?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the row `id` is stored.
async fn stored(db: &Database, id: Uuid) -> Result<bool, ServiceError> {
    let row: Option<String> = sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .fetch_optional(db.pool())
        .await
        .map_err(db_err)?;
    Ok(row.is_some())
}

/// Moves each pair's file to the id its owner's kind (`kind`, none: the
/// owner's type) derives for its path ([`rekey_target`]) — the scan's
/// re-key ([`rekey_rows`]) with its folders ([`move_item_folders`]): of the
/// rows standing for it, the one the scan's rule keeps ([`rekey_source`]:
/// what users made of each, a stored target winning unless bare) takes the
/// id and the others fold into it. A 10.11 local version stored as a
/// `Video` moving only changes representation ([`RekeyKind::Represented`]:
/// its refresh stamp and provider ids stay); any other move is a change of
/// kind ([`RekeyKind::Changed`]). Returns the pairs with their children's
/// ids as they now are (a move not made drops its pair: the rows stay as
/// they were, for the scan), the number of moves made, and whether every
/// batch went through — a failed one is undone, its folders back where they
/// were, and logged.
async fn rekey_to_owner_kind(
    db: &Database,
    derivation: &IdDerivation,
    folders: &FolderRoots<'_>,
    pairs: Vec<OwnedPair>,
    kind: Option<BaseItemKind>,
) -> Result<(Vec<OwnedPair>, usize, bool), ServiceError> {
    // Everything read before any folder moves.
    let mut planned: Vec<Option<(RekeyTarget, bool)>> = Vec::with_capacity(pairs.len());
    let mut contested: Vec<Uuid> = Vec::new();
    for pair in &pairs {
        let Some(target) = rekey_target(derivation, pair, kind) else {
            planned.push(None);
            continue;
        };
        let to_stored = stored(db, target.0).await?;
        if to_stored || target.3.len() > 1 {
            contested.extend(target.3.iter().map(|(id, _)| *id));
            contested.push(target.0);
        }
        planned.push(Some((target, to_stored)));
    }
    let weights: HashMap<Uuid, ItemUserWeight> = user_weights(db, &contested)
        .await?
        .into_iter()
        .map(|weight| (weight.id, weight))
        .collect();
    let video = stored_type_name(BaseItemKind::Video);
    let mut batches: [RekeyBatch; 2] = [
        (RekeyKind::Represented, Vec::new(), Vec::new()),
        (RekeyKind::Changed, Vec::new(), Vec::new()),
    ];
    for ((to, type_name, path, rows), to_stored) in planned.iter().flatten() {
        let mut others: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
        let source = rekey_source(*to, *to_stored, &mut others, &weights);
        let represented = kind.is_none()
            && rows
                .iter()
                .any(|(id, type_)| *id == source && Some(type_.as_str()) == video);
        let batch = &mut batches[usize::from(!represented)];
        let dirs = if source == *to {
            Vec::new()
        } else {
            let (renamed, dirs) = move_item_folders(
                folders.art_root,
                folders.path_manager,
                (source, *to),
                path,
                *to_stored,
            );
            batch.2.push((*to, renamed));
            dirs
        };
        batch.1.push(ItemRekey {
            from: source,
            to: *to,
            type_name: (*type_name).to_owned(),
            dirs,
            merged: others,
            extra: false,
        });
    }
    let mut made: BTreeSet<Uuid> = BTreeSet::new();
    let mut complete = true;
    for (how, moves, moved) in batches {
        match rekey_rows(db, &moves, how).await {
            Ok(done) => {
                made.extend(done.iter().map(|m| m.to));
                for (to, renamed) in moved {
                    if made.contains(&to) {
                        renamed.commit();
                    } else {
                        renamed.undo();
                    }
                }
            }
            Err(err) => {
                tracing::warn!(
                    %err,
                    "could not move local versions or stacked parts to their owner's kind; \
                     they stay for the scan, and the repair runs again on the next boot"
                );
                for (_, renamed) in moved {
                    renamed.undo();
                }
                complete = false;
            }
        }
    }
    let mut kept = Vec::with_capacity(pairs.len());
    for (mut pair, plan) in pairs.into_iter().zip(planned) {
        match plan {
            Some(((to, ..), _)) if made.contains(&to) => {
                pair.child.id = guid_to_db(to);
                pair.others.clear();
                pair.rekeyed = true;
                kept.push(pair);
            }
            Some(_) => {}
            None => kept.push(pair),
        }
    }
    Ok((kept, made.len(), complete))
}

/// Whether `pair`'s child is not yet in its owner's shape — what
/// [`place_owned`] writes: the owner as `OwnerId`, its `ParentId` and
/// `TopParentId`, its `IsInMixedFolder` (for a part only one 10.11 stored
/// without a parent, or one the repair just moved: a 12.x part keeps the
/// flag it was created with, `Video.cs:690`), its ancestors; for a local
/// version (`versions`) also the owner as `PrimaryVersionId`, its N-format
/// id as the key, and a `ChildType` 2 link.
async fn place_differs(
    conn: &mut sqlx::SqliteConnection,
    pair: &OwnedPair,
    versions: bool,
) -> Result<bool, ServiceError> {
    let OwnedPair {
        owner,
        child,
        rekeyed,
        ..
    } = pair;
    let key = simple_key(&owner.id)?;
    let row: bool = sqlx::query_scalar(
        r#"SELECT EXISTS (SELECT 1 FROM "BaseItems" c, "BaseItems" o
           WHERE c."Id" = ?1 AND o."Id" = ?2
             AND (c."OwnerId" IS NOT o."Id" OR c."ParentId" IS NOT o."ParentId"
                  OR c."TopParentId" IS NOT o."TopParentId"
                  OR ((?3 OR ?4 OR c."ParentId" IS NULL)
                      AND c."IsInMixedFolder" IS NOT o."IsInMixedFolder")
                  OR (?3 AND (c."PrimaryVersionId" IS NOT o."Id"
                              OR c."PresentationUniqueKey" IS NOT ?5))))"#,
    )
    .bind(&child.id)
    .bind(&owner.id)
    .bind(versions)
    .bind(rekeyed)
    .bind(&key)
    .fetch_one(&mut *conn)
    .await
    .map_err(db_err)?;
    if row || ancestors_differ(conn, &owner.id, &child.id).await? {
        return Ok(true);
    }
    Ok(versions && link_type(conn, &owner.id, &child.id).await? != Some(LOCAL_ALTERNATE_VERSION))
}

/// `id`'s N-format form, a presentation key.
fn simple_key(id: &str) -> Result<String, ServiceError> {
    Uuid::parse_str(id)
        .map(|id| id.as_simple().to_string())
        .map_err(|err| ServiceError::Backend(err.to_string()))
}

/// Gives each pair's child its owner's shape ([`place_differs`]), in one
/// transaction; a local version (`versions`) is linked under its owner (a
/// linked one re-typed, as local beats linked). Returns the number of
/// children changed.
async fn place_owned(
    db: &Database,
    pairs: &[OwnedPair],
    versions: bool,
) -> Result<usize, ServiceError> {
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let mut changed = 0usize;
    for pair in pairs {
        if !place_differs(&mut tx, pair, versions).await? {
            continue;
        }
        let OwnedPair {
            owner,
            child,
            rekeyed,
            ..
        } = pair;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "OwnerId" = o."Id", "ParentId" = o."ParentId",
                   "TopParentId" = o."TopParentId",
                   "IsInMixedFolder" = CASE WHEN ?3 OR ?4 OR "BaseItems"."ParentId" IS NULL
                                            THEN o."IsInMixedFolder"
                                            ELSE "BaseItems"."IsInMixedFolder" END,
                   "PrimaryVersionId" = CASE WHEN ?3 THEN o."Id"
                                             ELSE "BaseItems"."PrimaryVersionId" END,
                   "PresentationUniqueKey" = CASE WHEN ?3 THEN ?5
                                                  ELSE "BaseItems"."PresentationUniqueKey" END
               FROM (SELECT "Id", "ParentId", "TopParentId", "IsInMixedFolder"
                     FROM "BaseItems" WHERE "Id" = ?2) AS o
               WHERE "BaseItems"."Id" = ?1"#,
        )
        .bind(&child.id)
        .bind(&owner.id)
        .bind(versions)
        .bind(rekeyed)
        .bind(simple_key(&owner.id)?)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if ancestors_differ(&mut tx, &owner.id, &child.id).await? {
            sqlx::query(r#"DELETE FROM "AncestorIds" WHERE "ItemId" = ?1"#)
                .bind(&child.id)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            sqlx::query(
                r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId")
                   SELECT ?2, "ParentItemId" FROM "AncestorIds" WHERE "ItemId" = ?1"#,
            )
            .bind(&owner.id)
            .bind(&child.id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        if versions {
            link_local_version(&mut tx, &owner.id, &child.id).await?;
        }
        changed += 1;
    }
    tx.commit().await.map_err(db_err)?;
    Ok(changed)
}

/// Whether `child`'s ancestors are not `owner`'s.
async fn ancestors_differ(
    conn: &mut sqlx::SqliteConnection,
    owner: &str,
    child: &str,
) -> Result<bool, ServiceError> {
    let mut of = Vec::with_capacity(2);
    for id in [owner, child] {
        let ancestors: Vec<String> = sqlx::query_scalar(
            r#"SELECT "ParentItemId" FROM "AncestorIds" WHERE "ItemId" = ?1 ORDER BY 1"#,
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?;
        of.push(ancestors);
    }
    Ok(of[0] != of[1])
}

/// The `ChildType` of the link of `child` under `owner`, if any.
async fn link_type(
    conn: &mut sqlx::SqliteConnection,
    owner: &str,
    child: &str,
) -> Result<Option<i64>, ServiceError> {
    sqlx::query_scalar(
        r#"SELECT "ChildType" FROM "LinkedChildren" WHERE "ParentId" = ?1 AND "ChildId" = ?2"#,
    )
    .bind(owner)
    .bind(child)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_err)
}

/// Links `child` under `owner` as a local version (`ChildType` 2): a linked
/// one (3) is re-typed, a missing one appended. Returns whether it wrote.
async fn link_local_version(
    tx: &mut sqlx::SqliteConnection,
    owner: &str,
    child: &str,
) -> Result<bool, ServiceError> {
    let stored = link_type(tx, owner, child).await?;
    match stored {
        Some(LOCAL_ALTERNATE_VERSION) => Ok(false),
        Some(_) => {
            sqlx::query(
                r#"UPDATE "LinkedChildren" SET "ChildType" = ?3
                   WHERE "ParentId" = ?1 AND "ChildId" = ?2"#,
            )
            .bind(owner)
            .bind(child)
            .bind(LOCAL_ALTERNATE_VERSION)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            Ok(true)
        }
        None => {
            append_link(tx, owner, child, LOCAL_ALTERNATE_VERSION).await?;
            Ok(true)
        }
    }
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

async fn count(
    tx: &mut sqlx::SqliteConnection,
    sql: &'static str,
    id: &str,
) -> Result<i64, ServiceError> {
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
    use crate::test_support::{
        seed_folder_item, seed_item, seed_named_item, seed_provider_id, seed_user, seed_user_data,
        test_db,
    };

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

    /// Sets `columns` (`"Col" = value` SQL, values bound in order) on `id`.
    async fn set_columns(db: &Database, id: Uuid, columns: &str, values: &[Option<String>]) {
        let sql = format!(r#"UPDATE "BaseItems" SET {columns} WHERE "Id" = ?1"#);
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(guid_to_db(id));
        for value in values {
            query = query.bind(value.clone());
        }
        query.execute(db.writer()).await.expect("set columns");
    }

    async fn add_ancestor(db: &Database, item: Uuid, ancestor: Uuid) {
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(item))
            .bind(guid_to_db(ancestor))
            .execute(db.writer())
            .await
            .expect("ancestor");
    }

    async fn ancestors(db: &Database, item: &str) -> Vec<String> {
        sqlx::query_scalar(
            r#"SELECT "ParentItemId" FROM "AncestorIds" WHERE "ItemId" = ?1 ORDER BY 1"#,
        )
        .bind(item)
        .fetch_all(db.pool())
        .await
        .expect("ancestors")
    }

    /// `(Type, OwnerId, PrimaryVersionId, PresentationUniqueKey, ParentId,
    /// TopParentId, IsInMixedFolder, DateLastRefreshed)` of `id`.
    type Shape = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        bool,
        Option<String>,
    );

    async fn shape(db: &Database, id: Uuid) -> Option<Shape> {
        sqlx::query_as(
            r#"SELECT "Type", "OwnerId", "PrimaryVersionId", "PresentationUniqueKey",
                      "ParentId", "TopParentId", "IsInMixedFolder", "DateLastRefreshed"
               FROM "BaseItems" WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .fetch_optional(db.pool())
        .await
        .expect("shape")
    }

    const DERIVATION: IdDerivation = IdDerivation::Jellyfin {
        program_data_path: None,
    };

    /// A movies library's group and a home-video stack, both stored as a
    /// group's primary and its owned rows, under one library folder.
    struct Group {
        library: Uuid,
        primary: Uuid,
        stack: Uuid,
    }

    const PRIMARY: &str = "/m/Bridge (2014)/Bridge (2014) - 2160p.mkv";
    const ALTERNATE: &str = "/m/Bridge (2014)/Bridge (2014) - 1080p.mkv";
    const STACK: &str = "/h/Lesson/Lesson part 1.mp4";
    const PART: &str = "/h/Lesson/Lesson part 2.mp4";
    const MOVIE_PART: &str = "/h/Lesson/Lesson part 3.mp4";

    async fn seed_group(db: &Database) -> Group {
        let library = Uuid::from_u128(0x1001);
        seed_folder_item(db, library, BaseItemKind::CollectionFolder, "Movies", None).await;
        let primary = derive_item_id_with(&DERIVATION, BaseItemKind::Movie, PRIMARY).unwrap();
        let stack = derive_item_id_with(&DERIVATION, BaseItemKind::Video, STACK).unwrap();
        for (id, kind, path) in [
            (primary, BaseItemKind::Movie, PRIMARY),
            (stack, BaseItemKind::Video, STACK),
        ] {
            seed_item(db, id, kind).await;
            set_path(db, id, path).await;
            set_columns(
                db,
                id,
                r#""ParentId" = ?2, "TopParentId" = ?2, "IsInMixedFolder" = ?3"#,
                &[
                    Some(guid_to_db(library)),
                    Some(i32::from(kind == BaseItemKind::Video).to_string()),
                ],
            )
            .await;
            add_ancestor(db, id, library).await;
        }
        set_data(
            db,
            primary,
            &format!(r#"{{"AdditionalParts":[],"LocalAlternateVersions":["{ALTERNATE}"]}}"#),
        )
        .await;
        set_data(
            db,
            stack,
            &format!(
                r#"{{"AdditionalParts":["{PART}","{MOVIE_PART}"],"LocalAlternateVersions":[]}}"#
            ),
        )
        .await;
        Group {
            library,
            primary,
            stack,
        }
    }

    /// The bench's 10.11 rows of [`seed_group`]'s groups: the local version
    /// (played, with a provider id), the part, and an older scan's `Movie`
    /// part. Returns their ids.
    async fn seed_10_11_rows(db: &Database, g: &Group) -> [Uuid; 3] {
        let adopted = derive_item_id_with(&DERIVATION, BaseItemKind::Video, ALTERNATE).unwrap();
        let part = derive_item_id_with(&DERIVATION, BaseItemKind::Video, PART).unwrap();
        let older = derive_item_id_with(&DERIVATION, BaseItemKind::Movie, MOVIE_PART).unwrap();
        for (id, kind, path, owner) in [
            (adopted, BaseItemKind::Video, ALTERNATE, Some(g.primary)),
            (part, BaseItemKind::Video, PART, Some(g.stack)),
            (older, BaseItemKind::Movie, MOVIE_PART, None),
        ] {
            seed_item(db, id, kind).await;
            set_path(db, id, path).await;
            set_columns(
                db,
                id,
                r#""OwnerId" = ?2, "PresentationUniqueKey" = ?3,
                   "DateLastRefreshed" = '2026-09-02 22:10:38.9466347'"#,
                &[owner.map(guid_to_db), Some(id.as_simple().to_string())],
            )
            .await;
        }
        link(db, g.primary, adopted, LOCAL_ALTERNATE_VERSION, 0).await;
        seed_provider_id(db, adopted, "Tmdb", "101332").await;
        let user = Uuid::from_u128(0x99);
        seed_user(db, user).await;
        seed_user_data(db, user, adopted, true, None).await;

        [adopted, part, older]
    }

    /// The bench's adopted Jellyfin 10.11 shape: a movie's local version is
    /// a `Video` it owns — no parent, no top parent, no pointer, keyed by its
    /// own id — linked under it as `MigrateLinkedChildren` imports it; a
    /// stack's part is a `Video` it owns with no parent and its own folder
    /// state, and an older scan's part a `Movie` of its own. The repair
    /// moves the version to its `Movie` id with its user data, provider ids
    /// and refresh stamp, the older part to its `Video` id as the scan's
    /// re-key does (refresh stamp cleared), gives all their owner's place,
    /// and leaves the result alone on a second run, even with its once-key
    /// cleared.
    #[tokio::test]
    async fn local_versions_and_parts_take_their_owners_shape() {
        let db = test_db().await;
        let g = seed_group(&db).await;
        let [adopted, part, older] = seed_10_11_rows(&db, &g).await;
        let rehomed = rehome_local_versions(&db, &DERIVATION, None, None)
            .await
            .unwrap();
        assert_eq!(
            rehomed,
            RehomedVersions {
                rekeyed: 2,
                versions: 1,
                parts: 2
            }
        );
        let (primary, library) = (guid_to_db(g.primary), Some(guid_to_db(g.library)));
        let alternate = derive_item_id_with(&DERIVATION, BaseItemKind::Movie, ALTERNATE).unwrap();
        assert!(!exists(&db, adopted).await);
        assert_eq!(
            shape(&db, alternate).await,
            Some((
                stored_type_name(BaseItemKind::Movie).unwrap().to_owned(),
                Some(primary.clone()),
                Some(primary.clone()),
                Some(g.primary.as_simple().to_string()),
                library.clone(),
                library.clone(),
                false,
                Some("2026-09-02 22:10:38.9466347".to_owned()),
            ))
        );
        assert_eq!(
            ancestors(&db, &guid_to_db(alternate)).await,
            vec![guid_to_db(g.library)]
        );
        let kept: (i64, i64) = sqlx::query_as(
            r#"SELECT (SELECT COUNT(*) FROM "BaseItemProviders" WHERE "ItemId" = ?1),
                      (SELECT COUNT(*) FROM "UserData" WHERE "ItemId" = ?1 AND "Played" = 1
                         AND "CustomDataKey" = ?2)"#,
        )
        .bind(guid_to_db(alternate))
        .bind(alternate.to_string())
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(kept, (1, 1), "provider ids and user data stay with it");
        assert_eq!(
            links(&db).await,
            vec![(primary.clone(), guid_to_db(alternate), 2, 0)]
        );
        let video = stored_type_name(BaseItemKind::Video).unwrap().to_owned();
        let stack = Some(guid_to_db(g.stack));
        let moved_part = derive_item_id_with(&DERIVATION, BaseItemKind::Video, MOVIE_PART).unwrap();
        assert!(!exists(&db, older).await);
        // The 10.11 part stays what it was; the older scan's `Movie` changes
        // kind, refreshed as a part on the next scan (as the scan's re-key).
        for (id, stays) in [(part, true), (moved_part, false)] {
            let (type_, owner, pointer, _, parent, top, mixed, refreshed) =
                shape(&db, id).await.expect("part");
            assert_eq!(
                (type_, owner, pointer, parent, top, mixed),
                (
                    video.clone(),
                    stack.clone(),
                    None,
                    library.clone(),
                    library.clone(),
                    true
                )
            );
            assert_eq!(refreshed.is_some(), stays, "refresh stamp of {id}");
            assert_eq!(
                ancestors(&db, &guid_to_db(id)).await,
                vec![guid_to_db(g.library)]
            );
        }

        assert_eq!(
            rehome_local_versions(&db, &DERIVATION, None, None)
                .await
                .unwrap(),
            RehomedVersions::default()
        );
        sqlx::query(r#"DELETE FROM "FerrofinMeta" WHERE "Key" = 'rehome_local_versions_v12'"#)
            .execute(db.writer())
            .await
            .unwrap();
        assert_eq!(
            rehome_local_versions(&db, &DERIVATION, None, None)
                .await
                .unwrap(),
            RehomedVersions::default(),
            "the repaired shape is the 12.x one"
        );
    }

    /// An episode's group takes the shape the TV walk plans
    /// (`ResolveVideos<Episode>`): a version stored as an episode of its own
    /// is owned by, points at and is linked under the primary, in its season;
    /// a part stored as an episode moves to its `Video` id, owned by it.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one group's seed, then its three rows checked
    async fn episode_versions_and_parts_take_their_owners_shape() {
        const EPISODE: &str = "/tv/Show/Season 1/Show - S01E01 - 1080p.mkv";
        const VERSION: &str = "/tv/Show/Season 1/Show - S01E01 - 720p.mkv";
        const PART: &str = "/tv/Show/Season 1/Show - S01E01 - 1080p - part2.mkv";
        let db = test_db().await;
        let library = Uuid::from_u128(0x2001);
        let season = Uuid::from_u128(0x2002);
        seed_folder_item(&db, library, BaseItemKind::CollectionFolder, "Shows", None).await;
        seed_folder_item(&db, season, BaseItemKind::Season, "Season 1", None).await;
        let id = |kind, path| derive_item_id_with(&DERIVATION, kind, path).unwrap();
        let (primary, version) = (
            id(BaseItemKind::Episode, EPISODE),
            id(BaseItemKind::Episode, VERSION),
        );
        let stored_part = id(BaseItemKind::Episode, PART);
        for (item, path, mixed) in [
            (primary, EPISODE, "1"),
            (version, VERSION, "0"),
            (stored_part, PART, "0"),
        ] {
            seed_item(&db, item, BaseItemKind::Episode).await;
            set_path(&db, item, path).await;
            set_columns(
                &db,
                item,
                r#""ParentId" = ?2, "TopParentId" = ?3, "IsInMixedFolder" = ?4,
                   "PresentationUniqueKey" = ?5"#,
                &[
                    Some(guid_to_db(season)),
                    Some(guid_to_db(library)),
                    Some(mixed.to_owned()),
                    Some(item.as_simple().to_string()),
                ],
            )
            .await;
            add_ancestor(&db, item, library).await;
            add_ancestor(&db, item, season).await;
        }
        set_data(
            &db,
            primary,
            &format!(r#"{{"AdditionalParts":["{PART}"],"LocalAlternateVersions":["{VERSION}"]}}"#),
        )
        .await;

        let rehomed = rehome_local_versions(&db, &DERIVATION, None, None)
            .await
            .unwrap();

        assert_eq!(
            rehomed,
            RehomedVersions {
                rekeyed: 1,
                versions: 1,
                parts: 1
            }
        );
        let (owner, parent, top) = (
            Some(guid_to_db(primary)),
            Some(guid_to_db(season)),
            Some(guid_to_db(library)),
        );
        let (type_, version_owner, pointer, key, version_parent, version_top, mixed, _) =
            shape(&db, version).await.expect("version");
        assert_eq!(
            (
                type_,
                version_owner,
                pointer,
                key,
                version_parent,
                version_top,
                mixed
            ),
            (
                stored_type_name(BaseItemKind::Episode).unwrap().to_owned(),
                owner.clone(),
                owner.clone(),
                Some(primary.as_simple().to_string()),
                parent.clone(),
                top.clone(),
                true
            )
        );
        assert!(
            links(&db)
                .await
                .contains(&(guid_to_db(primary), guid_to_db(version), 2, 0))
        );
        assert!(!exists(&db, stored_part).await);
        let part = id(BaseItemKind::Video, PART);
        let (type_, part_owner, pointer, _, part_parent, part_top, mixed, _) =
            shape(&db, part).await.expect("part");
        assert_eq!(
            (type_, part_owner, pointer, part_parent, part_top, mixed),
            (
                stored_type_name(BaseItemKind::Video).unwrap().to_owned(),
                owner,
                None,
                parent,
                top,
                true
            )
        );
        assert_eq!(
            ancestors(&db, &guid_to_db(part)).await,
            ancestors(&db, &guid_to_db(primary)).await
        );
    }

    /// The art folder named after `id` under `art`, N-format.
    fn art_folder(art: &Path, id: Uuid) -> std::path::PathBuf {
        let n = id.as_simple().to_string();
        art.join(&n[..2]).join(&n)
    }

    /// Makes every re-key transaction fail at its commit: a moved row gets
    /// an ancestor that does not exist (a deferred foreign-key violation).
    async fn fail_moves(db: &Database) {
        sqlx::query(
            r#"CREATE TRIGGER "TestFailMove" AFTER UPDATE OF "Id" ON "BaseItems"
               BEGIN INSERT INTO "AncestorIds" ("ItemId", "ParentItemId")
                     VALUES (NEW."Id", '00000000-0000-0000-0000-00000000DEAD'); END"#,
        )
        .execute(db.writer())
        .await
        .unwrap();
    }

    /// A move whose transaction fails is undone with its folders — the
    /// moving row's art folder back at its old name, the stored target's own
    /// folder back from `.rekey-aside` — its row is left as it was, and the
    /// repair is not marked done. The next boot moves it: the adopted row
    /// users played wins over the bare stored target (the scan's fold rule),
    /// takes its id and its folder name, and the set-aside folder goes.
    #[tokio::test]
    async fn a_failed_move_is_undone_and_tried_again() {
        let db = test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let art = tmp.path().join("library");
        let g = seed_group(&db).await;
        let [adopted, ..] = seed_10_11_rows(&db, &g).await;
        let target = derive_item_id_with(&DERIVATION, BaseItemKind::Movie, ALTERNATE).unwrap();
        seed_named_item(&db, target, BaseItemKind::Movie, "bare").await;
        set_path(&db, target, ALTERNATE).await;
        for (id, file) in [(adopted, "poster.jpg"), (target, "own.jpg")] {
            std::fs::create_dir_all(art_folder(&art, id)).unwrap();
            std::fs::write(art_folder(&art, id).join(file), b"x").unwrap();
        }
        fail_moves(&db).await;

        let rehomed = rehome_local_versions(&db, &DERIVATION, Some(&art), None)
            .await
            .unwrap();
        assert_eq!(rehomed.rekeyed, 0);
        assert!(exists(&db, adopted).await && exists(&db, target).await);
        assert!(art_folder(&art, adopted).join("poster.jpg").is_file());
        assert!(art_folder(&art, target).join("own.jpg").is_file());
        let aside = format!("{}.rekey-aside", art_folder(&art, target).display());
        assert!(!Path::new(&aside).exists(), "nothing left aside");
        assert_eq!(
            db.meta_get("rehome_local_versions_v12").await.unwrap(),
            None
        );

        sqlx::query(r#"DROP TRIGGER "TestFailMove""#)
            .execute(db.writer())
            .await
            .unwrap();
        let rehomed = rehome_local_versions(&db, &DERIVATION, Some(&art), None)
            .await
            .unwrap();
        // The version and the older scan's `Movie` part.
        assert_eq!((rehomed.rekeyed, rehomed.versions), (2, 1));
        assert!(!exists(&db, adopted).await);
        let name: Option<String> =
            sqlx::query_scalar(r#"SELECT "Name" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(target))
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_ne!(name.as_deref(), Some("bare"), "the played row won");
        assert!(art_folder(&art, target).join("poster.jpg").is_file());
        assert!(!art_folder(&art, adopted).exists());
        assert!(!Path::new(&aside).exists(), "the target's own folder went");
        assert!(
            db.meta_get("rehome_local_versions_v12")
                .await
                .unwrap()
                .is_some()
        );
    }

    /// A stored target users made more of than of the adopted row keeps its
    /// row: the adopted one folds into it, and it becomes the version.
    #[tokio::test]
    async fn a_stored_target_users_made_more_of_keeps_its_row() {
        let db = test_db().await;
        let g = seed_group(&db).await;
        let adopted = derive_item_id_with(&DERIVATION, BaseItemKind::Video, ALTERNATE).unwrap();
        let target = derive_item_id_with(&DERIVATION, BaseItemKind::Movie, ALTERNATE).unwrap();
        seed_item(&db, adopted, BaseItemKind::Video).await;
        seed_named_item(&db, target, BaseItemKind::Movie, "kept").await;
        for id in [adopted, target] {
            set_path(&db, id, ALTERNATE).await;
        }
        let user = Uuid::from_u128(0x98);
        seed_user(&db, user).await;
        seed_user_data(&db, user, target, true, None).await;

        let rehomed = rehome_local_versions(&db, &DERIVATION, None, None)
            .await
            .unwrap();
        assert_eq!((rehomed.rekeyed, rehomed.versions), (1, 1));
        assert!(!exists(&db, adopted).await);
        let name: Option<String> =
            sqlx::query_scalar(r#"SELECT "Name" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(target))
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(name.as_deref(), Some("kept"));
        assert_eq!(owner_of(&db, target).await, Some(guid_to_db(g.primary)));
    }

    /// The group edges: a path two primaries list goes to the first (by id);
    /// a child that is itself a primary stays one while its own version
    /// joins it; two rows at one path are one file, as the scan's re-key has
    /// it: the one users made more of (here: a collection links it) moves to
    /// the primary's kind's id and the other folds into it; a linked (3)
    /// link of a local version becomes local (2).
    #[tokio::test]
    async fn group_edges_resolve_like_the_scan() {
        let db = test_db().await;
        let movie = |db: &Database, id: Uuid, kind: BaseItemKind, path: &'static str| {
            let db = db.clone();
            async move {
                seed_item(&db, id, kind).await;
                set_path(&db, id, path).await;
            }
        };
        // Two primaries listing one path.
        let [first, second, shared] = [0x71, 0x72, 0x73].map(Uuid::from_u128);
        movie(&db, first, BaseItemKind::Movie, "/e/A/A.mkv").await;
        movie(&db, second, BaseItemKind::Movie, "/e/A/A - 4k.mkv").await;
        movie(&db, shared, BaseItemKind::Movie, "/e/A/A - 720p.mkv").await;
        for id in [first, second] {
            set_data(
                &db,
                id,
                r#"{"LocalAlternateVersions":["/e/A/A - 720p.mkv"]}"#,
            )
            .await;
        }
        // A chain: top lists middle, middle lists leaf.
        let [top, middle, leaf] = [0x81, 0x82, 0x83].map(Uuid::from_u128);
        movie(&db, top, BaseItemKind::Movie, "/e/B/B.mkv").await;
        movie(&db, middle, BaseItemKind::Movie, "/e/B/B - 1080p.mkv").await;
        movie(&db, leaf, BaseItemKind::Movie, "/e/B/B - 720p.mkv").await;
        set_data(
            &db,
            top,
            r#"{"LocalAlternateVersions":["/e/B/B - 1080p.mkv"]}"#,
        )
        .await;
        set_data(
            &db,
            middle,
            r#"{"LocalAlternateVersions":["/e/B/B - 720p.mkv"]}"#,
        )
        .await;
        // Two rows at one path, and a linked link.
        let [primary, as_movie, as_video] = [0x91, 0x92, 0x93].map(Uuid::from_u128);
        movie(&db, primary, BaseItemKind::Movie, "/e/C/C.mkv").await;
        movie(&db, as_movie, BaseItemKind::Movie, "/e/C/C - 720p.mkv").await;
        movie(&db, as_video, BaseItemKind::Video, "/e/C/C - 720p.mkv").await;
        set_data(
            &db,
            primary,
            r#"{"LocalAlternateVersions":["/e/C/C - 720p.mkv"]}"#,
        )
        .await;
        link(&db, primary, as_movie, LINKED_ALTERNATE_VERSION, 0).await;

        let rehomed = rehome_local_versions(&db, &DERIVATION, None, None)
            .await
            .unwrap();
        assert_eq!(
            rehomed,
            RehomedVersions {
                rekeyed: 1,
                versions: 3,
                parts: 0
            }
        );
        assert_eq!(owner_of(&db, shared).await, Some(guid_to_db(first)));
        assert_eq!(owner_of(&db, middle).await, None, "a primary stays one");
        assert_eq!(owner_of(&db, leaf).await, Some(guid_to_db(middle)));
        // A lone row of the kind keeps its id; the two at one path are one.
        let version =
            derive_item_id_with(&DERIVATION, BaseItemKind::Movie, "/e/C/C - 720p.mkv").unwrap();
        assert!(!exists(&db, as_movie).await && !exists(&db, as_video).await);
        assert_eq!(owner_of(&db, version).await, Some(guid_to_db(primary)));
        let all = links(&db).await;
        assert_eq!(
            all.iter()
                .filter(|(_, child, ..)| *child == guid_to_db(shared))
                .count(),
            1
        );
        assert!(all.contains(&(guid_to_db(primary), guid_to_db(version), 2, 0)));
    }

    /// Before it changes anything on a file-backed database the repair
    /// copies the file to `<db>.pre-rehome`; with nothing to change it
    /// takes none.
    #[tokio::test]
    async fn a_snapshot_is_taken_only_when_there_is_work() {
        let tmp = tempfile::tempdir().unwrap();
        let open = |name: &str| {
            let url = format!("sqlite://{}", tmp.path().join(name).display());
            async move {
                let db = Database::connect(&url).await.unwrap();
                db.run_migrations().await.unwrap();
                db
            }
        };
        let quiet = open("quiet.db").await;
        rehome_local_versions(&quiet, &DERIVATION, None, None)
            .await
            .unwrap();
        assert!(!tmp.path().join("quiet.db.pre-rehome").exists());

        let db = open("jellyfin.db").await;
        let g = seed_group(&db).await;
        seed_10_11_rows(&db, &g).await;
        rehome_local_versions(&db, &DERIVATION, None, None)
            .await
            .unwrap();
        assert!(tmp.path().join("jellyfin.db.pre-rehome").is_file());
    }

    /// A Jellyfin 12.x database's groups already have the shape: nothing is
    /// written. A version link whose child sits in another folder (a merge
    /// stored as an owned version) is no group of the folder's and is left
    /// as it is.
    #[tokio::test]
    async fn the_12_x_shape_is_left_alone() {
        let db = test_db().await;
        let g = seed_group(&db).await;
        let alternate = derive_item_id_with(&DERIVATION, BaseItemKind::Movie, ALTERNATE).unwrap();
        let part = derive_item_id_with(&DERIVATION, BaseItemKind::Video, PART).unwrap();
        let elsewhere = Uuid::from_u128(0x77);
        let (primary, library) = (guid_to_db(g.primary), guid_to_db(g.library));
        for (id, kind, path, owner, mixed) in [
            (alternate, BaseItemKind::Movie, ALTERNATE, g.primary, "0"),
            // Created outside its stack's folder state: a 12.x part keeps it.
            (part, BaseItemKind::Video, PART, g.stack, "0"),
            (
                elsewhere,
                BaseItemKind::Video,
                "/m/Other/Other.mkv",
                g.primary,
                "0",
            ),
        ] {
            seed_item(&db, id, kind).await;
            set_path(&db, id, path).await;
            let version = (kind == BaseItemKind::Movie).then(|| primary.clone());
            set_columns(
                &db,
                id,
                r#""OwnerId" = ?2, "ParentId" = ?3, "TopParentId" = ?3, "IsInMixedFolder" = ?4,
                   "PrimaryVersionId" = ?5, "PresentationUniqueKey" = ?6"#,
                &[
                    Some(guid_to_db(owner)),
                    Some(library.clone()),
                    Some(mixed.to_owned()),
                    version,
                    Some(g.primary.as_simple().to_string()),
                ],
            )
            .await;
            add_ancestor(&db, id, g.library).await;
        }
        link(&db, g.primary, alternate, LOCAL_ALTERNATE_VERSION, 0).await;
        link(&db, g.primary, elsewhere, LOCAL_ALTERNATE_VERSION, 1).await;
        let before = (
            shape(&db, alternate).await,
            shape(&db, part).await,
            shape(&db, elsewhere).await,
            links(&db).await,
        );

        assert_eq!(
            rehome_local_versions(&db, &DERIVATION, None, None)
                .await
                .unwrap(),
            RehomedVersions::default()
        );
        let after = (
            shape(&db, alternate).await,
            shape(&db, part).await,
            shape(&db, elsewhere).await,
            links(&db).await,
        );
        assert_eq!(before, after);
    }

    /// `FixIncorrectOwnerIdRelationships` step 2 keeps a video's own local
    /// version and stacked part owned — by a `ChildType` 2 link or by a path
    /// its `Data` lists — and still unowns a movie another movie owns for no
    /// such reason.
    #[tokio::test]
    async fn owned_versions_and_parts_keep_their_owner() {
        let db = test_db().await;
        let [primary, linked, listed, part, stray] =
            [0x61, 0x62, 0x63, 0x64, 0x65].map(Uuid::from_u128);
        seed_item(&db, primary, BaseItemKind::Movie).await;
        for id in [linked, listed, part] {
            seed_item(&db, id, BaseItemKind::Video).await;
        }
        seed_item(&db, stray, BaseItemKind::Movie).await;
        set_path(&db, listed, "/m/M/M - 720p.mkv").await;
        set_path(&db, part, "/m/M/M cd2.mkv").await;
        set_data(
            &db,
            primary,
            r#"{"LocalAlternateVersions":["/m/M/M - 720p.mkv"],"AdditionalParts":["/m/M/M cd2.mkv"]}"#,
        )
        .await;
        link(&db, primary, linked, LOCAL_ALTERNATE_VERSION, 0).await;
        for id in [linked, listed, part, stray] {
            set_columns(&db, id, r#""OwnerId" = ?2"#, &[Some(guid_to_db(primary))]).await;
        }

        fix_owner_id_relationships(&db).await.unwrap();
        for id in [linked, listed, part] {
            assert_eq!(owner_of(&db, id).await, Some(guid_to_db(primary)));
        }
        assert_eq!(owner_of(&db, stray).await, None);
    }

    /// Ferrofin's same-folder merges were written as local (2) links: the
    /// repair re-types them linked (3), as upstream writes every merge, and
    /// leaves a scanner-owned local version and a 10.11 adoption's listed
    /// one alone. A second run is a no-op.
    #[tokio::test]
    async fn merged_version_links_are_retyped_once() {
        let db = test_db().await;
        let [primary, merged, owned, listed, extra] =
            [0x51, 0x52, 0x53, 0x54, 0x55].map(Uuid::from_u128);
        for id in [primary, merged, owned, listed, extra] {
            seed_item(&db, id, BaseItemKind::Movie).await;
        }
        set_path(&db, listed, "/m/Movie/Movie - 720p.mkv").await;
        set_data(
            &db,
            primary,
            r#"{"LocalAlternateVersions":["/m/Movie/Movie - 720p.mkv"]}"#,
        )
        .await;
        for (id, extra_type) in [(owned, None), (extra, Some(1))] {
            sqlx::query(
                r#"UPDATE "BaseItems" SET "OwnerId" = ?2, "ExtraType" = ?3 WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .bind(guid_to_db(primary))
            .bind(extra_type)
            .execute(db.writer())
            .await
            .expect("owner");
        }
        for (order, child) in [merged, owned, listed, extra].into_iter().enumerate() {
            let order = i64::try_from(order).expect("order");
            link(&db, primary, child, LOCAL_ALTERNATE_VERSION, order).await;
        }

        assert_eq!(retype_merged_version_links(&db).await.expect("retype"), 2);
        let types: Vec<(String, i64)> = links(&db)
            .await
            .into_iter()
            .map(|(_, child, child_type, _)| (child, child_type))
            .collect();
        assert_eq!(
            types,
            vec![
                (guid_to_db(merged), LINKED_ALTERNATE_VERSION),
                (guid_to_db(owned), LOCAL_ALTERNATE_VERSION),
                (guid_to_db(listed), LOCAL_ALTERNATE_VERSION),
                (guid_to_db(extra), LINKED_ALTERNATE_VERSION),
            ]
        );

        // A merge row written after the repair is not its to touch again.
        link(&db, merged, owned, LOCAL_ALTERNATE_VERSION, 0).await;
        assert_eq!(retype_merged_version_links(&db).await.expect("again"), 0);
        assert_eq!(links(&db).await.len(), 5);
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
        let (primary, local, merged, linked, already) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        for id in [primary, local, merged, linked, already] {
            seed_item(&db, id, BaseItemKind::Movie).await;
        }
        set_path(&db, primary, "/m/film/film 1080p.mkv").await;
        set_path(&db, local, "/m/film/film 2160p.mkv").await;
        // A merge in the same folder is still a merge.
        set_path(&db, merged, "/m/film/film 720p.mkv").await;
        set_path(&db, linked, "/m/other/film.mkv").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "OwnerId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(local))
            .bind(guid_to_db(primary))
            .execute(db.writer())
            .await
            .expect("owner");
        for id in [local, merged, linked, already] {
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
            3
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
            (guid_to_db(merged), 3),
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
