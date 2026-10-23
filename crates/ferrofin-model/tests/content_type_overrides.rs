//! The folder content-type override list (`ServerConfiguration.ContentTypes`).

use ferrofin_model::content_type_overrides::{
    contains_sub_path, content_type_override, paths_equal, remove_content_type_overrides,
    with_content_type_override,
};
use ferrofin_model::data::CollectionType;
use ferrofin_model::dto::NameValuePair;
use rstest::rstest;

fn pair(name: Option<&str>, value: Option<&str>) -> NameValuePair {
    NameValuePair {
        name: name.map(str::to_owned),
        value: value.map(str::to_owned),
    }
}

fn entries(list: &[(&str, &str)]) -> Vec<NameValuePair> {
    list.iter()
        .map(|(name, value)| pair(Some(name), Some(value)))
        .collect()
}

/// `ManagedFileSystemTests.ContainsSubPath_Unix_ReturnsExpected`.
#[rstest]
#[case("/media", "/media/tv", true)]
#[case("/media", "/media/tv/show/episode.mkv", true)]
#[case("/media/", "/media/tv", true)]
#[case("/", "/media", true)]
#[case("/media", "/media", false)]
#[case("/media", "/media/", true)]
#[case("/media", "/data/media/tv", false)]
#[case("/media", "/mediastuff/tv", false)]
#[case("/data/media", "/data/media/tv", true)]
#[case("/media/tv", "/media", false)]
#[case("/MEDIA", "/media/tv", false)]
fn contains_sub_path_follows_managed_file_system(
    #[case] parent: &str,
    #[case] path: &str,
    #[case] expected: bool,
) {
    assert_eq!(contains_sub_path(parent, path), expected);
}

/// `ManagedFileSystem.AreEqual`: one trailing separator is ignored, case is
/// not, and a blank path equals nothing.
#[rstest]
#[case("/media/tv", "/media/tv", true)]
#[case("/media/tv/", "/media/tv", true)]
#[case("/media/tv//", "/media/tv", false)]
#[case("/", "/", true)]
#[case("/Media/tv", "/media/tv", false)]
#[case("", "", false)]
#[case(" ", " ", false)]
fn paths_equal_follows_managed_file_system(
    #[case] left: &str,
    #[case] right: &str,
    #[case] expected: bool,
) {
    assert_eq!(paths_equal(left, right), expected);
}

#[test]
fn an_override_matches_its_own_path_and_with_inherit_the_paths_below_it() {
    let list = entries(&[("/media/mixed/Blade (1998)", "movies")]);
    let blade = "/media/mixed/Blade (1998)";
    let disc = "/media/mixed/Blade (1998)/extras";
    assert_eq!(
        content_type_override(&list, blade, false),
        Some(CollectionType::movies)
    );
    assert_eq!(
        content_type_override(&list, &format!("{blade}/"), false),
        Some(CollectionType::movies)
    );
    assert_eq!(content_type_override(&list, disc, false), None);
    assert_eq!(
        content_type_override(&list, disc, true),
        Some(CollectionType::movies)
    );
    assert_eq!(content_type_override(&list, "/media/mixed", true), None);
    assert_eq!(
        content_type_override(&list, "/media/mixed/Blade (1998) 2", true),
        None
    );
}

/// `FirstOrDefault` takes the first entry in the list that matches, not the
/// nearest folder, and a value that does not parse ends the lookup.
#[test]
fn the_first_matching_entry_decides() {
    let list = entries(&[
        ("/media/mixed", "tvshows"),
        ("/media/mixed/Blade (1998)", "movies"),
    ]);
    assert_eq!(
        content_type_override(&list, "/media/mixed/Blade (1998)", true),
        Some(CollectionType::tvshows)
    );
    assert_eq!(
        content_type_override(&list, "/media/mixed/Blade (1998)", false),
        Some(CollectionType::movies)
    );
    let list = entries(&[
        ("/media/mixed", "Movies"),
        ("/media/mixed/Blade (1998)", "movies"),
    ]);
    assert_eq!(
        content_type_override(&list, "/media/mixed/Blade (1998)", true),
        None
    );
}

/// `Enum.TryParse<CollectionType>(value)`: case-sensitive names, integers too.
#[rstest]
#[case(Some("movies"), Some(CollectionType::movies))]
#[case(Some("tvshows"), Some(CollectionType::tvshows))]
#[case(Some("TvShows"), None)]
#[case(Some("2"), Some(CollectionType::tvshows))]
#[case(Some(" movies "), Some(CollectionType::movies))]
#[case(Some("mixed"), None)]
#[case(Some(""), None)]
#[case(None, None)]
fn an_override_value_parses_as_enum_try_parse(
    #[case] value: Option<&str>,
    #[case] expected: Option<CollectionType>,
) {
    let list = vec![pair(Some("/media/x"), value)];
    assert_eq!(content_type_override(&list, "/media/x", false), expected);
}

#[test]
fn an_entry_without_a_name_matches_nothing() {
    let list = vec![pair(None, Some("movies")), pair(Some(""), Some("movies"))];
    assert_eq!(content_type_override(&list, "/media/x", true), None);
    assert_eq!(content_type_override(&list, "", true), None);
}

/// `UpdateItemContentType`: the folder's old entry goes (ignoring case), as
/// do entries without a name; a blank type only clears.
#[test]
fn setting_an_override_replaces_the_folders_entry() {
    let list = vec![
        pair(Some("/media/mixed/BLADE (1998)"), Some("tvshows")),
        pair(Some(" "), Some("movies")),
        pair(None, Some("movies")),
        pair(Some("/media/other"), Some("movies")),
    ];
    let saved =
        with_content_type_override(&list, Some("/media/mixed/Blade (1998)"), Some("movies"));
    assert_eq!(
        saved,
        vec![
            pair(Some("/media/other"), Some("movies")),
            pair(Some("/media/mixed/Blade (1998)"), Some("movies")),
        ]
    );
    let cleared = with_content_type_override(&saved, Some("/media/mixed/Blade (1998)"), Some("  "));
    assert_eq!(cleared, vec![pair(Some("/media/other"), Some("movies"))]);
    assert_eq!(
        with_content_type_override(&saved, Some("/media/mixed/Blade (1998)"), None),
        cleared
    );
}

/// The value is stored as sent: upstream does not trim it.
#[test]
fn an_override_is_stored_as_sent() {
    let saved = with_content_type_override(&[], Some("/media/x"), Some(" movies"));
    assert_eq!(saved, vec![pair(Some("/media/x"), Some(" movies"))]);
}

/// An item without a path has no containing folder: nothing is dropped as
/// its entry, and the new entry has no name.
#[test]
fn an_item_without_a_folder_keeps_every_named_entry() {
    let list = entries(&[("/media/x", "movies")]);
    let saved = with_content_type_override(&list, None, Some("tvshows"));
    assert_eq!(
        saved,
        vec![
            pair(Some("/media/x"), Some("movies")),
            pair(None, Some("tvshows")),
        ]
    );
}

/// `RemoveContentTypeOverrides`: a new library location drops the entries at
/// or below it and those without a name, and nothing else.
#[test]
fn adding_a_location_drops_the_overrides_at_and_below_it() {
    let mut list = vec![
        pair(Some("/media/mixed"), Some("movies")),
        pair(Some("/media/mixed/Show"), Some("tvshows")),
        pair(Some("/media/mixed2"), Some("movies")),
        pair(Some("/media"), Some("movies")),
        pair(None, Some("movies")),
    ];
    assert!(remove_content_type_overrides(&mut list, "/media/mixed/"));
    assert_eq!(
        list,
        vec![
            pair(Some("/media/mixed2"), Some("movies")),
            pair(Some("/media"), Some("movies")),
        ]
    );
    assert!(!remove_content_type_overrides(&mut list, "/srv/tv"));
}
