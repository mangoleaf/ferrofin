//! Seasons with no folder of their own — a series' episodes sit in its
//! folder, no `Season NN` folders — as the library scan keys and dates them,
//! and as the boot repair moves the ones an older Ferrofin scan keyed by a
//! synthetic path.
//!
//! Upstream: `SeriesMetadataService.CreateSeasonsAsync` takes the series'
//! existing season of a number and otherwise creates one keyed
//! `series.Id + (number ?? -1) + name` (`CreateSeasonAsync`), which
//! `Folder.AddChild` stamps with `DateModified = UtcNow` once.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{
    IdDerivation, ItemTypeLookup, derive_item_id_with, legacy_virtual_season_id, virtual_season_id,
};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRekey, ItemRepository};
use uuid::Uuid;

/// The stored type of a season.
const SEASON: &str = "MediaBrowser.Controller.Entities.TV.Season";

/// A TV library holding one flat series — `Flat S01E01.mkv` and
/// `Flat S02E01.mkv` in the series folder — scanned once under Jellyfin's
/// id derivation.
struct Fixture {
    series: PathBuf,
    db: Database,
    persistence: Arc<FerrofinItemPersistenceService>,
    scanner: LibraryScanner,
    mode: IdDerivation,
}

impl Fixture {
    async fn scanned(root: &Path) -> Self {
        let tv = root.join("tv");
        let series = tv.join("Flat");
        std::fs::create_dir_all(&series).expect("mkdir");
        for file in ["Flat S01E01.mkv", "Flat S02E01.mkv"] {
            std::fs::write(series.join(file), b"0123").expect("write");
        }
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(root.join("views"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "TV",
            Some(CollectionTypeOptions::tvshows),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: tv.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add library");
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let mode = IdDerivation::Jellyfin {
            program_data_path: None,
        };
        let scanner =
            LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
                .with_items(items)
                .with_id_derivation(mode.clone());
        scanner.scan_all().await.expect("scan");
        Self {
            series,
            db,
            persistence,
            scanner,
            mode,
        }
    }

    /// The id Jellyfin gives the series' season `n` with no folder, created
    /// under `name`.
    fn season(&self, n: i32, name: &str) -> Uuid {
        let series = derive_item_id_with(
            &self.mode,
            BaseItemKind::Series,
            &self.series.to_string_lossy(),
        )
        .expect("series id");
        virtual_season_id(&self.mode, series, Some(n), name).expect("season id")
    }

    /// Moves a stored season to the id `to`, as another scanner would have
    /// stored it, keeping its refresh stamp.
    async fn rekey_season(&self, from: Uuid, to: Uuid) {
        let refreshed: Option<String> =
            sqlx::query_scalar(r#"SELECT "DateLastRefreshed" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(from))
                .fetch_one(self.db.pool())
                .await
                .expect("stamp");
        let moves = [ItemRekey {
            from,
            to,
            type_name: SEASON.to_owned(),
            dirs: Vec::new(),
            merged: Vec::new(),
            extra: false,
        }];
        let done = self.persistence.rekey_items(&moves).await.expect("rekey");
        assert_eq!(done.len(), 1);
        sqlx::query(r#"UPDATE "BaseItems" SET "DateLastRefreshed" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(to))
            .bind(refreshed)
            .execute(self.db.pool())
            .await
            .expect("restamp");
    }

    /// An episode's stored `(ParentId, SeasonId)` and the ancestors it has.
    async fn episode_links(&self, file: &str) -> (Option<String>, Option<String>, Vec<String>) {
        let id = derive_item_id_with(
            &self.mode,
            BaseItemKind::Episode,
            &self.series.join(file).to_string_lossy(),
        )
        .expect("episode id");
        let (parent, season): (Option<String>, Option<String>) =
            sqlx::query_as(r#"SELECT "ParentId", "SeasonId" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_one(self.db.pool())
                .await
                .expect("episode");
        let mut ancestors: Vec<String> =
            sqlx::query_scalar(r#"SELECT "ParentItemId" FROM "AncestorIds" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_all(self.db.pool())
                .await
                .expect("ancestors");
        ancestors.sort();
        (parent, season, ancestors)
    }

    /// A season's stored `(DateCreated, DateModified)`, as text.
    async fn dates(&self, id: Uuid) -> (Option<String>, Option<String>) {
        sqlx::query_as(r#"SELECT "DateCreated", "DateModified" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(self.db.pool())
            .await
            .expect("season")
    }

    /// The stored seasons' ids, sorted.
    async fn seasons(&self) -> Vec<String> {
        sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Type" = ?1 ORDER BY "Id""#)
            .bind(SEASON)
            .fetch_all(self.db.pool())
            .await
            .expect("seasons")
    }
}

/// A season with no folder takes the id Jellyfin keys it by — its series,
/// number and name — and `Folder.AddChild`'s `DateModified`, the `UtcNow` of
/// its creation, which no later scan restamps: a rescan writes nothing.
#[tokio::test]
async fn a_season_with_no_folder_takes_jellyfins_key_and_creation_stamp() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::scanned(tmp.path()).await;
    let mut expected = Vec::new();
    for (n, file) in [(1, "Flat S01E01.mkv"), (2, "Flat S02E01.mkv")] {
        let season = fx.season(n, &format!("Season {n}"));
        let season_db = guid_to_db(season);
        let (parent, season_id, ancestors) = fx.episode_links(file).await;
        assert_eq!(parent.as_deref(), Some(season_db.as_str()));
        assert_eq!(season_id.as_deref(), Some(season_db.as_str()));
        assert!(ancestors.contains(&season_db), "{ancestors:?}");
        let (created, modified) = fx.dates(season).await;
        assert!(modified.is_some(), "stamped at creation");
        assert_eq!(modified, created, "with the creation's UtcNow");
        expected.push(season_db);
    }
    expected.sort();
    assert_eq!(fx.seasons().await, expected);

    let first = fx.season(1, "Season 1");
    let before = fx.dates(first).await;
    let rescan = fx.scanner.scan_all().await.expect("rescan");
    assert_eq!(
        (rescan.created, rescan.updated, rescan.removed),
        (0, 0, 0),
        "{rescan:?}"
    );
    assert_eq!(fx.dates(first).await, before, "kept, not restamped");
}

/// A series' stored season with no folder keeps its id whatever name it was
/// created with (Jellyfin in another UI language here) and the
/// `DateModified` it has (none, after a refresh a provider answered), and
/// its episodes stay with it: upstream's `CreateSeasonsAsync` takes the
/// series' season of the number and creates none.
#[tokio::test]
async fn a_stored_season_with_no_folder_keeps_its_id_and_episodes() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::scanned(tmp.path()).await;
    let adopted = fx.season(1, "Staffel 1");
    fx.rekey_season(fx.season(1, "Season 1"), adopted).await;
    sqlx::query(
        r#"UPDATE "BaseItems"
           SET "DateModified" = NULL, "Name" = 'Staffel 1', "CleanName" = 'staffel 1'
           WHERE "Id" = ?1"#,
    )
    .bind(guid_to_db(adopted))
    .execute(fx.db.pool())
    .await
    .expect("no stamp, its own name");
    sqlx::query(r#"UPDATE "BaseItems" SET "SeasonName" = 'Staffel 1' WHERE "SeasonId" = ?1"#)
        .bind(guid_to_db(adopted))
        .execute(fx.db.pool())
        .await
        .expect("its episodes' season name");
    let seasons = fx.seasons().await;
    let links = fx.episode_links("Flat S01E01.mkv").await;
    assert_eq!(links.1.as_deref(), Some(guid_to_db(adopted).as_str()));

    let rescan = fx.scanner.scan_all().await.expect("rescan");
    assert_eq!(
        (rescan.created, rescan.updated, rescan.removed),
        (0, 0, 0),
        "{rescan:?}"
    );
    assert_eq!(fx.seasons().await, seasons, "no season made or lost");
    assert_eq!(
        fx.episode_links("Flat S01E01.mkv").await,
        links,
        "the episode stays with its season"
    );
    assert_eq!(fx.dates(adopted).await.1, None, "kept as stored");
    let names: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT "Name", "SeasonName" FROM "BaseItems" WHERE "Id" = ?1 OR "SeasonId" = ?1
           ORDER BY "Type" DESC"#,
    )
    .bind(guid_to_db(adopted))
    .fetch_all(fx.db.pool())
    .await
    .expect("names");
    assert_eq!(
        names,
        vec![
            (Some("Staffel 1".to_owned()), None),
            (Some("Flat S01E01".to_owned()), Some("Staffel 1".to_owned())),
        ],
        "the season keeps its name, and its episode the season's"
    );
}

/// A season with no folder an older Ferrofin scan keyed by the synthetic
/// path `{series}/#virtual-season-{n}` moves to Jellyfin's id on the boot
/// repair, with its user data and its episodes' links; a season already
/// keyed Jellyfin's way is left alone, the repair runs once, and the next
/// scan writes nothing.
#[tokio::test]
async fn an_older_scans_season_with_no_folder_moves_to_jellyfins_id() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::scanned(tmp.path()).await;
    let first = fx.season(1, "Season 1");
    let second = fx.season(2, "Season 2");
    let legacy =
        legacy_virtual_season_id(&fx.mode, &fx.series.to_string_lossy(), Some(2)).expect("id");
    // What an older scan stored: season 2 under the synthetic path's id,
    // watched by a user.
    fx.rekey_season(second, legacy).await;
    let user = Uuid::new_v4();
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
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'u', 'U')"#,
    )
    .bind(guid_to_db(user))
    .execute(fx.db.pool())
    .await
    .expect("user");
    sqlx::query(
        r#"INSERT INTO "UserData"
           ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
            "PlaybackPositionTicks", "Played")
           VALUES (?1, ?2, ?3, 1, 0, 0, 0)"#,
    )
    .bind(guid_to_db(legacy))
    .bind(guid_to_db(user))
    .bind(legacy.to_string())
    .execute(fx.db.pool())
    .await
    .expect("user data");
    let legacy_db = guid_to_db(legacy);
    assert_eq!(
        fx.episode_links("Flat S02E01.mkv").await.1.as_deref(),
        Some(legacy_db.as_str())
    );

    let rekeyed =
        ferrofin_core::adoption_repairs::rekey_virtual_seasons(&fx.db, &fx.mode, None, None)
            .await
            .expect("repair");
    assert_eq!(rekeyed, 1);
    let second_db = guid_to_db(second);
    let mut expected = vec![guid_to_db(first), second_db.clone()];
    expected.sort();
    assert_eq!(fx.seasons().await, expected, "season 1 left alone");
    let (parent, season_id, ancestors) = fx.episode_links("Flat S02E01.mkv").await;
    assert_eq!(parent.as_deref(), Some(second_db.as_str()));
    assert_eq!(season_id.as_deref(), Some(second_db.as_str()));
    assert!(ancestors.contains(&second_db) && !ancestors.contains(&legacy_db));
    let user_data: Vec<(String, String, bool)> =
        sqlx::query_as(r#"SELECT "ItemId", "CustomDataKey", "IsFavorite" FROM "UserData""#)
            .fetch_all(fx.db.pool())
            .await
            .expect("user data");
    assert_eq!(user_data, vec![(second_db, second.to_string(), true)]);
    let broken: Vec<(String, i64, String, i64)> = sqlx::query_as("PRAGMA foreign_key_check")
        .fetch_all(fx.db.pool())
        .await
        .expect("check");
    assert!(broken.is_empty(), "{broken:?}");
    assert_eq!(
        ferrofin_core::adoption_repairs::rekey_virtual_seasons(&fx.db, &fx.mode, None, None)
            .await
            .expect("again"),
        0,
        "once"
    );

    let rescan = fx.scanner.scan_all().await.expect("rescan");
    assert_eq!(
        (rescan.created, rescan.updated, rescan.removed),
        (0, 0, 0),
        "{rescan:?}"
    );
}
