//! A played mark on a container reads and writes its members a chunk at a
//! time, not one transaction per member.
//!
//! `Folder.MarkPlayed`/`MarkUnplayed` save each member through
//! `SaveUserData`. Ferrofin did that in one `BEGIN IMMEDIATE` transaction per
//! member, each with its own key read, row read and version check, so marking
//! a 1,000-episode series ran about 8,000 statements in one request
//! (gl-1321). The members now share one transaction, one key read, one row
//! read and one version check per chunk, and only their writes stay per
//! member.
//!
//! This binary records the statements one call runs ([`sql_statements`]).

mod sql_statements;

use std::sync::Arc;

use ferrofin_core::{
    FerrofinItemCountService, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinLibraryManager, FerrofinPeopleRepository, FerrofinServerApplicationPaths,
    FerrofinServerConfigurationManager, FerrofinUserDataManager, ItemTypeLookup,
    LocalizationManager,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::library::{LibraryManager, UserDataManager};
use ferrofin_traits::persistence::{ItemCountService, ItemPersistenceService as _, ItemRepository};
use sql_statements::{record_statements, statements_of};
use uuid::Uuid;

/// How many episodes the series holds: enough that one read per episode
/// cannot pass for one per mark.
const EPISODES: usize = 40;

/// Inserts the minimal `Users` row the user-data foreign key needs.
async fn seed_user(db: &Database, id: Uuid) {
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username", "NormalizedUsername")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'viewer', 'VIEWER')"#,
    )
    .bind(guid_to_db(id))
    .execute(db.writer())
    .await
    .expect("insert user");
}

/// A series with a TVDB id, its season 1 and `episodes` under it, stored as a
/// scan stores them.
async fn seed_show(db: &Database, series: Uuid, season: Uuid, episodes: &[Uuid]) {
    let key = format!("show-{series}");
    let mut rows = vec![
        BaseItemEntity {
            id: guid_to_db(series),
            type_: "MediaBrowser.Controller.Entities.TV.Series".into(),
            name: Some("Show".into()),
            is_folder: true,
            presentation_unique_key: Some(key.clone()),
            ..Default::default()
        },
        BaseItemEntity {
            id: guid_to_db(season),
            type_: "MediaBrowser.Controller.Entities.TV.Season".into(),
            name: Some("Season 1".into()),
            index_number: Some(1),
            parent_id: Some(guid_to_db(series)),
            series_id: Some(guid_to_db(series)),
            series_presentation_unique_key: Some(key.clone()),
            presentation_unique_key: Some(format!("{key}-season-1")),
            is_folder: true,
            ..Default::default()
        },
    ];
    for (number, &id) in (1..).zip(episodes) {
        rows.push(BaseItemEntity {
            id: guid_to_db(id),
            type_: "MediaBrowser.Controller.Entities.TV.Episode".into(),
            name: Some(format!("Episode {number}")),
            parent_id: Some(guid_to_db(season)),
            season_id: Some(guid_to_db(season)),
            series_id: Some(guid_to_db(series)),
            series_presentation_unique_key: Some(key.clone()),
            parent_index_number: Some(1),
            index_number: Some(number),
            media_type: Some("Video".into()),
            path: Some(format!("/media/show/s01e{number:03}.mkv")),
            ..Default::default()
        });
    }
    let persistence = FerrofinItemPersistenceService::new(db.clone());
    persistence.save_items(&rows).await.expect("save items");
    persistence
        .set_ancestors(season, &[series])
        .await
        .expect("season ancestors");
    for &id in episodes {
        persistence
            .set_ancestors(id, &[season, series])
            .await
            .expect("episode ancestors");
    }
    sqlx::query(
        r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue")
           VALUES (?1, 'Tvdb', '73739')"#,
    )
    .bind(guid_to_db(series))
    .execute(db.writer())
    .await
    .expect("insert provider id");
}

/// The statements of `statements` whose text contains `needle`.
fn count(statements: &[String], needle: &str) -> usize {
    statements.iter().filter(|s| s.contains(needle)).count()
}

/// Marking a series played and then unplayed saves every episode under each
/// of its keys, and reads the episodes' keys, rows and versions once for the
/// whole series, not once per episode.
#[tokio::test]
async fn a_series_mark_reads_its_episodes_together() {
    record_statements();
    let root = tempfile::tempdir().expect("tempdir");
    let paths = Arc::new(FerrofinServerApplicationPaths::new(
        root.path().join("program"),
        root.path().join("log"),
        root.path().join("config"),
        root.path().join("cache"),
        root.path().join("web"),
    ));
    let config: Arc<dyn ServerConfigurationManager> = Arc::new(
        FerrofinServerConfigurationManager::load(paths)
            .await
            .expect("configuration"),
    );
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    let user = Uuid::new_v4();
    seed_user(&db, user).await;
    let (series, season) = (Uuid::new_v4(), Uuid::new_v4());
    let episodes: Vec<Uuid> = (0..EPISODES).map(|_| Uuid::new_v4()).collect();
    seed_show(&db, series, season, &episodes).await;

    let counts: Arc<dyn ItemCountService> = Arc::new(FerrofinItemCountService::new(db.clone()));
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let visibility = Arc::new(ferrofin_core::item_visibility::ItemVisibility::new(
        db.clone(),
        Arc::clone(&items),
        Arc::new(LocalizationManager::new("US")),
        root.path().join("data").display().to_string(),
    ));
    let library: Arc<dyn LibraryManager> = Arc::new(
        FerrofinLibraryManager::new(
            items,
            Arc::clone(&counts),
            Arc::new(FerrofinItemPersistenceService::new(db.clone())),
            Arc::new(FerrofinPeopleRepository::new(db.clone())),
        )
        .with_dashboard_configuration(Arc::clone(&config))
        .with_visibility(visibility),
    );
    let manager = FerrofinUserDataManager::new(db.clone(), config);
    manager.set_container_members(Arc::downgrade(&library), counts);

    for played in [true, false] {
        let (dto, statements) = statements_of(async {
            if played {
                manager.mark_played(user, series, None).await
            } else {
                manager.mark_unplayed(user, series).await
            }
        })
        .await;
        let dto = dto.expect("mark");
        assert_eq!(dto.played, played, "the series' rolled-up state");
        assert_eq!(
            count(&statements, r#"INSERT INTO "UserData""#),
            2 * EPISODES,
            "each episode under its series-derived key and its own id"
        );
        assert_eq!(
            count(&statements, r#"UPDATE "UserData" SET "AudioStreamIndex""#),
            EPISODES,
            "each episode's retired keys"
        );
        assert_eq!(count(&statements, "identity_ids"), 1, "key reads");
        assert_eq!(
            count(&statements, r#"x."PrimaryVersionId" = b."Id""#),
            1,
            "version checks"
        );
        assert_eq!(
            count(&statements, r#"SELECT * FROM "UserData""#),
            2,
            "the episodes' rows, then the series' own for the answer"
        );
        for (number, &episode) in (1..).zip(&episodes) {
            let data = manager
                .get_user_data_dto(episode, user)
                .await
                .expect("read")
                .expect("a row");
            assert_eq!(data.played, played, "episode {number}");
            assert_eq!(data.key, format!("73739001{number:03}"), "episode {number}");
        }
    }
}
