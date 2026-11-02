//! The .NET runtime behaviour the libse 4.0.12 subtitle writers depend on.
//!
//! libse leans on `System.Drawing.Color` / `ColorTranslator`, on `decimal`, and on
//! .NET's number formatting, and its output bytes follow from those. These are ports
//! of exactly the members the writers call, on the Linux runtime Jellyfin's image
//! uses (`en-US` culture). [`KNOWN_COLORS`] is the runtime's own `KnownColor` table,
//! dumped from .NET 10 on Linux.

use std::sync::LazyLock;

use regex::Regex;

/// `\p{Nd}`: .NET's `char.IsDigit`.
static DECIMAL_DIGIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\p{Nd}$").expect("valid regex"));
/// `\p{L}`: .NET's `char.IsLetter` (`Lu`, `Ll`, `Lt`, `Lm`, `Lo`).
static LETTER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{L}$").expect("valid regex"));

/// `char.IsDigit`: a decimal digit (`Nd`). Narrower than `char::is_numeric`, which also
/// takes `²`, `½` and `Ⅷ`. .NET classifies UTF-16 units, so a character beyond the
/// BMP (two surrogate units, neither a digit) never is one.
#[must_use]
pub fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
        || (!c.is_ascii() && in_bmp(c) && DECIMAL_DIGIT.is_match(c.encode_utf8(&mut [0; 4])))
}

/// `char.IsLetter`: a letter (`L`), within the BMP as [`is_digit`] explains. Narrower
/// than `char::is_alphabetic`, which also takes letter numbers and combining marks.
#[must_use]
pub fn is_letter(c: char) -> bool {
    c.is_ascii_alphabetic()
        || (!c.is_ascii() && in_bmp(c) && LETTER.is_match(c.encode_utf8(&mut [0; 4])))
}

/// Whether `c` is one UTF-16 unit, as .NET's `char` is.
fn in_bmp(c: char) -> bool {
    u32::from(c) <= 0xFFFF
}

/// `char.IsLetterOrDigit`: [`is_letter`] or [`is_digit`].
#[must_use]
pub fn is_letter_or_digit(c: char) -> bool {
    is_letter(c) || is_digit(c)
}

/// A .NET call that throws (`ArgumentException`, `FormatException`, …). libse
/// catches it around every call the writers make, so callers only need to know
/// that it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throws;

impl std::fmt::Display for Throws {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the .NET call throws")
    }
}

impl std::error::Error for Throws {}

/// An ARGB colour (`System.Drawing.Color`, reduced to its value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Color {
    /// Alpha, 0 (transparent) to 255 (opaque).
    pub a: u8,
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
}

impl Color {
    /// `Color.FromArgb(int argb)`: the bytes of a packed `0xAARRGGBB` value.
    #[must_use]
    pub fn from_argb(argb: u32) -> Self {
        let [a, r, g, b] = argb.to_be_bytes();
        Self { a, r, g, b }
    }

    /// `Color.FromArgb(int alpha, int red, int green, int blue)`, which throws for a
    /// component outside 0–255.
    ///
    /// # Errors
    ///
    /// Returns `Err(Throws)` for a component outside 0–255.
    pub fn from_components(a: i64, r: i64, g: i64, b: i64) -> Result<Self, Throws> {
        let byte = |v: i64| u8::try_from(v).map_err(|_| Throws);
        Ok(Self {
            a: byte(a)?,
            r: byte(r)?,
            g: byte(g)?,
            b: byte(b)?,
        })
    }

    /// `Color.FromArgb(int alpha, Color baseColor)`.
    ///
    /// # Errors
    ///
    /// Returns `Err(Throws)` for an alpha outside 0–255.
    pub fn with_alpha(self, alpha: i64) -> Result<Self, Throws> {
        Ok(Self {
            a: u8::try_from(alpha).map_err(|_| Throws)?,
            ..self
        })
    }

    /// `Color.ToArgb()`: the packed `0xAARRGGBB` value as a signed `int`.
    #[must_use]
    pub fn to_argb(self) -> i32 {
        i32::from_be_bytes([self.a, self.r, self.g, self.b])
    }

    /// `ColorTranslator.ToWin32(Color)`: `R | G << 8 | B << 16`.
    #[must_use]
    pub fn to_win32(self) -> i32 {
        i32::from(self.r) | i32::from(self.g) << 8 | i32::from(self.b) << 16
    }

    /// `Color.FromName(name)` when it names a known colour (`IsKnownColor`); the
    /// lookup ignores case.
    #[must_use]
    pub fn from_known_name(name: &str) -> Option<Self> {
        KNOWN_COLORS
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|&(_, argb)| Self::from_argb(argb))
    }

    /// `Color.White`.
    pub const WHITE: Self = Self {
        a: 255,
        r: 255,
        g: 255,
        b: 255,
    };

    /// `Color.Black`.
    pub const BLACK: Self = Self {
        a: 255,
        r: 0,
        g: 0,
        b: 0,
    };

    /// `Color.Yellow`.
    pub const YELLOW: Self = Self {
        a: 255,
        r: 255,
        g: 255,
        b: 0,
    };
}

/// `ColorTranslator.FromHtml(string)`.
///
/// `#RRGGBB` and `#RGB`, then `LightGrey`, the HTML system colour names, and last
/// `ColorConverter.ConvertFromString` (a named colour, a quoted name, `0x`/`&h`/`#`
/// hex, or one, three or four comma-separated integers). An empty string is
/// `Color.Empty` (all zero).
///
/// # Errors
///
/// Returns `Err(Throws)` wherever .NET throws.
pub fn color_from_html(html: &str) -> Result<Color, Throws> {
    if html.is_empty() {
        return Ok(Color::default());
    }
    let chars: Vec<char> = html.chars().collect();
    if chars[0] == '#' && (chars.len() == 7 || chars.len() == 4) {
        // `Convert.ToInt32(piece, 16)`, then `Color.FromArgb`'s 0–255 check.
        let hex =
            |s: &str| convert_to_int32_hex(s).and_then(|v| u8::try_from(v).map_err(|_| Throws));
        let piece = |from: usize| chars[from..from + 2].iter().collect::<String>();
        return if chars.len() == 7 {
            Ok(Color {
                a: 255,
                r: hex(&piece(1))?,
                g: hex(&piece(3))?,
                b: hex(&piece(5))?,
            })
        } else {
            let twice = |c: char| hex(&format!("{c}{c}"));
            Ok(Color {
                a: 255,
                r: twice(chars[1])?,
                g: twice(chars[2])?,
                b: twice(chars[3])?,
            })
        };
    }
    if html.eq_ignore_ascii_case("LightGrey") {
        return Color::from_known_name("LightGray").ok_or(Throws);
    }
    let lower = html.to_lowercase();
    if let Some(&(_, known)) = HTML_SYSTEM_COLORS.iter().find(|(name, _)| *name == lower) {
        return Color::from_known_name(known).ok_or(Throws);
    }
    convert_from_string(html)
}

/// `ColorConverterCommon.ConvertFromString(text, CultureInfo.CurrentCulture)` with
/// the `en-US` list separator `,`.
fn convert_from_string(value: &str) -> Result<Color, Throws> {
    let text = value.trim();
    if text.is_empty() {
        return Ok(Color::default());
    }
    if let Some(known) = Color::from_known_name(text) {
        return Ok(known);
    }
    if !text.contains(',') {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() >= 2
            && (chars[0] == '\'' || chars[0] == '"')
            && chars[0] == chars[chars.len() - 1]
        {
            // A quoted name: `Color.FromName`, whose unknown result is all zero.
            let inner: String = chars[1..chars.len() - 1].iter().collect();
            return Ok(Color::from_known_name(&inner).unwrap_or_default());
        }
        let lower = text.to_ascii_lowercase();
        if (chars.len() == 7 && chars[0] == '#')
            || (chars.len() == 8 && (lower.starts_with("0x") || lower.starts_with("&h")))
        {
            #[allow(clippy::cast_sign_loss)] // `(uint)` of the parsed int, as .NET does
            return Ok(Color::from_argb(
                0xFF00_0000 | int_from_string(text)? as u32,
            ));
        }
    }
    let values = text
        .split(',')
        .map(int_from_string)
        .collect::<Result<Vec<_>, _>>()?;
    match values.as_slice() {
        #[allow(clippy::cast_sign_loss)]
        [argb] => Ok(Color::from_argb(*argb as u32)),
        [r, g, b] => Color::from_components(255, (*r).into(), (*g).into(), (*b).into()),
        [a, r, g, b] => Color::from_components((*a).into(), (*r).into(), (*g).into(), (*b).into()),
        _ => Err(Throws),
    }
}

/// `ColorConverterCommon.IntFromString`: `#` and `0x`/`&h` prefixes are hex
/// (`Convert.ToInt32(s, 16)`), anything else is `int.Parse` in the current culture.
fn int_from_string(text: &str) -> Result<i32, Throws> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    if let Some(hex) = text.strip_prefix('#') {
        convert_to_int32_hex(hex)
    } else if lower.starts_with("0x") || lower.starts_with("&h") {
        convert_to_int32_hex(&text[2..])
    } else {
        int_try_parse(text).ok_or(Throws)
    }
}

/// `Convert.ToInt32(value, 16)`: an optional `+`, an optional `0x`, then hex digits
/// whose value fits 32 bits (leading zeros do not count), read as an unsigned value
/// and reinterpreted as `int`. No white space; a `-` throws.
///
/// # Errors
///
/// Returns [`Throws`] for anything else.
pub fn convert_to_int32_hex(value: &str) -> Result<i32, Throws> {
    let unsigned = value.strip_prefix('+').unwrap_or(value);
    let digits = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
        .unwrap_or(unsigned);
    parse_hex_digits(digits).ok_or(Throws)
}

/// `int.TryParse(s, NumberStyles.HexNumber, null, out _)`: optional leading and
/// trailing white space around hex digits (no sign, no `0x`) whose value fits 32
/// bits, read as an unsigned value and reinterpreted as `int`. Like every .NET
/// integer parse, it ignores trailing NUL characters.
#[must_use]
pub fn hex_number_try_parse(s: &str) -> Option<i32> {
    parse_hex_digits(
        s.trim_end_matches('\0')
            .trim_matches(|c: char| matches!(c, '\u{9}'..='\u{d}' | ' ')),
    )
}

/// ASCII hex digits whose value fits 32 bits, reinterpreted as `int`.
fn parse_hex_digits(digits: &str) -> Option<i32> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // .NET counts the value, not the digits: leading zeros are free.
    let significant = digits.trim_start_matches('0');
    if significant.len() > 8 {
        return None;
    }
    let digits = if significant.is_empty() {
        "0"
    } else {
        significant
    };
    #[allow(clippy::cast_possible_wrap)] // the unsigned reinterpretation .NET does
    u32::from_str_radix(digits, 16).ok().map(|v| v as i32)
}

/// `int.TryParse(s, out _)` (`NumberStyles.Integer`, `en-US`): optional leading and
/// trailing white space, an optional sign, then ASCII digits, within `int`'s range.
/// Trailing NUL characters are ignored, as by every .NET integer parse.
#[must_use]
pub fn int_try_parse(s: &str) -> Option<i32> {
    let is_white = |c: char| matches!(c, '\u{9}'..='\u{d}' | ' ');
    let t = s.trim_end_matches('\0').trim_matches(is_white);
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let magnitude: i64 = digits
        .trim_start_matches('0')
        .parse::<i64>()
        .ok()
        .or_else(|| digits.chars().all(|c| c == '0').then_some(0))?;
    i32::try_from(if negative { -magnitude } else { magnitude }).ok()
}

/// `float.TryParse(s, NumberStyles.AllowDecimalPoint, CultureInfo.InvariantCulture)`:
/// ASCII digits with at most one `.` (no sign, exponent or white space). Failing
/// that, .NET's fallback trims white space and takes the invariant symbols in any
/// case, whatever the styles: `Infinity`, `-Infinity`, `NaN`, and `+` or `-` before
/// `NaN`, or `+` before `Infinity`.
#[must_use]
pub fn float_try_parse(s: &str) -> Option<f32> {
    // The number path ignores trailing NUL characters, as every .NET number parse
    // does; the symbol fallback trims white space only, so `NaN\0` fails.
    let digits = s.trim_end_matches('\0');
    if !digits.is_empty()
        && digits != "."
        && digits.matches('.').count() <= 1
        && digits.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    {
        return digits.parse().ok();
    }
    match s.trim().to_lowercase().as_str() {
        "infinity" | "+infinity" => Some(f32::INFINITY),
        "-infinity" => Some(f32::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => Some(f32::NAN),
        _ => None,
    }
}

/// `string.Equals(a, b, StringComparison.OrdinalIgnoreCase)`: each UTF-16 unit
/// compared after its simple upper-case mapping (a mapping to several characters,
/// such as `ß`, leaves the character as it is).
#[must_use]
pub fn eq_ordinal_ignore_case(a: &str, b: &str) -> bool {
    fn upper(c: char) -> char {
        let mut it = c.to_uppercase();
        match (it.next(), it.next()) {
            (Some(u), None) => u,
            _ => c,
        }
    }
    a.chars().map(upper).eq(b.chars().map(upper))
}

/// A .NET `decimal` as libse's styles keep it: its digits and its scale, which
/// `ToString()` preserves (`1.00` stays `1.00`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    /// The value times `10^scale`, as a magnitude.
    mantissa: u128,
    /// The number of fractional digits.
    scale: u32,
    /// The sign.
    negative: bool,
}

impl Decimal {
    /// A whole-number decimal (`20m`).
    #[must_use]
    pub fn from_int(value: i64) -> Self {
        Self {
            mantissa: u128::from(value.unsigned_abs()),
            scale: 0,
            negative: value < 0,
        }
    }

    /// The largest `decimal` mantissa, `2^96 - 1`.
    const MAX_MANTISSA: u128 = (1 << 96) - 1;
    /// The largest `decimal` scale.
    const MAX_SCALE: i64 = 28;

    /// `decimal.TryParse(s, NumberStyles.AllowDecimalPoint[ | AllowLeadingSign],
    /// CultureInfo.InvariantCulture)`: digits with at most one `.`, and a leading sign
    /// only when `allow_sign`.
    ///
    /// The value is built as .NET's `Number.TryNumberBufferToDecimal` builds it: at
    /// most 28 fractional digits and a 96-bit mantissa, the first digit that does not
    /// fit rounding the rest half to even, and an integer part too large to fit
    /// failing the parse.
    #[must_use]
    pub fn try_parse(s: &str, allow_sign: bool) -> Option<Self> {
        let (negative, body) = match s.as_bytes().first() {
            Some(b'-') if allow_sign => (true, &s[1..]),
            Some(b'+') if allow_sign => (false, &s[1..]),
            _ => (false, s),
        };
        let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        if !int_part
            .bytes()
            .chain(frac_part.bytes())
            .all(|b| b.is_ascii_digit())
        {
            return None;
        }

        // `ParseNumber`'s number buffer: the significant digits (leading zeros dropped,
        // trailing zeros kept) and `Scale`, the decimal point's position relative to
        // the first of them.
        let int_digits = int_part.trim_start_matches('0');
        let (digits, mut e): (Vec<u8>, i64) = if int_digits.is_empty() {
            let frac_digits = frac_part.trim_start_matches('0');
            let lead = i64::try_from(frac_part.len() - frac_digits.len()).ok()?;
            (frac_digits.bytes().map(|b| b - b'0').collect(), -lead)
        } else {
            let all = format!("{int_digits}{frac_part}");
            (
                all.bytes().map(|b| b - b'0').collect(),
                i64::try_from(int_digits.len()).ok()?,
            )
        };
        if digits.is_empty() {
            // A zero keeps its fractional digits, up to 28.
            let scale = u32::try_from((-e).clamp(0, Self::MAX_SCALE)).ok()?;
            return Some(Self {
                mantissa: 0,
                scale,
                negative: false,
            });
        }
        if e > 29 {
            return None;
        }

        let mut value: u128 = 0;
        let mut next = digits.iter().copied().peekable();
        let max_div10 = Self::MAX_MANTISSA / 10;
        while (e > 0 || (next.peek().is_some() && e > -Self::MAX_SCALE))
            && (value < max_div10 || (value == max_div10 && next.peek().copied().unwrap_or(0) <= 5))
        {
            value = value * 10 + u128::from(next.next().unwrap_or(0));
            e -= 1;
        }
        if let Some(c) = next.next()
            && c >= 5
        {
            // Half to even: an exact half (only zeros after the 5) of an even value
            // stays.
            let exact_half_of_even = c == 5 && value.is_multiple_of(2) && next.all(|d| d == 0);
            if !exact_half_of_even {
                value += 1;
                if value > Self::MAX_MANTISSA {
                    // The nearest value that fits, one digit shorter.
                    value = max_div10 + 1;
                    e += 1;
                }
            }
        }
        if e > 0 {
            return None;
        }
        if e <= -29 {
            return Some(Self {
                mantissa: 0,
                scale: 28,
                negative: false,
            });
        }
        Some(Self {
            mantissa: value,
            scale: u32::try_from(-e).ok()?,
            negative: negative && value != 0,
        })
    }

    /// `decimal.ToString(CultureInfo.InvariantCulture)`: every stored fractional digit.
    #[must_use]
    pub fn to_invariant_string(self) -> String {
        Self::render(self.negative, self.mantissa, self.scale, self.scale)
    }

    /// A custom format of the form `0.#…#` with `max_fraction` `#`s (`ToString("0.#")`,
    /// `{0:0.##}`): rounded half away from zero, trailing fractional zeros dropped.
    #[must_use]
    pub fn to_hash_format(self, max_fraction: u32) -> String {
        let (mut mantissa, mut scale) = (self.mantissa, self.scale);
        if scale > max_fraction {
            let divisor = 10u128.pow(scale - max_fraction);
            let (quotient, remainder) = (mantissa / divisor, mantissa % divisor);
            mantissa = quotient + u128::from(remainder * 2 >= divisor);
            scale = max_fraction;
        }
        while scale > 0 && mantissa % 10 == 0 {
            mantissa /= 10;
            scale -= 1;
        }
        Self::render(self.negative && mantissa != 0, mantissa, scale, scale)
    }

    fn render(negative: bool, mantissa: u128, scale: u32, shown: u32) -> String {
        let digits = format!(
            "{mantissa:0width$}",
            width = usize::try_from(scale).unwrap_or(0) + 1
        );
        let split = digits.len() - usize::try_from(scale).unwrap_or(0);
        let mut out = String::new();
        if negative {
            out.push('-');
        }
        out.push_str(&digits[..split]);
        if shown > 0 {
            out.push('.');
            out.push_str(&digits[split..]);
        }
        out
    }
}

/// A `double` formatted with a .NET custom format of the form `0.0##`:
/// `min_fraction` required and `max_fraction` total fractional digits.
///
/// .NET first takes the value to 15 significant digits, then rounds that decimal
/// string half away from zero, so `1.0005` prints as `1.001` (Rust's own
/// formatting rounds the binary value and prints `1.000`).
#[must_use]
pub fn format_double_custom(value: f64, min_fraction: usize, max_fraction: usize) -> String {
    // 15 significant digits: `d.dddddddddddddde±x`.
    let sci = format!("{:.14e}", value.abs());
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exponent: i64 = exponent.parse().unwrap_or(0);
    let digits: Vec<u8> = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|b| b - b'0')
        .collect();

    // Lay the digits out around the decimal point.
    let (mut int_digits, mut frac_digits): (Vec<u8>, Vec<u8>) = if exponent >= 0 {
        let int_len = usize::try_from(exponent + 1).unwrap_or(0);
        let int: Vec<u8> = (0..int_len)
            .map(|i| digits.get(i).copied().unwrap_or(0))
            .collect();
        (int, digits.iter().skip(int_len).copied().collect())
    } else {
        let lead = usize::try_from(-exponent - 1).unwrap_or(0);
        (
            vec![0],
            std::iter::repeat_n(0, lead)
                .chain(digits.iter().copied())
                .collect(),
        )
    };

    // Round half away from zero at `max_fraction` digits.
    let round_up = frac_digits.get(max_fraction).is_some_and(|&d| d >= 5);
    frac_digits.resize(max_fraction, 0);
    if round_up {
        let mut carry = true;
        for d in frac_digits
            .iter_mut()
            .rev()
            .chain(int_digits.iter_mut().rev())
        {
            if *d == 9 {
                *d = 0;
            } else {
                *d += 1;
                carry = false;
                break;
            }
        }
        if carry {
            int_digits.insert(0, 1);
        }
    }
    while frac_digits.len() > min_fraction && frac_digits.last() == Some(&0) {
        frac_digits.pop();
    }

    let first_nonzero = int_digits
        .iter()
        .position(|&d| d != 0)
        .unwrap_or(int_digits.len() - 1);
    let is_zero = int_digits.iter().chain(&frac_digits).all(|&d| d == 0);
    let mut out = String::new();
    if value.is_sign_negative() && !is_zero {
        out.push('-');
    }
    out.extend(
        int_digits[first_nonzero..]
            .iter()
            .map(|d| char::from(b'0' + d)),
    );
    if !frac_digits.is_empty() {
        out.push('.');
        out.extend(frac_digits.iter().map(|d| char::from(b'0' + d)));
    }
    out
}

/// `ColorTranslator`'s HTML system-colour names, each mapped to its `KnownColor`.
const HTML_SYSTEM_COLORS: &[(&str, &str)] = &[
    ("activeborder", "ActiveBorder"),
    ("activecaption", "ActiveCaption"),
    ("appworkspace", "AppWorkspace"),
    ("background", "Desktop"),
    ("buttonface", "Control"),
    ("buttonhighlight", "ControlLightLight"),
    ("buttonshadow", "ControlDark"),
    ("buttontext", "ControlText"),
    ("captiontext", "ActiveCaptionText"),
    ("graytext", "GrayText"),
    ("highlight", "Highlight"),
    ("highlighttext", "HighlightText"),
    ("inactiveborder", "InactiveBorder"),
    ("inactivecaption", "InactiveCaption"),
    ("inactivecaptiontext", "InactiveCaptionText"),
    ("infobackground", "Info"),
    ("infotext", "InfoText"),
    ("menu", "Menu"),
    ("menutext", "MenuText"),
    ("scrollbar", "ScrollBar"),
    ("threeddarkshadow", "ControlDarkDark"),
    ("threedface", "Control"),
    ("threedhighlight", "ControlLight"),
    ("threedlightshadow", "ControlLightLight"),
    ("window", "Window"),
    ("windowframe", "WindowFrame"),
    ("windowtext", "WindowText"),
];

/// Every `System.Drawing.KnownColor` and its ARGB value on .NET 10 for Linux, where
/// the system colours have fixed values.
pub const KNOWN_COLORS: &[(&str, u32)] = &[
    ("ActiveBorder", 0xFFD4_D0C8),
    ("ActiveCaption", 0xFF00_54E3),
    ("ActiveCaptionText", 0xFFFF_FFFF),
    ("AppWorkspace", 0xFF80_8080),
    ("Control", 0xFFEC_E9D8),
    ("ControlDark", 0xFFAC_A899),
    ("ControlDarkDark", 0xFF71_6F64),
    ("ControlLight", 0xFFF1_EFE2),
    ("ControlLightLight", 0xFFFF_FFFF),
    ("ControlText", 0xFF00_0000),
    ("Desktop", 0xFF00_4E98),
    ("GrayText", 0xFFAC_A899),
    ("Highlight", 0xFF31_6AC5),
    ("HighlightText", 0xFFFF_FFFF),
    ("HotTrack", 0xFF00_0080),
    ("InactiveBorder", 0xFFD4_D0C8),
    ("InactiveCaption", 0xFF7A_96DF),
    ("InactiveCaptionText", 0xFFD8_E4F8),
    ("Info", 0xFFFF_FFE1),
    ("InfoText", 0xFF00_0000),
    ("Menu", 0xFFFF_FFFF),
    ("MenuText", 0xFF00_0000),
    ("ScrollBar", 0xFFD4_D0C8),
    ("Window", 0xFFFF_FFFF),
    ("WindowFrame", 0xFF00_0000),
    ("WindowText", 0xFF00_0000),
    ("Transparent", 0x00FF_FFFF),
    ("AliceBlue", 0xFFF0_F8FF),
    ("AntiqueWhite", 0xFFFA_EBD7),
    ("Aqua", 0xFF00_FFFF),
    ("Aquamarine", 0xFF7F_FFD4),
    ("Azure", 0xFFF0_FFFF),
    ("Beige", 0xFFF5_F5DC),
    ("Bisque", 0xFFFF_E4C4),
    ("Black", 0xFF00_0000),
    ("BlanchedAlmond", 0xFFFF_EBCD),
    ("Blue", 0xFF00_00FF),
    ("BlueViolet", 0xFF8A_2BE2),
    ("Brown", 0xFFA5_2A2A),
    ("BurlyWood", 0xFFDE_B887),
    ("CadetBlue", 0xFF5F_9EA0),
    ("Chartreuse", 0xFF7F_FF00),
    ("Chocolate", 0xFFD2_691E),
    ("Coral", 0xFFFF_7F50),
    ("CornflowerBlue", 0xFF64_95ED),
    ("Cornsilk", 0xFFFF_F8DC),
    ("Crimson", 0xFFDC_143C),
    ("Cyan", 0xFF00_FFFF),
    ("DarkBlue", 0xFF00_008B),
    ("DarkCyan", 0xFF00_8B8B),
    ("DarkGoldenrod", 0xFFB8_860B),
    ("DarkGray", 0xFFA9_A9A9),
    ("DarkGreen", 0xFF00_6400),
    ("DarkKhaki", 0xFFBD_B76B),
    ("DarkMagenta", 0xFF8B_008B),
    ("DarkOliveGreen", 0xFF55_6B2F),
    ("DarkOrange", 0xFFFF_8C00),
    ("DarkOrchid", 0xFF99_32CC),
    ("DarkRed", 0xFF8B_0000),
    ("DarkSalmon", 0xFFE9_967A),
    ("DarkSeaGreen", 0xFF8F_BC8F),
    ("DarkSlateBlue", 0xFF48_3D8B),
    ("DarkSlateGray", 0xFF2F_4F4F),
    ("DarkTurquoise", 0xFF00_CED1),
    ("DarkViolet", 0xFF94_00D3),
    ("DeepPink", 0xFFFF_1493),
    ("DeepSkyBlue", 0xFF00_BFFF),
    ("DimGray", 0xFF69_6969),
    ("DodgerBlue", 0xFF1E_90FF),
    ("Firebrick", 0xFFB2_2222),
    ("FloralWhite", 0xFFFF_FAF0),
    ("ForestGreen", 0xFF22_8B22),
    ("Fuchsia", 0xFFFF_00FF),
    ("Gainsboro", 0xFFDC_DCDC),
    ("GhostWhite", 0xFFF8_F8FF),
    ("Gold", 0xFFFF_D700),
    ("Goldenrod", 0xFFDA_A520),
    ("Gray", 0xFF80_8080),
    ("Green", 0xFF00_8000),
    ("GreenYellow", 0xFFAD_FF2F),
    ("Honeydew", 0xFFF0_FFF0),
    ("HotPink", 0xFFFF_69B4),
    ("IndianRed", 0xFFCD_5C5C),
    ("Indigo", 0xFF4B_0082),
    ("Ivory", 0xFFFF_FFF0),
    ("Khaki", 0xFFF0_E68C),
    ("Lavender", 0xFFE6_E6FA),
    ("LavenderBlush", 0xFFFF_F0F5),
    ("LawnGreen", 0xFF7C_FC00),
    ("LemonChiffon", 0xFFFF_FACD),
    ("LightBlue", 0xFFAD_D8E6),
    ("LightCoral", 0xFFF0_8080),
    ("LightCyan", 0xFFE0_FFFF),
    ("LightGoldenrodYellow", 0xFFFA_FAD2),
    ("LightGray", 0xFFD3_D3D3),
    ("LightGreen", 0xFF90_EE90),
    ("LightPink", 0xFFFF_B6C1),
    ("LightSalmon", 0xFFFF_A07A),
    ("LightSeaGreen", 0xFF20_B2AA),
    ("LightSkyBlue", 0xFF87_CEFA),
    ("LightSlateGray", 0xFF77_8899),
    ("LightSteelBlue", 0xFFB0_C4DE),
    ("LightYellow", 0xFFFF_FFE0),
    ("Lime", 0xFF00_FF00),
    ("LimeGreen", 0xFF32_CD32),
    ("Linen", 0xFFFA_F0E6),
    ("Magenta", 0xFFFF_00FF),
    ("Maroon", 0xFF80_0000),
    ("MediumAquamarine", 0xFF66_CDAA),
    ("MediumBlue", 0xFF00_00CD),
    ("MediumOrchid", 0xFFBA_55D3),
    ("MediumPurple", 0xFF93_70DB),
    ("MediumSeaGreen", 0xFF3C_B371),
    ("MediumSlateBlue", 0xFF7B_68EE),
    ("MediumSpringGreen", 0xFF00_FA9A),
    ("MediumTurquoise", 0xFF48_D1CC),
    ("MediumVioletRed", 0xFFC7_1585),
    ("MidnightBlue", 0xFF19_1970),
    ("MintCream", 0xFFF5_FFFA),
    ("MistyRose", 0xFFFF_E4E1),
    ("Moccasin", 0xFFFF_E4B5),
    ("NavajoWhite", 0xFFFF_DEAD),
    ("Navy", 0xFF00_0080),
    ("OldLace", 0xFFFD_F5E6),
    ("Olive", 0xFF80_8000),
    ("OliveDrab", 0xFF6B_8E23),
    ("Orange", 0xFFFF_A500),
    ("OrangeRed", 0xFFFF_4500),
    ("Orchid", 0xFFDA_70D6),
    ("PaleGoldenrod", 0xFFEE_E8AA),
    ("PaleGreen", 0xFF98_FB98),
    ("PaleTurquoise", 0xFFAF_EEEE),
    ("PaleVioletRed", 0xFFDB_7093),
    ("PapayaWhip", 0xFFFF_EFD5),
    ("PeachPuff", 0xFFFF_DAB9),
    ("Peru", 0xFFCD_853F),
    ("Pink", 0xFFFF_C0CB),
    ("Plum", 0xFFDD_A0DD),
    ("PowderBlue", 0xFFB0_E0E6),
    ("Purple", 0xFF80_0080),
    ("Red", 0xFFFF_0000),
    ("RosyBrown", 0xFFBC_8F8F),
    ("RoyalBlue", 0xFF41_69E1),
    ("SaddleBrown", 0xFF8B_4513),
    ("Salmon", 0xFFFA_8072),
    ("SandyBrown", 0xFFF4_A460),
    ("SeaGreen", 0xFF2E_8B57),
    ("SeaShell", 0xFFFF_F5EE),
    ("Sienna", 0xFFA0_522D),
    ("Silver", 0xFFC0_C0C0),
    ("SkyBlue", 0xFF87_CEEB),
    ("SlateBlue", 0xFF6A_5ACD),
    ("SlateGray", 0xFF70_8090),
    ("Snow", 0xFFFF_FAFA),
    ("SpringGreen", 0xFF00_FF7F),
    ("SteelBlue", 0xFF46_82B4),
    ("Tan", 0xFFD2_B48C),
    ("Teal", 0xFF00_8080),
    ("Thistle", 0xFFD8_BFD8),
    ("Tomato", 0xFFFF_6347),
    ("Turquoise", 0xFF40_E0D0),
    ("Violet", 0xFFEE_82EE),
    ("Wheat", 0xFFF5_DEB3),
    ("White", 0xFFFF_FFFF),
    ("WhiteSmoke", 0xFFF5_F5F5),
    ("Yellow", 0xFFFF_FF00),
    ("YellowGreen", 0xFF9A_CD32),
    ("ButtonFace", 0xFFF0_F0F0),
    ("ButtonHighlight", 0xFFFF_FFFF),
    ("ButtonShadow", 0xFFA0_A0A0),
    ("GradientActiveCaption", 0xFFB9_D1EA),
    ("GradientInactiveCaption", 0xFFD7_E4F2),
    ("MenuBar", 0xFFF0_F0F0),
    ("MenuHighlight", 0xFF33_99FF),
    ("RebeccaPurple", 0xFF66_3399),
];
