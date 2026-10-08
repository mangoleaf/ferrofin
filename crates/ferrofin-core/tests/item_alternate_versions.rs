//! A primary video's local alternate versions as the scan writes them
//! (`ItemPersistenceService::sync_local_versions`, the port of upstream's
//! `ItemPersistenceService.SaveItems` video branch,
//! `ItemPersistenceService.cs:651-780`), how they meet the version links a
//! merge writes, and the merge and split (`VideosController.MergeVersions`,
//! `DeleteAlternateSources`) around them.
//!
//! The first five tests transliterate upstream's
//! `ItemPersistenceAlternateVersionTests`.

use std::path::Path;
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id, stored_type_name};
use ferrofin_core::{
    FerrofinItemCountService, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinLibraryManager, FerrofinLinkedChildrenService, FerrofinPeopleRepository,
    FerrofinVirtualFolderManager, LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::{LibraryManager as _, VirtualFolderManager};
use ferrofin_traits::persistence::{
    ItemPersistenceService as _, ItemRepository, LocalVersionGroup,
};
use uuid::Uuid;

const PRIMARY_PATH: &str = "/movies/Movie/Movie - 4K.mkv";
const VERSION_PATH: &str = "/movies/Movie/Movie - 1080p.mkv";

async fn test_db() -> Database {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    db
}

/// A stored movie at `path` (upstream's `CreateMovie`), owned by `owner`
/// when given — a local alternate is stored owned by its primary.
async fn movie(db: &Database, id: Uuid, path: &str, owner: Option<Uuid>, primary: Option<Uuid>) {
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&[BaseItemEntity {
            id: guid_to_db(id),
            type_: stored_type_name(BaseItemKind::Movie)
                .expect("movie type")
                .to_owned(),
            name: Some("Movie".to_owned()),
            path: Some(path.to_owned()),
            owner_id: owner.map(guid_to_db),
            primary_version_id: primary.map(guid_to_db),
            ..BaseItemEntity::default()
        }])
        .await
        .expect("save movie");
}

/// Sets `id`'s `Width`, which the merge's primary choice reads.
async fn set_width(db: &Database, id: Uuid, width: i64) {
    sqlx::query(r#"UPDATE "BaseItems" SET "Width" = ?2 WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .bind(width)
        .execute(db.writer())
        .await
        .expect("width");
}

/// Writes one `LinkedChildren` row as a database another writer left it.
async fn link(db: &Database, parent: Uuid, child: Uuid, child_type: i64, sort: i64) {
    sqlx::query(
        r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
           VALUES (?1, ?2, ?3, ?4)"#,
    )
    .bind(guid_to_db(parent))
    .bind(sort)
    .bind(guid_to_db(child))
    .bind(child_type)
    .execute(db.writer())
    .await
    .expect("link");
}

/// The scan's sync of one group whose versions it resolved.
async fn sync(db: &Database, primary: Uuid, versions: &[Uuid], released: &[Uuid]) {
    FerrofinItemPersistenceService::new(db.clone())
        .sync_local_versions(&[LocalVersionGroup {
            primary,
            versions: Some(versions.to_vec()),
            released: released.to_vec(),
        }])
        .await
        .expect("sync");
}

/// `(PrimaryVersionId, PresentationUniqueKey)` of `id`.
async fn version_of(db: &Database, id: Uuid) -> (Option<Uuid>, Option<String>) {
    let (primary, key): (Option<String>, Option<String>) = sqlx::query_as(
        r#"SELECT "PrimaryVersionId", "PresentationUniqueKey" FROM "BaseItems" WHERE "Id" = ?1"#,
    )
    .bind(guid_to_db(id))
    .fetch_one(db.pool())
    .await
    .expect("row");
    (primary.map(|p| Uuid::parse_str(&p).expect("guid")), key)
}

/// `OwnerId` of `id`.
async fn owner_of(db: &Database, id: Uuid) -> Option<Uuid> {
    let owner: Option<String> =
        sqlx::query_scalar(r#"SELECT "OwnerId" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("row");
    owner.map(|o| Uuid::parse_str(&o).expect("guid"))
}

/// The `LinkedChildren` rows under `parent`: `(ChildId, ChildType,
/// SortOrder)`.
async fn links_under(db: &Database, parent: Uuid) -> Vec<(Uuid, i64, i64)> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        r#"SELECT "ChildId", "ChildType", "SortOrder" FROM "LinkedChildren"
           WHERE "ParentId" = ?1 ORDER BY "SortOrder""#,
    )
    .bind(guid_to_db(parent))
    .fetch_all(db.pool())
    .await
    .expect("links");
    rows.into_iter()
        .map(|(c, t, s)| (Uuid::parse_str(&c).expect("guid"), t, s))
        .collect()
}

async fn link_count(db: &Database) -> i64 {
    sqlx::query_scalar(r#"SELECT COUNT(*) FROM "LinkedChildren""#)
        .fetch_one(db.pool())
        .await
        .expect("count")
}

/// `id`'s "N" form, the presentation key of a video whose primary it is.
fn key(id: Uuid) -> String {
    id.as_simple().to_string()
}

fn library_manager(db: &Database) -> FerrofinLibraryManager {
    FerrofinLibraryManager::new(
        Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        )),
        Arc::new(FerrofinItemCountService::new(db.clone())),
        Arc::new(FerrofinItemPersistenceService::new(db.clone())),
        Arc::new(FerrofinPeopleRepository::new(db.clone())),
    )
}

/// `SaveItems_LocalAlternateVersionAlreadyAnItem_SetsPrimaryVersionId`.
#[tokio::test]
async fn local_alternate_version_already_an_item_sets_primary_version_id() {
    let db = test_db().await;
    let primary = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").expect("guid");
    let version = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").expect("guid");

    // The version was scanned as a standalone movie before it became a
    // version, so it has a presentation key of its own and no primary.
    movie(&db, version, VERSION_PATH, None, None).await;
    sqlx::query(r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = 'standalone' WHERE "Id" = ?1"#)
        .bind(guid_to_db(version))
        .execute(db.writer())
        .await
        .expect("key");
    assert_eq!(version_of(&db, version).await.0, None);

    // Now the scan folds it into a primary.
    movie(&db, primary, PRIMARY_PATH, None, None).await;
    sync(&db, primary, &[version], &[]).await;

    assert_eq!(links_under(&db, primary).await, vec![(version, 2, 0)]);
    assert_eq!(link_count(&db).await, 1);
    // Presentation-key grouping has to collapse it onto the primary as well.
    assert_eq!(
        version_of(&db, version).await,
        (Some(primary), Some(key(primary)))
    );
}

/// `SaveItems_LinkedAlternateVersionAlreadyAnItem_SetsPrimaryVersionId`. A
/// linked version is a merge's here
/// (`ItemPersistenceService::set_primary_version_id`), not a list on the
/// saved primary; it is written linked (3) though the files share a folder.
#[tokio::test]
async fn linked_alternate_version_already_an_item_sets_primary_version_id() {
    let db = test_db().await;
    let svc = FerrofinItemPersistenceService::new(db.clone());
    let primary = Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").expect("guid");
    let version = Uuid::parse_str("dddddddd-dddd-dddd-dddd-dddddddddddd").expect("guid");
    movie(&db, version, VERSION_PATH, None, None).await;
    movie(&db, primary, PRIMARY_PATH, None, None).await;

    svc.set_primary_version_id(version, Some(primary))
        .await
        .expect("merge");

    assert_eq!(links_under(&db, primary).await, vec![(version, 3, 0)]);
    assert_eq!(link_count(&db).await, 1);
    assert_eq!(version_of(&db, version).await.0, Some(primary));
}

/// `SaveItems_VersionAlreadyPointingAtPrimary_LeavesItAlone`.
#[tokio::test]
async fn version_already_pointing_at_primary_leaves_it_alone() {
    let db = test_db().await;
    let primary = Uuid::parse_str("eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee").expect("guid");
    let version = Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").expect("guid");
    movie(&db, version, VERSION_PATH, None, Some(primary)).await;
    movie(&db, primary, PRIMARY_PATH, None, None).await;

    sync(&db, primary, &[version], &[]).await;

    assert_eq!(
        version_of(&db, version).await,
        (Some(primary), Some(key(primary)))
    );
}

/// `SaveItems_VideoListedAmongItsOwnVersions_KeepsItsOwnPrimaryVersionId`.
#[tokio::test]
async fn video_listed_among_its_own_versions_keeps_its_own_primary_version_id() {
    let db = test_db().await;
    let primary = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("guid");
    movie(&db, primary, PRIMARY_PATH, None, None).await;

    sync(&db, primary, &[primary], &[]).await;

    assert_eq!(version_of(&db, primary).await.0, None);
}

/// `SaveItems_PromotedVersionStillPointingAtOldPrimary_DoesNotCreateACycle`:
/// no pointer cycle. Upstream gets there by skipping the old primary's
/// demotion (`:731`) and leaving the promoted video pointing at it — which
/// hides the whole group (the promoted video is a version, the old primary
/// owned by it once the scan owns it). Ferrofin releases the promoted video
/// instead, local beating linked, and demotes the old primary to it: the
/// group's primary is visible and the expected values differ from the C#
/// test's on purpose.
#[tokio::test]
async fn promoted_version_still_pointing_at_old_primary_does_not_create_a_cycle() {
    let db = test_db().await;
    let promoted = Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("guid");
    let old_primary = Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("guid");
    movie(&db, old_primary, VERSION_PATH, None, None).await;

    // The rescan resolves this one as the primary of the group, but it still
    // carries the pointer to the version it was promoted over.
    movie(&db, promoted, PRIMARY_PATH, None, Some(old_primary)).await;
    sync(&db, promoted, &[old_primary], &[]).await;

    // Pointing the old primary back while the promoted video keeps its
    // pointer would make a cycle; the promoted video lets go instead.
    assert_eq!(version_of(&db, promoted).await, (None, Some(key(promoted))));
    assert_eq!(
        version_of(&db, old_primary).await,
        (Some(promoted), Some(key(promoted)))
    );
}

/// Local outranks linked within the primary's own list (`:688-692`): the
/// pair's merge row becomes the local one. A merge under another primary is
/// that primary's: the save never touches another parent's rows.
#[tokio::test]
async fn a_local_version_takes_the_pairs_merge_row_and_leaves_other_parents_alone() {
    let db = test_db().await;
    let svc = FerrofinItemPersistenceService::new(db.clone());
    let (primary, version, other, other_version) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    movie(&db, primary, PRIMARY_PATH, None, None).await;
    movie(&db, version, VERSION_PATH, None, None).await;
    movie(&db, other, "/movies/Other/Other.mkv", None, None).await;
    movie(
        &db,
        other_version,
        "/movies/Elsewhere/Movie.mkv",
        None,
        None,
    )
    .await;
    svc.set_primary_version_id(other_version, Some(primary))
        .await
        .expect("merge");
    svc.set_primary_version_id(version, Some(other))
        .await
        .expect("merge");

    sync(&db, primary, &[other_version, version], &[]).await;

    assert_eq!(
        links_under(&db, primary).await,
        vec![(other_version, 2, 0), (version, 2, 1)]
    );
    assert_eq!(links_under(&db, other).await, vec![(version, 3, 0)]);
    assert_eq!(version_of(&db, version).await.0, Some(primary));
}

/// Upstream drops the row of an owned version the primary no longer lists
/// and deletes that item (`:758-781`); here the row is unlinked and left for
/// the scan's prune (owner decision D9b), still hidden by its owner.
#[tokio::test]
async fn an_owned_version_no_longer_listed_loses_its_link() {
    let db = test_db().await;
    let (primary, kept, dropped) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    movie(&db, primary, PRIMARY_PATH, None, None).await;
    movie(&db, kept, VERSION_PATH, Some(primary), None).await;
    movie(
        &db,
        dropped,
        "/movies/Movie/Movie - 720p.mkv",
        Some(primary),
        None,
    )
    .await;
    sync(&db, primary, &[dropped, kept], &[]).await;

    sync(&db, primary, &[kept], &[]).await;

    assert_eq!(links_under(&db, primary).await, vec![(kept, 2, 0)]);
    assert_eq!(version_of(&db, dropped).await.0, Some(primary));
    assert_eq!(owner_of(&db, dropped).await, Some(primary));
}

/// A version the scan took out of the group (owned by the primary before
/// the save, by none after) stops pointing at it.
#[tokio::test]
async fn a_released_version_points_at_nothing_again() {
    let db = test_db().await;
    let (primary, version) = (Uuid::new_v4(), Uuid::new_v4());
    movie(&db, primary, PRIMARY_PATH, None, None).await;
    movie(&db, version, VERSION_PATH, Some(primary), None).await;
    sync(&db, primary, &[version], &[]).await;
    // The save that released it: no longer owned.
    movie(&db, version, VERSION_PATH, None, Some(primary)).await;

    sync(&db, primary, &[], &[version]).await;

    assert_eq!(link_count(&db).await, 0);
    assert_eq!(version_of(&db, version).await, (None, Some(key(version))));
}

/// A scan that stopped before it resolved the group knows only what it
/// released: the released version's link goes, the group's other owned
/// version keeps its own.
#[tokio::test]
async fn an_unknown_group_loses_only_its_released_version() {
    let db = test_db().await;
    let (primary, kept, released) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    movie(&db, primary, PRIMARY_PATH, None, None).await;
    movie(&db, kept, VERSION_PATH, Some(primary), None).await;
    movie(
        &db,
        released,
        "/movies/Movie/Movie - 720p.mkv",
        Some(primary),
        None,
    )
    .await;
    sync(&db, primary, &[kept, released], &[]).await;
    movie(
        &db,
        released,
        "/movies/Movie/Movie - 720p.mkv",
        None,
        Some(primary),
    )
    .await;

    FerrofinItemPersistenceService::new(db.clone())
        .sync_local_versions(&[LocalVersionGroup {
            primary,
            versions: None,
            released: vec![released],
        }])
        .await
        .expect("sync");

    assert_eq!(links_under(&db, primary).await, vec![(kept, 2, 0)]);
    assert_eq!(
        version_of(&db, kept).await,
        (Some(primary), Some(key(primary)))
    );
    assert_eq!(version_of(&db, released).await.0, None);
}

/// What a merge wrote is not the scan's: a merge's row stays behind the
/// local versions, pointer and all — and so does a type-2 row whose child
/// nothing owns (a same-folder merge as Ferrofin once typed it, before
/// `retype_merged_version_links`). A second sync changes nothing.
#[tokio::test]
async fn a_merge_versions_row_survives_a_sync() {
    let db = test_db().await;
    let svc = FerrofinItemPersistenceService::new(db.clone());
    let (primary, local, merged, legacy) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    movie(&db, primary, PRIMARY_PATH, None, None).await;
    movie(
        &db,
        merged,
        "/movies/Movie/Movie (Director's Cut).mkv",
        None,
        None,
    )
    .await;
    movie(
        &db,
        legacy,
        "/movies/Movie/Movie (Remastered).mkv",
        None,
        Some(primary),
    )
    .await;
    movie(&db, local, VERSION_PATH, Some(primary), None).await;
    svc.set_primary_version_id(merged, Some(primary))
        .await
        .expect("merge");
    link(&db, primary, legacy, 2, 1).await;

    for _ in 0..2 {
        sync(&db, primary, &[local], &[]).await;
        assert_eq!(
            links_under(&db, primary).await,
            vec![(local, 2, 0), (merged, 3, 1), (legacy, 2, 2)]
        );
    }
    for version in [merged, legacy] {
        assert_eq!(
            version_of(&db, version).await,
            (Some(primary), Some(key(primary)))
        );
    }
}

/// A version not stored is skipped (upstream logs and skips it), and a
/// primary not stored writes nothing; several groups sync in one call.
#[tokio::test]
async fn a_version_or_primary_not_stored_is_skipped() {
    let db = test_db().await;
    let (primary, version, ghost) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    movie(&db, primary, PRIMARY_PATH, None, None).await;
    movie(&db, version, VERSION_PATH, Some(primary), None).await;

    FerrofinItemPersistenceService::new(db.clone())
        .sync_local_versions(&[
            LocalVersionGroup {
                primary,
                versions: Some(vec![ghost, version]),
                released: Vec::new(),
            },
            LocalVersionGroup {
                primary: ghost,
                versions: Some(vec![version]),
                released: Vec::new(),
            },
        ])
        .await
        .expect("sync");

    assert_eq!(links_under(&db, primary).await, vec![(version, 2, 0)]);
    assert_eq!(link_count(&db).await, 1);
}

/// `DELETE /Videos/{id}/AlternateSources` splits the merge only
/// (`VideosController.DeleteAlternateSources` unlinks
/// `GetLinkedAlternateVersions`): the primary's local versions stay — an
/// owned one, and an unowned one under a local row (a 10.11 adoption's).
#[tokio::test]
async fn split_keeps_the_local_versions() {
    let db = test_db().await;
    let library = library_manager(&db);
    let persistence = FerrofinItemPersistenceService::new(db.clone());
    let (primary, local, adopted, merged) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    movie(&db, primary, PRIMARY_PATH, None, None).await;
    movie(&db, local, VERSION_PATH, Some(primary), None).await;
    movie(
        &db,
        adopted,
        "/movies/Movie/Movie - 720p.mkv",
        None,
        Some(primary),
    )
    .await;
    movie(&db, merged, "/movies/Elsewhere/Movie.mkv", None, None).await;
    sync(&db, primary, &[local], &[]).await;
    link(&db, primary, adopted, 2, 1).await;
    persistence
        .set_primary_version_id(merged, Some(primary))
        .await
        .expect("merge");

    // Asked of a local version, the split reaches its primary's group.
    library
        .remove_alternate_sources(local)
        .await
        .expect("split");

    assert_eq!(version_of(&db, merged).await, (None, Some(key(merged))));
    for version in [local, adopted] {
        assert_eq!(
            version_of(&db, version).await,
            (Some(primary), Some(key(primary)))
        );
    }
    assert_eq!(
        links_under(&db, primary).await,
        vec![(local, 2, 0), (adopted, 2, 1)]
    );
}

/// `POST /Videos/MergeVersions` picks the first video (by id) that already
/// has versions and is no version itself, however narrow; with none, the
/// widest.
#[tokio::test]
async fn merge_versions_keeps_a_video_with_versions_as_the_primary() {
    let db = test_db().await;
    let library = library_manager(&db);
    let primary = Uuid::from_u128(0x30);
    let local = Uuid::from_u128(0x31);
    let wide = Uuid::from_u128(0x10);
    movie(&db, primary, PRIMARY_PATH, None, None).await;
    movie(&db, local, VERSION_PATH, Some(primary), None).await;
    movie(&db, wide, "/movies/Elsewhere/Movie.mkv", None, None).await;
    set_width(&db, primary, 640).await;
    set_width(&db, wide, 3840).await;
    sync(&db, primary, &[local], &[]).await;

    library
        .merge_versions(&[wide, primary])
        .await
        .expect("merge");

    assert_eq!(version_of(&db, primary).await.0, None);
    assert_eq!(version_of(&db, wide).await.0, Some(primary));

    // Two videos without versions: the widest, the first of equals.
    let (a, b, c) = (
        Uuid::from_u128(0x41),
        Uuid::from_u128(0x42),
        Uuid::from_u128(0x43),
    );
    for (id, width) in [(a, 1280), (b, 1920), (c, 1920)] {
        movie(&db, id, &format!("/movies/{id}/Movie.mkv"), None, None).await;
        set_width(&db, id, width).await;
    }
    library.merge_versions(&[c, a, b]).await.expect("merge");
    assert_eq!(version_of(&db, b).await.0, None);
    for id in [a, c] {
        assert_eq!(version_of(&db, id).await.0, Some(b));
    }
}

/// A tvshows scan groups an episode's versions as `GetEpisodesGroupedByVersion`
/// does — the resolution-named file first (`OrganizeAlternateVersions`) — and
/// regroups a database that grouped them the other way round (a 12.x merge
/// or an older resolver's primary): the rows keep their ids, the version
/// is owned by, points at and is linked under the resolver's primary, and
/// the old primary lets it go.
#[tokio::test]
async fn a_tv_scan_groups_episode_versions_and_regroups_a_stored_group() {
    let tmp = tempfile::tempdir().expect("tmp");
    let tv = tmp.path().join("tv");
    let season = tv.join("Show").join("Season 1");
    std::fs::create_dir_all(&season).expect("mkdir");
    let primary_path = season.join("Show - S01E01.mkv");
    let version_path = season.join("Show - S01E01 - 1080p.mkv");
    for file in [&primary_path, &version_path] {
        std::fs::write(file, b"").expect("write");
    }
    let db = test_db().await;
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("default"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Shows",
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
    let scanner =
        LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence).with_items(items);
    scanner.scan_all().await.expect("first scan");
    let episode = |path: &Path| {
        derive_item_id(BaseItemKind::Episode, &path.to_string_lossy()).expect("episode id")
    };
    // `Show - S01E01 - 1080p` matches the resolution rule, so it leads.
    let (primary, version) = (episode(&version_path), episode(&primary_path));
    let grouped = |db: Database| async move {
        assert_eq!(owner_of(&db, version).await, Some(primary));
        assert_eq!(owner_of(&db, primary).await, None);
        // The primary points at nothing, keyed by its own id.
        assert_eq!(version_of(&db, primary).await, (None, Some(key(primary))));
        assert_eq!(
            version_of(&db, version).await,
            (Some(primary), Some(key(primary)))
        );
        assert_eq!(links_under(&db, primary).await, vec![(version, 2, 0)]);
        assert!(links_under(&db, version).await.is_empty());
    };
    grouped(db.clone()).await;
    // The group stored the other way round: the plain file leading.
    sqlx::query(
        r#"UPDATE "BaseItems" SET "OwnerId" = NULL, "PrimaryVersionId" = NULL,
               "PresentationUniqueKey" = ?2 WHERE "Id" = ?1"#,
    )
    .bind(guid_to_db(primary))
    .bind(key(primary))
    .execute(db.writer())
    .await
    .expect("primary");
    sqlx::query(r#"DELETE FROM "LinkedChildren""#)
        .execute(db.writer())
        .await
        .expect("links");
    sqlx::query(r#"UPDATE "BaseItems" SET "OwnerId" = ?2 WHERE "Id" = ?1"#)
        .bind(guid_to_db(primary))
        .bind(guid_to_db(version))
        .execute(db.writer())
        .await
        .expect("owner");
    sync(&db, version, &[primary], &[]).await;
    assert_eq!(owner_of(&db, primary).await, Some(version));

    scanner.scan_all().await.expect("second scan");

    grouped(db.clone()).await;
}

/// One scan's groups decide from one read: a version that moved from P's
/// group to Q's is released by P and demoted by Q, and ends up Q's in either
/// order.
#[tokio::test]
async fn a_version_moving_between_groups_in_one_sync_ends_up_in_the_new_one() {
    for reversed in [false, true] {
        let db = test_db().await;
        let (p, q, w) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        movie(&db, p, PRIMARY_PATH, None, None).await;
        movie(&db, q, "/movies/Movie/Movie - 3D.mkv", None, None).await;
        movie(&db, w, VERSION_PATH, Some(p), None).await;
        sync(&db, p, &[w], &[]).await;
        // The walk saved it owned by Q.
        movie(&db, w, VERSION_PATH, Some(q), Some(p)).await;
        let mut groups = vec![
            LocalVersionGroup {
                primary: p,
                versions: Some(Vec::new()),
                released: vec![w],
            },
            LocalVersionGroup {
                primary: q,
                versions: Some(vec![w]),
                released: Vec::new(),
            },
        ];
        if reversed {
            groups.reverse();
        }

        FerrofinItemPersistenceService::new(db.clone())
            .sync_local_versions(&groups)
            .await
            .expect("sync");

        assert!(links_under(&db, p).await.is_empty(), "reversed: {reversed}");
        assert_eq!(links_under(&db, q).await, vec![(w, 2, 0)]);
        assert_eq!(version_of(&db, w).await, (Some(q), Some(key(q))));
    }
}

/// `VideosController.MergeVersions` moves each merged item's own merged
/// versions to the chosen primary (`:238-250`), so no version is left
/// behind a version, and re-points each merged item's playlist and
/// collection entries to the primary (`RerouteLinkedChildReferencesAsync`:
/// a parent that already lists the primary drops the entry).
#[tokio::test]
async fn merge_versions_moves_linked_versions_and_reroutes_memberships() {
    let db = test_db().await;
    let library = library_manager(&db)
        .with_linked_children(Arc::new(FerrofinLinkedChildrenService::new(db.clone())));
    let persistence = FerrofinItemPersistenceService::new(db.clone());
    // Both already head a merge; the first by id is kept.
    let (primary, merged, its_version, primarys_version) = (
        Uuid::from_u128(0x51),
        Uuid::from_u128(0x52),
        Uuid::from_u128(0x53),
        Uuid::from_u128(0x54),
    );
    movie(&db, primary, "/movies/A/Movie.mkv", None, None).await;
    movie(&db, merged, "/movies/B/Movie.mkv", None, None).await;
    movie(&db, its_version, "/movies/C/Movie.mkv", None, None).await;
    movie(&db, primarys_version, "/movies/D/Movie.mkv", None, None).await;
    for (version, of) in [(its_version, merged), (primarys_version, primary)] {
        persistence
            .set_primary_version_id(version, Some(of))
            .await
            .expect("earlier merge");
    }
    let (only_merged, both) = (Uuid::from_u128(0x61), Uuid::from_u128(0x62));
    for collection in [only_merged, both] {
        persistence
            .save_items(&[BaseItemEntity {
                id: guid_to_db(collection),
                type_: stored_type_name(BaseItemKind::BoxSet)
                    .expect("box set type")
                    .to_owned(),
                name: Some("Collection".to_owned()),
                ..BaseItemEntity::default()
            }])
            .await
            .expect("collection");
    }
    link(&db, only_merged, merged, 0, 0).await;
    link(&db, both, merged, 0, 0).await;
    link(&db, both, primary, 0, 1).await;

    library
        .merge_versions(&[merged, primary])
        .await
        .expect("merge");

    assert_eq!(version_of(&db, merged).await.0, Some(primary));
    assert_eq!(
        version_of(&db, its_version).await,
        (Some(primary), Some(key(primary)))
    );
    assert!(links_under(&db, merged).await.is_empty());
    assert_eq!(
        links_under(&db, primary).await,
        vec![
            (primarys_version, 3, 0),
            (merged, 3, 1),
            (its_version, 3, 2)
        ]
    );
    assert_eq!(links_under(&db, only_merged).await, vec![(primary, 0, 0)]);
    assert_eq!(links_under(&db, both).await, vec![(primary, 0, 1)]);
}
