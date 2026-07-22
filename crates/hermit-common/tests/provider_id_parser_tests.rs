//! Transliteration of `Jellyfin.Common.Tests/Providers/ProviderIdParserTests.cs`.

use hermit_common::providers;
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
