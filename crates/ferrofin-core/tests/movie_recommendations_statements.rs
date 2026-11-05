//! A movie-recommendations request resolves its user and the user's library
//! access once, not once per baseline.
//!
//! `/Movies/Recommendations` builds a "similar to" category per recently
//! played and liked movie, and each category's access filter is scoped to the
//! user (C# `ConfigureUserAccess`). The scoping used to re-read the `Users` row
//! and the user's permissions and preferences, uncached, for every baseline
//! (gl-1263). The request now reads the user once and its policy from the
//! cross-request cache.
//!
//! This binary records the statements one call runs ([`sql_statements`]).

mod sql_statements;

use std::sync::Arc;

use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinSimilarItemsManager,
    ItemTypeLookup,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::enums::{ItemValueType, PermissionKind};
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::RecommendationType;
use ferrofin_traits::library::SimilarItemsManager;
use ferrofin_traits::options::DtoOptions;
use ferrofin_traits::persistence::ItemPersistenceService;
use sql_statements::{record_statements, statements_of};
use uuid::Uuid;

/// Inserts a `Users` row and grants it every library (`EnableAllFolders`), as
/// a Jellyfin-created user holds.
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
    ferrofin_core::user_entity_ext::set_permission(
        db,
        &guid_to_db(id),
        PermissionKind::EnableAllFolders,
        true,
    )
    .await
    .expect("grant EnableAllFolders");
}

/// Saves a row of `kind` named `name` under `library`, with `genre`.
async fn seed_item(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
    name: &str,
    library: Option<Uuid>,
    genre: Option<&str>,
) {
    let persistence = FerrofinItemPersistenceService::new(db.clone());
    let row = BaseItemEntity {
        id: guid_to_db(id),
        type_: kind.stored_type_name().expect("a stored type").to_owned(),
        name: Some(name.to_owned()),
        genres: genre.map(str::to_owned),
        top_parent_id: library.map(guid_to_db),
        ..BaseItemEntity::default()
    };
    persistence.save_items(&[row]).await.expect("save item");
    if let Some(genre) = genre {
        persistence
            .save_item_values(id, &[(i32::from(ItemValueType::Genre), genre.to_owned())])
            .await
            .expect("save genre");
    }
}

/// Marks `item` played by `user`.
async fn seed_played(db: &Database, user: Uuid, item: Uuid) {
    sqlx::query(
        r#"INSERT INTO "UserData"
           ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
            "PlaybackPositionTicks", "Played")
           VALUES (?1, ?2, ?3, 0, 1, 0, 1)"#,
    )
    .bind(guid_to_db(item))
    .bind(guid_to_db(user))
    .bind(item.to_string())
    .execute(db.writer())
    .await
    .expect("insert user data");
}

/// Three recently played movies, each of its own genre, and an unplayed one
/// sharing each genre, all in one library: three "similar to" baselines.
#[tokio::test]
async fn recommendations_read_the_user_and_its_policy_once_per_request() {
    record_statements();
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    db.enable_row_cache(ferrofin_db::row_cache::ROW_CACHE_TTL);
    let user = Uuid::new_v4();
    seed_user(&db, user).await;
    let library = Uuid::new_v4();
    seed_item(
        &db,
        library,
        BaseItemKind::CollectionFolder,
        "Movies",
        None,
        None,
    )
    .await;
    for genre in ["Drama", "Western", "Comedy"] {
        let played = Uuid::new_v4();
        seed_item(
            &db,
            played,
            BaseItemKind::Movie,
            &format!("Played {genre}"),
            Some(library),
            Some(genre),
        )
        .await;
        seed_played(&db, user, played).await;
        seed_item(
            &db,
            Uuid::new_v4(),
            BaseItemKind::Movie,
            &format!("Unplayed {genre}"),
            Some(library),
            Some(genre),
        )
        .await;
    }
    let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
        Arc::new(ItemTypeLookup::new());
    let manager = FerrofinSimilarItemsManager::new(
        db.clone(),
        Arc::new(FerrofinItemRepository::new(db.clone(), lookup)),
    );
    let options = DtoOptions::default();
    let recommend = || manager.get_movie_recommendations(Some(user), Uuid::nil(), 6, 5, &options);
    // The first request warms the cross-request caches.
    recommend().await.expect("recommendations");

    let (recommendations, statements) = statements_of(recommend()).await;

    let recommendations = recommendations.expect("recommendations");
    let mut categories: Vec<(String, Vec<String>)> = recommendations
        .iter()
        .inspect(|c| {
            assert_eq!(
                c.recommendation_type,
                RecommendationType::SimilarToRecentlyPlayed
            );
        })
        .map(|c| {
            let items = c.items.iter().filter_map(|i| i.name.clone()).collect();
            (c.baseline_item_name.clone(), items)
        })
        .collect();
    categories.sort();
    assert_eq!(
        categories,
        ["Comedy", "Drama", "Western"]
            .map(|g| (format!("Played {g}"), vec![format!("Unplayed {g}")]))
            .to_vec(),
        "one category per baseline, holding the movie of its genre"
    );
    let reads = |table: &str| {
        statements
            .iter()
            .filter(|s| s.contains(&format!(r#"FROM "{table}""#)))
            .count()
    };
    assert_eq!(reads("Users"), 1, "the user is read once: {statements:#?}");
    assert_eq!(
        reads("Permissions") + reads("Preferences"),
        0,
        "the user's policy comes from the warm cache: {statements:#?}"
    );
}
