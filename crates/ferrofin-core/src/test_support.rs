//! Shared test fixtures for the repository/service unit tests.
//!
//! These helpers seed a migrated in-memory `ferrofin-db` so each repository test
//! can insert exactly the rows it needs (an item, a user, user-data) without
//! duplicating the verbose `INSERT` boilerplate across modules. Items are seeded
//! with their real stored `Type` name (via
//! [`stored_type_name`](crate::item_type_lookup::stored_type_name)) so tests that
//! filter on kind behave like production. Compiled only under `cfg(test)`.
//!
//! Fixture SQL lives *here*, not in the module under test: this is the crate's
//! persistence-adjacent fixture module, and the `sql_boundary` ratchet
//! (`crates/ferrofin-db/tests/sql_boundary.rs`) keeps raw SQL out of the
//! manager/service files. Where a production write path already exists
//! (`ItemPersistenceService`, `ItemRepository`) the fixture goes through it
//! rather than re-deriving the same statement.

#![cfg(test)]

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::store::{guid_to_db, opt_datetime_to_db, opt_guid_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::ImageType;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use uuid::Uuid;

use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_type_lookup::stored_type_name;

/// Builds a [`FerrofinLibraryManager`](crate::library_manager::FerrofinLibraryManager)
/// backed by the real unit-1/2 repositories over the given database, for
/// manager tests that need to resolve item kinds/rows through the library seam.
pub fn library_manager_over(db: Database) -> Arc<dyn ferrofin_traits::library::LibraryManager> {
    use crate::item_count_service::FerrofinItemCountService;
    use crate::people_repository::FerrofinPeopleRepository;

    Arc::new(crate::library_manager::FerrofinLibraryManager::new(
        item_repository_over(db.clone()),
        Arc::new(FerrofinItemCountService::new(db.clone())),
        Arc::new(FerrofinItemPersistenceService::new(db.clone())),
        Arc::new(FerrofinPeopleRepository::new(db)),
    ))
}

/// Builds the real [`FerrofinItemRepository`](crate::item_repository::FerrofinItemRepository)
/// over the given database, for manager tests that inject the item-repository
/// seam directly, and for the read-path fixtures that hand a test back the row
/// it just seeded.
pub fn item_repository_over(db: Database) -> Arc<dyn ItemRepository> {
    use crate::item_repository::FerrofinItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;

    let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
        Arc::new(ItemTypeLookup::new());
    Arc::new(FerrofinItemRepository::new(db, lookup))
}

/// The real item-persistence service over `db` — the write-path fixtures use it
/// for images, provider ids and item values, so seeding goes through production
/// code instead of raw SQL.
fn persistence_over(db: &Database) -> FerrofinItemPersistenceService {
    FerrofinItemPersistenceService::new(db.clone())
}

/// Opens a fresh in-memory database with the head schema applied.
pub async fn test_db() -> Database {
    let db = Database::connect_in_memory()
        .await
        .expect("in-memory connect");
    db.run_migrations().await.expect("migrations apply");
    db
}

/// The stored `Type` name for a kind, panicking if the kind has none (a test-
/// authoring error).
fn type_name(kind: BaseItemKind) -> &'static str {
    stored_type_name(kind).expect("kind has a stored type name")
}

/// The optional `BaseItems` columns a seeded row can carry. Everything left at
/// its default is stored `NULL`/`0` — which is what a minimal row looks like.
#[derive(Default)]
struct ItemRow<'a> {
    /// `Name` (stored `NULL` when empty).
    name: &'a str,
    /// `IsFolder`.
    is_folder: bool,
    /// `IsVirtualItem`.
    is_virtual: bool,
    /// `ParentId`.
    parent: Option<Uuid>,
    /// `TopParentId`.
    top_parent: Option<Uuid>,
    /// `SeriesPresentationUniqueKey`.
    series_key: Option<&'a str>,
    /// `ParentIndexNumber` — the season number, for episodes.
    season: Option<i64>,
    /// `IndexNumber` — the episode number, for episodes.
    episode: Option<i64>,
    /// `Data` — Jellyfin's serialized-item JSON blob.
    data: Option<&'a str>,
    /// `MediaType` — `"Video"` / `"Audio"`. `None` leaves the column NULL,
    /// which is what every fixture that does not care about it gets.
    media_type: Option<&'a str>,
    /// `SeriesId` — the owning series, for a season or an episode.
    series_id: Option<Uuid>,
    /// `Studios` — the pipe-joined studio list a series carries, which is where
    /// a season's/episode's `SeriesStudio` comes from.
    studios: Option<&'a str>,
}

/// The one `BaseItems` insert every item fixture goes through.
async fn insert_base_item(db: &Database, id: Uuid, kind: BaseItemKind, row: &ItemRow<'_>) {
    insert_base_item_raw_id(db, &guid_to_db(id), kind, row).await;
}

/// Inserts a child row whose `Id` is **not** a `Guid`, parented to `parent`.
///
/// The column is plain `TEXT` with a single writer, so no production path
/// produces this — but code that reads `BaseItems.Id` back out must not degrade
/// it to the nil GUID when it fails to parse (see `fc01259`), and that guard
/// needs a row to exercise.
pub async fn seed_child_with_raw_id(db: &Database, raw_id: &str, kind: BaseItemKind, parent: Uuid) {
    insert_base_item_raw_id(
        db,
        raw_id,
        kind,
        &ItemRow {
            name: "Corrupt Row",
            parent: Some(parent),
            ..ItemRow::default()
        },
    )
    .await;
}

/// [`insert_base_item`] taking the `Id` column value verbatim, so a fixture can
/// store an id that is *not* a `Guid` (the column is plain `TEXT`, so the shape
/// exists even though no writer produces it).
async fn insert_base_item_raw_id(
    db: &Database,
    raw_id: &str,
    kind: BaseItemKind,
    row: &ItemRow<'_>,
) {
    // `PresentationUniqueKey`, exactly as `save_items` derives it, because a
    // seeded row that omits it is not a row a real server ever holds: the
    // recursive user universe is queried with `GROUP BY PresentationUniqueKey`
    // and SQLite groups NULLs TOGETHER, so two keyless fixtures collapse into
    // one and the test measures a shape the product does not have. Upstream
    // leaves the column NULL for exactly one kind, `LiveTvProgram` — the guide,
    // whose airings are meant to collapse — so that kind keeps its NULL here.
    let presentation_key = (kind != BaseItemKind::LiveTvProgram)
        .then(|| Uuid::parse_str(raw_id).ok())
        .flatten()
        .map(|id| {
            crate::kinds::presentation_unique_key(
                kind,
                id,
                (!row.name.is_empty()).then_some(row.name),
                None,
                row.series_key,
                row.episode,
            )
        });
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Name", "ParentId",
            "TopParentId", "SeriesPresentationUniqueKey", "ParentIndexNumber",
            "IndexNumber", "Data", "MediaType", "PresentationUniqueKey",
            "SeriesId", "Studios")
           VALUES (?1, ?2, ?3, 0, 0, 0, 0, 0, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                   ?14, ?15)"#,
    )
    .bind(raw_id)
    .bind(type_name(kind))
    .bind(i64::from(row.is_folder))
    .bind(i64::from(row.is_virtual))
    .bind(if row.name.is_empty() {
        None
    } else {
        Some(row.name)
    })
    .bind(opt_guid_to_db(row.parent))
    .bind(opt_guid_to_db(row.top_parent))
    .bind(row.series_key)
    .bind(row.season)
    .bind(row.episode)
    .bind(row.data)
    .bind(row.media_type)
    .bind(presentation_key)
    .bind(opt_guid_to_db(row.series_id))
    .bind(row.studios)
    .execute(db.writer())
    .await
    .expect("insert item");
}

/// Inserts a named `BaseItems` row carrying a `Data` JSON blob — the column
/// Jellyfin serializes an item's non-column properties into (photo EXIF,
/// playlist membership, `VideoType`, …).
pub async fn seed_item_with_data(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
    name: &str,
    data: &str,
) {
    insert_base_item(
        db,
        id,
        kind,
        &ItemRow {
            name,
            data: Some(data),
            ..ItemRow::default()
        },
    )
    .await;
}

/// Inserts a `Series` row carrying a pipe-joined studio list — the source of
/// its seasons'/episodes' `SeriesStudio`.
pub async fn seed_series_with_studios(db: &Database, id: Uuid, name: &str, studios: &str) {
    insert_base_item(
        db,
        id,
        BaseItemKind::Series,
        &ItemRow {
            name,
            is_folder: true,
            studios: Some(studios),
            ..ItemRow::default()
        },
    )
    .await;
}

/// Inserts a `Season`/`Episode` row bound to `series`.
pub async fn seed_item_of_series(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
    name: &str,
    series: Uuid,
) {
    insert_base_item(
        db,
        id,
        kind,
        &ItemRow {
            name,
            is_folder: kind == BaseItemKind::Season,
            series_id: Some(series),
            ..ItemRow::default()
        },
    )
    .await;
}

/// Inserts a named `BaseItems` row of the given kind hanging off `parent`.
pub async fn seed_child_item(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
    name: &str,
    parent: Uuid,
) {
    insert_base_item(
        db,
        id,
        kind,
        &ItemRow {
            name,
            parent: Some(parent),
            ..ItemRow::default()
        },
    )
    .await;
}

/// Inserts a named `BaseItems` row stamped with `top_parent` as its
/// `TopParentId` — the column a Jellyfin row carries the *physical* library
/// folder in, and what `Latest` and the by-library scopes filter on.
pub async fn seed_top_parented_item(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
    name: &str,
    top_parent: Uuid,
) {
    insert_base_item(
        db,
        id,
        kind,
        &ItemRow {
            name,
            top_parent: Some(top_parent),
            ..ItemRow::default()
        },
    )
    .await;
}

/// Puts `items` into a library and returns the library's id.
///
/// A query that names no scope is confined to the user's libraries (C#
/// `LibraryManager.AddUserToQuery`), so a fixture that seeds bare items and
/// then queries as a user gets nothing back — correctly, because on a real
/// server every scanned item has a library above it. Pair with
/// [`seed_user_with_defaults`], whose user can actually see it.
///
/// Goes through the real upsert rather than an `UPDATE` of its own, per this
/// module's rule about production write paths.
pub async fn seed_library_over(db: &Database, items: &[Uuid]) -> Uuid {
    let library = Uuid::from_u128(0x0011_B000_01B0);
    seed_named_item(db, library, BaseItemKind::CollectionFolder, "Library").await;

    let mut stamped = Vec::with_capacity(items.len());
    for id in items {
        let mut row = fetch_item(db, *id).await;
        row.top_parent_id = Some(guid_to_db(library));
        stamped.push(row);
    }
    persistence_over(db)
        .save_items(&stamped)
        .await
        .expect("put the seeded items in the library");
    library
}

/// Stamps an item's `Path`, the column the container lookups match on.
///
/// Through the real upsert, per this module's rule about production write
/// paths.
pub async fn set_item_path(db: &Database, id: Uuid, path: &str) {
    let mut row = fetch_item(db, id).await;
    row.path = Some(path.to_owned());
    persistence_over(db)
        .save_items(std::slice::from_ref(&row))
        .await
        .expect("set item path");
}

/// How many rows sit at `path` — one, for a container that was reused rather
/// than duplicated.
pub async fn items_at_path(db: &Database, path: &str) -> usize {
    item_repository_over(db.clone())
        .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
            path: Some(path.to_owned()),
            ..Default::default()
        })
        .await
        .expect("items at path")
        .len()
}

/// Inserts a minimal `BaseItems` row of the given kind.
pub async fn seed_item(db: &Database, id: Uuid, kind: BaseItemKind) {
    seed_named_item(db, id, kind, "").await;
}

/// Inserts an item whose `MediaType` is `"Video"`.
///
/// The column a real scan fills in and `seed_item` leaves NULL. Anything that
/// branches on `BaseItem.MediaType` — the per-user
/// `SupportsTranscoding`/`SupportsDirectStream` overwrite, for one — sees
/// nothing without it.
pub async fn seed_video_item(db: &Database, id: Uuid, kind: BaseItemKind) {
    insert_base_item(
        db,
        id,
        kind,
        &ItemRow {
            media_type: Some("Video"),
            ..ItemRow::default()
        },
    )
    .await;
}

/// Inserts a `BaseItems` row of the given kind with a name.
pub async fn seed_named_item(db: &Database, id: Uuid, kind: BaseItemKind, name: &str) {
    insert_base_item(
        db,
        id,
        kind,
        &ItemRow {
            name,
            ..ItemRow::default()
        },
    )
    .await;
}

/// Inserts a named `BaseItems` row stored as a folder (`IsFolder = 1`),
/// optionally parented to `parent`.
///
/// `IsFolder` is what the DTO/count paths branch on (child counts, played
/// aggregation), and an accessed-by-name row (a genre, or an artist with no
/// folder on disk) is stored as a folder with a `NULL` `ParentId` — pass `None`
/// for that shape.
pub async fn seed_folder_item(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
    name: &str,
    parent: Option<Uuid>,
) {
    insert_base_item(
        db,
        id,
        kind,
        &ItemRow {
            name,
            is_folder: true,
            parent,
            ..ItemRow::default()
        },
    )
    .await;
}

/// Inserts an `Episode` `BaseItems` row with the fields the next-up queries care
/// about (series presentation key, season/episode numbers, virtual flag).
pub async fn seed_episode(
    db: &Database,
    id: Uuid,
    series_key: &str,
    season: i64,
    episode: i64,
    is_virtual: bool,
    top_parent: Option<Uuid>,
) {
    insert_base_item(
        db,
        id,
        BaseItemKind::Episode,
        &ItemRow {
            name: &format!("S{season}E{episode}"),
            is_virtual,
            top_parent,
            series_key: Some(series_key),
            season: Some(season),
            episode: Some(episode),
            ..ItemRow::default()
        },
    )
    .await;
}

/// Writes an item row back through the real upsert — for a fixture that
/// needs columns the seeders do not take (`SeasonId`, `AlbumArtists`, `LUFS`,
/// `OwnerId`, …): `fetch_item`, set the fields, `save_item`.
pub async fn save_item(db: &Database, row: &BaseItemEntity) {
    persistence_over(db)
        .save_items(std::slice::from_ref(row))
        .await
        .expect("save item");
}

/// Makes `alternate` a merged version of `primary` — the `PrimaryVersionId`
/// link the alternate-version reads follow — through the real
/// `set_primary_version_id` write path, so the stored form is exactly what
/// the production writer produces.
pub async fn link_alternate_version(db: &Database, alternate: Uuid, primary: Uuid) {
    use ferrofin_traits::persistence::ItemPersistenceService as _;
    persistence_over(db)
        .set_primary_version_id(alternate, Some(primary))
        .await
        .expect("link the alternate to its primary");
}

/// Clears an episode's `IndexNumber`, so its position inside the season is only
/// half-known — the shape that leaves the next-up sort key incomparable.
pub async fn clear_index_number(db: &Database, id: Uuid) {
    sqlx::query(r#"UPDATE "BaseItems" SET "IndexNumber" = NULL WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .execute(db.writer())
        .await
        .expect("clear index number");
}

/// Sets an item's `CleanName` (the folded, lower-cased name the `name_contains`/
/// name filters match on). `seed_named_item` sets only `Name`; call this when a
/// test exercises a name filter.
pub async fn set_clean_name(db: &Database, id: Uuid, name: &str) {
    let clean = crate::text_util::get_clean_value(name);
    sqlx::query(r#"UPDATE "BaseItems" SET "CleanName" = ?2 WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .bind(clean)
        .execute(db.writer())
        .await
        .expect("set clean name");
}

/// Reads a seeded item row back through the real repository, panicking when the
/// row is missing.
pub async fn fetch_item(db: &Database, id: Uuid) -> BaseItemEntity {
    fetch_item_opt(db, id).await.expect("fetch item")
}

/// Reads a seeded item row back through the real repository, or [`None`] when
/// there is no such row.
pub async fn fetch_item_opt(db: &Database, id: Uuid) -> Option<BaseItemEntity> {
    item_repository_over(db.clone())
        .retrieve_item(id)
        .await
        .expect("retrieve item")
}

/// Attaches a genre value to an item through the `ItemValues`/`ItemValuesMap`
/// tables, so the `genres`/`genre_ids` query filters match it — and materializes
/// the browsable by-name `Genre` row, exactly as the scanner does.
///
/// See [`seed_item_value`] for the append semantics.
pub async fn seed_item_genre(db: &Database, item_id: Uuid, genre: &str) {
    seed_item_value(db, item_id, ferrofin_db::enums::ItemValueType::Genre, genre).await;
}

/// Attaches one `(type, value)` item value (genre / studio / tag / artist /
/// album-artist) to an item, the way the scanner links it.
///
/// Goes through the production write path
/// ([`ItemPersistenceService::save_item_values`]), which *replaces* an item's
/// value links; the item's existing (type, value) pairs are therefore read back
/// and re-sent alongside the new one, so calling this repeatedly appends.
pub async fn seed_item_value(
    db: &Database,
    item_id: Uuid,
    value_type: ferrofin_db::enums::ItemValueType,
    value: &str,
) {
    let mut values: Vec<(i32, String)> = sqlx::query_as(
        r#"SELECT iv."Type", iv."Value"
           FROM "ItemValuesMap" m
           JOIN "ItemValues" iv ON iv."ItemValueId" = m."ItemValueId"
           WHERE m."ItemId" = ?1"#,
    )
    .bind(guid_to_db(item_id))
    .fetch_all(db.pool())
    .await
    .expect("read existing item values");
    values.push((i32::from(value_type), value.to_owned()));

    persistence_over(db)
        .save_item_values(item_id, &values)
        .await
        .expect("save item values");
}

/// An [`ItemImageInfo`] for a fixture image: only the type, path and blurhash
/// vary between tests, the rest is the "dimensions unknown" default.
pub fn image_info(image_type: ImageType, path: &str, blur_hash: Option<&str>) -> ItemImageInfo {
    ItemImageInfo {
        path: path.to_owned(),
        image_type,
        date_modified: DateTime::UNIX_EPOCH,
        width: 0,
        height: 0,
        blur_hash: blur_hash.map(str::to_owned),
    }
}

/// Replaces an item's image rows with `images` (the scanner's write path), so
/// the DTO/image routes resolve tags and blurhashes for them.
pub async fn seed_images(db: &Database, item_id: Uuid, images: &[ItemImageInfo]) {
    persistence_over(db)
        .save_item_images(item_id, images)
        .await
        .expect("save item images");
}

/// Records one external provider id (`BaseItemProviders`) on an item — the row
/// the DTO's `ProviderIds`/`ExternalUrls` are built from.
pub async fn seed_provider_id(db: &Database, item_id: Uuid, provider: &str, value: &str) {
    persistence_over(db)
        .save_provider_id(item_id, provider, value)
        .await
        .expect("save provider id");
}

/// Inserts a minimal `Users` row and returns a [`UserEntity`] carrying its id.
///
/// `Username` is unique, so seeding a second user in one database needs
/// [`seed_named_user`].
pub async fn seed_user(db: &Database, id: Uuid) -> UserEntity {
    seed_named_user(db, id, "u").await
}

/// Gives an already-seeded user the permissions `create_user` would have.
///
/// [`seed_user`] deliberately leaves the `Permissions` table empty — several
/// tests assert on a user who has none, and a missing row reads as `false`. But
/// that also makes such a user maximally *restricted*: without
/// `EnableAllFolders` they can see no library, so any query confined to their
/// libraries comes back empty. Tests that browse as a real user want this.
pub async fn grant_default_permissions(db: &Database, id: Uuid) {
    let mut tx = db.writer().begin().await.expect("begin");
    crate::user_entity_ext::seed_defaults(&mut tx, &guid_to_db(id))
        .await
        .expect("default permissions");
    tx.commit().await.expect("commit");
}

/// [`seed_user`] plus [`grant_default_permissions`] — a user as the real
/// `create_user` makes one.
pub async fn seed_user_with_defaults(db: &Database, id: Uuid) -> UserEntity {
    let user = seed_user(db, id).await;
    grant_default_permissions(db, id).await;
    user
}

/// [`seed_user`] with an explicit `Username`, for tests that need to tell two
/// users apart by name.
pub async fn seed_named_user(db: &Database, id: Uuid, username: &str) -> UserEntity {
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, ?2)"#,
    )
    .bind(guid_to_db(id))
    .bind(username)
    .execute(db.writer())
    .await
    .expect("insert user");

    sqlx::query_as::<_, UserEntity>(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .fetch_one(db.pool())
        .await
        .expect("fetch seeded user")
}

/// Inserts a `UserData` row marking `item` played (or not) by `user` at the
/// given last-played timestamp. The timestamp is bound in the canonical storage
/// format (via [`opt_datetime_to_db`]) so it round-trips (and string-compares)
/// with the values the next-up service binds.
pub async fn seed_user_data(
    db: &Database,
    user: Uuid,
    item: Uuid,
    played: bool,
    last_played: Option<DateTime<Utc>>,
) {
    sqlx::query(
        r#"INSERT INTO "UserData"
           ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "LastPlayedDate",
            "PlayCount", "PlaybackPositionTicks", "Played")
           VALUES (?1, ?2, ?3, 0, ?4, 0, 0, ?5)"#,
    )
    .bind(guid_to_db(item))
    .bind(guid_to_db(user))
    .bind(item.to_string())
    .bind(opt_datetime_to_db(last_played))
    .bind(i64::from(played))
    .execute(db.writer())
    .await
    .expect("insert user data");
}

/// Overwrites a user-data row's `LastPlayedDate` with a value no timestamp
/// decoder accepts, so any query that *selects* the column fails loudly.
///
/// This is the fixture for proving a query is NOT issued: a code path that
/// still succeeds against the corrupted column never read it.
pub async fn corrupt_last_played_date(db: &Database, user: Uuid, item: Uuid) {
    sqlx::query(
        r#"UPDATE "UserData" SET "LastPlayedDate" = 'not-a-timestamp'
           WHERE "UserId" = ?1 AND "ItemId" = ?2"#,
    )
    .bind(guid_to_db(user))
    .bind(guid_to_db(item))
    .execute(db.writer())
    .await
    .expect("corrupt last played date");
}
