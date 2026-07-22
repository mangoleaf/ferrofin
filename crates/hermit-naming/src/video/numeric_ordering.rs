//! Numeric-ordinal string comparison.
//!
//! Reproduces .NET's `CompareOptions.NumericOrdering` (the
//! `_numericOrdinalComparer` in `VideoListResolver`): runs of ASCII digits are
//! compared by their numeric value, everything else char-by-char.

use std::cmp::Ordering;

/// Compares two strings using numeric-ordinal ordering.
#[must_use]
pub fn numeric_ordinal_cmp(a: &str, b: &str) -> Ordering {
    let mut ac = a.chars().peekable();
    let mut bc = b.chars().peekable();

    loop {
        match (ac.peek().copied(), bc.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let av = take_number(&mut ac);
                    let bv = take_number(&mut bc);
                    match av.cmp(&bv) {
                        Ordering::Equal => {}
                        non_eq => return non_eq,
                    }
                } else {
                    match x.cmp(&y) {
                        Ordering::Equal => {
                            ac.next();
                            bc.next();
                        }
                        non_eq => return non_eq,
                    }
                }
            }
        }
    }
}

fn take_number(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> u128 {
    // Skip leading zeros so "007" == "7" numerically, then accumulate. Values
    // in filenames are small, so u128 never overflows in practice.
    let mut value: u128 = 0;
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            value = value
                .saturating_mul(10)
                .saturating_add(u128::from(c as u8 - b'0'));
            chars.next();
        } else {
            break;
        }
    }
    value
}
