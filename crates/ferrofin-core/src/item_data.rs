//! `BaseItems.Data` JSON — Jellyfin 10.11.8's serialized-item payload, the
//! **source of truth** for playlist/collection membership, ownership, and
//! shares.
//!
//! Jellyfin serializes the whole runtime item object into the `Data` column
//! (`BaseItemRepository`: `entity.Data = JsonSerializer.Serialize(dto, dtoType,
//! JsonDefaults.Options)`), PascalCase, GUIDs in the lowercase un-hyphenated
//! N-format, enums as strings. Observed real 10.11.8 shapes:
//!
//! ```json
//! // Playlist
//! {"OwnerUserId":"dc68cd36…","OpenAccess":true,"Shares":[],
//!  "PlaylistMediaType":"Video","LinkedChildren":[
//!    {"Path":"/media/…/M.mkv","Type":"Manual","ItemId":"d37ecb9d…"}], …}
//! // BoxSet (children are path-only)
//! {"DisplayOrder":"PremiereDate","LinkedChildren":[
//!    {"Path":"/media/…/M.mkv","Type":"Manual"}], …}
//! ```
//!
//! Ferrofin's query paths keep using the `FerrofinLinkedChildren` /
//! `FerrofinPlaylists` / `FerrofinPlaylistShares` tables as a **derived cache**;
//! every membership/ownership mutation writes through to `Data` via
//! [`sync_container_data`], and [`reconcile_container_data`] rebuilds the
//! cache from `Data` (adopted Jellyfin databases) or backfills `Data` from the
//! cache (databases created before this module existed) at startup.
//!
//! Writes are **read-modify-write** over the raw JSON object: keys Ferrofin does
//! not model (`DisplayOrder`, `DateLastSaved`, `IsHD`, …) are preserved
//! byte-for-byte so a round trip back to Jellyfin loses nothing.

use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::db_error::db_err;
use crate::item_type_lookup::stored_type_name;

/// One entry of the `LinkedChildren` array (C# `LinkedChild`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct LinkedChildJson {
    /// The child's on-disk path — Jellyfin's primary link key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `"Manual"` or `"Shortcut"` (enum serialized as a string).
    #[serde(rename = "Type")]
    pub child_type: String,
    /// The child's id in N-format lowercase, when linked by id (playlists).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// The library item id for shortcut links, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_item_id: Option<String>,
}

/// One entry of a playlist's `Shares` array (C# `PlaylistUserPermissions`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct ShareJson {
    /// The shared-with user's id, N-format lowercase.
    pub user_id: String,
    /// Whether that user may edit the playlist.
    pub can_edit: bool,
}

/// The stored `FerrofinLinkedChildren.ChildType` discriminants and their JSON
/// enum-string forms.
const CHILD_TYPES: [(i64, &str); 2] = [(0, "Manual"), (1, "Shortcut")];

/// A [`Uuid`] in Jellyfin's Data-JSON GUID form: N-format lowercase.
#[must_use]
pub fn json_guid(id: Uuid) -> String {
    id.simple().to_string()
}

/// Parses a `Data` column value into its JSON object, tolerating `NULL`,
/// empty, and malformed payloads (each yields an empty object).
#[must_use]
pub fn parse_data(data: Option<&str>) -> Map<String, Value> {
    data.and_then(|d| serde_json::from_str::<Value>(d).ok())
        .and_then(|v| match v {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// The `LinkedChildren` array from a parsed `Data` object, or empty.
#[must_use]
pub fn read_linked_children(data: &Map<String, Value>) -> Vec<LinkedChildJson> {
    data.get("LinkedChildren")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Reads the `RemoteTrailers` array from a `Data` column value as
/// `(name, url)` pairs, in stored order. Entries without a URL are dropped.
///
/// Jellyfin stores every item's remote trailers here (`MediaUrl` objects:
/// `{"Url": …, "Name": …}`) — it is the only home for them in the 10.11.8
/// schema, so reading and writing it keeps the drop-in round trip lossless.
#[must_use]
pub fn read_remote_trailers(data: Option<&str>) -> Vec<(Option<String>, String)> {
    parse_data(data)
        .get("RemoteTrailers")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    let url = e.get("Url")?.as_str()?.to_owned();
                    let name = e
                        .get("Name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .filter(|n| !n.is_empty());
                    Some((name, url))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Merges `trailers` into a `Data` column value's `RemoteTrailers` array,
/// returning the new column text — or `None` when nothing changed (every URL
/// was already present), so callers can skip a pointless write.
///
/// De-duplicates by URL and appends, mirroring upstream's
/// `MetadataService.MergeBaseItemData` (`DistinctBy(t => t.Url)`). Every other
/// key in the blob is preserved.
#[must_use]
pub fn merge_remote_trailers(
    data: Option<&str>,
    trailers: &[(Option<String>, String)],
) -> Option<String> {
    let existing = read_remote_trailers(data);
    let mut merged = existing.clone();
    for (name, url) in trailers {
        if !merged.iter().any(|(_, u)| u == url) {
            merged.push((name.clone(), url.clone()));
        }
    }
    if merged.len() == existing.len() {
        return None;
    }
    let mut object = parse_data(data);
    let entries: Vec<Value> = merged
        .into_iter()
        .map(|(name, url)| {
            let mut e = Map::new();
            e.insert("Url".to_owned(), Value::String(url));
            if let Some(name) = name {
                e.insert("Name".to_owned(), Value::String(name));
            }
            Value::Object(e)
        })
        .collect();
    object.insert("RemoteTrailers".to_owned(), Value::Array(entries));
    serde_json::to_string(&Value::Object(object)).ok()
}

/// Sets a string field on a `Data` column value, returning the new column
/// text. Every other key is preserved.
///
/// Used for the fields Ferrofin has no dedicated column for and Jellyfin only
/// keeps in the blob (`VideoType`, `IsoType` — what the `videoTypes` browse
/// filter matches on).
#[must_use]
pub fn set_data_field(data: Option<&str>, key: &str, value: &str) -> Option<String> {
    let mut object = parse_data(data);
    object.insert(key.to_owned(), Value::String(value.to_owned()));
    serde_json::to_string(&Value::Object(object)).ok()
}

/// Whether the parsed `Data` object carries a `LinkedChildren` key at all —
/// the presence signal that Jellyfin (or a prior Ferrofin sync) owns this blob.
#[must_use]
pub fn has_linked_children_key(data: &Map<String, Value>) -> bool {
    data.contains_key("LinkedChildren")
}

/// Serializes a mutated `Data` object back to its column text.
///
/// # Errors
/// Never in practice (`Map<String, Value>` always serializes); surfaced as a
/// [`ServiceError`] to keep callers on one error path.
fn data_to_string(data: &Map<String, Value>) -> Result<String, ServiceError> {
    serde_json::to_string(&Value::Object(data.clone()))
        .map_err(|e| ServiceError::Backend(format!("serialize Data JSON: {e}")))
}

/// The container kinds whose `Data` JSON carries membership.
fn container_type_names() -> [&'static str; 2] {
    [
        stored_type_name(BaseItemKind::Playlist)
            .unwrap_or("MediaBrowser.Controller.Playlists.Playlist"),
        stored_type_name(BaseItemKind::BoxSet)
            .unwrap_or("MediaBrowser.Controller.Entities.Movies.BoxSet"),
    ]
}

/// Rewrites `container`'s `Data` JSON from the cache tables — call after any
/// membership / ownership / share mutation. A missing row or a non-container
/// type is a no-op (deletions race here harmlessly).
///
/// # Errors
/// Returns [`ServiceError`] if the underlying queries fail.
pub async fn sync_container_data(db: &Database, container: Uuid) -> Result<(), ServiceError> {
    let [playlist_type, _] = container_type_names();
    let container_db = guid_to_db(container);
    let Some((type_, data)): Option<(String, Option<String>)> =
        sqlx::query_as(r#"SELECT "Type", "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(&container_db)
            .fetch_optional(db.pool())
            .await
            .map_err(db_err)?
    else {
        return Ok(());
    };
    if !container_type_names().contains(&type_.as_str()) {
        return Ok(());
    }
    let is_playlist = type_ == playlist_type;

    // Cache edges (with each child's path) in stored order.
    let rows: Vec<(String, i64, Option<String>)> = sqlx::query_as(
        r#"SELECT lc."ChildId", lc."ChildType", bi."Path"
           FROM "FerrofinLinkedChildren" lc
           LEFT JOIN "BaseItems" bi ON bi."Id" = lc."ChildId"
           WHERE lc."ParentId" = ?1
           ORDER BY lc."SortOrder""#,
    )
    .bind(&container_db)
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;

    let children: Vec<LinkedChildJson> = rows
        .into_iter()
        .map(|(child_id, child_type, path)| {
            let type_name = CHILD_TYPES
                .iter()
                .find(|(d, _)| *d == child_type)
                .map_or("Manual", |(_, n)| n)
                .to_owned();
            let item_id = Uuid::parse_str(&child_id).ok().map(json_guid);
            // Playlists link by path AND id; box sets by path alone (Jellyfin
            // resolves them by path at load) with an id fallback for pathless
            // children.
            let (path, item_id) = if is_playlist {
                (path, item_id)
            } else if path.is_some() {
                (path, None)
            } else {
                (None, item_id)
            };
            LinkedChildJson {
                path,
                child_type: type_name,
                item_id,
                library_item_id: None,
            }
        })
        .collect();

    let mut map = parse_data(data.as_deref());
    map.insert(
        "LinkedChildren".to_owned(),
        serde_json::to_value(&children)
            .map_err(|e| ServiceError::Backend(format!("serialize LinkedChildren: {e}")))?,
    );

    if is_playlist {
        sync_playlist_meta(db, &container_db, &mut map).await?;
    }

    sqlx::query(r#"UPDATE "BaseItems" SET "Data" = ?2 WHERE "Id" = ?1"#)
        .bind(&container_db)
        .bind(data_to_string(&map)?)
        .execute(db.writer())
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Folds a playlist's owner / open-access / shares cache rows into its parsed
/// `Data` object (the `OwnerUserId` / `OpenAccess` / `Shares` keys).
async fn sync_playlist_meta(
    db: &Database,
    container_db: &str,
    map: &mut Map<String, Value>,
) -> Result<(), ServiceError> {
    let meta: Option<(Option<String>, bool)> = sqlx::query_as(
        r#"SELECT "OwnerUserId", "OpenAccess" FROM "FerrofinPlaylists" WHERE "PlaylistId" = ?1"#,
    )
    .bind(container_db)
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)?;
    if let Some((owner, open_access)) = meta {
        if let Some(owner_id) = owner.as_deref().and_then(|o| Uuid::parse_str(o).ok()) {
            map.insert("OwnerUserId".to_owned(), Value::String(json_guid(owner_id)));
        }
        map.insert("OpenAccess".to_owned(), Value::Bool(open_access));
    }
    let shares: Vec<(String, bool)> = sqlx::query_as(
        r#"SELECT "UserId", "CanEdit" FROM "FerrofinPlaylistShares"
           WHERE "PlaylistId" = ?1 ORDER BY "UserId""#,
    )
    .bind(container_db)
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    let shares: Vec<ShareJson> = shares
        .into_iter()
        .filter_map(|(user, can_edit)| {
            Uuid::parse_str(&user).ok().map(|u| ShareJson {
                user_id: json_guid(u),
                can_edit,
            })
        })
        .collect();
    map.insert(
        "Shares".to_owned(),
        serde_json::to_value(&shares)
            .map_err(|e| ServiceError::Backend(format!("serialize Shares: {e}")))?,
    );
    Ok(())
}

/// Reconciles every playlist/collection row between `Data` JSON and the cache
/// tables — run once at startup (and after adopting a Jellyfin database).
///
/// Direction per row: a `Data` blob carrying a `LinkedChildren` key is the
/// source of truth (Jellyfin wrote it, or a prior sync did) and is **imported**
/// into the cache; a row without one (created by Ferrofin before this module) is
/// **exported** — its `Data` is backfilled from the cache.
///
/// Returns `(imported, exported)` row counts.
///
/// # Errors
/// Returns [`ServiceError`] if the underlying queries fail.
pub async fn reconcile_container_data(db: &Database) -> Result<(usize, usize), ServiceError> {
    let [playlist_type, boxset_type] = container_type_names();
    let rows: Vec<(String, String, Option<String>)> =
        sqlx::query_as(r#"SELECT "Id", "Type", "Data" FROM "BaseItems" WHERE "Type" IN (?1, ?2)"#)
            .bind(playlist_type)
            .bind(boxset_type)
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;

    let (mut imported, mut exported) = (0usize, 0usize);
    for (id, type_, data) in rows {
        let Ok(container) = Uuid::parse_str(&id) else {
            continue;
        };
        let map = parse_data(data.as_deref());
        if has_linked_children_key(&map) {
            import_container(db, container, type_ == playlist_type, &map).await?;
            imported += 1;
        } else {
            sync_container_data(db, container).await?;
            exported += 1;
        }
    }
    if imported + exported > 0 {
        tracing::info!(
            imported,
            exported,
            "reconciled playlist/collection Data JSON with the membership cache"
        );
    }
    Ok((imported, exported))
}

/// Sets a single key in an item's `Data` JSON, preserving everything else.
///
/// # Errors
/// Returns [`ServiceError`] if the underlying queries fail.
pub async fn set_data_key(
    db: &Database,
    item: Uuid,
    key: &str,
    value: Value,
) -> Result<(), ServiceError> {
    let item_db = guid_to_db(item);
    let data: Option<Option<String>> =
        sqlx::query_scalar(r#"SELECT "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(&item_db)
            .fetch_optional(db.pool())
            .await
            .map_err(db_err)?;
    let Some(data) = data else { return Ok(()) };
    let mut map = parse_data(data.as_deref());
    map.insert(key.to_owned(), value);
    sqlx::query(r#"UPDATE "BaseItems" SET "Data" = ?2 WHERE "Id" = ?1"#)
        .bind(&item_db)
        .bind(data_to_string(&map)?)
        .execute(db.writer())
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Imports one container's `Data` JSON into the cache tables (edges replaced
/// wholesale; playlist owner/shares upserted).
async fn import_container(
    db: &Database,
    container: Uuid,
    is_playlist: bool,
    map: &Map<String, Value>,
) -> Result<(), ServiceError> {
    let container_db = guid_to_db(container);
    let children = read_linked_children(map);

    let mut tx = db.writer().begin().await.map_err(db_err)?;
    sqlx::query(r#"DELETE FROM "FerrofinLinkedChildren" WHERE "ParentId" = ?1"#)
        .bind(&container_db)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    for (order, child) in children.iter().enumerate() {
        // Resolve by id when present, else by path (Jellyfin's own load-time
        // strategy). Unresolvable children are skipped — same as Jellyfin
        // showing a stale path-only link as missing.
        let resolved: Option<String> = if let Some(id) = child
            .item_id
            .as_deref()
            .and_then(|i| Uuid::parse_str(i).ok())
        {
            Some(guid_to_db(id))
        } else if let Some(path) = child.path.as_deref() {
            sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Path" = ?1 LIMIT 1"#)
                .bind(path)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?
        } else {
            None
        };
        let Some(child_db) = resolved else { continue };
        let discriminant = CHILD_TYPES
            .iter()
            .find(|(_, n)| *n == child.child_type)
            .map_or(0, |(d, _)| *d);
        let order = i64::try_from(order).unwrap_or(i64::MAX);
        sqlx::query(
            r#"INSERT INTO "FerrofinLinkedChildren" ("ParentId", "ChildId", "ChildType", "SortOrder")
               VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT("ParentId", "ChildId") DO UPDATE
               SET "ChildType" = excluded."ChildType", "SortOrder" = excluded."SortOrder""#,
        )
        .bind(&container_db)
        .bind(child_db)
        .bind(discriminant)
        .bind(order)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }

    if is_playlist {
        let owner = map
            .get("OwnerUserId")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok());
        let open_access = map
            .get("OpenAccess")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        sqlx::query(
            r#"INSERT INTO "FerrofinPlaylists" ("PlaylistId", "OwnerUserId", "OpenAccess")
               VALUES (?1, ?2, ?3)
               ON CONFLICT("PlaylistId") DO UPDATE
               SET "OwnerUserId" = excluded."OwnerUserId",
                   "OpenAccess" = excluded."OpenAccess""#,
        )
        .bind(&container_db)
        .bind(owner.map(guid_to_db))
        .bind(open_access)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        let shares: Vec<ShareJson> = map
            .get("Shares")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        sqlx::query(r#"DELETE FROM "FerrofinPlaylistShares" WHERE "PlaylistId" = ?1"#)
            .bind(&container_db)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for share in shares {
            let Ok(user) = Uuid::parse_str(&share.user_id) else {
                continue;
            };
            sqlx::query(
                r#"INSERT INTO "FerrofinPlaylistShares" ("PlaylistId", "UserId", "CanEdit")
                   VALUES (?1, ?2, ?3)
                   ON CONFLICT("PlaylistId", "UserId") DO UPDATE
                   SET "CanEdit" = excluded."CanEdit""#,
            )
            .bind(&container_db)
            .bind(guid_to_db(user))
            .bind(share.can_edit)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
    }
    tx.commit().await.map_err(db_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{seed_item, seed_named_item, test_db};

    /// Seeds a movie with an on-disk path so path-only links resolve.
    async fn seed_movie_with_path(db: &Database, id: Uuid, path: &str) {
        seed_item(db, id, BaseItemKind::Movie).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "Path" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .bind(path)
            .execute(db.writer())
            .await
            .expect("set path");
    }

    #[tokio::test]
    async fn sync_writes_playlist_data_in_jellyfin_shape() {
        let db = test_db().await;
        let playlist = Uuid::new_v4();
        let child = Uuid::new_v4();
        let owner = Uuid::new_v4();
        seed_named_item(&db, playlist, BaseItemKind::Playlist, "Mix").await;
        seed_movie_with_path(&db, child, "/m/a.mkv").await;
        sqlx::query(
            r#"INSERT INTO "FerrofinPlaylists" ("PlaylistId", "OwnerUserId", "OpenAccess")
               VALUES (?1, ?2, 1)"#,
        )
        .bind(guid_to_db(playlist))
        .bind(guid_to_db(owner))
        .execute(db.writer())
        .await
        .expect("meta");
        sqlx::query(
            r#"INSERT INTO "FerrofinLinkedChildren" ("ParentId", "ChildId", "ChildType", "SortOrder")
               VALUES (?1, ?2, 0, 0)"#,
        )
        .bind(guid_to_db(playlist))
        .bind(guid_to_db(child))
        .execute(db.writer())
        .await
        .expect("edge");

        sync_container_data(&db, playlist).await.expect("sync");

        let data: Option<String> =
            sqlx::query_scalar(r#"SELECT "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(playlist))
                .fetch_one(db.pool())
                .await
                .expect("data");
        let map = parse_data(data.as_deref());
        assert_eq!(
            map.get("OwnerUserId").and_then(Value::as_str),
            Some(json_guid(owner).as_str())
        );
        assert_eq!(map.get("OpenAccess").and_then(Value::as_bool), Some(true));
        let children = read_linked_children(&map);
        assert_eq!(children.len(), 1);
        assert_eq!(
            children[0].item_id.as_deref(),
            Some(json_guid(child).as_str())
        );
        assert_eq!(children[0].path.as_deref(), Some("/m/a.mkv"));
        assert_eq!(children[0].child_type, "Manual");
    }

    #[tokio::test]
    async fn sync_boxset_children_are_path_only() {
        let db = test_db().await;
        let boxset = Uuid::new_v4();
        let child = Uuid::new_v4();
        seed_named_item(&db, boxset, BaseItemKind::BoxSet, "Set").await;
        seed_movie_with_path(&db, child, "/m/b.mkv").await;
        sqlx::query(
            r#"INSERT INTO "FerrofinLinkedChildren" ("ParentId", "ChildId", "ChildType", "SortOrder")
               VALUES (?1, ?2, 0, 0)"#,
        )
        .bind(guid_to_db(boxset))
        .bind(guid_to_db(child))
        .execute(db.writer())
        .await
        .expect("edge");

        sync_container_data(&db, boxset).await.expect("sync");

        let data: Option<String> =
            sqlx::query_scalar(r#"SELECT "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(boxset))
                .fetch_one(db.pool())
                .await
                .expect("data");
        let children = read_linked_children(&parse_data(data.as_deref()));
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].path.as_deref(), Some("/m/b.mkv"));
        assert_eq!(children[0].item_id, None, "box-set links are path-only");
    }

    #[tokio::test]
    async fn reconcile_imports_jellyfin_written_data() {
        let db = test_db().await;
        let playlist = Uuid::new_v4();
        let by_id = Uuid::new_v4();
        let by_path = Uuid::new_v4();
        let owner = Uuid::new_v4();
        seed_named_item(&db, playlist, BaseItemKind::Playlist, "FromJf").await;
        seed_movie_with_path(&db, by_id, "/m/one.mkv").await;
        seed_movie_with_path(&db, by_path, "/m/two.mkv").await;
        // A Jellyfin-shaped blob: first child linked by id, second by path only,
        // one share, owner set.
        let blob = format!(
            r#"{{"OwnerUserId":"{}","OpenAccess":false,
                "Shares":[{{"UserId":"{}","CanEdit":true}}],
                "LinkedChildren":[
                  {{"Path":"/m/one.mkv","Type":"Manual","ItemId":"{}"}},
                  {{"Path":"/m/two.mkv","Type":"Manual"}}]}}"#,
            json_guid(owner),
            json_guid(owner),
            json_guid(by_id)
        );
        sqlx::query(r#"UPDATE "BaseItems" SET "Data" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(playlist))
            .bind(&blob)
            .execute(db.writer())
            .await
            .expect("plant blob");

        let (imported, _) = reconcile_container_data(&db).await.expect("reconcile");
        assert_eq!(imported, 1);

        let edges: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT "ChildId", "SortOrder" FROM "FerrofinLinkedChildren"
               WHERE "ParentId" = ?1 ORDER BY "SortOrder""#,
        )
        .bind(guid_to_db(playlist))
        .fetch_all(db.pool())
        .await
        .expect("edges");
        assert_eq!(
            edges,
            vec![(guid_to_db(by_id), 0), (guid_to_db(by_path), 1)],
            "id-linked and path-only children both resolve, in order"
        );
        let (stored_owner, open): (Option<String>, bool) = sqlx::query_as(
            r#"SELECT "OwnerUserId", "OpenAccess" FROM "FerrofinPlaylists" WHERE "PlaylistId" = ?1"#,
        )
        .bind(guid_to_db(playlist))
        .fetch_one(db.pool())
        .await
        .expect("meta");
        assert_eq!(stored_owner.as_deref(), Some(guid_to_db(owner).as_str()));
        assert!(!open);
        let share: (String, bool) = sqlx::query_as(
            r#"SELECT "UserId", "CanEdit" FROM "FerrofinPlaylistShares" WHERE "PlaylistId" = ?1"#,
        )
        .bind(guid_to_db(playlist))
        .fetch_one(db.pool())
        .await
        .expect("share");
        assert_eq!(share, (guid_to_db(owner), true));
    }

    #[tokio::test]
    async fn reconcile_exports_pre_data_ferrofin_rows() {
        let db = test_db().await;
        let boxset = Uuid::new_v4();
        let child = Uuid::new_v4();
        seed_named_item(&db, boxset, BaseItemKind::BoxSet, "Old").await;
        seed_movie_with_path(&db, child, "/m/c.mkv").await;
        // Cache rows exist but Data has no LinkedChildren key — a pre-Data
        // Ferrofin database.
        sqlx::query(
            r#"INSERT INTO "FerrofinLinkedChildren" ("ParentId", "ChildId", "ChildType", "SortOrder")
               VALUES (?1, ?2, 0, 0)"#,
        )
        .bind(guid_to_db(boxset))
        .bind(guid_to_db(child))
        .execute(db.writer())
        .await
        .expect("edge");

        let (_, exported) = reconcile_container_data(&db).await.expect("reconcile");
        assert!(exported >= 1);

        let data: Option<String> =
            sqlx::query_scalar(r#"SELECT "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(boxset))
                .fetch_one(db.pool())
                .await
                .expect("data");
        let children = read_linked_children(&parse_data(data.as_deref()));
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].path.as_deref(), Some("/m/c.mkv"));
    }

    /// A verbatim playlist `Data` blob captured from a real 10.11.8 database.
    const REAL_PLAYLIST_DATA: &str = r#"{"OwnerUserId":"dc68cd3609094b02b1298b1ba641a16a","OpenAccess":true,"Shares":[],"PlaylistMediaType":"Video","IsRoot":false,"LinkedChildren":[{"Path":"/media/synth/movies/Movie 0001 (2020)/Movie 0001 (2020).mkv","Type":"Manual","ItemId":"d37ecb9d75b0c0a8e9ecb0a864ec670e"},{"Path":"/media/synth/movies/Movie 0002 (2020)/Movie 0002 (2020).mkv","Type":"Manual","ItemId":"cadc886bd825b379272a2a9b70b975f1"}],"IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"ExtraIds":[],"DateLastSaved":"2026-08-11T17:47:45.2295144Z","RemoteTrailers":[]}"#;

    /// A verbatim box-set `Data` blob (path-only children) from the same DB.
    const REAL_BOXSET_DATA: &str = r#"{"DisplayOrder":"PremiereDate","LibraryFolderIds":["f137a2dd21bbc1b99aa5c0f6bf02a805"],"IsRoot":false,"LinkedChildren":[{"Path":"/media/synth/movies/Movie 0001 (2020)/Movie 0001 (2020).mkv","Type":"Manual"},{"Path":"/media/synth/movies/Movie 0002 (2020)/Movie 0002 (2020).mkv","Type":"Manual"}],"IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"ExtraIds":[],"DateLastSaved":"2026-08-11T17:47:45.7268844Z","RemoteTrailers":[]}"#;

    #[test]
    fn remote_trailers_merge_dedupes_and_preserves_other_keys() {
        // Starting from a real 10.11.8 blob (empty RemoteTrailers, plus keys
        // Ferrofin does not model), merging must append and keep the rest.
        let merged = merge_remote_trailers(
            Some(REAL_BOXSET_DATA),
            &[(
                Some("Official Trailer".to_owned()),
                "https://www.youtube.com/watch?v=abc".to_owned(),
            )],
        )
        .expect("first merge writes");
        let trailers = read_remote_trailers(Some(&merged));
        assert_eq!(
            trailers,
            vec![(
                Some("Official Trailer".to_owned()),
                "https://www.youtube.com/watch?v=abc".to_owned()
            )]
        );
        let map = parse_data(Some(&merged));
        assert_eq!(
            map.get("DisplayOrder").and_then(Value::as_str),
            Some("PremiereDate")
        );
        assert_eq!(read_linked_children(&map).len(), 2);

        // Re-merging the same URL changes nothing (no pointless write), even
        // under a different name.
        assert!(
            merge_remote_trailers(
                Some(&merged),
                &[(
                    Some("Teaser".to_owned()),
                    "https://www.youtube.com/watch?v=abc".to_owned()
                )]
            )
            .is_none()
        );

        // A new URL appends after the existing one.
        let two = merge_remote_trailers(
            Some(&merged),
            &[(None, "https://www.youtube.com/watch?v=def".to_owned())],
        )
        .expect("second merge writes");
        let trailers = read_remote_trailers(Some(&two));
        assert_eq!(trailers.len(), 2);
        assert_eq!(trailers[1].0, None);
        assert!(trailers[1].1.ends_with("def"));
    }

    #[test]
    fn remote_trailers_read_tolerates_missing_and_malformed() {
        assert!(read_remote_trailers(None).is_empty());
        assert!(read_remote_trailers(Some("not json")).is_empty());
        // Entries without a Url are dropped.
        assert!(read_remote_trailers(Some(r#"{"RemoteTrailers":[{"Name":"x"}]}"#)).is_empty());
    }

    #[test]
    fn parses_real_playlist_data() {
        let map = parse_data(Some(REAL_PLAYLIST_DATA));
        let children = read_linked_children(&map);
        assert_eq!(children.len(), 2);
        assert_eq!(
            children[0].item_id.as_deref(),
            Some("d37ecb9d75b0c0a8e9ecb0a864ec670e")
        );
        assert_eq!(children[0].child_type, "Manual");
        assert!(
            children[0]
                .path
                .as_deref()
                .unwrap()
                .ends_with("Movie 0001 (2020).mkv")
        );
        assert_eq!(
            map.get("OwnerUserId").and_then(Value::as_str),
            Some("dc68cd3609094b02b1298b1ba641a16a")
        );
        assert_eq!(map.get("OpenAccess").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn parses_real_boxset_path_only_children() {
        let map = parse_data(Some(REAL_BOXSET_DATA));
        let children = read_linked_children(&map);
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].item_id, None);
        assert!(children[0].path.is_some());
    }

    #[test]
    fn rewrite_preserves_unknown_keys() {
        let mut map = parse_data(Some(REAL_BOXSET_DATA));
        map.insert("LinkedChildren".to_owned(), serde_json::json!([]));
        let out = data_to_string(&map).expect("serialize");
        let round = parse_data(Some(&out));
        // Jellyfin-only keys survive the rewrite untouched.
        assert_eq!(
            round.get("DisplayOrder").and_then(Value::as_str),
            Some("PremiereDate")
        );
        assert_eq!(
            round.get("DateLastSaved").and_then(Value::as_str),
            Some("2026-08-11T17:47:45.7268844Z")
        );
        assert!(read_linked_children(&round).is_empty());
    }

    #[test]
    fn tolerates_null_empty_and_garbage() {
        assert!(parse_data(None).is_empty());
        assert!(parse_data(Some("")).is_empty());
        assert!(parse_data(Some("not json")).is_empty());
        assert!(parse_data(Some("[1,2]")).is_empty());
        assert!(!has_linked_children_key(&parse_data(None)));
    }

    #[test]
    fn json_guid_is_n_format_lowercase() {
        let id = Uuid::parse_str("D37ECB9D-75B0-C0A8-E9EC-B0A864EC670E").expect("uuid");
        assert_eq!(json_guid(id), "d37ecb9d75b0c0a8e9ecb0a864ec670e");
    }

    #[test]
    fn child_serialization_matches_jellyfin_shape() {
        let child = LinkedChildJson {
            path: Some("/m/x.mkv".to_owned()),
            child_type: "Manual".to_owned(),
            item_id: Some("d37ecb9d75b0c0a8e9ecb0a864ec670e".to_owned()),
            library_item_id: None,
        };
        let json = serde_json::to_string(&child).expect("serialize");
        assert_eq!(
            json,
            r#"{"Path":"/m/x.mkv","Type":"Manual","ItemId":"d37ecb9d75b0c0a8e9ecb0a864ec670e"}"#
        );
    }
}
