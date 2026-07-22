//! Ported from `Book/BookResolverTests.cs`.

use hermit_naming::book::book_file_name_parser;
use rstest::rstest;

#[rstest]
#[case(
    "Sherlock Holmes (1887) #1 (of 4) (1887)",
    None,
    Some("Sherlock Holmes"),
    Some(1),
    Some(1887)
)]
#[case("Sherlock Holmes #2", None, Some("Sherlock Holmes"), Some(2), None)]
#[case(
    "Sherlock Holmes (1887) #1",
    None,
    Some("Sherlock Holmes"),
    Some(1),
    None
)]
#[case(
    "Sherlock Holmes #2 (1890)",
    None,
    Some("Sherlock Holmes"),
    Some(2),
    Some(1890)
)]
#[case(
    "A Study in Scarlet (Sherlock Holmes, #1) (1887)",
    Some("A Study in Scarlet"),
    Some("Sherlock Holmes"),
    Some(1),
    Some(1887)
)]
#[case(
    "The Adventures of Sherlock Holmes (Sherlock Holmes, #5)",
    Some("The Adventures of Sherlock Holmes"),
    Some("Sherlock Holmes"),
    Some(5),
    None
)]
#[case(
    "The Sign of the Four (1890)",
    Some("The Sign of the Four"),
    None,
    None,
    Some(1890)
)]
#[case(
    "The Valley of Fear (1915)",
    Some("The Valley of Fear"),
    None,
    None,
    Some(1915)
)]
#[case(
    "2 - The Sign of the Four (1890)",
    Some("The Sign of the Four"),
    None,
    Some(2),
    Some(1890)
)]
#[case(
    "4 - The Valley of Fear",
    Some("The Valley of Fear"),
    None,
    Some(4),
    None
)]
#[case("A Study in Scarlet", Some("A Study in Scarlet"), None, None, None)]
#[case(
    "The Adventures of Sherlock Holmes",
    Some("The Adventures of Sherlock Holmes"),
    None,
    None,
    None
)]
#[case(
    "00 - Dracula's Guest (1914)",
    Some("Dracula's Guest"),
    None,
    Some(0),
    Some(1914)
)]
#[case("01 - Dracula (1897)", Some("Dracula"), None, Some(1), Some(1897))]
#[case(
    "2.0 - Twenty Thousand Leagues Under the Sea",
    Some("Twenty Thousand Leagues Under the Sea"),
    None,
    Some(2),
    None
)]
#[case(
    "2.1 - The Blockade Runners",
    Some("2.1 - The Blockade Runners"),
    None,
    None,
    None
)]
fn resolve_books(
    #[case] input: &str,
    #[case] name: Option<&str>,
    #[case] series: Option<&str>,
    #[case] index: Option<i32>,
    #[case] year: Option<i32>,
) {
    let result = book_file_name_parser::parse(Some(input));
    assert_eq!(result.name.as_deref(), name);
    assert_eq!(result.series_name.as_deref(), series);
    assert_eq!(result.index, index);
    assert_eq!(result.year, year);
}

#[rstest]
#[case(
    "Captain Marvel Adventures v01 (1941)",
    Some("Captain Marvel Adventures v01"),
    None,
    None,
    Some(1),
    Some(1941)
)]
#[case(
    "Captain Marvel Adventures c120",
    Some("Captain Marvel Adventures c120"),
    None,
    Some(120),
    None,
    None
)]
#[case(
    "Captain Marvel Adventures v01 c120",
    Some("Captain Marvel Adventures v01 c120"),
    None,
    Some(120),
    Some(1),
    None
)]
fn resolve_comics(
    #[case] input: &str,
    #[case] name: Option<&str>,
    #[case] series: Option<&str>,
    #[case] chapter: Option<i32>,
    #[case] volume: Option<i32>,
    #[case] year: Option<i32>,
) {
    let result = book_file_name_parser::parse(Some(input));
    assert_eq!(result.name.as_deref(), name);
    assert_eq!(result.series_name.as_deref(), series);
    assert_eq!(result.index, chapter);
    assert_eq!(result.parent_index, volume);
    assert_eq!(result.year, year);
}
