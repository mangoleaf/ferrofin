//! Videos directly in a folder whose children resolve as in a tvshows library
//! — a TV library's location, or a folder set to "Content type: Shows" — as
//! v12.4 resolves them: `MovieResolver.ResolveMultiple` runs before any
//! single-item resolver (`LibraryManager.cs:1046-1061`) and makes them
//! `ResolveVideos<Episode>(parent, files, true, tvshows, true)`
//! (`MovieResolver.cs:232-235`): episodes with no series, hanging off the
//! folder, `IsInMixedFolder = Count > 1 || parent.IsTopParent` (`:287`), their
//! extras set aside. Below a folder that is no top parent an extras folder is
//! ignored (`CoreResolutionIgnoreRule.cs:45-61`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{IdDerivation, ItemTypeLookup};
use ferrofin_core::{
    AggregateFolderStore, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinVirtualFolderManager, LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::dto::NameValuePair;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;

/// The overrides a scan reads, which a test may change between scans.
type Overrides = Arc<Mutex<Vec<NameValuePair>>>;

/// A library of `collection_type` at `{tmp}/tv` holding `files` (paths below
/// it), under the server's root folder, read with `overrides` (folders below
/// the library).
struct Library {
    _tmp: tempfile::TempDir,
    media: PathBuf,
    db: Database,
    scanner: LibraryScanner,
    overrides: Overrides,
}

impl Library {
    async fn new(
        collection_type: CollectionTypeOptions,
        files: &[&str],
        overrides: &[(&str, &str)],
    ) -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let media = tmp.path().join("tv");
        std::fs::create_dir_all(&media).expect("library dir");
        for file in files {
            touch(&media, file);
        }
        let overrides: Overrides = Arc::new(Mutex::new(entries(&media, overrides)));
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let roots = AggregateFolderStore::new(
            db.clone(),
            IdDerivation::LegacyLowercase,
            tmp.path().join("root"),
            tmp.path().join("data"),
        )
        .ensure()
        .await
        .expect("roots");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("root/default"))
                .with_item_store(persistence.clone())
                .with_physical_root(db.clone(), roots.aggregate),
        );
        vf.add_virtual_folder(
            "TV",
            Some(collection_type),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add library");
        let items: Arc<dyn ItemRepository> = Arc::new(
            FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
                .with_root_ids(roots),
        );
        let read = Arc::clone(&overrides);
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_items(items)
            .with_root_folder(roots.aggregate)
            .with_content_types(move || read.lock().expect("overrides").clone());
        Self {
            _tmp: tmp,
            media,
            db,
            scanner,
            overrides,
        }
    }

    /// The stored episodes and videos, sorted by path below the library.
    async fn videos(&self) -> Vec<Video> {
        let rows: Vec<Video> = sqlx::query_as(
            r#"SELECT b."Type", b."Name", b."Path", p."Type", p."Name", b."IsInMixedFolder",
                      b."SeriesId", b."SeasonId", b."IndexNumber", b."ParentIndexNumber"
               FROM "BaseItems" b LEFT JOIN "BaseItems" p ON p."Id" = b."ParentId"
               WHERE b."Type" IN ('MediaBrowser.Controller.Entities.TV.Episode',
                                  'MediaBrowser.Controller.Entities.Video')"#,
        )
        .fetch_all(self.db.pool())
        .await
        .expect("videos");
        let below = |path: String| {
            Path::new(&path)
                .strip_prefix(&self.media)
                .map_or_else(|_| path.clone(), |p| p.to_string_lossy().into_owned())
        };
        let mut rows: Vec<Video> = rows
            .into_iter()
            .map(|mut row| {
                row.2 = row.2.map(below);
                row
            })
            .collect();
        rows.sort_by(|a, b| a.2.cmp(&b.2));
        rows
    }

    /// The stored `(type, name, parent's name)` of every series and plain
    /// folder, sorted.
    async fn folders(&self) -> Vec<(String, String, String)> {
        let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
            r#"SELECT b."Type", b."Name", p."Name" FROM "BaseItems" b
               LEFT JOIN "BaseItems" p ON p."Id" = b."ParentId"
               WHERE b."Type" IN ('MediaBrowser.Controller.Entities.TV.Series',
                                  'MediaBrowser.Controller.Entities.Folder')
                 AND b."Path" LIKE ?1"#,
        )
        .bind(format!("{}%", self.media.to_string_lossy()))
        .fetch_all(self.db.pool())
        .await
        .expect("folders");
        let mut rows: Vec<_> = rows
            .into_iter()
            .map(|(type_, name, parent)| (kind(&type_), name, parent.unwrap_or_default()))
            .collect();
        rows.sort();
        rows
    }
}

/// A stored video: type, name, path below the library, its parent's type and
/// name, `IsInMixedFolder`, `SeriesId`, `SeasonId`, `IndexNumber` and
/// `ParentIndexNumber`.
type Video = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

/// What Jellyfin stores for an episode with no series or season.
const NIL: &str = "00000000-0000-0000-0000-000000000000";

fn touch(media: &Path, file: &str) {
    let path = media.join(file);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("fixture dir");
    std::fs::write(&path, b"").expect("fixture file");
}

fn entries(media: &Path, overrides: &[(&str, &str)]) -> Vec<NameValuePair> {
    overrides
        .iter()
        .map(|(folder, content_type)| NameValuePair {
            name: Some(media.join(folder).to_string_lossy().into_owned()),
            value: Some((*content_type).to_owned()),
        })
        .collect()
}

fn kind(type_: &str) -> String {
    type_.rsplit('.').next().unwrap_or_default().to_owned()
}

/// A series-less episode row as [`Library::videos`] reads it.
fn episode(
    name: &str,
    path: &str,
    (parent_type, parent): (&str, &str),
    mixed: bool,
    numbers: (Option<i64>, Option<i64>),
) -> Video {
    (
        "MediaBrowser.Controller.Entities.TV.Episode".to_owned(),
        name.to_owned(),
        Some(path.to_owned()),
        Some(format!("MediaBrowser.Controller.Entities.{parent_type}")),
        Some(parent.to_owned()),
        mixed,
        Some(NIL.to_owned()),
        Some(NIL.to_owned()),
        numbers.0,
        numbers.1,
    )
}

/// Jellyfin 12.4, live: a video in a TV library's location is an `Episode`
/// with no series, hanging off the location's folder, in a mixed folder (a
/// location is a top parent), numbered from its name; an extra beside it is
/// no item, and a series beside it is planned as before.
#[tokio::test]
async fn a_video_in_a_tv_location_is_an_episode_without_a_series() {
    let library = Library::new(
        CollectionTypeOptions::tvshows,
        &[
            "Loose Root S01E01.mkv",
            "Holiday Special.mkv",
            "Loose Root S01E01-trailer.mkv",
            "Show A/Season 1/Show A S01E01.mkv",
        ],
        &[],
    )
    .await;
    library.scanner.scan_all().await.expect("scan");

    let videos = library.videos().await;
    let loose: Vec<_> = videos
        .iter()
        .filter(|video| video.6.as_deref() == Some(NIL))
        .cloned()
        .collect();
    assert_eq!(
        loose,
        [
            episode(
                "Holiday Special",
                "Holiday Special.mkv",
                ("Folder", "tv"),
                true,
                (None, None)
            ),
            episode(
                "Loose Root S01E01",
                "Loose Root S01E01.mkv",
                ("Folder", "tv"),
                true,
                (Some(1), Some(1))
            ),
        ]
    );
    assert_eq!(videos.len(), 3, "the series' episode, no extra: {videos:?}");
    assert!(library.folders().await.contains(&(
        "Series".to_owned(),
        "Show A".to_owned(),
        "tv".to_owned()
    )));
}

/// Jellyfin 12.4, live: in a folder set to "Content type: Shows" a lone video
/// is an `Episode` with no series hanging off the folder, not in a mixed
/// folder (the folder is no top parent); with a second one both are. Its
/// extras folder is ignored, where the location's is a series.
#[rstest::rstest]
#[case::lone(&["Kids/Kids Special S01E02.mkv"], false)]
#[case::two(&["Kids/Kids Special S01E02.mkv", "Kids/Kids Recap S01E03.mkv"], true)]
#[tokio::test]
async fn a_video_in_a_folder_of_shows_is_an_episode_without_a_series(
    #[case] videos: &[&str],
    #[case] mixed: bool,
) {
    let mut files = vec![
        "Kids/Show B/Season 1/Show B S01E01.mkv",
        "Kids/trailers/Kids Trailer.mkv",
        "trailers/Root Trailer S01E01.mkv",
    ];
    files.extend(videos);
    let library = Library::new(
        CollectionTypeOptions::tvshows,
        &files,
        &[("Kids", "tvshows")],
    )
    .await;
    library.scanner.scan_all().await.expect("scan");

    let stored = library.videos().await;
    let special = stored
        .iter()
        .find(|video| video.2.as_deref() == Some("Kids/Kids Special S01E02.mkv"))
        .unwrap_or_else(|| panic!("the loose video is an episode: {stored:?}"));
    assert_eq!(
        special,
        &episode(
            "Kids Special S01E02",
            "Kids/Kids Special S01E02.mkv",
            ("Folder", "Kids"),
            mixed,
            (Some(2), Some(1))
        )
    );
    assert!(
        stored.iter().all(|video| !video
            .2
            .as_deref()
            .is_some_and(|p| p.starts_with("Kids/trailers"))),
        "the extras folder is ignored: {stored:?}"
    );
    let folders = library.folders().await;
    assert!(
        folders.contains(&("Folder".to_owned(), "Kids".to_owned(), "tv".to_owned()))
            && folders.contains(&("Series".to_owned(), "Show B".to_owned(), "Kids".to_owned()))
            && folders.contains(&("Series".to_owned(), "trailers".to_owned(), "tv".to_owned()))
            && !folders.contains(&(
                "Series".to_owned(),
                "trailers".to_owned(),
                "Kids".to_owned()
            )),
        "{folders:?}"
    );
}

/// The movie walk's folder set to Shows resolves its children as in a
/// tvshows library: its video is a series-less episode too.
#[tokio::test]
async fn a_video_in_a_movie_library_folder_of_shows_is_an_episode() {
    let library = Library::new(
        CollectionTypeOptions::movies,
        &[
            "Shows/Special S01E04.mkv",
            "Shows/Show C/Season 1/Show C S01E01.mkv",
        ],
        &[("Shows", "tvshows")],
    )
    .await;
    library.scanner.scan_all().await.expect("scan");

    let stored = library.videos().await;
    assert!(
        stored.contains(&episode(
            "Special S01E04",
            "Shows/Special S01E04.mkv",
            ("Folder", "Shows"),
            false,
            (Some(4), Some(1))
        )),
        "{stored:?}"
    );
}

/// `FillMissingEpisodeNumbersFromPath` (`LibraryManager.cs:3324-3333`): a
/// file that reads as no episode takes the numbers of the plain folder it
/// hangs off, read as a directory — a folder of shows; never the
/// location's, whose name is above the library ([`below_library`]'s
/// accepted divergence).
#[tokio::test]
async fn a_video_named_as_no_episode_takes_its_folder_of_shows_numbers() {
    let library = Library::new(
        CollectionTypeOptions::tvshows,
        &["Odd S02E05/Bonus.mkv"],
        &[("Odd S02E05", "tvshows")],
    )
    .await;
    library.scanner.scan_all().await.expect("scan");

    assert_eq!(
        library.videos().await,
        [episode(
            "Bonus",
            "Odd S02E05/Bonus.mkv",
            ("Folder", "Odd S02E05"),
            false,
            (Some(5), Some(2))
        )]
    );
}

/// `ResolveVideos<Episode>` groups versions and stacks at a location as in
/// a season folder (`MovieResolverTests.ResolveMultiple_GivenTvShowsCollection_
/// CreatesEpisodeItems`): one episode per group, its alternate version an
/// episode too, every row hanging off the location's folder.
#[tokio::test]
async fn versions_in_a_tv_location_group_into_one_episode() {
    let library = Library::new(
        CollectionTypeOptions::tvshows,
        &[
            "Show - S01E01 - 1080p.mkv",
            "Show - S01E01 - 720p.mkv",
            "Show - S01E02.mkv",
        ],
        &[],
    )
    .await;
    library.scanner.scan_all().await.expect("scan");

    let stored = library.videos().await;
    assert_eq!(stored.len(), 3, "{stored:?}");
    assert!(
        stored.iter().all(|video| video.0.ends_with(".Episode")
            && video.4.as_deref() == Some("tv")
            && video.6.as_deref() == Some(NIL)),
        "{stored:?}"
    );
    let primaries: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "BaseItems"
           WHERE "Type" = 'MediaBrowser.Controller.Entities.TV.Episode'
             AND "PrimaryVersionId" IS NULL"#,
    )
    .fetch_one(library.db.pool())
    .await
    .expect("primaries");
    assert_eq!(
        primaries, 2,
        "S01E01's two versions are one item: {stored:?}"
    );
}

/// A folder an earlier scan stored as a series, set to Shows: the next scan
/// makes it a folder of shows and its video a series-less episode under it,
/// numbered from the folder when its own name reads as no episode, and the
/// series goes (Jellyfin 12.4, live). A path-scoped scan of a new loose
/// video plans it as the full scan does.
#[tokio::test]
async fn a_series_set_to_shows_gives_its_video_to_the_folder() {
    let library = Library::new(
        CollectionTypeOptions::tvshows,
        &[
            "Kids/Kids Special S01E02.mkv",
            "Kids/Show B/Season 1/Show B S01E01.mkv",
            "Odd S02E05/Bonus.mkv",
        ],
        &[],
    )
    .await;
    library.scanner.scan_all().await.expect("first scan");
    assert!(library.folders().await.contains(&(
        "Series".to_owned(),
        "Kids".to_owned(),
        "tv".to_owned()
    )));

    *library.overrides.lock().expect("overrides") = entries(
        &library.media,
        &[("Kids", "tvshows"), ("Odd S02E05", "tvshows")],
    );
    library.scanner.scan_all().await.expect("second scan");
    let folders = library.folders().await;
    assert!(
        !folders
            .iter()
            .any(|(kind, name, _)| kind == "Series" && name == "Kids"),
        "the series goes: {folders:?}"
    );
    let special = episode(
        "Kids Special S01E02",
        "Kids/Kids Special S01E02.mkv",
        ("Folder", "Kids"),
        false,
        (Some(2), Some(1)),
    );
    let stored = library.videos().await;
    assert!(stored.contains(&special), "{stored:?}");
    assert!(
        stored.contains(&episode(
            "Bonus",
            "Odd S02E05/Bonus.mkv",
            ("Folder", "Odd S02E05"),
            false,
            (Some(5), Some(2))
        )),
        "{stored:?}"
    );

    touch(&library.media, "Late S01E09.mkv");
    let late = library.media.join("Late S01E09.mkv");
    library
        .scanner
        .scan_paths(&[late.to_string_lossy().into_owned()])
        .await
        .expect("scoped scan");
    let stored = library.videos().await;
    assert!(
        stored.contains(&episode(
            "Late S01E09",
            "Late S01E09.mkv",
            ("Folder", "tv"),
            true,
            (Some(9), Some(1))
        )),
        "{stored:?}"
    );
    assert!(stored.contains(&special), "{stored:?}");
}
