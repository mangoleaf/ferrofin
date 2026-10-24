//! The names a scan gives a movie's extras (`LibraryManager.FindExtras`,
//! v12.2), and when a rescan renews them.
//!
//! An extra named after its owner takes its type's localized name, numbered
//! per type in ordinal path order; any other keeps the resolver's clean name
//! for its file. A rescan renews only a name the scan gave it, so an NFO
//! title or an edit stands. The cases transliterate upstream's
//! `FindExtrasTests` (`tests/Jellyfin.Server.Implementations.Tests/Library/
//! LibraryManager/FindExtrasTests.cs`), run through a real scan of a movie
//! library.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;

/// A movie library over a temporary folder, scanned by one scanner.
struct MovieLibrary {
    _tmp: tempfile::TempDir,
    media: PathBuf,
    db: Database,
    scanner: LibraryScanner,
}

impl MovieLibrary {
    /// A library holding `files` (paths below the library folder, empty).
    async fn new(files: &[&str]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        std::fs::create_dir_all(&media).unwrap();
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let folders: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join(".views"))
                .with_item_store(persistence.clone()),
        );
        folders
            .add_virtual_folder(
                "Movies",
                Some(CollectionTypeOptions::movies),
                &LibraryOptions {
                    path_infos: vec![MediaPathInfo {
                        path: media.to_string_lossy().into_owned(),
                    }],
                    ..LibraryOptions::default()
                },
            )
            .await
            .unwrap();
        let scanner =
            LibraryScanner::new(folders, Arc::new(FerrofinFileSystem::new()), persistence)
                .with_items(Arc::new(FerrofinItemRepository::new(
                    db.clone(),
                    Arc::new(ItemTypeLookup::new()),
                )));
        let library = Self {
            _tmp: tmp,
            media,
            db,
            scanner,
        };
        for file in files {
            library.write(file, "");
        }
        library
    }

    fn write(&self, file: &str, contents: &str) {
        let path = self.media.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn path(&self, file: &str) -> String {
        self.media.join(file).to_string_lossy().into_owned()
    }

    async fn scan(&self) {
        self.scanner.scan_all().await.unwrap();
    }

    /// Every extra's name, by its path below the library folder.
    async fn extras(&self) -> BTreeMap<String, String> {
        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT Path, Name FROM BaseItems WHERE ExtraType IS NOT NULL")
                .fetch_all(self.db.pool())
                .await
                .unwrap();
        let root = format!("{}/", self.media.to_string_lossy());
        rows.into_iter()
            .map(|(path, name)| {
                let file = path.strip_prefix(&root).unwrap_or(&path).to_owned();
                (file, name.unwrap_or_default())
            })
            .collect()
    }

    /// Stores `name` on the item at `file`, as an edit or an older scan did.
    async fn rename(&self, file: &str, name: &str) {
        let done = sqlx::query("UPDATE BaseItems SET Name = ? WHERE Path = ?")
            .bind(name)
            .bind(self.path(file))
            .execute(self.db.pool())
            .await
            .unwrap();
        assert_eq!(done.rows_affected(), 1, "{file} is stored");
    }
}

fn names(expected: &[(&str, &str)]) -> BTreeMap<String, String> {
    expected
        .iter()
        .map(|(file, name)| ((*file).to_owned(), (*name).to_owned()))
        .collect()
}

/// The names a first scan gives a movie's extras.
#[rstest::rstest]
// `FindExtras_SeparateMovieFolder_CleanExtraNames`
#[case::clean_extra_names(
    &[
        "Up/Up.mkv",
        "Up/Recording the audio[Bluray]-behindthescenes.mkv",
        "Up/Interview with the dog-interview.mkv",
        "Up/shorts/Balloons[1080p].mkv",
    ],
    &[
        ("Up/Interview with the dog-interview.mkv", "Interview with the dog"),
        ("Up/Recording the audio[Bluray]-behindthescenes.mkv", "Recording the audio"),
        ("Up/shorts/Balloons[1080p].mkv", "Balloons"),
    ],
)]
// `FindExtras_SameExtraInSeveralContainers_ReturnsEach`
#[case::same_extra_in_several_containers(
    &[
        "Skyscraper (2018)/Skyscraper (2018) - [1080p HEVC].mkv",
        "Skyscraper (2018)/Skyscraper (2018) - [1080p HEVC]-trailer.mkv",
        "Skyscraper (2018)/Skyscraper (2018) - [1080p HEVC]-trailer.mp4",
        "Skyscraper (2018)/Skyscraper (2018) - [1080p HEVC]-behindthescenes.mkv",
        "Skyscraper (2018)/Skyscraper (2018) - [1080p HEVC]-behindthescenes.mp4",
    ],
    &[
        ("Skyscraper (2018)/Skyscraper (2018) - [1080p HEVC]-behindthescenes.mkv", "Behind The Scenes"),
        ("Skyscraper (2018)/Skyscraper (2018) - [1080p HEVC]-behindthescenes.mp4", "Behind The Scenes 2"),
        ("Skyscraper (2018)/Skyscraper (2018) - [1080p HEVC]-trailer.mkv", "Trailer"),
        ("Skyscraper (2018)/Skyscraper (2018) - [1080p HEVC]-trailer.mp4", "Trailer 2"),
    ],
)]
// `FindExtras_SameExtraInSeveralResolutions_ReturnsEach`
#[case::same_extra_in_several_resolutions(
    &[
        "Dragon 2 (2014)/Dragon 2 (2014) - [2160p].mkv",
        "Dragon 2 (2014)/Dragon 2 (2014) - [1080p]-trailer.mkv",
        "Dragon 2 (2014)/Dragon 2 (2014) - [2160p]-trailer.mkv",
    ],
    &[
        ("Dragon 2 (2014)/Dragon 2 (2014) - [1080p]-trailer.mkv", "Trailer"),
        ("Dragon 2 (2014)/Dragon 2 (2014) - [2160p]-trailer.mkv", "Trailer 2"),
    ],
)]
// `FindExtras_NumberedExtras_AreKeptApart`: the index in the file name is
// not the number the extra is given.
#[case::numbered_extras(
    &[
        "Up (2009)/Up (2009).mkv",
        "Up (2009)/Up (2009)-trailer.mkv",
        "Up (2009)/Up (2009)-trailer2.mkv",
        "Up (2009)/Up (2009)-trailer2.mp4",
        "Up (2009)/Up (2009)-trailer3.mkv",
    ],
    &[
        ("Up (2009)/Up (2009)-trailer.mkv", "Trailer"),
        ("Up (2009)/Up (2009)-trailer2.mkv", "Trailer 2"),
        ("Up (2009)/Up (2009)-trailer2.mp4", "Trailer 3"),
        ("Up (2009)/Up (2009)-trailer3.mkv", "Trailer 4"),
    ],
)]
// `FindExtras_ExtraWithOwnTitleBesideOwner_KeepsTitle`: a descriptive file
// name is a real title and does not consume a number.
#[case::own_title_beside_owner(
    &[
        "Up (2009)/Up (2009).mkv",
        "Up (2009)/Up (2009)-trailer.mkv",
        "Up (2009)/Recording the audio-behindthescenes.mkv",
        "Up (2009)/Up (2009)-behindthescenes.mkv",
    ],
    &[
        ("Up (2009)/Recording the audio-behindthescenes.mkv", "Recording the audio"),
        ("Up (2009)/Up (2009)-behindthescenes.mkv", "Behind The Scenes"),
        ("Up (2009)/Up (2009)-trailer.mkv", "Trailer"),
    ],
)]
// `FindExtras_ExtraInOwnFolder_IsNamedAfterItsFile`
#[case::extra_in_own_folder(
    &[
        "Up/Up.mkv",
        "Up/trailers/Teaser.mkv",
        "Up/trailers/Comic-Con Reel.mkv",
    ],
    &[
        ("Up/trailers/Comic-Con Reel.mkv", "Comic-Con Reel"),
        ("Up/trailers/Teaser.mkv", "Teaser"),
    ],
)]
// A `Filename` rule is always named after the owner: a video and an audio
// extra (the issue's example).
#[case::filename_rules(
    &[
        "Heat (1995)/Heat (1995).mkv",
        "Heat (1995)/Heat (1995)-trailer.mkv",
        "Heat (1995)/Heat (1995)-trailer2.mkv",
        "Heat (1995)/theme.mp3",
    ],
    &[
        ("Heat (1995)/Heat (1995)-trailer.mkv", "Trailer"),
        ("Heat (1995)/Heat (1995)-trailer2.mkv", "Trailer 2"),
        ("Heat (1995)/theme.mp3", "Theme Song"),
    ],
)]
// A `Suffix` rule's clean name matches its owner's ignoring case, beyond
// ASCII too (`StringComparison.OrdinalIgnoreCase`).
#[case::suffix_ignores_case(
    &[
        "Léon (1994)/Léon (1994).mkv",
        "Léon (1994)/LÉON (1994)-trailer.mkv",
    ],
    &[("Léon (1994)/LÉON (1994)-trailer.mkv", "Trailer")],
)]
#[tokio::test]
async fn a_scan_names_movie_extras_as_find_extras_does(
    #[case] files: &[&str],
    #[case] expected: &[(&str, &str)],
) {
    let library = MovieLibrary::new(files).await;
    library.scan().await;
    assert_eq!(library.extras().await, names(expected));
}

/// `FindExtras_ExtraNamedByLocalMetadata_KeepsItsNameOnRescan`: an extra's
/// own NFO title wins on its first scan and stands on the next — the scan
/// renames only before the merge, never over a local reader's answer.
#[tokio::test]
async fn an_extras_nfo_title_survives_every_scan() {
    let making_of = "Up (2009)/behind the scenes/making-of.mkv";
    let library = MovieLibrary::new(&["Up (2009)/Up (2009).mkv", making_of]).await;
    library.write(
        "Up (2009)/behind the scenes/making-of.nfo",
        "<movie><title>Making of Up</title></movie>",
    );
    library.scan().await;
    assert_eq!(
        library.extras().await,
        names(&[(making_of, "Making of Up")])
    );
    library.scan().await;
    assert_eq!(
        library.extras().await,
        names(&[(making_of, "Making of Up")])
    );
}

/// `FindExtras_ExtraKeptItsGeneratedName_IsRenumberedOnRescan`: a trailer
/// sorting before the known one takes the first number, so the known one
/// moves on.
#[tokio::test]
async fn a_generated_name_is_renumbered_on_rescan() {
    let library = MovieLibrary::new(&[
        "Up (2009)/Up (2009).mkv",
        "Up (2009)/Up (2009)-trailer2.mkv",
    ])
    .await;
    library.scan().await;
    assert_eq!(
        library.extras().await,
        names(&[("Up (2009)/Up (2009)-trailer2.mkv", "Trailer")])
    );
    library.write("Up (2009)/Up (2009)-trailer1.mkv", "");
    library.scan().await;
    assert_eq!(
        library.extras().await,
        names(&[
            ("Up (2009)/Up (2009)-trailer1.mkv", "Trailer"),
            ("Up (2009)/Up (2009)-trailer2.mkv", "Trailer 2"),
        ])
    );
}

/// A stored name the scan did not give stands (`IsGeneratedExtraName`); the
/// raw file stem Ferrofin stored before it named a movie's extras, and the
/// resolver's name for the file, are the scan's own and are renewed.
#[tokio::test]
async fn a_rescan_renews_only_names_the_scan_gave() {
    let library = MovieLibrary::new(&[
        "Heat (1995)/Heat (1995).mkv",
        "Heat (1995)/Heat (1995)-trailer.mkv",
        "Heat (1995)/Heat (1995)-trailer2.mkv",
        "Heat (1995)/Heat (1995)-behindthescenes.mkv",
        "Heat (1995)/theme.mp3",
        "Heat (1995)/backdrops/Loop.mkv",
    ])
    .await;
    library.scan().await;
    library
        .rename("Heat (1995)/Heat (1995)-trailer.mkv", "Heat (1995)-trailer")
        .await;
    library
        .rename("Heat (1995)/Heat (1995)-trailer2.mkv", "Teaser")
        .await;
    library
        .rename("Heat (1995)/Heat (1995)-behindthescenes.mkv", "Heat")
        .await;
    // An audio extra and one named after its file keep a chosen name too.
    library.rename("Heat (1995)/theme.mp3", "Main Title").await;
    library
        .rename("Heat (1995)/backdrops/Loop.mkv", "Opening Shot")
        .await;
    library.scan().await;
    assert_eq!(
        library.extras().await,
        names(&[
            (
                "Heat (1995)/Heat (1995)-behindthescenes.mkv",
                "Behind The Scenes"
            ),
            ("Heat (1995)/Heat (1995)-trailer.mkv", "Trailer"),
            ("Heat (1995)/Heat (1995)-trailer2.mkv", "Teaser"),
            ("Heat (1995)/backdrops/Loop.mkv", "Opening Shot"),
            ("Heat (1995)/theme.mp3", "Main Title"),
        ])
    );
}
