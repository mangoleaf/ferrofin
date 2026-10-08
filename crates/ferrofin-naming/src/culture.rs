//! The string order of .NET's default comparer.

use std::cmp::Ordering;
use std::sync::OnceLock;

/// Compares like `Comparer<string>.Default`, which every upstream
/// `OrderBy(x => x.SomeString)` sorts with: the current culture at
/// `CompareOptions.None`.
///
/// Jellyfin never pins `CurrentCulture` (only the UI culture, `Startup.cs`)
/// and does not run in invariant-globalization mode, so it is ICU collation
/// for the host locale — `en-US` in the official image, which CLDR leaves
/// untailored, i.e. root collation at tertiary strength, the same data
/// `InvariantCulture` uses.
///
/// It is not code-point order even on ASCII: letters compare case-blind at
/// the primary level and lowercase sorts before uppercase only as a
/// tie-break, so `movie part1` precedes `Movie Part2` where `'M' < 'm'` says
/// otherwise.
///
/// The collator reads ICU4X's baked root data; a build without it compares
/// ordinally rather than failing the scan.
pub(crate) fn culture_cmp(a: &str, b: &str) -> Ordering {
    static COLLATOR: OnceLock<Option<icu_collator::CollatorBorrowed<'static>>> = OnceLock::new();
    COLLATOR
        .get_or_init(|| {
            icu_collator::Collator::try_new(
                icu_collator::CollatorPreferences::default(),
                icu_collator::options::CollatorOptions::default(),
            )
            .ok()
        })
        .as_ref()
        .map_or_else(|| a.cmp(b), |collator| collator.compare(a, b))
}

#[cfg(test)]
mod tests {
    use super::culture_cmp;
    use std::cmp::Ordering;

    #[test]
    fn letters_order_before_case() {
        assert_eq!(culture_cmp("movie part1", "Movie Part2"), Ordering::Less);
        assert_eq!(culture_cmp("apple", "Banana"), Ordering::Less);
        assert_eq!(culture_cmp("a", "A"), Ordering::Less);
        assert_eq!(culture_cmp("mp3", "mp3"), Ordering::Equal);
    }
}
