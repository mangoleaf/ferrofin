//! Ported from `TV/SeriesPathParserTest.cs`.

use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::tv::series_path_parser;
use rstest::rstest;

#[rstest]
#[case("The.Show.S01", "The.Show")]
#[case("/The.Show.S01", "The.Show")]
#[case("/some/place/The.Show.S01", "The.Show")]
#[case("/something/The.Show.S01", "The.Show")]
#[case("The Show Season 10", "The Show")]
#[case("The Show S01E01", "The Show")]
#[case("The Show S01E01 Episode", "The Show")]
#[case("/something/The Show/Season 1", "The Show")]
#[case("/something/The Show/S01", "The Show")]
fn series_path_parser_parse_test(#[case] path: &str, #[case] name: &str) {
    let options = NamingOptions::new();
    let res = series_path_parser::parse(&options, path);

    assert_eq!(res.series_name.as_deref(), Some(name));
    assert!(res.success);
}
