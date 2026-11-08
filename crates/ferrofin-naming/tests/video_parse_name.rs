//! `LibraryManager.ParseName`: the name the metadata providers search by.
//! Upstream has no xUnit test of it. The cases are names a library holds;
//! those marked `CleanDateTimeTests.cs` are that file's rows, whose expected
//! values `TryCleanString` leaves as they are.

use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::video::video_resolver;
use rstest::rstest;

#[rstest]
// An own-folder movie's name is its folder's, year and all.
#[case("Split (2016)", "Split", Some(2016))]
// `CleanDateTimeTests.cs`.
#[case(
    "The Wolf of Wall Street (2013)",
    "The Wolf of Wall Street",
    Some(2013)
)]
#[case("300 (2006)", "300", Some(2006))]
#[case("St. Vincent (2014)", "St. Vincent", Some(2014))]
#[case("", "", None)]
// A title that is a year keeps it; only the trailing year comes out.
#[case("1917 (2019)", "1917", Some(2019))]
#[case("2012 (2009)", "2012", Some(2009))]
#[case("2001", "2001", None)]
// A bare year and dotted release names (`Show 2026`, `Show.2026`).
#[case("Show 2026", "Show", Some(2026))]
#[case("Split.2016.1080p.BluRay", "Split", Some(2016))]
// No year: only the release clutter goes.
#[case("Super movie 480p", "Super movie", None)]
#[case(
    "Crouching.Tiger.Hidden.Dragon.4k",
    "Crouching.Tiger.Hidden.Dragon",
    None
)]
// Nothing to strip: the name as given.
#[case("Heat", "Heat", None)]
fn parse_name(#[case] input: &str, #[case] name: &str, #[case] year: Option<i32>) {
    let parsed = video_resolver::parse_name(input, &NamingOptions::new());
    assert_eq!(parsed.name, name, "{input}");
    assert_eq!(parsed.year, year, "{input}");
}
