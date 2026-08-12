//! Ported from `TV/EpisodeNumberTests.cs`.

use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::tv::EpisodePathParser;
use rstest::rstest;

#[rstest]
#[case("Season 21/One Piece 1001", Some(1001))]
#[case(
    "Watchmen (2019)/Watchmen 1x03 [WEBDL-720p][EAC3 5.1][h264][-TBS] - She Was Killed by Space Junk.mkv",
    Some(3)
)]
#[case(
    "The Daily Show/The Daily Show 25x22 - [WEBDL-720p][AAC 2.0][x264] Noah Baumbach-TBS.mkv",
    Some(22)
)]
#[case(
    "Castle Rock 2x01 Que el rio siga su curso [WEB-DL HULU 1080p h264 Dual DD5.1 Subs].mkv",
    Some(1)
)]
#[case(
    "After Life 1x06 Episodio 6 [WEB-DL NF 1080p h264 Dual DD 5.1 Sub].mkv",
    Some(6)
)]
#[case("Season 02/S02E03 blah.avi", Some(3))]
#[case("Season 2/02x03 - 02x04 - 02x15 - Ep Name.mp4", Some(3))]
#[case("Season 02/02x03 - x04 - x15 - Ep Name.mp4", Some(3))]
#[case("Season 1/01x02 blah.avi", Some(2))]
#[case("Season 1/S01x02 blah.avi", Some(2))]
#[case("Season 1/S01E02 blah.avi", Some(2))]
#[case("Season 2/Elementary - 02x03-04-15 - Ep Name.mp4", Some(3))]
#[case("Season 1/S01xE02 blah.avi", Some(2))]
#[case("Season 1/seriesname S01E02 blah.avi", Some(2))]
#[case("Season 2/Episode - 16.avi", Some(16))]
#[case("Season 2/Episode 16.avi", Some(16))]
#[case("Season 2/Episode 16 - Some Title.avi", Some(16))]
#[case("Season 2/16 Some Title.avi", Some(16))]
#[case("Season 2/16 - 12 Some Title.avi", Some(16))]
#[case("Season 2/7 - 12 Angry Men.avi", Some(7))]
#[case("Season 1/seriesname 01x02 blah.avi", Some(2))]
#[case("Season 25/The Simpsons.S25E09.Steal this episode.mp4", Some(9))]
#[case("Season 1/seriesname S01x02 blah.avi", Some(2))]
#[case("Season 2/Elementary - 02x03 - 02x04 - 02x15 - Ep Name.mp4", Some(3))]
#[case("Season 1/seriesname S01xE02 blah.avi", Some(2))]
#[case("Season 02/Elementary - 02x03 - x04 - x15 - Ep Name.mp4", Some(3))]
#[case("Season 02/Elementary - 02x03x04x15 - Ep Name.mp4", Some(3))]
#[case("Season 2/02x03-04-15 - Ep Name.mp4", Some(3))]
#[case("Season 02/02x03-E15 - Ep Name.mp4", Some(3))]
#[case("Season 02/Elementary - 02x03-E15 - Ep Name.mp4", Some(3))]
#[case("Season 1/Elementary - S01E23-E24-E26 - The Woman.mp4", Some(23))]
#[case("Season 2009/S2009E23-E24-E26 - The Woman.mp4", Some(23))]
#[case("Season 2009/2009x02 blah.avi", Some(2))]
#[case("Season 2009/S2009x02 blah.avi", Some(2))]
#[case("Season 2009/S2009E02 blah.avi", Some(2))]
#[case("Season 2009/seriesname 2009x02 blah.avi", Some(2))]
#[case("Season 2009/Elementary - 2009x03x04x15 - Ep Name.mp4", Some(3))]
#[case("Season 2009/2009x03x04x15 - Ep Name.mp4", Some(3))]
#[case("Season 2009/Elementary - 2009x03-E15 - Ep Name.mp4", Some(3))]
#[case("Season 2009/S2009xE02 blah.avi", Some(2))]
#[case("Season 2009/Elementary - S2009E23-E24-E26 - The Woman.mp4", Some(23))]
#[case("Season 2009/seriesname S2009xE02 blah.avi", Some(2))]
#[case("Season 2009/2009x03-E15 - Ep Name.mp4", Some(3))]
#[case("Season 2009/seriesname S2009E02 blah.avi", Some(2))]
#[case("Season 2009/2009x03 - 2009x04 - 2009x15 - Ep Name.mp4", Some(3))]
#[case("Season 2009/2009x03 - x04 - x15 - Ep Name.mp4", Some(3))]
#[case("Season 2009/seriesname S2009x02 blah.avi", Some(2))]
#[case(
    "Season 2009/Elementary - 2009x03 - 2009x04 - 2009x15 - Ep Name.mp4",
    Some(3)
)]
#[case("Season 2009/Elementary - 2009x03-04-15 - Ep Name.mp4", Some(3))]
#[case("Season 2009/2009x03-04-15 - Ep Name.mp4", Some(3))]
#[case("Season 2009/Elementary - 2009x03 - x04 - x15 - Ep Name.mp4", Some(3))]
#[case("Season 1/02 - blah-02 a.avi", Some(2))]
#[case("Season 1/02 - blah.avi", Some(2))]
#[case("Season 2/02 - blah 14 blah.avi", Some(2))]
#[case("Season 2/02.avi", Some(2))]
#[case("Season 2/2. Infestation.avi", Some(2))]
#[case(
    "The Wonder Years/The.Wonder.Years.S04.PDTV.x264-JCH/The Wonder Years s04e07 Christmas Party NTSC PDTV.avi",
    Some(7)
)]
#[case("Running Man/Running Man S2017E368.mkv", Some(368))]
#[case("Season 2/[HorribleSubs] Hunter X Hunter - 136 [720p].mkv", Some(136))]
#[case("Log Horizon 2/[HorribleSubs] Log Horizon 2 - 03 [720p].mkv", Some(3))]
#[case("Season 1/seriesname 05.mkv", Some(5))]
#[case("[BBT-RMX] Ranma ½ - 154 [50AC421A].mkv", Some(154))]
#[case("Season 2/Episode 21 - 94 Meetings.mp4", Some(21))]
#[case(
    "/The.Legend.of.Condor.Heroes.2017.V2.web-dl.1080p.h264.aac-hdctv/The.Legend.of.Condor.Heroes.2017.E07.V2.web-dl.1080p.h264.aac-hdctv.mkv",
    Some(7)
)]
#[case("Season 3/The Series Season 3 Episode 9 - The title.avi", Some(9))]
#[case("Season 3/The Series S3 E9 - The title.avi", Some(9))]
#[case("Season 3/S003 E009.avi", Some(9))]
#[case("Season 3/Season 3 Episode 9.avi", Some(9))]
#[case(
    "[VCB-Studio] Re Zero kara Hajimeru Isekai Seikatsu [21][Ma10p_1080p][x265_flac].mkv",
    Some(21)
)]
#[case(
    "[CASO&Sumisora][Oda_Nobuna_no_Yabou][04][BDRIP][1920x1080][x264_AAC][7620E503].mp4",
    Some(4)
)]
#[case("Case Closed (1996-2007)/Case Closed - 317.mkv", Some(317))]
#[case("Season 2/Hunter X Hunter - 101.mkv", Some(101))]
#[case("Season 1/Show Name - 1234 [720p].mkv", Some(1234))]
fn get_episode_number_from_file_test(#[case] path: &str, #[case] expected: Option<i32>) {
    let options = NamingOptions::new();
    let result = EpisodePathParser::new(&options).parse_simple(path, false);
    assert_eq!(result.episode_number, expected);
}
