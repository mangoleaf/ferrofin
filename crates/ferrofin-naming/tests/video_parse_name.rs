//! `LibraryManager.ParseName`: the title and year a metadata provider
//! searches by, from an item's raw name. A movie in its own folder is named
//! by that folder, year included (`MovieResolver`), so these are the names a
//! name-only library hands the providers.

use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::video::video_resolver;
use rstest::rstest;

#[rstest]
// Own-folder movie names, as the resolver keeps them.
#[case("Groundhog Day (1993)", "Groundhog Day", Some(1993))]
#[case("Fargo (1996)", "Fargo", Some(1996))]
#[case(
    "The Hunger Games - Mockingjay - Part 1 (2014)",
    "The Hunger Games - Mockingjay - Part 1",
    Some(2014)
)]
#[case(
    "Dave Chappelle - Killin' Them Softly (2000)",
    "Dave Chappelle - Killin' Them Softly",
    Some(2000)
)]
#[case("Monsters, Inc. (2001)", "Monsters, Inc.", Some(2001))]
// File stems with a trailing quality token.
#[case("Groundhog Day (1993) Unknown", "Groundhog Day", Some(1993))]
#[case("Fargo (1996) WEBDL-1080p", "Fargo", Some(1996))]
#[case("Interstellar (2014) Remux-2160p Proper", "Interstellar", Some(2014))]
// Release clutter without a year is still dropped.
#[case("Heat 1080p BluRay", "Heat", None)]
// A name with nothing to lift is left alone.
#[case("Groundhog Day", "Groundhog Day", None)]
#[case("", "", None)]
fn parse_name_test(
    #[case] input: &str,
    #[case] expected_name: &str,
    #[case] expected_year: Option<i32>,
) {
    let result = video_resolver::parse_name(input, &NamingOptions::new());

    assert_eq!(result.name, expected_name, "name of {input:?}");
    assert_eq!(result.year, expected_year, "year of {input:?}");
}
