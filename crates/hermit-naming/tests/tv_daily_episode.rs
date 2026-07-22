//! Ported from `TV/DailyEpisodeTests.cs`.

use hermit_naming::common::NamingOptions;
use hermit_naming::tv::EpisodeResolver;
use rstest::rstest;

#[rstest]
#[case(
    "/server/anything_1996.11.14.mp4",
    "anything",
    Some(1996),
    Some(11),
    Some(14)
)]
#[case(
    "/server/anything_1996-11-14.mp4",
    "anything",
    Some(1996),
    Some(11),
    Some(14)
)]
#[case(
    "/server/james.corden.2017.04.20.anne.hathaway.720p.hdtv.x264-crooks.mkv",
    "james.corden",
    Some(2017),
    Some(4),
    Some(20)
)]
#[case(
    "/server/ABC News 2018_03_24_19_00_00.mkv",
    "ABC News",
    Some(2018),
    Some(3),
    Some(24)
)]
#[case(
    "/server/Jeopardy 2023 07 14 HDTV x264 AC3.mkv",
    "Jeopardy",
    Some(2023),
    Some(7),
    Some(14)
)]
fn test(
    #[case] path: &str,
    #[case] series_name: &str,
    #[case] year: Option<i32>,
    #[case] month: Option<i32>,
    #[case] day: Option<i32>,
) {
    let options = NamingOptions::new();
    let result = EpisodeResolver::new(&options).resolve_simple(path, false);

    assert_eq!(result.as_ref().and_then(|r| r.season_number), None);
    assert_eq!(result.as_ref().and_then(|r| r.episode_number), None);
    assert_eq!(result.as_ref().and_then(|r| r.year), year);
    assert_eq!(result.as_ref().and_then(|r| r.month), month);
    assert_eq!(result.as_ref().and_then(|r| r.day), day);
    assert!(
        result
            .as_ref()
            .and_then(|r| r.series_name.as_deref())
            .unwrap_or("")
            .eq_ignore_ascii_case(series_name),
        "series name mismatch: got {:?}",
        result.as_ref().and_then(|r| r.series_name.as_deref())
    );
}
