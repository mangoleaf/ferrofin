//! Transliteration of `Jellyfin.Common.Tests/Providers/ProviderIdParserTests.cs`.

use ferrofin_common::providers;
use rstest::rstest;

#[rstest]
#[case("tt1234567", "tt1234567")]
#[case("tt12345678", "tt12345678")]
#[case("https://www.imdb.com/title/tt1234567", "tt1234567")]
#[case("https://www.imdb.com/title/tt12345678", "tt12345678")]
// C# verbatim strings: the `\n` is a literal backslash-n, not a newline.
#[case(r"multiline\nhttps://www.imdb.com/title/tt1234567", "tt1234567")]
#[case(r"multiline\nhttps://www.imdb.com/title/tt12345678", "tt12345678")]
#[case("tt1234567tt7654321", "tt1234567")]
#[case("tt12345678tt7654321", "tt12345678")]
#[case("tt123456789", "tt12345678")]
fn find_imdb_id_valid_success(#[case] text: &str, #[case] expected: &str) {
    let parsed = providers::try_find_imdb_id(text);
    assert_eq!(Some(expected), parsed);
}

#[rstest]
#[case("tt123456")]
#[case("https://www.imdb.com/title/tt123456")]
#[case("Jellyfin")]
fn find_imdb_id_invalid_success(#[case] text: &str) {
    assert_eq!(None, providers::try_find_imdb_id(text));
}

#[rstest]
#[case("https://www.themoviedb.org/movie/30287-fallo", "30287")]
#[case("themoviedb.org/movie/30287", "30287")]
fn find_tmdb_movie_id_valid_success(#[case] text: &str, #[case] expected: &str) {
    assert_eq!(Some(expected), providers::try_find_tmdb_movie_id(text));
}

#[rstest]
#[case("https://www.themoviedb.org/movie/fallo-30287")]
#[case("https://www.themoviedb.org/tv/1668-friends")]
fn find_tmdb_movie_id_invalid_success(#[case] text: &str) {
    assert_eq!(None, providers::try_find_tmdb_movie_id(text));
}

#[rstest]
#[case("https://www.themoviedb.org/tv/1668-friends", "1668")]
#[case("themoviedb.org/tv/1668", "1668")]
fn find_tmdb_series_id_valid_success(#[case] text: &str, #[case] expected: &str) {
    assert_eq!(Some(expected), providers::try_find_tmdb_series_id(text));
}

#[rstest]
#[case("https://www.themoviedb.org/tv/friends-1668")]
#[case("https://www.themoviedb.org/movie/30287-fallo")]
fn find_tmdb_series_id_invalid_success(#[case] text: &str) {
    assert_eq!(None, providers::try_find_tmdb_series_id(text));
}

#[rstest]
#[case("https://www.thetvdb.com/?tab=series&id=121361", "121361")]
#[case("thetvdb.com/?tab=series&id=121361", "121361")]
fn find_tvdb_id_valid_success(#[case] text: &str, #[case] expected: &str) {
    assert_eq!(Some(expected), providers::try_find_tvdb_id(text));
}

#[rstest]
#[case("thetvdb.com/?tab=series&id=Jellyfin121361")]
#[case("https://www.themoviedb.org/tv/1668-friends")]
fn find_tvdb_id_invalid_success(#[case] text: &str) {
    assert_eq!(None, providers::try_find_tvdb_id(text));
}

/// Not upstream's: the NFO parser hands these scanners the text after an NFO's
/// last closing tag, or the whole file when it has none, so text around an id
/// may be any UTF-8. The scan is bytewise; every key it matches is
/// ASCII, so it finds what upstream's UTF-16 scan finds and never slices inside
/// a character.
#[rstest]
#[case::imdb_after_accents(providers::try_find_imdb_id, "Amélie — tt0211915", Some("tt0211915"))]
#[case::imdb_before_cjk(providers::try_find_imdb_id, "tt1234567日本", Some("tt1234567"))]
#[case::imdb_too_short_before_accent(providers::try_find_imdb_id, "tt123456é", None)]
#[case::imdb_more_bytes_than_chars(providers::try_find_imdb_id, "日本語tt", None)]
#[case::imdb_prefix_then_multibyte(
    providers::try_find_imdb_id,
    "ttéééé tt7654321",
    Some("tt7654321")
)]
#[case::tmdb_movie_after_cjk(
    providers::try_find_tmdb_movie_id,
    "日本themoviedb.org/movie/30287-é",
    Some("30287")
)]
#[case::tmdb_series_before_accent(
    providers::try_find_tmdb_series_id,
    "themoviedb.org/tv/1668é",
    Some("1668")
)]
#[case::tvdb_multibyte_after_key(
    providers::try_find_tvdb_id,
    "thetvdb.com/?tab=series&id=ü121361",
    None
)]
fn non_ascii_text_around_an_id(
    #[case] find: fn(&str) -> Option<&str>,
    #[case] text: &str,
    #[case] expected: Option<&str>,
) {
    assert_eq!(expected, find(text));
}
