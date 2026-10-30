//! Ratings and gains that are not finite numbers.
//!
//! Jellyfin 12.3 stores a rating or gain that is NaN, +Infinity or -Infinity
//! as no value: every assignment to `BaseItem.CommunityRating`,
//! `CriticRating`, `LUFS` and `NormalizationGain` goes through the private
//! `BaseItem.FiniteOrNull` (`MediaBrowser.Controller/Entities/BaseItem.cs`,
//! v12.4 :1408), including when an item is read back from the database.
//! Ferrofin's items have no setters, so [`finite_or_none`] is applied where
//! those values enter an item, where they are written, and where they are
//! read.

/// `FiniteOrNull`: `value` when it is finite or has no value, otherwise
/// `None`. A non-finite rating cannot be stored by every database or written
/// as JSON.
///
/// The four properties are .NET `float`s, so a value is judged in the `float`
/// it becomes: one Ferrofin holds as an `f64` beyond `float`'s range (`1e39`)
/// is an infinity upstream (a provider's `double` cast on assignment, a REAL
/// column narrowed by EF on read) and so is no value too.
#[must_use]
pub fn finite_or_none<T: Copy + Into<f64>>(value: Option<T>) -> Option<T> {
    // The narrowing is the point: it is the cast upstream's assignment makes.
    #[allow(clippy::cast_possible_truncation)]
    value.filter(|v| ((*v).into() as f32).is_finite())
}

#[cfg(test)]
mod tests {
    use super::finite_or_none;
    use rstest::rstest;

    /// `BaseItemTests.RatingsAndGains_KeepOnlyFiniteValues` (v12.4): the same
    /// value assigned to the four properties reads back as `expected`.
    #[rstest]
    #[case(Some(f32::NAN), None)]
    #[case(Some(f32::INFINITY), None)]
    #[case(Some(f32::NEG_INFINITY), None)]
    #[case(Some(-3.5_f32), Some(-3.5_f32))]
    #[case(None, None)]
    fn ratings_and_gains_keep_only_finite_values(
        #[case] value: Option<f32>,
        #[case] expected: Option<f32>,
    ) {
        assert_eq!(finite_or_none(value), expected);
        // The entity columns are `f64`; the widening keeps NaN and the
        // infinities, so the answer is the same.
        assert_eq!(
            finite_or_none(value.map(f64::from)),
            expected.map(f64::from)
        );
    }

    /// An `f64` is judged as the `float` it becomes: past `float`'s range it
    /// is an infinity upstream, so no value; at the edge of the range it is
    /// kept as it is.
    #[rstest]
    #[case(1e39, None)]
    #[case(-1e39, None)]
    #[case(f64::MAX, None)]
    #[case(3.4e38, Some(3.4e38))]
    #[case(7.123_456_789, Some(7.123_456_789))]
    fn an_f64_is_judged_as_the_float_it_becomes(#[case] value: f64, #[case] expected: Option<f64>) {
        assert_eq!(finite_or_none(Some(value)), expected);
    }
}
