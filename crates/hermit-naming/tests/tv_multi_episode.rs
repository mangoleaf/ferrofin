//! Ported from `TV/MultiEpisodeTests.cs`.

use hermit_naming::common::NamingOptions;
use hermit_naming::tv::EpisodePathParser;
use rstest::rstest;

#[rstest]
#[case("Season 1/4x01 – 20 Hours in America (1).mkv", None)]
#[case("Season 1/01x02 blah.avi", None)]
#[case("Season 1/S01x02 blah.avi", None)]
#[case("Season 1/S01E02 blah.avi", None)]
#[case("Season 1/S01xE02 blah.avi", None)]
#[case("Season 1/seriesname 01x02 blah.avi", None)]
#[case("Season 1/seriesname S01x02 blah.avi", None)]
#[case("Season 1/seriesname S01E02 blah.avi", None)]
#[case("Season 1/seriesname S01xE02 blah.avi", None)]
#[case("Season 2/02x03 - 04 Ep Name.mp4", None)]
#[case("Season 2/My show name 02x03 - 04 Ep Name.mp4", None)]
#[case("Season 2/Elementary - 02x03 - 02x04 - 02x15 - Ep Name.mp4", Some(15))]
#[case("Season 2/02x03 - 02x04 - 02x15 - Ep Name.mp4", Some(15))]
#[case("Season 2/02x03-04-15 - Ep Name.mp4", Some(15))]
#[case("Season 2/Elementary - 02x03-04-15 - Ep Name.mp4", Some(15))]
#[case("Season 02/02x03-E15 - Ep Name.mp4", Some(15))]
#[case("Season 02/Elementary - 02x03-E15 - Ep Name.mp4", Some(15))]
#[case("Season 02/02x03 - x04 - x15 - Ep Name.mp4", Some(15))]
#[case("Season 02/Elementary - 02x03 - x04 - x15 - Ep Name.mp4", Some(15))]
#[case("Season 02/02x03x04x15 - Ep Name.mp4", Some(15))]
#[case("Season 02/Elementary - 02x03x04x15 - Ep Name.mp4", Some(15))]
#[case("Season 1/Elementary - S01E23-E24-E26 - The Woman.mp4", Some(26))]
#[case("Season 1/S01E23-E24-E26 - The Woman.mp4", Some(26))]
#[case("Season 2009/2009x02 blah.avi", None)]
#[case("Season 2009/S2009x02 blah.avi", None)]
#[case("Season 2009/S2009E02 blah.avi", None)]
#[case("Season 2009/S2009xE02 blah.avi", None)]
#[case("Season 2009/seriesname 2009x02 blah.avi", None)]
#[case("Season 2009/seriesname S2009x02 blah.avi", None)]
#[case("Season 2009/seriesname S2009E02 blah.avi", None)]
#[case("Season 2009/seriesname S2009xE02 blah.avi", None)]
#[case(
    "Season 2009/Elementary - 2009x03 - 2009x04 - 2009x15 - Ep Name.mp4",
    Some(15)
)]
#[case("Season 2009/2009x03 - 2009x04 - 2009x15 - Ep Name.mp4", Some(15))]
#[case("Season 2009/2009x03-04-15 - Ep Name.mp4", Some(15))]
#[case("Season 2009/Elementary - 2009x03-04-15 - Ep Name.mp4", Some(15))]
#[case("Season 2009/2009x03-E15 - Ep Name.mp4", Some(15))]
#[case("Season 2009/Elementary - 2009x03-E15 - Ep Name.mp4", Some(15))]
#[case("Season 2009/2009x03 - x04 - x15 - Ep Name.mp4", Some(15))]
#[case("Season 2009/Elementary - 2009x03 - x04 - x15 - Ep Name.mp4", Some(15))]
#[case("Season 2009/2009x03x04x15 - Ep Name.mp4", Some(15))]
#[case("Season 2009/Elementary - 2009x03x04x15 - Ep Name.mp4", Some(15))]
#[case("Season 2009/Elementary - S2009E23-E24-E26 - The Woman.mp4", Some(26))]
#[case("Season 2009/S2009E23-E24-E26 - The Woman.mp4", Some(26))]
#[case("Season 1/02 - blah.avi", None)]
#[case("Season 2/02 - blah 14 blah.avi", None)]
#[case("Season 1/02 - blah-02 a.avi", None)]
#[case("Season 2/02.avi", None)]
#[case("Season 1/02-03 - blah.avi", Some(3))]
#[case("Season 2/02-04 - blah 14 blah.avi", Some(4))]
#[case("Season 1/02-05 - blah-02 a.avi", Some(5))]
#[case("Season 2/02-04.avi", Some(4))]
#[case("Season 2 /[HorribleSubs] Hunter X Hunter - 136[720p].mkv", None)]
#[case("Season 1/series-s09e14-1080p.mkv", None)]
#[case("Season 1/series-s09e14-720p.mkv", None)]
#[case("Season 1/series-s09e14-720i.mkv", None)]
#[case("Season 1/MOONLIGHTING_s01e01-e04.mkv", Some(4))]
#[case("Season 1/MOONLIGHTING_s01e01-e04", Some(4))]
fn test_get_ending_episode_number_from_file(
    #[case] filename: &str,
    #[case] ending_episode_number: Option<i32>,
) {
    let options = NamingOptions::new();
    let result = EpisodePathParser::new(&options).parse_simple(filename, false);
    assert_eq!(result.ending_episode_number, ending_episode_number);
}
