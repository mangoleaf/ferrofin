//! Parsers for provider ids (IMDb / TMDb / TVDb).
//!
//! Faithful port of `MediaBrowser.Common/Providers/ProviderIdParsers.cs`.
//! The C# API scans a `ReadOnlySpan<char>` and yields a sub-span; here each
//! parser returns `Option<&str>` — `Some(id)` on a hit, `None` otherwise.
//!
//! All matching keys (the `tt` prefix, url fragments, digits) are ASCII, so
//! byte offsets and char offsets coincide for every input these parsers accept.

/// Minimum digit count for an IMDb id (matches `ImdbMinNumbers`).
const IMDB_MIN_NUMBERS: usize = 7;
/// Maximum digit count for an IMDb id (matches `ImdbMaxNumbers`).
const IMDB_MAX_NUMBERS: usize = 8;
/// The IMDb id prefix (matches `ImdbPrefix`).
const IMDB_PREFIX: &str = "tt";

/// Returns whether `c` is an ASCII digit (matches the C# `IsDigit` helper).
fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// Parses an IMDb id (`tt` + 7–8 digits) out of `text`.
///
/// Returns the matched id slice, or `None` when no valid id is present.
/// Mirrors `ProviderIdParsers.TryFindImdbId`.
#[must_use]
pub fn try_find_imdb_id(text: &str) -> Option<&str> {
    let mut text = text;
    // IMDb id is at least 9 chars (tt + 7 numbers).
    while text.len() >= 2 + IMDB_MIN_NUMBERS {
        let tt_pos = text.find(IMDB_PREFIX)?;

        text = &text[tt_pos..];
        let bytes = text.as_bytes();
        let mut i = 2;
        let limit = text.len().min(IMDB_MAX_NUMBERS + 2);
        while i < limit {
            let c = bytes[i];
            if !is_digit(c) {
                break;
            }

            i += 1;
        }

        // Skip if more than 8 digits + 2 chars for tt.
        if (IMDB_MIN_NUMBERS + 2..=IMDB_MAX_NUMBERS + 2).contains(&i) {
            return Some(&text[..i]);
        }

        text = &text[i..];
    }

    None
}

/// Parses a TMDb movie id out of a `themoviedb.org/movie/` url.
///
/// Mirrors `ProviderIdParsers.TryFindTmdbMovieId`.
#[must_use]
pub fn try_find_tmdb_movie_id(text: &str) -> Option<&str> {
    try_find_provider_id(text, "themoviedb.org/movie/")
}

/// Parses a TMDb series id out of a `themoviedb.org/tv/` url.
///
/// Mirrors `ProviderIdParsers.TryFindTmdbSeriesId`.
#[must_use]
pub fn try_find_tmdb_series_id(text: &str) -> Option<&str> {
    try_find_provider_id(text, "themoviedb.org/tv/")
}

/// Parses a TVDb id out of a `thetvdb.com/?tab=series&id=` url.
///
/// Mirrors `ProviderIdParsers.TryFindTvdbId`.
#[must_use]
pub fn try_find_tvdb_id(text: &str) -> Option<&str> {
    try_find_provider_id(text, "thetvdb.com/?tab=series&id=")
}

/// Shared scan: find `search_string`, then read the run of digits after it.
///
/// Mirrors the private `ProviderIdParsers.TryFindProviderId`.
fn try_find_provider_id<'a>(text: &'a str, search_string: &str) -> Option<&'a str> {
    let search_pos = text.find(search_string)?;

    let text = &text[search_pos + search_string.len()..];
    let bytes = text.as_bytes();

    let mut i = 0;
    while i < text.len() {
        let c = bytes[i];

        if !is_digit(c) {
            break;
        }

        i += 1;
    }

    if i >= 1 {
        return Some(&text[..i]);
    }

    None
}
