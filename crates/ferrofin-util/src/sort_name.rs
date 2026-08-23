//! Sort-name derivation — port of C# `BaseItem.CreateSortName` / `ModifySortChunks`
//! (`MediaBrowser.Controller/Entities/BaseItem.cs`), with Jellyfin's default
//! `SortRemoveCharacters` / `SortReplaceCharacters` / `SortRemoveWords`.

/// Port of C# `BaseItem.CreateSortName` + `ModifySortChunks`: lowercase the name, apply the
/// default `SortReplace`/`SortRemove` characters and strip a leading article, then left-pad each
/// run of digits to 10 so numbers sort naturally (e.g. `Movie 0001 (2020)` →
/// `movie 0000000001 (0000002020)`).
#[must_use]
pub fn create_sort_name(name: &str) -> String {
    let mut s = name.trim().to_lowercase();
    for c in [',', '&', '-', '{', '}', '\''] {
        s = s.replace(c, ""); // default SortRemoveCharacters
    }
    for c in ['.', '+', '%'] {
        s = s.replace(c, " "); // default SortReplaceCharacters → space
    }
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = s.strip_prefix(article) {
            s = rest.to_owned();
            break;
        }
    }
    modify_sort_chunks(&s)
}

/// Left-pads each maximal run of ASCII digits in `name` to width 10 with `0`.
fn modify_sort_chunks(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut digits = String::new();
            while chars.peek().is_some_and(char::is_ascii_digit) {
                digits.push(chars.next().unwrap());
            }
            for _ in digits.len()..10 {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            out.push(c);
            chars.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::create_sort_name;

    #[test]
    fn create_sort_name_matches_jellyfin() {
        assert_eq!(
            create_sort_name("Movie 0001 (2020)"),
            "movie 0000000001 (0000002020)"
        );
        assert_eq!(create_sort_name("The Matrix"), "matrix");
        assert_eq!(create_sort_name("Se7en"), "se0000000007en");
    }
}
