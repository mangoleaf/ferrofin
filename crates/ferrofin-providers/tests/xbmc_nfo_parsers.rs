//! Parity tests for the XbmcMetadata NFO parsers — transliterated verbatim from
//! the Jellyfin xUnit suites (`Jellyfin.XbmcMetadata.Tests/Parsers/*`).
//!
//! The C# `Assert.Equal`/`Assert.Contains` expectations are the oracle. Fixtures
//! are the same `.nfo` files, copied into `tests/data`. File reads happen in the
//! test (the parser takes contents); the local-image directory lookup uses a
//! fake matching the C# `Mock<IDirectoryService>` setup.

use std::fs;
use std::sync::Arc;

use ferrofin_model::data::PersonKind;
use ferrofin_model::dto::DayOfWeek;
use ferrofin_model::entities::{ImageType, SeriesStatus, Video3DFormat};

use ferrofin_providers::DirectoryService;
use ferrofin_providers::container_types::{FileSystemMetadata, MetadataResult};
use ferrofin_providers::xbmc::base_parser::NoDirectoryService;
use ferrofin_providers::xbmc::base_parser::{FetchError, FetchResult};
use ferrofin_providers::xbmc::config::NfoConfiguration;
use ferrofin_providers::xbmc::item::{NfoBaseItem, NfoItemKind};
use ferrofin_providers::xbmc::{
    StaticExternalIds, fetch_episode, fetch_movie, fetch_music, fetch_season, fetch_series,
    new_result,
};
use ferrofin_util::ticks::TICKS_PER_SECOND;

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
    fn file_system_entries(&self, _path: &str) -> Arc<[FileSystemMetadata]> {
        Arc::from([])
    }

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

// The service the scan parses NFOs with resolves no path, even an existing
// file: an NFO's local `<thumb>` must not reach the item until gl-1818 decides
// which files an NFO may name.
#[test]
fn no_directory_service_resolves_no_file() {
    let existing = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    assert!(std::path::Path::new(existing).is_file());
    assert_eq!(NoDirectoryService.get_file(existing), None);
}

fn get_provider_id<'a>(item: &'a NfoBaseItem, provider: &str) -> Option<&'a str> {
    item.provider_ids.get(provider).map(String::as_str)
}

fn people_of(
    result: &MetadataResult<NfoBaseItem>,
    kind: PersonKind,
) -> Vec<&ferrofin_providers::container_types::PersonInfo> {
    result
        .people
        .as_ref()
        .map_or_default(|p| p.iter().filter(|x| x.type_ == kind).collect())
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

/// A trailer with a multi-byte character across the YouTube plugin prefix's
/// length (45 bytes; `é` spans bytes 44-45 here) is no plugin link: it is
/// skipped, not sliced inside the character, and the next trailer is read.
#[test]
fn movie_parse_non_ascii_trailer_across_the_plugin_prefix_length() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    let not_a_plugin_link = format!("https://example.org/{}é", "x".repeat(24));
    let nfo = format!(
        "<movie><title>Trailers</title>\
         <trailer>{not_a_plugin_link}</trailer>\
         <trailer>plugin://plugin.video.youtube/play/?video_id=dQw4w9WgXcQ</trailer>\
         </movie>"
    );
    fetch_movie(
        &mut result,
        "/media/Trailers.nfo",
        &nfo,
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");

    assert_eq!(
        result.item.remote_trailers,
        ["https://www.youtube.com/watch?v=dQw4w9WgXcQ"]
    );
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

// MovieNfoParserTests.Parse_Ratings_NotFinite_Ignored (v12.4): `float.TryParse`
// reads `NaN` and `Infinity`, and the `BaseItem` setters' `FiniteOrNull` stores
// them as no value.
#[test]
fn movie_ratings_not_finite_ignored() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "Test Data/Ratings_NotFinite.nfo",
        &read_fixture("Test Data/Ratings_NotFinite.nfo"),
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(result.item.community_rating, None);
    assert_eq!(result.item.critic_rating, None);
}

/// .NET's `float.TryParse` reads the `NaN` and `Infinity` symbols in any case
/// and whatever the `NumberStyles`, so upstream still assigns: the setter turns
/// the value into no value, which clears a rating an earlier element set
/// (`BaseNfoParser.cs` v12.4 :291-293, :537-539 and :969-983). Rust's parser
/// also reads `inf`, which .NET does not; that spelling is the parsers'
/// accepted sets differing, not `FiniteOrNull`, so it is not a case here.
#[rstest::rstest]
#[case::rating("<communityrating>7.5</communityrating><rating>nan</rating>")]
#[case::criticrating(
    r#"<ratings><rating name="tomatometerallcritics"><value>80</value></rating></ratings>
       <criticrating>Infinity</criticrating>"#
)]
// `<criticrating>` parses with the default `Float` styles, which read an
// exponent, and .NET Core rounds an overflow to infinity. (`<rating>` and
// `<ratings><value>` allow only a decimal point, so `1e40` fails to parse there
// and upstream keeps the earlier value.)
#[case::criticrating_overflow(
    r#"<ratings><rating name="tomatometerallcritics"><value>80</value></rating></ratings>
       <criticrating>1e40</criticrating>"#
)]
#[case::ratings_community(
    r#"<rating>7.5</rating><ratings><rating name="imdb"><value>NaN</value></rating></ratings>"#
)]
#[case::ratings_critic(
    r#"<criticrating>80</criticrating>
       <ratings><rating name="tomatometerallcritics"><value>-Infinity</value></rating></ratings>"#
)]
fn a_non_finite_rating_clears_an_earlier_one(#[case] elements: &str) {
    let nfo = format!(r#"<?xml version="1.0" encoding="utf-8"?><movie>{elements}</movie>"#);
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "Test Data/Ratings_Cleared.nfo",
        &nfo,
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(result.item.community_rating, None);
    assert_eq!(result.item.critic_rating, None);
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

/// Port of v12.2 `Fetch_Valid_MultiEpisode_Unordered_Success` (PR 17805):
/// the `<episodedetails>` blocks are stored in descending order (as Sonarr
/// writes them), and the merged episode must still be in ascending order.
#[test]
fn episode_fetch_multi_episode_unordered_success() {
    let external_ids = StaticExternalIds::new(["Imdb"]);
    let mut result = new_result(NfoItemKind::Episode);
    fetch_episode(
        &mut result,
        "Test Data/Rising-Reversed.nfo",
        &read_fixture("Test Data/Rising-Reversed.nfo"),
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

// ---------------------------------------------------------------------------
// GetPersonFromXmlNode `<type>` (not in the C# suites)
// ---------------------------------------------------------------------------

/// `XmlReaderExtensions.GetPersonFromXmlNode` reads `<type>` with
/// `Enum.TryParse(text, true, out type)`: any member name in any case binds,
/// and a value that does not parse leaves `default(PersonKind)`, `Unknown`,
/// not the `Actor` a missing `<type>` keeps.
#[test]
fn person_type_parses_like_enum_try_parse() {
    let nfo = r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<movie>
  <title>T</title>
  <actor><name>Missing</name></actor>
  <actor><name>Lower</name><type>lyricist</type></actor>
  <actor><name>Comic</name><type>CoverArtist</type></actor>
  <actor><name>Bogus</name><type>Chief Vibes Officer</type></actor>
  <actor><name>Empty</name><type></type></actor>
</movie>"#;
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "inline.nfo",
        nfo,
        &NfoConfiguration::default(),
        &StaticExternalIds::new(["Tmdb"]),
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    let kinds: Vec<(&str, PersonKind)> = result
        .people
        .as_ref()
        .expect("people")
        .iter()
        .map(|person| (person.name.as_str(), person.type_))
        .collect();
    assert_eq!(
        kinds,
        [
            ("Missing", PersonKind::Actor),
            ("Lower", PersonKind::Lyricist),
            ("Comic", PersonKind::CoverArtist),
            ("Bogus", PersonKind::Unknown),
            ("Empty", PersonKind::Unknown),
        ]
    );
}

/// `MovieNfoParser`'s `Fetch` (the `SupportsUrlAfterClosingXmlTag` path)
/// swallows `XmlException`, so one malformed spot keeps every element read
/// before it: here a bare `&` in `<plot>` keeps the title and the pinned
/// TMDb id, and neither the plot nor anything after it.
#[test]
fn a_malformed_movie_nfo_keeps_the_elements_before_the_error() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "movie.nfo",
        "<movie><title>Ronin</title><tmdbid>8195</tmdbid>\
         <plot>Tom & Jerry</plot><year>1998</year></movie>",
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(result.item.name.as_deref(), Some("Ronin"));
    assert_eq!(get_provider_id(&result.item, "Tmdb"), Some("8195"));
    assert_eq!(result.item.overview, None, "the element with the error");
    assert_eq!(result.item.production_year, None, "nothing after it");
}

/// `EpisodeNfoParser.Fetch` swallows `XmlException` too: an `&nbsp;` (not an
/// XML entity) in a late `<actor>` keeps the fields before it, and the actor
/// it sits in is not added.
#[test]
fn a_malformed_episode_nfo_keeps_the_elements_before_the_error() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Episode);
    fetch_episode(
        &mut result,
        "episode.nfo",
        "<episodedetails><title>Pilot</title><season>1</season><episode>2</episode>\
         <actor><name>Ann&nbsp;Lee</name></actor></episodedetails>",
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(result.item.name.as_deref(), Some("Pilot"));
    assert_eq!(result.item.index_number, Some(2));
    let people = result.people.unwrap_or_default();
    assert!(people.is_empty(), "{people:?}");
}

/// The plain `Fetch` (`SeasonNfoParser`, and the album/artist parsers) does not
/// catch `XmlException`: it reaches the provider, whose result is not used,
/// so a malformed season NFO is a failed fetch rather than an empty success.
#[test]
fn a_malformed_season_nfo_is_a_failed_fetch() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Season);
    let fetched = fetch_season(
        &mut result,
        "season.nfo",
        "<season><title>Season 1</title><plot>a & b</plot></season>",
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    );
    assert!(fetched.is_err(), "{fetched:?}");
}

/// `SeriesNfoParser` shares the movie's swallowing path: the elements before a
/// malformed spot stand.
#[test]
fn a_malformed_series_nfo_keeps_the_elements_before_the_error() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Series);
    fetch_series(
        &mut result,
        "tvshow.nfo",
        "<tvshow><title>Show</title><plot>a &nbsp; b</plot><year>2001</year></tvshow>",
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(result.item.name.as_deref(), Some("Show"));
    assert_eq!(result.item.overview, None);
    assert_eq!(result.item.production_year, None);
}

/// An `album.nfo`/`artist.nfo` reads through the plain `Fetch`, which does not
/// recover from a malformed document.
#[test]
fn a_malformed_music_nfo_is_a_failed_fetch() {
    let external_ids = StaticExternalIds::new(["MusicBrainzAlbum"]);
    let mut result = new_result(NfoItemKind::MusicAlbum);
    let fetched = fetch_music(
        &mut result,
        "album.nfo",
        "<album><title>Kind of Blue</title><review>A & B</review></album>",
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    );
    assert_eq!(fetched, Err(FetchError::MalformedXml));
}

/// With several `<episodedetails>` blocks, C# parses them all before merging,
/// inside one `catch (XmlException)`: a malformed block ends the read before
/// anything reaches the item.
#[test]
fn a_malformed_block_of_a_multi_episode_nfo_merges_nothing() {
    let external_ids = StaticExternalIds::new(["Tmdb"]);
    let mut result = new_result(NfoItemKind::Episode);
    fetch_episode(
        &mut result,
        "episode.nfo",
        "<episodedetails><title>One</title><episode>1</episode></episodedetails>\
         <episodedetails><title>Two & a half</title><episode>2</episode></episodedetails>",
        &NfoConfiguration::default(),
        &external_ids,
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(result.item.name, None);
    assert_eq!(result.item.index_number, None);
}

/// gl-2020: `_validProviderIds` is an `OrdinalIgnoreCase` dictionary, so a
/// plugin provider's id tag matches in another case beyond ASCII.
#[test]
fn a_plugin_id_tag_matches_in_another_non_ascii_case() {
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "plugin.nfo",
        "<movie><title>T</title><äRZTEID>42</äRZTEID></movie>",
        &NfoConfiguration::default(),
        &StaticExternalIds::new(["Ärzte"]),
        &NoDirectoryService,
    )
    .expect("fetch succeeds");
    assert_eq!(get_provider_id(&result.item, "Ärzte"), Some("42"));
}

// ---------------------------------------------------------------------------
// Typed reads: `ReadElementContentAsBoolean` (`<watched>`) and
// `ReadElementContentAsInt` (fileinfo `<width>`, `<height>`,
// `<durationinseconds>`) throw where the other reads only skip a value.
// ---------------------------------------------------------------------------

/// Parses `xml` as a movie NFO.
fn fetch_movie_xml(xml: &str) -> (FetchResult, MetadataResult<NfoBaseItem>) {
    let mut result = new_result(NfoItemKind::Movie);
    let fetched = fetch_movie(
        &mut result,
        "movie.nfo",
        xml,
        &NfoConfiguration::default(),
        &StaticExternalIds::new(["Tmdb"]),
        &NoDirectoryService,
    );
    (fetched, result)
}

/// Parses `xml` as an episode NFO.
fn fetch_episode_xml(xml: &str) -> (FetchResult, MetadataResult<NfoBaseItem>) {
    let mut result = new_result(NfoItemKind::Episode);
    let fetched = fetch_episode(
        &mut result,
        "episode.nfo",
        xml,
        &NfoConfiguration::default(),
        &StaticExternalIds::new(["Tmdb"]),
        &NoDirectoryService,
    );
    (fetched, result)
}

/// Parses `xml` as a season NFO.
fn fetch_season_xml(xml: &str) -> FetchResult {
    fetch_season(
        &mut new_result(NfoItemKind::Season),
        "season.nfo",
        xml,
        &NfoConfiguration::default(),
        &StaticExternalIds::new(["Tmdb"]),
        &NoDirectoryService,
    )
}

/// `XmlConvert.ToBoolean` takes exactly `true`, `false`, `1` or `0` after
/// trimming XML whitespace (space, tab, CR, LF). Anything else, a child
/// element included, is an `XmlException`. The movie parser catches it and
/// stops: the title before `<watched>` stays, the plot after it is lost (stock
/// 12.4 read `<watched>True</watched>` that way).
#[rstest::rstest]
#[case::lower_true("true", Some("After"))]
#[case::lower_false("false", Some("After"))]
#[case::one("1", Some("After"))]
#[case::zero("0", Some("After"))]
#[case::xml_whitespace_around(" \ttrue\r\n", Some("After"))]
#[case::capital("True", None)]
#[case::upper("FALSE", None)]
#[case::yes("yes", None)]
#[case::whitespace_only("  ", None)]
#[case::no_break_space_around("\u{a0}true", None)]
#[case::child_element("<b/>", None)]
#[case::child_after_the_text("true<b/>", None)]
fn movie_watched_converts_like_xml_convert(#[case] value: &str, #[case] overview: Option<&str>) {
    let (fetched, result) = fetch_movie_xml(&format!(
        "<movie><title>Before</title><watched>{value}</watched><plot>After</plot></movie>"
    ));
    assert_eq!(fetched, Ok(()), "{value:?}");
    assert_eq!(result.item.name.as_deref(), Some("Before"), "{value:?}");
    assert_eq!(result.item.overview.as_deref(), overview, "{value:?}");
}

/// `XmlConvert.ToInt32` is `int.Parse` with an optional sign, white space
/// (tab to CR, and space) around it, and ASCII digits. A value that does not
/// match is an `XmlException`: the movie keeps what came before it, the
/// `<aspect>` of the same `<video>` included, and loses the rest.
#[rstest::rstest]
#[case::digits("1920", Some(1920), Some("After"))]
#[case::plus_sign("+1920", Some(1920), Some("After"))]
#[case::minus_sign("-5", Some(-5), Some("After"))]
#[case::white_space_around(" \t1920\r\n", Some(1920), Some("After"))]
#[case::letters("abc", None, None)]
#[case::decimal_point("1920.0", None, None)]
#[case::no_break_space_around("\u{a0}1920", None, None)]
#[case::whitespace_only(" ", None, None)]
#[case::bad_format_beats_overflow("99999999999x", None, None)]
#[case::child_element("<b/>", None, None)]
fn movie_fileinfo_width_converts_like_xml_convert(
    #[case] value: &str,
    #[case] width: Option<i32>,
    #[case] overview: Option<&str>,
) {
    let (fetched, result) = fetch_movie_xml(&format!(
        "<movie><title>Before</title><fileinfo><streamdetails><video>\
         <aspect>2.35</aspect><width>{value}</width></video></streamdetails></fileinfo>\
         <plot>After</plot></movie>"
    ));
    assert_eq!(fetched, Ok(()), "{value:?}");
    assert_eq!(result.item.name.as_deref(), Some("Before"), "{value:?}");
    assert_eq!(
        result.item.aspect_ratio.as_deref(),
        Some("2.35"),
        "{value:?}"
    );
    assert_eq!(result.item.width, width, "{value:?}");
    assert_eq!(result.item.overview.as_deref(), overview, "{value:?}");
}

/// `<height>` and `<durationinseconds>` read the same way.
#[test]
fn movie_fileinfo_height_and_duration_stop_at_a_bad_value() {
    let (fetched, result) = fetch_movie_xml(
        "<movie><fileinfo><streamdetails><video><height>1080</height>\
         <durationinseconds>61</durationinseconds></video></streamdetails></fileinfo>\
         <title>Read</title></movie>",
    );
    assert_eq!(fetched, Ok(()));
    assert_eq!(result.item.height, Some(1080));
    assert_eq!(result.item.run_time_ticks, Some(61 * TICKS_PER_SECOND));
    assert_eq!(result.item.name.as_deref(), Some("Read"));

    let (fetched, result) = fetch_movie_xml(
        "<movie><fileinfo><streamdetails><video><height>1080</height>\
         <durationinseconds>1h</durationinseconds></video></streamdetails></fileinfo>\
         <title>Lost</title></movie>",
    );
    assert_eq!(fetched, Ok(()));
    assert_eq!(result.item.height, Some(1080));
    assert_eq!(result.item.run_time_ticks, None);
    assert_eq!(result.item.name, None);
}

/// An empty element is `XmlConvert.ToXxx(string.Empty)`, a `FormatException`
/// outside the `XmlException` wrapping, and an integer out of range is an
/// `OverflowException`. No parser catches either, so the local provider fails
/// and the NFO gives nothing, on the movie and episode paths too. A season is
/// no `Video`, so `FetchFromVideoNode` reads none of its fileinfo values.
#[rstest::rstest]
#[case::self_closing_watched("<watched/>", Err(FetchError::InvalidValue))]
#[case::empty_watched("<watched></watched>", Err(FetchError::InvalidValue))]
#[case::comment_only_watched("<watched><!-- x --></watched>", Err(FetchError::InvalidValue))]
#[case::self_closing_width(
    "<fileinfo><streamdetails><video><width/></video></streamdetails></fileinfo>",
    Ok(())
)]
#[case::empty_height(
    "<fileinfo><streamdetails><video><height></height></video></streamdetails></fileinfo>",
    Ok(())
)]
#[case::duration_overflow(
    "<fileinfo><streamdetails><video><durationinseconds>2147483648</durationinseconds>\
     </video></streamdetails></fileinfo>",
    Ok(())
)]
#[case::width_overflow_with_white_space(
    "<fileinfo><streamdetails><video><width> 99999999999 </width></video></streamdetails></fileinfo>",
    Ok(())
)]
fn an_empty_or_overflowing_typed_value_fails_the_fetch(
    #[case] element: &str,
    #[case] season: FetchResult,
) {
    let failed = Err(FetchError::InvalidValue);
    let (fetched, _) = fetch_movie_xml(&format!("<movie><title>Before</title>{element}</movie>"));
    assert_eq!(fetched, failed, "movie {element}");
    let (fetched, _) = fetch_episode_xml(&format!(
        "<episodedetails><title>Before</title>{element}</episodedetails>"
    ));
    assert_eq!(fetched, failed, "episode {element}");
    let fetched = fetch_season_xml(&format!("<season><title>Before</title>{element}</season>"));
    assert_eq!(fetched, season, "season {element}");
}

/// `EpisodeNfoParser.Fetch` catches the `XmlException` as well: one block
/// keeps what it read before `<watched>True</watched>`, and a bad block among
/// several merges nothing.
#[test]
fn an_episode_stops_at_an_unconvertible_watched() {
    let (fetched, result) = fetch_episode_xml(
        "<episodedetails><title>Pilot</title><episode>2</episode>\
         <watched>True</watched><plot>After</plot></episodedetails>",
    );
    assert_eq!(fetched, Ok(()));
    assert_eq!(result.item.name.as_deref(), Some("Pilot"));
    assert_eq!(result.item.index_number, Some(2));
    assert_eq!(result.item.overview, None);

    let (fetched, result) = fetch_episode_xml(
        "<episodedetails><title>One</title><episode>1</episode></episodedetails>\
         <episodedetails><title>Two</title><watched>yes</watched><episode>2</episode>\
         </episodedetails>",
    );
    assert_eq!(fetched, Ok(()));
    assert_eq!(result.item.name, None);
    assert_eq!(result.item.index_number, None);
}

/// Several blocks are all read before any is merged, in file order, so the
/// first bad block decides: an `XmlException` merges nothing, an uncaught
/// error fails the fetch.
#[rstest::rstest]
#[case::uncaught_second("<watched>true</watched>", "<watched/>", Err(FetchError::InvalidValue))]
#[case::xml_before_uncaught("<watched>yes</watched>", "<watched/>", Ok(()))]
#[case::uncaught_before_xml("<watched/>", "<watched>yes</watched>", Err(FetchError::InvalidValue))]
fn a_multi_episode_nfo_stops_at_its_first_bad_block(
    #[case] first: &str,
    #[case] second: &str,
    #[case] expected: FetchResult,
) {
    let (fetched, result) = fetch_episode_xml(&format!(
        "<episodedetails><title>One</title><episode>1</episode>{first}</episodedetails>\
         <episodedetails><title>Two</title><episode>2</episode>{second}</episodedetails>"
    ));
    assert_eq!(fetched, expected);
    assert_eq!(result.item.name, None);
}

/// The series parser takes the movie's path, and `album.nfo`/`artist.nfo`
/// the plain one.
#[test]
fn series_and_music_nfos_stop_at_a_bad_watched() {
    let mut result = new_result(NfoItemKind::Series);
    let fetched = fetch_series(
        &mut result,
        "tvshow.nfo",
        "<tvshow><title>Show</title><watched>yes</watched><plot>After</plot></tvshow>",
        &NfoConfiguration::default(),
        &StaticExternalIds::new(["Tmdb"]),
        &NoDirectoryService,
    );
    assert_eq!(fetched, Ok(()));
    assert_eq!(result.item.name.as_deref(), Some("Show"));
    assert_eq!(result.item.overview, None);

    for (watched, expected) in [
        ("<watched>yes</watched>", FetchError::MalformedXml),
        ("<watched/>", FetchError::InvalidValue),
    ] {
        let fetched = fetch_music(
            &mut new_result(NfoItemKind::MusicAlbum),
            "album.nfo",
            &format!("<album><title>Kind of Blue</title>{watched}</album>"),
            &NfoConfiguration::default(),
            &StaticExternalIds::new(["MusicBrainzAlbum"]),
            &NoDirectoryService,
        );
        assert_eq!(fetched, Err(expected), "{watched}");
    }
}

/// `TryReadInt` is `int.TryParse(s, InvariantCulture)`: the same grammar as
/// `XmlConvert.ToInt32`, with no value where that throws, so the read goes on.
#[rstest::rstest]
#[case::digits("2", Some(2))]
#[case::plus_sign("+2", Some(2))]
#[case::white_space_around("\t2\r\n", Some(2))]
#[case::no_break_space_around("\u{a0}2", None)]
#[case::overflow("2147483648", None)]
fn episode_numbers_parse_like_int_try_parse(#[case] value: &str, #[case] episode: Option<i32>) {
    let (fetched, result) = fetch_episode_xml(&format!(
        "<episodedetails><episode>{value}</episode><title>After</title></episodedetails>"
    ));
    assert_eq!(fetched, Ok(()));
    assert_eq!(result.item.index_number, episode, "{value:?}");
    assert_eq!(result.item.name.as_deref(), Some("After"), "{value:?}");
}

/// The plain `Fetch` (season, album, artist) catches nothing: an
/// unconvertible `<watched>` fails it like a syntax error does.
#[test]
fn a_season_with_an_unconvertible_watched_is_a_failed_fetch() {
    let fetched = fetch_season_xml("<season><title>S1</title><watched>True</watched></season>");
    assert_eq!(fetched, Err(FetchError::MalformedXml));
    let fetched = fetch_season_xml("<season><title>S1</title><watched>true</watched></season>");
    assert_eq!(fetched, Ok(()));
}
