//! Sort-name derivation — port of C# `BaseItem.CreateSortName` / `ModifySortChunks`
//! (`MediaBrowser.Controller/Entities/BaseItem.cs`), with Jellyfin's default
//! `SortRemoveWords` / `SortRemoveCharacters` / `SortReplaceCharacters`.

/// `ServerConfiguration.SortRemoveWords` — leading/interior/trailing articles.
const SORT_REMOVE_WORDS: [&str; 3] = ["the", "a", "an"];

/// `ServerConfiguration.SortRemoveCharacters` — deleted outright.
const SORT_REMOVE_CHARACTERS: [char; 6] = [',', '&', '-', '{', '}', '\''];

/// `ServerConfiguration.SortReplaceCharacters` — each becomes a space.
const SORT_REPLACE_CHARACTERS: [char; 3] = ['.', '+', '%'];

/// Port of C# `BaseItem.CreateSortName` + `ModifySortChunks`.
///
/// Lower-cases the trimmed name, removes each article where it stands as a
/// whole word (at the start, surrounded by spaces, or at the end), then applies
/// the remove/replace character sets, then left-pads every run of digits to 10
/// so numbers sort naturally (`Movie 0001 (2020)` → `movie 0000000001
/// (0000002020)`).
///
/// **The stage order is load-bearing and matches C#: words, then removes, then
/// replaces.** Doing characters first changes the answer for real titles —
/// `A.I. Artificial Intelligence` replaces `.`→space into `a i  artificial …`,
/// which then *starts* with the article `a` and gets it stripped. C# strips
/// articles while the `.` is still attached, so nothing matches.
#[must_use]
pub fn create_sort_name(name: &str) -> String {
    let mut sortable = name.trim().to_lowercase();
    for search in SORT_REMOVE_WORDS {
        if let Some(rest) = sortable.strip_prefix(&format!("{search} ")) {
            sortable = rest.to_owned();
        }
        sortable = sortable.replace(&format!(" {search} "), " ");
        if let Some(rest) = sortable.strip_suffix(&format!(" {search}")) {
            sortable = rest.to_owned();
        }
    }
    for c in SORT_REMOVE_CHARACTERS {
        sortable = sortable.replace(c, "");
    }
    for c in SORT_REPLACE_CHARACTERS {
        sortable = sortable.replace(c, " ");
    }
    modify_sort_chunks(&sortable)
}

/// The sort key C# derives from a non-empty `ForcedSortName`.
///
/// `BaseItem.SortName` short-circuits to `ModifySortChunks(ForcedSortName)
/// .ToLowerInvariant()` — the digit padding and the lower-casing, but
/// deliberately **not** the article/character stripping: a forced sort name is
/// the user's explicit answer and only gets the numeric normalization that
/// makes it comparable with derived keys.
#[must_use]
pub fn forced_sort_key(forced: &str) -> String {
    modify_sort_chunks(forced).to_lowercase()
}

/// Left-pads each maximal run of ASCII digits in `name` to width 10 with `0`.
fn modify_sort_chunks(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut digits = String::new();
            while chars.peek().is_some_and(char::is_ascii_digit) {
                digits.push(chars.next().unwrap_or_default());
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
    use super::{create_sort_name, forced_sort_key};

    #[test]
    fn create_sort_name_matches_jellyfin() {
        assert_eq!(
            create_sort_name("Movie 0001 (2020)"),
            "movie 0000000001 (0000002020)"
        );
        assert_eq!(create_sort_name("The Matrix"), "matrix");
        assert_eq!(create_sort_name("A Beautiful Mind"), "beautiful mind");
        assert_eq!(create_sort_name("An Education"), "education");
        assert_eq!(create_sort_name("Theatre of Blood"), "theatre of blood");
        assert_eq!(create_sort_name("Se7en"), "se0000000007en");
    }

    /// C# removes an article anywhere it stands as a whole word, not just at
    /// the front — `sortable.Replace(" the ", " ")` and the `EndsWith` arm.
    #[test]
    fn articles_are_removed_in_the_middle_and_at_the_end() {
        assert_eq!(
            create_sort_name("Attack of the Killer Tomatoes"),
            "attack of killer tomatoes"
        );
        assert_eq!(create_sort_name("All About the"), "all about");
        assert_eq!(create_sort_name("Withnail and I"), "withnail and i");
    }

    /// Words run BEFORE the character sets. With the order reversed the `.`
    /// becomes a space first and `a` then looks like a leading article.
    #[test]
    fn words_are_stripped_before_characters_are_replaced() {
        assert_eq!(
            create_sort_name("A.I. Artificial Intelligence"),
            "a i  artificial intelligence"
        );
        // Removal characters likewise: the leading `-` means C# sees no
        // leading article at all, and only deletes the dash afterwards.
        assert_eq!(create_sort_name("-The Matrix"), "the matrix");
    }

    /// A name that is *only* an article keeps it — C# matches on `"the "`,
    /// `" the "` and `" the"`, none of which occur in a bare `"The"`.
    #[test]
    fn a_bare_article_survives() {
        assert_eq!(create_sort_name("The"), "the");
        assert_eq!(create_sort_name("Theatre"), "theatre");
    }

    /// `ForcedSortName` skips article and character handling.
    #[test]
    fn a_forced_sort_name_is_only_padded_and_lowercased() {
        assert_eq!(forced_sort_key("The Matrix 2"), "the matrix 0000000002");
        assert_eq!(forced_sort_key("A.I."), "a.i.");
    }
}
