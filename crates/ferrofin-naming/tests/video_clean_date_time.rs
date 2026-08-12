//! Ported from `Video/CleanDateTimeTests.cs`.

use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::video::video_resolver;
use rstest::rstest;

fn file_name(path: &str) -> &str {
    match path.rfind(['/', '\\']) {
        Some(idx) => &path[idx + 1..],
        None => path,
    }
}

#[rstest]
#[case(
    "The Wolf of Wall Street (2013).mkv",
    "The Wolf of Wall Street",
    Some(2013)
)]
#[case(
    "The Wolf of Wall Street 2 (2013).mkv",
    "The Wolf of Wall Street 2",
    Some(2013)
)]
#[case(
    "The Wolf of Wall Street - 2 (2013).mkv",
    "The Wolf of Wall Street - 2",
    Some(2013)
)]
#[case(
    "The Wolf of Wall Street 2001 (2013).mkv",
    "The Wolf of Wall Street 2001",
    Some(2013)
)]
#[case("300 (2006).mkv", "300", Some(2006))]
#[case("d:/movies/300 (2006).mkv", "300", Some(2006))]
#[case("300 2 (2006).mkv", "300 2", Some(2006))]
#[case("300 - 2 (2006).mkv", "300 - 2", Some(2006))]
#[case("300 2001 (2006).mkv", "300 2001", Some(2006))]
#[case(
    "curse.of.chucky.2013.stv.unrated.multi.1080p.bluray.x264-rough",
    "curse.of.chucky",
    Some(2013)
)]
#[case(
    "curse.of.chucky.2013.stv.unrated.multi.2160p.bluray.x264-rough",
    "curse.of.chucky",
    Some(2013)
)]
#[case("/server/Movies/300 (2007)/300 (2006).bluray.disc", "300", Some(2006))]
#[case("Arrival.2016.2160p.Blu-Ray.HEVC.mkv", "Arrival", Some(2016))]
#[case(
    "The Wolf of Wall Street (2013)",
    "The Wolf of Wall Street",
    Some(2013)
)]
#[case(
    "The Wolf of Wall Street 2 (2013)",
    "The Wolf of Wall Street 2",
    Some(2013)
)]
#[case(
    "The Wolf of Wall Street - 2 (2013)",
    "The Wolf of Wall Street - 2",
    Some(2013)
)]
#[case(
    "The Wolf of Wall Street 2001 (2013)",
    "The Wolf of Wall Street 2001",
    Some(2013)
)]
#[case("300 (2006)", "300", Some(2006))]
#[case("d:/movies/300 (2006)", "300", Some(2006))]
#[case("300 2 (2006)", "300 2", Some(2006))]
#[case("300 - 2 (2006)", "300 - 2", Some(2006))]
#[case("300 2001 (2006)", "300 2001", Some(2006))]
#[case("/server/Movies/300 (2007)/300 (2006)", "300", Some(2006))]
#[case("/server/Movies/300 (2007)/300 (2006).mkv", "300", Some(2006))]
#[case("American.Psycho.mkv", "American.Psycho.mkv", None)]
#[case("American Psycho.mkv", "American Psycho.mkv", None)]
#[case("[rec].mkv", "[rec].mkv", None)]
#[case("St. Vincent (2014)", "St. Vincent", Some(2014))]
#[case("Super movie(2009).mp4", "Super movie", Some(2009))]
#[case("Drug War 2013.mp4", "Drug War", Some(2013))]
#[case(
    "My Movie (1997) - GreatestReleaseGroup 2019.mp4",
    "My Movie",
    Some(1997)
)]
#[case("First Man 2018 1080p.mkv", "First Man", Some(2018))]
#[case("First Man (2018) 1080p.mkv", "First Man", Some(2018))]
#[case(
    "Maximum Ride - 2016 - WEBDL-1080p - x264 AC3.mkv",
    "Maximum Ride",
    Some(2016)
)]
// In this case, running CleanDateTime first produces no date, so it runs
// CleanString first and then CleanDateTime again.
#[case(
    "3.Days.to.Kill.2014.720p.BluRay.x264.YIFY.mkv",
    "3.Days.to.Kill",
    Some(2014)
)]
#[case("3 days to kill (2005).mkv", "3 days to kill", Some(2005))]
#[case(
    "Rain Man 1988 REMASTERED 1080p BluRay x264 AAC - Ozlem.mp4",
    "Rain Man",
    Some(1988)
)]
#[case("My Movie 2013.12.09", "My Movie 2013.12.09", None)]
#[case("My Movie 2013-12-09", "My Movie 2013-12-09", None)]
#[case("My Movie 20131209", "My Movie 20131209", None)]
#[case("My Movie 2013-12-09 2013", "My Movie 2013-12-09", Some(2013))]
#[case("", "", None)]
fn clean_date_time_test(
    #[case] input: &str,
    #[case] expected_name: &str,
    #[case] expected_year: Option<i32>,
) {
    let input = file_name(input);

    let result = video_resolver::clean_date_time(input, &NamingOptions::new());

    assert!(
        result.name.eq_ignore_ascii_case(expected_name),
        "name mismatch: got {:?}, expected {:?}",
        result.name,
        expected_name
    );
    assert_eq!(result.year, expected_year);
}
