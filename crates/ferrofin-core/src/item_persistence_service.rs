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

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::{datetime_to_db, guid_to_db, opt_datetime_to_db};
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::{ItemPersistenceService, StoredImageMetadata};

use ferrofin_model::data::BaseItemKind;

use crate::db_error::db_err;
use crate::item_repository::image_type_to_disc;
use crate::item_type_lookup::{MUSIC_GENRE_TYPES, stored_type_name};
use crate::text_util::get_clean_value;
use crate::translate_query::PLACEHOLDER_ID;

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
        r#"INSERT INTO "BaseItems"
           ("Id","Type","Name","CleanName","SortName","IsFolder","IsInMixedFolder",
            "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
           SELECT ?1,?2,?3,?4,?5,1,0,0,0,0,0,0
           WHERE NOT EXISTS (
               SELECT 1 FROM "BaseItems" WHERE "Type" = ?2 AND "CleanName" = ?4)"#,
    )
    .bind(guid_to_db(id))
    .bind(type_name)
    .bind(value)
    .bind(clean)
    .bind(ferrofin_util::sort_name::create_sort_name(value))
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
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Name", "SortName",
            "ParentId", "TopParentId")
           VALUES (?1, ?2, ?3, 0, 0, 0, 0, 0, 0, ?4, ?5, ?6, ?6)"#,
    )
    .bind(guid_to_db(id))
    .bind(type_name)
    .bind(i64::from(is_folder))
    .bind(name)
    .bind(ferrofin_util::sort_name::create_sort_name(name))
    .bind(container.map(guid_to_db))
    .execute(db.writer())
    .await
    .map_err(db_err)?;

    sqlx::query_as::<_, BaseItemEntity>(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .fetch_one(db.pool())
        .await
        .map_err(db_err)
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
/// **Matched by exact path**, the way `EnsureLibraryFolder` does
/// (`FindFolders(path)`), and never by type: `CollectionFolder` is the type of
/// *every* library, so a type match would file collections into whichever one
/// sorted first. Two spellings are accepted because Jellyfin writes the
/// literal `%AppDataPath%` token where Ferrofin writes the resolved path —
/// both are equalities, not patterns, so a user library that happens to be
/// called `collections` cannot be mistaken for this.
pub(crate) async fn ensure_container(
    db: &Database,
    kind: BaseItemKind,
    name: &str,
    path: &str,
    mode: &crate::item_type_lookup::IdDerivation,
    parent: Option<Uuid>,
) -> Result<Option<Uuid>, ServiceError> {
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
    if let Some(id) = existing {
        let id = Uuid::parse_str(&id).map_err(|e| {
            ServiceError::backend(format!("container row {id} has an unusable id: {e}"))
        })?;
        // Adopt a row that was created before the user root existed — the
        // parent is set on the first provision that CAN set it, rather than
        // staying null forever because the row is already there.
        if let Some(root) = parent {
            attach_to_root(db, id, root).await?;
        }
        return Ok(Some(id));
    }

    // Derived from the path, like every other folder id on both sides, so the
    // same directory yields the same id wherever it is scanned — under the
    // database's CONFIGURED derivation, not a hardcoded one. Getting that wrong
    // is not cosmetic: Jellyfin computes its own id for
    // `%AppDataPath%/collections`, and if ours differs it does not recognise
    // the row, creates a SECOND Collections library beside it, and the two-way
    // swap this project rests on stops being clean.
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
    Ok(Some(id))
}

/// Parents a container to the user root, if it has no parent yet and the root
/// row exists.
///
/// Both guards matter: a row that already has a parent is not ours to move, and
/// `BaseItems.ParentId` is a foreign key, so pointing at a root that has not
/// been provisioned yet would fail the statement.
async fn attach_to_root(db: &Database, id: Uuid, root: Uuid) -> Result<(), ServiceError> {
    if !row_exists(db, root).await? {
        return Ok(());
    }
    sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1 AND "ParentId" IS NULL"#)
        .bind(guid_to_db(id))
        .bind(guid_to_db(root))
        .execute(db.writer())
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Whether a `BaseItems` row with this id exists.
async fn row_exists(db: &Database, id: Uuid) -> Result<bool, ServiceError> {
    let found: Option<String> =
        sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_optional(db.pool())
            .await
            .map_err(db_err)?;
    Ok(found.is_some())
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
        Self { db }
    }

    /// One-shot startup pass: rewrites every stored `CleanName` / `CleanValue`
    /// that disagrees with [`get_clean_value`], recording completion in
    /// `FerrofinMeta` so later boots skip it.
    ///
    /// Ferrofin used to compute the clean columns by also replacing punctuation
    /// with spaces and collapsing whitespace, where C# `GetCleanValue` only
    /// removes diacritics and lowercases. Databases written by those versions
    /// hold `'h jon benjamin'` where the lookups now compute `'h. jon
    /// benjamin'`, so every by-name resolution of a punctuated name — person,
    /// studio, genre, tag — would miss until the next full scan rewrote the
    /// row. A database adopted from Jellyfin already agrees, and this pass
    /// leaves it untouched.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when a query or the rewrite transaction
    /// fails; the marker is written inside that transaction, so a failure
    /// simply retries on the next boot.
    pub async fn repair_clean_values(&self) -> Result<u64, ServiceError> {
        const META_KEY: &str = "clean_values_keep_punctuation_v1";
        let done = self
            .db
            .meta_get(META_KEY)
            .await
            .map_err(|e| ServiceError::Backend(e.to_string()))?;
        if done.as_deref() == Some("1") {
            return Ok(0);
        }

        let items: Vec<(String, Option<String>, Option<String>)> =
            // The migration's placeholder row is excluded here as it is
            // everywhere else — it has no name, and rewriting it would report
            // work on a database that has nothing to repair.
            sqlx::query_as(r#"SELECT "Id", "Name", "CleanName" FROM "BaseItems" WHERE "Id" <> ?1"#)
                .bind(PLACEHOLDER_ID)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;
        let values: Vec<(String, Option<String>, Option<String>)> =
            sqlx::query_as(r#"SELECT "ItemValueId", "Value", "CleanValue" FROM "ItemValues""#)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;

        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        let mut repaired: u64 = 0;
        for (id, name, stored) in items {
            let want = name.as_deref().map(get_clean_value);
            if want.as_deref() == stored.as_deref()
                || !was_written_by_the_old_rule(name.as_deref(), stored.as_deref())
            {
                continue;
            }
            sqlx::query(r#"UPDATE "BaseItems" SET "CleanName" = ?2 WHERE "Id" = ?1"#)
                .bind(&id)
                .bind(&want)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            repaired += 1;
        }
        for (id, value, stored) in values {
            // `CleanValue` is NOT NULL; a null `Value` cleans to the empty
            // string rather than dropping the column.
            let want = get_clean_value(value.as_deref().unwrap_or_default());
            if Some(want.as_str()) == stored.as_deref()
                || !was_written_by_the_old_rule(value.as_deref(), stored.as_deref())
            {
                continue;
            }
            sqlx::query(r#"UPDATE "ItemValues" SET "CleanValue" = ?2 WHERE "ItemValueId" = ?1"#)
                .bind(&id)
                .bind(&want)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            repaired += 1;
        }
        sqlx::query(
            r#"INSERT INTO "FerrofinMeta" ("Key", "Value") VALUES (?1, '1')
               ON CONFLICT("Key") DO UPDATE SET "Value" = '1'"#,
        )
        .bind(META_KEY)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(repaired)
    }

    /// Upserts a single item row (`INSERT … ON CONFLICT("Id") DO UPDATE`) using
    /// `sql` — [`UPSERT_SQL`] for a full-row replace, [`scan_upsert_sql`] for
    /// the library scan's ownership-respecting variant. Both bind the same
    /// columns in the same order.
    async fn upsert_item(&self, item: &BaseItemEntity, sql: &str) -> Result<(), ServiceError> {
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
        // `ModifySortChunks(ForcedSortName).ToLowerInvariant()` or
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
        let sort_name = item.sort_name.clone().or_else(|| {
            let forced = item.forced_sort_name.as_deref().filter(|f| !f.is_empty());
            match forced {
                Some(f) => Some(ferrofin_util::sort_name::forced_sort_key(f)),
                None => item
                    .name
                    .as_deref()
                    .map(ferrofin_util::sort_name::create_sort_name),
            }
        });
        sqlx::query(sql)
            .bind(&item.id)
            .bind(&item.album)
            .bind(&item.album_artists)
            .bind(&item.artists)
            .bind(item.audio)
            .bind(&item.channel_id)
            .bind(clean_name)
            .bind(item.community_rating)
            .bind(item.critic_rating)
            .bind(&item.custom_rating)
            .bind(&item.data)
            .bind(opt_datetime_to_db(item.date_created))
            .bind(opt_datetime_to_db(item.date_last_media_added))
            .bind(opt_datetime_to_db(item.date_last_refreshed))
            .bind(opt_datetime_to_db(item.date_last_saved))
            .bind(opt_datetime_to_db(item.date_modified))
            .bind(opt_datetime_to_db(item.end_date))
            .bind(&item.episode_title)
            .bind(&item.external_id)
            .bind(&item.external_series_id)
            .bind(&item.external_service_id)
            .bind(item.extra_type)
            .bind(&item.forced_sort_name)
            .bind(&item.genres)
            .bind(item.height)
            .bind(item.index_number)
            .bind(item.inherited_parental_rating_sub_value)
            .bind(item.inherited_parental_rating_value)
            .bind(item.is_folder)
            .bind(item.is_in_mixed_folder)
            .bind(item.is_locked)
            .bind(item.is_movie)
            .bind(item.is_repeat)
            .bind(item.is_series)
            .bind(item.is_virtual_item)
            .bind(item.lufs)
            .bind(&item.media_type)
            .bind(&item.name)
            .bind(item.normalization_gain)
            .bind(&item.official_rating)
            .bind(&item.extra_ids)
            .bind(&item.original_title)
            .bind(&item.overview)
            .bind(&item.owner_id)
            .bind(&item.parent_id)
            .bind(item.parent_index_number)
            .bind(&item.path)
            .bind(&item.preferred_metadata_country_code)
            .bind(&item.preferred_metadata_language)
            .bind(opt_datetime_to_db(item.premiere_date))
            .bind(&presentation_unique_key)
            .bind(&item.primary_version_id)
            .bind(&item.production_locations)
            .bind(item.production_year)
            .bind(item.run_time_ticks)
            .bind(&item.season_id)
            .bind(&item.season_name)
            .bind(&item.series_id)
            .bind(&item.series_name)
            .bind(&item.series_presentation_unique_key)
            .bind(&item.show_id)
            .bind(item.size)
            .bind(&sort_name)
            .bind(opt_datetime_to_db(item.start_date))
            .bind(&item.studios)
            .bind(&item.tagline)
            .bind(&item.tags)
            .bind(&item.top_parent_id)
            .bind(item.total_bitrate)
            .bind(&item.type_)
            .bind(&item.unrated_type)
            .bind(item.width)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

#[async_trait]
impl ItemPersistenceService for FerrofinItemPersistenceService {
    async fn delete_items(&self, ids: &[Uuid]) -> Result<(), ServiceError> {
        let mut touched_parents: Vec<Uuid> = Vec::new();
        for id in ids {
            let id_db = guid_to_db(*id);
            if id_db == PLACEHOLDER_ID {
                // Never delete the UserData placeholder row.
                continue;
            }
            // Containers whose membership shrinks need their Data JSON
            // re-synced after the delete (captured before the edges go).
            let parents: Vec<String> = sqlx::query_scalar(
                r#"SELECT DISTINCT "ParentId" FROM "FerrofinLinkedChildren" WHERE "ChildId" = ?1"#,
            )
            .bind(&id_db)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
            touched_parents.extend(parents.iter().filter_map(|p| Uuid::parse_str(p).ok()));
            // `FerrofinLinkedChildren` is the one BaseItems FK without `ON DELETE
            // CASCADE` (it references the item as both parent and child), so
            // clear those links first — otherwise deleting a
            // playlist/collection, or an item that belongs to one, trips a
            // FOREIGN KEY constraint (787).
            sqlx::query(
                r#"DELETE FROM "FerrofinLinkedChildren" WHERE "ParentId" = ?1 OR "ChildId" = ?1"#,
            )
            .bind(&id_db)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
            sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&id_db)
                .execute(self.db.writer())
                .await
                .map_err(db_err)?;
        }
        touched_parents.sort_unstable();
        touched_parents.dedup();
        for parent in touched_parents {
            // Deleted containers no-op inside (their row is gone).
            crate::item_data::sync_container_data(&self.db, parent).await?;
        }
        Ok(())
    }

    async fn save_items(&self, items: &[BaseItemEntity]) -> Result<(), ServiceError> {
        for item in items {
            self.upsert_item(item, UPSERT_SQL).await?;
        }
        Ok(())
    }

    async fn save_scanned_items(&self, items: &[BaseItemEntity]) -> Result<(), ServiceError> {
        for item in items {
            self.upsert_item(item, scan_upsert_sql()).await?;
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
        sqlx::query(
            r#"UPDATE "BaseItems" SET "PrimaryVersionId" = ?1, "PresentationUniqueKey" = ?2
               WHERE "Id" = ?3"#,
        )
        .bind(primary_version_id.map(guid_to_db))
        .bind(presentation_key)
        .bind(guid_to_db(item_id))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
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
            if let Some(type_name) = by_name_type_name(*type_) {
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
                    r#"INSERT OR IGNORE INTO "BaseItems"
                       ("Id","Type","Name","CleanName","SortName","PresentationUniqueKey",
                        "IsFolder","IsInMixedFolder",
                        "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
                       VALUES (?1,?2,?3,?4,?5,?6,1,0,0,0,0,0,0)"#,
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
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }
            if owner_is_music && *type_ == i32::from(ferrofin_db::enums::ItemValueType::Genre) {
                music_genre_row(&mut tx, value, &clean).await?;
            }
        }
        tx.commit().await.map_err(db_err)
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
        let item = guid_to_db(item_id);
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        // Replace the item's image set (idempotent re-scan).
        sqlx::query(r#"DELETE FROM "BaseItemImageInfos" WHERE "ItemId" = ?1"#)
            .bind(&item)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for image in images {
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos"
                   ("Id", "ItemId", "ImageType", "Path", "Width", "Height", "Blurhash", "DateModified")
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
            )
            .bind(guid_to_db(Uuid::new_v4()))
            .bind(&item)
            .bind(image_type_to_disc(image.image_type))
            .bind(&image.path)
            .bind(i64::from(image.width))
            .bind(i64::from(image.height))
            .bind(image.blur_hash.as_deref().map(str::as_bytes)) // BLOB of the hash's UTF-8 bytes
            .bind(datetime_to_db(image.date_modified))
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
            >(&sql);
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

/// The full-column upsert statement for a `BaseItems` row. Column order matches
/// the bind order in [`FerrofinItemPersistenceService::upsert_item`].
const UPSERT_SQL: &str = r#"INSERT INTO "BaseItems" (
    "Id", "Album", "AlbumArtists", "Artists", "Audio", "ChannelId", "CleanName",
    "CommunityRating", "CriticRating", "CustomRating", "Data", "DateCreated",
    "DateLastMediaAdded", "DateLastRefreshed", "DateLastSaved", "DateModified",
    "EndDate", "EpisodeTitle", "ExternalId", "ExternalSeriesId", "ExternalServiceId",
    "ExtraType", "ForcedSortName", "Genres", "Height", "IndexNumber",
    "InheritedParentalRatingSubValue", "InheritedParentalRatingValue", "IsFolder",
    "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem",
    "LUFS", "MediaType", "Name", "NormalizationGain", "OfficialRating",
    "ExtraIds", "OriginalTitle", "Overview", "OwnerId", "ParentId",
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
    "DateLastRefreshed" = excluded."DateLastRefreshed", "DateLastSaved" = excluded."DateLastSaved",
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
    "ExtraIds" = excluded."ExtraIds", "OriginalTitle" = excluded."OriginalTitle",
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
/// `POST /Items/{id}` writes, plus the `Name`-derived `CleanName`/`SortName`):
/// the scan's upsert keeps the stored value for each of these when the row is
/// locked, so a locked item's edits survive every rescan.
const LOCKED_PRESERVED_COLUMNS: &[&str] = &[
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

/// The library scan's upsert: identical to [`UPSERT_SQL`] except for the
/// columns the scanner does not own on an existing row —
///
/// - `PrimaryVersionId` is left untouched (a scanned entity always carries
///   `None`, and overwriting erased every merge-versions link on each scan),
/// - `DateCreated` keeps its stored first-import value (`coalesce` still fills
///   it when the stored value is `NULL`),
/// - `IsLocked` can be set by the scan (an NFO `<lockdata>`) but never
///   cleared (`max`) — otherwise every scan would silently unlock edits,
/// - every [`LOCKED_PRESERVED_COLUMNS`] entry keeps its stored value when the
///   row is locked (in the `CASE`, the unqualified `"IsLocked"` reads the
///   existing row, so the guard sees the pre-write lock state).
///
/// Derived from [`UPSERT_SQL`] by text substitution so the column/bind layout
/// cannot drift between the two statements; the substitutions are asserted in
/// `scan_upsert_preserves_unowned_columns`.
fn scan_upsert_sql() -> &'static str {
    static SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let mut sql = UPSERT_SQL
            .replace(
                r#""DateCreated" = excluded."DateCreated","#,
                r#""DateCreated" = coalesce("DateCreated", excluded."DateCreated"),"#,
            )
            .replace(r#""PrimaryVersionId" = excluded."PrimaryVersionId","#, "")
            .replace(
                r#""IsLocked" = excluded."IsLocked","#,
                r#""IsLocked" = max("IsLocked", excluded."IsLocked"),"#,
            );
        for col in LOCKED_PRESERVED_COLUMNS {
            sql = sql.replace(
                &format!(r#""{col}" = excluded."{col}""#),
                &format!(
                    r#""{col}" = CASE WHEN "IsLocked" = 1 THEN "{col}" ELSE excluded."{col}" END"#
                ),
            );
        }
        sql
    });
    &SQL
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
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT "Id", "Name", "ForcedSortName" FROM "BaseItems"
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
    for (id, name, forced) in &rows {
        let sort_name = match forced.as_deref().filter(|f| !f.is_empty()) {
            Some(f) => ferrofin_util::sort_name::forced_sort_key(f),
            None => ferrofin_util::sort_name::create_sort_name(name),
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
    // the metadata language and the library folders (`Series.cs:81`). That
    // option is not ported, so recomputing here would flip every re-saved
    // series on such a server to its own id and orphan its seasons'
    // `SeriesPresentationUniqueKey`. (The verification library has the option
    // off on every TV folder, which is why its 126 series all store own-id and
    // the adoption suite cannot see this.)
    if kind == BaseItemKind::Series && stored.is_some() {
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

/// Whether `stored` is what Ferrofin's OLD clean rule would have produced for
/// `source` — diacritics removed, lowercased, every other character collapsed
/// to a single space, trimmed.
///
/// The repair pass rewrites a column only when this says yes, so it undoes
/// Ferrofin's own damage and touches nothing else. Without the guard it would
/// rewrite any row where the two implementations of diacritic folding disagree
/// at all — including rows a Jellyfin install wrote, which is a silent mutation
/// of someone else's data and breaks the two-way adoption guarantee.
fn was_written_by_the_old_rule(source: Option<&str>, stored: Option<&str>) -> bool {
    let (Some(source), Some(stored)) = (source, stored) else {
        // A null clean column was never written by the old rule for a named
        // row; filling it in is safe and is what a save would do anyway.
        return stored.is_none();
    };
    let cleaned = get_clean_value(source);
    let old: String = {
        let mut out = String::with_capacity(cleaned.len());
        let mut last_was_space = false;
        for ch in cleaned.chars() {
            if ch.is_alphabetic() || ch.is_numeric() {
                out.push(ch);
                last_was_space = false;
            } else if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        }
        out.trim().to_owned()
    };
    stored == old
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

#[cfg(test)]
mod tests {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::persistence::{ItemPersistenceService, LinkedChildrenService};
    use uuid::Uuid;

    use crate::linked_children_service::FerrofinLinkedChildrenService;
    use crate::test_support::{seed_item, test_db};

    use super::FerrofinItemPersistenceService;

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

        let remaining: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinLinkedChildren""#)
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

    // `ForcedSortName` short-circuits `CreateSortName` in C#: it is padded and
    // lower-cased, but its articles and punctuation are left alone.
    #[tokio::test]
    async fn save_items_derives_from_a_forced_sort_name_when_present() {
        let db = test_db().await;
        let entity = ferrofin_db::entities::base_items::BaseItemEntity {
            forced_sort_name: Some("The Matrix 2".to_owned()),
            ..named("The Matrix")
        };
        assert_eq!(
            persisted_sort_name(&db, entity).await.as_deref(),
            Some("the matrix 0000000002")
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

    // The library scan rebuilds entities from disk with no merge link and a
    // scan-time DateCreated. Its save must not clobber either on an existing
    // row (a plain save_items erased every merge-versions link on each scan),
    // while the full save — the merge/split write path — must still set AND
    // clear both.
    #[tokio::test]
    async fn scan_upsert_preserves_unowned_columns() {
        // Guard the text-substitution derivation of the scan SQL: if the base
        // UPSERT_SQL text drifts, the replacements silently no-op and this
        // catches it before the behavioral asserts do.
        let sql = super::scan_upsert_sql();
        assert!(sql.contains(r#"coalesce("DateCreated", excluded."DateCreated")"#));
        assert!(!sql.contains(r#""PrimaryVersionId" = excluded."PrimaryVersionId""#));
        assert!(sql.contains(r#""IsLocked" = max("IsLocked", excluded."IsLocked")"#));
        for col in super::LOCKED_PRESERVED_COLUMNS {
            assert!(
                sql.contains(&format!(
                    r#""{col}" = CASE WHEN "IsLocked" = 1 THEN "{col}""#
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

    /// A database written by a Ferrofin version whose `get_clean_value`
    /// stripped punctuation is repaired in place on the next boot — otherwise a
    /// person, studio or genre with a `.` or `-` in its name stays unreachable
    /// by name until someone runs a full rescan.
    #[tokio::test]
    async fn the_clean_value_repair_rewrites_stale_columns_once() {
        let db = test_db().await;
        let service = FerrofinItemPersistenceService::new(db.clone());
        let id = Uuid::from_u128(0xC1EA);
        crate::test_support::seed_named_item(&db, id, BaseItemKind::Person, "H. Jon Benjamin")
            .await;
        // The stale spelling the old rule produced.
        sqlx::query(r#"UPDATE "BaseItems" SET "CleanName" = 'h jon benjamin' WHERE "Id" = ?1"#)
            .bind(ferrofin_db::store::guid_to_db(id))
            .execute(db.writer())
            .await
            .expect("stale clean name");
        sqlx::query(
            r#"INSERT INTO "ItemValues" ("ItemValueId","Type","Value","CleanValue")
               VALUES (1, 3, 'Warner Bros. Pictures', 'warner bros pictures')"#,
        )
        .execute(db.writer())
        .await
        .expect("stale clean value");

        assert_eq!(service.repair_clean_values().await.expect("repair"), 2);
        let clean: Option<String> =
            sqlx::query_scalar(r#"SELECT "CleanName" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(ferrofin_db::store::guid_to_db(id))
                .fetch_one(db.pool())
                .await
                .expect("read back");
        assert_eq!(clean.as_deref(), Some("h. jon benjamin"));
        let value: String =
            sqlx::query_scalar(r#"SELECT "CleanValue" FROM "ItemValues" WHERE "ItemValueId" = 1"#)
                .fetch_one(db.pool())
                .await
                .expect("read back");
        assert_eq!(value, "warner bros. pictures");

        // Once only: the marker means a second boot does no work.
        assert_eq!(service.repair_clean_values().await.expect("repair"), 0);
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
}
