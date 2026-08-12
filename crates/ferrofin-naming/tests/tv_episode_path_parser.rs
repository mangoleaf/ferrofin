//! Ported from `TV/EpisodePathParserTest.cs`.

use ferrofin_naming::common::{EpisodeExpression, NamingOptions};
use ferrofin_naming::tv::{EpisodePathParser, EpisodeResolver};
use rstest::rstest;

#[rstest]
#[case("/media/Foo/Foo-S01E01", true, "Foo", 1, 1)]
#[case("/media/Foo - S04E011", true, "Foo", 4, 11)]
#[case("/media/Foo/Foo s01x01", true, "Foo", 1, 1)]
#[case(
    "/media/Foo (2019)/Season 4/Foo (2019).S04E03",
    true,
    "Foo (2019)",
    4,
    3
)]
#[case(r"D:\media\Foo\Foo-S01E01", true, "Foo", 1, 1)]
#[case(r"D:\media\Foo - S04E011", true, "Foo", 4, 11)]
#[case(r"D:\media\Foo\Foo s01x01", true, "Foo", 1, 1)]
#[case(
    r"D:\media\Foo (2019)\Season 4\Foo (2019).S04E03",
    true,
    "Foo (2019)",
    4,
    3
)]
#[case(
    "/Season 2/Elementary - 02x03-04-15 - Ep Name.mp4",
    false,
    "Elementary",
    2,
    3
)]
#[case("/Season 1/seriesname S01E02 blah.avi", false, "seriesname", 1, 2)]
#[case(
    "/Running Man/Running Man S2017E368.mkv",
    false,
    "Running Man",
    2017,
    368
)]
#[case("/Season 1/seriesname 01x02 blah.avi", false, "seriesname", 1, 2)]
#[case(
    "/Season 25/The Simpsons.S25E09.Steal this episode.mp4",
    false,
    "The Simpsons",
    25,
    9
)]
#[case("/Season 1/seriesname S01x02 blah.avi", false, "seriesname", 1, 2)]
#[case(
    "/Season 2/Elementary - 02x03 - 02x04 - 02x15 - Ep Name.mp4",
    false,
    "Elementary",
    2,
    3
)]
#[case("/Season 1/seriesname S01xE02 blah.avi", false, "seriesname", 1, 2)]
#[case(
    "/Season 02/Elementary - 02x03 - x04 - x15 - Ep Name.mp4",
    false,
    "Elementary",
    2,
    3
)]
#[case(
    "/Season 02/Elementary - 02x03x04x15 - Ep Name.mp4",
    false,
    "Elementary",
    2,
    3
)]
#[case(
    "/Season 02/Elementary - 02x03-E15 - Ep Name.mp4",
    false,
    "Elementary",
    2,
    3
)]
#[case(
    "/Season 1/Elementary - S01E23-E24-E26 - The Woman.mp4",
    false,
    "Elementary",
    1,
    23
)]
#[case(
    "/The Wonder Years/The.Wonder.Years.S04.PDTV.x264-JCH/The Wonder Years s04e07 Christmas Party NTSC PDTV.avi",
    false,
    "The Wonder Years",
    4,
    7
)]
#[case(
    "/The.Sopranos/Season 3/The Sopranos Season 3 Episode 09 - The Telltale Moozadell.avi",
    false,
    "The Sopranos",
    3,
    9
)]
fn parse_episodes_correctly(
    #[case] path: &str,
    #[case] is_directory: bool,
    #[case] name: &str,
    #[case] season: i32,
    #[case] episode: i32,
) {
    let options = NamingOptions::new();
    let parser = EpisodePathParser::new(&options);
    let res = parser.parse_simple(path, is_directory);

    assert!(res.success, "expected success for {path}");
    assert_eq!(res.series_name.as_deref(), Some(name));
    assert_eq!(res.season_number, Some(season));
    assert_eq!(res.episode_number, Some(episode));
}

#[rstest]
#[case("/test/01-03.avi", Some(true), Some(true))]
fn episode_path_parser_test_different_expressions_parameters(
    #[case] path: &str,
    #[case] is_named: Option<bool>,
    #[case] is_optimistic: Option<bool>,
) {
    let options = NamingOptions::new();
    let parser = EpisodePathParser::new(&options);
    let res = parser.parse(path, false, is_named, is_optimistic, None, true);
    assert!(res.success);
}

#[test]
fn episode_path_parser_test_false_positive_pixel_rate() {
    let options = NamingOptions::new();
    let parser = EpisodePathParser::new(&options);
    let res = parser.parse_simple("Series Special (1920x1080).mkv", false);
    assert!(!res.success);
}

#[test]
fn episode_resolver_test_wrong_extension() {
    let options = NamingOptions::new();
    let res = EpisodeResolver::new(&options).resolve_simple("test.mp3", false);
    assert!(res.is_none());
}

#[test]
fn episode_resolver_test_wrong_extension_stub() {
    let options = NamingOptions::new();
    let res = EpisodeResolver::new(&options).resolve_simple("dvd.disc", false);
    let res = res.expect("stub should resolve");
    assert!(res.is_stub);
}

#[test]
fn episode_path_parser_test_empty_date_parsers() {
    let mut options = NamingOptions::new();
    options.episode_expressions = vec![EpisodeExpression::new(
        "(([0-9]{4})-([0-9]{2})-([0-9]{2}) [0-9]{2}:[0-9]{2}:[0-9]{2})",
        true,
    )];
    options.compile();

    let parser = EpisodePathParser::new(&options);
    let res = parser.parse_simple("ABC_2019_10_21 11:00:00", false);
    assert!(res.success);
}
