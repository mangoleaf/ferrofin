//! Parity tests for the XbmcMetadata NFO parsers — transliterated verbatim from
//! the Jellyfin xUnit suites (`Jellyfin.XbmcMetadata.Tests/Parsers/*`).
//!
//! The C# `Assert.Equal`/`Assert.Contains` expectations are the oracle. Fixtures
//! are the same `.nfo` files, copied into `tests/data`. File reads happen in the
//! test (the parser takes contents); the local-image directory lookup uses a
//! fake matching the C# `Mock<IDirectoryService>` setup.

use std::fs;

use hermit_model::data::PersonKind;
use hermit_model::dto::DayOfWeek;
use hermit_model::entities::{ImageType, SeriesStatus, Video3DFormat};

use hermit_providers::container_types::{FileSystemMetadata, MetadataResult};
use hermit_providers::xbmc::base_parser::{DirectoryService, NoDirectoryService};
use hermit_providers::xbmc::config::NfoConfiguration;
use hermit_providers::xbmc::item::{NfoBaseItem, NfoItemKind};
use hermit_providers::xbmc::{
    StaticExternalIds, fetch_episode, fetch_movie, fetch_season, fetch_series, new_result,
};

const TICKS_PER_SECOND: i64 = 10_000_000;

/// Reads a fixture from `tests/data` by its C# `Test Data/<name>` path.
fn read_fixture(test_data_path: &str) -> String {
    let name = test_data_path.trim_start_matches("Test Data/");
    fs::read_to_string(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

/// A fake [`DirectoryService`] resolving exactly one absolute path to an
/// existing file — mirroring the C# `Mock<IDirectoryService>.GetFile` setup.
struct FakeDirectoryService {
    full_name: String,
}

impl DirectoryService for FakeDirectoryService {
    fn get_file(&self, path: &str) -> Option<FileSystemMetadata> {
        if path == self.full_name {
            Some(FileSystemMetadata {
                exists: true,
                full_name: self.full_name.clone(),
                name: self
                    .full_name
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
                ..FileSystemMetadata::default()
            })
        } else {
            None
        }
    }
}

fn get_provider_id<'a>(item: &'a NfoBaseItem, provider: &str) -> Option<&'a str> {
    item.provider_ids.get(provider).map(String::as_str)
}

fn people_of(
    result: &MetadataResult<NfoBaseItem>,
    kind: PersonKind,
) -> Vec<&hermit_providers::container_types::PersonInfo> {
    result
        .people
        .as_ref()
        .map(|p| p.iter().filter(|x| x.type_ == kind).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// MovieNfoParserTests
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::too_many_lines)] // Verbatim transliteration of the C# Fetch_Valid_Success oracle.
fn movie_fetch_valid_success() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let config = NfoConfiguration {
        user_id: Some("F38E6443-090B-4F7A-BD12-9CFF5020F7BC".to_owned()),
        ..NfoConfiguration::default()
    };
    let local_path = "/media/movies/Justice League (2017).jpg";
    let dir = FakeDirectoryService {
        full_name: local_path.to_owned(),
    };

    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "Test Data/Justice League.nfo",
        &read_fixture("Test Data/Justice League.nfo"),
        &config,
        &external_ids,
        &dir,
    )
    .expect("fetch succeeds");
    let item = &result.item;

    assert_eq!(item.original_title.as_deref(), Some("Justice League"));
    assert_eq!(item.tagline.as_deref(), Some("Justice for all."));
    assert_eq!(get_provider_id(item, "Imdb"), Some("tt0974015"));
    assert_eq!(get_provider_id(item, "Tmdb"), Some("141052"));

    assert_eq!(item.genres.len(), 4);
    assert!(item.genres.contains(&"Action".to_owned()));
    assert!(item.genres.contains(&"Adventure".to_owned()));
    assert!(item.genres.contains(&"Fantasy".to_owned()));
    assert!(item.genres.contains(&"Sci-Fi".to_owned()));

    assert_eq!(
        item.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
        Some("2017-11-15".to_owned())
    );
    assert_eq!(
        item.end_date.map(|d| d.format("%Y-%m-%d").to_string()),
        Some("2017-11-16".to_owned())
    );
    assert_eq!(item.studios.len(), 1);
    assert!(item.studios.contains(&"DC Comics".to_owned()));

    assert_eq!(item.aspect_ratio.as_deref(), Some("1.777778"));
    assert_eq!(item.video_3d_format, Some(Video3DFormat::HalfSideBySide));
    assert_eq!(item.width, Some(1920));
    assert_eq!(item.height, Some(1080));
    assert_eq!(item.run_time_ticks, Some(6268 * TICKS_PER_SECOND));
    assert!(item.has_subtitles);
    assert_eq!(item.critic_rating, Some(7.6));
    assert_eq!(item.custom_rating.as_deref(), Some("8.7"));
    assert_eq!(item.preferred_metadata_language.as_deref(), Some("en"));
    assert_eq!(item.preferred_metadata_country_code.as_deref(), Some("us"));
    assert_eq!(item.remote_trailers.len(), 1);
    assert_eq!(
        item.remote_trailers[0],
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
    );

    assert_eq!(result.people.as_ref().unwrap().len(), 20);

    let writers = people_of(&result, PersonKind::Writer);
    assert_eq!(writers.len(), 3);
    let writer_names: Vec<&str> = writers.iter().map(|w| w.name.as_str()).collect();
    assert!(writer_names.contains(&"Jerry Siegel"));
    assert!(writer_names.contains(&"Joe Shuster"));
    assert!(writer_names.contains(&"Test"));

    let directors = people_of(&result, PersonKind::Director);
    assert_eq!(directors.len(), 1);
    assert_eq!(directors[0].name, "Zack Snyder");

    let actors = people_of(&result, PersonKind::Actor);
    assert_eq!(actors.len(), 15);

    let aquaman = actors
        .iter()
        .find(|x| x.role.as_deref() == Some("Aquaman"))
        .expect("aquaman present");
    assert_eq!(aquaman.name, "Jason Momoa");
    assert_eq!(aquaman.sort_order, Some(5));
    assert_eq!(
        aquaman.image_url.as_deref(),
        Some(
            "https://m.media-amazon.com/images/M/MV5BMTI5MTU5NjM1MV5BMl5BanBnXkFtZTcwODc4MDk0Mw@@._V1_SX1024_SY1024_.jpg"
        )
    );

    let lyricist = people_of(&result, PersonKind::Lyricist);
    assert_eq!(lyricist.len(), 1);
    assert_eq!(lyricist[0].name, "Test Lyricist");

    assert_eq!(
        item.date_created
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string()),
        Some("2019-08-06 09:01:18".to_owned())
    );

    // Movie set
    assert_eq!(get_provider_id(item, "TmdbCollection"), Some("702342"));
    assert_eq!(
        item.collection_name.as_deref(),
        Some("Justice League Collection")
    );

    // Images
    assert_eq!(result.remote_images.len(), 7);
    let by_type = |t: ImageType| -> Vec<&(String, ImageType)> {
        result
            .remote_images
            .iter()
            .filter(|(_, ty)| *ty == t)
            .collect()
    };
    assert_eq!(by_type(ImageType::Primary).len(), 1);
    assert_eq!(
        by_type(ImageType::Primary)[0].0,
        "http://image.tmdb.org/t/p/original/9rtrRGeRnL0JKtu9IMBWsmlmmZz.jpg"
    );
    assert_eq!(by_type(ImageType::Logo).len(), 1);
    assert_eq!(
        by_type(ImageType::Logo)[0].0,
        "https://assets.fanart.tv/fanart/movies/141052/hdmovielogo/justice-league-5865bf95cbadb.png"
    );
    assert_eq!(by_type(ImageType::Banner).len(), 1);
    assert_eq!(
        by_type(ImageType::Banner)[0].0,
        "https://assets.fanart.tv/fanart/movies/141052/moviebanner/justice-league-586017e95adbd.jpg"
    );
    assert_eq!(by_type(ImageType::Thumb).len(), 1);
    assert_eq!(
        by_type(ImageType::Thumb)[0].0,
        "https://assets.fanart.tv/fanart/movies/141052/moviethumb/justice-league-585fb155c3743.jpg"
    );
    assert_eq!(by_type(ImageType::Art).len(), 1);
    assert_eq!(
        by_type(ImageType::Art)[0].0,
        "https://assets.fanart.tv/fanart/movies/141052/hdmovieclearart/justice-league-5865c23193041.png"
    );
    assert_eq!(by_type(ImageType::Disc).len(), 1);
    assert_eq!(
        by_type(ImageType::Disc)[0].0,
        "https://assets.fanart.tv/fanart/movies/141052/moviedisc/justice-league-5a3af26360617.png"
    );
    assert_eq!(by_type(ImageType::Backdrop).len(), 1);
    assert_eq!(
        by_type(ImageType::Backdrop)[0].0,
        "https://assets.fanart.tv/fanart/movies/141052/moviebackground/justice-league-5793f518c6d6e.jpg"
    );

    // Local image
    assert_eq!(result.images.len(), 1);
    assert_eq!(result.images[0].file_info.name, "Justice League (2017).jpg");
}

#[test]
fn movie_parse_url_file_success() {
    for (path, provider, id) in [
        ("Test Data/Tmdb.nfo", "Tmdb", "30287"),
        ("Test Data/Imdb.nfo", "Imdb", "tt0944947"),
    ] {
        let external_ids = StaticExternalIds::new(["Tmdb"]);
        let mut result = new_result(NfoItemKind::Movie);
        fetch_movie(
            &mut result,
            path,
            &read_fixture(path),
            &NfoConfiguration::default(),
            &external_ids,
            &NoDirectoryService,
        )
        .expect("fetch succeeds");
        assert_eq!(get_provider_id(&result.item, provider), Some(id));
    }
}

#[test]
fn movie_parse_fanart_tag_success() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "Test Data/Fanart.nfo",
        &read_fixture("Test Data/Fanart.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");

    let backdrops: Vec<&(String, ImageType)> = result
        .remote_images
        .iter()
        .filter(|(_, t)| *t == ImageType::Backdrop)
        .collect();
    assert_eq!(backdrops.len(), 1);
    assert_eq!(
        backdrops[0].0,
        "https://assets.fanart.tv/fanart/movies/141052/moviebackground/justice-league-5a5332c7b5e77.jpg"
    );
}

#[test]
fn movie_parse_radarr_url_file_success() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "Test Data/Radarr.nfo",
        &read_fixture("Test Data/Radarr.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(get_provider_id(&result.item, "Tmdb"), Some("583689"));
    assert_eq!(get_provider_id(&result.item, "Imdb"), Some("tt4154796"));
}

#[test]
fn movie_fetch_empty_path_errors() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    let err = fetch_movie(
        &mut result,
        "",
        "",
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    );
    assert!(err.is_err());
}

#[test]
fn movie_parse_escaped_xml_special_characters() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "Test Data/Lilo & Stitch.nfo",
        &read_fixture("Test Data/Lilo & Stitch.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    let item = &result.item;

    assert_eq!(item.name.as_deref(), Some("Lilo & Stitch"));
    assert_eq!(item.original_title.as_deref(), Some("Lilo & Stitch"));
    assert_eq!(
        item.collection_name.as_deref(),
        Some("Lilo & Stitch Collection")
    );
    assert!(item.overview.as_deref().unwrap().starts_with(">>"));
    assert!(item.overview.as_deref().unwrap().ends_with("<<"));
}

#[test]
fn movie_parse_tmdbcol_uniqueid_normalized() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "Test Data/Lilo & Stitch.nfo",
        &read_fixture("Test Data/Lilo & Stitch.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    let item = &result.item;

    assert!(item.provider_ids.contains_key("TmdbCollection"));
    assert_eq!(get_provider_id(item, "TmdbCollection"), Some("97020"));
    assert!(!item.provider_ids.contains_key("tmdbcol"));
}

#[test]
fn movie_community_rating_valid() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "Test Data/CommunityRating.nfo",
        &read_fixture("Test Data/CommunityRating.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(result.item.community_rating, Some(7.5));
}

#[test]
fn movie_community_rating_out_of_range_ignored() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "Test Data/CommunityRating_OutOfRange.nfo",
        &read_fixture("Test Data/CommunityRating_OutOfRange.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(result.item.community_rating, None);
}

#[test]
fn movie_community_rating_comma() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "Test Data/CommunityRating_Comma.nfo",
        &read_fixture("Test Data/CommunityRating_Comma.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(result.item.community_rating, Some(7.5));
}

// ---------------------------------------------------------------------------
// MusicVideoNfoParserTests (reuses the movie parser)
// ---------------------------------------------------------------------------

#[test]
fn music_video_fetch_valid_success() {
    let external_ids = StaticExternalIds::new(Vec::<String>::new());
    let mut result = new_result(NfoItemKind::MusicVideo);
    fetch_movie(
        &mut result,
        "Test Data/Dancing Queen.nfo",
        &read_fixture("Test Data/Dancing Queen.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    let item = &result.item;

    assert_eq!(item.name.as_deref(), Some("Dancing Queen"));
    assert_eq!(item.artists.len(), 1);
    assert!(item.artists.contains(&"ABBA".to_owned()));
    assert_eq!(item.album.as_deref(), Some("Arrival"));
}

// ---------------------------------------------------------------------------
// EpisodeNfoProviderTests
// ---------------------------------------------------------------------------

#[test]
fn episode_fetch_valid_success() {
    let external_ids = StaticExternalIds::new(["Imdb"]);
    let mut result = new_result(NfoItemKind::Episode);
    fetch_episode(
        &mut result,
        "Test Data/The Bone Orchard.nfo",
        &read_fixture("Test Data/The Bone Orchard.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    let item = &result.item;

    assert_eq!(item.name.as_deref(), Some("The Bone Orchard"));
    assert_eq!(item.series_name.as_deref(), Some("American Gods"));
    assert_eq!(item.index_number, Some(1));
    assert_eq!(item.parent_index_number, Some(1));
    assert_eq!(
        item.overview.as_deref(),
        Some(
            "When Shadow Moon is released from prison early after the death of his wife, he meets Mr. Wednesday and is recruited as his bodyguard. Shadow discovers that this may be more than he bargained for."
        )
    );
    assert_eq!(item.run_time_ticks, Some(0));
    assert_eq!(item.official_rating.as_deref(), Some("16"));
    assert!(item.genres.contains(&"Drama".to_owned()));
    assert!(item.genres.contains(&"Mystery".to_owned()));
    assert!(item.genres.contains(&"Sci-Fi & Fantasy".to_owned()));
    assert_eq!(
        item.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
        Some("2017-04-30".to_owned())
    );
    assert_eq!(item.production_year, Some(2017));
    assert_eq!(item.studios.len(), 1);
    assert!(item.studios.contains(&"Starz".to_owned()));
    assert_eq!(item.index_number_end, Some(1));
    assert_eq!(item.airs_after_season_number, Some(2));
    assert_eq!(item.airs_before_season_number, Some(3));
    assert_eq!(item.airs_before_episode_number, Some(1));
    assert_eq!(get_provider_id(item, "Imdb"), Some("tt5017734"));
    assert_eq!(get_provider_id(item, "Tmdb"), Some("1276153"));

    let writers = people_of(&result, PersonKind::Writer);
    assert_eq!(writers.len(), 2);
    let wn: Vec<&str> = writers.iter().map(|w| w.name.as_str()).collect();
    assert!(wn.contains(&"Bryan Fuller"));
    assert!(wn.contains(&"Michael Green"));

    let directors = people_of(&result, PersonKind::Director);
    assert_eq!(directors.len(), 1);
    assert_eq!(directors[0].name, "David Slade");

    let actors = people_of(&result, PersonKind::Actor);
    assert_eq!(actors.len(), 11);
    let shadow = actors
        .iter()
        .find(|x| x.role.as_deref() == Some("Shadow Moon"))
        .expect("shadow present");
    assert_eq!(shadow.name, "Ricky Whittle");
    assert_eq!(shadow.sort_order, Some(0));
    assert_eq!(
        shadow.image_url.as_deref(),
        Some("http://image.tmdb.org/t/p/original/cjeDbVfBp6Qvb3C74Dfy7BKDTQN.jpg")
    );

    assert_eq!(
        item.date_created
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string()),
        Some("2017-10-07 14:25:47".to_owned())
    );
}

#[test]
fn episode_fetch_multi_episode_success() {
    let external_ids = StaticExternalIds::new(["Imdb"]);
    let mut result = new_result(NfoItemKind::Episode);
    fetch_episode(
        &mut result,
        "Test Data/Rising.nfo",
        &read_fixture("Test Data/Rising.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    let item = &result.item;

    assert_eq!(item.name.as_deref(), Some("Rising (1) / Rising (2)"));
    assert_eq!(item.index_number, Some(1));
    assert_eq!(item.index_number_end, Some(2));
    assert_eq!(item.parent_index_number, Some(1));
    assert_eq!(
        item.overview.as_deref(),
        Some(
            "A new Stargate team embarks on a dangerous mission to a distant galaxy, where they discover a mythical lost city -- and a deadly new enemy. / Sheppard tries to convince Weir to mount a rescue mission to free Colonel Sumner, Teyla, and the others captured by the Wraith."
        )
    );
    assert_eq!(
        item.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
        Some("2004-07-16".to_owned())
    );
    assert_eq!(item.production_year, Some(2004));
}

#[test]
fn episode_fetch_multi_episode_with_missing_tags() {
    let external_ids = StaticExternalIds::new(["Imdb"]);
    let mut result = new_result(NfoItemKind::Episode);
    fetch_episode(
        &mut result,
        "Test Data/Stargate Atlantis S01E01-E04.nfo",
        &read_fixture("Test Data/Stargate Atlantis S01E01-E04.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    let item = &result.item;

    assert_eq!(
        item.name.as_deref(),
        Some("Rising / Hide and Seek / Thirty-Eight Minutes")
    );
    assert_eq!(
        item.original_title.as_deref(),
        Some("Rising (1) / Rising (2) / Hide and Seek / Thirty-Eight Minutes")
    );
    assert_eq!(item.index_number, Some(1));
    assert_eq!(item.index_number_end, Some(4));
    assert_eq!(item.parent_index_number, Some(1));
    assert_eq!(
        item.overview.as_deref(),
        Some(
            "A new Stargate team embarks on a dangerous mission to a distant galaxy, where they discover a mythical lost city -- and a deadly new enemy."
        )
    );
    assert_eq!(
        item.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
        Some("2004-07-16".to_owned())
    );
    assert_eq!(item.production_year, Some(2004));
}

#[test]
fn episode_thumb_without_aspect() {
    let external_ids = StaticExternalIds::new(["Imdb"]);
    let mut result = new_result(NfoItemKind::Episode);
    fetch_episode(
        &mut result,
        "Test Data/Sonarr-Thumb.nfo",
        &read_fixture("Test Data/Sonarr-Thumb.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");

    let primary: Vec<&(String, ImageType)> = result
        .remote_images
        .iter()
        .filter(|(_, t)| *t == ImageType::Primary)
        .collect();
    assert_eq!(primary.len(), 1);
    assert_eq!(
        primary[0].0,
        "https://artworks.thetvdb.com/banners/episodes/359095/7081317.jpg"
    );
}

#[test]
fn episode_fetch_empty_path_errors() {
    let external_ids = StaticExternalIds::new(["Imdb"]);
    let mut result = new_result(NfoItemKind::Episode);
    assert!(
        fetch_episode(
            &mut result,
            "",
            "",
            &NfoConfiguration::default(),
            &external_ids,
            &NoDirectoryService,
        )
        .is_err()
    );
}

// ---------------------------------------------------------------------------
// SeriesNfoParserTests
// ---------------------------------------------------------------------------

#[test]
fn series_fetch_valid_success() {
    let external_ids = StaticExternalIds::new(Vec::<String>::new());
    let mut result = new_result(NfoItemKind::Series);
    fetch_series(
        &mut result,
        "Test Data/American Gods.nfo",
        &read_fixture("Test Data/American Gods.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    let item = &result.item;

    assert_eq!(item.original_title.as_deref(), Some("American Gods"));
    assert_eq!(item.tagline.as_deref(), Some(""));
    assert_eq!(item.run_time_ticks, Some(0));
    assert_eq!(get_provider_id(item, "Tmdb"), Some("46639"));
    assert_eq!(get_provider_id(item, "Tvdb"), Some("253573"));
    assert_eq!(get_provider_id(item, "Imdb"), Some("tt11111"));

    assert_eq!(item.genres.len(), 3);
    assert!(item.genres.contains(&"Drama".to_owned()));
    assert!(item.genres.contains(&"Mystery".to_owned()));
    assert!(item.genres.contains(&"Sci-Fi & Fantasy".to_owned()));

    assert_eq!(
        item.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
        Some("2017-04-30".to_owned())
    );
    assert_eq!(item.studios.len(), 1);
    assert!(item.studios.contains(&"Starz".to_owned()));
    assert_eq!(item.air_time.as_deref(), Some("9 PM"));
    assert_eq!(item.air_days.len(), 1);
    assert!(item.air_days.contains(&DayOfWeek::Friday));
    assert_eq!(item.status, Some(SeriesStatus::Ended));

    assert_eq!(result.people.as_ref().unwrap().len(), 6);
    assert!(
        result
            .people
            .as_ref()
            .unwrap()
            .iter()
            .all(|p| p.type_ == PersonKind::Actor)
    );

    let sweeney = result
        .people
        .as_ref()
        .unwrap()
        .iter()
        .find(|x| x.role.as_deref() == Some("Mad Sweeney"))
        .expect("sweeney present");
    assert_eq!(sweeney.name, "Pablo Schreiber");
    assert_eq!(sweeney.sort_order, Some(3));
    assert_eq!(
        sweeney.image_url.as_deref(),
        Some("http://image.tmdb.org/t/p/original/uo8YljeePz3pbj7gvWXdB4gOOW4.jpg")
    );

    assert_eq!(
        item.date_created
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string()),
        Some("2017-10-07 14:25:47".to_owned())
    );
}

#[test]
fn series_parse_url_file_success() {
    let external_ids = StaticExternalIds::new(Vec::<String>::new());
    let mut result = new_result(NfoItemKind::Series);
    fetch_series(
        &mut result,
        "Test Data/Tvdb.nfo",
        &read_fixture("Test Data/Tvdb.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(get_provider_id(&result.item, "Tvdb"), Some("121361"));
}

// ---------------------------------------------------------------------------
// SeasonNfoProviderTests
// ---------------------------------------------------------------------------

#[test]
fn season_fetch_valid_success() {
    let external_ids = StaticExternalIds::new(Vec::<String>::new());
    let mut result = new_result(NfoItemKind::Season);
    fetch_season(
        &mut result,
        "Test Data/Season 01.nfo",
        &read_fixture("Test Data/Season 01.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    let item = &result.item;

    assert_eq!(item.name.as_deref(), Some("Season 1"));
    assert_eq!(item.index_number, Some(1));
    assert!(!item.is_locked);
    assert_eq!(item.production_year, Some(2019));
    assert_eq!(
        item.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
        Some("2019-11-08".to_owned())
    );
    assert_eq!(
        item.date_created
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string()),
        Some("2020-06-14 17:26:51".to_owned())
    );

    assert_eq!(result.people.as_ref().unwrap().len(), 10);
    assert!(
        result
            .people
            .as_ref()
            .unwrap()
            .iter()
            .all(|p| p.type_ == PersonKind::Actor)
    );

    let nini = result
        .people
        .as_ref()
        .unwrap()
        .iter()
        .find(|x| x.role.as_deref() == Some("Nini"))
        .expect("nini present");
    assert_eq!(nini.name, "Olivia Rodrigo");
    assert_eq!(nini.sort_order, Some(0));
    assert_eq!(
        nini.image_url.as_deref(),
        Some("/config/metadata/People/O/Olivia Rodrigo/poster.jpg")
    );
}
