//! .NET's floating-point parse in the invariant culture.
//!
//! `double.Parse`/`float.Parse` with `NumberStyles.Float |
//! NumberStyles.AllowThousands` and `CultureInfo.InvariantCulture` is what
//! ASP.NET's `FloatingPointTypeModelBinderProvider` binds a `double` or `float`
//! query and route value with, and what the DLNA `ConditionProcessor` and
//! `StreamBuilder` read a device profile's condition value with
//! (`double.TryParse`/`float.TryParse`, the same styles). Rust's `f64::from_str`
//! differs from it on `inf`, on group separators, on Unicode white space
//! around a number and on trailing NULs.

use std::str::FromStr;

/// The white space `NumberStyles.AllowLeadingWhite`/`AllowTrailingWhite` skip
/// (`Number.IsWhite`): the space and U+0009 to U+000D, no other Unicode space.
fn is_white(c: char) -> bool {
    matches!(c, ' ' | '\t'..='\r')
}

/// The invariant culture's infinity and NaN symbols, each with the spelling
/// Rust's parser reads for it. `inf` is Rust's alone.
const SYMBOLS: [(&str, &str); 6] = [
    ("Infinity", "inf"),
    ("+Infinity", "inf"),
    ("-Infinity", "-inf"),
    ("NaN", "NaN"),
    ("+NaN", "NaN"),
    ("-NaN", "NaN"),
];

/// `T::TryParse(text, NumberStyles.Float | NumberStyles.AllowThousands,
/// CultureInfo.InvariantCulture, out _)` for `T` `f64` (`double`) or `f32`
/// (`float`), or `None` where it fails. Measured on Jellyfin 12.4:
///
/// * the number may have ASCII white space around it (no NBSP or other Unicode
///   space), and NULs after that (`TrailingZeros`);
/// * one leading sign, the digits, a `.` and an exponent, as Rust reads them;
/// * any number of `,` group separators after the first digit and before the
///   decimal point or exponent (`1,000`, `1,`, `1,,0`, `1,e5`; `,1` and `1.5,0`
///   fail), which are dropped;
/// * when that fails, the text trimmed of Unicode white space may be one of the
///   symbols `Infinity`, `-Infinity`, `NaN`, signed `+Infinity`, `+NaN` or
///   `-NaN`, in any case. Rust's `inf` is not one.
///
/// A value out of range is an infinity or zero, in both. Only for `f64` and
/// `f32`: an integer `T` would take the group separators `int.Parse` refuses.
#[must_use]
pub fn parse<T: FromStr>(text: &str) -> Option<T> {
    let number = text.trim_end_matches('\0').trim_matches(is_white);
    let unsigned = number.strip_prefix(['+', '-']).unwrap_or(number);
    if unsigned.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        let (integer, rest) =
            unsigned.split_at(unsigned.find(['.', 'e', 'E']).unwrap_or(unsigned.len()));
        if !integer.contains(',') {
            return number.parse().ok();
        }
        let sign = &number[..number.len() - unsigned.len()];
        return format!("{sign}{}{rest}", integer.replace(',', ""))
            .parse()
            .ok();
    }
    let symbol = text.trim();
    SYMBOLS
        .iter()
        .find(|(dotnet, _)| dotnet.eq_ignore_ascii_case(symbol))
        .and_then(|(_, rust)| rust.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::parse;
    use rstest::rstest;

    /// Values `GET /Items?minCommunityRating=` (`double?`) and
    /// `GET /Videos/{itemId}/stream?maxFramerate=` (`float?`) bound on a stock
    /// Jellyfin 12.4, with the number each one reads as.
    #[rstest]
    #[case("1", 1.0)]
    #[case("1.5", 1.5)]
    #[case("-1.5", -1.5)]
    #[case("+1.5", 1.5)]
    #[case(" 1.5 ", 1.5)]
    #[case("\t1.5\t", 1.5)]
    #[case("\n1\n", 1.0)]
    #[case("\u{b}1\u{b}", 1.0)]
    #[case("\u{c}1\u{c}", 1.0)]
    #[case("\r1\r", 1.0)]
    #[case("00001.5000", 1.5)]
    #[case(".5", 0.5)]
    #[case("5.", 5.0)]
    #[case("+.5", 0.5)]
    #[case("-.5", -0.5)]
    #[case("1.e5", 1e5)]
    #[case("1e5", 1e5)]
    #[case("1E5", 1e5)]
    #[case("1e+5", 1e5)]
    #[case("1e-5", 1e-5)]
    #[case("1e-400", 0.0)]
    #[case("1,000", 1000.0)]
    #[case("1,000.5", 1000.5)]
    #[case("1,", 1.0)]
    #[case("1,,0", 10.0)]
    #[case("12,34,56", 123_456.0)]
    #[case("1,5", 15.0)]
    #[case("0,5", 5.0)]
    #[case("1,e5", 1e5)]
    #[case("1,.5", 1.5)]
    #[case("+1,0", 10.0)]
    #[case("-1,0", -10.0)]
    #[case("1,000,", 1000.0)]
    #[case(" 1,000 ", 1000.0)]
    #[case("1\0", 1.0)]
    #[case("1\0\0", 1.0)]
    #[case("1 \0", 1.0)]
    #[case("1.5\0", 1.5)]
    fn numbers_bind(#[case] text: &str, #[case] expected: f64) {
        assert_eq!(parse::<f64>(text), Some(expected), "{text:?}");
        #[allow(clippy::cast_possible_truncation)]
        let narrowed = expected as f32;
        assert_eq!(parse::<f32>(text), Some(narrowed), "{text:?}");
    }

    /// The infinity symbols bound on Jellyfin 12.4, in any case and inside
    /// Unicode white space, and a number past `double`'s range.
    #[rstest]
    #[case("infinity", f64::INFINITY)]
    #[case("Infinity", f64::INFINITY)]
    #[case("INFINITY", f64::INFINITY)]
    #[case("+Infinity", f64::INFINITY)]
    #[case("+infinity", f64::INFINITY)]
    #[case("-Infinity", f64::NEG_INFINITY)]
    #[case("-INFINITY", f64::NEG_INFINITY)]
    #[case(" Infinity ", f64::INFINITY)]
    #[case("\u{a0}Infinity", f64::INFINITY)]
    #[case("Infinity\u{a0}", f64::INFINITY)]
    #[case("\u{a0}-Infinity", f64::NEG_INFINITY)]
    #[case("1e400", f64::INFINITY)]
    #[case("-1e400", f64::NEG_INFINITY)]
    fn infinities_bind(#[case] text: &str, #[case] expected: f64) {
        assert_eq!(parse::<f64>(text), Some(expected), "{text:?}");
        #[allow(clippy::cast_possible_truncation)]
        let narrowed = expected as f32;
        assert_eq!(parse::<f32>(text), Some(narrowed), "{text:?}");
    }

    /// The NaN symbols bound on Jellyfin 12.4, signed or not, in any case and
    /// inside Unicode white space.
    #[rstest]
    #[case("NaN")]
    #[case("nan")]
    #[case("NAN")]
    #[case("+NaN")]
    #[case("-NaN")]
    #[case("+nan")]
    #[case("-nan")]
    #[case("\tNaN")]
    #[case("\u{3000}NaN\u{3000}")]
    fn nans_bind(#[case] text: &str) {
        assert!(parse::<f64>(text).is_some_and(f64::is_nan), "{text:?}");
        assert!(parse::<f32>(text).is_some_and(f32::is_nan), "{text:?}");
    }

    /// `float` overflows to an infinity on its own range, as Rust's `f32` does.
    #[rstest]
    #[case("1e39", f32::INFINITY)]
    #[case("-1e39", f32::NEG_INFINITY)]
    #[case("3.4028235e38", f32::MAX)]
    #[case("3.4028236e38", f32::INFINITY)]
    fn floats_overflow_on_their_own_range(#[case] text: &str, #[case] expected: f32) {
        assert_eq!(parse::<f32>(text), Some(expected));
    }

    /// Values Jellyfin 12.4 answered 400 for, on both routes.
    #[rstest]
    #[case("")]
    #[case(" ")]
    #[case("\0")]
    #[case(".")]
    #[case(".e5")]
    #[case("e5")]
    #[case("1e")]
    #[case("1e+")]
    #[case("1e-")]
    #[case("1e5.5")]
    #[case("1e1e1")]
    #[case("1.2.3")]
    #[case("\u{a0}1")]
    #[case("1\u{a0}")]
    #[case("\u{3000}1")]
    #[case(",1")]
    #[case("-,1")]
    #[case("+,1")]
    #[case(".,5")]
    #[case("1.5,0")]
    #[case("1e5,0")]
    #[case("1e,5")]
    #[case("1, 1")]
    #[case("1,000.5,")]
    #[case("inf")]
    #[case("+inf")]
    #[case("-inf")]
    #[case("INF")]
    #[case("\u{221e}")]
    #[case("-\u{221e}")]
    #[case("+-Infinity")]
    #[case("- Infinity")]
    #[case("+ NaN")]
    #[case("--NaN")]
    #[case("Infinity1")]
    #[case("Infinit")]
    #[case("NaNa")]
    #[case("1\0 ")]
    #[case("\u{0}1")]
    #[case("1\u{0}5")]
    #[case("Infinity\0")]
    #[case("NaN\0")]
    #[case("0x10")]
    #[case("1_0")]
    #[case("1.5 x")]
    #[case("- 5")]
    #[case("+-5")]
    #[case("--5")]
    #[case("5-")]
    #[case("5+")]
    #[case("(5)")]
    #[case("\u{a4}5")]
    #[case("$5")]
    #[case("\u{ff11}")]
    #[case("\u{661}")]
    fn refused(#[case] text: &str) {
        assert_eq!(parse::<f64>(text), None, "{text:?}");
        assert_eq!(parse::<f32>(text), None, "{text:?}");
    }
}
