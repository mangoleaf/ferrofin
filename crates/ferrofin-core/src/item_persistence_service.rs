//! [`FerrofinItemPersistenceService`] — the concrete [`ItemPersistenceService`].
//!
//! Port of `ItemPersistenceService`. Writes `BaseItems` rows and deletes items.
//! In C# this service maps a domain `BaseItem` onto the entity via
//! `BaseItemMapper` and then saves; here the trait already receives mapped
//! [`BaseItemEntity`] rows (per the persistence-trait port rules), so
//! [`save_items`](FerrofinItemPersistenceService::save_items) is a full-column
//! upsert. Child-collection writes (images, streams, people, item-values) have
//! their own repositories/services; the image write is provided here to satisfy
//! the trait, delegating the row layout to `BaseItemImageInfos`.
//!
//! The `IServerApplicationHost` constructor dependency only supplies path
//! normalization in C# and is not needed to persist already-mapped rows, so it
//! is not taken here.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::{datetime_to_db, guid_to_db, opt_datetime_to_db};
use sqlx::{QueryBuilder, Sqlite};
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::{
    FolderAggregate, ItemChildLink, ItemPathRow, ItemPersistenceService, StoredImageMetadata,
    StoredItemLinks,
};
use std::collections::HashMap;

/// Compare the values the image save actually persists, including the database's
/// 100 ns timestamp precision. A provider returning identical artwork is a no-op.
fn image_row_matches(
    row: &ferrofin_db::entities::base_items::BaseItemImageInfoEntity,
    image: &ItemImageInfo,
) -> bool {
    row.image_type == image_type_to_disc(image.image_type)
        && row.path == image.path
        && row.width == i64::from(image.width)
        && row.height == i64::from(image.height)
        && row.blurhash.as_deref() == image.blur_hash.as_deref().map(str::as_bytes)
        && row.date_modified.map(datetime_to_db).as_deref()
            == Some(datetime_to_db(image.date_modified).as_str())
}

/// One `BaseItemImageInfos` row as the scan's change detection reads it:
/// `ItemId`, `ImageType`, `Path`, `Width`, `Height`, `Blurhash`, `DateModified`.
type ScanImageRow = (
    String,
    i32,
    String,
    i64,
    i64,
    Option<Vec<u8>>,
    Option<chrono::DateTime<chrono::Utc>>,
);

use ferrofin_model::data::BaseItemKind;

use crate::db_error::db_err;
use crate::item_repository::image_type_to_disc;
use crate::item_type_lookup::{MUSIC_GENRE_TYPES, stored_type_name};
use crate::text_util::get_clean_value;
use crate::translate_query::PLACEHOLDER_ID;

/// Rows per partition for the one-shot startup repairs — upstream's
/// `const int Limit = 10000` in every 12.0 `Refresh*` migration routine
/// (`RefreshCleanNamesAndValues`, `RefreshForcedSortNames`). Each partition is
/// read with one keyset query and its rewrites committed as one transaction,
/// which bounds both the memory a pass holds and the length of any single
/// write lock.
const REPAIR_PARTITION: i64 = 10_000;

/// One `Season` row as `recompute_season_keys` reads it:
/// `(Id, SeriesId, IndexNumber, PresentationUniqueKey)`.
type SeasonKeyRow = (String, Option<String>, Option<i64>, Option<String>);

/// Maps an `ItemValues.Type` discriminant to the stored `BaseItems.Type` name of
/// its browsable by-name item, or [`None`] for value types with no browse tab
/// (tags, artists — handled elsewhere).
///
/// Genre (2) is the one that needs a companion: Jellyfin keeps a **separate**
/// `MusicGenre` item for the same value when the owner is a music item — one
/// `ItemValueType`, two browses — and `/MusicGenres` selects on that row type
/// alone. See [`music_genre_row`], which materializes it.
fn by_name_kind(value_type: i32) -> Option<BaseItemKind> {
    match value_type {
        1 => Some(BaseItemKind::MusicArtist),
        2 => Some(BaseItemKind::Genre),
        3 => Some(BaseItemKind::Studio),
        _ => None,
    }
}

/// The `ItemValues.Type` discriminant whose value space a by-name kind lives in
/// — the inverse of [`by_name_kind`]. `MusicGenre` is deliberately absent: it
/// shares `Genre`'s value space, so its row takes a derived id instead of the
/// (already claimed) `ItemValueId`. See [`music_genre_row`].
fn by_name_value_type(kind: BaseItemKind) -> Option<i32> {
    match kind {
        BaseItemKind::MusicArtist => Some(1),
        BaseItemKind::Genre => Some(2),
        BaseItemKind::Studio => Some(3),
        _ => None,
    }
}

/// The stored CLR type name of the row [`by_name_kind`] names.
fn by_name_type_name(value_type: i32) -> Option<&'static str> {
    match value_type {
        // AlbumArtist (1) is the canonical artist identity — it materializes the
        // browsable MusicArtist item (so /Artists + /Artists/AlbumArtists resolve
        // real rows and artist bio/artwork attaches, keyed on MusicBrainzAlbumArtist).
        // Artist (0, track performer) stays an ItemValue for filtering only, so a
        // name that is both doesn't produce two MusicArtist rows.
        1 => stored_type_name(BaseItemKind::MusicArtist),
        2 => stored_type_name(BaseItemKind::Genre),
        3 => stored_type_name(BaseItemKind::Studio),
        _ => None,
    }
}

/// Materializes the browsable `MusicGenre` row for a genre carried by a music
/// item, if the database does not already have one under that name.
///
/// Jellyfin keeps `Genre` and `MusicGenre` as two separate items over the one
/// `ItemValueType`, and `GetMusicGenres` selects on the row type alone
/// (`BaseItemRepository.cs:221`), so without this row `/MusicGenres` is empty.
/// Ferrofin's other by-name rows borrow the `ItemValueId` as their id, which
/// this one cannot — that id already belongs to the `Genre` row for the same
/// value — so it takes a derived id instead, the way Jellyfin derives every
/// by-name id.
///
/// The existence check is by **type and name**, not by id: an adopted database
/// already has Jellyfin's `MusicGenre` rows under Jellyfin's ids, and a scan
/// must not lay a second row beside each of them.
async fn music_genre_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    value: &str,
    clean: &str,
) -> Result<(), ServiceError> {
    let (Some(type_name), Some(id)) = (
        stored_type_name(BaseItemKind::MusicGenre),
        crate::item_type_lookup::derive_item_id(BaseItemKind::MusicGenre, value),
    ) else {
        return Ok(());
    };
    sqlx::query(
        // `PresentationUniqueKey` for the same reason the sibling by-name insert
        // writes one: a by-name row's key is `{Type}-{Name}`
        // (`kinds::presentation_unique_key`) — a real 10.11.8 scanner row reads
        // back as `MusicGenre-Ambient` — this insert bypasses `upsert_item`,
        // nothing else would set it, and the column is what `GetItemValues`
        // groups on. A real 10.11.8 leaves the column NULL for exactly ONE
        // kind — `LiveTvProgram` — so a keyless `MusicGenre` is a Ferrofin-only
        // shape.
        r#"INSERT INTO "BaseItems"
           ("Id","Type","Name","CleanName","SortName","PresentationUniqueKey",
            "IsFolder","IsInMixedFolder",
            "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
           SELECT ?1,?2,?3,?4,?5,?6,0,0,0,0,0,0,0
           WHERE NOT EXISTS (
               SELECT 1 FROM "BaseItems" WHERE "Type" = ?2 AND "CleanName" = ?4)"#,
    )
    .bind(guid_to_db(id))
    .bind(type_name)
    .bind(value)
    .bind(clean)
    .bind(ferrofin_util::sort_name::create_sort_name(value))
    .bind(crate::kinds::presentation_unique_key(
        BaseItemKind::MusicGenre,
        id,
        Some(value),
        None,
        None,
        None,
    ))
    .execute(&mut **tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Inserts a minimal `BaseItems` row of the given folder-ish kind and returns
/// the persisted row. Only the schema-required columns are set; richer metadata
/// is populated by later refreshes (mirrors how the C# path creates a stub item
/// then refreshes it).
pub(crate) async fn insert_named_item(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
    name: &str,
    is_folder: bool,
    container: Option<Uuid>,
) -> Result<BaseItemEntity, ServiceError> {
    let type_name = stored_type_name(kind)
        .ok_or_else(|| ServiceError::backend(format!("no stored type name for {kind:?}")))?;
    sqlx::query(
        // `SortName` persisted, not derived on read. jellyfin-web's Collections
        // and Playlists tabs both send `SortBy=SortName`; with the column NULL
        // they came back in creation order while each DTO still carried a
        // correctly COMPUTED SortName, which is what made this hard to see.
        // `ParentId`/`TopParentId` are what make the item reachable: a query
        // that names no scope is confined to the user's libraries (C#
        // `AddUserToQuery`), so a row with neither is invisible to every user
        // browse. Upstream never creates one — a playlist lands in the
        // `ManualPlaylistsFolder` and a collection in the auto-provisioned
        // "Collections" library — and neither should this.
        // `PresentationUniqueKey` is `BaseItem.CreatePresentationUniqueKey()` —
        // the row's own id in the `N` form — and it is LOAD-BEARING here, not
        // decorative: this row carries a `TopParentId`, so it is inside the
        // recursive user universe, and that universe is queried with
        // `GROUP BY PresentationUniqueKey` whenever a user asks for no
        // particular kind. A keyless row shares the NULL group with every other
        // keyless row (upstream's only ones are guide airings), so it would
        // vanish from the user's own home query behind whichever row the group
        // happened to elect.
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Name", "SortName",
            "PresentationUniqueKey", "ParentId", "TopParentId")
           VALUES (?1, ?2, ?3, 0, 0, 0, 0, 0, 0, ?4, ?5, ?7, ?6, ?6)"#,
    )
    .bind(guid_to_db(id))
    .bind(type_name)
    .bind(i64::from(is_folder))
    .bind(name)
    .bind(ferrofin_util::sort_name::create_sort_name(name))
    .bind(container.map(guid_to_db))
    .bind(crate::kinds::presentation_unique_key(
        kind,
        id,
        Some(name),
        None,
        None,
        None,
    ))
    .execute(db.writer())
    .await
    .map_err(db_err)?;

    sqlx::query_as::<_, BaseItemEntity>(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .fetch_one(db.pool())
        .await
        .map_err(db_err)
}

/// The id of the container row stored at `path`, or `None`.
///
/// **Matched by exact path**, the way `CollectionManager.EnsureLibraryFolder`
/// does (`FindFolders(path)`), and never by type: `CollectionFolder` is the type
/// of *every* library, so a type match would file collections into whichever one
/// sorted first. Two spellings are accepted because Jellyfin writes the literal
/// `%AppDataPath%` token where Ferrofin writes the resolved path — both are
/// equalities, not patterns, so a user library that happens to be called
/// `collections` cannot be mistaken for this.
pub(crate) async fn container_at(db: &Database, path: &str) -> Result<Option<Uuid>, ServiceError> {
    let leaf = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let jellyfin_form = format!("{JELLYFIN_DATA_PATH_TOKEN}/{leaf}");
    let existing: Option<String> = sqlx::query_scalar(
        r#"SELECT "Id" FROM "BaseItems" WHERE "Path" IN (?1, ?2) ORDER BY "Id" LIMIT 1"#,
    )
    .bind(path)
    .bind(&jellyfin_form)
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)?;
    existing
        .map(|id| {
            Uuid::parse_str(&id).map_err(|e| {
                ServiceError::backend(format!("container row {id} has an unusable id: {e}"))
            })
        })
        .transpose()
}

/// The `BaseItems` row a user-created container hangs off, provisioning it if
/// this server has never had one.
///
/// Upstream never leaves a created item parentless: `CreateCollectionAsync`
/// goes through `EnsureLibraryFolder`, which auto-creates a container at
/// `{data}/collections` on first use, and a playlist lands in the one at
/// `{data}/playlists`. Ferrofin had neither link, so every collection and
/// playlist it created was an orphan — reachable only by a query that names no
/// scope, and invisible the moment one does.
///
/// Matched by exact path — see [`container_at`] for why by path and never by
/// type, and which two spellings are accepted.
pub(crate) async fn ensure_container(
    db: &Database,
    kind: BaseItemKind,
    name: &str,
    path: &str,
    mode: &crate::item_type_lookup::IdDerivation,
    parent: Option<Uuid>,
    data: Option<&str>,
) -> Result<Option<Uuid>, ServiceError> {
    if let Some(id) = container_at(db, path).await? {
        // Adopt a row that was created before the user root existed — the
        // parent is set on the first provision that CAN set it, rather than
        // staying null forever because the row is already there.
        if let Some(root) = parent {
            attach_to_root(db, id, root).await?;
        }
        // …and the same for the `Data` blob, which an older Ferrofin left NULL:
        // without it the row's `CollectionType` reads as absent forever.
        if let Some(data) = data {
            backfill_container_data(db, id, data).await?;
        }
        return Ok(Some(id));
    }

    // Derived from the path, like every other folder id on both sides, so the
    // same directory yields the same id wherever it is scanned — under the
    // database's CONFIGURED derivation, not a hardcoded one.
    //
    // CORRECTION, measured on the parity pair 2026-08-31. This comment used to
    // claim "Jellyfin computes its own id for `%AppDataPath%/collections`". It
    // does not, and the claim is only true for the PLAYLISTS folder. Upstream's
    // Collections library is created by `AddVirtualFolder`, so its row's `Path`
    // is the shortcut directory `{RootFolderPath}/default/Collections` and its
    // id is `GetNewItemIdInternal("root\default\Collections")` =
    // `9d7ad6afe9afa2dab1a2f6e00ad28fa6`; Ferrofin's is derived from
    // `{data}/collections` = `6f929a39bd27711ce6208fb0aef66e5b`. Both were read
    // off the live pair. The playlists folder DOES match byte for byte
    // (`1071671e7bffa0532e930debee501d2e`, `/config/data/playlists`) on both
    // servers, because `CreateRootFolder` builds that one directly.
    //
    // TODO(open-work, tracked on `GET /Library/MediaFolders` in
    // suite/parity/classifications.json): route the COLLECTIONS provision
    // through the virtual-folder path (`{root}/default/Collections` +
    // `.mblink` -> `{data}/collections`, registered so it shows in
    // `GET /Library/VirtualFolders`) so the id converges. That is an id change
    // for existing Ferrofin databases, so it needs a migration that re-parents
    // the box sets hanging off the old id — which is why it is a named work
    // item here and not a drive-by edit. Until then, adopting a Jellyfin
    // database means `container_at` misses upstream's row and provisions a
    // second Collections library beside it.
    let Some(id) = crate::item_type_lookup::derive_item_id_with(mode, kind, path) else {
        return Ok(None);
    };
    // Jellyfin's row describes a directory that exists; the scanner and the
    // library-structure endpoints both expect to find it.
    if let Err(e) = tokio::fs::create_dir_all(path).await {
        tracing::warn!(path, %e, "could not create the container directory");
    }
    // …and it hangs off the user root, which is what puts it in
    // `GetUserRootFolder().Children`.
    //
    // Only if that row is actually there: `BaseItems.ParentId` is a foreign key
    // to `BaseItems.Id`, and the root is provisioned lazily too, so a container
    // created before it would fail the insert outright. Going without the parent
    // is recoverable — `attach_to_root` above sets it on the first provision
    // that finds the root in place — where a failed creation is not.
    let parent = match parent {
        Some(p) if row_exists(db, p).await? => Some(p),
        _ => None,
    };
    insert_named_item(db, id, kind, name, true, parent).await?;
    set_container_path(db, id, path).await?;
    if let Some(data) = data {
        backfill_container_data(db, id, data).await?;
    }
    Ok(Some(id))
}

/// The full `BaseItems` row for a container id, or `None`.
///
/// Lives here rather than in the manager that wants it because SQL belongs
/// behind the persistence boundary (`crates/ferrofin-db/tests/sql_boundary.rs`).
pub(crate) async fn container_row(
    db: &Database,
    id: Uuid,
) -> Result<Option<BaseItemEntity>, ServiceError> {
    sqlx::query_as::<_, BaseItemEntity>(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .fetch_optional(db.pool())
        .await
        .map_err(db_err)
}

/// Writes the `Data` blob a provisioned container needs, but **only over an
/// empty one**.
///
/// `Data` is where 10.11.8 keeps a `CollectionFolder`'s `CollectionType` — the
/// auto-provisioned Collections library is created by
/// `CollectionManager.EnsureLibraryFolder` through
/// `AddVirtualFolder(name, CollectionTypeOptions.boxsets, …)`
/// (v10.11.8 Emby.Server.Implementations/Collections/CollectionManager.cs:81-109),
/// and a real Jellyfin row carries `{"…","CollectionType":"boxsets",…}` there.
/// Ferrofin provisioned the row with no `Data` at all, so `DtoService`'s
/// `collection_type_of` found nothing and the folder went out with a null
/// `CollectionType` on `/Items?parentId={root}`, `/UserViews` and
/// `/Library/MediaFolders` where Jellyfin sends `boxsets` — measured on the pair
/// 2026-08-31.
///
/// The `Data IS NULL OR = ''` guard is what makes this safe on an ADOPTED
/// database: Jellyfin's own blob carries `PhysicalLocationsList`,
/// `PhysicalFolderIds` and the rest, and overwriting it with a one-key document
/// would destroy the library's physical paths on swap-back.
async fn backfill_container_data(db: &Database, id: Uuid, data: &str) -> Result<(), ServiceError> {
    sqlx::query(
        r#"UPDATE "BaseItems" SET "Data" = ?2
           WHERE "Id" = ?1 AND ("Data" IS NULL OR "Data" = '')"#,
    )
    .bind(guid_to_db(id))
    .bind(data)
    .execute(db.writer())
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Parents a container to the root row it belongs under, if it is not there
/// already and that root row exists.
///
/// The existence guard matters: `BaseItems.ParentId` is a foreign key, so
/// pointing at a root that has not been provisioned yet would fail the
/// statement. The `<>` arm is what moves a container an earlier version filed
/// under the wrong root — the playlists folder belongs under the
/// `AggregateFolder` (`CreateRootFolder`), and Ferrofin used to hang it off the
/// `UserRootFolder`. A container's root is fixed by design, so re-aiming it is
/// idempotent rather than destructive.
async fn attach_to_root(db: &Database, id: Uuid, root: Uuid) -> Result<(), ServiceError> {
    if !row_exists(db, root).await? {
        return Ok(());
    }
    sqlx::query(
        r#"UPDATE "BaseItems" SET "ParentId" = ?2
           WHERE "Id" = ?1 AND ("ParentId" IS NULL OR "ParentId" <> ?2)"#,
    )
    .bind(guid_to_db(id))
    .bind(guid_to_db(root))
    .execute(db.writer())
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Whether a `BaseItems` row with this id exists.
pub(crate) async fn row_exists(db: &Database, id: Uuid) -> Result<bool, ServiceError> {
    let found: Option<String> =
        sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_optional(db.pool())
            .await
            .map_err(db_err)?;
    Ok(found.is_some())
}

/// Inserts one of the two root folders (`AggregateFolder`/`UserRootFolder`) —
/// a parentless, top-parentless folder row at a fixed path.
///
/// Port of the row `LibraryManager.CreateRootFolder()` persists: `Name`,
/// `SortName` and `CleanName` all the directory's own name, `Path` the
/// directory, `PresentationUniqueKey` the id in simple form. The NOT NULL flag
/// columns are spelled out because the pinned 10.11.8 schema gives them no
/// defaults.
pub(crate) async fn insert_root_folder(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
    name: &str,
    path: &str,
) -> Result<(), ServiceError> {
    let type_name = stored_type_name(kind)
        .ok_or_else(|| ServiceError::backend(format!("no stored type name for {kind:?}")))?;
    sqlx::query(
        r#"INSERT OR IGNORE INTO "BaseItems"
             ("Id", "Type", "Name", "SortName", "CleanName", "Path",
              "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
              "IsRepeat", "IsSeries", "IsVirtualItem",
              "PresentationUniqueKey", "DateCreated")
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 0, 0, 0, 0, 0, 0, ?7, ?8)"#,
    )
    .bind(guid_to_db(id))
    .bind(type_name)
    .bind(name)
    .bind(ferrofin_util::sort_name::create_sort_name(name))
    .bind(name.to_lowercase())
    .bind(path)
    .bind(id.as_simple().to_string())
    .bind(ferrofin_db::store::datetime_to_db(chrono::Utc::now()))
    .execute(db.writer())
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Moves a container row onto a different id (and stored type), carrying
/// everything that points at it.
///
/// Insert-then-repoint-then-delete, in that order: `BaseItems` children and
/// `AncestorIds` rows are re-aimed while both rows exist, so the final `DELETE`
/// cascades over nothing. Rewriting the primary key in place would silently
/// orphan every child instead — `BaseItems` is the target of ten
/// `ON DELETE CASCADE` foreign keys.
pub(crate) async fn rekey_container(
    db: &Database,
    legacy: Uuid,
    correct: Uuid,
    kind: BaseItemKind,
) -> Result<(), ServiceError> {
    let type_name = stored_type_name(kind)
        .ok_or_else(|| ServiceError::backend(format!("no stored type name for {kind:?}")))?;
    let (legacy_db, correct_db) = (guid_to_db(legacy), guid_to_db(correct));
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    sqlx::query(
        r#"INSERT INTO "BaseItems"
             ("Id", "Type", "Name", "SortName", "CleanName", "Path",
              "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
              "IsRepeat", "IsSeries", "IsVirtualItem",
              "ParentId", "TopParentId", "PresentationUniqueKey", "DateCreated")
           SELECT ?2, ?3, "Name", "SortName", "CleanName", "Path",
                  "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
                  "IsRepeat", "IsSeries", "IsVirtualItem",
                  "ParentId", ?2, ?4, "DateCreated"
             FROM "BaseItems" WHERE "Id" = ?1"#,
    )
    .bind(&legacy_db)
    .bind(&correct_db)
    .bind(type_name)
    .bind(correct.as_simple().to_string())
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    for sql in [
        r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "ParentId" = ?1"#,
        r#"UPDATE "BaseItems" SET "TopParentId" = ?2 WHERE "TopParentId" = ?1"#,
        r#"UPDATE OR IGNORE "AncestorIds" SET "ParentItemId" = ?2 WHERE "ParentItemId" = ?1"#,
        r#"UPDATE OR IGNORE "AncestorIds" SET "ItemId" = ?2 WHERE "ItemId" = ?1"#,
    ] {
        sqlx::query(sql)
            .bind(&legacy_db)
            .bind(&correct_db)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
    }
    // Last, once nothing points at it any more, so the ON DELETE CASCADE has
    // nothing left to take with it.
    sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(&legacy_db)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    tx.commit().await.map_err(db_err)
}

/// Parents a plug-in folder to the `AggregateFolder` and makes it its own top
/// parent — `LibraryManager.CreateRootFolder`'s
/// `folder.ParentId = rootFolder.Id`.
///
/// Scoped to the two states a Ferrofin server can have produced (parentless, or
/// hung off the `UserRootFolder`), so a row deliberately parented elsewhere is
/// never moved.
pub(crate) async fn reparent_virtual_child(
    db: &Database,
    id: Uuid,
    aggregate: Uuid,
    user_root: Uuid,
) -> Result<(), ServiceError> {
    sqlx::query(
        r#"UPDATE "BaseItems" SET "ParentId" = ?2, "TopParentId" = ?1
           WHERE "Id" = ?1 AND ("ParentId" IS NULL OR "ParentId" = ?3)"#,
    )
    .bind(guid_to_db(id))
    .bind(guid_to_db(aggregate))
    .bind(guid_to_db(user_root))
    .execute(db.writer())
    .await
    .map_err(db_err)?;
    Ok(())
}

/// The literal Jellyfin writes into `BaseItems.Path` in place of the data
/// directory (`%AppDataPath%/collections`), which Ferrofin stores resolved.
const JELLYFIN_DATA_PATH_TOKEN: &str = "%AppDataPath%";

/// Stamps a provisioned container with its directory and makes it its own top
/// parent, the shape Jellyfin's `%AppDataPath%/collections` row has.
async fn set_container_path(db: &Database, id: Uuid, path: &str) -> Result<(), ServiceError> {
    sqlx::query(r#"UPDATE "BaseItems" SET "Path" = ?2, "TopParentId" = ?1 WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .bind(path)
        .execute(db.writer())
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Puts the orphans an older Ferrofin created into `container`.
///
/// Only rows with **neither** a parent nor a top parent are touched: those can
/// only have come from `insert_named_item` before it linked anything. A row
/// adopted from Jellyfin already sits somewhere real and is left alone.
pub(crate) async fn adopt_orphans(
    db: &Database,
    kind: BaseItemKind,
    container: Uuid,
) -> Result<(), ServiceError> {
    let Some(type_name) = stored_type_name(kind) else {
        return Ok(());
    };
    sqlx::query(
        r#"UPDATE "BaseItems" SET "ParentId" = ?2, "TopParentId" = ?2
           WHERE "Type" = ?1 AND "ParentId" IS NULL AND "TopParentId" IS NULL
             AND "Id" <> ?2"#,
    )
    .bind(type_name)
    .bind(guid_to_db(container))
    .execute(db.writer())
    .await
    .map_err(db_err)?;
    Ok(())
}

/// The concrete item-persistence service.
#[derive(Clone)]
pub struct FerrofinItemPersistenceService {
    db: Database,
    /// The debounced `LibraryChanged` push. Set by the composition root.
    ///
    /// The hook lives HERE and not only on `LibraryManager` because that is
    /// where Jellyfin's `ItemUpdated` fires from — `BaseItem.UpdateToRepositoryAsync`,
    /// i.e. the repository save, which every writer funnels through. Hooking
    /// only the library manager's own `update_items` caught API metadata edits
    /// and missed provider, view and media-source writes entirely.
    ///
    /// A `OnceLock` rather than a constructor argument: this service is built
    /// long before the event bus the notifier publishes on exists.
    changed: std::sync::OnceLock<Arc<crate::library_changed_notifier::LibraryChangedNotifier>>,
}

impl std::fmt::Debug for FerrofinItemPersistenceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinItemPersistenceService")
            .finish_non_exhaustive()
    }
}

impl FerrofinItemPersistenceService {
    /// Creates the service over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self {
            db,
            changed: std::sync::OnceLock::new(),
        }
    }

    /// Attaches the `LibraryChanged` notifier so every item save announces
    /// itself. Scans are deliberately NOT announced through here — they use
    /// [`save_scanned_items`](ItemPersistenceService::save_scanned_items) and
    /// publish one folded event at scan end instead, so a full scan cannot
    /// accumulate a row per item for the length of the debounce.
    /// Later calls are ignored — which notifier a save announces on is a
    /// wiring decision, not a runtime one.
    pub fn set_change_notifier(
        &self,
        notifier: Arc<crate::library_changed_notifier::LibraryChangedNotifier>,
    ) {
        let _ = self.changed.set(notifier);
    }

    /// Whether the one-shot pass recorded under `key` in `FerrofinMeta` has
    /// already run on this database.
    async fn repair_done(&self, key: &str) -> Result<bool, ServiceError> {
        let done = self
            .db
            .meta_get(key)
            .await
            .map_err(|e| ServiceError::Backend(e.to_string()))?;
        Ok(done.as_deref() == Some("1"))
    }

    /// Records the one-shot pass under `key` as complete.
    async fn mark_repair_done(&self, key: &str) -> Result<(), ServiceError> {
        self.db
            .meta_set(key, "1")
            .await
            .map_err(|e| ServiceError::Backend(e.to_string()))
    }

    /// One-shot startup pass — the port of Jellyfin 12.0's
    /// `RefreshCleanNamesAndValues` migration routine
    /// (`Jellyfin.Server/Migrations/Routines/20260610120000_RefreshCleanNamesAndValues.cs`).
    ///
    /// Rewrites `BaseItems.CleanName` from `Name` (rows with a non-empty
    /// name, `Id` order) and then `ItemValues.CleanValue` from `Value`
    /// (non-empty values, `ItemValueId` order) with [`get_clean_value`], in
    /// partitions of [`REPAIR_PARTITION`] rows, writing only the rows whose
    /// stored column disagrees — exactly the routine's loop, with each
    /// partition's updates committed as one transaction where EF's
    /// `SaveChangesAsync` runs per partition upstream. A whitespace-only source
    /// stores the empty string, as upstream's `IsNullOrWhiteSpace ?
    /// string.Empty : GetCleanValue()` does.
    ///
    /// Completion is recorded in `FerrofinMeta` under `clean_values_v12` so
    /// later boots skip the pass. The key is new on purpose: the earlier marker
    /// (`clean_values_keep_punctuation_v1`) recorded the 10.11.8 rule — fold
    /// and lower-case, punctuation kept — and must not suppress this one.
    ///
    /// Why once per database: 12.0's `GetCleanValue` replaces punctuation with
    /// spaces, and the query translator now computes that form, so every
    /// stored `'h. jon benjamin'` would miss a lookup for `'h jon benjamin'`
    /// until rewritten. A database Jellyfin 12.0 wrote already agrees and
    /// reads back untouched (0 rewrites).
    ///
    /// Returns the number of rows rewritten (names + values).
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when a read or a partition's write fails.
    /// The marker is only written after the last partition, so a failed boot
    /// resumes the whole pass next time; every step is idempotent.
    pub async fn repair_clean_values(&self) -> Result<u64, ServiceError> {
        const META_KEY: &str = "clean_values_v12";
        if self.repair_done(META_KEY).await? {
            return Ok(0);
        }
        let repaired = self.refresh_clean_names().await? + self.refresh_clean_values().await?;
        self.mark_repair_done(META_KEY).await?;
        Ok(repaired)
    }

    /// `RefreshCleanNamesAsync`: `BaseItems.CleanName` from `Name`, for every
    /// row with a non-empty name, in `Id` order. Returns the rows rewritten.
    async fn refresh_clean_names(&self) -> Result<u64, ServiceError> {
        let mut repaired: u64 = 0;
        let mut last_id = String::new();
        loop {
            let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
                r#"SELECT "Id", "Name", "CleanName" FROM "BaseItems"
                   WHERE "Name" IS NOT NULL AND "Name" <> '' AND "Id" > ?1
                   ORDER BY "Id" LIMIT ?2"#,
            )
            .bind(&last_id)
            .bind(REPAIR_PARTITION)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
            let Some((tail, _, _)) = rows.last() else {
                break;
            };
            last_id.clone_from(tail);
            let full_partition = rows.len() == usize::try_from(REPAIR_PARTITION).unwrap_or(0);

            let mut tx = self.db.writer().begin().await.map_err(db_err)?;
            for (id, name, stored) in &rows {
                let want = if name.trim().is_empty() {
                    String::new()
                } else {
                    get_clean_value(name)
                };
                if stored.as_deref() == Some(want.as_str()) {
                    continue;
                }
                sqlx::query(r#"UPDATE "BaseItems" SET "CleanName" = ?2 WHERE "Id" = ?1"#)
                    .bind(id)
                    .bind(&want)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                repaired += 1;
            }
            tx.commit().await.map_err(db_err)?;
            tracing::debug!(
                through = %last_id,
                repaired,
                "clean-name repair partition committed"
            );
            if !full_partition {
                break;
            }
        }
        Ok(repaired)
    }

    /// `RefreshCleanValuesAsync`: `ItemValues.CleanValue` from `Value`, for
    /// every row with a non-empty value, in `ItemValueId` order. Returns the
    /// rows rewritten.
    async fn refresh_clean_values(&self) -> Result<u64, ServiceError> {
        let mut repaired: u64 = 0;
        let mut last_id = String::new();
        loop {
            let rows: Vec<(String, String, String)> = sqlx::query_as(
                r#"SELECT "ItemValueId", "Value", "CleanValue" FROM "ItemValues"
                   WHERE "Value" <> '' AND "ItemValueId" > ?1
                   ORDER BY "ItemValueId" LIMIT ?2"#,
            )
            .bind(&last_id)
            .bind(REPAIR_PARTITION)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
            let Some((tail, _, _)) = rows.last() else {
                break;
            };
            last_id.clone_from(tail);
            let full_partition = rows.len() == usize::try_from(REPAIR_PARTITION).unwrap_or(0);

            let mut tx = self.db.writer().begin().await.map_err(db_err)?;
            for (id, value, stored) in &rows {
                let want = if value.trim().is_empty() {
                    String::new()
                } else {
                    get_clean_value(value)
                };
                if *stored == want {
                    continue;
                }
                sqlx::query(
                    r#"UPDATE "ItemValues" SET "CleanValue" = ?2 WHERE "ItemValueId" = ?1"#,
                )
                .bind(id)
                .bind(&want)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                repaired += 1;
            }
            tx.commit().await.map_err(db_err)?;
            tracing::debug!(
                through = %last_id,
                repaired,
                "clean-value repair partition committed"
            );
            if !full_partition {
                break;
            }
        }
        Ok(repaired)
    }

    /// One-shot startup pass — the port of Jellyfin 12.0's
    /// `RefreshForcedSortNames` migration routine
    /// (`Jellyfin.Server/Migrations/Routines/20260722120000_RefreshForcedSortNames.cs`).
    ///
    /// For every `BaseItems` row with a non-empty `ForcedSortName` (`Id`
    /// order, partitions of [`REPAIR_PARTITION`]) recomputes `SortName` as
    /// `GetSortName(ForcedSortName, Type != Person)` —
    /// [`crate::kinds::forced_sort_name_for`] — and writes it only when it
    /// differs from the stored value. 10.11.8 derived a forced sort name with
    /// `ModifySortChunks(ForcedSortName).ToLowerInvariant()`, keeping the
    /// article and the punctuation; 12.0 runs it through the full cleaning
    /// pipeline so a forced `"The Spider-Man: Homecoming"` sorts next to the
    /// auto-generated key (jellyfin#17388). A `Person` row keeps its override
    /// verbatim apart from `TrimStart()`.
    ///
    /// Completion is recorded in `FerrofinMeta` under `forced_sort_names_v12`.
    /// Returns the number of rows rewritten.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when a read or a partition's write fails; the
    /// marker is written only after the last partition, so the pass resumes on
    /// the next boot.
    pub async fn repair_forced_sort_names(&self) -> Result<u64, ServiceError> {
        const META_KEY: &str = "forced_sort_names_v12";
        if self.repair_done(META_KEY).await? {
            return Ok(0);
        }
        let mut repaired: u64 = 0;
        let mut last_id = String::new();
        loop {
            let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
                r#"SELECT "Id", "Type", "ForcedSortName", "SortName" FROM "BaseItems"
                   WHERE "ForcedSortName" IS NOT NULL AND "ForcedSortName" <> '' AND "Id" > ?1
                   ORDER BY "Id" LIMIT ?2"#,
            )
            .bind(&last_id)
            .bind(REPAIR_PARTITION)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
            let Some((tail, _, _, _)) = rows.last() else {
                break;
            };
            last_id.clone_from(tail);
            let full_partition = rows.len() == usize::try_from(REPAIR_PARTITION).unwrap_or(0);

            let mut tx = self.db.writer().begin().await.map_err(db_err)?;
            for (id, type_name, forced, stored) in &rows {
                // Upstream: `enableAlphaNumericSorting = Type != typeof(Person)`
                // — every other type, known to Ferrofin or not, takes the
                // alphanumeric branch.
                let want = match crate::item_type_lookup::kind_from_type_name(type_name) {
                    Some(kind) => crate::kinds::forced_sort_name_for(kind, forced),
                    None => ferrofin_util::sort_name::forced_sort_key(forced),
                };
                if stored.as_deref() == Some(want.as_str()) {
                    continue;
                }
                tracing::debug!(
                    item_id = %id,
                    old = ?stored,
                    new = %want,
                    "forced sort name recomputed"
                );
                sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = ?2 WHERE "Id" = ?1"#)
                    .bind(id)
                    .bind(&want)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                repaired += 1;
            }
            tx.commit().await.map_err(db_err)?;
            if !full_partition {
                break;
            }
        }
        self.mark_repair_done(META_KEY).await?;
        Ok(repaired)
    }

    /// One-shot startup pass: recomputes `InheritedParentalRatingValue` /
    /// `InheritedParentalRatingSubValue` from every row's OWN `OfficialRating`
    /// — Jellyfin 12.0's `MigrateRatingLevels` routine
    /// (`Jellyfin.Server/Migrations/Routines/20260302090000_MigrateRatingLevels.cs`),
    /// recorded in `FerrofinMeta` so later boots skip it.
    ///
    /// In one transaction, for each DISTINCT `OfficialRating`: a null/empty
    /// rating clears both columns; any other resolves through the localization
    /// manager's `GetRatingScore` (server default country) and writes its
    /// `Score`/`SubScore` — both NULL when the string does not resolve. Upstream
    /// writes from the item's own rating only, with no parent walk, and so
    /// does this. Returns the number of rows updated.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when a query or the transaction fails; the
    /// marker is written inside the transaction, so a failure retries on the
    /// next boot.
    pub async fn repair_rating_levels(
        &self,
        localization: &dyn ferrofin_traits::localization::LocalizationManager,
    ) -> Result<u64, ServiceError> {
        // 12.1 re-dated `MigrateRatingLevels` (its `GetRatingScore` changed:
        // whole-value lookup first, unrated parts skipped, case-insensitive
        // tables), so the pass runs once more under a new key.
        const META_KEY: &str = "rating_levels_v121";
        if self.repair_done(META_KEY).await? {
            return Ok(0);
        }
        let ratings: Vec<Option<String>> =
            sqlx::query_scalar(r#"SELECT DISTINCT "OfficialRating" FROM "BaseItems""#)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        let mut updated: u64 = 0;
        for rating in ratings {
            let result = match rating.as_deref().filter(|r| !r.is_empty()) {
                None => sqlx::query(
                    r#"UPDATE "BaseItems"
                       SET "InheritedParentalRatingValue" = NULL,
                           "InheritedParentalRatingSubValue" = NULL
                       WHERE "OfficialRating" IS NULL OR "OfficialRating" = ''"#,
                )
                .execute(&mut *tx)
                .await
                .map_err(db_err)?,
                Some(rating) => {
                    let score = localization.get_rating_score(rating, None);
                    sqlx::query(
                        r#"UPDATE "BaseItems"
                           SET "InheritedParentalRatingValue" = ?2,
                               "InheritedParentalRatingSubValue" = ?3
                           WHERE "OfficialRating" = ?1"#,
                    )
                    .bind(rating)
                    .bind(score.map(|s| i64::from(s.score)))
                    .bind(score.and_then(|s| s.sub_score).map(i64::from))
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?
                }
            };
            updated += result.rows_affected();
        }
        Self::mark_repair_done_in(&mut tx, META_KEY).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(updated)
    }

    /// One-shot startup pass: recomputes every `Series`' `PresentationUniqueKey`
    /// under 12.0's rule ([`crate::kinds::series_presentation_unique_key`]) —
    /// Jellyfin 12.0's `RecomputeSeriesPresentationKey` routine
    /// (`Jellyfin.Server/Migrations/Routines/20260821120000_RecomputeSeriesPresentationKey.cs`),
    /// recorded in `FerrofinMeta` so later boots skip it.
    ///
    /// Why keys move: 12.0 falls back to `series-{name}` when grouping is on
    /// and the series has no provider id (10.11 kept the own id), and ORDERS
    /// the library-folder ids before joining them. For every series the new
    /// key is computed from its own name, provider ids, resolved metadata
    /// language and collection folders ([`series_key_scope`] over `folders`);
    /// when it differs the series row is updated and every row whose
    /// `SeriesId` is the series is re-pointed (scoped by `SeriesId`,
    /// deliberately not by the old key, which several libraries can share).
    /// Then every `Season` with an `IndexNumber` whose series was processed
    /// gets `{series key}-{index:000}`. Returns the rows changed.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when a query or the transaction fails; the
    /// marker is written inside the transaction, so a failure retries on the
    /// next boot.
    pub async fn repair_series_presentation_keys(
        &self,
        folders: &[ferrofin_model::entities_media::VirtualFolderInfo],
        default_metadata_language: &str,
    ) -> Result<u64, ServiceError> {
        const META_KEY: &str = "series_presentation_keys_v12";
        if self.repair_done(META_KEY).await? {
            return Ok(0);
        }
        #[allow(clippy::type_complexity)]
        let series: Vec<(
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(
            r#"SELECT "Id", "Name", "Path", "TopParentId", "PreferredMetadataLanguage",
                      "PresentationUniqueKey"
               FROM "BaseItems" WHERE "Type" = ?1"#,
        )
        .bind(stored_type_name(BaseItemKind::Series).unwrap_or_default())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        let ids: Vec<Uuid> = series
            .iter()
            .filter_map(|row| Uuid::parse_str(&row.0).ok())
            .collect();
        let provider_ids = self.provider_ids_for_items(&ids).await?;

        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        let mut updated: u64 = 0;
        // Every processed series, changed or not — a season whose own key went
        // stale under an unchanged series key is repaired too (upstream fills
        // `newSeriesKeys` before the equality check).
        let mut new_keys: std::collections::HashMap<String, String> =
            std::collections::HashMap::with_capacity(series.len());
        for (id, name, path, top_parent_id, own_language, stored_key) in series {
            let Ok(uuid) = Uuid::parse_str(&id) else {
                continue;
            };
            let scope = series_key_scope(
                folders,
                default_metadata_language,
                top_parent_id.as_deref(),
                path.as_deref(),
                own_language.as_deref(),
            );
            let key = crate::kinds::series_presentation_unique_key(
                uuid,
                scope.enable_automatic_series_grouping,
                name.as_deref(),
                provider_ids.get(&uuid).map_or(&[][..], Vec::as_slice),
                scope.preferred_metadata_language.as_deref(),
                &scope.collection_folder_ids,
            );
            new_keys.insert(id.clone(), key.clone());
            if stored_key.as_deref() == Some(key.as_str()) {
                continue;
            }
            sqlx::query(r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = ?2 WHERE "Id" = ?1"#)
                .bind(&id)
                .bind(&key)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            updated += 1;
            updated += sqlx::query(
                r#"UPDATE "BaseItems" SET "SeriesPresentationUniqueKey" = ?2 WHERE "SeriesId" = ?1"#,
            )
            .bind(&id)
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?
            .rows_affected();
        }
        updated += Self::recompute_season_keys(&mut tx, &new_keys, None).await?;
        Self::mark_repair_done_in(&mut tx, META_KEY).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(updated)
    }

    /// `RecomputeSeasonsAsync`: every `Season` with an `IndexNumber` whose
    /// `SeriesId` is in `series_keys` gets `{series key}-{index:000}`
    /// (`Season.CreatePresentationUniqueKey`) when it does not already carry
    /// it. A season without an index number keeps the base key, which carries
    /// no series key at all. Returns the rows changed.
    ///
    /// `only_series` narrows the season read to one series' rows (the
    /// scan-time re-point, once per refreshed series); the boot repair passes
    /// `None` and reads every season once.
    async fn recompute_season_keys(
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        series_keys: &std::collections::HashMap<String, String>,
        only_series: Option<&str>,
    ) -> Result<u64, ServiceError> {
        let seasons: Vec<SeasonKeyRow> = sqlx::query_as(
            r#"SELECT "Id", "SeriesId", "IndexNumber", "PresentationUniqueKey"
               FROM "BaseItems"
               WHERE "Type" = ?1 AND "IndexNumber" IS NOT NULL
                 AND (?2 IS NULL OR "SeriesId" = ?2)"#,
        )
        .bind(stored_type_name(BaseItemKind::Season).unwrap_or_default())
        .bind(only_series)
        .fetch_all(&mut **tx)
        .await
        .map_err(db_err)?;
        let mut updated: u64 = 0;
        for (id, series_id, index, stored_key) in seasons {
            let Some((series_key, index)) = series_id
                .as_deref()
                .and_then(|sid| series_keys.get(sid))
                .zip(index)
            else {
                continue;
            };
            let key = format!("{series_key}-{index:03}");
            if stored_key.as_deref() == Some(key.as_str()) {
                continue;
            }
            sqlx::query(r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = ?2 WHERE "Id" = ?1"#)
                .bind(&id)
                .bind(&key)
                .execute(&mut **tx)
                .await
                .map_err(db_err)?;
            updated += 1;
        }
        Ok(updated)
    }

    /// Records the one-shot repair `key` as done, inside the repair's own
    /// transaction so a failed pass retries on the next boot.
    async fn mark_repair_done_in(
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        key: &str,
    ) -> Result<(), ServiceError> {
        sqlx::query(
            r#"INSERT INTO "FerrofinMeta" ("Key", "Value") VALUES (?1, '1')
               ON CONFLICT("Key") DO UPDATE SET "Value" = '1'"#,
        )
        .bind(key)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Upserts a single item row (`INSERT … ON CONFLICT("Id") DO UPDATE`) using
    /// `sql` — [`UPSERT_SQL`] for a full-row replace, [`scan_upsert_sql`] for
    /// the library scan's ownership-respecting variant. Both bind the same
    /// columns in the same order: [`written_columns`]'s, then the save time.
    async fn upsert_item(
        &self,
        item: &BaseItemEntity,
        sql: &'static str,
    ) -> Result<(), ServiceError> {
        let mut query = sqlx::query(sql);
        for (_, value) in written_columns(item) {
            query = match value {
                Column::Text(v) => query.bind(v),
                Column::Int(v) => query.bind(v),
                Column::Real(v) => query.bind(v),
                Column::Bool(v) => query.bind(v),
            };
        }
        query
            .bind(datetime_to_db(chrono::Utc::now()))
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

/// One `BaseItems` column value as [`FerrofinItemPersistenceService`] binds
/// it: dates already in [`datetime_to_db`]'s text form, so two values compare
/// equal exactly when the stored text would.
#[derive(Debug, Clone, PartialEq)]
enum Column {
    Text(Option<String>),
    Int(Option<i64>),
    Real(Option<f64>),
    Bool(bool),
}

/// The column values the upsert binds for `item`, in [`UPSERT_SQL`]'s
/// column (= bind) order (an update writes the save time into
/// `DateLastSaved` instead of this value). The write-time derivations are
/// applied here, so this is exactly what reaches the table.
fn written_columns(item: &BaseItemEntity) -> Vec<(&'static str, Column)> {
    // C# `SaveItem` always stamps `CleanName = GetCleanValue(item.Name)` at
    // write time (no caller pre-computes it); deriving here keeps every
    // saved item matchable by the search filter, which queries `CleanName`.
    let clean_name = item
        .name
        .as_deref()
        .filter(|n| !n.is_empty())
        .map(crate::text_util::get_clean_value);
    let presentation_unique_key = derive_presentation_key(item);
    // Same reasoning for `SortName`, and it is why this belongs here rather
    // than at each call site. In C# `SortName` is not a field a caller can
    // forget: `BaseItem.SortName` is a lazy property that resolves to
    // `GetSortName(ForcedSortName, EnableAlphaNumericSorting, config)` or
    // `CreateSortName()` on first read, so `SaveItems` can never persist a
    // null. Modelled as a plain `Option` on the entity, every construction
    // site *could* forget — and several did, leaving 7,191 of 9,865 rows
    // with `SortName IS NULL`. That is not merely an unsorted list:
    // `nameStartsWith` filters `lower(SortName)` (faithfully to C#
    // `ApplyNameFilters`), so a NULL row matches nothing and the A-Z picker
    // returned `TotalRecordCount: 0` for types that had hundreds of rows.
    //
    // A caller-supplied value always wins — that is what carries the
    // per-kind `CreateSortName` overrides (episode/season) the scanner
    // computes, which drive the client's play queue.
    //
    // Both fallbacks go through the kind-aware helpers in `kinds`, not
    // `create_sort_name` / `forced_sort_key` directly, because `GetSortName`
    // has a per-kind branch: `Person` overrides `EnableAlphaNumericSorting
    // => false` and keeps its name — or its forced sort name — verbatim.
    // Deriving the generic key for a `Person` here lower-cased rows the
    // people repository had written correctly.
    let sort_kind = crate::item_type_lookup::kind_from_type_name(&item.type_);
    let sort_name = item.sort_name.clone().or_else(|| {
        let forced = item.forced_sort_name.as_deref().filter(|f| !f.is_empty());
        match forced {
            Some(f) => Some(match sort_kind {
                Some(kind) => crate::kinds::forced_sort_name_for(kind, f),
                None => ferrofin_util::sort_name::forced_sort_key(f),
            }),
            None => item.name.as_deref().map(|n| match sort_kind {
                Some(kind) => crate::kinds::sort_name_for(kind, n),
                None => ferrofin_util::sort_name::create_sort_name(n),
            }),
        }
    });
    columns_of(item, clean_name, presentation_unique_key, sort_name)
}

/// `item`'s columns with the three derived ones given, in [`UPSERT_SQL`]'s
/// order.
fn columns_of(
    item: &BaseItemEntity,
    clean_name: Option<String>,
    presentation_unique_key: Option<String>,
    sort_name: Option<String>,
) -> Vec<(&'static str, Column)> {
    use Column::{Bool, Int, Real, Text};
    let text = |v: &Option<String>| Text(v.clone());
    let date = |v: Option<chrono::DateTime<chrono::Utc>>| Text(opt_datetime_to_db(v));
    vec![
        ("Id", Text(Some(item.id.clone()))),
        ("Album", text(&item.album)),
        ("AlbumArtists", text(&item.album_artists)),
        ("Artists", text(&item.artists)),
        ("Audio", Int(item.audio.map(i64::from))),
        ("ChannelId", text(&item.channel_id)),
        ("CleanName", Text(clean_name)),
        ("CommunityRating", Real(item.community_rating)),
        ("CriticRating", Real(item.critic_rating)),
        ("CustomRating", text(&item.custom_rating)),
        ("Data", text(&item.data)),
        ("DateCreated", date(item.date_created)),
        ("DateLastMediaAdded", date(item.date_last_media_added)),
        ("DateLastRefreshed", date(item.date_last_refreshed)),
        // The value an INSERT stores; an update binds the save time instead
        // (`?73`).
        ("DateLastSaved", date(item.date_last_saved)),
        ("DateModified", date(item.date_modified)),
        ("EndDate", date(item.end_date)),
        ("EpisodeTitle", text(&item.episode_title)),
        ("ExternalId", text(&item.external_id)),
        ("ExternalSeriesId", text(&item.external_series_id)),
        ("ExternalServiceId", text(&item.external_service_id)),
        ("ExtraType", Int(item.extra_type.map(i64::from))),
        ("ForcedSortName", text(&item.forced_sort_name)),
        ("Genres", text(&item.genres)),
        ("Height", Int(item.height)),
        ("IndexNumber", Int(item.index_number)),
        (
            "InheritedParentalRatingSubValue",
            Int(item.inherited_parental_rating_sub_value),
        ),
        (
            "InheritedParentalRatingValue",
            Int(item.inherited_parental_rating_value),
        ),
        ("IsFolder", Bool(item.is_folder)),
        ("IsInMixedFolder", Bool(item.is_in_mixed_folder)),
        ("IsLocked", Bool(item.is_locked)),
        ("IsMovie", Bool(item.is_movie)),
        ("IsRepeat", Bool(item.is_repeat)),
        ("IsSeries", Bool(item.is_series)),
        ("IsVirtualItem", Bool(item.is_virtual_item)),
        ("LUFS", Real(item.lufs)),
        ("MediaType", text(&item.media_type)),
        ("Name", text(&item.name)),
        ("NormalizationGain", Real(item.normalization_gain)),
        ("OfficialRating", text(&item.official_rating)),
        ("OriginalLanguage", text(&item.original_language)),
        ("OriginalTitle", text(&item.original_title)),
        ("Overview", text(&item.overview)),
        ("OwnerId", text(&item.owner_id)),
        ("ParentId", text(&item.parent_id)),
        ("ParentIndexNumber", Int(item.parent_index_number)),
        ("Path", text(&item.path)),
        (
            "PreferredMetadataCountryCode",
            text(&item.preferred_metadata_country_code),
        ),
        (
            "PreferredMetadataLanguage",
            text(&item.preferred_metadata_language),
        ),
        ("PremiereDate", date(item.premiere_date)),
        ("PresentationUniqueKey", Text(presentation_unique_key)),
        ("PrimaryVersionId", text(&item.primary_version_id)),
        ("ProductionLocations", text(&item.production_locations)),
        ("ProductionYear", Int(item.production_year)),
        ("RunTimeTicks", Int(item.run_time_ticks)),
        ("SeasonId", text(&item.season_id)),
        ("SeasonName", text(&item.season_name)),
        ("SeriesId", text(&item.series_id)),
        ("SeriesName", text(&item.series_name)),
        (
            "SeriesPresentationUniqueKey",
            text(&item.series_presentation_unique_key),
        ),
        ("ShowId", text(&item.show_id)),
        ("Size", Int(item.size)),
        ("SortName", Text(sort_name)),
        ("StartDate", date(item.start_date)),
        ("Studios", text(&item.studios)),
        ("Tagline", text(&item.tagline)),
        ("Tags", text(&item.tags)),
        ("TopParentId", text(&item.top_parent_id)),
        ("TotalBitrate", Int(item.total_bitrate)),
        ("Type", Text(Some(item.type_.clone()))),
        ("UnratedType", text(&item.unrated_type)),
        ("Width", Int(item.width)),
    ]
}

/// Whether the library scan's save of `saved` over the row it read back as
/// `stored` would change any column other than `DateLastSaved` — the
/// "something changed" test of the scan's save rule for the row itself.
///
/// It evaluates [`scan_upsert_sql`]'s `SET` clause in Rust over the values
/// the upsert would bind ([`written_columns`], with the write-time
/// derivations) against the stored ones (as stored, no derivation):
/// `PrimaryVersionId` is never written, `DateCreated` only fills a gap,
/// `IsLocked` only rises, the never-cleared columns keep the stored value
/// over a `NULL`, and a locked row keeps its user-owned columns (its `Data`
/// taking only the resolver's `VideoType`, [`LOCKED_DATA_SQL`]). With
/// `writes_date_created` it evaluates [`scan_upsert_date_created_sql`]
/// instead, whose `DateCreated` is never cleared but otherwise written.
pub(crate) fn scan_save_changes_row(
    saved: &BaseItemEntity,
    stored: &BaseItemEntity,
    writes_date_created: bool,
) -> bool {
    let incoming = written_columns(saved);
    let current = columns_of(
        stored,
        stored.clean_name.clone(),
        stored.presentation_unique_key.clone(),
        stored.sort_name.clone(),
    );
    let is_null = |v: &Column| {
        matches!(
            v,
            Column::Text(None) | Column::Int(None) | Column::Real(None)
        )
    };
    incoming
        .iter()
        .zip(&current)
        .any(|((name, new), (_, old))| {
            let result = match *name {
                // The save stamps it; that is not a change of the item.
                "DateLastSaved" | "PrimaryVersionId" => old,
                "DateCreated" if writes_date_created && is_null(new) => old,
                "DateCreated" if !writes_date_created && !is_null(old) => old,
                "IsLocked" => {
                    if stored.is_locked {
                        old
                    } else {
                        new
                    }
                }
                col if SCAN_NEVER_CLEARED_COLUMNS.contains(&col) && is_null(new) => old,
                // `LOCKED_DATA_SQL`: only the resolver's `VideoType` moves.
                "Data" if stored.is_locked => {
                    return crate::item_data::with_resolved_video_type(
                        stored.data.as_deref(),
                        saved.data.as_deref(),
                    )
                    .is_some();
                }
                "Name" | "CleanName" | "SortName" if saved.owner_id.is_some() => new,
                col if stored.is_locked && LOCKED_PRESERVED_COLUMNS.contains(&col) => old,
                _ => new,
            };
            result != old
        })
}

#[async_trait]
impl ItemPersistenceService for FerrofinItemPersistenceService {
    async fn delete_items(&self, ids: &[Uuid]) -> Result<Vec<Uuid>, ServiceError> {
        // Upstream's `ItemPersistenceService.DeleteItem`
        // (`ItemPersistenceService.cs:51-157`): the whole closure — the
        // items, their `ParentId` descendants and the extras they own
        // (`OwnerId`), to a fixed point — in one transaction.
        let given: Vec<String> = ids
            .iter()
            .map(|id| guid_to_db(*id))
            .filter(|id| id != PLACEHOLDER_ID)
            .collect();
        if given.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        // Two BaseItems references have no `ON DELETE CASCADE`: an extra's
        // `OwnerId`, and `LinkedChildren` (the item as a playlist or
        // collection and as a member). The closure holds every row either
        // points at, and the links are cleared below, so nothing dangles at
        // commit; deferring the checks to it lets the rows go in any order.
        sqlx::query("PRAGMA defer_foreign_keys = ON")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        let mut closure: Vec<String> = Vec::new();
        let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();
        for chunk in given.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = format!(
                r#"SELECT "Id" FROM "BaseItems" WHERE "Id" IN ({})"#,
                numbered_placeholders(chunk.len())
            );
            let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql.as_str()));
            for id in chunk {
                query = query.bind(id);
            }
            for id in query.fetch_all(&mut *tx).await.map_err(db_err)? {
                if known.insert(id.clone()) {
                    closure.push(id);
                }
            }
        }
        let mut frontier = closure.clone();
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for chunk in frontier.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
                let sql = child_links_sql(chunk.len());
                let mut query = sqlx::query_as::<
                    _,
                    (String, Option<String>, Option<String>, Option<String>),
                >(sqlx::AssertSqlSafe(sql.as_str()));
                for id in chunk {
                    query = query.bind(id);
                }
                for (id, ..) in query.fetch_all(&mut *tx).await.map_err(db_err)? {
                    // Only ids not seen yet go on, so ownership cycles end.
                    if id != PLACEHOLDER_ID && known.insert(id.clone()) {
                        next.push(id);
                    }
                }
            }
            closure.extend(next.iter().cloned());
            frontier = next;
        }
        // The containers whose membership shrinks, for their `Data` re-sync
        // after the commit (the deleted ones among them no-op there).
        let mut containers: Vec<String> = Vec::new();
        for chunk in closure.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let ids = numbered_placeholders(chunk.len());
            let select = format!(
                r#"SELECT DISTINCT "ParentId" FROM "LinkedChildren" WHERE "ChildId" IN ({ids})"#
            );
            let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(select.as_str()));
            for id in chunk {
                query = query.bind(id);
            }
            containers.extend(query.fetch_all(&mut *tx).await.map_err(db_err)?);
            for sql in [
                format!(r#"DELETE FROM "LinkedChildren" WHERE "ParentId" IN ({ids})"#),
                format!(r#"DELETE FROM "LinkedChildren" WHERE "ChildId" IN ({ids})"#),
            ] {
                let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
                for id in chunk {
                    query = query.bind(id);
                }
                query.execute(&mut *tx).await.map_err(db_err)?;
            }
        }
        // Deepest first: a row's descendants were found after it.
        for chunk in closure.rchunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = format!(
                r#"DELETE FROM "BaseItems" WHERE "Id" IN ({})"#,
                numbered_placeholders(chunk.len())
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
            for id in chunk {
                query = query.bind(id);
            }
            query.execute(&mut *tx).await.map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        containers.sort_unstable();
        containers.dedup();
        for container in containers.iter().filter(|c| !known.contains(*c)) {
            if let Ok(container) = Uuid::parse_str(container) {
                crate::item_data::sync_container_data(&self.db, container).await?;
            }
        }
        Ok(closure
            .iter()
            .filter_map(|id| Uuid::parse_str(id).ok())
            .collect())
    }

    async fn save_items(&self, items: &[BaseItemEntity]) -> Result<(), ServiceError> {
        for item in items {
            self.upsert_item(item, UPSERT_SQL).await?;
        }
        // `ItemUpdated`. An item the caller also reported as ADDED is folded back
        // out of the updated bucket by the notifier, so the library manager's
        // `create_items` hook still wins for a creation.
        if let Some(changed) = self.changed.get() {
            changed.record_updated(items);
        }
        Ok(())
    }

    async fn save_scanned_items(&self, items: &[BaseItemEntity]) -> Result<(), ServiceError> {
        for item in items {
            self.upsert_item(item, scan_upsert_sql()).await?;
        }
        Ok(())
    }

    async fn save_scanned_items_with_date_created(
        &self,
        items: &[BaseItemEntity],
    ) -> Result<(), ServiceError> {
        for item in items {
            self.upsert_item(item, scan_upsert_date_created_sql())
                .await?;
        }
        Ok(())
    }

    async fn set_primary_version_id(
        &self,
        item_id: Uuid,
        primary_version_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        // C# `Video.SetPrimaryVersionId` also rewrites the presentation key,
        // and `Video.CreatePresentationUniqueKey` returns the PRIMARY's id when
        // there is one, else the item's own — both in the "N" (32 hex, no
        // hyphen) form. That shared key is what makes every copy of a film
        // count as one item in "similar", Next Up and the resume rows; leaving
        // it stale makes a merged group behave as separate titles.
        let presentation_key = primary_version_id
            .unwrap_or(item_id)
            .as_simple()
            .to_string();
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?1, "PresentationUniqueKey" = ?2
               WHERE "Id" = ?3"#,
        )
        .bind(primary_version_id.map(guid_to_db))
        .bind(&presentation_key)
        .bind(guid_to_db(item_id))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        // 12.0 keeps the version link in `LinkedChildren` too — that is what
        // `GetLocalAlternateVersionIds` / `GetLinkedAlternateVersions` read —
        // so the pointer and the row move together (C# `MergeVersions` +
        // `RefreshMetadataForVersions`): linking writes a
        // Local(2)/LinkedAlternateVersion(3) row under the primary, unlinking
        // removes the item's version rows.
        sqlx::query(
            r#"DELETE FROM "LinkedChildren" WHERE "ChildId" = ?1 AND "ChildType" IN (2, 3)"#,
        )
        .bind(guid_to_db(item_id))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if let Some(primary) = primary_version_id {
            let child_type = alternate_version_child_type(&mut tx, item_id, primary).await?;
            sqlx::query(
                r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
                   VALUES (?1,
                       (SELECT COALESCE(MAX("SortOrder"), -1) + 1
                        FROM "LinkedChildren" WHERE "ParentId" = ?1),
                       ?2, ?3)"#,
            )
            .bind(guid_to_db(primary))
            .bind(guid_to_db(item_id))
            .bind(child_type)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn provider_ids_for_items(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<(String, String)>>, ServiceError> {
        let mut map: std::collections::HashMap<Uuid, Vec<(String, String)>> =
            std::collections::HashMap::new();
        if item_ids.is_empty() {
            return Ok(map);
        }
        let stored: Vec<String> = item_ids.iter().copied().map(guid_to_db).collect();
        for (item_id, key, value) in self
            .db
            .provider_ids_for_items(&stored)
            .await
            .map_err(ServiceError::from)?
        {
            if let Ok(id) = Uuid::parse_str(&item_id) {
                map.entry(id).or_default().push((key, value));
            }
        }
        Ok(map)
    }

    async fn repoint_series_children(
        &self,
        series_id: Uuid,
        key: &str,
    ) -> Result<u64, ServiceError> {
        let stored = guid_to_db(series_id);
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        let mut updated = sqlx::query(
            r#"UPDATE "BaseItems" SET "SeriesPresentationUniqueKey" = ?2
               WHERE "SeriesId" = ?1 AND "SeriesPresentationUniqueKey" IS NOT ?2"#,
        )
        .bind(&stored)
        .bind(key)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        let keys = std::collections::HashMap::from([(stored.clone(), key.to_owned())]);
        updated += Self::recompute_season_keys(&mut tx, &keys, Some(&stored)).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(updated)
    }

    async fn save_provider_id(
        &self,
        item_id: Uuid,
        provider: &str,
        value: &str,
    ) -> Result<(), ServiceError> {
        // One row per (item, provider key) — the table's primary key — so a
        // re-save replaces the value (the C# `ProviderIds[key] = value` write).
        sqlx::query(
            r#"INSERT OR REPLACE INTO "BaseItemProviders"
               ("ItemId", "ProviderId", "ProviderValue") VALUES (?1, ?2, ?3)"#,
        )
        .bind(guid_to_db(item_id))
        .bind(provider)
        .bind(value)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn replace_provider_ids(
        &self,
        item_id: Uuid,
        ids: &[(String, String)],
    ) -> Result<(), ServiceError> {
        let id = guid_to_db(item_id);
        // One transaction so the clear+rewrite is atomic on the single writer
        // connection (same shape as `set_ancestors`).
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "BaseItemProviders" WHERE "ItemId" = ?1"#)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for (provider, value) in ids {
            // Blank keys/values are not ids (the C# `SetProviderId` drops them).
            if provider.trim().is_empty() || value.trim().is_empty() {
                continue;
            }
            sqlx::query(
                r#"INSERT OR REPLACE INTO "BaseItemProviders"
                   ("ItemId", "ProviderId", "ProviderValue") VALUES (?1, ?2, ?3)"#,
            )
            .bind(&id)
            .bind(provider)
            .bind(value)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn set_parent_id(&self, item_id: Uuid, parent_id: Uuid) -> Result<(), ServiceError> {
        let id = guid_to_db(item_id);
        let parent = guid_to_db(parent_id);
        // Read first on the pool: the steady state (already parented) must not
        // touch the single writer connection.
        let current: Option<Option<String>> =
            sqlx::query_scalar(r#"SELECT "ParentId" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&id)
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        match current {
            None => return Ok(()),
            Some(Some(existing)) if existing.eq_ignore_ascii_case(&parent) => return Ok(()),
            Some(_) => {}
        }
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(&id)
            .bind(&parent)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn set_collection_type(
        &self,
        item_id: Uuid,
        collection_type: &str,
    ) -> Result<(), ServiceError> {
        let id = guid_to_db(item_id);
        // Read on the pool first: the steady state (already recorded) must not
        // touch the single writer connection.
        let current: Option<Option<String>> =
            sqlx::query_scalar(r#"SELECT "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&id)
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        let Some(stored) = current else {
            return Ok(()); // no such row
        };
        // Merge into whatever the blob already holds — `Data` also carries
        // `PhysicalFolderIds`/`ViewType`/`DisplayParentId` on some rows, and
        // replacing it wholesale would drop them.
        let mut data = stored
            .as_deref()
            .and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        if data
            .get("CollectionType")
            .and_then(serde_json::Value::as_str)
            == Some(collection_type)
        {
            return Ok(());
        }
        if let Some(obj) = data.as_object_mut() {
            obj.insert(
                "CollectionType".to_owned(),
                serde_json::Value::String(collection_type.to_owned()),
            );
        }
        sqlx::query(r#"UPDATE "BaseItems" SET "Data" = ?2 WHERE "Id" = ?1"#)
            .bind(&id)
            .bind(data.to_string())
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn save_item_values(
        &self,
        item_id: Uuid,
        values: &[(i32, String)],
    ) -> Result<(), ServiceError> {
        let id = guid_to_db(item_id);
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        // Whether this item's genres are *music* genres, which get their own
        // by-name row (see `music_genre_row`). One `SELECT` on the primary key,
        // on a write path that already runs several statements per item.
        let owner_type: Option<String> =
            sqlx::query_scalar(r#"SELECT "Type" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        let owner_is_music = owner_type.is_some_and(|t| MUSIC_GENRE_TYPES.contains(&t.as_str()));
        // Rewrite this item's links; the shared ItemValues rows are kept.
        sqlx::query(r#"DELETE FROM "ItemValuesMap" WHERE "ItemId" = ?1"#)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for (type_, value) in values {
            if value.is_empty() {
                continue;
            }
            let clean = crate::text_util::get_clean_value(value);
            // Get-or-create the (Type, Value) row (unique index on Type+Value).
            let new_id = guid_to_db(Uuid::new_v4());
            sqlx::query(
                r#"INSERT OR IGNORE INTO "ItemValues" ("ItemValueId","CleanValue","Type","Value")
                   VALUES (?1,?2,?3,?4)"#,
            )
            .bind(&new_id)
            .bind(&clean)
            .bind(type_)
            .bind(value)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            let value_id: String = sqlx::query_scalar(
                r#"SELECT "ItemValueId" FROM "ItemValues" WHERE "Type" = ?1 AND "Value" = ?2"#,
            )
            .bind(type_)
            .bind(value)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            sqlx::query(
                r#"INSERT OR IGNORE INTO "ItemValuesMap" ("ItemValueId","ItemId") VALUES (?1,?2)"#,
            )
            .bind(&value_id)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            // Materialize the browsable by-name item (genre/studio) sharing the
            // ItemValueId as its id, so the Genres/Studios tabs list it, the
            // /Genres/{name} lookup resolves it, and a `GenreIds=<id>` filter
            // (which resolves the id → BaseItems.CleanName) matches. Faithful to
            // Jellyfin, where genres/studios are BaseItems; here their id is the
            // shared value id the DTO layer already emits for genre_items.
            // A music item's genre materializes ONLY as a `MusicGenre` row.
            // Upstream keeps the two disjoint — `LibraryManager.GetMusicGenre`
            // for audio, `GetGenre` otherwise — so a real 10.11.8 library has
            // `Ambient` as a MusicGenre and nothing else; writing a `Genre` row
            // too returns the name twice from `/Search/Hints` and lists a
            // phantom entry on the Genres tab.
            let music_genre =
                owner_is_music && *type_ == i32::from(ferrofin_db::enums::ItemValueType::Genre);
            if let (false, Some(type_name)) = (music_genre, by_name_type_name(*type_)) {
                sqlx::query(
                    // `SortName` persisted, not derived on read — see
                    // `people_repository`. Without it the Genres/Studios tabs
                    // (which sort on it) come back unsorted and
                    // `nameStartsWith` matches nothing.
                    // `PresentationUniqueKey` too: a by-name row's key is
                    // `{Type}-{Name}` (see `kinds::presentation_unique_key`),
                    // and this insert bypasses `upsert_item`, so without it
                    // the column stays NULL where Jellyfin writes
                    // `Genre-Action` — 23,186 such rows on a real library.
                    // The existence guard is by **type and name**, not by id
                    // (`OR IGNORE` keys on the PRIMARY KEY, which is the
                    // `ItemValueId` — a fresh guid every first write). A
                    // `MusicArtist` the scanner resolved from an artist
                    // DIRECTORY already carries that CleanName under its
                    // path-derived id, and `item_repository::push_by_name_join`
                    // joins `agg.cval = bi."CleanName"`, so a second row here
                    // would list every artist TWICE on /Artists. Same shape as
                    // `music_genre_row`, and the same reason an adopted
                    // Jellyfin database (whose by-name rows carry Jellyfin's
                    // ids) must not get a duplicate laid beside each row.
                    r#"INSERT INTO "BaseItems"
                       ("Id","Type","Name","CleanName","SortName","PresentationUniqueKey",
                        "IsFolder","IsInMixedFolder",
                        "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
                       SELECT ?1,?2,?3,?4,?5,?6,?7,0,0,0,0,0,0
                       WHERE NOT EXISTS (
                           SELECT 1 FROM "BaseItems" WHERE "Type" = ?2 AND "CleanName" = ?4)"#,
                )
                .bind(&value_id)
                .bind(type_name)
                .bind(value)
                .bind(&clean)
                .bind(ferrofin_util::sort_name::create_sort_name(value))
                .bind(by_name_kind(*type_).map(|kind| {
                    crate::kinds::presentation_unique_key(
                        kind,
                        Uuid::parse_str(&value_id).unwrap_or_default(),
                        Some(value),
                        None,
                        None,
                        None,
                    )
                }))
                // `IsFolder` follows the C# class, not the by-name-ness:
                // `MusicArtist : Folder`, but `Genre`/`MusicGenre`/`Studio`
                // are plain `BaseItem`s, so the controller's
                // `if (item.IsFolder) result.IsFolder = true;` never fires for
                // them and a genre hint carries no `IsFolder` at all.
                .bind(i32::from(
                    by_name_kind(*type_) == Some(BaseItemKind::MusicArtist),
                ))
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }
            if music_genre {
                music_genre_row(&mut tx, value, &clean).await?;
            }
        }
        tx.commit().await.map_err(db_err)
    }

    async fn ensure_by_name_item(
        &self,
        kind: BaseItemKind,
        name: &str,
        path: &str,
    ) -> Result<Option<Uuid>, ServiceError> {
        let name = name.trim();
        let (Some(type_name), false) = (stored_type_name(kind), name.is_empty()) else {
            return Ok(None);
        };
        let clean = get_clean_value(name);
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        // An existing row wins, whatever id it carries — Jellyfin's own on an
        // adopted database, or the one a scan materialized.
        let existing: Option<String> = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems" WHERE "Type" = ?1 AND "CleanName" = ?2 LIMIT 1"#,
        )
        .bind(type_name)
        .bind(&clean)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        if let Some(id) = existing {
            tx.rollback().await.map_err(db_err)?;
            return Ok(Uuid::parse_str(&id).ok());
        }
        // Mint the id the SCANNER would mint for this name, not a fresh one, so
        // that a later scan of content carrying this value converges on THIS
        // row instead of inserting a second one beside it: the `ItemValues` id
        // for the value-backed kinds (what `save_item_values` uses), the
        // derived id for `MusicGenre` (what `music_genre_row` uses).
        let id = match by_name_value_type(kind) {
            Some(value_type) => {
                let fresh = guid_to_db(Uuid::new_v4());
                sqlx::query(
                    r#"INSERT OR IGNORE INTO "ItemValues" ("ItemValueId","CleanValue","Type","Value")
                       VALUES (?1,?2,?3,?4)"#,
                )
                .bind(&fresh)
                .bind(&clean)
                .bind(value_type)
                .bind(name)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                let value_id: String = sqlx::query_scalar(
                    r#"SELECT "ItemValueId" FROM "ItemValues" WHERE "Type" = ?1 AND "Value" = ?2"#,
                )
                .bind(value_type)
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
                Uuid::parse_str(&value_id).ok()
            }
            None if kind == BaseItemKind::MusicGenre => {
                crate::item_type_lookup::derive_item_id(BaseItemKind::MusicGenre, name)
            }
            None => None,
        };
        let Some(id) = id else {
            tx.rollback().await.map_err(db_err)?;
            return Ok(None);
        };
        sqlx::query(
            // `IsFolder` is 0: `Genre`, `MusicGenre` and `Studio` all derive
            // from `BaseItem`, not `Folder`, and a parentless `MusicArtist` is
            // `IsAccessedByName`, whose `IsFolder` is `!IsAccessedByName`
            // (`MusicArtist.cs:33`). A real 10.11.8 stores 0 on the row a GET
            // lazily creates.
            r#"INSERT OR IGNORE INTO "BaseItems"
               ("Id","Type","Name","CleanName","SortName","PresentationUniqueKey","Path",
                "DateCreated","DateModified",
                "IsFolder","IsInMixedFolder","IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8,0,0,0,0,0,0,0)"#,
        )
        .bind(guid_to_db(id))
        .bind(type_name)
        .bind(name)
        .bind(&clean)
        .bind(ferrofin_util::sort_name::create_sort_name(name))
        .bind(crate::kinds::presentation_unique_key(
            kind,
            id,
            Some(name),
            None,
            None,
            None,
        ))
        .bind(path)
        .bind(chrono::Utc::now())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(Some(id))
    }

    async fn by_name_rows_without_path(
        &self,
        kind: BaseItemKind,
    ) -> Result<Vec<(Uuid, String)>, ServiceError> {
        let Some(type_name) = stored_type_name(kind) else {
            return Ok(Vec::new());
        };
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT "Id", "Name" FROM "BaseItems"
               WHERE "Type" = ?1 AND "Path" IS NULL AND "Name" IS NOT NULL"#,
        )
        .bind(type_name)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, name)| Uuid::parse_str(&id).ok().map(|id| (id, name)))
            .collect())
    }

    async fn set_item_path(&self, id: Uuid, path: &str) -> Result<(), ServiceError> {
        sqlx::query(r#"UPDATE "BaseItems" SET "Path" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .bind(path)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError> {
        let exists: Option<i64> =
            sqlx::query_scalar(r#"SELECT 1 FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(exists.is_some())
    }

    async fn set_ancestors(
        &self,
        item_id: Uuid,
        ancestor_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        let id = guid_to_db(item_id);
        // One transaction so the clear+rewrite is atomic on a single connection —
        // otherwise the DELETE and INSERTs land on different pool connections and
        // can interleave with a concurrent rebuild, and `INSERT OR IGNORE` makes
        // a duplicate ancestor (or a lost race) a no-op instead of a UNIQUE 500.
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "AncestorIds" WHERE "ItemId" = ?1"#)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for ancestor in ancestor_ids {
            sqlx::query(
                r#"INSERT OR IGNORE INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#,
            )
            .bind(&id)
            .bind(guid_to_db(*ancestor))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn save_images(&self, item: &BaseItemEntity) -> Result<(), ServiceError> {
        // The image rows live in BaseItemImageInfos and are owned by their own
        // repository; without the domain item's ImageInfos list on the entity
        // there is nothing to persist here beyond confirming the item exists.
        // (Full image persistence lands with the image repository unit.)
        let exists: Option<i64> =
            sqlx::query_scalar(r#"SELECT 1 FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&item.id)
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        if exists.is_none() {
            return Err(ServiceError::not_found(format!("item {}", item.id)));
        }
        Ok(())
    }

    async fn save_item_images(
        &self,
        item_id: Uuid,
        images: &[ItemImageInfo],
    ) -> Result<(), ServiceError> {
        use ferrofin_db::entities::base_items::BaseItemImageInfoEntity;
        let item = guid_to_db(item_id);
        // Reserve the writer before reading: a concurrent image edit must not
        // invalidate a deferred transaction's snapshot before its first write.
        let mut tx = self
            .db
            .writer()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(db_err)?;
        let mut stored = sqlx::query_as::<_, BaseItemImageInfoEntity>(
            r#"SELECT * FROM "BaseItemImageInfos" WHERE "ItemId" = ?1"#,
        )
        .bind(&item)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        // Preserve exact matches first, including their row ids and therefore
        // the stable order of multiple Backdrops. Consume matches one-to-one
        // so duplicate paths do not hide additions or removals.
        let mut changed = Vec::new();
        for image in images {
            if let Some(index) = stored.iter().position(|row| image_row_matches(row, image)) {
                stored.swap_remove(index);
            } else {
                changed.push(image);
            }
        }
        for image in changed {
            // A changed file's metadata updates its own row; a new path/type
            // gets a new row. The other images remain untouched.
            let id = stored
                .iter()
                .position(|row| {
                    row.image_type == image_type_to_disc(image.image_type) && row.path == image.path
                })
                .map_or_else(
                    || guid_to_db(Uuid::new_v4()),
                    |index| stored.swap_remove(index).id,
                );
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos"
                   ("Id", "ItemId", "ImageType", "Path", "Width", "Height", "Blurhash", "DateModified")
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                   ON CONFLICT("Id") DO UPDATE SET
                     "Width" = excluded."Width", "Height" = excluded."Height",
                     "Blurhash" = excluded."Blurhash", "DateModified" = excluded."DateModified""#,
            )
            .bind(id)
            .bind(&item)
            .bind(image_type_to_disc(image.image_type))
            .bind(&image.path)
            .bind(i64::from(image.width))
            .bind(i64::from(image.height))
            .bind(image.blur_hash.as_deref().map(str::as_bytes))
            .bind(datetime_to_db(image.date_modified))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        for row in stored {
            sqlx::query(r#"DELETE FROM "BaseItemImageInfos" WHERE "Id" = ?1"#)
                .bind(row.id)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn image_metadata_for_items(
        &self,
        item_ids: &[Uuid],
    ) -> Result<Vec<StoredImageMetadata>, ServiceError> {
        let mut out: Vec<StoredImageMetadata> = Vec::new();
        if item_ids.is_empty() {
            return Ok(out);
        }
        // Chunked to stay under SQLite's bound-parameter ceiling, the same
        // 500-wide shape every other batched id lookup here uses.
        for chunk in item_ids.chunks(500) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT "Path", "Width", "Height", "Blurhash", "DateModified"
                   FROM "BaseItemImageInfos" WHERE "ItemId" IN ({placeholders})"#
            );
            let mut query = sqlx::query_as::<
                _,
                (
                    String,
                    i64,
                    i64,
                    Option<Vec<u8>>,
                    Option<chrono::DateTime<chrono::Utc>>,
                ),
            >(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
            out.extend(
                rows.into_iter()
                    .map(
                        |(path, width, height, blurhash, date_modified)| StoredImageMetadata {
                            path,
                            width: i32::try_from(width).unwrap_or(0),
                            height: i32::try_from(height).unwrap_or(0),
                            // Stored as a UTF-8 byte blob; an empty or non-UTF-8 blob
                            // reads back as "no blurhash", which forces a recompute.
                            blur_hash: blurhash
                                .filter(|b| !b.is_empty())
                                .and_then(|b| String::from_utf8(b).ok()),
                            // A row with no stored mtime can never match the file's,
                            // so it falls through to a recompute — the same outcome
                            // C# reaches for a `default(DateTime)` image.
                            date_modified: date_modified.unwrap_or_else(|| {
                                chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0)
                                    .unwrap_or_else(chrono::Utc::now)
                            }),
                        },
                    ),
            );
        }
        Ok(out)
    }

    async fn scan_stored_links(
        &self,
        item_ids: &[Uuid],
    ) -> Result<Option<HashMap<Uuid, StoredItemLinks>>, ServiceError> {
        use ferrofin_model::entities::MediaStreamType;
        let mut out: HashMap<Uuid, StoredItemLinks> = HashMap::with_capacity(item_ids.len());
        let key = |id: &str| Uuid::parse_str(id).ok();
        for chunk in item_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let images = format!(
                r#"SELECT "ItemId", "ImageType", "Path", "Width", "Height", "Blurhash", "DateModified"
                   FROM "BaseItemImageInfos" WHERE "ItemId" IN ({placeholders})
                   ORDER BY "ItemId", "ImageType", "Id""#
            );
            let mut query = sqlx::query_as::<_, ScanImageRow>(sqlx::AssertSqlSafe(images.as_str()));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for row in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                let Some(id) = key(&row.0) else { continue };
                out.entry(id).or_default().images.push(ItemImageInfo {
                    path: row.2,
                    image_type: crate::item_repository::image_type_from_disc(row.1),
                    date_modified: row.6.unwrap_or_else(|| {
                        chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0)
                            .unwrap_or_else(chrono::Utc::now)
                    }),
                    width: i32::try_from(row.3).unwrap_or(0),
                    height: i32::try_from(row.4).unwrap_or(0),
                    blur_hash: row
                        .5
                        .filter(|b| !b.is_empty())
                        .and_then(|b| String::from_utf8(b).ok()),
                });
            }
            let ancestors = format!(
                r#"SELECT "ItemId", "ParentItemId" FROM "AncestorIds"
                   WHERE "ItemId" IN ({placeholders})"#
            );
            let mut query =
                sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(ancestors.as_str()));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for (item, parent) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let (Some(id), Some(parent)) = (key(&item), key(&parent)) {
                    out.entry(id).or_default().ancestors.push(parent);
                }
            }
            let externals = format!(
                r#"SELECT "ItemId", "StreamType", "Path" FROM "MediaStreamInfos"
                   WHERE "ItemId" IN ({placeholders}) AND "IsExternal" = 1
                     AND "Path" IS NOT NULL"#
            );
            let mut query =
                sqlx::query_as::<_, (String, i32, String)>(sqlx::AssertSqlSafe(externals.as_str()));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for (item, stream_type, path) in
                query.fetch_all(self.db.pool()).await.map_err(db_err)?
            {
                let Some(id) = key(&item) else { continue };
                let links = out.entry(id).or_default();
                match crate::db_error::media_stream_type_from_disc(stream_type) {
                    MediaStreamType::Subtitle => links.external_subtitles.push(path),
                    MediaStreamType::Audio => links.external_audio.push(path),
                    _ => {}
                }
            }
        }
        for (id, fields) in self.locked_fields_for_items(item_ids).await? {
            out.entry(id).or_default().locked_fields = fields;
        }
        Ok(Some(out))
    }

    async fn items_at_paths(
        &self,
        paths: &[String],
    ) -> Result<Option<Vec<ItemPathRow>>, ServiceError> {
        let mut out = Vec::new();
        for chunk in paths.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = items_at_paths_sql(chunk.len());
            let mut query = sqlx::query_as::<_, PathRow>(sqlx::AssertSqlSafe(sql.as_str()));
            for path in chunk {
                query = query.bind(path);
            }
            out.extend(path_rows(
                query.fetch_all(self.db.pool()).await.map_err(db_err)?,
            ));
        }
        Ok(Some(out))
    }

    async fn library_top_parents(&self, library: Uuid) -> Result<Option<Vec<Uuid>>, ServiceError> {
        Ok(Some(
            crate::item_repository::library_top_parents_by_view(&self.db, &[library])
                .await?
                .remove(&library)
                .unwrap_or_else(|| vec![library]),
        ))
    }

    async fn library_items(
        &self,
        top_parent_ids: &[Uuid],
    ) -> Result<Option<Vec<ItemPathRow>>, ServiceError> {
        if top_parent_ids.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let mut out = Vec::new();
        for sql in [
            library_items_sql(top_parent_ids.len()),
            owned_extra_items_sql(top_parent_ids.len(), 0),
        ] {
            let mut query = sqlx::query_as::<_, PathRow>(sqlx::AssertSqlSafe(sql.as_str()));
            for library in top_parent_ids {
                query = query.bind(guid_to_db(*library));
            }
            out.extend(path_rows(
                query.fetch_all(self.db.pool()).await.map_err(db_err)?,
            ));
        }
        // Extras written by older scans may also match their own TopParentId.
        let mut seen = std::collections::HashSet::with_capacity(out.len());
        out.retain(|row| seen.insert(row.id));
        Ok(Some(out))
    }

    async fn items_in_scope(
        &self,
        top_parent_ids: &[Uuid],
        roots: &[String],
    ) -> Result<Option<Vec<ItemPathRow>>, ServiceError> {
        if top_parent_ids.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let libraries: Vec<String> = top_parent_ids.iter().copied().map(guid_to_db).collect();
        let mut out = Vec::new();
        // Three binds per root, after the libraries'.
        let per_chunk = (ferrofin_db::BATCH_BIND_CHUNK.saturating_sub(libraries.len()) / 3).max(1);
        for chunk in roots.chunks(per_chunk) {
            for sql in [
                items_under_roots_sql(libraries.len(), chunk.len()),
                pathless_children_sql(libraries.len(), chunk.len()),
                owned_extra_items_sql(libraries.len(), chunk.len()),
            ] {
                let mut query = sqlx::query_as::<_, PathRow>(sqlx::AssertSqlSafe(sql.as_str()));
                for library in &libraries {
                    query = query.bind(library);
                }
                for root in chunk {
                    let (exact, from, to) = path_prefix_range(root);
                    query = query.bind(exact).bind(from).bind(to);
                }
                out.extend(path_rows(
                    query.fetch_all(self.db.pool()).await.map_err(db_err)?,
                ));
            }
        }
        // Overlapping roots match a row more than once.
        let mut seen = std::collections::HashSet::with_capacity(out.len());
        out.retain(|row| seen.insert(row.id));
        Ok(Some(out))
    }

    async fn child_links(
        &self,
        parents: &[Uuid],
    ) -> Result<Option<Vec<ItemChildLink>>, ServiceError> {
        let mut out: Vec<ItemChildLink> = Vec::new();
        for chunk in parents.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = child_links_sql(chunk.len());
            let mut query = sqlx::query_as::<
                _,
                (String, Option<String>, Option<String>, Option<String>),
            >(sqlx::AssertSqlSafe(sql.as_str()));
            for parent in chunk {
                query = query.bind(guid_to_db(*parent));
            }
            let parse = |id: Option<String>| id.as_deref().and_then(|id| Uuid::parse_str(id).ok());
            for (id, parent, owner, path) in
                query.fetch_all(self.db.pool()).await.map_err(db_err)?
            {
                if let Ok(id) = Uuid::parse_str(&id) {
                    out.push(ItemChildLink {
                        id,
                        parent_id: parse(parent),
                        owner_id: parse(owner),
                        path,
                    });
                }
            }
        }
        let mut seen = std::collections::HashSet::with_capacity(out.len());
        out.retain(|row| seen.insert(row.id));
        Ok(Some(out))
    }

    async fn update_file_facts(
        &self,
        item: &BaseItemEntity,
        date_if_changed: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<bool, ServiceError> {
        /// Milliseconds per day, for `julianday` differences.
        const MS_PER_DAY: f64 = 86_400_000.0;
        let date_modified = opt_datetime_to_db(item.date_modified);
        // `BaseItemExtensions.HasChanged`'s tolerance, in `julianday` days.
        #[allow(clippy::cast_precision_loss)] // 1_000 is exact in an f64
        let tolerance =
            ferrofin_providers::refresh_plan::FILE_CHANGE_TOLERANCE_MS as f64 / MS_PER_DAY;
        // Every `SET` expression reads the row as it was, so the re-date
        // compares the new mtime with the `DateModified` it replaces. A stored
        // value `julianday` cannot read compares as unchanged.
        let written = sqlx::query(
            r#"UPDATE "BaseItems"
               SET "Path" = ?2, "ParentId" = ?3, "TopParentId" = ?4,
                   "DateCreated" = CASE
                       WHEN ?8 IS NOT NULL AND ?5 IS NOT NULL
                            AND ("DateModified" IS NULL
                                 OR abs(julianday(?5) - julianday("DateModified")) > ?9)
                       THEN ?8 ELSE "DateCreated" END,
                   "DateModified" = coalesce(?5, "DateModified"),
                   "Size" = coalesce(?6, "Size"),
                   "DateLastSaved" = ?7
               WHERE "Id" = ?1
                 AND ("Path" IS NOT ?2 OR "ParentId" IS NOT ?3 OR "TopParentId" IS NOT ?4
                      OR (?5 IS NOT NULL AND "DateModified" IS NOT ?5)
                      OR (?6 IS NOT NULL AND "Size" IS NOT ?6))"#,
        )
        .bind(&item.id)
        .bind(&item.path)
        .bind(&item.parent_id)
        .bind(&item.top_parent_id)
        .bind(date_modified)
        .bind(item.size)
        .bind(datetime_to_db(chrono::Utc::now()))
        .bind(opt_datetime_to_db(date_if_changed))
        .bind(tolerance)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(written.rows_affected() > 0)
    }

    async fn update_run_time_ticks(&self, item_id: Uuid, ticks: i64) -> Result<bool, ServiceError> {
        let written = sqlx::query(
            r#"UPDATE "BaseItems" SET "RunTimeTicks" = ?2, "DateLastSaved" = ?3
               WHERE "Id" = ?1 AND "RunTimeTicks" IS NOT ?2"#,
        )
        .bind(guid_to_db(item_id))
        .bind(ticks)
        .bind(datetime_to_db(chrono::Utc::now()))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(written.rows_affected() > 0)
    }

    async fn folder_run_time_sums(
        &self,
        folder_ids: &[Uuid],
        child_kinds: &[BaseItemKind],
    ) -> Result<HashMap<Uuid, FolderAggregate<i64>>, ServiceError> {
        let types: Vec<&str> = child_kinds
            .iter()
            .filter_map(|k| stored_type_name(*k))
            .collect();
        let mut out = HashMap::with_capacity(folder_ids.len());
        if types.is_empty() {
            return Ok(out);
        }
        for chunk in folder_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = folder_run_time_sums_sql(chunk.len(), types.len());
            let mut query =
                sqlx::query_as::<_, (String, Option<i64>, i64)>(sqlx::AssertSqlSafe(sql.as_str()));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for type_name in &types {
                query = query.bind(*type_name);
            }
            for (id, stored, sum) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Ok(id) = Uuid::parse_str(&id) {
                    out.insert(
                        id,
                        FolderAggregate {
                            stored,
                            aggregate: Some(sum),
                        },
                    );
                }
            }
        }
        Ok(out)
    }

    async fn folder_last_media_added(
        &self,
        folder_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, FolderAggregate<chrono::DateTime<chrono::Utc>>>, ServiceError> {
        let mut out = HashMap::with_capacity(folder_ids.len());
        for chunk in folder_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let sql = folder_last_media_added_sql(chunk.len());
            let mut query = sqlx::query_as::<
                _,
                (
                    String,
                    Option<chrono::DateTime<chrono::Utc>>,
                    Option<chrono::DateTime<chrono::Utc>>,
                ),
            >(sqlx::AssertSqlSafe(sql.as_str()));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for (id, stored, aggregate) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Ok(id) = Uuid::parse_str(&id) {
                    out.insert(id, FolderAggregate { stored, aggregate });
                }
            }
        }
        Ok(out)
    }

    async fn update_date_last_media_added(
        &self,
        item_id: Uuid,
        at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<bool, ServiceError> {
        let written = sqlx::query(
            r#"UPDATE "BaseItems" SET "DateLastMediaAdded" = ?2, "DateLastSaved" = ?3
               WHERE "Id" = ?1 AND "DateLastMediaAdded" IS NOT ?2"#,
        )
        .bind(guid_to_db(item_id))
        .bind(opt_datetime_to_db(at))
        .bind(datetime_to_db(chrono::Utc::now()))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(written.rows_affected() > 0)
    }

    async fn never_refreshed_ids(
        &self,
        kind: BaseItemKind,
        by_name_only: bool,
    ) -> Result<Vec<Uuid>, ServiceError> {
        let Some(type_name) = stored_type_name(kind) else {
            return Ok(Vec::new());
        };
        let ids: Vec<String> = sqlx::query_scalar(never_refreshed_ids_sql(by_name_only))
            .bind(type_name)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(ids
            .iter()
            .filter_map(|id| Uuid::parse_str(id).ok())
            .collect())
    }

    async fn superseded_by_name_artists(&self) -> Result<Vec<Uuid>, ServiceError> {
        let Some(type_name) = stored_type_name(BaseItemKind::MusicArtist) else {
            return Ok(Vec::new());
        };
        let ids: Vec<String> = sqlx::query_scalar(SUPERSEDED_BY_NAME_ARTISTS_SQL)
            .bind(type_name)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(ids
            .iter()
            .filter_map(|id| Uuid::parse_str(id).ok())
            .collect())
    }

    async fn stamp_date_last_refreshed(
        &self,
        item_ids: &[Uuid],
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, ServiceError> {
        let mut written = 0;
        let at = datetime_to_db(at);
        for chunk in item_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let placeholders = (2..=chunk.len() + 1)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"UPDATE "BaseItems" SET "DateLastRefreshed" = ?1, "DateLastSaved" = ?1
                   WHERE "Id" IN ({placeholders})"#
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str())).bind(&at);
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            written += query
                .execute(self.db.writer())
                .await
                .map_err(db_err)?
                .rows_affected();
        }
        Ok(written)
    }

    async fn locked_fields_for_items(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<ferrofin_model::entities::MetadataField>>, ServiceError> {
        let mut out: HashMap<Uuid, Vec<ferrofin_model::entities::MetadataField>> = HashMap::new();
        for chunk in item_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            // `IX_BaseItemMetadataFields_ItemId` serves the lookup; ordered by
            // field so the set reads back the same way every time.
            let sql = format!(
                r#"SELECT "ItemId", "Id" FROM "BaseItemMetadataFields"
                   WHERE "ItemId" IN ({placeholders}) ORDER BY "ItemId", "Id""#
            );
            let mut query = sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(sql.as_str()));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for (item, field) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                let Ok(id) = Uuid::parse_str(&item) else {
                    continue;
                };
                // An id this server does not know (a newer Jellyfin's field) is
                // skipped rather than failing the item's read.
                let Some(field) = i32::try_from(field)
                    .ok()
                    .and_then(|f| ferrofin_db::enums::metadata_field::from_i32(f).ok())
                else {
                    continue;
                };
                out.entry(id).or_default().push(field);
            }
        }
        Ok(out)
    }

    async fn replace_locked_fields(
        &self,
        item_id: Uuid,
        field_ids: &[i32],
    ) -> Result<(), ServiceError> {
        let id = guid_to_db(item_id);
        // Upstream rewrites an updated item's `BaseItemMetadataFields` rows
        // wholesale (`ItemPersistenceService.SaveItems`: delete, then add the
        // current set) — one transaction here, like `replace_provider_ids`.
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "BaseItemMetadataFields" WHERE "ItemId" = ?1"#)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for field in field_ids {
            // `OR IGNORE`: a set that names a field twice is one lock (the
            // composite key would otherwise reject the save).
            sqlx::query(
                r#"INSERT OR IGNORE INTO "BaseItemMetadataFields" ("Id", "ItemId") VALUES (?1, ?2)"#,
            )
            .bind(*field)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn add_locked_fields(
        &self,
        item_id: Uuid,
        field_ids: &[i32],
    ) -> Result<(), ServiceError> {
        let id = guid_to_db(item_id);
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        for field in field_ids {
            sqlx::query(
                r#"INSERT OR IGNORE INTO "BaseItemMetadataFields" ("Id", "ItemId") VALUES (?1, ?2)"#,
            )
            .bind(*field)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn set_item_image(
        &self,
        item_id: Uuid,
        image: &ItemImageInfo,
    ) -> Result<(), ServiceError> {
        let item = guid_to_db(item_id);
        let disc = image_type_to_disc(image.image_type);
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        // Replace any existing rows of this type (an uploaded image supersedes the
        // prior one of the same type).
        sqlx::query(r#"DELETE FROM "BaseItemImageInfos" WHERE "ItemId" = ?1 AND "ImageType" = ?2"#)
            .bind(&item)
            .bind(disc)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
               ("Id", "ItemId", "ImageType", "Path", "Width", "Height", "DateModified")
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
        )
        .bind(guid_to_db(Uuid::new_v4()))
        .bind(&item)
        .bind(disc)
        .bind(&image.path)
        .bind(i64::from(image.width))
        .bind(i64::from(image.height))
        .bind(datetime_to_db(image.date_modified))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn delete_item_image(
        &self,
        item_id: Uuid,
        image_type: ferrofin_model::entities::ImageType,
        _index: Option<i32>,
    ) -> Result<Vec<String>, ServiceError> {
        let item = guid_to_db(item_id);
        let disc = image_type_to_disc(image_type);
        // Collect the on-disk paths before deleting so the caller can remove files.
        let paths: Vec<String> = sqlx::query_scalar(
            r#"SELECT "Path" FROM "BaseItemImageInfos" WHERE "ItemId" = ?1 AND "ImageType" = ?2"#,
        )
        .bind(&item)
        .bind(disc)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "BaseItemImageInfos" WHERE "ItemId" = ?1 AND "ImageType" = ?2"#)
            .bind(&item)
            .bind(disc)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(paths)
    }

    async fn reattach_user_data(&self, item: &BaseItemEntity) -> Result<(), ServiceError> {
        // Reattach user-data rows detached onto the placeholder item back to this
        // item when their CustomDataKey matches the item's presentation key
        // (C# `RetentionDate` reattachment keys user data by presentation key).
        let Some(key) = item.presentation_unique_key.as_ref() else {
            return Ok(());
        };
        sqlx::query(
            r#"UPDATE "UserData" SET "ItemId" = ?1
               WHERE "ItemId" = ?2 AND "CustomDataKey" = ?3"#,
        )
        .bind(&item.id)
        .bind(PLACEHOLDER_ID)
        .bind(key)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn update_inherited_values(&self) -> Result<(), ServiceError> {
        // Recomputing inherited parental-rating / tag values across the item tree
        // requires the AncestorIds closure traversal owned by the library manager;
        // deferred to that unit. No-op here so callers can invoke it safely.
        Ok(())
    }
}

/// Which `LinkedChildren.ChildType` a version link gets: `LocalAlternateVersion`
/// (2) when the two files share a directory — what 12.0's scanner writes for
/// same-folder versions (`Video.RefreshMetadataForVersions`) — else
/// `LinkedAlternateVersion` (3), what a manual merge writes
/// (`VideosController.MergeVersions`).
pub(crate) async fn alternate_version_child_type(
    conn: &mut sqlx::SqliteConnection,
    item_id: Uuid,
    primary_id: Uuid,
) -> Result<i64, ServiceError> {
    let paths: Vec<(String, Option<String>)> =
        sqlx::query_as(r#"SELECT "Id", "Path" FROM "BaseItems" WHERE "Id" IN (?1, ?2)"#)
            .bind(guid_to_db(item_id))
            .bind(guid_to_db(primary_id))
            .fetch_all(&mut *conn)
            .await
            .map_err(db_err)?;
    let dir = |id: Uuid| {
        paths
            .iter()
            .find(|(i, _)| *i == guid_to_db(id))
            .and_then(|(_, p)| p.as_deref())
            .map(|p| {
                std::path::Path::new(p)
                    .parent()
                    .map(std::path::Path::to_path_buf)
            })
    };
    Ok(match (dir(item_id), dir(primary_id)) {
        (Some(Some(a)), Some(Some(b))) if a == b => 2,
        _ => 3,
    })
}

/// The full-column upsert statement for a `BaseItems` row. Column order matches
/// the bind order in [`FerrofinItemPersistenceService::upsert_item`].
///
/// `?73` is the save time: an insert stores the row's own `DateLastSaved`
/// (`NULL` for a new item — `CreateItems` never stamps it), and every update
/// stamps it (`LibraryManager.UpdateItemsAsync`: `item.DateLastSaved =
/// DateTime.UtcNow`).
const UPSERT_SQL: &str = r#"INSERT INTO "BaseItems" (
    "Id", "Album", "AlbumArtists", "Artists", "Audio", "ChannelId", "CleanName",
    "CommunityRating", "CriticRating", "CustomRating", "Data", "DateCreated",
    "DateLastMediaAdded", "DateLastRefreshed", "DateLastSaved", "DateModified",
    "EndDate", "EpisodeTitle", "ExternalId", "ExternalSeriesId", "ExternalServiceId",
    "ExtraType", "ForcedSortName", "Genres", "Height", "IndexNumber",
    "InheritedParentalRatingSubValue", "InheritedParentalRatingValue", "IsFolder",
    "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem",
    "LUFS", "MediaType", "Name", "NormalizationGain", "OfficialRating",
    "OriginalLanguage", "OriginalTitle", "Overview", "OwnerId", "ParentId",
    "ParentIndexNumber", "Path", "PreferredMetadataCountryCode",
    "PreferredMetadataLanguage", "PremiereDate", "PresentationUniqueKey",
    "PrimaryVersionId", "ProductionLocations", "ProductionYear", "RunTimeTicks",
    "SeasonId", "SeasonName", "SeriesId", "SeriesName", "SeriesPresentationUniqueKey",
    "ShowId", "Size", "SortName", "StartDate", "Studios", "Tagline", "Tags",
    "TopParentId", "TotalBitrate", "Type", "UnratedType", "Width"
) VALUES (
    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
) ON CONFLICT("Id") DO UPDATE SET
    "Album" = excluded."Album", "AlbumArtists" = excluded."AlbumArtists",
    "Artists" = excluded."Artists", "Audio" = excluded."Audio",
    "ChannelId" = excluded."ChannelId", "CleanName" = excluded."CleanName",
    "CommunityRating" = excluded."CommunityRating", "CriticRating" = excluded."CriticRating",
    "CustomRating" = excluded."CustomRating", "Data" = excluded."Data",
    "DateCreated" = excluded."DateCreated", "DateLastMediaAdded" = excluded."DateLastMediaAdded",
    "DateLastRefreshed" = excluded."DateLastRefreshed", "DateLastSaved" = ?73,
    "DateModified" = excluded."DateModified", "EndDate" = excluded."EndDate",
    "EpisodeTitle" = excluded."EpisodeTitle", "ExternalId" = excluded."ExternalId",
    "ExternalSeriesId" = excluded."ExternalSeriesId", "ExternalServiceId" = excluded."ExternalServiceId",
    "ExtraType" = excluded."ExtraType", "ForcedSortName" = excluded."ForcedSortName",
    "Genres" = excluded."Genres", "Height" = excluded."Height",
    "IndexNumber" = excluded."IndexNumber",
    "InheritedParentalRatingSubValue" = excluded."InheritedParentalRatingSubValue",
    "InheritedParentalRatingValue" = excluded."InheritedParentalRatingValue",
    "IsFolder" = excluded."IsFolder", "IsInMixedFolder" = excluded."IsInMixedFolder",
    "IsLocked" = excluded."IsLocked", "IsMovie" = excluded."IsMovie",
    "IsRepeat" = excluded."IsRepeat", "IsSeries" = excluded."IsSeries",
    "IsVirtualItem" = excluded."IsVirtualItem", "LUFS" = excluded."LUFS",
    "MediaType" = excluded."MediaType", "Name" = excluded."Name",
    "NormalizationGain" = excluded."NormalizationGain", "OfficialRating" = excluded."OfficialRating",
    "OriginalLanguage" = excluded."OriginalLanguage",
    "OriginalTitle" = excluded."OriginalTitle",
    "Overview" = excluded."Overview", "OwnerId" = excluded."OwnerId",
    "ParentId" = excluded."ParentId", "ParentIndexNumber" = excluded."ParentIndexNumber",
    "Path" = excluded."Path",
    "PreferredMetadataCountryCode" = excluded."PreferredMetadataCountryCode",
    "PreferredMetadataLanguage" = excluded."PreferredMetadataLanguage",
    "PremiereDate" = excluded."PremiereDate",
    "PresentationUniqueKey" = excluded."PresentationUniqueKey",
    "PrimaryVersionId" = excluded."PrimaryVersionId",
    "ProductionLocations" = excluded."ProductionLocations",
    "ProductionYear" = excluded."ProductionYear", "RunTimeTicks" = excluded."RunTimeTicks",
    "SeasonId" = excluded."SeasonId", "SeasonName" = excluded."SeasonName",
    "SeriesId" = excluded."SeriesId", "SeriesName" = excluded."SeriesName",
    "SeriesPresentationUniqueKey" = excluded."SeriesPresentationUniqueKey",
    "ShowId" = excluded."ShowId", "Size" = excluded."Size", "SortName" = excluded."SortName",
    "StartDate" = excluded."StartDate", "Studios" = excluded."Studios",
    "Tagline" = excluded."Tagline", "Tags" = excluded."Tags",
    "TopParentId" = excluded."TopParentId", "TotalBitrate" = excluded."TotalBitrate",
    "Type" = excluded."Type", "UnratedType" = excluded."UnratedType", "Width" = excluded."Width"
"#;

/// The user-editable metadata columns (everything the metadata editor's
/// `POST /Items/{id}` writes, plus the `Name`-derived `CleanName`/`SortName`)
/// and the `Data` blob that holds the rest of a row's provider metadata
/// (`RemoteTrailers`, a series' `Status`, …): the scan's upsert keeps the
/// stored value for each of these when the row is locked, so a locked item's
/// edits survive every rescan.
const LOCKED_PRESERVED_COLUMNS: &[&str] = &[
    "Data",
    "Name",
    "CleanName",
    "SortName",
    "ForcedSortName",
    "OriginalTitle",
    "CriticRating",
    "CommunityRating",
    "IndexNumber",
    "ParentIndexNumber",
    "Overview",
    "Genres",
    "Tagline",
    "Studios",
    "SeriesName",
    "EndDate",
    "PremiereDate",
    "ProductionYear",
    "OfficialRating",
    "CustomRating",
    "Tags",
    "ProductionLocations",
    "PreferredMetadataCountryCode",
    "PreferredMetadataLanguage",
    "Album",
    "Artists",
    "AlbumArtists",
];

/// The [`LOCKED_PRESERVED_COLUMNS`] a locked row's scan save may still FILL
/// when they are empty: what upstream's `BeforeMetadataRefresh` derives
/// from a locked item's path (its name, year, numbers and a by-date air
/// date), and the sort keys that follow the name. A value the row holds is
/// kept as for every other preserved column.
const LOCKED_FILLABLE_COLUMNS: &[&str] = &[
    "Name",
    "CleanName",
    "SortName",
    "ProductionYear",
    "IndexNumber",
    "ParentIndexNumber",
    "PremiereDate",
];

/// What a locked row's `Data` becomes on a scan save: the stored blob, with
/// the resolver's `VideoType` from the incoming one set in it
/// (`Video.UpdateFromResolvedItem`, `Video.cs:518-522`, which upstream runs
/// whatever the lock) and every other key kept with its value and place
/// (`json_set` re-renders the document minified). Left as stored when
/// the incoming blob names no `VideoType`, the stored one already holds it,
/// or either is not valid JSON (the stored one must be an object; `NULL` or
/// empty is `{}`). [`with_resolved_video_type`] is the same rule in Rust,
/// which the scan's merge and [`scan_save_changes_row`] apply.
///
/// The CASEs nest so the JSON functions only ever read valid JSON: they
/// raise an error on malformed input, which would fail the whole save.
///
/// [`with_resolved_video_type`]: crate::item_data::with_resolved_video_type
const LOCKED_DATA_SQL: &str = r#"CASE WHEN json_valid(excluded."Data") = 1
            AND json_valid(coalesce(nullif("Data", ''), '{}')) = 1
        THEN CASE WHEN json_type(excluded."Data", '$.VideoType') = 'text'
                AND json_type(coalesce(nullif("Data", ''), '{}')) = 'object'
                AND json_extract(coalesce(nullif("Data", ''), '{}'), '$.VideoType')
                    IS NOT json_extract(excluded."Data", '$.VideoType')
            THEN json_set(coalesce(nullif("Data", ''), '{}'), '$.VideoType',
                          json_extract(excluded."Data", '$.VideoType'))
            ELSE "Data" END
        ELSE "Data" END"#;

/// The columns a scan save may set but never clear — see [`scan_upsert_sql`].
const SCAN_NEVER_CLEARED_COLUMNS: &[&str] = &[
    "DateLastRefreshed",
    "DateLastMediaAdded",
    "DateModified",
    "Size",
];

/// The library scan's upsert: identical to [`UPSERT_SQL`] except for the
/// columns the scanner does not own on an existing row —
///
/// - `PrimaryVersionId` is left untouched (a scanned entity always carries
///   `None`, and overwriting erased every merge-versions link on each scan),
/// - `DateCreated` keeps its stored first-import value (`coalesce` still fills
///   it when the stored value is `NULL`),
/// - `IsLocked` can be set by the scan (an NFO `<lockdata>`) but never
///   cleared (`max`) — otherwise every scan would silently unlock edits,
/// - `DateLastRefreshed`, `DateLastMediaAdded`, `DateModified` and `Size` are
///   never cleared (`coalesce(excluded, stored)`): the scan saves them from
///   the stored row or its stat, and a row it could not read, or a path it
///   could not stat, must not lose them,
/// - every [`LOCKED_PRESERVED_COLUMNS`] entry keeps its stored value when the
///   row is locked (in the `CASE`, the unqualified `"IsLocked"` reads the
///   existing row, so the guard sees the pre-write lock state) — an empty
///   [`LOCKED_FILLABLE_COLUMNS`] entry excepted, which the save may fill, and
///   `Data`, whose `VideoType` alone follows the resolver
///   ([`LOCKED_DATA_SQL`]).
///
/// Derived from [`UPSERT_SQL`] by text substitution so the column/bind layout
/// cannot drift between the two statements; the substitutions are asserted in
/// `scan_upsert_preserves_unowned_columns`.
fn scan_upsert_sql() -> &'static str {
    static SQL: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| build_scan_upsert_sql(false));
    &SQL
}

/// [`scan_upsert_sql`] whose `DateCreated` is set but never cleared
/// (`coalesce(excluded, stored)`), like the other never-cleared columns: the
/// save of a file re-dated by `BeforeSaveInternal`
/// ([`ItemPersistenceService::save_scanned_items_with_date_created`]).
fn scan_upsert_date_created_sql() -> &'static str {
    static SQL: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| build_scan_upsert_sql(true));
    &SQL
}

/// See [`scan_upsert_sql`]; `writes_date_created` keeps a stored
/// `DateCreated` only over a `NULL` instead of always.
fn build_scan_upsert_sql(writes_date_created: bool) -> String {
    let date_created = if writes_date_created {
        r#""DateCreated" = coalesce(excluded."DateCreated", "DateCreated"),"#
    } else {
        r#""DateCreated" = coalesce("DateCreated", excluded."DateCreated"),"#
    };
    let mut sql = UPSERT_SQL
        .replace(r#""DateCreated" = excluded."DateCreated","#, date_created)
        .replace(r#""PrimaryVersionId" = excluded."PrimaryVersionId","#, "")
        .replace(
            r#""IsLocked" = excluded."IsLocked","#,
            r#""IsLocked" = max("IsLocked", excluded."IsLocked"),"#,
        );
    for col in SCAN_NEVER_CLEARED_COLUMNS {
        sql = sql.replace(
            &format!(r#""{col}" = excluded."{col}""#),
            &format!(r#""{col}" = coalesce(excluded."{col}", "{col}")"#),
        );
    }
    for col in LOCKED_PRESERVED_COLUMNS {
        let kept = if LOCKED_FILLABLE_COLUMNS.contains(col) {
            format!(r#""IsLocked" = 1 AND nullif("{col}", '') IS NOT NULL"#)
        } else {
            r#""IsLocked" = 1"#.to_owned()
        };
        // FindExtras owns the filename-derived display name. The scanner
        // preserves an explicit Name field lock before handing us the row.
        let kept = if ["Name", "CleanName", "SortName"].contains(col) {
            format!(r#"({kept}) AND excluded."OwnerId" IS NULL"#)
        } else {
            kept
        };
        let locked_value = if *col == "Data" {
            LOCKED_DATA_SQL.to_owned()
        } else {
            format!(r#""{col}""#)
        };
        sql = sql.replace(
            &format!(r#""{col}" = excluded."{col}""#),
            &format!(r#""{col}" = CASE WHEN {kept} THEN {locked_value} ELSE excluded."{col}" END"#),
        );
    }
    sql
}

/// Fills in `BaseItems."SortName"` for rows written before the write path
/// derived it — run once at startup, and cheap thereafter.
///
/// `upsert_item` now guarantees a non-null `SortName` on every save, but that
/// only covers rows written from here on. Rows already in the database keep
/// whatever they were created with, and a `Person`/`Genre`/`Studio` row is
/// inserted with `INSERT OR IGNORE` — a rescan will not rewrite it. Without a
/// repair pass those rows stay invisible to `nameStartsWith` forever.
///
/// The derivation cannot be expressed in SQLite: it strips articles as whole
/// words and left-pads every run of digits to width 10. So the rows are read,
/// computed in Rust, and written back in one transaction on the single writer.
///
/// Only NULL `SortName`s are touched — an adopted Jellyfin database, where the
/// column is already populated, is left byte-identical. The one exception is
/// the `PLACEHOLDER` row migration `0001` seeds (UserData detached from its
/// item): Jellyfin inserts it with a NULL `SortName` and never lists it, so
/// writing one would be a gratuitous divergence from an adopted database.
/// Returns the number of rows repaired.
///
/// # Errors
/// Returns [`ServiceError`] if the read or the write fails.
pub async fn backfill_missing_sort_names(db: &Database) -> Result<usize, ServiceError> {
    let rows: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
        r#"SELECT "Id", "Name", "ForcedSortName", "Type" FROM "BaseItems"
           WHERE "SortName" IS NULL AND "Name" IS NOT NULL AND "Name" <> ''
             AND "Type" <> 'PLACEHOLDER'"#,
    )
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    if rows.is_empty() {
        return Ok(0);
    }

    let mut tx = db.writer().begin().await.map_err(db_err)?;
    for (id, name, forced, type_name) in &rows {
        // `Type` is selected so the per-kind `GetSortName` branch applies to
        // both the derived and the forced key: a `Person` keeps its name
        // verbatim (`EnableAlphaNumericSorting => false`). Backfilling the
        // generic key here wrote the WRONG value into exactly the rows this
        // function exists to repair.
        let kind = crate::item_type_lookup::kind_from_type_name(type_name);
        let sort_name = match forced.as_deref().filter(|f| !f.is_empty()) {
            Some(f) => match kind {
                Some(kind) => crate::kinds::forced_sort_name_for(kind, f),
                None => ferrofin_util::sort_name::forced_sort_key(f),
            },
            None => match kind {
                Some(kind) => crate::kinds::sort_name_for(kind, name),
                None => ferrofin_util::sort_name::create_sort_name(name),
            },
        };
        sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = ?1 WHERE "Id" = ?2"#)
            .bind(sort_name)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
    }
    tx.commit().await.map_err(db_err)?;
    Ok(rows.len())
}

/// Fills in `BaseItems."PresentationUniqueKey"` for the by-name rows written
/// before the write path derived it — run once at startup, cheap thereafter.
///
/// Why this exists at all: the by-name inserts are
/// `INSERT ... WHERE NOT EXISTS` / `INSERT OR IGNORE`, so writing the column in
/// the insert (as `music_genre_row` and `insert_named_item` now do) is INERT on
/// every database that already holds the row. On this campaign's own lab that
/// was every scanner-created music genre: `MusicGenre|Ambient`, `Jazz` and
/// `Rock` still read back `PresentationUniqueKey NULL` after the fix shipped.
/// The column is what `GetItemValues` GROUPs on, and SQLite groups all NULLs
/// together, so a set of unkeyed by-name rows collapses to ONE representative
/// — the whole tab silently loses names.
///
/// Upstream needs no such pass because it recomputes the key on every
/// `SaveItems`: `MetadataService.BeforeSaveInternal`
/// (`MediaBrowser.Providers/Manager/MetadataService.cs:332-338`, identical on
/// v10.11.8 and master) assigns `item.CreatePresentationUniqueKey()` whenever
/// it differs. A Jellyfin by-name row is unkeyed only until its first metadata
/// refresh runs — `CreateItemByName` calls `CreateItem` and queues no refresh
/// itself — which is why a live 10.11.8 shows a MIXTURE of keyed and unkeyed
/// by-name rows. So the value written here is exactly the value Jellyfin's own
/// next refresh of that row would write, which is what makes the pass safe on
/// an adopted database: it moves a row forward to Jellyfin's settled state,
/// never to a value Jellyfin would not produce.
///
/// Scoped to the four by-name kinds whose key is a pure function of the name
/// (`Genre-…`, `MusicGenre-…`, `Studio-…`, `Artist-…` — see
/// [`crate::kinds::presentation_unique_key`]). `Person` is deliberately absent:
/// [`crate::people_repository::FerrofinPeopleRepository::repair_person_items`]
/// already repairs that kind, together with the `Path` column and the metadata
/// directory that only it knows how to build. Every other kind derives its key
/// from an id or a parent chain, where NULL is not the same kind of damage and
/// a blanket rewrite would be a guess.
///
/// Only NULL/empty keys are touched, so an already-keyed row — the whole of an
/// adopted, refreshed Jellyfin database — is left byte-identical. Returns the
/// number of rows repaired.
///
/// # Errors
/// Returns [`ServiceError`] if the read or the write fails.
pub async fn backfill_missing_presentation_keys(db: &Database) -> Result<usize, ServiceError> {
    let kinds = [
        BaseItemKind::Genre,
        BaseItemKind::MusicGenre,
        BaseItemKind::Studio,
        BaseItemKind::MusicArtist,
    ];
    let type_names: Vec<&'static str> =
        kinds.iter().copied().filter_map(stored_type_name).collect();
    if type_names.is_empty() {
        return Ok(0);
    }
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        r#"SELECT "Id", "Name", "Type" FROM "BaseItems"
           WHERE coalesce("PresentationUniqueKey", '') = ''
             AND "Name" IS NOT NULL AND "Name" <> '' AND "Type" IN ("#,
    );
    let mut sep = qb.separated(", ");
    for t in &type_names {
        sep.push_bind(*t);
    }
    qb.push(")");
    let rows: Vec<(String, String, String)> = qb
        .build_query_as()
        .fetch_all(db.pool())
        .await
        .map_err(db_err)?;

    let mut pending: Vec<(String, String)> = Vec::new();
    for (id, name, type_name) in &rows {
        let Some((kind, uuid)) =
            crate::item_type_lookup::kind_from_type_name(type_name).zip(Uuid::parse_str(id).ok())
        else {
            continue;
        };
        pending.push((
            id.clone(),
            crate::kinds::presentation_unique_key(kind, uuid, Some(name), None, None, None),
        ));
    }
    if pending.is_empty() {
        return Ok(0);
    }
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    for (id, key) in &pending {
        sqlx::query(
            r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = ?2
               WHERE "Id" = ?1 AND coalesce("PresentationUniqueKey", '') = ''"#,
        )
        .bind(id)
        .bind(key)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    tx.commit().await.map_err(db_err)?;
    Ok(pending.len())
}

/// The library-side inputs of a series' presentation key — what C#
/// `Series.CreatePresentationUniqueKey` reads off `LibraryManager` — resolved
/// by [`series_key_scope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesKeyScope {
    /// `LibraryOptions.EnableAutomaticSeriesGrouping` of the owning library
    /// (TRUE for a library with no saved options, upstream's default).
    pub enable_automatic_series_grouping: bool,
    /// `GetPreferredMetadataLanguage()`: the row's own value, else the
    /// library's, else the server's; `None` when all three are blank.
    pub preferred_metadata_language: Option<String>,
    /// `GetCollectionFolders(series)`: every library whose locations include
    /// the series' parent directory — in the caller's order; the key sorts
    /// them.
    pub collection_folder_ids: Vec<Uuid>,
}

/// Resolves a series' [`SeriesKeyScope`] from the configured libraries.
///
/// C# `GetCollectionFolders(item)` walks up to the top-level physical folder
/// (the series' parent directory) and returns every `CollectionFolder` whose
/// `PhysicalLocations` contain it, compared `OrdinalIgnoreCase` — which is
/// how one show folder shared by two libraries lands in both. A series with
/// no path (or one under no library) falls back to its `TopParentId` library
/// alone. The library options come from the `TopParentId` library, else the
/// first location match.
#[must_use]
pub fn series_key_scope(
    folders: &[ferrofin_model::entities_media::VirtualFolderInfo],
    default_metadata_language: &str,
    top_parent_id: Option<&str>,
    path: Option<&str>,
    own_language: Option<&str>,
) -> SeriesKeyScope {
    fn blank(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|v| !v.is_empty())
    }
    let folder_id = |f: &ferrofin_model::entities_media::VirtualFolderInfo| {
        f.item_id.as_deref().and_then(|id| Uuid::parse_str(id).ok())
    };
    let top_parent = top_parent_id.and_then(|id| Uuid::parse_str(id).ok());
    let parent_dir = path
        .and_then(|p| std::path::Path::new(p).parent())
        .and_then(std::path::Path::to_str)
        .map(|d| d.trim_end_matches('/'));
    let located: Vec<&ferrofin_model::entities_media::VirtualFolderInfo> = parent_dir
        .map(|dir| {
            folders
                .iter()
                .filter(|f| {
                    f.locations
                        .iter()
                        .any(|loc| loc.trim_end_matches('/').eq_ignore_ascii_case(dir))
                })
                .collect()
        })
        .unwrap_or_default();
    let owning = top_parent
        .and_then(|tp| folders.iter().find(|f| folder_id(f) == Some(tp)))
        .or_else(|| located.first().copied());
    let options = owning.and_then(|f| f.library_options.as_ref());
    let mut collection_folder_ids: Vec<Uuid> =
        located.iter().filter_map(|f| folder_id(f)).collect();
    if collection_folder_ids.is_empty() {
        collection_folder_ids.extend(top_parent);
    }
    let preferred_metadata_language = blank(own_language)
        .or_else(|| blank(options.and_then(|o| o.preferred_metadata_language.as_deref())))
        .or_else(|| blank(Some(default_metadata_language)))
        .map(str::to_owned);
    SeriesKeyScope {
        enable_automatic_series_grouping: options
            .is_none_or(|o| o.enable_automatic_series_grouping),
        preferred_metadata_language,
        collection_folder_ids,
    }
}

/// The `PresentationUniqueKey` to store for `item`.
///
/// Derived at write time for the same reason `CleanName` and `SortName` are:
/// upstream recomputes it on every refresh (`MetadataService.cs:335`), so no
/// caller can forget it. It is the column a query groups on, and Ferrofin left
/// it null on nearly every row — which is why merging two versions of a film
/// stopped hiding the alternate the moment the grouping was ported. On a first
/// merge, `merge_versions` only touches the alternates (upstream does the
/// same), so the primary's key has to have been right all along: null on the
/// primary and the primary's id on the alternate are two different groups.
///
/// A row whose stored type or id cannot be parsed keeps whatever it arrived
/// with rather than losing its key.
fn derive_presentation_key(item: &BaseItemEntity) -> Option<String> {
    let stored = item
        .presentation_unique_key
        .as_deref()
        .filter(|k| !k.is_empty());
    let Some((kind, id)) = crate::item_type_lookup::kind_from_type_name(&item.type_)
        .zip(Uuid::parse_str(&item.id).ok())
    else {
        return stored.map(str::to_owned);
    };
    // A `Series` keeps whatever is stored. Upstream's key depends on
    // `LibraryOptions.EnableAutomaticSeriesGrouping` — which defaults to TRUE
    // (`LibraryOptions.cs:34`) and then derives the key from the provider ids,
    // the metadata language and the library folders (`Series.cs:79`). Those
    // inputs live with the library scan (`kinds::series_presentation_unique_key`
    // over a `series_key_scope`), not with a bare row, so recomputing here
    // would flip every re-saved series on such a server to its own id and
    // orphan its seasons' `SeriesPresentationUniqueKey`. The scan and the
    // `series_presentation_keys_v12` boot repair are the two writers of it.
    if kind == BaseItemKind::Series && stored.is_some() {
        return stored.map(str::to_owned);
    }
    // A guide programme keeps whatever it arrived with — which is nothing.
    // `PresentationUniqueKey` is populated by exactly one thing upstream,
    // `MetadataService.UpdatePresentationUniqueKey` (v10.11.8
    // MediaBrowser.Providers/Manager/MetadataService.cs:332-336), and the guide
    // refresh calls `RefreshMetadata` on a CHANNEL only (v10.11.8
    // src/Jellyfin.LiveTv/Guide/GuideManager.cs:305) — an airing is written
    // straight through by `CreateItems`/`UpdateItemsAsync` and never sees the
    // metadata service. A real 10.11.8 database bears that out exactly: all four
    // channel rows carry their own id as the key, all 338 programme rows carry
    // NULL.
    //
    // The null is load-bearing, not cosmetic. `EnableGroupByPresentationUniqueKey`
    // is TRUE for a user query with an empty `IncludeItemTypes`
    // (Jellyfin.Server.Implementations/Item/BaseItemRepository.cs:1557-1589), and
    // SQLite groups NULLs together — so the whole guide collapses to ONE row on
    // an unfiltered recursive page. Minting a key per airing here would put the
    // entire guide on every user's home screen.
    if kind == BaseItemKind::LiveTvProgram {
        return stored.map(str::to_owned);
    }
    let derived = crate::kinds::presentation_unique_key(
        kind,
        id,
        item.name.as_deref(),
        item.primary_version_id.as_deref(),
        item.series_presentation_unique_key.as_deref(),
        item.index_number,
    );
    // Where the per-kind inputs were incomplete the rule falls back to the
    // row's own id, which is a *guess* — a season with no series key, a
    // by-name row with no name. Never overwrite a stored key with a guess:
    // upstream would have resolved the missing half rather than given up.
    let guessed = derived == id.as_simple().to_string()
        && !matches!(kind, BaseItemKind::Movie | BaseItemKind::Episode)
        && incomplete_inputs(kind, item);
    if guessed {
        return stored.map(str::to_owned).or(Some(derived));
    }
    Some(derived)
}

/// Whether the per-kind rule had to fall back for `item` because an input it
/// needed was absent — see [`derive_presentation_key`].
fn incomplete_inputs(kind: BaseItemKind, item: &BaseItemEntity) -> bool {
    let blank = |v: Option<&String>| v.is_none_or(String::is_empty);
    match kind {
        BaseItemKind::Season => {
            blank(item.series_presentation_unique_key.as_ref()) || item.index_number.is_none()
        }
        BaseItemKind::Genre
        | BaseItemKind::MusicGenre
        | BaseItemKind::Person
        | BaseItemKind::Studio
        | BaseItemKind::MusicArtist => blank(item.name.as_ref()),
        _ => false,
    }
}

/// `?1, …, ?n`.
fn numbered_placeholders(n: usize) -> String {
    (1..=n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// One `(Id, Type, Path, ParentId)` row of the path-scoped reads.
type PathRow = (String, String, Option<String>, Option<String>);

/// [`PathRow`]s as [`ItemPathRow`]s; a row whose id does not parse is no
/// item the scan can act on.
fn path_rows(rows: Vec<PathRow>) -> impl Iterator<Item = ItemPathRow> {
    rows.into_iter()
        .filter_map(|(id, item_type, path, parent)| {
            Some(ItemPathRow {
                id: Uuid::parse_str(&id).ok()?,
                item_type,
                path,
                parent_id: parent.as_deref().and_then(|p| Uuid::parse_str(p).ok()),
            })
        })
}

/// The binds that match a path at or under `root`, component-wise:
/// `(root, root + "/", root + "0")` — the root itself, then the half-open
/// range of every path that starts with `root/` (`'0'` is the byte after
/// `'/'`, and a `BINARY` comparison orders paths bytewise), so `/tv/Show`
/// matches `/tv/Show/e.mkv` but neither `/tv/Show (2019)` nor `/tv/Show2`.
/// A trailing slash is not part of the root.
pub(crate) fn path_prefix_range(root: &str) -> (String, String, String) {
    let root = root.trim_end_matches('/');
    (root.to_owned(), format!("{root}/"), format!("{root}0"))
}

/// [`ItemPersistenceService::items_at_paths`] over `n` paths: a batched
/// `FindByPath`, each path an equality seek on `IX_BaseItems_Path`;
/// `EXPLAIN QUERY PLAN` pinned by `the_path_scoped_reads_seek_by_path`.
pub(crate) fn items_at_paths_sql(n: usize) -> String {
    format!(
        r#"SELECT "Id", "Type", "Path", "ParentId" FROM "BaseItems" WHERE "Path" IN ({})"#,
        numbered_placeholders(n)
    )
}

/// The rows of a library — `TopParentId` one of the `libraries` top
/// parents bound as `?1..?libraries` ([`ItemPersistenceService::library_top_parents`])
/// — that its pruning weighs: every one but the placeholder, an owned
/// non-extra (a part, or a version stored under its owner) and an alternate
/// version whose primary is in the same library. Those are exactly the rows
/// the item repository's library read leaves out (`translate_query`'s
/// owner and `ALTERNATE_VERSION_HIDDEN` terms), so a row a library browse
/// never lists is never weighed as gone either — not even when a planned
/// item claims its path, which is the case for a version that shares its
/// primary's file. "The same library" is the library's top-parent set, not
/// the row's own `TopParentId`: on an adopted database a primary a scan
/// saved carries the collection folder while its version may still carry
/// Jellyfin's physical folder. `top_parent` is the `TopParentId` term's
/// column expression, which the caller pins (`+`) or not. The outer table
/// is aliased `bi`.
fn prune_candidate_terms(libraries: usize, top_parent: &str) -> String {
    let tops = numbered_placeholders(libraries);
    format!(
        r#"{top_parent} IN ({tops}) AND bi."Id" <> '{PLACEHOLDER_ID}'
            AND (+bi."OwnerId" IS NULL
                 OR +bi."OwnerId" = '00000000-0000-0000-0000-000000000000'
                 OR +bi."ExtraType" IS NOT NULL)
            AND (+bi."PrimaryVersionId" IS NULL OR NOT EXISTS (
                 SELECT 1 FROM "BaseItems" p
                 WHERE p."Id" = bi."PrimaryVersionId" AND p."TopParentId" IN ({tops})))"#
    )
}

/// [`ItemPersistenceService::library_items`] over the `libraries` top
/// parents bound as `?1..?libraries`: the library's rows by a `TopParentId`
/// index seek per top parent, the other terms a filter on the rows found (an
/// alternate's primary a primary-key probe), unordered — the pruning needs
/// no order, and sorting some 20,000 rows spilled to a temp file on every
/// scan. `EXPLAIN QUERY PLAN` pinned by `the_library_read_seeks_by_top_parent`.
pub(crate) fn library_items_sql(libraries: usize) -> String {
    let terms = prune_candidate_terms(libraries, r#"bi."TopParentId""#);
    format!(
        r#"SELECT bi."Id", bi."Type", bi."Path", bi."ParentId" FROM "BaseItems" AS bi WHERE {terms}"#
    )
}

/// [`ItemPersistenceService::items_in_scope`]'s rows at or under `n` roots
/// of the library whose `libraries` top parents
/// ([`ItemPersistenceService::library_top_parents`]) are bound as
/// `?1..?libraries`, three binds per root after them
/// ([`path_prefix_range`]): one `IX_BaseItems_Path` seek per root and range
/// (SQLite's multi-index `OR`), the library and the candidate terms
/// ([`prune_candidate_terms`], the same as the library scan's) a filter on
/// the rows found. The `+` keeps the planner off the `TopParentId` indexes,
/// which would walk every row of the library; `EXPLAIN QUERY PLAN` pinned by
/// `the_path_scoped_reads_seek_by_path`.
pub(crate) fn items_under_roots_sql(libraries: usize, n: usize) -> String {
    let roots = root_terms(libraries, n);
    let terms = prune_candidate_terms(libraries, r#"+bi."TopParentId""#);
    format!(
        r#"SELECT bi."Id", bi."Type", bi."Path", bi."ParentId" FROM "BaseItems" AS bi
            WHERE {terms} AND ({roots})"#
    )
}

/// Parentless extras belong to the library through their owner. A full scan
/// seeks owners by top parent then extras by OwnerId. A scoped scan instead
/// seeks extras by Path and checks each owner by primary key, so a watcher
/// event does not walk every owner in the library.
fn owned_extra_items_sql(libraries: usize, roots: usize) -> String {
    let tops = numbered_placeholders(libraries);
    if roots > 0 {
        let paths = root_terms(libraries, roots).replace("\"Path\"", "e.\"Path\"");
        return format!(
            r#"SELECT e."Id", e."Type", e."Path", e."ParentId"
                FROM "BaseItems" AS e CROSS JOIN "BaseItems" AS p
                WHERE ({paths}) AND +e."ExtraType" IS NOT NULL
                  AND p."Id" = e."OwnerId" AND +p."TopParentId" IN ({tops})"#
        );
    }
    format!(
        r#"SELECT e."Id", e."Type", e."Path", e."ParentId"
            FROM "BaseItems" AS p CROSS JOIN "BaseItems" AS e
            WHERE p."TopParentId" IN ({tops}) AND e."OwnerId" = p."Id"
              AND e."ExtraType" IS NOT NULL"#
    )
}

/// The `Path` terms of `n` roots bound after the `libraries` top parents,
/// three binds each ([`path_prefix_range`]): the root itself, or the range
/// under it.
fn root_terms(libraries: usize, n: usize) -> String {
    (0..n)
        .map(|i| {
            let at = libraries + 1 + 3 * i;
            format!(
                r#""Path" = ?{at} OR ("Path" >= ?{} AND "Path" < ?{})"#,
                at + 1,
                at + 2
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// [`ItemPersistenceService::items_in_scope`]'s path-less rows of the
/// library whose `libraries` top parents are bound as `?1..?libraries` whose
/// parent is at or under one of the `n` roots bound after them (three binds
/// each, as [`items_under_roots_sql`]), less what [`prune_candidate_terms`]
/// leaves out — the
/// virtual seasons (`Season` rows with no path) a scan of a series plans
/// and so may find gone, which the `ParentId` cascade would otherwise
/// delete unannounced with it. No other path-less row: upstream's
/// validation removes only a child that is `IsFileProtocol`
/// (`Folder.cs:569`), so an adopted Jellyfin missing episode (a virtual
/// `Episode` with no path) is never weighed. The parents are found by the
/// roots' `IX_BaseItems_Path` seeks and their children by
/// `IX_BaseItems_ParentId`, never the `NULL` end of `IX_BaseItems_Path` or a
/// `Type` index (every season in the database) — the `+`s pin it;
/// `EXPLAIN QUERY PLAN` pinned by `the_path_scoped_reads_seek_by_path`.
pub(crate) fn pathless_children_sql(libraries: usize, n: usize) -> String {
    let roots = root_terms(libraries, n);
    let terms = prune_candidate_terms(libraries, r#"+bi."TopParentId""#);
    let season = stored_type_name(BaseItemKind::Season).unwrap_or_default();
    format!(
        r#"SELECT bi."Id", bi."Type", bi."Path", bi."ParentId" FROM "BaseItems" AS bi
            WHERE {terms} AND +bi."Path" IS NULL AND +bi."Type" = '{season}'
              AND bi."ParentId" IN (SELECT "Id" FROM "BaseItems" WHERE {roots})"#
    )
}

/// [`ItemPersistenceService::child_links`] over `n` parent ids (binds
/// `?1..?n`, each used for both columns): one `IX_BaseItems_ParentId` and
/// one `IX_BaseItems_OwnerId` seek per id (SQLite's multi-index `OR`);
/// `EXPLAIN QUERY PLAN` pinned by `the_path_scoped_reads_seek_by_path`.
pub(crate) fn child_links_sql(n: usize) -> String {
    let ids = numbered_placeholders(n);
    format!(
        r#"SELECT "Id", "ParentId", "OwnerId", "Path" FROM "BaseItems"
            WHERE "ParentId" IN ({ids}) OR "OwnerId" IN ({ids})"#
    )
}

/// [`ItemPersistenceService::folder_run_time_sums`] over `n` folder ids
/// (binds `?1..?n`) and `kinds` child types (binds after them): each folder
/// by its primary key, and per folder a correlated sum over its non-folder
/// descendants of those types — `AncestorIds` sought by `ParentItemId`, each
/// child reached by its id. The `CROSS JOIN` pins that order: left free,
/// SQLite may drive the sum from `BaseItems."IsFolder"` (every non-folder
/// row in the database) and probe `AncestorIds` per row. `EXPLAIN QUERY
/// PLAN` pinned by `folder_aggregates_seek_from_the_ancestor_closure`.
pub(crate) fn folder_run_time_sums_sql(n: usize, kinds: usize) -> String {
    let types = (n + 1..=n + kinds)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"SELECT f."Id", f."RunTimeTicks",
                  (SELECT COALESCE(SUM(c."RunTimeTicks"), 0)
                     FROM "AncestorIds" AS a
                     CROSS JOIN "BaseItems" AS c ON c."Id" = a."ItemId"
                    WHERE a."ParentItemId" = f."Id" AND c."IsFolder" = 0
                      AND c."Type" IN ({types}))
             FROM "BaseItems" AS f
            WHERE f."Id" IN ({})"#,
        numbered_placeholders(n)
    )
}

/// [`ItemPersistenceService::folder_last_media_added`] over `n` folder ids,
/// shaped and pinned as [`folder_run_time_sums_sql`]: the latest
/// `DateCreated` of the non-folder, non-virtual descendants. The column is
/// `YYYY-MM-DD HH:MM:SS[.fffffff]` text (EF Core's form, and
/// [`datetime_to_db`]'s), whose textual order is chronological, so its
/// `MAX` is the latest.
pub(crate) fn folder_last_media_added_sql(n: usize) -> String {
    format!(
        r#"SELECT f."Id", f."DateLastMediaAdded",
                  (SELECT MAX(c."DateCreated")
                     FROM "AncestorIds" AS a
                     CROSS JOIN "BaseItems" AS c ON c."Id" = a."ItemId"
                    WHERE a."ParentItemId" = f."Id"
                      AND c."IsFolder" = 0 AND c."IsVirtualItem" = 0)
             FROM "BaseItems" AS f
            WHERE f."Id" IN ({})"#,
        numbered_placeholders(n)
    )
}

/// [`ItemPersistenceService::never_refreshed_ids`]: the rows of the type
/// bound as `?1` never refreshed, sought by the `Type`-leading
/// `IX_BaseItems_Type_TopParentId_Id` (with `by_name_only`, its
/// `TopParentId` too); `EXPLAIN QUERY PLAN` pinned by
/// `the_closing_pass_selections_seek_by_type`.
pub(crate) fn never_refreshed_ids_sql(by_name_only: bool) -> &'static str {
    if by_name_only {
        r#"SELECT "Id" FROM "BaseItems"
           WHERE "Type" = ?1 AND "DateLastRefreshed" IS NULL
             AND ("TopParentId" IS NULL OR "TopParentId" = '')
           ORDER BY "Id""#
    } else {
        r#"SELECT "Id" FROM "BaseItems"
           WHERE "Type" = ?1 AND "DateLastRefreshed" IS NULL
           ORDER BY "Id""#
    }
}

/// [`ItemPersistenceService::superseded_by_name_artists`] over the
/// `MusicArtist` type bound as `?1`: the rows in no library whose
/// `CleanName` a row in a library carries too. Both sides are sought by
/// `Type` — the twin by `IX_BaseItems_Type_CleanName` — never a scan of
/// `BaseItems`; `EXPLAIN QUERY PLAN` pinned by
/// `the_closing_pass_selections_seek_by_type`. The `+` on the twin's
/// `TopParentId` keeps SQLite (3.50+) off the `(Type, TopParentId, …)`
/// indexes, which it otherwise ranges on `TopParentId <> ''` instead of
/// seeking the `CleanName` equality.
pub(crate) const SUPERSEDED_BY_NAME_ARTISTS_SQL: &str = r#"SELECT b."Id" FROM "BaseItems" AS b
   WHERE b."Type" = ?1
     AND (b."TopParentId" IS NULL OR b."TopParentId" = '')
     AND b."CleanName" IS NOT NULL
     AND EXISTS (SELECT 1 FROM "BaseItems" AS f
                  WHERE f."Type" = ?1 AND f."CleanName" = b."CleanName"
                    AND +f."TopParentId" IS NOT NULL AND +f."TopParentId" <> '')
   ORDER BY b."Id""#;

/// The ids of the `item_type` rows never refreshed (`DateLastRefreshed`
/// unset), in id order — `PeopleValidator`'s new person items.
///
/// # Errors
///
/// [`ServiceError::Backend`] on a storage failure.
pub(crate) async fn never_refreshed_items(
    db: &Database,
    item_type: &str,
) -> Result<Vec<String>, ServiceError> {
    sqlx::query_scalar(never_refreshed_ids_sql(false))
        .bind(item_type)
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

/// The `item_type` rows not refreshed since `cutoff` (or never) that lack an
/// overview or a primary image, in id order, each as `(id, has overview, has
/// primary image)` — `PeopleValidator.RefreshPeopleImagesAsync`'s selection.
///
/// # Errors
///
/// [`ServiceError::Backend`] on a storage failure.
pub(crate) async fn items_lacking_overview_or_primary(
    db: &Database,
    item_type: &str,
    cutoff: &str,
) -> Result<Vec<(String, bool, bool)>, ServiceError> {
    sqlx::query_as(
        r#"SELECT "Id",
                  coalesce("Overview", '') <> '',
                  EXISTS (SELECT 1 FROM "BaseItemImageInfos" i
                          WHERE i."ItemId" = b."Id" AND i."ImageType" = 0)
           FROM "BaseItems" b
           WHERE "Type" = ?1
             AND ("DateLastRefreshed" IS NULL OR "DateLastRefreshed" < ?2)
             AND (("Overview" IS NULL OR "Overview" = '')
                  OR NOT EXISTS (SELECT 1 FROM "BaseItemImageInfos" i
                                 WHERE i."ItemId" = b."Id" AND i."ImageType" = 0))
           ORDER BY "Id""#,
    )
    .bind(item_type)
    .bind(cutoff)
    .fetch_all(db.pool())
    .await
    .map_err(db_err)
}

/// Test-only: sets a row's `ProductionYear` directly (a value a user
/// cleared, say), stamping nothing else.
#[cfg(test)]
pub(crate) async fn seed_production_year(db: &Database, id: Uuid, year: Option<i64>) {
    sqlx::query(r#"UPDATE "BaseItems" SET "ProductionYear" = ?2 WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .bind(year)
        .execute(db.writer())
        .await
        .expect("seed production year");
}

/// Test-only: stamps a row as refreshed at `refreshed` (stored text) with
/// `overview`, keeping the raw SQL inside the repository boundary.
#[cfg(test)]
pub(crate) async fn seed_refreshed_overview(
    db: &Database,
    id: Uuid,
    refreshed: &str,
    overview: &str,
) {
    sqlx::query(
        r#"UPDATE "BaseItems" SET "DateLastRefreshed" = ?2, "Overview" = ?3 WHERE "Id" = ?1"#,
    )
    .bind(guid_to_db(id))
    .bind(refreshed)
    .bind(overview)
    .execute(db.writer())
    .await
    .expect("seed refreshed overview");
}

/// Test-only: a primary image row at `path` for `item`.
#[cfg(test)]
pub(crate) async fn seed_primary_image(db: &Database, image_id: Uuid, item: Uuid, path: &str) {
    sqlx::query(
        r#"INSERT INTO "BaseItemImageInfos" ("Id", "ItemId", "Path", "ImageType", "DateModified", "Width", "Height")
           VALUES (?1, ?2, ?3, 0, '2026-09-01 00:00:00.0000000', 0, 0)"#,
    )
    .bind(guid_to_db(image_id))
    .bind(guid_to_db(item))
    .bind(path)
    .execute(db.writer())
    .await
    .expect("seed primary image");
}

/// Stamps a row's `PresentationUniqueKey` directly, for tests that need a
/// specific key rather than the one [`crate::kinds::presentation_unique_key`]
/// derives (the writer always recomputes it, exactly as C# `MetadataService`
/// does, so a fixture cannot express a shared key by saving one).
///
/// It lives here so the raw SQL stays inside the repository boundary.
#[cfg(test)]
pub(crate) async fn seed_presentation_key(db: &Database, id: Uuid, key: &str) {
    sqlx::query(r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = ?2 WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .bind(key)
        .execute(db.writer())
        .await
        .expect("seed presentation key");
}

/// Test-only: plant the `Data` blob an ADOPTED Jellyfin row would carry, so the
/// backfill's "only over an empty one" guard can be exercised.
#[cfg(test)]
pub(crate) async fn seed_container_data(db: &Database, id: Uuid, data: &str) {
    sqlx::query(r#"UPDATE "BaseItems" SET "Data" = ?2 WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .bind(data)
        .execute(db.writer())
        .await
        .expect("seed container data");
}

#[cfg(test)]
mod tests {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::persistence::{ItemPersistenceService, LinkedChildrenService};
    use uuid::Uuid;

    use crate::item_type_lookup::stored_type_name;
    use crate::linked_children_service::FerrofinLinkedChildrenService;
    use crate::test_support::{seed_item, test_db};
    use ferrofin_db::store::guid_to_db;

    use super::{
        FerrofinItemPersistenceService, container_row, ensure_container, seed_container_data,
    };

    /// `ensure_container` writes the `Data` blob it was given onto a row that
    /// has none, and NEVER over one that already has content.
    ///
    /// The second half is the drop-in guarantee: an adopted Jellyfin row's blob
    /// carries `PhysicalLocationsList`, `PhysicalFolderIds` and the rest, and
    /// replacing it with the one-key document Ferrofin provisions would lose the
    /// library's physical paths on swap-back.
    #[tokio::test]
    async fn a_container_data_blob_is_backfilled_but_never_overwritten() {
        let db = test_db().await;
        let mode = crate::item_type_lookup::IdDerivation::from_meta(
            Some("jellyfin-10.11.8"),
            Some("/data".to_owned()),
        );
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp
            .path()
            .join("collections")
            .to_string_lossy()
            .into_owned();
        let ours = r#"{"CollectionType":"boxsets"}"#;

        let id = ensure_container(
            &db,
            BaseItemKind::CollectionFolder,
            "Collections",
            &path,
            &mode,
            None,
            Some(ours),
        )
        .await
        .expect("provision")
        .expect("an id");
        assert_eq!(
            container_row(&db, id)
                .await
                .expect("row")
                .expect("present")
                .data
                .as_deref(),
            Some(ours),
            "an empty Data column is backfilled"
        );

        let adopted = r#"{"PhysicalLocationsList":["/x"],"CollectionType":"boxsets"}"#;
        seed_container_data(&db, id, adopted).await;
        let again = ensure_container(
            &db,
            BaseItemKind::CollectionFolder,
            "Collections",
            &path,
            &mode,
            None,
            Some(ours),
        )
        .await
        .expect("provision")
        .expect("an id");
        assert_eq!(again, id, "the same row, not a second one");
        assert_eq!(
            container_row(&db, id)
                .await
                .expect("row")
                .expect("present")
                .data
                .as_deref(),
            Some(adopted),
            "an adopted database's own Data survives"
        );
    }

    // A playlist (parent) and one of its members (child) both live in
    // `LinkedChildren`, whose BaseItems FK lacks `ON DELETE CASCADE`. Deleting
    // either must clear those links first instead of tripping constraint 787.
    #[tokio::test]
    async fn delete_clears_linked_children_both_directions() {
        let db = test_db().await;
        let playlist = Uuid::new_v4();
        let (member_a, member_b) = (Uuid::new_v4(), Uuid::new_v4());
        seed_item(&db, playlist, BaseItemKind::Playlist).await;
        seed_item(&db, member_a, BaseItemKind::Movie).await;
        seed_item(&db, member_b, BaseItemKind::Movie).await;

        let links = FerrofinLinkedChildrenService::new(db.clone());
        links
            .upsert_linked_child(playlist, member_a, 0)
            .await
            .expect("link a");
        links
            .upsert_linked_child(playlist, member_b, 0)
            .await
            .expect("link b");

        let svc = FerrofinItemPersistenceService::new(db.clone());

        // Delete a member (the ChildId FK direction): its link clears, the
        // playlist and other member survive — no FK 787.
        svc.delete_items(&[member_a])
            .await
            .expect("delete member_a");
        assert!(!svc.item_exists(member_a).await.expect("exists a"));
        assert!(svc.item_exists(playlist).await.expect("playlist survives"));

        // Delete the playlist (the ParentId FK direction) while member_b's link
        // still exists — must clear it instead of tripping FK 787.
        svc.delete_items(&[playlist])
            .await
            .expect("delete playlist");
        assert!(!svc.item_exists(playlist).await.expect("exists p"));
        assert!(svc.item_exists(member_b).await.expect("member_b survives"));

        let remaining: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "LinkedChildren""#)
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(remaining, 0, "all links should be cleared");
    }

    // A provider id round-trips through the repository's by-provider lookup,
    // and a re-save for the same (item, key) replaces the value instead of
    // stacking rows (the table's composite primary key).
    #[tokio::test]
    async fn save_provider_id_upserts_the_row() {
        use ferrofin_traits::persistence::ItemRepository;

        let db = test_db().await;
        let movie = Uuid::new_v4();
        seed_item(&db, movie, BaseItemKind::Movie).await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let repo = crate::item_repository::FerrofinItemRepository::new(
            db.clone(),
            std::sync::Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
        );

        svc.save_provider_id(movie, "Tmdb", "603")
            .await
            .expect("save");
        svc.save_provider_id(movie, "Tmdb", "604")
            .await
            .expect("replace");

        let rows = repo
            .get_items_with_provider_id("Tmdb")
            .await
            .expect("lookup");
        assert_eq!(rows, vec![(movie, "604".to_owned())]);
    }

    // "Identify → Apply" assigns the chosen result's whole id set: stale keys
    // go, the new ones land, blanks are dropped.
    #[tokio::test]
    async fn replace_provider_ids_swaps_the_whole_set() {
        let db = test_db().await;
        let movie = Uuid::new_v4();
        seed_item(&db, movie, BaseItemKind::Movie).await;
        let svc = FerrofinItemPersistenceService::new(db.clone());

        svc.save_provider_id(movie, "Tvdb", "1")
            .await
            .expect("seed stale id");
        svc.replace_provider_ids(
            movie,
            &[
                ("Tmdb".to_owned(), "603".to_owned()),
                ("Imdb".to_owned(), "tt0133093".to_owned()),
                ("Blank".to_owned(), "  ".to_owned()),
            ],
        )
        .await
        .expect("replace");

        let mut rows = svc
            .provider_ids_for_items(&[movie])
            .await
            .expect("read back")
            .remove(&movie)
            .unwrap_or_default();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                ("Imdb".to_owned(), "tt0133093".to_owned()),
                ("Tmdb".to_owned(), "603".to_owned()),
            ]
        );
    }

    // Saving an item must stamp the derived `CleanName` (C# `SaveItem` computes
    // `GetCleanValue(item.Name)` at write time). No scan path pre-computes it,
    // and the `searchTerm` filter queries `CleanName` — a NULL there makes the
    // item invisible to search (the web search page returned nothing).
    #[tokio::test]
    async fn save_items_stamps_derived_clean_name() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let svc = FerrofinItemPersistenceService::new(db.clone());

        let item = ferrofin_db::entities::base_items::BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
            name: Some("Amélie".to_owned()),
            ..ferrofin_db::entities::base_items::BaseItemEntity::default()
        };
        svc.save_items(std::slice::from_ref(&item))
            .await
            .expect("save");

        let clean: Option<String> =
            sqlx::query_scalar(r#"SELECT "CleanName" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("query");
        assert_eq!(clean.as_deref(), Some("amelie"));
    }

    /// Saves `entity` and returns the `SortName` the write path persisted.
    async fn persisted_sort_name(
        db: &ferrofin_db::Database,
        entity: ferrofin_db::entities::base_items::BaseItemEntity,
    ) -> Option<String> {
        let svc = FerrofinItemPersistenceService::new(db.clone());
        svc.save_items(std::slice::from_ref(&entity))
            .await
            .expect("save");
        sqlx::query_scalar(r#"SELECT "SortName" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(&entity.id)
            .fetch_one(db.pool())
            .await
            .expect("query")
    }

    fn named(name: &str) -> ferrofin_db::entities::base_items::BaseItemEntity {
        ferrofin_db::entities::base_items::BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(Uuid::new_v4()),
            type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
            name: Some(name.to_owned()),
            ..ferrofin_db::entities::base_items::BaseItemEntity::default()
        }
    }

    // C# `BaseItem.SortName` is a lazy property, so `SaveItems` can never write
    // a null. Deriving it here is what makes that true for every construction
    // site — including the ones (virtual folders, by-name items) that never set
    // the field and left the column NULL, which made `nameStartsWith` — it
    // filters `lower(SortName)` — match nothing.
    #[tokio::test]
    async fn save_items_derives_a_sort_name_when_the_caller_leaves_it_unset() {
        let db = test_db().await;
        assert_eq!(
            persisted_sort_name(&db, named("The Matrix"))
                .await
                .as_deref(),
            Some("matrix")
        );
    }

    // The repair pass for rows written before the derivation existed. Those
    // rows are unreachable by `nameStartsWith` (it filters `lower(SortName)`),
    // and an `INSERT OR IGNORE` by-name row is never rewritten by a rescan, so
    // without this they stay broken forever.
    #[tokio::test]
    async fn backfill_fills_null_sort_names_and_leaves_populated_ones_alone() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());

        // Two rows the write path would now derive for, forced to NULL to stand
        // in for what a pre-fix insert left behind, plus one already populated.
        let (null_plain, null_forced, populated) =
            (named("The Matrix"), named("Alien"), named("Up"));
        for e in [&null_plain, &null_forced, &populated] {
            svc.save_items(std::slice::from_ref(e)).await.expect("save");
        }
        sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = NULL WHERE "Id" IN (?1, ?2)"#)
            .bind(&null_plain.id)
            .bind(&null_forced.id)
            .execute(db.writer())
            .await
            .expect("null them out");
        sqlx::query(r#"UPDATE "BaseItems" SET "ForcedSortName" = 'Zzz 9' WHERE "Id" = ?1"#)
            .bind(&null_forced.id)
            .execute(db.writer())
            .await
            .expect("force");
        sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = 'hand-written' WHERE "Id" = ?1"#)
            .bind(&populated.id)
            .execute(db.writer())
            .await
            .expect("populate");

        assert_eq!(
            super::backfill_missing_sort_names(&db)
                .await
                .expect("backfill"),
            2,
            "only the NULL rows are repaired"
        );

        let read = |id: String| async {
            let v: Option<String> =
                sqlx::query_scalar(r#"SELECT "SortName" FROM "BaseItems" WHERE "Id" = ?1"#)
                    .bind(id)
                    .fetch_one(db.pool())
                    .await
                    .expect("query");
            v
        };
        assert_eq!(read(null_plain.id.clone()).await.as_deref(), Some("matrix"));
        assert_eq!(
            read(null_forced.id.clone()).await.as_deref(),
            Some("zzz 0000000009"),
            "a forced sort name is padded and lower-cased, not article-stripped"
        );
        assert_eq!(
            read(populated.id.clone()).await.as_deref(),
            Some("hand-written"),
            "an adopted Jellyfin database must come through byte-identical"
        );

        assert_eq!(
            super::backfill_missing_sort_names(&db)
                .await
                .expect("second run"),
            0,
            "the pass is a no-op once repaired"
        );
    }

    /// The by-name key backfill: an unkeyed scanner-created row gets the key
    /// upstream's next metadata refresh would give it, an already-keyed row is
    /// left byte-identical, and a kind whose key is not name-derived is not
    /// touched at all.
    ///
    /// This is the half the batch's first cut was missing: writing the key in
    /// `music_genre_row`'s `INSERT ... WHERE NOT EXISTS` is INERT on every
    /// database that already holds the row, and the campaign's own lab proved
    /// it — `MusicGenre|Ambient`/`Jazz`/`Rock` still read back NULL after the
    /// "fix" shipped.
    #[tokio::test]
    async fn backfill_missing_presentation_keys_repairs_only_unkeyed_by_name_rows() {
        async fn key(db: &ferrofin_db::Database, id: Uuid) -> Option<String> {
            sqlx::query_scalar(r#"SELECT "PresentationUniqueKey" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("query")
        }
        let db = test_db().await;
        let unkeyed = Uuid::from_u128(0xB201);
        let keyed = Uuid::from_u128(0xB202);
        let artist = Uuid::from_u128(0xB203);
        let movie = Uuid::from_u128(0xB204);
        crate::test_support::seed_named_item(&db, unkeyed, BaseItemKind::MusicGenre, "Ambient")
            .await;
        crate::test_support::seed_named_item(&db, keyed, BaseItemKind::Genre, "Action").await;
        crate::test_support::seed_named_item(&db, artist, BaseItemKind::MusicArtist, "Bjork").await;
        crate::test_support::seed_named_item(&db, movie, BaseItemKind::Movie, "Heat").await;
        // The unkeyed shapes are BOTH real: Ferrofin's inserts left NULL, and
        // Jellyfin's lazily-created rows can carry ''.
        sqlx::query(r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = NULL WHERE "Id" = ?1"#)
            .bind(ferrofin_db::store::guid_to_db(unkeyed))
            .execute(db.writer())
            .await
            .expect("null key");
        sqlx::query(r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = '' WHERE "Id" = ?1"#)
            .bind(ferrofin_db::store::guid_to_db(artist))
            .execute(db.writer())
            .await
            .expect("empty key");
        sqlx::query(
            r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = 'Genre-Action' WHERE "Id" = ?1"#,
        )
        .bind(ferrofin_db::store::guid_to_db(keyed))
        .execute(db.writer())
        .await
        .expect("already keyed");
        sqlx::query(r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = NULL WHERE "Id" = ?1"#)
            .bind(ferrofin_db::store::guid_to_db(movie))
            .execute(db.writer())
            .await
            .expect("null movie key");

        assert_eq!(
            super::backfill_missing_presentation_keys(&db)
                .await
                .expect("backfill"),
            2,
            "only the two unkeyed BY-NAME rows"
        );
        assert_eq!(
            key(&db, unkeyed).await.as_deref(),
            Some("MusicGenre-Ambient")
        );
        // `MusicArtist` is spelled `Artist-` in the key (MusicArtist.cs:152).
        assert_eq!(key(&db, artist).await.as_deref(), Some("Artist-Bjork"));
        assert_eq!(
            key(&db, keyed).await.as_deref(),
            Some("Genre-Action"),
            "an adopted, refreshed Jellyfin row comes through byte-identical"
        );
        assert_eq!(
            key(&db, movie).await,
            None,
            "a Movie's key is derived from its id/primary version, not its name — not this pass's"
        );
        assert_eq!(
            super::backfill_missing_presentation_keys(&db)
                .await
                .expect("second run"),
            0,
            "the pass is a no-op once repaired"
        );
    }

    /// The backfill applies the per-kind `CreateSortName` branch: `Person`
    /// overrides `EnableAlphaNumericSorting => false` on both trees, so its key
    /// is the name verbatim. Writing the generic lower-cased key here would
    /// corrupt exactly the rows this pass exists to repair.
    #[tokio::test]
    async fn backfill_missing_sort_names_uses_the_person_rule_for_a_person() {
        async fn read(db: &ferrofin_db::Database, id: Uuid) -> Option<String> {
            sqlx::query_scalar(r#"SELECT "SortName" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("query")
        }
        let db = test_db().await;
        let person = Uuid::from_u128(0xB101);
        let movie = Uuid::from_u128(0xB102);
        crate::test_support::seed_named_item(&db, person, BaseItemKind::Person, "The Rock").await;
        crate::test_support::seed_named_item(&db, movie, BaseItemKind::Movie, "The Rock").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = NULL"#)
            .execute(db.writer())
            .await
            .expect("null them out");

        assert_eq!(
            super::backfill_missing_sort_names(&db)
                .await
                .expect("backfill"),
            2
        );
        assert_eq!(read(&db, person).await.as_deref(), Some("The Rock"));
        assert_eq!(
            read(&db, movie).await.as_deref(),
            Some("rock"),
            "every other kind still gets the alphanumeric key"
        );
    }

    // A caller-supplied sort name wins: that is what carries the per-kind
    // `CreateSortName` overrides (episode/season) the scanner computes, and
    // those drive the client's play queue.
    #[tokio::test]
    async fn save_items_keeps_a_caller_supplied_sort_name() {
        let db = test_db().await;
        let entity = ferrofin_db::entities::base_items::BaseItemEntity {
            sort_name: Some("0003".to_owned()),
            ..named("The Matrix")
        };
        assert_eq!(
            persisted_sort_name(&db, entity).await.as_deref(),
            Some("0003")
        );
    }

    /// `OriginalLanguage` (12.0) is written by both upsert statements and read
    /// back by the entity.
    #[tokio::test]
    async fn original_language_round_trips_through_the_row() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let entity = ferrofin_db::entities::base_items::BaseItemEntity {
            original_language: Some("ja".to_owned()),
            ..named("Seven Samurai")
        };
        let id = entity.id.clone();
        svc.save_items(std::slice::from_ref(&entity))
            .await
            .expect("save");
        let read = |db: ferrofin_db::Database, id: String| async move {
            sqlx::query_as::<_, ferrofin_db::entities::base_items::BaseItemEntity>(
                r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#,
            )
            .bind(id)
            .fetch_one(db.pool())
            .await
            .expect("row")
        };
        assert_eq!(
            read(db.clone(), id.clone())
                .await
                .original_language
                .as_deref(),
            Some("ja")
        );
        // The scan's statement carries the column too (and a later save can
        // change it: the column is not lock-preserved).
        let rescanned = ferrofin_db::entities::base_items::BaseItemEntity {
            original_language: Some("en".to_owned()),
            ..entity
        };
        svc.save_scanned_items(std::slice::from_ref(&rescanned))
            .await
            .expect("scan save");
        assert_eq!(read(db, id).await.original_language.as_deref(), Some("en"));
    }

    // `ForcedSortName` short-circuits `CreateSortName` in C#: it is padded and
    // lower-cased, but its articles and punctuation are left alone.

    // A `ForcedSortName` wins over the name, and since 12.0 it goes through
    // the same `GetSortName` cleaning as an auto-generated key (article and
    // remove-characters stripped, digits padded, lower-cased).
    #[tokio::test]
    async fn save_items_derives_from_a_forced_sort_name_when_present() {
        let db = test_db().await;
        let entity = ferrofin_db::entities::base_items::BaseItemEntity {
            forced_sort_name: Some("The Matrix 2".to_owned()),
            ..named("Unrelated")
        };
        assert_eq!(
            persisted_sort_name(&db, entity).await.as_deref(),
            Some("matrix 0000000002")
        );
    }

    // …except for a `Person`, whose `EnableAlphaNumericSorting => false`
    // sends the forced name down the verbatim `TrimStart()` branch too.
    #[tokio::test]
    async fn save_items_keeps_a_person_forced_sort_name_verbatim() {
        let db = test_db().await;
        let entity = ferrofin_db::entities::base_items::BaseItemEntity {
            type_: stored_type_name(BaseItemKind::Person)
                .expect("person type")
                .to_owned(),
            forced_sort_name: Some("  Parity, Alice".to_owned()),
            ..named("Alice Parity")
        };
        assert_eq!(
            persisted_sort_name(&db, entity).await.as_deref(),
            Some("Parity, Alice")
        );
    }

    // Saving a movie's genre/studio values must also materialize the browsable
    // by-name BaseItems row (sharing the ItemValueId as its id) so the
    // Genres/Studios tabs list it and a `GenreIds=<id>` filter resolves.
    #[tokio::test]
    async fn save_item_values_materializes_by_name_items() {
        let db = test_db().await;
        let movie = Uuid::new_v4();
        seed_item(&db, movie, BaseItemKind::Movie).await;
        let svc = FerrofinItemPersistenceService::new(db.clone());

        // 2 = Genre, 3 = Studios, 4 = Tags (tags get no browse item).
        svc.save_item_values(
            movie,
            &[
                (2, "Horror".to_owned()),
                (3, "A24".to_owned()),
                (4, "4k".to_owned()),
            ],
        )
        .await
        .expect("save values");

        // The Genre by-name row exists, and its id equals the shared ItemValueId.
        let genre: Option<(String, String)> = sqlx::query_as(
            r#"SELECT bi."Id", iv."ItemValueId"
               FROM "BaseItems" bi
               JOIN "ItemValues" iv ON iv."Value" = bi."Name" AND iv."Type" = 2
               WHERE bi."Type" LIKE '%.Genre' AND bi."Name" = 'Horror'"#,
        )
        .fetch_optional(db.pool())
        .await
        .expect("query genre");
        let (genre_item_id, genre_value_id) = genre.expect("genre by-name row exists");
        assert_eq!(genre_item_id, genre_value_id, "id is the shared value id");

        // Studio row exists too; the tag does NOT get a by-name row.
        let studios: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Type" LIKE '%.Studio'"#)
                .fetch_one(db.pool())
                .await
                .expect("studio count");
        assert_eq!(studios, 1);
        let tags: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Name" = '4k' AND "Type" NOT LIKE '%.Movie'"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("tag count");
        assert_eq!(tags, 0, "tags are not browsable by-name items");
    }

    // AlbumArtist (1) materializes a browsable MusicArtist row sharing the
    // ItemValueId; Artist (0) stays a filter-only value (no MusicArtist row), so
    // a name that is both does not produce a duplicate artist item.
    #[tokio::test]
    async fn save_item_values_materializes_music_artist_items() {
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;
        let svc = FerrofinItemPersistenceService::new(db.clone());

        svc.save_item_values(
            track,
            &[
                (0, "John Coltrane".to_owned()), // Artist (track performer) only
                (0, "Miles Davis".to_owned()),   // Artist too
                (1, "Miles Davis".to_owned()),   // AlbumArtist (same name)
                (1, "Various Artists".to_owned()),
            ],
        )
        .await
        .expect("save values");

        // Exactly one MusicArtist row per distinct album-artist name, each id
        // equal to its AlbumArtist ItemValueId. No row for the Artist-only name.
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT bi."Name", bi."Id"
               FROM "BaseItems" bi
               WHERE bi."Type" LIKE '%.MusicArtist'
               ORDER BY bi."Name""#,
        )
        .fetch_all(db.pool())
        .await
        .expect("query artists");
        let names: Vec<&str> = rows.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["Miles Davis", "Various Artists"]);
        for (name, id) in &rows {
            let value_id: String = sqlx::query_scalar(
                r#"SELECT "ItemValueId" FROM "ItemValues" WHERE "Type" = 1 AND "Value" = ?1"#,
            )
            .bind(name)
            .fetch_one(db.pool())
            .await
            .expect("value id");
            assert_eq!(id, &value_id, "artist item id is the AlbumArtist value id");
        }
        // The Artist-only performer got an ItemValue but no MusicArtist row.
        let coltrane: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Name" = 'John Coltrane'"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("count");
        assert_eq!(coltrane, 0);
    }

    /// The by-name materializer must be a NO-OP once the scanner has resolved a
    /// folder-backed `MusicArtist` of the same `CleanName`
    /// (`MusicArtistResolver`). `item_repository::push_by_name_join` joins
    /// `agg.cval = bi."CleanName"`, so a second row of the same name makes
    /// /Artists list that artist twice — the failure a previous attempt at this
    /// port was rolled back for.
    #[tokio::test]
    async fn save_item_values_does_not_duplicate_a_resolved_music_artist() {
        let db = test_db().await;
        let track = Uuid::new_v4();
        seed_item(&db, track, BaseItemKind::Audio).await;

        // The row the scanner writes: path-derived id, real media Path, parented
        // into the music library (so it has a TopParentId).
        let resolved = Uuid::new_v4();
        sqlx::query(
            r#"INSERT INTO "BaseItems"
               ("Id","Type","Name","CleanName","Path","IsFolder","IsInMixedFolder",
                "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
               VALUES (?1,?2,'Miles Davis','miles davis','/media/music/Miles Davis',
                       1,0,0,0,0,0,0)"#,
        )
        .bind(guid_to_db(resolved))
        .bind(stored_type_name(BaseItemKind::MusicArtist).expect("type name"))
        .execute(db.pool())
        .await
        .expect("seed resolved artist");

        let svc = FerrofinItemPersistenceService::new(db.clone());
        svc.save_item_values(track, &[(1, "Miles Davis".to_owned())])
            .await
            .expect("save values");

        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            r#"SELECT bi."Id", bi."Path" FROM "BaseItems" bi
               WHERE bi."Type" LIKE '%.MusicArtist' AND bi."CleanName" = 'miles davis'"#,
        )
        .fetch_all(db.pool())
        .await
        .expect("query artists");
        assert_eq!(rows.len(), 1, "one row per artist: {rows:?}");
        assert_eq!(rows[0].0, guid_to_db(resolved), "the scanned row survives");
        assert_eq!(rows[0].1.as_deref(), Some("/media/music/Miles Davis"));

        // The ItemValues link is still written — the artist is still browsable
        // through the by-name aggregate, it just resolves to the scanned row.
        let linked: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "ItemValuesMap" m
               JOIN "ItemValues" v ON v."ItemValueId" = m."ItemValueId"
               WHERE m."ItemId" = ?1 AND v."Type" = 1 AND v."Value" = 'Miles Davis'"#,
        )
        .bind(guid_to_db(track))
        .fetch_one(db.pool())
        .await
        .expect("count links");
        assert_eq!(linked, 1);
    }

    // The library scan rebuilds entities from disk with no merge link and a
    // scan-time DateCreated. Its save must not clobber either on an existing
    // row (a plain save_items erased every merge-versions link on each scan),
    // while the full save — the merge/split write path — must still set AND
    // clear both.
    /// `column` of `item` set to NULL (`null`) or to a different value.
    /// Booleans and `Type` have no NULL; that mode leaves them equal.
    #[allow(clippy::too_many_lines)]
    fn vary(
        item: &mut ferrofin_db::entities::base_items::BaseItemEntity,
        column: &str,
        null: bool,
        other: &str,
    ) {
        match column {
            "Album" => {
                item.album = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.album.clone().unwrap_or_default()
                    ))
                };
            }
            "AlbumArtists" => {
                item.album_artists = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.album_artists.clone().unwrap_or_default()
                    ))
                };
            }
            "Artists" => {
                item.artists = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.artists.clone().unwrap_or_default()
                    ))
                };
            }
            "Audio" => {
                item.audio = if null {
                    None
                } else {
                    Some(item.audio.unwrap_or(0) + 1)
                };
            }
            "ChannelId" => {
                item.channel_id = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.channel_id.clone().unwrap_or_default()
                    ))
                };
            }
            "CleanName" => {
                item.clean_name = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.clean_name.clone().unwrap_or_default()
                    ))
                };
            }
            "CommunityRating" => {
                item.community_rating = if null {
                    None
                } else {
                    Some(item.community_rating.unwrap_or(0.0) + 0.5)
                };
            }
            "CriticRating" => {
                item.critic_rating = if null {
                    None
                } else {
                    Some(item.critic_rating.unwrap_or(0.0) + 0.5)
                };
            }
            "CustomRating" => {
                item.custom_rating = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.custom_rating.clone().unwrap_or_default()
                    ))
                };
            }
            "Data" => {
                item.data = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.data.clone().unwrap_or_default()
                    ))
                };
            }
            "DateCreated" => {
                item.date_created = if null {
                    None
                } else {
                    item.date_created.map(|d| d + chrono::TimeDelta::days(1))
                };
            }
            "DateLastMediaAdded" => {
                item.date_last_media_added = if null {
                    None
                } else {
                    item.date_last_media_added
                        .map(|d| d + chrono::TimeDelta::days(1))
                };
            }
            "DateLastRefreshed" => {
                item.date_last_refreshed = if null {
                    None
                } else {
                    item.date_last_refreshed
                        .map(|d| d + chrono::TimeDelta::days(1))
                };
            }
            "DateLastSaved" => {
                item.date_last_saved = if null {
                    None
                } else {
                    item.date_last_saved.map(|d| d + chrono::TimeDelta::days(1))
                };
            }
            "DateModified" => {
                item.date_modified = if null {
                    None
                } else {
                    item.date_modified.map(|d| d + chrono::TimeDelta::days(1))
                };
            }
            "EndDate" => {
                item.end_date = if null {
                    None
                } else {
                    item.end_date.map(|d| d + chrono::TimeDelta::days(1))
                };
            }
            "EpisodeTitle" => {
                item.episode_title = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.episode_title.clone().unwrap_or_default()
                    ))
                };
            }
            "ExternalId" => {
                item.external_id = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.external_id.clone().unwrap_or_default()
                    ))
                };
            }
            "ExternalSeriesId" => {
                item.external_series_id = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.external_series_id.clone().unwrap_or_default()
                    ))
                };
            }
            "ExternalServiceId" => {
                item.external_service_id = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.external_service_id.clone().unwrap_or_default()
                    ))
                };
            }
            "ExtraType" => {
                item.extra_type = if null {
                    None
                } else {
                    Some(item.extra_type.unwrap_or(0) + 1)
                };
            }
            "ForcedSortName" => {
                item.forced_sort_name = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.forced_sort_name.clone().unwrap_or_default()
                    ))
                };
            }
            "Genres" => {
                item.genres = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.genres.clone().unwrap_or_default()
                    ))
                };
            }
            "Height" => {
                item.height = if null {
                    None
                } else {
                    Some(item.height.unwrap_or(0) + 1)
                };
            }
            "IndexNumber" => {
                item.index_number = if null {
                    None
                } else {
                    Some(item.index_number.unwrap_or(0) + 1)
                };
            }
            "InheritedParentalRatingSubValue" => {
                item.inherited_parental_rating_sub_value = if null {
                    None
                } else {
                    Some(item.inherited_parental_rating_sub_value.unwrap_or(0) + 1)
                };
            }
            "InheritedParentalRatingValue" => {
                item.inherited_parental_rating_value = if null {
                    None
                } else {
                    Some(item.inherited_parental_rating_value.unwrap_or(0) + 1)
                };
            }
            "IsFolder" => {
                if !null {
                    item.is_folder = !item.is_folder;
                }
            }
            "IsInMixedFolder" => {
                if !null {
                    item.is_in_mixed_folder = !item.is_in_mixed_folder;
                }
            }
            "IsLocked" => {
                if !null {
                    item.is_locked = !item.is_locked;
                }
            }
            "IsMovie" => {
                if !null {
                    item.is_movie = !item.is_movie;
                }
            }
            "IsRepeat" => {
                if !null {
                    item.is_repeat = !item.is_repeat;
                }
            }
            "IsSeries" => {
                if !null {
                    item.is_series = !item.is_series;
                }
            }
            "IsVirtualItem" => {
                if !null {
                    item.is_virtual_item = !item.is_virtual_item;
                }
            }
            "LUFS" => {
                item.lufs = if null {
                    None
                } else {
                    Some(item.lufs.unwrap_or(0.0) + 0.5)
                };
            }
            "MediaType" => {
                item.media_type = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.media_type.clone().unwrap_or_default()
                    ))
                };
            }
            "Name" => {
                item.name = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.name.clone().unwrap_or_default()
                    ))
                };
            }
            "NormalizationGain" => {
                item.normalization_gain = if null {
                    None
                } else {
                    Some(item.normalization_gain.unwrap_or(0.0) + 0.5)
                };
            }
            "OfficialRating" => {
                item.official_rating = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.official_rating.clone().unwrap_or_default()
                    ))
                };
            }
            "OriginalLanguage" => {
                item.original_language = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.original_language.clone().unwrap_or_default()
                    ))
                };
            }
            "OriginalTitle" => {
                item.original_title = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.original_title.clone().unwrap_or_default()
                    ))
                };
            }
            "Overview" => {
                item.overview = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.overview.clone().unwrap_or_default()
                    ))
                };
            }
            "OwnerId" => {
                item.owner_id = if null { None } else { Some(other.to_owned()) };
            }
            "ParentId" => {
                item.parent_id = if null { None } else { Some(other.to_owned()) };
            }
            "ParentIndexNumber" => {
                item.parent_index_number = if null {
                    None
                } else {
                    Some(item.parent_index_number.unwrap_or(0) + 1)
                };
            }
            "Path" => {
                item.path = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.path.clone().unwrap_or_default()
                    ))
                };
            }
            "PreferredMetadataCountryCode" => {
                item.preferred_metadata_country_code = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.preferred_metadata_country_code
                            .clone()
                            .unwrap_or_default()
                    ))
                };
            }
            "PreferredMetadataLanguage" => {
                item.preferred_metadata_language = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.preferred_metadata_language.clone().unwrap_or_default()
                    ))
                };
            }
            "PremiereDate" => {
                item.premiere_date = if null {
                    None
                } else {
                    item.premiere_date.map(|d| d + chrono::TimeDelta::days(1))
                };
            }
            "PresentationUniqueKey" => {
                item.presentation_unique_key = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.presentation_unique_key.clone().unwrap_or_default()
                    ))
                };
            }
            "PrimaryVersionId" => {
                item.primary_version_id = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.primary_version_id.clone().unwrap_or_default()
                    ))
                };
            }
            "ProductionLocations" => {
                item.production_locations = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.production_locations.clone().unwrap_or_default()
                    ))
                };
            }
            "ProductionYear" => {
                item.production_year = if null {
                    None
                } else {
                    Some(item.production_year.unwrap_or(0) + 1)
                };
            }
            "RunTimeTicks" => {
                item.run_time_ticks = if null {
                    None
                } else {
                    Some(item.run_time_ticks.unwrap_or(0) + 1)
                };
            }
            "SeasonId" => {
                item.season_id = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.season_id.clone().unwrap_or_default()
                    ))
                };
            }
            "SeasonName" => {
                item.season_name = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.season_name.clone().unwrap_or_default()
                    ))
                };
            }
            "SeriesId" => {
                item.series_id = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.series_id.clone().unwrap_or_default()
                    ))
                };
            }
            "SeriesName" => {
                item.series_name = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.series_name.clone().unwrap_or_default()
                    ))
                };
            }
            "SeriesPresentationUniqueKey" => {
                item.series_presentation_unique_key = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.series_presentation_unique_key
                            .clone()
                            .unwrap_or_default()
                    ))
                };
            }
            "ShowId" => {
                item.show_id = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.show_id.clone().unwrap_or_default()
                    ))
                };
            }
            "Size" => {
                item.size = if null {
                    None
                } else {
                    Some(item.size.unwrap_or(0) + 1)
                };
            }
            "SortName" => {
                item.sort_name = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.sort_name.clone().unwrap_or_default()
                    ))
                };
            }
            "StartDate" => {
                item.start_date = if null {
                    None
                } else {
                    item.start_date.map(|d| d + chrono::TimeDelta::days(1))
                };
            }
            "Studios" => {
                item.studios = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.studios.clone().unwrap_or_default()
                    ))
                };
            }
            "Tagline" => {
                item.tagline = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.tagline.clone().unwrap_or_default()
                    ))
                };
            }
            "Tags" => {
                item.tags = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.tags.clone().unwrap_or_default()
                    ))
                };
            }
            "TopParentId" => {
                item.top_parent_id = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.top_parent_id.clone().unwrap_or_default()
                    ))
                };
            }
            "TotalBitrate" => {
                item.total_bitrate = if null {
                    None
                } else {
                    Some(item.total_bitrate.unwrap_or(0) + 1)
                };
            }
            "Type" => {
                if !null {
                    item.type_ = crate::item_type_lookup::stored_type_name(BaseItemKind::Episode)
                        .unwrap()
                        .to_owned();
                }
            }
            "UnratedType" => {
                item.unrated_type = if null {
                    None
                } else {
                    Some(format!(
                        "{} (changed)",
                        item.unrated_type.clone().unwrap_or_default()
                    ))
                };
            }
            "Width" => {
                item.width = if null {
                    None
                } else {
                    Some(item.width.unwrap_or(0) + 1)
                };
            }
            other_column => panic!("no variation for column {other_column}"),
        }
    }

    /// The differential check of `scan_save_changes_row` against the real
    /// scan upsert: for every written column, over a stored row that is
    /// locked or not, and an incoming value that is NULL, equal or
    /// different, the stored row changes (anything but `DateLastSaved`)
    /// exactly when `scan_save_changes_row` says it would.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn scan_save_changes_row_predicts_the_scan_upsert_column_by_column() {
        use ferrofin_traits::persistence::ItemRepository as _;
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let repo = crate::item_repository::FerrofinItemRepository::new(
            db.clone(),
            std::sync::Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
        );
        let (id, parent, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        seed_item(&db, parent, BaseItemKind::Folder).await;
        seed_item(&db, other, BaseItemKind::Folder).await;
        let at = |s: &str| {
            Some(
                chrono::DateTime::parse_from_rfc3339(s)
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            )
        };
        let text = |s: &str| Some(s.to_owned());
        let base = ferrofin_db::entities::base_items::BaseItemEntity {
            id: guid_to_db(id),
            album: text("Album"),
            album_artists: text("AA"),
            artists: text("A"),
            audio: Some(1),
            channel_id: text("CH"),
            clean_name: None,
            community_rating: Some(7.5),
            critic_rating: Some(80.0),
            custom_rating: text("CR"),
            data: text(r#"{"VideoType":"VideoFile"}"#),
            date_created: at("2020-01-01T00:00:00Z"),
            date_last_media_added: at("2020-01-02T00:00:00Z"),
            date_last_refreshed: at("2020-01-03T00:00:00Z"),
            date_last_saved: at("2020-01-04T00:00:00Z"),
            date_modified: at("2020-01-05T00:00:00Z"),
            end_date: at("2020-01-06T00:00:00Z"),
            episode_title: text("ET"),
            external_id: text("X"),
            external_series_id: text("XS"),
            external_service_id: text("XSV"),
            extra_type: Some(2),
            forced_sort_name: text("Forced"),
            genres: text("Drama"),
            height: Some(1080),
            index_number: Some(3),
            inherited_parental_rating_sub_value: Some(1),
            inherited_parental_rating_value: Some(12),
            is_folder: false,
            is_in_mixed_folder: false,
            is_locked: false,
            is_movie: true,
            is_repeat: false,
            is_series: false,
            is_virtual_item: false,
            lufs: Some(-14.0),
            media_type: text("Video"),
            name: text("Name"),
            normalization_gain: Some(1.5),
            official_rating: text("PG"),
            original_language: text("en"),
            original_title: text("Original"),
            overview: text("Overview"),
            owner_id: Some(guid_to_db(parent)),
            parent_id: Some(guid_to_db(parent)),
            parent_index_number: Some(1),
            path: text("/media/movie.mkv"),
            preferred_metadata_country_code: text("US"),
            preferred_metadata_language: text("en"),
            premiere_date: at("1999-03-30T00:00:00Z"),
            presentation_unique_key: None,
            primary_version_id: None,
            production_locations: text("USA"),
            production_year: Some(1999),
            run_time_ticks: Some(100),
            season_id: text("SEASON"),
            season_name: text("Season 1"),
            series_id: text("SERIES"),
            series_name: text("Series"),
            series_presentation_unique_key: text("SPK"),
            show_id: text("SHOW"),
            size: Some(10),
            sort_name: None,
            start_date: at("2020-01-07T00:00:00Z"),
            studios: text("Studio"),
            tagline: text("Tagline"),
            tags: text("Tag"),
            top_parent_id: text("TOP"),
            total_bitrate: Some(8000),
            type_: stored_type_name(BaseItemKind::Movie).unwrap().to_owned(),
            unrated_type: text("UT"),
            width: Some(1920),
        };
        let columns: Vec<&str> = super::written_columns(&base)
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| *name != "Id")
            .collect();
        let snapshot_sql = format!(
            r#"SELECT {} FROM "BaseItems" WHERE "Id" = ?1"#,
            columns
                .iter()
                .filter(|c| **c != "DateLastSaved")
                .map(|c| format!(r#"quote("{c}")"#))
                .collect::<Vec<_>>()
                .join(" || '|' || ")
        );
        let snapshot = || async {
            sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(snapshot_sql.as_str()))
                .bind(guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("snapshot")
        };
        let other = guid_to_db(other);
        let (mut checked, mut changed_cases) = (0, 0);
        for writes_date_created in [false, true] {
            for column in &columns {
                for locked in [false, true] {
                    for mode in ["null", "equal", "different"] {
                        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
                            .bind(guid_to_db(id))
                            .execute(db.writer())
                            .await
                            .expect("reset");
                        svc.save_items(std::slice::from_ref(&base))
                            .await
                            .expect("seed");
                        sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = ?1 WHERE "Id" = ?2"#)
                            .bind(locked)
                            .bind(guid_to_db(id))
                            .execute(db.writer())
                            .await
                            .expect("lock");
                        let stored = repo.retrieve_item(id).await.expect("read").expect("row");
                        let mut incoming = base.clone();
                        if mode != "equal" {
                            vary(&mut incoming, column, mode == "null", &other);
                        }
                        let predicted =
                            super::scan_save_changes_row(&incoming, &stored, writes_date_created);
                        let before = snapshot().await;
                        if writes_date_created {
                            svc.save_scanned_items_with_date_created(std::slice::from_ref(
                                &incoming,
                            ))
                            .await
                            .expect("scan save");
                        } else {
                            svc.save_scanned_items(std::slice::from_ref(&incoming))
                                .await
                                .expect("scan save");
                        }
                        let row_moved = snapshot().await != before;
                        assert_eq!(
                            row_moved, predicted,
                            "column {column}, locked {locked}, incoming {mode}, \
                         writes DateCreated {writes_date_created}"
                        );
                        checked += 1;
                        changed_cases += usize::from(row_moved);
                    }
                }
            }
        }
        assert_eq!(checked, columns.len() * 12);
        assert!(
            changed_cases > columns.len(),
            "the cases have teeth: {changed_cases} changed"
        );
    }

    /// The comparison mirrors the statement: its column list is the
    /// statement's, in bind order.
    #[test]
    fn written_columns_follow_the_upsert_column_order() {
        let head = super::UPSERT_SQL
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(cols, _)| cols)
            .expect("column list");
        let statement: Vec<&str> = head
            .split(',')
            .map(|c| c.trim().trim_matches('"'))
            .collect();
        let bound: Vec<&str> =
            super::written_columns(&ferrofin_db::entities::base_items::BaseItemEntity::default())
                .into_iter()
                .map(|(name, _)| name)
                .collect();
        assert_eq!(bound, statement);
    }

    /// `scan_save_changes_row` answers "would the scan save change this
    /// row?" the way the statement does: a row read back after a save is
    /// unchanged by saving it again; any column the scan owns changes it;
    /// the columns the statement guards do not.
    #[tokio::test]
    async fn a_row_read_back_after_a_scan_save_is_unchanged_by_saving_it_again() {
        use ferrofin_traits::persistence::ItemRepository as _;
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let repo = crate::item_repository::FerrofinItemRepository::new(
            db.clone(),
            std::sync::Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
        );
        let id = Uuid::new_v4();
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let item = ferrofin_db::entities::base_items::BaseItemEntity {
            id: guid_to_db(id),
            type_: stored_type_name(BaseItemKind::Movie).unwrap().to_owned(),
            name: Some("The Matrix".into()),
            overview: Some("A hacker learns the truth.".into()),
            community_rating: Some(8.2),
            // Nanosecond precision, as a stat reports it; the table keeps
            // 100 ns.
            date_modified: Some(at("2026-09-01T08:30:00.123456789Z")),
            date_created: Some(at("2026-01-02T03:04:05Z")),
            size: Some(10),
            data: Some(r#"{"VideoType":"VideoFile"}"#.into()),
            ..Default::default()
        };
        svc.save_scanned_items(std::slice::from_ref(&item))
            .await
            .expect("save");
        let stored = repo.retrieve_item(id).await.expect("read").expect("row");
        assert!(!super::scan_save_changes_row(&stored, &stored, false));
        assert!(
            !super::scan_save_changes_row(&item, &stored, false),
            "the row as built (no derived columns, full-precision mtime) \
             saves to the same stored values"
        );

        // A scan-owned column changes it.
        let renamed = ferrofin_db::entities::base_items::BaseItemEntity {
            overview: Some("Reality is a simulation.".into()),
            ..item.clone()
        };
        assert!(super::scan_save_changes_row(&renamed, &stored, false));
        let moved = ferrofin_db::entities::base_items::BaseItemEntity {
            date_modified: Some(at("2026-09-02T08:30:00Z")),
            ..item.clone()
        };
        assert!(super::scan_save_changes_row(&moved, &stored, false));

        // Guarded columns do not: a NULL never clears Size/DateModified/the
        // refresh dates, DateCreated only fills a gap, and DateLastSaved is
        // the save's own stamp.
        let guarded = ferrofin_db::entities::base_items::BaseItemEntity {
            size: None,
            date_modified: None,
            date_last_refreshed: None,
            date_created: Some(at("2026-09-24T00:00:00Z")),
            date_last_saved: Some(at("2026-09-24T00:00:00Z")),
            ..item.clone()
        };
        assert!(!super::scan_save_changes_row(&guarded, &stored, false));
        // The re-dating save writes the new DateCreated, and still never
        // clears it.
        assert!(super::scan_save_changes_row(&guarded, &stored, true));
        let undated = ferrofin_db::entities::base_items::BaseItemEntity {
            date_created: None,
            ..item.clone()
        };
        assert!(!super::scan_save_changes_row(&undated, &stored, true));

        // A locked row keeps its user-owned columns, but not the file facts.
        let locked = ferrofin_db::entities::base_items::BaseItemEntity {
            is_locked: true,
            ..stored.clone()
        };
        assert!(!super::scan_save_changes_row(&renamed, &locked, false));
        assert!(super::scan_save_changes_row(&moved, &locked, false));
    }

    /// An undecodable row's file-facts write carries `BeforeSaveInternal`'s
    /// re-date: `DateCreated` moves to the given date when the new
    /// `DateModified` drifted from the stored one by more than a second (an
    /// unset one included), never within the second, and never without a
    /// date to move to.
    #[tokio::test]
    async fn update_file_facts_redates_a_changed_file() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let id = Uuid::new_v4();
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let row = ferrofin_db::entities::base_items::BaseItemEntity {
            id: guid_to_db(id),
            type_: stored_type_name(BaseItemKind::Episode).unwrap().to_owned(),
            name: Some("S01E01".into()),
            path: Some("/tv/Show/S01E01.mkv".into()),
            date_created: Some(at("2020-01-01T00:00:00Z")),
            date_modified: Some(at("2020-01-05T00:00:00Z")),
            ..Default::default()
        };
        svc.save_items(std::slice::from_ref(&row))
            .await
            .expect("seed");
        let created = async || -> Option<String> {
            sqlx::query_scalar(r#"SELECT "DateCreated" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("row")
        };
        let born = at("2019-06-07T08:09:10Z");
        let stat = |mtime: &str| ferrofin_db::entities::base_items::BaseItemEntity {
            date_modified: Some(at(mtime)),
            ..row.clone()
        };

        // Within the second: the mtime is written, the date stays.
        assert!(
            svc.update_file_facts(&stat("2020-01-05T00:00:00.500Z"), Some(born))
                .await
                .expect("write")
        );
        assert_eq!(
            created().await,
            ferrofin_db::store::opt_datetime_to_db(row.date_created)
        );
        // No date to move to (another rule, or a folder): it stays.
        assert!(
            svc.update_file_facts(&stat("2021-01-01T00:00:00Z"), None)
                .await
                .expect("write")
        );
        assert_eq!(
            created().await,
            ferrofin_db::store::opt_datetime_to_db(row.date_created)
        );
        // Drifted past the second: re-dated.
        assert!(
            svc.update_file_facts(&stat("2022-01-01T00:00:00Z"), Some(born))
                .await
                .expect("write")
        );
        assert_eq!(
            created().await,
            ferrofin_db::store::opt_datetime_to_db(Some(born))
        );
        // An unset stored DateModified counts as changed.
        sqlx::query(r#"UPDATE "BaseItems" SET "DateModified" = NULL WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .expect("clear");
        let later = at("2023-03-03T03:03:03Z");
        assert!(
            svc.update_file_facts(&stat("2022-01-01T00:00:00Z"), Some(later))
                .await
                .expect("write")
        );
        assert_eq!(
            created().await,
            ferrofin_db::store::opt_datetime_to_db(Some(later))
        );
    }

    /// `BaseItemMetadataFields`: the editor's replace-all write (duplicates
    /// written once, an empty set clearing it), the batch read in field
    /// order with the stored discriminants upstream's enum values, and the
    /// scan's window read carrying the set.
    #[tokio::test]
    async fn locked_fields_round_trip_through_base_item_metadata_fields() {
        use ferrofin_model::entities::MetadataField;
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let (item, other) = (Uuid::new_v4(), Uuid::new_v4());
        for id in [item, other] {
            seed_item(&db, id, BaseItemKind::Movie).await;
        }
        svc.replace_locked_fields(item, &[8, 0])
            .await
            .expect("first set");
        // An id this server has no name for is stored as sent (upstream's
        // enum carries it) and skipped on read.
        svc.replace_locked_fields(item, &[6, 5, 6, 42])
            .await
            .expect("replaced");
        let stored: Vec<(i64, String)> =
            sqlx::query_as(r#"SELECT "Id", "ItemId" FROM "BaseItemMetadataFields" ORDER BY "Id""#)
                .fetch_all(db.pool())
                .await
                .expect("rows");
        // `MetadataField.Name = 5`, `Overview = 6` (MetadataField.cs).
        assert_eq!(
            stored,
            vec![
                (5, guid_to_db(item)),
                (6, guid_to_db(item)),
                (42, guid_to_db(item))
            ]
        );

        let map = svc
            .locked_fields_for_items(&[item, other])
            .await
            .expect("read");
        assert_eq!(
            map.get(&item),
            Some(&vec![MetadataField::Name, MetadataField::Overview])
        );
        assert!(!map.contains_key(&other));

        let links = svc
            .scan_stored_links(&[item])
            .await
            .expect("read")
            .expect("supported");
        assert_eq!(
            links[&item].locked_fields,
            vec![MetadataField::Name, MetadataField::Overview]
        );

        // The NFO union adds without deleting: the unnamed 42 survives.
        svc.add_locked_fields(item, &[5, 0]).await.expect("added");
        let ids: Vec<i64> = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItemMetadataFields" WHERE "ItemId" = ?1 ORDER BY "Id""#,
        )
        .bind(guid_to_db(item))
        .fetch_all(db.pool())
        .await
        .expect("rows");
        assert_eq!(ids, vec![0, 5, 6, 42]);

        svc.replace_locked_fields(item, &[]).await.expect("cleared");
        assert!(
            svc.locked_fields_for_items(&[item])
                .await
                .expect("read")
                .is_empty()
        );
    }

    /// Observe real SQLite mutations: identical final values alone would miss
    /// a DELETE/INSERT replacement or an unconditional UPDATE.
    async fn image_writes(db: &ferrofin_db::Database) -> Vec<(String, String)> {
        let rows =
            sqlx::query_as(r#"SELECT "Op", "ImageId" FROM "TestImageWrites" ORDER BY rowid"#)
                .fetch_all(db.pool())
                .await
                .unwrap();
        sqlx::query(r#"DELETE FROM "TestImageWrites""#)
            .execute(db.writer())
            .await
            .unwrap();
        rows
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn image_saves_only_write_changed_rows() {
        use ferrofin_db::entities::base_items::BaseItemImageInfoEntity;
        use ferrofin_model::entities::ImageType;
        use ferrofin_traits::options::ItemImageInfo;
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Person).await;
        sqlx::query(r#"CREATE TABLE "TestImageWrites" ("Op" TEXT, "ImageId" TEXT)"#)
            .execute(db.writer())
            .await
            .unwrap();
        for (op, row) in [("INSERT", "NEW"), ("UPDATE", "NEW"), ("DELETE", "OLD")] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                r#"CREATE TRIGGER "TestImage_{op}" AFTER {op} ON "BaseItemImageInfos"
                   BEGIN INSERT INTO "TestImageWrites" VALUES ('{op}', {row}."Id"); END"#
            )))
            .execute(db.writer())
            .await
            .unwrap();
        }
        let primary = ItemImageInfo {
            path: "/metadata/person/primary.jpg".to_owned(),
            image_type: ImageType::Primary,
            // Values beyond the database's precision must not trigger writes
            // each time this identical input is passed back to persistence.
            date_modified: chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap(),
            width: 48,
            height: 64,
            blur_hash: Some("hash".to_owned()),
        };
        let backdrop = ItemImageInfo {
            path: "/metadata/person/backdrop.jpg".to_owned(),
            image_type: ImageType::Backdrop,
            ..primary.clone()
        };
        let mut images = vec![primary, backdrop.clone(), backdrop];
        svc.save_item_images(item, &images).await.unwrap();
        assert_eq!(image_writes(&db).await.len(), 3);
        let read = || async {
            sqlx::query_as::<_, BaseItemImageInfoEntity>(
                r#"SELECT * FROM "BaseItemImageInfos" WHERE "ItemId" = ?1 ORDER BY "Id""#,
            )
            .bind(guid_to_db(item))
            .fetch_all(db.pool())
            .await
            .unwrap()
        };
        let before = read().await;
        let primary_id = before
            .iter()
            .find(|r| r.image_type == 0)
            .unwrap()
            .id
            .clone();
        // Even a reordered list with duplicate paths is the same multiset.
        images.reverse();
        svc.save_item_images(item, &images).await.unwrap();
        assert!(image_writes(&db).await.is_empty());
        assert_eq!(read().await, before);
        images.reverse();

        // Each persisted metadata field independently triggers just one UPDATE,
        // retaining the row id. The backdrop rows are untouched throughout.
        for field in ["width", "height", "blurhash", "mtime"] {
            match field {
                "width" => images[0].width += 1,
                "height" => images[0].height += 1,
                "blurhash" => images[0].blur_hash = None,
                "mtime" => images[0].date_modified += chrono::Duration::seconds(1),
                _ => unreachable!(),
            }
            svc.save_item_images(item, &images).await.unwrap();
            assert_eq!(
                image_writes(&db).await,
                vec![("UPDATE".to_owned(), primary_id.clone())],
                "{field}"
            );
            svc.save_item_images(item, &images).await.unwrap();
            assert!(image_writes(&db).await.is_empty(), "{field} settled");
        }
        // Removing one duplicate removes one row, without replacing its twin.
        images.pop();
        svc.save_item_images(item, &images).await.unwrap();
        let removed = image_writes(&db).await;
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].0, "DELETE");
        assert_ne!(removed[0].1, primary_id);
        let retained = read()
            .await
            .into_iter()
            .find(|r| r.image_type == 2)
            .unwrap();

        // A replacement at a different path removes/inserts that image only.
        images[0].path = "/metadata/person/new-primary.jpg".to_owned();
        svc.save_item_images(item, &images).await.unwrap();
        let replaced = image_writes(&db).await;
        assert_eq!(replaced.len(), 2);
        assert_eq!(replaced[0].0, "INSERT");
        assert_eq!(replaced[1], ("DELETE".to_owned(), primary_id));
        assert!(read().await.contains(&retained));
        // A changed type at the same path also replaces just that entry.
        images[0].image_type = ImageType::Thumb;
        svc.save_item_images(item, &images).await.unwrap();
        assert_eq!(image_writes(&db).await.len(), 2);
        assert!(read().await.contains(&retained));
        svc.save_item_images(item, &[]).await.unwrap();
        assert_eq!(image_writes(&db).await.len(), 2);
        assert!(read().await.is_empty());
        svc.save_item_images(item, &[]).await.unwrap();
        assert!(image_writes(&db).await.is_empty());
    }

    #[tokio::test]
    async fn scan_stored_links_reads_images_ancestors_and_external_streams() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let (parent, item, bare) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for id in [parent, item, bare] {
            seed_item(&db, id, BaseItemKind::Movie).await;
        }
        svc.set_ancestors(item, &[parent]).await.expect("ancestors");
        let poster = ferrofin_traits::options::ItemImageInfo {
            path: "/media/poster.jpg".into(),
            image_type: ferrofin_model::entities::ImageType::Primary,
            date_modified: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            width: 10,
            height: 20,
            blur_hash: Some("hash".into()),
        };
        svc.save_item_images(item, std::slice::from_ref(&poster))
            .await
            .expect("images");
        let streams: Vec<ferrofin_db::entities::base_items::MediaStreamInfoEntity> = [
            (0, 2, true, Some("/media/movie.eng.srt")),
            (1, 0, true, Some("/media/movie.fra.mka")),
            (2, 1, false, Some("/media/movie.mkv")),
            (3, 2, true, None),
        ]
        .into_iter()
        .map(|(stream_index, stream_type, is_external, path)| {
            ferrofin_db::entities::base_items::MediaStreamInfoEntity {
                item_id: guid_to_db(item),
                stream_index,
                stream_type,
                is_external,
                path: path.map(str::to_owned),
                ..Default::default()
            }
        })
        .collect();
        {
            use ferrofin_traits::persistence::MediaStreamRepository as _;
            crate::media_stream_repository::FerrofinMediaStreamRepository::new(db.clone())
                .save_media_streams(item, &streams)
                .await
                .expect("streams");
        }

        let links = svc
            .scan_stored_links(&[item, bare])
            .await
            .expect("read")
            .expect("supported");
        let got = links.get(&item).expect("item links");
        assert_eq!(got.images, vec![poster]);
        assert_eq!(got.ancestors, vec![parent]);
        assert_eq!(
            got.external_subtitles,
            vec!["/media/movie.eng.srt".to_owned()]
        );
        assert_eq!(got.external_audio, vec!["/media/movie.fra.mka".to_owned()]);
        assert!(
            !links.contains_key(&bare),
            "an item with no rows reads as no entry"
        );
    }

    // One test for the whole scan statement: its guards are derived from one
    // text substitution, so they are asserted together.
    #[allow(clippy::too_many_lines, clippy::items_after_statements)]
    #[tokio::test]
    async fn scan_upsert_preserves_unowned_columns() {
        // Guard the text-substitution derivation of the scan SQL: if the base
        // UPSERT_SQL text drifts, the replacements silently no-op and this
        // catches it before the behavioral asserts do.
        let sql = super::scan_upsert_sql();
        assert!(sql.contains(r#"coalesce("DateCreated", excluded."DateCreated")"#));
        // The re-dating variant differs in that one clause only.
        let redated = super::scan_upsert_date_created_sql();
        assert!(
            redated.contains(r#""DateCreated" = coalesce(excluded."DateCreated", "DateCreated")"#)
        );
        assert_eq!(
            redated.replace(
                r#""DateCreated" = coalesce(excluded."DateCreated", "DateCreated")"#,
                r#""DateCreated" = coalesce("DateCreated", excluded."DateCreated")"#,
            ),
            sql
        );
        assert!(!sql.contains(r#""PrimaryVersionId" = excluded."PrimaryVersionId""#));
        assert!(sql.contains(r#""IsLocked" = max("IsLocked", excluded."IsLocked")"#));
        for col in super::SCAN_NEVER_CLEARED_COLUMNS {
            assert!(
                sql.contains(&format!(r#""{col}" = coalesce(excluded."{col}", "{col}")"#)),
                "never-cleared guard missing for column {col}"
            );
        }
        for stmt in [sql, super::UPSERT_SQL] {
            assert!(
                stmt.contains(r#""DateLastSaved" = ?73"#),
                "an update stamps DateLastSaved"
            );
        }
        for col in super::LOCKED_PRESERVED_COLUMNS {
            let kept = if super::LOCKED_FILLABLE_COLUMNS.contains(col) {
                format!(r#""IsLocked" = 1 AND nullif("{col}", '') IS NOT NULL"#)
            } else {
                r#""IsLocked" = 1"#.to_owned()
            };
            let kept = if ["Name", "CleanName", "SortName"].contains(col) {
                format!(r#"({kept}) AND excluded."OwnerId" IS NULL"#)
            } else {
                kept
            };
            let locked_value = if *col == "Data" {
                super::LOCKED_DATA_SQL.to_owned()
            } else {
                format!(r#""{col}""#)
            };
            assert!(
                sql.contains(&format!(
                    r#""{col}" = CASE WHEN {kept} THEN {locked_value} ELSE"#
                )),
                "locked guard missing for column {col}"
            );
        }

        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let id = Uuid::new_v4();
        let first_import = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        // A merged alternate version: full save sets the link + import date.
        let mut item = ferrofin_db::entities::base_items::BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: crate::item_type_lookup::stored_type_name(BaseItemKind::Episode)
                .unwrap()
                .to_owned(),
            name: Some("S01E01".into()),
            primary_version_id: Some("PRIMARY-ID".into()),
            date_created: Some(first_import),
            ..Default::default()
        };
        svc.save_items(std::slice::from_ref(&item))
            .await
            .expect("full save");

        // The next scan re-saves the same row rebuilt from disk: link gone,
        // DateCreated re-stamped to scan time.
        item.primary_version_id = None;
        item.date_created = Some(chrono::Utc::now());
        item.name = Some("S01E01 rescanned".into());
        svc.save_scanned_items(std::slice::from_ref(&item))
            .await
            .expect("scan save");

        let (name, pvid, created): (Option<String>, Option<String>, Option<String>) =
            sqlx::query_as(
                r#"SELECT "Name", "PrimaryVersionId", "DateCreated"
                   FROM "BaseItems" WHERE "Id" = ?1"#,
            )
            .bind(ferrofin_db::store::guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("row");
        assert_eq!(name.as_deref(), Some("S01E01 rescanned"), "scan owns Name");
        assert_eq!(pvid.as_deref(), Some("PRIMARY-ID"), "merge link survives");
        assert_eq!(
            created,
            ferrofin_db::store::opt_datetime_to_db(Some(first_import)),
            "first-import DateCreated survives"
        );

        // A file re-dated by `BeforeSaveInternal` writes its DateCreated,
        // the merge link still kept; a NULL one keeps the stored date.
        let created_at = async || -> Option<String> {
            sqlx::query_scalar(r#"SELECT "DateCreated" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("row")
        };
        let born = chrono::DateTime::parse_from_rfc3339("2025-06-07T08:09:10Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        item.date_created = Some(born);
        svc.save_scanned_items_with_date_created(std::slice::from_ref(&item))
            .await
            .expect("re-dating save");
        assert_eq!(
            created_at().await,
            ferrofin_db::store::opt_datetime_to_db(Some(born))
        );
        item.date_created = None;
        svc.save_scanned_items_with_date_created(std::slice::from_ref(&item))
            .await
            .expect("re-dating save");
        assert_eq!(
            created_at().await,
            ferrofin_db::store::opt_datetime_to_db(Some(born)),
            "a NULL never clears it"
        );
        let pvid: Option<String> =
            sqlx::query_scalar(r#"SELECT "PrimaryVersionId" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("row");
        assert_eq!(pvid.as_deref(), Some("PRIMARY-ID"), "merge link survives");

        // Split/unmerge still clears the link through the full save.
        svc.save_items(std::slice::from_ref(&item))
            .await
            .expect("full re-save");
        let pvid: Option<String> =
            sqlx::query_scalar(r#"SELECT "PrimaryVersionId" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("row");
        assert_eq!(pvid, None, "full save still clears the merge link");

        // ── The dates and `Data` (scan plan, Phase 3) ────────────────────
        type Dates = (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
        );
        let read = async |id: Uuid| -> Dates {
            sqlx::query_as(
                r#"SELECT "DateLastSaved", "DateLastRefreshed", "DateLastMediaAdded",
                          "DateModified", "Size", "Data"
                   FROM "BaseItems" WHERE "Id" = ?1"#,
            )
            .bind(ferrofin_db::store::guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("row")
        };
        // A brand-new row keeps a NULL `DateLastSaved` (`CreateItems` never
        // stamps it) — whichever save path inserts it.
        let fresh = Uuid::new_v4();
        let mut row = ferrofin_db::entities::base_items::BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(fresh),
            ..item.clone()
        };
        svc.save_scanned_items(std::slice::from_ref(&row))
            .await
            .expect("scan insert");
        assert_eq!(
            read(fresh).await.0,
            None,
            "an insert leaves DateLastSaved NULL"
        );

        // A provider pass stamps the refresh date; the folder pass the media
        // date; the save its stat. Every update stamps `DateLastSaved`.
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let trailers = r#"{"RemoteTrailers":[{"Url":"https://youtu.be/x"}]}"#;
        row.date_last_refreshed = Some(at("2026-02-01T00:00:00Z"));
        row.date_last_media_added = Some(at("2026-03-01T00:00:00Z"));
        row.date_modified = Some(at("2026-04-01T00:00:00Z"));
        row.size = Some(1234);
        row.data = Some(trailers.into());
        let before = ferrofin_db::store::datetime_to_db(chrono::Utc::now());
        svc.save_items(std::slice::from_ref(&row))
            .await
            .expect("full update");
        let saved = read(fresh).await;
        let stamped = saved.0.clone().expect("an update stamps DateLastSaved");
        assert!(stamped >= before, "{stamped} is the save time");

        // A scan save of a row it could not read carries none of them: the
        // scan must still never clear them, and it re-stamps the save time.
        let scanned = ferrofin_db::entities::base_items::BaseItemEntity {
            date_last_refreshed: None,
            date_last_media_added: None,
            date_last_saved: None,
            date_modified: None,
            size: None,
            data: Some(r#"{"VideoType":"VideoFile"}"#.into()),
            ..row.clone()
        };
        svc.save_scanned_items(std::slice::from_ref(&scanned))
            .await
            .expect("scan update");
        let after = read(fresh).await;
        assert!(
            after.0.clone().expect("stamped") >= stamped,
            "re-stamped on the scan save"
        );
        assert_eq!(after.1, saved.1, "DateLastRefreshed survives the scan");
        assert_eq!(after.2, saved.2, "DateLastMediaAdded survives the scan");
        assert_eq!(after.3, saved.3, "DateModified survives an un-stat'ed save");
        assert_eq!(after.4, Some(1234), "Size survives an un-stat'ed save");
        assert_eq!(
            after.5.as_deref(),
            Some(r#"{"VideoType":"VideoFile"}"#),
            "an unlocked row takes the scan's (merged) Data"
        );

        // A locked row keeps its `Data` blob — trailers, series status… —
        // and takes only the resolver's `VideoType` (`UpdateFromResolvedItem`).
        svc.save_items(std::slice::from_ref(&row))
            .await
            .expect("restore trailers");
        sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = 1 WHERE "Id" = ?1"#)
            .bind(ferrofin_db::store::guid_to_db(fresh))
            .execute(db.writer())
            .await
            .expect("lock");
        svc.save_scanned_items(std::slice::from_ref(&scanned))
            .await
            .expect("locked scan update");
        assert_eq!(
            read(fresh).await.5.as_deref(),
            Some(r#"{"RemoteTrailers":[{"Url":"https://youtu.be/x"}],"VideoType":"VideoFile"}"#),
            "a locked row's Data survives the scan but for its VideoType"
        );
    }

    /// A locked row's `Data` on a scan save (`LOCKED_DATA_SQL`): only the
    /// resolver's `VideoType` is set in it (`Video.UpdateFromResolvedItem`,
    /// whatever the lock); every other key keeps its value and its place,
    /// though `json_set` re-renders the document minified. A blob that is not
    /// a JSON object, an incoming blob with no `VideoType`, or one that
    /// already matches leave the stored text as it is.
    /// `scan_save_changes_row` agrees with the SQL in every case.
    #[rstest::rstest]
    #[case::replaced(
        Some(r#"{"VideoType":"Dvd", "RemoteTrailers":[{"Url":"u"}],"IsoType":"Dvd"}"#),
        Some(r#"{"VideoType":"VideoFile"}"#),
        Some(r#"{"VideoType":"VideoFile","RemoteTrailers":[{"Url":"u"}],"IsoType":"Dvd"}"#)
    )]
    #[case::added(
        Some(r#"{"Status":"Ended"}"#),
        Some(r#"{"VideoType":"Iso","IsoType":"BluRay"}"#),
        Some(r#"{"Status":"Ended","VideoType":"Iso"}"#)
    )]
    #[case::null_blob(
        None,
        Some(r#"{"VideoType":"VideoFile"}"#),
        Some(r#"{"VideoType":"VideoFile"}"#)
    )]
    #[case::empty_blob(
        Some(""),
        Some(r#"{"VideoType":"VideoFile"}"#),
        Some(r#"{"VideoType":"VideoFile"}"#)
    )]
    #[case::same(
        Some(r#"{"VideoType":"VideoFile", "Status":"Ended"}"#),
        Some(r#"{"VideoType":"VideoFile"}"#),
        Some(r#"{"VideoType":"VideoFile", "Status":"Ended"}"#)
    )]
    #[case::no_video_type(
        Some(r#"{"VideoType":"Dvd"}"#),
        Some(r#"{"RemoteTrailers":[]}"#),
        Some(r#"{"VideoType":"Dvd"}"#)
    )]
    #[case::no_incoming_blob(Some(r#"{"VideoType":"Dvd"}"#), None, Some(r#"{"VideoType":"Dvd"}"#))]
    #[case::malformed(
        Some("not json"),
        Some(r#"{"VideoType":"VideoFile"}"#),
        Some("not json")
    )]
    #[case::array(Some("[1,2]"), Some(r#"{"VideoType":"VideoFile"}"#), Some("[1,2]"))]
    #[case::malformed_incoming(
        Some(r#"{"VideoType":"Dvd"}"#),
        Some("{"),
        Some(r#"{"VideoType":"Dvd"}"#)
    )]
    #[tokio::test]
    async fn a_locked_rows_data_takes_only_the_resolvers_video_type(
        #[case] stored: Option<&str>,
        #[case] incoming: Option<&str>,
        #[case] expected: Option<&str>,
    ) {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let id = Uuid::new_v4();
        let row = ferrofin_db::entities::base_items::BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: crate::item_type_lookup::stored_type_name(BaseItemKind::Movie)
                .unwrap()
                .to_owned(),
            name: Some("Heat".into()),
            is_locked: true,
            data: stored.map(str::to_owned),
            ..Default::default()
        };
        svc.save_items(std::slice::from_ref(&row))
            .await
            .expect("seed");
        let row = sqlx::query_as::<_, ferrofin_db::entities::base_items::BaseItemEntity>(
            r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#,
        )
        .bind(ferrofin_db::store::guid_to_db(id))
        .fetch_one(db.pool())
        .await
        .expect("stored row");
        let scanned = ferrofin_db::entities::base_items::BaseItemEntity {
            data: incoming.map(str::to_owned),
            ..row.clone()
        };
        assert_eq!(
            super::scan_save_changes_row(&scanned, &row, false),
            expected != row.data.as_deref(),
            "the change rule agrees with the SQL"
        );
        svc.save_scanned_items(std::slice::from_ref(&scanned))
            .await
            .expect("scan save");
        let data: Option<String> =
            sqlx::query_scalar(r#"SELECT "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("row");
        assert_eq!(data.as_deref(), expected);
    }

    // Merge/split write their link through set_primary_version_id, which must
    // touch ONLY that column: the callers hold rows loaded earlier, and a
    // full-row write would revert concurrent scan/refresh/edit writes to the
    // load-time values.
    #[tokio::test]
    async fn set_primary_version_id_leaves_other_columns_alone() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let (id, primary) = (Uuid::new_v4(), Uuid::new_v4());
        seed_item(&db, id, BaseItemKind::Episode).await;
        // The primary must exist: the link row's ParentId is a foreign key.
        seed_item(&db, primary, BaseItemKind::Episode).await;
        // A concurrent writer's change, landed after any caller loaded the row.
        sqlx::query(
            r#"UPDATE "BaseItems" SET "Name" = 'fresh title', "RunTimeTicks" = 42 WHERE "Id" = ?1"#,
        )
        .bind(ferrofin_db::store::guid_to_db(id))
        .execute(db.writer())
        .await
        .expect("concurrent write");

        svc.set_primary_version_id(id, Some(primary))
            .await
            .expect("link");
        let read =
            async |id: Uuid| -> (Option<String>, Option<i64>, Option<String>, Option<String>) {
                sqlx::query_as(
                    r#"SELECT "Name", "RunTimeTicks", "PrimaryVersionId", "PresentationUniqueKey"
                   FROM "BaseItems" WHERE "Id" = ?1"#,
                )
                .bind(ferrofin_db::store::guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("row")
            };
        let (name, ticks, pvid, key) = read(id).await;
        assert_eq!(pvid, Some(ferrofin_db::store::guid_to_db(primary)));
        // C# `Video.SetPrimaryVersionId` rewrites the presentation key to the
        // PRIMARY's id in "N" form, which is what makes every copy of a film
        // count as one item in "similar", Next Up and the resume rows.
        assert_eq!(
            key.as_deref(),
            Some(primary.as_simple().to_string().as_str())
        );
        assert_eq!(
            name.as_deref(),
            Some("fresh title"),
            "link write must not touch Name"
        );
        assert_eq!(ticks, Some(42), "link write must not touch RunTimeTicks");

        // Unlinking reverts the key to the item's own id, as
        // `base.CreatePresentationUniqueKey()` does.
        svc.set_primary_version_id(id, None).await.expect("unlink");
        let (_, _, pvid, key) = read(id).await;
        assert_eq!(pvid, None);
        assert_eq!(key.as_deref(), Some(id.as_simple().to_string().as_str()));

        svc.set_primary_version_id(id, None).await.expect("unlink");
        let pvid: Option<String> =
            sqlx::query_scalar(r#"SELECT "PrimaryVersionId" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("row");
        assert_eq!(pvid, None);
    }

    // A locked row keeps its user-edited metadata through a scan save (which
    // rebuilds the entity from disk), while file-derived columns still update
    // and the scan can never clear the lock itself. Unlocked rows keep taking
    // the scanned values.
    #[tokio::test]
    async fn scan_upsert_keeps_locked_metadata_and_never_unlocks() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let id = Uuid::new_v4();

        // The user's edit: custom title/overview + the editor's LockData flag.
        let mut item = ferrofin_db::entities::base_items::BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: crate::item_type_lookup::stored_type_name(BaseItemKind::Movie)
                .unwrap()
                .to_owned(),
            name: Some("My Custom Title".into()),
            overview: Some("my notes".into()),
            production_year: Some(1999),
            is_locked: true,
            run_time_ticks: Some(100),
            ..Default::default()
        };
        svc.save_items(std::slice::from_ref(&item))
            .await
            .expect("editor save");

        // The next scan rebuilds the row from disk: filename-derived name, TMDB
        // overview/year, fresh probe runtime, and is_locked=false (the scanned
        // entity knows nothing of the lock).
        item.name = Some("Movie.Title.2010.1080p".into());
        item.overview = Some("tmdb overview".into());
        item.production_year = Some(2010);
        item.is_locked = false;
        item.run_time_ticks = Some(4242);
        svc.save_scanned_items(std::slice::from_ref(&item))
            .await
            .expect("scan save");

        let (name, overview, year, locked, ticks): (
            Option<String>,
            Option<String>,
            Option<i64>,
            bool,
            Option<i64>,
        ) = sqlx::query_as(
            r#"SELECT "Name", "Overview", "ProductionYear", "IsLocked", "RunTimeTicks"
               FROM "BaseItems" WHERE "Id" = ?1"#,
        )
        .bind(ferrofin_db::store::guid_to_db(id))
        .fetch_one(db.pool())
        .await
        .expect("row");
        assert_eq!(name.as_deref(), Some("My Custom Title"), "locked Name kept");
        assert_eq!(
            overview.as_deref(),
            Some("my notes"),
            "locked Overview kept"
        );
        assert_eq!(year, Some(1999), "locked ProductionYear kept");
        assert!(locked, "scan must never clear the lock");
        assert_eq!(ticks, Some(4242), "file-derived RunTimeTicks still updates");

        // Unlocked rows keep scan ownership: same save on a fresh unlocked row
        // takes the scanned values.
        let id2 = Uuid::new_v4();
        item.id = ferrofin_db::store::guid_to_db(id2);
        svc.save_scanned_items(std::slice::from_ref(&item))
            .await
            .expect("scan save unlocked");
        item.name = Some("Renamed.File.2011".into());
        svc.save_scanned_items(std::slice::from_ref(&item))
            .await
            .expect("rescan unlocked");
        let name: Option<String> =
            sqlx::query_scalar(r#"SELECT "Name" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id2))
                .fetch_one(db.pool())
                .await
                .expect("row");
        assert_eq!(name.as_deref(), Some("Renamed.File.2011"));
    }

    /// A key written with Rust's full lowercase (`ΟΣ` → `ος`, final-sigma
    /// context) is rewritten with .NET's simple invariant casing, and the
    /// 12.0 punctuation rule applies in the same pass.
    #[tokio::test]
    async fn the_clean_value_repair_uses_invariant_casing_and_the_12_0_form() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        crate::test_support::seed_named_item(&db, id, BaseItemKind::Person, "ΟΣ.").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "CleanName" = 'ος' WHERE "Id" = ?1"#)
            .bind(ferrofin_db::store::guid_to_db(id))
            .execute(db.writer())
            .await
            .unwrap();
        let service = FerrofinItemPersistenceService::new(db.clone());
        // Two rows: this one and the detached-UserData placeholder item `0032`
        // seeds with no `CleanName`.
        assert_eq!(service.repair_clean_values().await.unwrap(), 2);
        let clean: String =
            sqlx::query_scalar(r#"SELECT "CleanName" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .unwrap();
        // 12.0's rule: simple invariant lowercase (no final-sigma context), then
        // punctuation to space and trim — so the dot goes too.
        assert_eq!(clean, "οσ");
    }

    /// The port of 12.0's `RefreshCleanNamesAndValues`: a database written
    /// under 10.11.8's rule (fold + lower-case, punctuation kept) is moved to
    /// the 12.0 form once, names and values alike, and a second boot does
    /// nothing. The 10.11.8-era marker must not suppress the pass.
    #[tokio::test]
    async fn the_clean_value_repair_rewrites_10_11_8_columns_once() {
        let db = test_db().await;
        let service = FerrofinItemPersistenceService::new(db.clone());
        // The marker the previous repair (which kept punctuation) left behind.
        db.meta_set("clean_values_keep_punctuation_v1", "1")
            .await
            .expect("old marker");

        let person = Uuid::from_u128(0xC1EA);
        crate::test_support::seed_named_item(&db, person, BaseItemKind::Person, "H. Jon Benjamin")
            .await;
        sqlx::query(r#"UPDATE "BaseItems" SET "CleanName" = 'h. jon benjamin' WHERE "Id" = ?1"#)
            .bind(guid_to_db(person))
            .execute(db.writer())
            .await
            .expect("10.11.8 clean name");
        let movie = Uuid::from_u128(0xC1EB);
        crate::test_support::seed_named_item(&db, movie, BaseItemKind::Movie, "Dune: Part Two")
            .await;
        sqlx::query(r#"UPDATE "BaseItems" SET "CleanName" = 'dune: part two' WHERE "Id" = ?1"#)
            .bind(guid_to_db(movie))
            .execute(db.writer())
            .await
            .expect("10.11.8 clean name");
        // Already in the 12.0 form: read, compared, not counted.
        let settled = Uuid::from_u128(0xC1EC);
        crate::test_support::seed_named_item(&db, settled, BaseItemKind::Movie, "Heat").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "CleanName" = 'heat' WHERE "Id" = ?1"#)
            .bind(guid_to_db(settled))
            .execute(db.writer())
            .await
            .expect("settled clean name");
        sqlx::query(
            r#"INSERT INTO "ItemValues" ("ItemValueId","Type","Value","CleanValue") VALUES
               ('v1', 3, 'Warner Bros. Pictures', 'warner bros. pictures'),
               ('v2', 2, 'Mötley Crüe', 'motley crue'),
               ('v3', 1, 'Sci-Fi', 'sci-fi')"#,
        )
        .execute(db.writer())
        .await
        .expect("10.11.8 clean values");

        // 2 stale names + 2 stale values, plus migration 0001's placeholder
        // row: it has a `Name` and a NULL `CleanName`, and upstream's
        // `Where(!IsNullOrEmpty(Name))` takes it along. The settled rows are
        // read, compared and left alone.
        assert_eq!(service.repair_clean_values().await.expect("repair"), 5);
        let clean = |id: Uuid| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, Option<String>>(
                    r#"SELECT "CleanName" FROM "BaseItems" WHERE "Id" = ?1"#,
                )
                .bind(guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("read back")
            }
        };
        assert_eq!(clean(person).await.as_deref(), Some("h jon benjamin"));
        assert_eq!(clean(movie).await.as_deref(), Some("dune part two"));
        assert_eq!(clean(settled).await.as_deref(), Some("heat"));
        assert_eq!(
            clean(Uuid::from_u128(1)).await.as_deref(),
            Some(
                "this is a placeholder item for userdata that has been detached from its original item"
            )
        );
        let values: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT "ItemValueId", "CleanValue" FROM "ItemValues" ORDER BY "ItemValueId""#,
        )
        .fetch_all(db.pool())
        .await
        .expect("read back");
        assert_eq!(
            values,
            vec![
                ("v1".to_owned(), "warner bros pictures".to_owned()),
                ("v2".to_owned(), "motley crue".to_owned()),
                ("v3".to_owned(), "sci fi".to_owned()),
            ]
        );

        // Once only: the marker means a second boot does no work — even for a
        // row that drifted after the pass.
        sqlx::query(r#"UPDATE "BaseItems" SET "CleanName" = 'h. jon benjamin' WHERE "Id" = ?1"#)
            .bind(guid_to_db(person))
            .execute(db.writer())
            .await
            .expect("drift");
        assert_eq!(service.repair_clean_values().await.expect("repair"), 0);
        assert_eq!(clean(person).await.as_deref(), Some("h. jon benjamin"));
        assert_eq!(
            db.meta_get("clean_values_v12")
                .await
                .expect("meta")
                .as_deref(),
            Some("1")
        );
    }

    /// The port of 12.0's `RefreshForcedSortNames`: every row with a forced
    /// sort name gets `GetSortName(ForcedSortName, Type != Person)`, written
    /// only when it changed; rows without a forced name are never touched; the
    /// pass runs once.
    #[tokio::test]
    async fn the_forced_sort_name_repair_recomputes_12_0_keys_once() {
        async fn force(db: &ferrofin_db::Database, id: Uuid, forced: &str, sort: &str) {
            sqlx::query(
                r#"UPDATE "BaseItems" SET "ForcedSortName" = ?2, "SortName" = ?3 WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .bind(forced)
            .bind(sort)
            .execute(db.writer())
            .await
            .expect("forced row");
        }
        async fn sort_name(db: &ferrofin_db::Database, id: Uuid) -> Option<String> {
            sqlx::query_scalar(r#"SELECT "SortName" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("read back")
        }

        let db = test_db().await;
        let service = FerrofinItemPersistenceService::new(db.clone());
        // 10.11.8 stored `ModifySortChunks(forced).ToLowerInvariant()`.
        let movie = Uuid::from_u128(0xF0C1);
        crate::test_support::seed_named_item(&db, movie, BaseItemKind::Movie, "zzz unrelated")
            .await;
        force(
            &db,
            movie,
            "The Spider-Man: Homecoming",
            "the spider-man: homecoming",
        )
        .await;
        let sequel = Uuid::from_u128(0xF0C2);
        crate::test_support::seed_named_item(&db, sequel, BaseItemKind::Series, "Matrix").await;
        force(&db, sequel, "The Matrix 2", "the matrix 0000000002").await;
        // A person keeps the override verbatim, trimmed at the start only.
        let person = Uuid::from_u128(0xF0C3);
        crate::test_support::seed_named_item(&db, person, BaseItemKind::Person, "Alice Parity")
            .await;
        force(&db, person, "  Parity, Alice", "parity, alice").await;
        // Already the 12.0 key: read, compared, not counted.
        let settled = Uuid::from_u128(0xF0C4);
        crate::test_support::seed_named_item(&db, settled, BaseItemKind::Movie, "Heat").await;
        force(&db, settled, "Heat 2", "heat 0000000002").await;
        // No override: never part of the pass, whatever its SortName holds.
        let plain = Uuid::from_u128(0xF0C5);
        crate::test_support::seed_named_item(&db, plain, BaseItemKind::Movie, "The Plain").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "SortName" = 'left alone' WHERE "Id" = ?1"#)
            .bind(guid_to_db(plain))
            .execute(db.writer())
            .await
            .expect("plain row");

        assert_eq!(service.repair_forced_sort_names().await.expect("repair"), 3);
        assert_eq!(
            sort_name(&db, movie).await.as_deref(),
            Some("spiderman: homecoming")
        );
        assert_eq!(
            sort_name(&db, sequel).await.as_deref(),
            Some("matrix 0000000002")
        );
        assert_eq!(
            sort_name(&db, person).await.as_deref(),
            Some("Parity, Alice")
        );
        assert_eq!(
            sort_name(&db, settled).await.as_deref(),
            Some("heat 0000000002")
        );
        assert_eq!(sort_name(&db, plain).await.as_deref(), Some("left alone"));

        // Second boot: nothing to do.
        assert_eq!(service.repair_forced_sort_names().await.expect("repair"), 0);
        assert_eq!(
            db.meta_get("forced_sort_names_v12")
                .await
                .expect("meta")
                .as_deref(),
            Some("1")
        );
    }

    /// A stored key that the per-kind rule cannot reproduce is never
    /// overwritten with a guess.
    ///
    /// Two cases the verification library cannot show, because it has neither:
    /// a `Series` on a server with `EnableAutomaticSeriesGrouping` on (upstream
    /// default TRUE, and its key then derives from provider ids + language +
    /// library folders, none of which is ported), and a `Season` whose
    /// `SeriesPresentationUniqueKey` is missing. Overwriting either with the
    /// row's own id orphans every season that points at it.
    #[tokio::test]
    async fn a_key_the_rule_cannot_reproduce_survives_a_save() {
        async fn key(db: &ferrofin_db::Database, id: Uuid) -> Option<String> {
            crate::test_support::fetch_item(db, id)
                .await
                .presentation_unique_key
        }
        let db = test_db().await;
        let service = FerrofinItemPersistenceService::new(db.clone());
        let series = Uuid::from_u128(0x5E01);
        let season = Uuid::from_u128(0x5E02);
        let movie = Uuid::from_u128(0x5E03);
        crate::test_support::seed_named_item(&db, series, BaseItemKind::Series, "Breaking Bad")
            .await;
        crate::test_support::seed_named_item(&db, season, BaseItemKind::Season, "Season 2").await;
        crate::test_support::seed_named_item(&db, movie, BaseItemKind::Movie, "Heat").await;
        for id in [series, season, movie] {
            super::seed_presentation_key(&db, id, "grouped-key-from-jellyfin").await;
        }

        for id in [series, season, movie] {
            let row = crate::test_support::fetch_item(&db, id).await;
            service
                .save_items(std::slice::from_ref(&row))
                .await
                .expect("save");
        }

        assert_eq!(
            key(&db, series).await.as_deref(),
            Some("grouped-key-from-jellyfin"),
            "a series keeps the key the server that wrote it derived"
        );
        assert_eq!(
            key(&db, season).await.as_deref(),
            Some("grouped-key-from-jellyfin"),
            "…and so does a season with no series key to rebuild from"
        );
        assert_eq!(
            key(&db, movie).await.as_deref(),
            Some("00000000000000000000000000005e03"),
            "but a movie's key IS reproducible, so it is recomputed"
        );
    }

    /// 12.0 keeps a version group in `LinkedChildren` as well as in
    /// `PrimaryVersionId`; the pointer and the row move together.
    #[tokio::test]
    async fn set_primary_version_id_writes_and_clears_the_version_link() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let (alt, primary) = (Uuid::new_v4(), Uuid::new_v4());
        seed_item(&db, alt, BaseItemKind::Movie).await;
        seed_item(&db, primary, BaseItemKind::Movie).await;
        for (id, path) in [(alt, "/m/a/x.mkv"), (primary, "/m/b/x.mkv")] {
            sqlx::query(r#"UPDATE "BaseItems" SET "Path" = ?2 WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .bind(path)
                .execute(db.writer())
                .await
                .expect("path");
        }
        svc.set_primary_version_id(alt, Some(primary))
            .await
            .expect("link");
        let links: Vec<(String, String, i64)> =
            sqlx::query_as(r#"SELECT "ParentId", "ChildId", "ChildType" FROM "LinkedChildren""#)
                .fetch_all(db.pool())
                .await
                .expect("links");
        assert_eq!(
            links,
            vec![(
                ferrofin_db::store::guid_to_db(primary),
                ferrofin_db::store::guid_to_db(alt),
                3
            )],
            "different directories → LinkedAlternateVersion"
        );
        svc.set_primary_version_id(alt, None).await.expect("unlink");
        let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "LinkedChildren""#)
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(count, 0);
    }

    /// `OwnerId` is a foreign key on the 12.0 shape: an item's extras are
    /// deleted with it (upstream `DeleteItem` includes `GetExtras()`), never
    /// left to trip the constraint.
    #[tokio::test]
    async fn delete_items_removes_owned_extras_first() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let (movie, extra) = (Uuid::new_v4(), Uuid::new_v4());
        seed_item(&db, movie, BaseItemKind::Movie).await;
        seed_item(&db, extra, BaseItemKind::Trailer).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "OwnerId" = ?2, "ExtraType" = 1 WHERE "Id" = ?1"#)
            .bind(ferrofin_db::store::guid_to_db(extra))
            .bind(ferrofin_db::store::guid_to_db(movie))
            .execute(db.writer())
            .await
            .expect("own");
        svc.delete_items(&[movie]).await.expect("delete owner");
        let left: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Id" IN (?1, ?2)"#)
                .bind(ferrofin_db::store::guid_to_db(movie))
                .bind(ferrofin_db::store::guid_to_db(extra))
                .fetch_one(db.pool())
                .await
                .expect("count");
        assert_eq!(left, 0);
    }

    /// The `rating_levels_v12` repair (Jellyfin 12.0 `MigrateRatingLevels`):
    /// every row's inherited rating columns are recomputed from its OWN
    /// `OfficialRating` through `GetRatingScore` — a bare age, a US table
    /// entry with a sub-score, a "Rated R" spelling — and cleared when the
    /// rating is blank or resolves to nothing. It runs once per database.
    #[tokio::test]
    async fn repair_rating_levels_writes_scores_from_each_rows_own_rating() {
        use crate::test_support::{fetch_item, save_item, seed_named_item};
        // (item id, OfficialRating, expected Score, expected SubScore)
        type RatingCase = (u128, Option<&'static str>, Option<i64>, Option<i64>);
        let db = test_db().await;
        let service = FerrofinItemPersistenceService::new(db.clone());
        let localization = crate::localization_manager::LocalizationManager::new("US");
        let cases: [RatingCase; 5] = [
            (0x7A01, Some("12"), Some(12), None),
            (0x7A02, Some("TV-MA"), Some(17), Some(1)),
            (0x7A03, Some("Rated R"), Some(17), Some(0)),
            (0x7A04, Some("unknown-junk"), None, None),
            (0x7A05, None, None, None),
        ];
        for (id, rating, _, _) in cases {
            let id = Uuid::from_u128(id);
            seed_named_item(&db, id, BaseItemKind::Movie, "Film").await;
            let mut row = fetch_item(&db, id).await;
            row.official_rating = rating.map(str::to_owned);
            // Stale values a pre-12.0 build (or a parent walk) left behind.
            row.inherited_parental_rating_value = Some(99);
            row.inherited_parental_rating_sub_value = Some(99);
            save_item(&db, &row).await;
        }
        let unrated_rows: i64 = sqlx::query_scalar(
            r#"SELECT count(*) FROM "BaseItems"
               WHERE "OfficialRating" IS NULL OR "OfficialRating" = ''"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("count");

        let updated = service
            .repair_rating_levels(&localization)
            .await
            .expect("repair");

        // Four rated rows plus every unrated one (upstream's blank-rating
        // update matches the migration placeholder too).
        assert_eq!(
            updated,
            4 + u64::try_from(unrated_rows).expect("count fits")
        );
        for (id, rating, score, sub_score) in cases {
            let row = fetch_item(&db, Uuid::from_u128(id)).await;
            assert_eq!(
                (
                    row.inherited_parental_rating_value,
                    row.inherited_parental_rating_sub_value
                ),
                (score, sub_score),
                "{rating:?}"
            );
        }
        assert_eq!(
            service
                .repair_rating_levels(&localization)
                .await
                .expect("second run"),
            0,
            "the repair runs once per database"
        );
    }

    /// `series_key_scope`: the language falls through row → library → server,
    /// the folders are every library sharing the series' parent directory
    /// (case-insensitively), and a series with no path falls back to its
    /// `TopParentId` library alone.
    #[test]
    fn series_key_scope_resolves_language_and_folders_like_the_library_manager() {
        use ferrofin_model::configuration::LibraryOptions;
        use ferrofin_model::entities_media::VirtualFolderInfo;
        let (a, b, c) = (
            Uuid::from_u128(0xA),
            Uuid::from_u128(0xB),
            Uuid::from_u128(0xC),
        );
        let folder = |id: Uuid, location: &str, options: LibraryOptions| VirtualFolderInfo {
            item_id: Some(id.to_string()),
            locations: vec![location.to_owned()],
            library_options: Some(options),
            ..VirtualFolderInfo::default()
        };
        let folders = [
            folder(a, "/media/TV/", LibraryOptions::default()),
            folder(
                b,
                "/media/tv",
                LibraryOptions {
                    preferred_metadata_language: Some("de".to_owned()),
                    enable_automatic_series_grouping: false,
                    ..LibraryOptions::default()
                },
            ),
            folder(c, "/media/anime", LibraryOptions::default()),
        ];
        let scope = |top: Uuid, path: Option<&str>, own: Option<&str>| {
            super::series_key_scope(&folders, "en", Some(&guid_to_db(top)), path, own)
        };

        let shared = scope(a, Some("/media/tv/Show"), None);
        assert_eq!(shared.collection_folder_ids, vec![a, b]);
        assert!(
            shared.enable_automatic_series_grouping,
            "library A's options"
        );
        assert_eq!(shared.preferred_metadata_language.as_deref(), Some("en"));

        let german = scope(b, Some("/media/tv/Show"), None);
        assert!(
            !german.enable_automatic_series_grouping,
            "library B's options"
        );
        assert_eq!(german.preferred_metadata_language.as_deref(), Some("de"));
        assert_eq!(
            scope(b, Some("/media/tv/Show"), Some("fr"))
                .preferred_metadata_language
                .as_deref(),
            Some("fr"),
            "the row's own language wins"
        );

        let pathless = scope(c, None, None);
        assert_eq!(pathless.collection_folder_ids, vec![c]);
        assert_eq!(
            scope(c, Some("/elsewhere/Show"), None).collection_folder_ids,
            vec![c],
            "under no library: the TopParentId library"
        );
    }

    /// The `series_presentation_keys_v12` repair (Jellyfin 12.0
    /// `RecomputeSeriesPresentationKey`) over a seeded series tree: a series
    /// with a provider id in a shared location gets the id + language + both
    /// folders (ordered), one without gets the `series-{name}` fallback, one in
    /// a grouping-off library keeps its own id; every child is re-pointed by
    /// `SeriesId`, indexed seasons get `{key}-{index:000}` (even under an
    /// unchanged series key), an unindexed season keeps its own key. Runs once.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one seeded tree, asserted from every angle
    async fn repair_series_presentation_keys_recomputes_the_tree() {
        use crate::test_support::{
            fetch_item, save_item, seed_item_of_series, seed_provider_id, seed_top_parented_item,
            set_item_path,
        };
        use ferrofin_model::configuration::LibraryOptions;
        use ferrofin_model::entities_media::VirtualFolderInfo;
        async fn key(db: &ferrofin_db::Database, id: Uuid) -> Option<String> {
            fetch_item(db, id).await.presentation_unique_key
        }
        async fn series_key(db: &ferrofin_db::Database, id: Uuid) -> Option<String> {
            fetch_item(db, id).await.series_presentation_unique_key
        }
        let db = test_db().await;
        let service = FerrofinItemPersistenceService::new(db.clone());
        let (lib_a, lib_b, lib_c) = (
            Uuid::from_u128(0xA),
            Uuid::from_u128(0xB),
            Uuid::from_u128(0xC),
        );
        let folder = |id: Uuid, location: &str, grouping: bool| VirtualFolderInfo {
            item_id: Some(id.to_string()),
            locations: vec![location.to_owned()],
            library_options: Some(LibraryOptions {
                enable_automatic_series_grouping: grouping,
                ..LibraryOptions::default()
            }),
            ..VirtualFolderInfo::default()
        };
        let folders = [
            folder(lib_a, "/media/tv", true),
            folder(lib_b, "/media/tv", true),
            folder(lib_c, "/media/anime", false),
        ];
        let (grouped, named, ungrouped) = (
            Uuid::from_u128(0x5E01),
            Uuid::from_u128(0x5E02),
            Uuid::from_u128(0x5E03),
        );
        for (id, name, lib, path) in [
            (grouped, "Breaking Bad", lib_a, "/media/tv/Breaking Bad"),
            (named, "The Office", lib_a, "/media/tv/The Office"),
            (
                ungrouped,
                "Cowboy Bebop",
                lib_c,
                "/media/anime/Cowboy Bebop",
            ),
        ] {
            seed_top_parented_item(&db, id, BaseItemKind::Series, name, lib).await;
            set_item_path(&db, id, path).await;
        }
        seed_provider_id(&db, grouped, "Tvdb", "81189").await;
        // Children: two indexed seasons + a specials season + an episode under
        // the grouped series, one indexed season under the ungrouped one.
        let (s1, s2, specials, ep, s3) = (
            Uuid::from_u128(0x5A01),
            Uuid::from_u128(0x5A02),
            Uuid::from_u128(0x5A03),
            Uuid::from_u128(0x5A04),
            Uuid::from_u128(0x5A05),
        );
        for (id, kind, name, series, index) in [
            (s1, BaseItemKind::Season, "Season 1", grouped, Some(1)),
            (s2, BaseItemKind::Season, "Season 2", grouped, Some(2)),
            (specials, BaseItemKind::Season, "Specials", grouped, None),
            (ep, BaseItemKind::Episode, "Pilot", grouped, Some(1)),
            (s3, BaseItemKind::Season, "Session 1", ungrouped, Some(1)),
        ] {
            seed_item_of_series(&db, id, kind, name, series).await;
            let mut row = fetch_item(&db, id).await;
            row.index_number = index;
            row.series_presentation_unique_key = Some("stale".to_owned());
            save_item(&db, &row).await;
        }
        // The pre-12.0 state: every series keyed on its own id.
        for id in [grouped, named, ungrouped] {
            super::seed_presentation_key(&db, id, &id.as_simple().to_string()).await;
        }

        let updated = service
            .repair_series_presentation_keys(&folders, "en")
            .await
            .expect("repair");

        let grouped_key = format!("81189-en-{}-{}", lib_a.as_simple(), lib_b.as_simple());
        assert_eq!(
            key(&db, grouped).await.as_deref(),
            Some(grouped_key.as_str())
        );
        assert_eq!(
            key(&db, named).await.as_deref(),
            Some(
                format!(
                    "series-the office-en-{}-{}",
                    lib_a.as_simple(),
                    lib_b.as_simple()
                )
                .as_str()
            )
        );
        let own = ungrouped.as_simple().to_string();
        assert_eq!(key(&db, ungrouped).await.as_deref(), Some(own.as_str()));
        for child in [s1, s2, specials, ep] {
            assert_eq!(
                series_key(&db, child).await.as_deref(),
                Some(grouped_key.as_str())
            );
        }
        assert_eq!(
            key(&db, s1).await.as_deref(),
            Some(format!("{grouped_key}-001").as_str())
        );
        assert_eq!(
            key(&db, s2).await.as_deref(),
            Some(format!("{grouped_key}-002").as_str())
        );
        assert_eq!(
            key(&db, specials).await.as_deref(),
            Some(specials.as_simple().to_string().as_str()),
            "a season without an index number keeps the base key"
        );
        assert_eq!(
            series_key(&db, s3).await.as_deref(),
            Some("stale"),
            "an unchanged series re-points nothing by SeriesId"
        );
        assert_eq!(
            key(&db, s3).await.as_deref(),
            Some(format!("{own}-001").as_str()),
            "…but its indexed season is still recomputed"
        );
        // grouped + named series rows, four children re-pointed, three seasons.
        assert_eq!(updated, 2 + 4 + 3);
        assert_eq!(
            service
                .repair_series_presentation_keys(&folders, "en")
                .await
                .expect("second run"),
            0,
            "the repair runs once per database"
        );
    }

    /// `repoint_series_children` is the scan-time half of the same rule: rows
    /// matched by `SeriesId` take the key, indexed seasons rebuild their own,
    /// another series' children are untouched, and a repeat is a no-op.
    #[tokio::test]
    async fn repoint_series_children_follows_series_id_not_the_old_key() {
        use crate::test_support::{fetch_item, save_item, seed_item_of_series, seed_named_item};
        let db = test_db().await;
        let service = FerrofinItemPersistenceService::new(db.clone());
        let (series, other) = (Uuid::from_u128(0x5E11), Uuid::from_u128(0x5E12));
        seed_named_item(&db, series, BaseItemKind::Series, "Show").await;
        seed_named_item(&db, other, BaseItemKind::Series, "Other").await;
        let (season, episode, other_season) = (
            Uuid::from_u128(0x5A11),
            Uuid::from_u128(0x5A12),
            Uuid::from_u128(0x5A13),
        );
        for (id, kind, owner) in [
            (season, BaseItemKind::Season, series),
            (episode, BaseItemKind::Episode, series),
            (other_season, BaseItemKind::Season, other),
        ] {
            seed_item_of_series(&db, id, kind, "child", owner).await;
            let mut row = fetch_item(&db, id).await;
            row.index_number = Some(3);
            // Both series' children share the OLD key — the case that made
            // upstream scope by SeriesId.
            row.series_presentation_unique_key = Some("shared-old-key".to_owned());
            save_item(&db, &row).await;
        }

        let updated = service
            .repoint_series_children(series, "81189-en-lib")
            .await
            .expect("repoint");

        assert_eq!(updated, 3, "two children re-pointed, one season rekeyed");
        for id in [season, episode] {
            assert_eq!(
                fetch_item(&db, id)
                    .await
                    .series_presentation_unique_key
                    .as_deref(),
                Some("81189-en-lib")
            );
        }
        assert_eq!(
            fetch_item(&db, season)
                .await
                .presentation_unique_key
                .as_deref(),
            Some("81189-en-lib-003")
        );
        assert_eq!(
            fetch_item(&db, other_season)
                .await
                .series_presentation_unique_key
                .as_deref(),
            Some("shared-old-key"),
            "the other series' child keeps the shared old key"
        );
        assert_eq!(
            service
                .repoint_series_children(series, "81189-en-lib")
                .await
                .expect("repeat"),
            0
        );
    }

    /// The rows [`folder_aggregates_follow_the_ancestor_closure`] seeds: a
    /// music artist over an album over three tracks (one virtual, one with no
    /// runtime) plus a disc folder, and a series over a season over two
    /// episodes (one virtual, newer than the real one).
    async fn seed_folder_tree(
        db: &ferrofin_db::Database,
    ) -> (Uuid, Uuid, Uuid, chrono::DateTime<chrono::Utc>) {
        use chrono::TimeZone as _;
        use ferrofin_db::entities::base_items::BaseItemEntity;
        let service = FerrofinItemPersistenceService::new(db.clone());
        let (artist, album, disc) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (series, season) = (Uuid::new_v4(), Uuid::new_v4());
        let at = |d| chrono::Utc.with_ymd_and_hms(2024, 1, d, 0, 0, 0).unwrap();
        let row = |id: Uuid, kind, folder: bool| BaseItemEntity {
            id: guid_to_db(id),
            type_: stored_type_name(kind).unwrap().to_owned(),
            is_folder: folder,
            ..BaseItemEntity::default()
        };
        let mut rows = vec![
            row(artist, BaseItemKind::MusicArtist, true),
            BaseItemEntity {
                run_time_ticks: Some(7),
                ..row(album, BaseItemKind::MusicAlbum, true)
            },
            row(disc, BaseItemKind::Folder, true),
            row(series, BaseItemKind::Series, true),
            row(season, BaseItemKind::Season, true),
        ];
        let mut tracks = Vec::new();
        for (ticks, virtual_item) in [(Some(10), false), (None, false), (Some(5), true)] {
            let id = Uuid::new_v4();
            tracks.push(id);
            rows.push(BaseItemEntity {
                run_time_ticks: ticks,
                is_virtual_item: virtual_item,
                ..row(id, BaseItemKind::Audio, false)
            });
        }
        let mut episodes = Vec::new();
        for (day, virtual_item) in [(3, false), (9, true)] {
            let id = Uuid::new_v4();
            episodes.push(id);
            rows.push(BaseItemEntity {
                date_created: Some(at(day)),
                is_virtual_item: virtual_item,
                ..row(id, BaseItemKind::Episode, false)
            });
        }
        service.save_items(&rows).await.expect("seed");
        service.set_ancestors(album, &[artist]).await.unwrap();
        service.set_ancestors(disc, &[album, artist]).await.unwrap();
        for track in tracks {
            service
                .set_ancestors(track, &[disc, album, artist])
                .await
                .unwrap();
        }
        service.set_ancestors(season, &[series]).await.unwrap();
        for episode in episodes {
            service
                .set_ancestors(episode, &[season, series])
                .await
                .unwrap();
        }
        (artist, album, series, at(3))
    }

    /// `UpdateCumulativeRunTimeTicks` / `UpdateDateLastMediaAdded`
    /// (`MetadataService.cs:485-541`) as one aggregate read each: the runtime
    /// sums every non-folder descendant (a missing runtime counts 0, a virtual
    /// track counts), the last-media date takes the latest `DateCreated` of the
    /// non-folder, NON-VIRTUAL descendants, a folder with neither answers 0 /
    /// `None`, and an unknown id is absent. The targeted writes change only
    /// their own column, and only when it moved.
    #[tokio::test]
    async fn folder_aggregates_follow_the_ancestor_closure() {
        let db = test_db().await;
        let service = FerrofinItemPersistenceService::new(db.clone());
        let (artist, album, series, newest) = seed_folder_tree(&db).await;
        let unknown = Uuid::new_v4();

        let sums = service
            .folder_run_time_sums(
                &[artist, album, series, unknown],
                &[BaseItemKind::Audio, BaseItemKind::AudioBook],
            )
            .await
            .expect("sums");
        assert_eq!(sums[&artist].aggregate, Some(15));
        assert_eq!(sums[&artist].stored, None);
        assert_eq!(sums[&album].aggregate, Some(15));
        assert_eq!(sums[&album].stored, Some(7));
        assert_eq!(
            sums[&series].aggregate,
            Some(0),
            "no runtime under the series"
        );
        assert!(!sums.contains_key(&unknown));

        let added = service
            .folder_last_media_added(&[series, artist, unknown])
            .await
            .expect("dates");
        assert_eq!(
            added[&series].aggregate,
            Some(newest),
            "the virtual episode is skipped"
        );
        assert_eq!(added[&series].stored, None);
        assert_eq!(
            added[&artist].aggregate, None,
            "tracks carry no DateCreated"
        );
        assert!(!added.contains_key(&unknown));

        assert!(service.update_run_time_ticks(album, 15).await.unwrap());
        assert!(
            !service.update_run_time_ticks(album, 15).await.unwrap(),
            "unchanged"
        );
        assert!(
            service
                .update_date_last_media_added(series, Some(newest))
                .await
                .unwrap()
        );
        assert!(
            !service
                .update_date_last_media_added(series, Some(newest))
                .await
                .unwrap()
        );
        let row = crate::test_support::fetch_item(&db, series).await;
        assert_eq!(row.date_last_media_added, Some(newest));
        assert!(
            row.date_last_saved.is_some(),
            "a write that moves the value stamps DateLastSaved, as upstream's save does"
        );
        let saved = row.date_last_saved;
        assert!(
            !service
                .update_date_last_media_added(series, Some(newest))
                .await
                .unwrap()
        );
        assert_eq!(
            crate::test_support::fetch_item(&db, series)
                .await
                .date_last_saved,
            saved,
            "an unmoved value writes nothing"
        );
        assert!(
            crate::test_support::fetch_item(&db, album)
                .await
                .date_last_saved
                .is_some(),
            "the runtime write stamps it too"
        );

        let at = newest + chrono::TimeDelta::days(1);
        assert_eq!(
            service
                .stamp_date_last_refreshed(&[artist, album, unknown], at)
                .await
                .unwrap(),
            2
        );
        let row = crate::test_support::fetch_item(&db, artist).await;
        assert_eq!(row.date_last_refreshed, Some(at));
        assert_eq!(
            row.date_last_saved,
            Some(at),
            "the refresh's save: upstream saves a first refresh"
        );
    }

    /// `EXPLAIN QUERY PLAN` for `sql` with `binds` parameters, one `detail`
    /// per step, outer to inner.
    async fn query_plan(db: &ferrofin_db::Database, sql: &str, binds: usize) -> Vec<String> {
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let mut query =
            sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(explain.as_str()));
        for _ in 0..binds {
            query = query.bind("x");
        }
        query
            .fetch_all(db.pool())
            .await
            .expect("explain query plan")
            .into_iter()
            .map(|(_, _, _, detail)| detail)
            .collect()
    }

    #[tokio::test]
    async fn owned_extra_pruning_seeks_only_its_scope() {
        let db = test_db().await;
        for roots in [0, 1, 3] {
            let plan =
                query_plan(&db, &super::owned_extra_items_sql(2, roots), 2 + 3 * roots).await;
            let (owner_key, extra_key) = if roots == 0 {
                ("TopParentId=?", "OwnerId=?")
            } else {
                ("Id=?", "IX_BaseItems_Path")
            };
            assert!(
                plan.iter()
                    .any(|s| s.contains("SEARCH p") && s.contains(owner_key)),
                "{plan:?}"
            );
            assert!(
                plan.iter()
                    .any(|s| s.contains("SEARCH e") && s.contains(extra_key)),
                "{plan:?}"
            );
            assert!(!plan.iter().any(|s| s.starts_with("SCAN")), "{plan:?}");
        }
    }

    /// Both folder aggregates must reach each folder by its primary key and
    /// sum its descendants from the ANCESTOR CLOSURE — `AncestorIds` sought by
    /// `ParentItemId`, each child by its id — never from a `BaseItems` index
    /// (`IsFolder`, `TopParentId`, …), which would walk every item in the
    /// library per folder. The `CROSS JOIN` in the statements pins it.
    #[tokio::test]
    async fn folder_aggregates_seek_from_the_ancestor_closure() {
        let db = test_db().await;
        for (sql, binds) in [
            (super::folder_run_time_sums_sql(3, 2), 5),
            (super::folder_last_media_added_sql(3), 3),
        ] {
            let plan = query_plan(&db, &sql, binds).await;
            assert!(
                plan.iter().any(|s| s.starts_with("SEARCH f USING")
                    && s.contains("sqlite_autoindex_BaseItems_1")
                    && s.contains("Id=?")),
                "the folders by primary key, got: {plan:?}"
            );
            let a = plan
                .iter()
                .position(|s| {
                    s.starts_with("SEARCH a USING") && s.contains("IX_AncestorIds_ParentItemId")
                })
                .unwrap_or_else(|| panic!("AncestorIds by ParentItemId, got: {plan:?}"));
            let c = plan
                .iter()
                .position(|s| {
                    s.starts_with("SEARCH c USING")
                        && s.contains("sqlite_autoindex_BaseItems_1")
                        && s.contains("Id=?")
                })
                .unwrap_or_else(|| panic!("each child by its id, got: {plan:?}"));
            assert!(a < c, "the closure drives the children, got: {plan:?}");
            assert!(
                !plan.iter().any(|s| s.starts_with("SCAN")),
                "no scan of any table, got: {plan:?}"
            );
        }
    }

    /// The selections every library validation's closing passes run
    /// (never-refreshed by-name artists and studios, superseded by-name
    /// artists) seek `BaseItems` by `Type` — never a scan of the table,
    /// which on an unchanged rescan of a large library would be the pass's
    /// whole cost.
    #[tokio::test]
    async fn the_closing_pass_selections_seek_by_type() {
        let db = test_db().await;
        for (sql, binds) in [
            (super::never_refreshed_ids_sql(true), 1),
            (super::never_refreshed_ids_sql(false), 1),
            (super::SUPERSEDED_BY_NAME_ARTISTS_SQL, 1),
        ] {
            let plan = query_plan(&db, sql, binds).await;
            assert!(
                !plan.iter().any(|s| s.starts_with("SCAN")),
                "no scan of any table, got: {plan:?}"
            );
            assert!(
                plan.iter()
                    .filter(|s| s.starts_with("SEARCH"))
                    .all(|s| s.contains("(Type=?")),
                "every read sought by Type, got: {plan:?}"
            );
        }
        let plan = query_plan(&db, super::SUPERSEDED_BY_NAME_ARTISTS_SQL, 1).await;
        assert!(
            // SQLite 3.50+ words a correlated subquery's seek `SEARCH f EXISTS
            // USING …`; older versions `SEARCH f USING …`.
            plan.iter().any(|s| s.starts_with("SEARCH f ")
                && s.contains("IX_BaseItems_Type_CleanName")
                && s.contains("CleanName=?")),
            "the folder twin by Type and CleanName, got: {plan:?}"
        );
    }

    /// The two persisted selections over real rows: a never-refreshed row of
    /// the kind (in no library, when asked), and a by-name artist whose
    /// `CleanName` a library artist carries.
    #[tokio::test]
    async fn the_closing_pass_selections_pick_the_rows_they_name() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let library = Uuid::from_u128(0x10);
        let row = |id: u128, kind: BaseItemKind, name: &str, top: Option<Uuid>, refreshed: bool| {
            ferrofin_db::entities::base_items::BaseItemEntity {
                id: guid_to_db(Uuid::from_u128(id)),
                type_: stored_type_name(kind).unwrap_or_default().to_owned(),
                name: Some(name.to_owned()),
                clean_name: Some(crate::text_util::get_clean_value(name)),
                is_folder: true,
                top_parent_id: top.map(guid_to_db),
                date_last_refreshed: refreshed.then(chrono::Utc::now),
                ..Default::default()
            }
        };
        svc.save_items(&[
            row(0x10, BaseItemKind::CollectionFolder, "Music", None, true),
            row(
                0x21,
                BaseItemKind::MusicArtist,
                "Miles Davis",
                Some(library),
                false,
            ),
            row(0x22, BaseItemKind::MusicArtist, "Miles Davis", None, true),
            row(0x23, BaseItemKind::MusicArtist, "Gil Evans", None, false),
            row(0x31, BaseItemKind::Studio, "Blue Note", None, false),
            row(0x32, BaseItemKind::Studio, "Columbia", None, true),
        ])
        .await
        .expect("seed");
        assert_eq!(
            svc.never_refreshed_ids(BaseItemKind::MusicArtist, true)
                .await
                .unwrap(),
            vec![Uuid::from_u128(0x23)],
            "the by-name artist never refreshed, not the library one"
        );
        assert_eq!(
            svc.never_refreshed_ids(BaseItemKind::MusicArtist, false)
                .await
                .unwrap(),
            vec![Uuid::from_u128(0x21), Uuid::from_u128(0x23)]
        );
        assert_eq!(
            svc.never_refreshed_ids(BaseItemKind::Studio, false)
                .await
                .unwrap(),
            vec![Uuid::from_u128(0x31)]
        );
        assert_eq!(
            svc.superseded_by_name_artists().await.unwrap(),
            vec![Uuid::from_u128(0x22)],
            "only the by-name twin of a library artist"
        );
    }

    /// A folder with no media stores `DateLastMediaAdded` as `NULL` —
    /// upstream's `DateTime.MinValue`, which its mapper persists as `NULL`
    /// (`BaseItemMapper.cs:418`) — and a write of the same value is none.
    #[tokio::test]
    async fn an_unset_last_media_date_is_stored_as_null() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let series = Uuid::from_u128(0x51);
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        svc.save_items(&[ferrofin_db::entities::base_items::BaseItemEntity {
            id: guid_to_db(series),
            type_: stored_type_name(BaseItemKind::Series)
                .unwrap_or_default()
                .to_owned(),
            name: Some("Charlie".to_owned()),
            is_folder: true,
            date_last_media_added: Some(at),
            ..Default::default()
        }])
        .await
        .expect("seed");
        assert!(
            svc.update_date_last_media_added(series, None)
                .await
                .unwrap()
        );
        assert_eq!(
            crate::test_support::fetch_item(&db, series)
                .await
                .date_last_media_added,
            None
        );
        assert!(
            !svc.update_date_last_media_added(series, None)
                .await
                .unwrap()
        );
        assert!(
            svc.update_date_last_media_added(series, Some(at))
                .await
                .unwrap()
        );
        assert!(
            !svc.update_date_last_media_added(series, Some(at))
                .await
                .unwrap()
        );
    }

    /// The path-scoped scan's reads seek by path (and a path-less row by its
    /// parent) — never a walk of `BaseItems` or of the library's
    /// `TopParentId` rows: the library monitor's changed-path lookup
    /// (`FindByPath`), and the pruning of a watcher/webhook or folder scan,
    /// whose rows are the scanned roots' only. The same whether the library
    /// is read by its collection folder alone (a native library) or with
    /// the physical folders an adopted one's rows may still carry.
    #[tokio::test]
    async fn the_path_scoped_reads_seek_by_path() {
        let db = test_db().await;
        // An alternate version's primary is a primary-key probe of `p`
        // (`prune_candidate_terms`), never a walk.
        let primary_probe = |s: &String| {
            s.starts_with("SEARCH p ") && s.contains("sqlite_autoindex_BaseItems_1 (Id=?)")
        };
        // Up to the largest chunk each read binds: 166 roots (three binds
        // each, after the library's top parents), 500 paths and parent ids.
        for (tops, roots) in [(1, 1), (1, 3), (1, 166), (2, 1), (2, 3), (2, 166), (4, 165)] {
            let binds = tops + 3 * roots;
            let n = format!("{tops} top parents, {roots} roots");
            let plan = query_plan(&db, &super::items_under_roots_sql(tops, roots), binds).await;
            assert!(
                !plan.iter().any(|s| s.starts_with("SCAN")),
                "no scan (n={n}), got: {plan:?}"
            );
            let seeks: Vec<&String> = plan
                .iter()
                .filter(|s| s.starts_with("SEARCH") && !primary_probe(s))
                .collect();
            assert!(
                !seeks.is_empty()
                    && seeks.iter().all(|s| s.starts_with("SEARCH bi ")
                        && s.contains("IX_BaseItems_Path")
                        && (s.contains("Path=?") || s.contains("Path>? AND Path<?"))),
                "each root and range an IX_BaseItems_Path seek (n={n}), got: {plan:?}"
            );
            assert!(
                !plan.iter().any(|s| s.contains("TopParentId")),
                "never through a TopParentId index (n={n}), got: {plan:?}"
            );

            let plan = query_plan(&db, &super::pathless_children_sql(tops, roots), binds).await;
            assert!(
                !plan.iter().any(|s| s.starts_with("SCAN")),
                "no scan (n={n}), got: {plan:?}"
            );
            assert!(
                plan.iter().any(|s| s.starts_with("SEARCH")
                    && s.contains("IX_BaseItems_ParentId")
                    && s.contains("ParentId=?")),
                "the children by IX_BaseItems_ParentId (n={n}), got: {plan:?}"
            );
            assert!(
                plan.iter()
                    .filter(|s| s.starts_with("SEARCH") && !primary_probe(s))
                    .all(|s| s.contains("IX_BaseItems_ParentId")
                        || (s.contains("IX_BaseItems_Path")
                            && (s.contains("Path=?") || s.contains("Path>? AND Path<?")))),
                "the parents by the roots' IX_BaseItems_Path seeks (n={n}), got: {plan:?}"
            );
            assert!(
                !plan.iter().any(|s| s.contains("TEMP B-TREE")),
                "no sort (n={n}), got: {plan:?}"
            );
        }
        for n in [1, 3, 500] {
            let plan = query_plan(&db, &super::items_at_paths_sql(n), n).await;
            assert!(
                plan.iter()
                    .all(|s| s.starts_with("SEARCH") && s.contains("IX_BaseItems_Path")),
                "every path an IX_BaseItems_Path seek (n={n}), got: {plan:?}"
            );
            let plan = query_plan(&db, &super::child_links_sql(n), n).await;
            assert!(
                !plan.iter().any(|s| s.starts_with("SCAN")),
                "no scan (n={n}), got: {plan:?}"
            );
            let seeks: Vec<&String> = plan.iter().filter(|s| s.starts_with("SEARCH")).collect();
            assert!(
                seeks
                    .iter()
                    .any(|s| s.contains("IX_BaseItems_ParentId") && s.contains("ParentId=?"))
                    && seeks
                        .iter()
                        .any(|s| s.contains("IX_BaseItems_OwnerId") && s.contains("OwnerId=?"))
                    && seeks.iter().all(|s| s.contains("IX_BaseItems_ParentId")
                        || s.contains("IX_BaseItems_OwnerId")),
                "the children by ParentId and OwnerId (n={n}), got: {plan:?}"
            );
        }
    }

    /// The library scan's prune read (`library_items`) seeks each top parent
    /// by a `TopParentId` index and filters the rest — an alternate's
    /// primary a primary-key probe — with no sort: ordering some 20,000
    /// rows, as the item repository's library read did, spilled to a temp
    /// file on every scan (0.2 MB → 20.1 MB written by an unchanged
    /// rescan).
    #[tokio::test]
    async fn the_library_read_seeks_by_top_parent() {
        let db = test_db().await;
        for tops in [1, 2, 4] {
            let plan = query_plan(&db, &super::library_items_sql(tops), tops).await;
            assert!(
                !plan.iter().any(|s| s.starts_with("SCAN")),
                "no scan ({tops} top parents), got: {plan:?}"
            );
            assert!(
                !plan.iter().any(|s| s.contains("TEMP B-TREE")),
                "no sort ({tops} top parents), got: {plan:?}"
            );
            let seeks: Vec<&String> = plan.iter().filter(|s| s.starts_with("SEARCH")).collect();
            assert!(
                seeks
                    .iter()
                    .any(|s| s.starts_with("SEARCH bi ") && s.contains("(TopParentId=?")),
                "the library by a TopParentId seek ({tops} top parents), got: {plan:?}"
            );
            assert!(
                seeks.iter().all(
                    |s| (s.starts_with("SEARCH bi ") && s.contains("(TopParentId=?"))
                        || (s.starts_with("SEARCH p ")
                            && s.contains("sqlite_autoindex_BaseItems_1 (Id=?)"))
                ),
                "nothing else sought ({tops} top parents), got: {plan:?}"
            );
        }
    }

    /// `items_in_scope` returns the library's rows at or under the roots
    /// (component-wise: not a sibling whose name starts alike, whatever byte
    /// follows the prefix, nor one differing in case) and the path-less
    /// children of those rows — nothing else of the library, and nothing of
    /// another library under the same path. `child_links` returns the rows
    /// under the given parents.
    #[tokio::test]
    async fn items_in_scope_reads_the_rows_under_the_roots_and_the_pathless_children() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let (library, other) = (Uuid::from_u128(0x10), Uuid::from_u128(0x11));
        let rows = [
            (0x1, "/tv/Show", Some(library), None),
            (0x2, "/tv/Show/Season 1", Some(library), Some(0x1)),
            (0x3, "/tv/Show/Season 1/e1.mkv", Some(library), Some(0x2)),
            (0x4, "/tv/Show (2019)", Some(library), None),
            (0x5, "/tv/Show2/e.mkv", Some(library), None),
            (0x6, "/tv/Other/e.mkv", Some(library), None),
            (0x7, "/tv/Show/Season 1/e2.mkv", Some(other), None),
            (0x8, "", Some(library), Some(0x1)),
            (0x9, "", Some(library), Some(0x6)),
            (0xA, "/tv/Show-2/e.mkv", Some(library), None),
            (0xB, "/tv/Show.2/e.mkv", Some(library), None),
            (0xC, "/tv/Show0/e.mkv", Some(library), None),
            (0xD, "/tv/show/e.mkv", Some(library), None),
            (0xE, "/tv/Show/", Some(library), None),
            (0xF, "/tv/Show\te.mkv", Some(library), None),
        ];
        let id = |n: u128| Uuid::from_u128(0x5C0_0000 + n);
        for (n, path, top, parent) in rows {
            let id = id(n);
            let kind = if path.is_empty() {
                BaseItemKind::Season
            } else {
                BaseItemKind::Episode
            };
            seed_item(&db, id, kind).await;
            let mut row = crate::test_support::fetch_item(&db, id).await;
            row.path = (!path.is_empty()).then(|| path.to_owned());
            row.top_parent_id = top.map(guid_to_db);
            row.parent_id = parent.map(|p| guid_to_db(Uuid::from_u128(0x5C0_0000 + p)));
            crate::test_support::save_item(&db, &row).await;
        }
        let ids = |rows: Vec<ferrofin_traits::persistence::ItemPathRow>| {
            let mut ids: Vec<u128> = rows
                .into_iter()
                .map(|r| r.id.as_u128() - 0x5C0_0000)
                .collect();
            ids.sort_unstable();
            ids
        };

        let roots = ["/tv/Show/".to_owned(), "/tv/Show/Season 1".to_owned()];
        let found = svc
            .items_in_scope(&[library], &roots)
            .await
            .unwrap()
            .expect("answers");
        assert_eq!(ids(found), vec![0x1, 0x2, 0x3, 0x8, 0xE]);
        let children = svc
            .child_links(&[id(0x1), id(0x2)])
            .await
            .unwrap()
            .expect("answers");
        let mut children: Vec<(u128, Option<u128>)> = children
            .into_iter()
            .map(|c| {
                (
                    c.id.as_u128() - 0x5C0_0000,
                    c.parent_id.map(|p| p.as_u128() - 0x5C0_0000),
                )
            })
            .collect();
        children.sort_unstable();
        assert_eq!(
            children,
            vec![(0x2, Some(0x1)), (0x3, Some(0x2)), (0x8, Some(0x1))]
        );

        let at = svc
            .items_at_paths(&[
                "/tv/Show/Season 1".to_owned(),
                "/tv/Show/Season 1/e9.mkv".to_owned(),
            ])
            .await
            .unwrap()
            .expect("answers");
        assert_eq!(ids(at), vec![0x2]);
        assert_eq!(
            super::path_prefix_range("/tv/Show/"),
            (
                "/tv/Show".to_owned(),
                "/tv/Show/".to_owned(),
                "/tv/Show0".to_owned()
            )
        );
    }

    /// `items_in_scope` over an adopted library's top parents — its
    /// collection folder and its physical folder — reads the rows under the
    /// roots that carry either (one a scan saved, and an episode and a
    /// virtual season that still carry Jellyfin's physical folder), and
    /// still nothing of another library under the same path; over the
    /// collection folder alone, only the saved one; over none, nothing.
    #[tokio::test]
    async fn items_in_scope_reads_an_adopted_library_by_each_top_parent() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let (library, physical, other) = (
            Uuid::from_u128(0x20),
            Uuid::from_u128(0x21),
            Uuid::from_u128(0x22),
        );
        let id = |n: u128| Uuid::from_u128(0x5C1_0000 + n);
        for (n, path, top, parent) in [
            (0x1, "/tv/Show", library, None),
            (0x2, "/tv/Show/Season 1/e1.mkv", physical, Some(0x1)),
            (0x3, "", physical, Some(0x1)),
            (0x4, "/tv/Show/Season 1/e2.mkv", other, Some(0x1)),
        ] {
            let kind = if path.is_empty() {
                BaseItemKind::Season
            } else {
                BaseItemKind::Episode
            };
            seed_item(&db, id(n), kind).await;
            let mut row = crate::test_support::fetch_item(&db, id(n)).await;
            row.path = (!path.is_empty()).then(|| path.to_owned());
            row.top_parent_id = Some(guid_to_db(top));
            row.parent_id = parent.map(|p| guid_to_db(id(p)));
            crate::test_support::save_item(&db, &row).await;
        }
        let found = |tops: Vec<Uuid>| {
            let svc = &svc;
            async move {
                let mut ids: Vec<u128> = svc
                    .items_in_scope(&tops, &["/tv/Show".to_owned()])
                    .await
                    .unwrap()
                    .expect("answers")
                    .into_iter()
                    .map(|r| r.id.as_u128() - 0x5C1_0000)
                    .collect();
                ids.sort_unstable();
                ids
            }
        };
        assert_eq!(found(vec![library, physical]).await, vec![0x1, 0x2, 0x3]);
        assert_eq!(found(vec![library]).await, vec![0x1]);
        assert!(found(Vec::new()).await.is_empty());
    }

    /// `library_top_parents`: an adopted library (a collection folder whose
    /// `Data` names `PhysicalFolderIds`, N-format as Jellyfin writes them)
    /// answers itself first, then its physical folders; a native one (no
    /// `PhysicalFolderIds`, only the `CollectionType` Ferrofin stamps) and an
    /// id that is not a library answer themselves alone.
    #[tokio::test]
    async fn library_top_parents_is_the_library_and_its_physical_folders() {
        let db = test_db().await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let (adopted, native, physical, other) = (
            Uuid::from_u128(0xA0),
            Uuid::from_u128(0xA1),
            Uuid::from_u128(0xA2),
            Uuid::from_u128(0xA3),
        );
        seed_item(&db, adopted, BaseItemKind::CollectionFolder).await;
        seed_item(&db, native, BaseItemKind::CollectionFolder).await;
        seed_item(&db, physical, BaseItemKind::Folder).await;
        seed_item(&db, other, BaseItemKind::Movie).await;
        for (id, data) in [
            (
                adopted,
                format!(
                    r#"{{"CollectionType":"movies","PhysicalFolderIds":["{}"]}}"#,
                    physical.simple()
                ),
            ),
            (native, r#"{"CollectionType":"movies"}"#.to_owned()),
        ] {
            sqlx::query(r#"UPDATE "BaseItems" SET "Data" = ?2 WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .bind(data)
                .execute(db.writer())
                .await
                .expect("stamp data");
        }
        let tops = |id| {
            let svc = &svc;
            async move { svc.library_top_parents(id).await.unwrap().expect("answers") }
        };
        assert_eq!(tops(adopted).await, vec![adopted, physical]);
        assert_eq!(tops(native).await, vec![native]);
        assert_eq!(tops(other).await, vec![other]);
    }

    /// A season with an episode in a playlist and a collection, the episode
    /// owning an extra filed under the series (an adopted shape), the season
    /// owning one too. Seeded parent to child; returns `(series, season,
    /// episode, episode's extra, season's extra, playlist, collection)`.
    async fn season_with_links(
        db: &ferrofin_db::Database,
    ) -> (Uuid, Uuid, Uuid, Uuid, Uuid, Uuid, Uuid) {
        let ids: Vec<Uuid> = (0..7).map(|_| Uuid::new_v4()).collect();
        let (series, season, episode, extra, season_extra, playlist, boxset) =
            (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5], ids[6]);
        seed_item(db, series, BaseItemKind::Series).await;
        for (id, kind, parent, owner) in [
            (season, BaseItemKind::Season, series, None),
            (episode, BaseItemKind::Episode, season, None),
            (extra, BaseItemKind::Video, series, Some(episode)),
            (season_extra, BaseItemKind::Video, series, Some(season)),
        ] {
            seed_item(db, id, kind).await;
            let mut row = crate::test_support::fetch_item(db, id).await;
            row.parent_id = Some(guid_to_db(parent));
            row.owner_id = owner.map(guid_to_db);
            row.extra_type = owner.map(|_| 1);
            crate::test_support::save_item(db, &row).await;
        }
        seed_item(db, playlist, BaseItemKind::Playlist).await;
        seed_item(db, boxset, BaseItemKind::BoxSet).await;
        let links = FerrofinLinkedChildrenService::new(db.clone());
        for container in [playlist, boxset] {
            links
                .upsert_linked_child(container, episode, 0)
                .await
                .expect("link");
        }
        (
            series,
            season,
            episode,
            extra,
            season_extra,
            playlist,
            boxset,
        )
    }

    async fn rows_and_links(db: &ferrofin_db::Database) -> (i64, i64) {
        let rows: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems""#)
            .fetch_one(db.pool())
            .await
            .expect("rows");
        let links: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "LinkedChildren""#)
            .fetch_one(db.pool())
            .await
            .expect("links");
        (rows, links)
    }

    /// Upstream's `DeleteItem` (`ItemPersistenceService.cs:51-157`): the
    /// closure — the season, its episode (`ParentId`), the extras they own
    /// (`OwnerId`) wherever those are filed — goes in one transaction, the
    /// playlist and collection membership naming any of it first, and the
    /// ids deleted are returned exactly. Before, the season went first, the
    /// cascade reached the episode still named by `LinkedChildren` and by its
    /// extra's `OwnerId`, and the delete failed on the foreign key.
    #[tokio::test]
    async fn delete_items_takes_the_whole_closure_in_one_transaction() {
        let db = test_db().await;
        let (series, season, episode, extra, season_extra, playlist, boxset) =
            season_with_links(&db).await;
        let svc = FerrofinItemPersistenceService::new(db.clone());
        let (rows, links) = rows_and_links(&db).await;
        assert_eq!(links, 2);

        let mut deleted = svc
            .delete_items(&[season, Uuid::new_v4()])
            .await
            .expect("delete");
        deleted.sort_unstable();
        let mut expected = vec![season, episode, extra, season_extra];
        expected.sort_unstable();
        assert_eq!(deleted, expected, "the closure, and no id without a row");
        assert_eq!(rows_and_links(&db).await, (rows - 4, 0));
        for kept in [series, playlist, boxset] {
            assert!(
                crate::test_support::fetch_item_opt(&db, kept)
                    .await
                    .is_some(),
                "{kept} kept"
            );
        }
        assert!(
            svc.delete_items(&[season]).await.expect("again").is_empty(),
            "nothing left to delete"
        );
    }

    /// Upstream's `ItemPersistenceDeleteItemTests`
    /// (`tests/Jellyfin.Server.Implementations.Tests/Item/
    /// ItemPersistenceDeleteItemTests.cs`), transliterated: each seeds
    /// `(Id, OwnerId, ParentId)` rows, deletes the first, and expects every
    /// seeded row gone.
    ///
    /// - `DeleteItem_OwnerIdChain_DeletesWholeChain`: an extra that owns an
    ///   extra of its own;
    /// - `DeleteItem_ExtraOwnedByCascadedChild_DeletesExtraToo`: a child
    ///   reached by `ParentId` owns an extra;
    /// - `DeleteItem_OwnershipCycle_Terminates`: two rows that own each
    ///   other.
    #[rstest::rstest]
    #[case::owner_id_chain(&[(0, None, None), (1, Some(0), None), (2, Some(1), None)], None)]
    #[case::extra_owned_by_cascaded_child(&[(0, None, None), (1, None, Some(0)), (2, Some(1), None)], None)]
    #[case::ownership_cycle(&[(0, None, None), (1, Some(0), None)], Some(1))]
    #[tokio::test]
    async fn upstream_delete_item_closure(
        #[case] rows: &[(usize, Option<usize>, Option<usize>)],
        #[case] owner_of_first: Option<usize>,
    ) {
        let db = test_db().await;
        let ids: Vec<Uuid> = (0..rows.len()).map(|_| Uuid::new_v4()).collect();
        for (index, _, _) in rows {
            seed_item(&db, ids[*index], BaseItemKind::Movie).await;
        }
        for (index, owner, parent) in rows {
            let mut row = crate::test_support::fetch_item(&db, ids[*index]).await;
            row.owner_id = owner.map(|o| guid_to_db(ids[o]));
            row.parent_id = parent.map(|p| guid_to_db(ids[p]));
            crate::test_support::save_item(&db, &row).await;
        }
        if let Some(owner) = owner_of_first {
            let mut row = crate::test_support::fetch_item(&db, ids[0]).await;
            row.owner_id = Some(guid_to_db(ids[owner]));
            crate::test_support::save_item(&db, &row).await;
        }
        let svc = FerrofinItemPersistenceService::new(db.clone());
        svc.delete_items(&ids[..1]).await.expect("delete");
        for id in ids {
            assert!(
                crate::test_support::fetch_item_opt(&db, id).await.is_none(),
                "{id} deleted"
            );
        }
    }

    /// A failure part-way through the delete leaves everything as it was: no
    /// row and no playlist or collection link half-deleted.
    #[tokio::test]
    async fn a_failure_mid_delete_rolls_the_whole_delete_back() {
        let db = test_db().await;
        let (_, season, _, extra, _, _, _) = season_with_links(&db).await;
        let before = rows_and_links(&db).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"CREATE TRIGGER "TestFailDelete" BEFORE DELETE ON "BaseItems"
               WHEN old."Id" = '{}' BEGIN SELECT RAISE(ABORT, 'injected'); END"#,
            guid_to_db(extra)
        )))
        .execute(db.writer())
        .await
        .expect("trigger");
        let svc = FerrofinItemPersistenceService::new(db.clone());
        assert!(svc.delete_items(&[season]).await.is_err());
        assert_eq!(rows_and_links(&db).await, before, "nothing half-deleted");
    }
}
