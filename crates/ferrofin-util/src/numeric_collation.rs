//! The name order Jellyfin 12 sorts with.

use std::cmp::Ordering;
use std::sync::OnceLock;

/// Compares like `StringComparer.Create(CultureInfo.InvariantCulture,
/// CompareOptions.NumericOrdering)`: the comparer of `SortExtensions`
/// (`OrderByString`, `ThenByString` and their descending forms) and of
/// `VideoListResolver`'s alternate versions, which replaced
/// `AlphanumericComparator` in Jellyfin 12.
///
/// Jellyfin runs on ICU, so it is CLDR root collation at tertiary strength
/// with ICU's numeric collation on: letters compare case-blind first and
/// lowercase sorts before uppercase only as a tie-break, punctuation sorts
/// before digits and digits before letters, and a run of digits compares by
/// its value with leading zeros ignored, so `Show 007` equals `Show 7`.
///
/// The collator reads ICU4X's baked root data; a build without it compares
/// ordinally rather than failing.
#[must_use]
pub fn invariant_numeric_cmp(a: &str, b: &str) -> Ordering {
    static COLLATOR: OnceLock<Option<icu_collator::CollatorBorrowed<'static>>> = OnceLock::new();
    COLLATOR
        .get_or_init(|| {
            let mut preferences = icu_collator::CollatorPreferences::default();
            preferences.numeric_ordering =
                Some(icu_collator::preferences::CollationNumericOrdering::True);
            icu_collator::Collator::try_new(
                preferences,
                icu_collator::options::CollatorOptions::default(),
            )
            .ok()
        })
        .as_ref()
        .map_or_else(|| a.cmp(b), |collator| collator.compare(a, b))
}

#[cfg(test)]
mod tests {
    use super::invariant_numeric_cmp;
    use rstest::rstest;
    use std::cmp::Ordering::{self, Equal, Greater, Less};

    /// Expected values from .NET 10 on ICU (`mcr.microsoft.com/dotnet/sdk:10.0`):
    /// `Math.Sign(StringComparer.Create(CultureInfo.InvariantCulture,
    /// CompareOptions.NumericOrdering).Compare(a, b))`.
    #[rstest]
    #[case("theatrical", "Unrated", Less)]
    #[case("a", "B", Less)]
    #[case("a", "A", Less)]
    #[case("1080p hdr", "1080p Remux", Less)]
    #[case("Show 2", "Show 10", Less)]
    #[case("2160p", "1080p", Greater)]
    #[case("Show 007", "Show 7", Equal)]
    #[case("the Show 2", "The Show 1", Greater)]
    #[case("[Remux]", "Extended", Less)]
    #[case("Movie (2020) - theatrical", "Movie (2020) - Unrated", Less)]
    #[case("Café 2", "Café 10", Less)]
    #[case("", "a", Less)]
    #[case("Show", "Show 1", Less)]
    #[case("apple parity", "Banana parity", Less)]
    #[case("_x", "A", Less)]
    #[case("x1", "x a", Greater)]
    #[case("Movie 1", "Movie 01", Equal)]
    #[case("4K", "1080p", Less)]
    #[case("Episode 1x02", "Episode 1x10", Less)]
    #[case("a10b", "a2b", Greater)]
    #[case("Movie - 1080p", "Movie - 720p", Greater)]
    #[case("Movie-Extended", "Movie - Extended", Greater)]
    fn compares_as_dotnet_numeric_ordering(
        #[case] a: &str,
        #[case] b: &str,
        #[case] expected: Ordering,
    ) {
        assert_eq!(invariant_numeric_cmp(a, b), expected, "{a:?} vs {b:?}");
        assert_eq!(
            invariant_numeric_cmp(b, a),
            expected.reverse(),
            "{b:?} vs {a:?}"
        );
    }

    /// `list.OrderBy(x => x, comparer)` in .NET 10: stable, so the equal
    /// `Show 7` and `Show 007` keep their order.
    #[test]
    fn a_stable_sort_orders_as_linq_does() {
        let mut list = [
            "Show 7", "Show 007", "show 10", "Show 2", "[x]", "_y", "Zed", "zed", "éclair",
            "eclair",
        ];
        list.sort_by(|a, b| invariant_numeric_cmp(a, b));
        assert_eq!(
            list,
            [
                "_y", "[x]", "eclair", "éclair", "Show 2", "Show 7", "Show 007", "show 10", "zed",
                "Zed",
            ]
        );
    }
}
