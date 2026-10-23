//! `DisableLiveTvChannelUserDataName` reaches a Live TV channel's user-data
//! keys (`LiveTvChannel.GetUserDataKeys`). This is its own test binary because
//! the setting is published process-wide: no other test shares it.

use std::sync::Arc;

use ferrofin_core::{
    FerrofinServerApplicationPaths, FerrofinServerConfigurationManager, FerrofinUserDataManager,
};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::UpdateUserItemDataDto;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::library::UserDataManager;
use uuid::Uuid;

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

/// Inserts a Live TV channel row named `name`.
async fn seed_channel(db: &Database, id: Uuid, name: &str) {
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Name")
           VALUES (?1, ?2, 0, 0, 0, 0, 0, 0, 0, ?3)"#,
    )
    .bind(guid_to_db(id))
    .bind(BaseItemKind::TvChannel.stored_type_name())
    .bind(name)
    .execute(db.writer())
    .await
    .expect("insert channel");
}

/// `keys`, sorted, to compare with [`favourite_keys`].
fn sorted(mut keys: Vec<String>) -> Vec<String> {
    keys.sort();
    keys
}

/// Saves `user`'s favourite flag on `channel`.
async fn set_favourite(
    manager: &FerrofinUserDataManager,
    user: Uuid,
    channel: Uuid,
    is_favorite: bool,
) {
    let update = UpdateUserItemDataDto {
        is_favorite: Some(is_favorite),
        ..UpdateUserItemDataDto::default()
    };
    manager
        .save_user_data(user, channel, &update)
        .await
        .expect("save favourite");
}

/// `user`'s rows on `channel`: `(CustomDataKey, IsFavorite)`, sorted.
async fn rows(db: &Database, user: Uuid, channel: Uuid) -> Vec<(String, bool)> {
    sqlx::query_as(
        r#"SELECT "CustomDataKey", "IsFavorite" FROM "UserData"
           WHERE "ItemId" = ?1 AND "UserId" = ?2 ORDER BY "CustomDataKey""#,
    )
    .bind(guid_to_db(channel))
    .bind(guid_to_db(user))
    .fetch_all(db.pool())
    .await
    .expect("user data rows")
}

/// The `CustomDataKey`s a favourite on `channel` was saved under, sorted.
async fn favourite_keys(
    db: &Database,
    manager: &FerrofinUserDataManager,
    user: Uuid,
    channel: Uuid,
) -> Vec<String> {
    set_favourite(manager, user, channel, true).await;
    rows(db, user, channel)
        .await
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

/// The `UserData` a client is served for `channel`: `(Key, IsFavorite)`.
async fn served(manager: &FerrofinUserDataManager, user: Uuid, channel: Uuid) -> (String, bool) {
    let dto = manager
        .get_user_data_dto(channel, user)
        .await
        .expect("user data")
        .expect("a row");
    (dto.key, dto.is_favorite)
}

/// Application paths for a server rooted at `root`.
fn paths_in(root: &std::path::Path) -> Arc<FerrofinServerApplicationPaths> {
    Arc::new(FerrofinServerApplicationPaths::new(
        root.join("program"),
        root.join("log"),
        root.join("config"),
        root.join("cache"),
        root.join("web"),
    ))
}

#[tokio::test]
async fn the_saved_setting_decides_whether_a_channel_is_keyed_by_its_name() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths_in(root.path());
    let config = Arc::new(
        FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .unwrap(),
    );
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    let user = Uuid::new_v4();
    seed_user(&db, user).await;
    let [stock, named, stock_again, restarted, renamed] = [(); 5].map(|()| Uuid::new_v4());
    for (channel, name) in [
        (stock, "BBC One"),
        (named, "BBC Two"),
        (stock_again, "BBC Four"),
        (restarted, "Ça Télé"),
        (renamed, "BBC Three"),
    ] {
        seed_channel(&db, channel, name).await;
    }
    let manager = FerrofinUserDataManager::new(
        db.clone(),
        Arc::clone(&config) as Arc<dyn ServerConfigurationManager>,
    );

    // The default (`true`): a channel is keyed by its id alone.
    assert_eq!(
        favourite_keys(&db, &manager, user, stock).await,
        vec![stock.to_string()]
    );

    // Off: `TvChannel-<Name>` leads, and the id key stays after it.
    let mut configuration = (*config.configuration().await.unwrap()).clone();
    configuration.disable_live_tv_channel_user_data_name = false;
    config.update_configuration(&configuration).await.unwrap();
    assert_eq!(
        favourite_keys(&db, &manager, user, named).await,
        sorted(vec!["TvChannel-BBC Two".to_owned(), named.to_string()])
    );
    assert_eq!(
        served(&manager, user, named).await,
        ("TvChannel-BBC Two".to_owned(), true)
    );

    // A favourite saved before the switch, under the id alone, is still found
    // through the id key, and the next save writes both keys.
    assert_eq!(
        served(&manager, user, stock).await,
        (stock.to_string(), true)
    );
    set_favourite(&manager, user, stock, false).await;
    assert_eq!(
        rows(&db, user, stock).await,
        sorted(vec!["TvChannel-BBC One".to_owned(), stock.to_string()])
            .into_iter()
            .map(|key| (key, false))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        served(&manager, user, stock).await,
        ("TvChannel-BBC One".to_owned(), false)
    );

    // Loading a stock configuration publishes its `true`...
    let stock_root = tempfile::tempdir().unwrap();
    FerrofinServerConfigurationManager::load(paths_in(stock_root.path()))
        .await
        .unwrap();
    assert_eq!(
        favourite_keys(&db, &manager, user, stock_again).await,
        vec![stock_again.to_string()]
    );

    // ...and a restart on the saved one publishes its `false` again. The name
    // is used as it is, with its diacritics (upstream concatenates the raw
    // `Name`).
    let reloaded = FerrofinServerConfigurationManager::load(paths)
        .await
        .unwrap();
    assert_eq!(
        favourite_keys(&db, &manager, user, restarted).await,
        sorted(vec!["TvChannel-Ça Télé".to_owned(), restarted.to_string()])
    );

    // Back on: the name key is no longer derived.
    configuration.disable_live_tv_channel_user_data_name = true;
    reloaded.update_configuration(&configuration).await.unwrap();
    assert_eq!(
        favourite_keys(&db, &manager, user, renamed).await,
        vec![renamed.to_string()]
    );
}
