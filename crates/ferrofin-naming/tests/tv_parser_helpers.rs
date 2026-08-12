//! Ported from `TV/TvParserHelpersTest.cs`.

use ferrofin_model::entities::SeriesStatus;
use ferrofin_naming::tv::try_parse_series_status;
use rstest::rstest;

#[rstest]
#[case("Ended", SeriesStatus::Ended)]
#[case("Cancelled", SeriesStatus::Ended)]
#[case("Continuing", SeriesStatus::Continuing)]
#[case("Returning", SeriesStatus::Continuing)]
#[case("Returning Series", SeriesStatus::Continuing)]
#[case("Unreleased", SeriesStatus::Unreleased)]
fn series_status_parser_test_valid(#[case] status_string: &str, #[case] status: SeriesStatus) {
    let parsed = try_parse_series_status(Some(status_string));
    assert_eq!(parsed, Some(status));
}

#[rstest]
#[case("XXX")]
fn series_status_parser_test_invalid(#[case] status_string: &str) {
    let parsed = try_parse_series_status(Some(status_string));
    assert!(parsed.is_none());
}
