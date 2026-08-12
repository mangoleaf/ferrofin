//! Ported from `TV/SeasonPathParserTests.cs`.

use ferrofin_naming::tv::season_path_parser;
use rstest::rstest;

#[rstest]
#[case("/Drive/Season 1", "/Drive", Some(1), true)]
#[case("/Drive/SEASON 1", "/Drive", Some(1), true)]
#[case("/Drive/Staffel 1", "/Drive", Some(1), true)]
#[case("/Drive/STAFFEL 1", "/Drive", Some(1), true)]
#[case("/Drive/Stagione 1", "/Drive", Some(1), true)]
#[case("/Drive/STAGIONE 1", "/Drive", Some(1), true)]
#[case("/Drive/sæson 1", "/Drive", Some(1), true)]
#[case("/Drive/SÆSON 1", "/Drive", Some(1), true)]
#[case("/Drive/Temporada 1", "/Drive", Some(1), true)]
#[case("/Drive/TEMPORADA 1", "/Drive", Some(1), true)]
#[case("/Drive/series 1", "/Drive", Some(1), true)]
#[case("/Drive/SERIES 1", "/Drive", Some(1), true)]
#[case("/Drive/Kausi 1", "/Drive", Some(1), true)]
#[case("/Drive/KAUSI 1", "/Drive", Some(1), true)]
#[case("/Drive/Säsong 1", "/Drive", Some(1), true)]
#[case("/Drive/SÄSONG 1", "/Drive", Some(1), true)]
#[case("/Drive/Seizoen 1", "/Drive", Some(1), true)]
#[case("/Drive/SEIZOEN 1", "/Drive", Some(1), true)]
#[case("/Drive/Seasong 1", "/Drive", Some(1), true)]
#[case("/Drive/SEASONG 1", "/Drive", Some(1), true)]
#[case("/Drive/Sezon 1", "/Drive", Some(1), true)]
#[case("/Drive/SEZON 1", "/Drive", Some(1), true)]
#[case("/Drive/sezona 1", "/Drive", Some(1), true)]
#[case("/Drive/SEZONA 1", "/Drive", Some(1), true)]
#[case("/Drive/sezóna 1", "/Drive", Some(1), true)]
#[case("/Drive/SEZÓNA 1", "/Drive", Some(1), true)]
#[case("/Drive/Sezonul 1", "/Drive", Some(1), true)]
#[case("/Drive/SEZONUL 1", "/Drive", Some(1), true)]
#[case("/Drive/시즌 1", "/Drive", Some(1), true)]
#[case("/Drive/シーズン 1", "/Drive", Some(1), true)]
#[case("/Drive/сезон 1", "/Drive", Some(1), true)]
#[case("/Drive/Сезон 1", "/Drive", Some(1), true)]
#[case("/Drive/СЕЗОН 1", "/Drive", Some(1), true)]
#[case("/Drive/Season 10", "/Drive", Some(10), true)]
#[case("/Drive/Season 100", "/Drive", Some(100), true)]
#[case("/Drive/s1", "/Drive", Some(1), true)]
#[case("/Drive/S1", "/Drive", Some(1), true)]
#[case("/Drive/Season 2", "/Drive", Some(2), true)]
#[case("/Drive/Season 02", "/Drive", Some(2), true)]
#[case("/Drive/Seinfeld/S02", "/Seinfeld", Some(2), true)]
#[case("/Drive/Seinfeld/2", "/Seinfeld", Some(2), true)]
#[case("/Drive/Seinfeld Season 2", "/Drive", None, false)]
#[case("/Drive/Season 2009", "/Drive", Some(2009), true)]
#[case("/Drive/Season1", "/Drive", Some(1), true)]
#[case(
    "The Wonder Years/The.Wonder.Years.S04.PDTV.x264-JCH",
    "/The Wonder Years",
    Some(4),
    true
)]
#[case("/Drive/Season 7 (2016)", "/Drive", Some(7), true)]
#[case("/Drive/Staffel 7 (2016)", "/Drive", Some(7), true)]
#[case("/Drive/Stagione 7 (2016)", "/Drive", Some(7), true)]
#[case("/Drive/Stargate SG-1/Season 1", "/Drive/Stargate SG-1", Some(1), true)]
#[case(
    "/Drive/Stargate SG-1/Stargate SG-1 Season 1",
    "/Drive/Stargate SG-1",
    Some(1),
    true
)]
#[case("/Drive/Season (8)", "/Drive", None, false)]
#[case("/Drive/3.Staffel", "/Drive", Some(3), true)]
#[case("/Drive/s06e05", "/Drive", None, false)]
#[case(
    "/Drive/The.Legend.of.Condor.Heroes.2017.V2.web-dl.1080p.h264.aac-hdctv",
    "/Drive",
    None,
    false
)]
#[case("/Drive/extras", "/Drive", Some(0), true)]
#[case("/Drive/EXTRAS", "/Drive", Some(0), true)]
#[case("/Drive/specials", "/Drive", Some(0), true)]
#[case("/Drive/SPECIALS", "/Drive", Some(0), true)]
#[case("/Drive/Episode 1 Season 2", "/Drive", None, false)]
#[case("/Drive/Episode 1 SEASON 2", "/Drive", None, false)]
#[case(
    "/media/YouTube/Devyn Johnston/2024-01-24 4070 Ti SUPER in under 7 minutes",
    "/media/YouTube/Devyn Johnston",
    None,
    false
)]
#[case(
    "/media/YouTube/Devyn Johnston/2025-01-28 5090 vs 2 SFF Cases",
    "/media/YouTube/Devyn Johnston",
    None,
    false
)]
#[case("/Drive/202401244070", "/Drive", None, false)]
#[case(
    "/Drive/Drive.S01.2160p.WEB-DL.DDP5.1.H.265-XXXX",
    "/Drive",
    Some(1),
    true
)]
#[case(
    "The Wonder Years/The.Wonder.Years.S04.1080p.PDTV.x264-JCH",
    "/The Wonder Years",
    Some(4),
    true
)]
#[case(
    "The Wonder Years/[The.Wonder.Years.S04.1080p.PDTV.x264-JCH]",
    "/The Wonder Years",
    Some(4),
    true
)]
#[case(
    "The Wonder Years/The.Wonder.Years [S04][1080p.PDTV.x264-JCH]",
    "/The Wonder Years",
    Some(4),
    true
)]
#[case(
    "The Wonder Years/The Wonder Years Season 01 1080p",
    "/The Wonder Years",
    Some(1),
    true
)]
fn get_season_number_from_path_test(
    #[case] path: &str,
    #[case] parent_path: &str,
    #[case] season_number: Option<i32>,
    #[case] is_season_directory: bool,
) {
    let result = season_path_parser::parse(path, Some(parent_path), true, true);

    assert_eq!(result.season_number.is_some(), result.success);
    assert_eq!(result.season_number, season_number);
    assert_eq!(result.is_season_folder, is_season_directory);
}

#[rstest]
#[case(
    "/Drive/300 Collection/300 (2006)",
    "/Drive/300 Collection",
    None,
    false
)]
#[case(
    "/Drive/300 Collection/300 Rise of an Empire",
    "/Drive/300 Collection",
    None,
    false
)]
#[case("/Drive/300 Collection/1", "/Drive/300 Collection", None, false)]
#[case(
    "/Drive/300 Collection/300 Disc 1",
    "/Drive/300 Collection",
    None,
    false
)]
#[case(
    "/Drive/28 Years Later Collection/28 Days Later",
    "/Drive/28 Years Later Collection",
    None,
    false
)]
#[case(
    "/Drive/28 Years Later Collection/28 Weeks Later (2007)",
    "/Drive/28 Years Later Collection",
    None,
    false
)]
#[case(
    "/Drive/28 Years Later Collection/28 Years Later 2025",
    "/Drive/28 Years Later Collection",
    None,
    false
)]
#[case(
    "/Drive/300 Collection/Season 1",
    "/Drive/300 Collection",
    Some(1),
    true
)]
#[case(
    "/Drive/28 Years Later Collection/Season 01",
    "/Drive/28 Years Later Collection",
    Some(1),
    true
)]
#[case("/Drive/300 Collection/S01", "/Drive/300 Collection", Some(1), true)]
#[case("/Drive/300 Collection/S1", "/Drive/300 Collection", Some(1), true)]
fn get_season_number_from_path_mixed_library_test(
    #[case] path: &str,
    #[case] parent_path: &str,
    #[case] season_number: Option<i32>,
    #[case] is_season_directory: bool,
) {
    let result = season_path_parser::parse(path, Some(parent_path), false, false);

    assert_eq!(result.season_number.is_some(), result.success);
    assert_eq!(result.season_number, season_number);
    assert_eq!(result.is_season_folder, is_season_directory);
}
