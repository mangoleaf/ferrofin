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
}

/// The one `BaseItems` insert every item fixture goes through.
async fn insert_base_item(db: &Database, id: Uuid, kind: BaseItemKind, row: &ItemRow<'_>) {
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Name", "ParentId",
            "TopParentId", "SeriesPresentationUniqueKey", "ParentIndexNumber",
            "IndexNumber")
           VALUES (?1, ?2, ?3, 0, 0, 0, 0, 0, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
    )
    .bind(guid_to_db(id))
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
    .execute(db.writer())
    .await
    .expect("insert item");
}

/// Inserts a minimal `BaseItems` row of the given kind.
pub async fn seed_item(db: &Database, id: Uuid, kind: BaseItemKind) {
    seed_named_item(db, id, kind, "").await;
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
/// Goes through the production write path
/// ([`ItemPersistenceService::save_item_values`]), which *replaces* an item's
/// value links; the item's existing (type, value) pairs are therefore read back
/// and re-sent alongside the new genre, so calling this repeatedly appends.
pub async fn seed_item_genre(db: &Database, item_id: Uuid, genre: &str) {
    let genre_type = i32::from(ferrofin_db::enums::ItemValueType::Genre);

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
    values.push((genre_type, genre.to_owned()));

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
pub async fn seed_user(db: &Database, id: Uuid) -> UserEntity {
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
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'u')"#,
    )
    .bind(guid_to_db(id))
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
