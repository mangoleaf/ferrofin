//! Shared test fixtures for the repository/service unit tests.
//!
//! These helpers seed a migrated in-memory `hermit-db` so each repository test
//! can insert exactly the rows it needs (an item, a user, user-data) without
//! duplicating the verbose `INSERT` boilerplate across modules. Items are seeded
//! with their real stored `Type` name (via
//! [`stored_type_name`](crate::item_type_lookup::stored_type_name)) so tests that
//! filter on kind behave like production. Compiled only under `cfg(test)`.

#![cfg(test)]

use std::sync::Arc;

use chrono::{DateTime, Utc};
use hermit_db::Database;
use hermit_db::entities::users::UserEntity;
use hermit_model::data::BaseItemKind;
use uuid::Uuid;

use crate::item_type_lookup::stored_type_name;

/// Builds a [`HermitLibraryManager`](crate::library_manager::HermitLibraryManager)
/// backed by the real unit-1/2 repositories over the given database, for
/// manager tests that need to resolve item kinds/rows through the library seam.
pub fn library_manager_over(db: Database) -> Arc<dyn hermit_traits::library::LibraryManager> {
    use crate::item_count_service::HermitItemCountService;
    use crate::item_persistence_service::HermitItemPersistenceService;
    use crate::item_repository::HermitItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::people_repository::HermitPeopleRepository;

    let lookup: Arc<dyn hermit_traits::persistence::ItemTypeLookup> =
        Arc::new(ItemTypeLookup::new());
    Arc::new(crate::library_manager::HermitLibraryManager::new(
        Arc::new(HermitItemRepository::new(db.clone(), lookup)),
        Arc::new(HermitItemCountService::new(db.clone())),
        Arc::new(HermitItemPersistenceService::new(db.clone())),
        Arc::new(HermitPeopleRepository::new(db)),
    ))
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

/// Inserts a minimal `BaseItems` row of the given kind.
pub async fn seed_item(db: &Database, id: Uuid, kind: BaseItemKind) {
    seed_named_item(db, id, kind, "").await;
}

/// Inserts a `BaseItems` row of the given kind with a name.
pub async fn seed_named_item(db: &Database, id: Uuid, kind: BaseItemKind, name: &str) {
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Name")
           VALUES (?1, ?2, 0, 0, 0, 0, 0, 0, 0, ?3)"#,
    )
    .bind(id.to_string())
    .bind(type_name(kind))
    .bind(if name.is_empty() { None } else { Some(name) })
    .execute(db.pool())
    .await
    .expect("insert item");
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
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem",
            "SeriesPresentationUniqueKey", "ParentIndexNumber", "IndexNumber",
            "TopParentId", "Name")
           VALUES (?1, ?2, 0, 0, 0, 0, 0, 0, ?3, ?4, ?5, ?6, ?7, ?8)"#,
    )
    .bind(id.to_string())
    .bind(type_name(BaseItemKind::Episode))
    .bind(i64::from(is_virtual))
    .bind(series_key)
    .bind(season)
    .bind(episode)
    .bind(top_parent.map(|t| t.to_string()))
    .bind(format!("S{season}E{episode}"))
    .execute(db.pool())
    .await
    .expect("insert episode");
}

/// Sets an item's `CleanName` (the folded, lower-cased name the `name_contains`/
/// name filters match on). `seed_named_item` sets only `Name`; call this when a
/// test exercises a name filter.
pub async fn set_clean_name(db: &Database, id: Uuid, name: &str) {
    let clean = crate::text_util::get_clean_value(name);
    sqlx::query(r#"UPDATE "BaseItems" SET "CleanName" = ?2 WHERE "Id" = ?1"#)
        .bind(id.to_string())
        .bind(clean)
        .execute(db.pool())
        .await
        .expect("set clean name");
}

/// Attaches a genre value to an item through the `ItemValues`/`ItemValuesMap`
/// tables, so the `genres`/`genre_ids` query filters match it. Mirrors how the
/// scanner records an item's genres. The `ItemValues.Type` for a genre is `2`
/// ([`ItemValueType::Genre`](hermit_db::enums::ItemValueType::Genre)).
pub async fn seed_item_genre(db: &Database, item_id: Uuid, genre: &str) {
    let clean = crate::text_util::get_clean_value(genre);
    let genre_type = i64::from(i32::from(hermit_db::enums::ItemValueType::Genre));

    // Reuse an existing value row of this (type, clean) pair, or allocate a fresh
    // id — the `ItemValueId` PK is a TEXT (guid) column, not auto-incremented.
    let existing: Option<String> = sqlx::query_scalar(
        r#"SELECT "ItemValueId" FROM "ItemValues"
           WHERE "Type" = ?1 AND "CleanValue" = ?2 LIMIT 1"#,
    )
    .bind(genre_type)
    .bind(&clean)
    .fetch_optional(db.pool())
    .await
    .expect("read item value id");

    let value_id = if let Some(id) = existing {
        id
    } else {
        let new_id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"INSERT INTO "ItemValues" ("ItemValueId", "Type", "Value", "CleanValue")
               VALUES (?1, ?2, ?3, ?4)"#,
        )
        .bind(&new_id)
        .bind(genre_type)
        .bind(genre)
        .bind(&clean)
        .execute(db.pool())
        .await
        .expect("insert item value");
        new_id
    };

    sqlx::query(
        r#"INSERT INTO "ItemValuesMap" ("ItemId", "ItemValueId")
           VALUES (?1, ?2) ON CONFLICT DO NOTHING"#,
    )
    .bind(item_id.to_string())
    .bind(value_id)
    .execute(db.pool())
    .await
    .expect("map item value");
}

/// Inserts a minimal `Users` row and returns a [`UserEntity`] carrying its id.
pub async fn seed_user(db: &Database, id: Uuid) -> UserEntity {
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword", "NormalizedUsername",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 'U', '', 1, 1, 1, 0, 0, 0, 'u')"#,
    )
    .bind(id.to_string())
    .execute(db.pool())
    .await
    .expect("insert user");

    sqlx::query_as::<_, UserEntity>(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
        .bind(id.to_string())
        .fetch_one(db.pool())
        .await
        .expect("fetch seeded user")
}

/// Inserts a `UserData` row marking `item` played (or not) by `user` at the
/// given last-played timestamp. The timestamp is bound as a real
/// [`DateTime<Utc>`] so it round-trips (and string-compares) with the values the
/// next-up service binds.
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
    .bind(item.to_string())
    .bind(user.to_string())
    .bind(item.to_string())
    .bind(last_played)
    .bind(i64::from(played))
    .execute(db.pool())
    .await
    .expect("insert user data");
}
