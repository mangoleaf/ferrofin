//! [`sqlx::FromRow`] row structs — one struct per head-schema table, grouped by
//! functional area (a module per area).
//!
//! Each struct mirrors exactly one table: its fields map one-to-one onto that
//! table's columns (see `migrations/0001_initial.sql`, which reflects the EF
//! model snapshot). The structs are the raw storage shape; the `From<entity>`
//! conversions into `ferrofin-model` DTOs live alongside the conversion layer.
//!
//! ## Column-to-Rust type conventions
//! - `INTEGER` surrogate primary keys → [`i64`] (SQLite integer affinity).
//! - `TEXT` `Guid` columns → [`String`], the hyphenated stored form
//!   (`Option<String>` where nullable); the conversion layer parses them into
//!   `Uuid`. (SQLite has no native `Guid`, and sqlx's `Uuid` decoder expects a
//!   16-byte `BLOB`, so the faithful storage shape here is the `TEXT` string.)
//! - `TEXT` `DateTime` columns → [`DateTime<Utc>`](chrono::DateTime).
//! - `REAL` columns → [`f64`]; `INTEGER` booleans → [`bool`].
//! - Enum-valued `INTEGER` columns are kept as [`i32`] discriminants here and
//!   mapped onto the [`crate::enums`] / `ferrofin-model` enum types by the
//!   conversion layer.
//! - `RowVersion` optimistic-concurrency tokens (`INTEGER`) → [`i64`].

pub mod activity;
pub mod base_items;
pub mod display_preferences;
pub mod playback;
pub mod security;
pub mod users;

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::base_items::{
        AncestorIdEntity, AttachmentStreamInfoEntity, BaseItemEntity, BaseItemImageInfoEntity,
        BaseItemMetadataFieldEntity, BaseItemProviderEntity, BaseItemTrailerTypeEntity,
        ChapterEntity, ItemValueEntity, ItemValueMapEntity, KeyframeDataEntity, LinkedChildEntity,
        MediaStreamInfoEntity, PeopleBaseItemMapEntity, PeopleEntity,
    };
    use super::display_preferences::{
        CustomItemDisplayPreferencesEntity, DisplayPreferencesEntity, HomeSectionEntity,
        ItemDisplayPreferencesEntity,
    };
    use super::playback::{MediaSegmentEntity, TrickplayInfoEntity, UserDataEntity};
    use super::security::{ApiKeyEntity, DeviceEntity, DeviceOptionsEntity};
    use super::users::{
        AccessScheduleEntity, ActivityLogEntity, ImageInfoEntity, PermissionEntity,
        PreferenceEntity, UserEntity,
    };
    use crate::Database;
    use crate::store::{datetime_to_db, guid_to_db};

    /// A fixed timestamp used across the round-trip fixtures.
    fn instant() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0)
            .single()
            .expect("valid instant")
    }

    /// Inserts a user with the given id so the FK-bearing fixtures resolve.
    async fn insert_user(db: &Database, id: Uuid) {
        sqlx::query(
            r#"INSERT INTO "Users" (
                "Id", "AuthenticationProviderId", "DisplayCollectionsView",
                "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
                "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
                "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
                "MaxActiveSessions", "MustUpdatePassword",
                "PasswordResetProviderId", "PlayDefaultAudioTrack",
                "RememberAudioSelections", "RememberSubtitleSelections", "RowVersion",
                "SubtitleMode", "SyncPlayAccess", "Username"
            ) VALUES (
                ?1, 'auth', 0, 0, 0, 0, 0, 1, 0, 42, 0, 5, 0,
                'reset', 1, 1, 1, 7, 1, 2, 'ada'
            )"#,
        )
        .bind(guid_to_db(id))
        .execute(db.writer())
        .await
        .expect("insert user");
    }

    /// Inserts a minimal `BaseItems` row with the given id so FK-bearing child
    /// and map fixtures resolve.
    async fn insert_base_item(db: &Database, id: Uuid) {
        sqlx::query(
            r#"INSERT INTO "BaseItems" (
                "Id", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
                "IsRepeat", "IsSeries", "IsVirtualItem", "Type"
            ) VALUES (?1, 0, 0, 0, 0, 0, 0, 0, 'Movie')"#,
        )
        .bind(guid_to_db(id))
        .execute(db.writer())
        .await
        .expect("insert base item");
    }

    #[tokio::test]
    async fn user_row_round_trips() {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let id = Uuid::from_u128(1);
        insert_user(&db, id).await;

        let user: UserEntity = sqlx::query_as(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("read user");

        assert_eq!(user.id, guid_to_db(id));
        assert_eq!(user.internal_id, 42);
        assert_eq!(user.max_active_sessions, 5);
        assert_eq!(user.row_version, 7);
        assert_eq!(user.subtitle_mode, 1);
        assert_eq!(user.sync_play_access, 2);
        assert_eq!(user.username, "ada");
        assert!(user.enable_user_preference_access);
        assert!(!user.enable_auto_login);
        assert_eq!(user.password, None);
        assert_eq!(user.last_login_date, None);
    }

    #[tokio::test]
    async fn security_rows_round_trip() {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let user_id = Uuid::from_u128(2);
        insert_user(&db, user_id).await;
        let now = instant();

        sqlx::query(
            r#"INSERT INTO "ApiKeys" ("Id", "AccessToken", "DateCreated",
                "DateLastActivity", "Name") VALUES (1, 'tok', ?1, ?1, 'cli')"#,
        )
        .bind(datetime_to_db(now))
        .execute(db.writer())
        .await
        .expect("insert api key");

        sqlx::query(
            r#"INSERT INTO "Devices" ("Id", "AccessToken", "AppName", "AppVersion",
                "DateCreated", "DateLastActivity", "DateModified", "DeviceId",
                "DeviceName", "IsActive", "UserId")
                VALUES (1, 'atk', 'app', '1.0', ?1, ?1, ?1, 'dev', 'Phone', 1, ?2)"#,
        )
        .bind(datetime_to_db(now))
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("insert device");

        sqlx::query(
            r#"INSERT INTO "DeviceOptions" ("Id", "CustomName", "DeviceId")
                VALUES (1, 'Kitchen', 'dev')"#,
        )
        .execute(db.writer())
        .await
        .expect("insert device options");

        let key: ApiKeyEntity = sqlx::query_as(r#"SELECT * FROM "ApiKeys" WHERE "Id" = 1"#)
            .fetch_one(db.pool())
            .await
            .expect("read api key");
        assert_eq!(key.access_token, "tok");
        assert_eq!(key.date_created, now);

        let device: DeviceEntity = sqlx::query_as(r#"SELECT * FROM "Devices" WHERE "Id" = 1"#)
            .fetch_one(db.pool())
            .await
            .expect("read device");
        assert_eq!(device.user_id, guid_to_db(user_id));
        assert!(device.is_active);
        assert_eq!(device.app_name, "app");

        let options: DeviceOptionsEntity =
            sqlx::query_as(r#"SELECT * FROM "DeviceOptions" WHERE "Id" = 1"#)
                .fetch_one(db.pool())
                .await
                .expect("read device options");
        assert_eq!(options.custom_name.as_deref(), Some("Kitchen"));
    }

    #[tokio::test]
    async fn user_dependent_rows_round_trip() {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let user_id = Uuid::from_u128(3);
        insert_user(&db, user_id).await;
        let now = instant();

        sqlx::query(
            r#"INSERT INTO "AccessSchedules" ("Id", "DayOfWeek", "EndHour",
                "StartHour", "UserId") VALUES (1, 3, 18.5, 8.0, ?1)"#,
        )
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("insert schedule");

        sqlx::query(
            r#"INSERT INTO "Permissions" ("Id", "Kind", "RowVersion", "UserId",
                "Value") VALUES (1, 4, 2, ?1, 1)"#,
        )
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("insert permission");

        sqlx::query(
            r#"INSERT INTO "Preferences" ("Id", "Kind", "RowVersion", "UserId",
                "Value") VALUES (1, 5, 2, ?1, 'a,b,c')"#,
        )
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("insert preference");

        sqlx::query(
            r#"INSERT INTO "ImageInfos" ("Id", "LastModified", "Path", "UserId")
                VALUES (1, ?1, '/img.png', ?2)"#,
        )
        .bind(datetime_to_db(now))
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("insert image info");

        sqlx::query(
            r#"INSERT INTO "ActivityLogs" ("Id", "DateCreated", "LogSeverity",
                "Name", "RowVersion", "Type", "UserId")
                VALUES (1, ?1, 2, 'Login', 1, 'AuthenticationSucceeded', ?2)"#,
        )
        .bind(datetime_to_db(now))
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("insert activity log");

        let schedule: AccessScheduleEntity =
            sqlx::query_as(r#"SELECT * FROM "AccessSchedules" WHERE "Id" = 1"#)
                .fetch_one(db.pool())
                .await
                .expect("read schedule");
        assert_eq!(schedule.day_of_week, 3);
        assert!((schedule.end_hour - 18.5).abs() < f64::EPSILON);
        assert_eq!(schedule.user_id, guid_to_db(user_id));

        let permission: PermissionEntity =
            sqlx::query_as(r#"SELECT * FROM "Permissions" WHERE "Id" = 1"#)
                .fetch_one(db.pool())
                .await
                .expect("read permission");
        assert_eq!(permission.kind, 4);
        assert!(permission.value);
        assert_eq!(permission.user_id, Some(guid_to_db(user_id)));
        assert_eq!(permission.permission_guid, None);

        let preference: PreferenceEntity =
            sqlx::query_as(r#"SELECT * FROM "Preferences" WHERE "Id" = 1"#)
                .fetch_one(db.pool())
                .await
                .expect("read preference");
        assert_eq!(preference.value, "a,b,c");

        let image: ImageInfoEntity = sqlx::query_as(r#"SELECT * FROM "ImageInfos" WHERE "Id" = 1"#)
            .fetch_one(db.pool())
            .await
            .expect("read image info");
        assert_eq!(image.path, "/img.png");
        assert_eq!(image.user_id, Some(guid_to_db(user_id)));

        let log: ActivityLogEntity =
            sqlx::query_as(r#"SELECT * FROM "ActivityLogs" WHERE "Id" = 1"#)
                .fetch_one(db.pool())
                .await
                .expect("read activity log");
        assert_eq!(log.log_severity, 2);
        assert_eq!(log.type_, "AuthenticationSucceeded");
        assert_eq!(log.item_id, None);
        assert_eq!(log.user_id, guid_to_db(user_id));
    }

    #[tokio::test]
    async fn display_preferences_rows_round_trip() {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let user_id = Uuid::from_u128(4);
        insert_user(&db, user_id).await;
        let item_id = Uuid::from_u128(99);

        sqlx::query(
            r#"INSERT INTO "DisplayPreferences" ("Id", "ChromecastVersion", "Client",
                "DashboardTheme", "EnableNextVideoInfoOverlay", "IndexBy", "ItemId",
                "ScrollDirection", "ShowBackdrop", "ShowSidebar", "SkipBackwardLength",
                "SkipForwardLength", "TvHome", "UserId")
                VALUES (1, 2, 'web', 'dark', 1, NULL, ?1, 0, 0, 1, 10, 30, NULL, ?2)"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("insert display preferences");

        sqlx::query(
            r#"INSERT INTO "HomeSection" ("Id", "DisplayPreferencesId", "Order", "Type")
                VALUES (1, 1, 3, 5)"#,
        )
        .execute(db.writer())
        .await
        .expect("insert home section");

        sqlx::query(
            r#"INSERT INTO "ItemDisplayPreferences" ("Id", "Client", "IndexBy", "ItemId",
                "RememberIndexing", "RememberSorting", "SortBy", "SortOrder", "UserId",
                "ViewType")
                VALUES (1, 'web', 2, ?1, 0, 1, 'SortName', 1, ?2, 4)"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("insert item display preferences");

        sqlx::query(
            r#"INSERT INTO "CustomItemDisplayPreferences" ("Id", "Client", "ItemId",
                "Key", "UserId", "Value")
                VALUES (1, 'web', ?1, 'poster', ?2, 'large')"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(user_id))
        .execute(db.writer())
        .await
        .expect("insert custom item display preferences");

        let prefs: DisplayPreferencesEntity =
            sqlx::query_as(r#"SELECT * FROM "DisplayPreferences" WHERE "Id" = 1"#)
                .fetch_one(db.pool())
                .await
                .expect("read display preferences");
        assert_eq!(prefs.chromecast_version, 2);
        assert_eq!(prefs.dashboard_theme.as_deref(), Some("dark"));
        assert!(prefs.enable_next_video_info_overlay);
        assert_eq!(prefs.index_by, None);
        assert_eq!(prefs.item_id, guid_to_db(item_id));
        assert_eq!(prefs.scroll_direction, 0);
        assert!(!prefs.show_backdrop);
        assert!(prefs.show_sidebar);
        assert_eq!(prefs.skip_backward_length, 10);
        assert_eq!(prefs.skip_forward_length, 30);
        assert_eq!(prefs.user_id, guid_to_db(user_id));

        let section: HomeSectionEntity =
            sqlx::query_as(r#"SELECT * FROM "HomeSection" WHERE "Id" = 1"#)
                .fetch_one(db.pool())
                .await
                .expect("read home section");
        assert_eq!(section.display_preferences_id, 1);
        assert_eq!(section.order, 3);
        assert_eq!(section.type_, 5);

        let item_prefs: ItemDisplayPreferencesEntity =
            sqlx::query_as(r#"SELECT * FROM "ItemDisplayPreferences" WHERE "Id" = 1"#)
                .fetch_one(db.pool())
                .await
                .expect("read item display preferences");
        assert_eq!(item_prefs.index_by, Some(2));
        assert!(!item_prefs.remember_indexing);
        assert!(item_prefs.remember_sorting);
        assert_eq!(item_prefs.sort_by, "SortName");
        assert_eq!(item_prefs.sort_order, 1);
        assert_eq!(item_prefs.view_type, 4);

        let custom: CustomItemDisplayPreferencesEntity =
            sqlx::query_as(r#"SELECT * FROM "CustomItemDisplayPreferences" WHERE "Id" = 1"#)
                .fetch_one(db.pool())
                .await
                .expect("read custom item display preferences");
        assert_eq!(custom.key, "poster");
        assert_eq!(custom.value.as_deref(), Some("large"));
        assert_eq!(custom.user_id, guid_to_db(user_id));
    }

    #[tokio::test]
    async fn base_item_row_round_trips() {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let owner = Uuid::from_u128(0x10);
        let id = Uuid::from_u128(0x11);
        insert_base_item(&db, owner).await;
        let now = instant();

        sqlx::query(
            r#"INSERT INTO "BaseItems" (
                "Id", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
                "IsRepeat", "IsSeries", "IsVirtualItem", "Type", "Name", "OwnerId",
                "ParentId", "LUFS", "CommunityRating", "RunTimeTicks", "DateCreated"
            ) VALUES (?1, 0, 0, 0, 1, 0, 0, 0, 'Movie', 'Blade Runner', ?2, ?2,
                -14.0, 8.1, 12000, ?3)"#,
        )
        .bind(guid_to_db(id))
        .bind(guid_to_db(owner))
        .bind(datetime_to_db(now))
        .execute(db.writer())
        .await
        .expect("insert base item");

        let item: BaseItemEntity = sqlx::query_as(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("read base item");
        assert_eq!(item.id, guid_to_db(id));
        assert_eq!(item.name.as_deref(), Some("Blade Runner"));
        assert_eq!(item.type_, "Movie");
        assert_eq!(item.owner_id, Some(guid_to_db(owner)));
        assert_eq!(item.parent_id, Some(guid_to_db(owner)));
        assert!(item.is_movie);
        assert!(!item.is_folder);
        assert_eq!(item.lufs, Some(-14.0));
        assert_eq!(item.run_time_ticks, Some(12000));
        assert_eq!(item.date_created, Some(now));
        assert_eq!(item.album, None);
    }

    #[tokio::test]
    async fn base_item_child_rows_round_trip() {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let item_id = Uuid::from_u128(0x20);
        let parent_id = Uuid::from_u128(0x21);
        insert_base_item(&db, item_id).await;
        insert_base_item(&db, parent_id).await;
        let now = instant();

        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos" ("Id", "Blurhash", "DateModified",
                "Height", "ImageType", "ItemId", "Path", "Width")
                VALUES (?1, ?2, ?3, 1080, 0, ?4, '/poster.jpg', 1920)"#,
        )
        .bind(guid_to_db(Uuid::from_u128(0x22)))
        .bind(vec![1u8, 2, 3])
        .bind(datetime_to_db(now))
        .bind(guid_to_db(item_id))
        .execute(db.writer())
        .await
        .expect("insert image info");

        sqlx::query(r#"INSERT INTO "BaseItemMetadataFields" ("Id", "ItemId") VALUES (3, ?1)"#)
            .bind(guid_to_db(item_id))
            .execute(db.writer())
            .await
            .expect("insert metadata field");

        sqlx::query(
            r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue")
                VALUES (?1, 'Imdb', 'tt0083658')"#,
        )
        .bind(guid_to_db(item_id))
        .execute(db.writer())
        .await
        .expect("insert provider");

        sqlx::query(r#"INSERT INTO "BaseItemTrailerTypes" ("Id", "ItemId") VALUES (1, ?1)"#)
            .bind(guid_to_db(item_id))
            .execute(db.writer())
            .await
            .expect("insert trailer type");

        sqlx::query(
            r#"INSERT INTO "Chapters" ("ItemId", "ChapterIndex", "Name",
                "StartPositionTicks") VALUES (?1, 0, 'Opening', 0)"#,
        )
        .bind(guid_to_db(item_id))
        .execute(db.writer())
        .await
        .expect("insert chapter");

        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(item_id))
            .bind(guid_to_db(parent_id))
            .execute(db.writer())
            .await
            .expect("insert ancestor id");

        let image: BaseItemImageInfoEntity =
            sqlx::query_as(r#"SELECT * FROM "BaseItemImageInfos" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read image info");
        assert_eq!(image.blurhash, Some(vec![1u8, 2, 3]));
        assert_eq!(image.height, 1080);
        assert_eq!(image.image_type, 0);
        assert_eq!(image.item_id, guid_to_db(item_id));

        let field: BaseItemMetadataFieldEntity =
            sqlx::query_as(r#"SELECT * FROM "BaseItemMetadataFields" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read metadata field");
        assert_eq!(field.id, 3);

        let provider: BaseItemProviderEntity =
            sqlx::query_as(r#"SELECT * FROM "BaseItemProviders" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read provider");
        assert_eq!(provider.provider_id, "Imdb");
        assert_eq!(provider.provider_value, "tt0083658");

        let trailer: BaseItemTrailerTypeEntity =
            sqlx::query_as(r#"SELECT * FROM "BaseItemTrailerTypes" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read trailer type");
        assert_eq!(trailer.id, 1);

        let chapter: ChapterEntity =
            sqlx::query_as(r#"SELECT * FROM "Chapters" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read chapter");
        assert_eq!(chapter.name.as_deref(), Some("Opening"));
        assert_eq!(chapter.chapter_index, 0);

        let ancestor: AncestorIdEntity =
            sqlx::query_as(r#"SELECT * FROM "AncestorIds" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read ancestor id");
        assert_eq!(ancestor.parent_item_id, guid_to_db(parent_id));
    }

    #[tokio::test]
    async fn item_value_and_people_maps_round_trip() {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let item_id = Uuid::from_u128(0x30);
        let value_id = Uuid::from_u128(0x31);
        let people_id = Uuid::from_u128(0x32);
        insert_base_item(&db, item_id).await;

        sqlx::query(
            r#"INSERT INTO "ItemValues" ("ItemValueId", "CleanValue", "Type", "Value")
                VALUES (?1, 'action', 2, 'Action')"#,
        )
        .bind(guid_to_db(value_id))
        .execute(db.writer())
        .await
        .expect("insert item value");

        sqlx::query(r#"INSERT INTO "ItemValuesMap" ("ItemValueId", "ItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(value_id))
            .bind(guid_to_db(item_id))
            .execute(db.writer())
            .await
            .expect("insert item value map");

        sqlx::query(
            r#"INSERT INTO "Peoples" ("Id", "Name", "PersonType")
                VALUES (?1, 'Harrison Ford', 'Actor')"#,
        )
        .bind(guid_to_db(people_id))
        .execute(db.writer())
        .await
        .expect("insert people");

        sqlx::query(
            r#"INSERT INTO "PeopleBaseItemMap" ("ItemId", "PeopleId", "Role",
                "ListOrder", "SortOrder") VALUES (?1, ?2, 'Deckard', 0, 0)"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(people_id))
        .execute(db.writer())
        .await
        .expect("insert people map");

        let value: ItemValueEntity =
            sqlx::query_as(r#"SELECT * FROM "ItemValues" WHERE "ItemValueId" = ?1"#)
                .bind(guid_to_db(value_id))
                .fetch_one(db.pool())
                .await
                .expect("read item value");
        assert_eq!(value.type_, 2);
        assert_eq!(value.value, "Action");
        assert_eq!(value.clean_value, "action");

        let map: ItemValueMapEntity =
            sqlx::query_as(r#"SELECT * FROM "ItemValuesMap" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read item value map");
        assert_eq!(map.item_value_id, guid_to_db(value_id));

        let person: PeopleEntity = sqlx::query_as(r#"SELECT * FROM "Peoples" WHERE "Id" = ?1"#)
            .bind(guid_to_db(people_id))
            .fetch_one(db.pool())
            .await
            .expect("read people");
        assert_eq!(person.name, "Harrison Ford");
        assert_eq!(person.person_type.as_deref(), Some("Actor"));

        let credit: PeopleBaseItemMapEntity =
            sqlx::query_as(r#"SELECT * FROM "PeopleBaseItemMap" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read people map");
        assert_eq!(credit.role, "Deckard");
        assert_eq!(credit.list_order, Some(0));
        assert_eq!(credit.people_id, guid_to_db(people_id));
    }

    #[tokio::test]
    async fn stream_and_keyframe_rows_round_trip() {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let item_id = Uuid::from_u128(0x40);
        let child_id = Uuid::from_u128(0x41);
        insert_base_item(&db, item_id).await;
        insert_base_item(&db, child_id).await;

        sqlx::query(
            r#"INSERT INTO "FerrofinLinkedChildren" ("ParentId", "ChildId", "ChildType",
                "SortOrder") VALUES (?1, ?2, 1, 5)"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(child_id))
        .execute(db.writer())
        .await
        .expect("insert linked child");

        sqlx::query(
            r#"INSERT INTO "AttachmentStreamInfos" ("ItemId", "Index", "Codec",
                "Filename", "MimeType") VALUES (?1, 0, 'ttf', 'font.ttf', 'font/ttf')"#,
        )
        .bind(guid_to_db(item_id))
        .execute(db.writer())
        .await
        .expect("insert attachment");

        sqlx::query(
            r#"INSERT INTO "MediaStreamInfos" ("ItemId", "StreamIndex", "Codec",
                "IsDefault", "IsExternal", "IsForced", "StreamType",
                "Width", "Height", "IsAvc", "AverageFrameRate", "Level")
                VALUES (?1, 0, 'h264', 1, 0, 0, 1, 1920, 1080, 1, 23.976, 4.1)"#,
        )
        .bind(guid_to_db(item_id))
        .execute(db.writer())
        .await
        .expect("insert media stream");

        sqlx::query(
            r#"INSERT INTO "KeyframeData" ("ItemId", "KeyframeTicks", "TotalDuration")
                VALUES (?1, '[0,10000,20000]', 30000)"#,
        )
        .bind(guid_to_db(item_id))
        .execute(db.writer())
        .await
        .expect("insert keyframe data");

        let link: LinkedChildEntity =
            sqlx::query_as(r#"SELECT * FROM "FerrofinLinkedChildren" WHERE "ParentId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read linked child");
        assert_eq!(link.child_id, guid_to_db(child_id));
        assert_eq!(link.child_type, 1);
        assert_eq!(link.sort_order, Some(5));

        let attachment: AttachmentStreamInfoEntity =
            sqlx::query_as(r#"SELECT * FROM "AttachmentStreamInfos" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read attachment");
        assert_eq!(attachment.index, 0);
        assert_eq!(attachment.codec.as_deref(), Some("ttf"));
        assert_eq!(attachment.mime_type.as_deref(), Some("font/ttf"));

        let stream: MediaStreamInfoEntity =
            sqlx::query_as(r#"SELECT * FROM "MediaStreamInfos" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read media stream");
        assert_eq!(stream.stream_type, 1);
        assert_eq!(stream.codec.as_deref(), Some("h264"));
        assert_eq!(stream.width, Some(1920));
        assert!(stream.is_default);
        assert!(!stream.is_external);
        assert_eq!(stream.is_avc, Some(true));
        assert_eq!(stream.bl_present_flag, None);
        assert_eq!(stream.average_frame_rate, Some(23.976));
        assert_eq!(stream.level, Some(4.1));

        let keyframes: KeyframeDataEntity =
            sqlx::query_as(r#"SELECT * FROM "KeyframeData" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read keyframe data");
        assert_eq!(keyframes.keyframe_ticks.as_deref(), Some("[0,10000,20000]"));
        assert_eq!(keyframes.total_duration, 30000);
    }

    #[tokio::test]
    async fn playback_leaf_rows_round_trip() {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let item_id = Uuid::from_u128(0x50);
        let user_id = Uuid::from_u128(0x51);
        let segment_id = Uuid::from_u128(0x52);
        insert_base_item(&db, item_id).await;
        insert_user(&db, user_id).await;
        let now = instant();

        sqlx::query(
            r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey",
                "AudioStreamIndex", "IsFavorite", "LastPlayedDate", "Likes",
                "PlayCount", "PlaybackPositionTicks", "Played", "Rating",
                "RetentionDate", "SubtitleStreamIndex")
                VALUES (?1, ?2, 'default', 2, 1, ?3, 1, 4, 987654, 1, 9.5, NULL, NULL)"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(user_id))
        .bind(datetime_to_db(now))
        .execute(db.writer())
        .await
        .expect("insert user data");

        sqlx::query(
            r#"INSERT INTO "TrickplayInfos" ("ItemId", "Width", "Bandwidth",
                "Height", "Interval", "ThumbnailCount", "TileHeight", "TileWidth")
                VALUES (?1, 320, 500000, 180, 10000, 240, 10, 10)"#,
        )
        .bind(guid_to_db(item_id))
        .execute(db.writer())
        .await
        .expect("insert trickplay info");

        sqlx::query(
            r#"INSERT INTO "MediaSegments" ("Id", "EndTicks", "ItemId",
                "SegmentProviderId", "StartTicks", "Type")
                VALUES (?1, 6000000, ?2, 'chapter-provider', 0, 1)"#,
        )
        .bind(guid_to_db(segment_id))
        .bind(guid_to_db(item_id))
        .execute(db.writer())
        .await
        .expect("insert media segment");

        let data: UserDataEntity = sqlx::query_as(
            r#"SELECT * FROM "UserData" WHERE "ItemId" = ?1 AND "UserId" = ?2
                AND "CustomDataKey" = 'default'"#,
        )
        .bind(guid_to_db(item_id))
        .bind(guid_to_db(user_id))
        .fetch_one(db.pool())
        .await
        .expect("read user data");
        assert_eq!(data.custom_data_key, "default");
        assert_eq!(data.audio_stream_index, Some(2));
        assert!(data.is_favorite);
        assert_eq!(data.last_played_date, Some(now));
        assert_eq!(data.likes, Some(true));
        assert_eq!(data.play_count, 4);
        assert_eq!(data.playback_position_ticks, 987_654);
        assert!(data.played);
        assert_eq!(data.rating, Some(9.5));
        assert_eq!(data.retention_date, None);
        assert_eq!(data.subtitle_stream_index, None);

        let trickplay: TrickplayInfoEntity =
            sqlx::query_as(r#"SELECT * FROM "TrickplayInfos" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item_id))
                .fetch_one(db.pool())
                .await
                .expect("read trickplay info");
        assert_eq!(trickplay.width, 320);
        assert_eq!(trickplay.bandwidth, 500_000);
        assert_eq!(trickplay.height, 180);
        assert_eq!(trickplay.interval, 10_000);
        assert_eq!(trickplay.thumbnail_count, 240);
        assert_eq!(trickplay.tile_height, 10);
        assert_eq!(trickplay.tile_width, 10);

        let segment: MediaSegmentEntity =
            sqlx::query_as(r#"SELECT * FROM "MediaSegments" WHERE "Id" = ?1"#)
                .bind(guid_to_db(segment_id))
                .fetch_one(db.pool())
                .await
                .expect("read media segment");
        assert_eq!(segment.item_id, guid_to_db(item_id));
        assert_eq!(segment.end_ticks, 6_000_000);
        assert_eq!(segment.start_ticks, 0);
        assert_eq!(segment.segment_provider_id, "chapter-provider");
        assert_eq!(segment.type_, 1);
    }
}
