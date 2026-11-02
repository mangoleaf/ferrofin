//! Port of `PathExtensions.GetAttributeValue`
//! (`Emby.Server.Implementations/Library/PathExtensions.cs`): the value of a
//! `[name=value]`, `[name-value]`, `{name-value}` or `(name=value)` attribute
//! in a file or folder name, the way the resolvers read the provider ids
//! users put in names (`Heat (1995) [tmdbid-949]`). It sits beside
//! [`crate::providers::try_find_imdb_id`], its IMDb fallback.

/// [`get_attribute_value`] was handed an empty name or attribute (C#'s
/// `ArgumentException`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyArgument;

impl std::fmt::Display for EmptyArgument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("string can't be empty")
    }
}

impl std::error::Error for EmptyArgument {}

/// The value of `attribute` in `text`: the first `[attribute=value]`,
/// `[attribute-value]`, `{…}` or `(…)` form whose value is not blank,
/// trimmed. `tmdbid`, `tvdbid` and `imdbid` also answer to `tmdb`, `tvdb`
/// and `imdb`, and `imdbid` falls back to any IMDb id in `text`
/// ([`try_find_imdb_id`](crate::providers::try_find_imdb_id)). The attribute
/// matches ignoring case (`OrdinalIgnoreCase`).
///
/// # Errors
///
/// [`EmptyArgument`] when `text` or `attribute` is empty.
pub fn get_attribute_value(text: &str, attribute: &str) -> Result<Option<String>, EmptyArgument> {
    if text.is_empty() || attribute.is_empty() {
        return Err(EmptyArgument);
    }
    // An alias is the attribute without its "id" suffix.
    let short: &str = ["tmdbid", "tvdbid", "imdbid"]
        .into_iter()
        .find(|full| attribute.eq_ignore_ascii_case(full))
        .map_or("", |full| &full[..4]);
    let needle: Vec<char> = if short.is_empty() { attribute } else { short }
        .chars()
        .collect();
    let text_chars: Vec<char> = text.chars().collect();
    let mut start = 0;
    while let Some(index) = find_ignore_case(&text_chars[start..], &needle) {
        let sub = &text_chars[start..];
        let mut end = index + needle.len();
        // The next search starts after this match.
        start += end;
        if index == 0 {
            continue;
        }
        let closer = match sub[index - 1] {
            '[' => ']',
            '(' => ')',
            '{' => '}',
            _ => continue,
        };
        // An alias followed by "id" is the full attribute.
        if !short.is_empty()
            && end + 1 < sub.len()
            && sub[end].eq_ignore_ascii_case(&'i')
            && sub[end + 1].eq_ignore_ascii_case(&'d')
        {
            end += 2;
        }
        // The separator, at least one character, and the closer.
        if end + 2 < sub.len()
            && matches!(sub[end], '=' | '-')
            && let Some(closing) = sub[end..].iter().position(|&c| c == closer)
            && closing > 1
        {
            let value: String = sub[end + 1..end + closing].iter().collect();
            let value = value.trim();
            if !value.is_empty() {
                return Ok(Some(value.to_owned()));
            }
        }
    }
    if attribute.eq_ignore_ascii_case("imdbid") {
        return Ok(crate::providers::try_find_imdb_id(text).map(str::to_owned));
    }
    Ok(None)
}

/// The first index of `needle` in `haystack`, comparing characters as
/// `OrdinalIgnoreCase` does (their simple uppercase).
fn find_ignore_case(haystack: &[char], needle: &[char]) -> Option<usize> {
    let upper = |c: char| {
        let mut upper = c.to_uppercase();
        match (upper.next(), upper.next()) {
            (Some(u), None) => u,
            _ => c,
        }
    };
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&at| {
        haystack[at..at + needle.len()]
            .iter()
            .zip(needle)
            .all(|(&a, &b)| a == b || upper(a) == upper(b))
    })
}

#[cfg(test)]
mod tests {
    use super::{EmptyArgument, get_attribute_value};

    /// `PathExtensionsTests.GetAttributeValue_ValidArgs_Correct`.
    #[rstest::rstest]
    #[case("Superman: Red Son [imdbid=tt10985510]", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son [imdb=tt10985510]", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son [imdbid-tt10985510]", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son [imdb-tt10985510]", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son - tt10985510", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son {imdbid=tt10985510}", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son {imdb=tt10985510}", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son {imdbid-tt10985510}", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son {imdb-tt10985510}", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son (imdbid=tt10985510)", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son (imdb=tt10985510)", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son (imdbid-tt10985510)", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son (imdb-tt10985510)", "imdbid", Some("tt10985510"))]
    #[case("Superman: Red Son", "imdbid", None)]
    #[case(
        "Superman: Red Son [imdbid1=tt11111111][imdbid=tt10985510]",
        "imdbid",
        Some("tt10985510")
    )]
    #[case(
        "Superman: Red Son [imdbid1=tt11111111][imdb=tt10985510]",
        "imdbid",
        Some("tt10985510")
    )]
    #[case(
        "Superman: Red Son {imdbid1=tt11111111}(imdbid=tt10985510)",
        "imdbid",
        Some("tt10985510")
    )]
    #[case(
        "Superman: Red Son {imdbid1=tt11111111}(imdb=tt10985510)",
        "imdbid",
        Some("tt10985510")
    )]
    #[case(
        "Superman: Red Son (imdbid1-tt11111111)[imdbid=tt10985510]",
        "imdbid",
        Some("tt10985510")
    )]
    #[case(
        "Superman: Red Son (imdbid1-tt11111111)[imdb=tt10985510]",
        "imdbid",
        Some("tt10985510")
    )]
    #[case(
        "Superman: Red Son [tmdbid=618355][imdbid=tt10985510]",
        "imdbid",
        Some("tt10985510")
    )]
    #[case(
        "Superman: Red Son [tmdbid=618355][imdb=tt10985510]",
        "imdbid",
        Some("tt10985510")
    )]
    #[case(
        "Superman: Red Son [tmdbid-618355]{imdbid-tt10985510}",
        "imdbid",
        Some("tt10985510")
    )]
    #[case(
        "Superman: Red Son [tmdbid-618355]{imdb-tt10985510}",
        "imdbid",
        Some("tt10985510")
    )]
    #[case(
        "Superman: Red Son (tmdbid-618355)[imdbid-tt10985510]",
        "tmdbid",
        Some("618355")
    )]
    #[case(
        "Superman: Red Son (tmdbid-618355)[imdb-tt10985510]",
        "tmdbid",
        Some("618355")
    )]
    #[case("Superman: Red Son [providera-id=1]", "providera-id", Some("1"))]
    #[case("Superman: Red Son [providerb-id=2]", "providerb-id", Some("2"))]
    #[case("Superman: Red Son [providera id=4]", "providera id", Some("4"))]
    #[case("Superman: Red Son [providerb id=5]", "providerb id", Some("5"))]
    #[case(
        "Superman: Red Son [provider=99][providerid=5]",
        "providerid",
        Some("5")
    )]
    #[case("Superman: Red Son [tmdbid=3]", "tmdbid", Some("3"))]
    #[case("Superman: Red Son [tmdb=3]", "tmdbid", Some("3"))]
    #[case("Superman: Red Son [tmdbid-3]", "tmdbid", Some("3"))]
    #[case("Superman: Red Son [tmdb-3]", "tmdbid", Some("3"))]
    #[case("Superman: Red Son {tmdbid=3}", "tmdbid", Some("3"))]
    #[case("Superman: Red Son {tmdb=3}", "tmdbid", Some("3"))]
    #[case("Superman: Red Son {tmdbid-3}", "tmdbid", Some("3"))]
    #[case("Superman: Red Son {tmdb-3}", "tmdbid", Some("3"))]
    #[case("Superman: Red Son (tmdbid=6)", "tmdbid", Some("6"))]
    #[case("Superman: Red Son (tmdb=6)", "tmdbid", Some("6"))]
    #[case("Superman: Red Son (tmdbid-6)", "tmdbid", Some("6"))]
    #[case("Superman: Red Son (tmdb-6)", "tmdbid", Some("6"))]
    #[case("Superman: Red Son [tvdbid=6]", "tvdbid", Some("6"))]
    #[case("Superman: Red Son [tvdb=6]", "tvdbid", Some("6"))]
    #[case("Superman: Red Son [tvdbid-6]", "tvdbid", Some("6"))]
    #[case("Superman: Red Son [tvdb-6]", "tvdbid", Some("6"))]
    #[case("Superman: Red Son {tvdbid=3}", "tvdbid", Some("3"))]
    #[case("Superman: Red Son {tvdb=3}", "tvdbid", Some("3"))]
    #[case("Superman: Red Son {tvdbid-3}", "tvdbid", Some("3"))]
    #[case("Superman: Red Son {tvdb-3}", "tvdbid", Some("3"))]
    #[case("Superman: Red Son (tvdbid=6)", "tvdbid", Some("6"))]
    #[case("Superman: Red Son (tvdb=6)", "tvdbid", Some("6"))]
    #[case("Superman: Red Son (tvdbid-6)", "tvdbid", Some("6"))]
    #[case("Superman: Red Son (tvdb-6)", "tvdbid", Some("6"))]
    #[case("[tmdbid=618355]", "tmdbid", Some("618355"))]
    #[case("[tmdb=618355]", "tmdbid", Some("618355"))]
    #[case("{tmdbid=618355}", "tmdbid", Some("618355"))]
    #[case("{tmdb=618355}", "tmdbid", Some("618355"))]
    #[case("(tmdbid=618355)", "tmdbid", Some("618355"))]
    #[case("(tmdb=618355)", "tmdbid", Some("618355"))]
    #[case("[tmdbid-618355]", "tmdbid", Some("618355"))]
    #[case("[tmdb-618355]", "tmdbid", Some("618355"))]
    #[case("{tmdbid-618355)", "tmdbid", None)]
    #[case("{tmdb-618355)", "tmdbid", None)]
    #[case("[tmdbid-618355}", "tmdbid", None)]
    #[case("[tmdb-618355}", "tmdbid", None)]
    #[case("tmdbid=111111][tmdbid=618355]", "tmdbid", Some("618355"))]
    #[case("tmdbid=111111][tmdb=618355]", "tmdbid", Some("618355"))]
    #[case("[tmdbid=618355]tmdbid=111111]", "tmdbid", Some("618355"))]
    #[case("[tmdb=618355]tmdbid=111111]", "tmdbid", Some("618355"))]
    #[case("tmdbid=618355]", "tmdbid", None)]
    #[case("tmdb=618355]", "tmdbid", None)]
    #[case("[tmdbid=618355", "tmdbid", None)]
    #[case("[tmdb=618355", "tmdbid", None)]
    #[case("tmdbid=618355", "tmdbid", None)]
    #[case("tmdb=618355", "tmdbid", None)]
    #[case("tmdbid=", "tmdbid", None)]
    #[case("tmdb=", "tmdbid", None)]
    #[case("tmdbid", "tmdbid", None)]
    #[case("tmdb", "tmdbid", None)]
    #[case("[tmdbid= ][tmdbid=223344]", "tmdbid", Some("223344"))]
    #[case("[tmdb= ][tmdb=223344]", "tmdbid", Some("223344"))]
    #[case("[tmdbid=  ][tmdb=223344]", "tmdbid", Some("223344"))]
    #[case("[tmdb=   ][tmdbid=223344]", "tmdbid", Some("223344"))]
    #[case("[tmdbid=][imdbid=tt10985510]", "tmdbid", None)]
    #[case("[tmdb=][imdbid=tt10985510]", "tmdbid", None)]
    #[case("[tmdbid-][imdbid-tt10985510]", "tmdbid", None)]
    #[case("[tmdb-][imdbid-tt10985510]", "tmdbid", None)]
    #[case(
        "Superman: Red Son [tmdbid-618355][tmdbid=1234567]",
        "tmdbid",
        Some("618355")
    )]
    #[case(
        "Superman: Red Son [tmdb-618355][tmdbid=1234567]",
        "tmdbid",
        Some("618355")
    )]
    #[case("{tmdbid=}{imdbid=tt10985510}", "tmdbid", None)]
    #[case("{tmdb=}{imdbid=tt10985510}", "tmdbid", None)]
    #[case("(tmdbid-)(imdbid-tt10985510)", "tmdbid", None)]
    #[case("(tmdb-)(imdbid-tt10985510)", "tmdbid", None)]
    #[case(
        "Superman: Red Son {tmdbid-618355}{tmdbid=1234567}",
        "tmdbid",
        Some("618355")
    )]
    #[case(
        "Superman: Red Son {tmdb-618355}{tmdbid=1234567}",
        "tmdbid",
        Some("618355")
    )]
    #[case(
        "Superman: Red Son - tt10985510 [imdbid1=tt11]",
        "imdbid",
        Some("tt10985510")
    )]
    #[case("Superman: Red Son [tmdb=618355][tmdbid1=1]", "tmdbid", Some("618355"))]
    #[case(
        "Superman: Red Son [tmdb=618355][tmdbid=12345]",
        "tmdbid",
        Some("618355")
    )]
    fn get_attribute_value_valid_args_correct(
        #[case] input: &str,
        #[case] attribute: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(
            get_attribute_value(input, attribute),
            Ok(expected.map(str::to_owned)),
            "{input} / {attribute}"
        );
    }

    /// `PathExtensionsTests.GetAttributeValue_EmptyString_ThrowsArgumentException`.
    #[rstest::rstest]
    #[case("", "")]
    #[case("Superman: Red Son [imdbid=tt10985510]", "")]
    #[case("", "imdbid")]
    fn get_attribute_value_empty_string_is_an_error(#[case] input: &str, #[case] attribute: &str) {
        assert_eq!(get_attribute_value(input, attribute), Err(EmptyArgument));
    }
}
