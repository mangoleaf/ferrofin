//! Ported from `TV/SeasonNumberTests.cs`.

use hermit_naming::common::NamingOptions;
use hermit_naming::tv::EpisodeResolver;
use rstest::rstest;

#[rstest]
#[case(
    "The Daily Show/The Daily Show 25x22 - [WEBDL-720p][AAC 2.0][x264] Noah Baumbach-TBS.mkv",
    Some(25)
)]
#[case("/Show/Season 02/S02E03 blah.avi", Some(2))]
#[case("Season 1/seriesname S01x02 blah.avi", Some(1))]
#[case("Season 1/S01x02 blah.avi", Some(1))]
#[case("Season 1/seriesname S01xE02 blah.avi", Some(1))]
#[case("Season 1/01x02 blah.avi", Some(1))]
#[case("Season 1/S01E02 blah.avi", Some(1))]
#[case("Season 1/S01xE02 blah.avi", Some(1))]
#[case("Season 1/seriesname 01x02 blah.avi", Some(1))]
#[case("Season 1/seriesname S01E02 blah.avi", Some(1))]
#[case("Season 2/Elementary - 02x03 - 02x04 - 02x15 - Ep Name.mp4", Some(2))]
#[case("Season 2/02x03 - 02x04 - 02x15 - Ep Name.mp4", Some(2))]
#[case("Season 2/02x03-04-15 - Ep Name.mp4", Some(2))]
#[case("Season 2/Elementary - 02x03-04-15 - Ep Name.mp4", Some(2))]
#[case("Season 02/02x03-E15 - Ep Name.mp4", Some(2))]
#[case("Season 02/Elementary - 02x03-E15 - Ep Name.mp4", Some(2))]
#[case("Season 02/02x03 - x04 - x15 - Ep Name.mp4", Some(2))]
#[case("Season 02/Elementary - 02x03 - x04 - x15 - Ep Name.mp4", Some(2))]
#[case("Season 02/02x03x04x15 - Ep Name.mp4", Some(2))]
#[case("Season 02/Elementary - 02x03x04x15 - Ep Name.mp4", Some(2))]
#[case("Season 1/Elementary - S01E23-E24-E26 - The Woman.mp4", Some(1))]
#[case("Season 1/S01E23-E24-E26 - The Woman.mp4", Some(1))]
#[case("Season 25/The Simpsons.S25E09.Steal this episode.mp4", Some(25))]
#[case("The Simpsons/The Simpsons.S25E09.Steal this episode.mp4", Some(25))]
#[case("2016/Season s2016e1.mp4", Some(2016))]
#[case("2016/Season 2016x1.mp4", Some(2016))]
#[case("Season 2009/2009x02 blah.avi", Some(2009))]
#[case("Season 2009/S2009x02 blah.avi", Some(2009))]
#[case("Season 2009/S2009E02 blah.avi", Some(2009))]
#[case("Season 2009/S2009xE02 blah.avi", Some(2009))]
#[case("Season 2009/seriesname 2009x02 blah.avi", Some(2009))]
#[case("Season 2009/seriesname S2009x02 blah.avi", Some(2009))]
#[case("Season 2009/seriesname S2009E02 blah.avi", Some(2009))]
#[case(
    "Season 2009/Elementary - 2009x03 - 2009x04 - 2009x15 - Ep Name.mp4",
    Some(2009)
)]
#[case("Season 2009/2009x03 - 2009x04 - 2009x15 - Ep Name.mp4", Some(2009))]
#[case("Season 2009/2009x03-04-15 - Ep Name.mp4", Some(2009))]
#[case(
    "Season 2009/Elementary - 2009x03 - x04 - x15 - Ep Name.mp4",
    Some(2009)
)]
#[case("Season 2009/2009x03x04x15 - Ep Name.mp4", Some(2009))]
#[case("Season 2009/Elementary - 2009x03x04x15 - Ep Name.mp4", Some(2009))]
#[case(
    "Season 2009/Elementary - S2009E23-E24-E26 - The Woman.mp4",
    Some(2009)
)]
#[case("Season 2009/S2009E23-E24-E26 - The Woman.mp4", Some(2009))]
#[case("Series/1-12 - The Woman.mp4", Some(1))]
#[case("Running Man/Running Man S2017E368.mkv", Some(2017))]
#[case("Case Closed (1996-2007)/Case Closed - 317.mkv", None)]
fn get_season_number_from_episode_file_test(#[case] path: &str, #[case] expected: Option<i32>) {
    let options = NamingOptions::new();
    let result = EpisodeResolver::new(&options).resolve_simple(path, false);
    assert_eq!(result.and_then(|r| r.season_number), expected);
}
