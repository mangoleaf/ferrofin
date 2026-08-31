//! Port of `GuidExtensions.cs` — nil-GUID predicates over `uuid::Uuid`.

use uuid::Uuid;

/// Determines whether the GUID is the default (all-zero / nil) value.
#[must_use]
pub fn is_empty(guid: &Uuid) -> bool {
    guid.is_nil()
}

/// Determines whether the GUID is `None` or the default (nil) value.
#[must_use]
pub fn is_null_or_empty(guid: Option<&Uuid>) -> bool {
    match guid {
        None => true,
        Some(g) => is_empty(g),
    }
}

/// Port of .NET's `Guid.TryParse(string)` — the exact format set ASP.NET Core's
/// `Guid?` model binding accepts for a query parameter, and nothing else.
///
/// `uuid::Uuid::parse_str` is NOT that set: it accepts an `urn:uuid:` prefix
/// .NET rejects, and rejects the parenthesised (`P`) and hex-object (`X`)
/// spellings .NET accepts. Both differences were measured live against a
/// Jellyfin 10.11.8 oracle on `GET /Packages/{name}?assemblyGuid=…`
/// (`(guid)` → 400 here / 200 there; `urn:uuid:guid` → 200 here / 400 there),
/// which is why the guid-bound handlers parse through this instead.
///
/// Faithful to `Guid.TryParse`'s dispatch (`System.Guid.TryParseGuid`):
///
/// ```text
/// guidString = guidString.Trim();          // whitespace at BOTH ends, any form
/// switch (guidString[0])
///     '(' => TryParseExactP                // (dddddddd-dddd-dddd-dddd-dddddddddddd)
///     '{' => contains('-') ? ExactB        // {dddddddd-dddd-…}
///                          : ExactX        // {0xdddddddd,0xdddd,0xdddd,{0xdd,…}}
///     _   => contains('-') ? ExactD        // dddddddd-dddd-dddd-dddd-dddddddddddd
///                          : ExactN        // dddddddddddddddddddddddddddddddd
/// ```
///
/// The `X` arm keeps the two documented compatibility quirks of the C#
/// (both re-measured against the oracle, see the tests): it eats whitespace
/// *anywhere*, not just at the ends; and its components are parsed as 32-bit
/// and only overflow past 8 significant hex digits, so `0x1031b` is an
/// accepted spelling of the `short` component `0x031b`. The eight byte
/// components are the exception — the C# rejects anything above `byte.MaxValue`.
#[must_use]
pub fn parse_dotnet_guid(value: &str) -> Option<Uuid> {
    let trimmed = value.trim();
    match trimmed.as_bytes().first()? {
        b'(' => parse_exact_p(trimmed),
        b'{' => {
            if trimmed.contains('-') {
                parse_exact_b(trimmed)
            } else {
                parse_exact_x(trimmed)
            }
        }
        _ => {
            if trimmed.contains('-') {
                parse_exact_d(trimmed)
            } else {
                parse_exact_n(trimmed)
            }
        }
    }
}

/// `TryParseExactN` — 32 bare hex digits.
fn parse_exact_n(value: &str) -> Option<Uuid> {
    let bytes = value.as_bytes();
    if bytes.len() != 32 {
        return None;
    }
    uuid_from_hex_digits(bytes)
}

/// `TryParseExactD` — 36 characters with hyphens at 8/13/18/23 and nowhere else.
fn parse_exact_d(value: &str) -> Option<Uuid> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || bytes[8] != b'-'
        || bytes[13] != b'-'
        || bytes[18] != b'-'
        || bytes[23] != b'-'
    {
        return None;
    }
    let mut digits = [0_u8; 32];
    let mut written = 0_usize;
    for (index, &byte) in bytes.iter().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            continue;
        }
        digits[written] = byte;
        written += 1;
    }
    uuid_from_hex_digits(&digits)
}

/// `TryParseExactB` — the `D` form wrapped in braces.
fn parse_exact_b(value: &str) -> Option<Uuid> {
    parse_exact_wrapped(value, b'{', b'}')
}

/// `TryParseExactP` — the `D` form wrapped in parentheses.
fn parse_exact_p(value: &str) -> Option<Uuid> {
    parse_exact_wrapped(value, b'(', b')')
}

/// The shared body of `TryParseExactB`/`TryParseExactP`: an exact-length
/// bracket pair around an otherwise unmodified `D`.
fn parse_exact_wrapped(value: &str, open: u8, close: u8) -> Option<Uuid> {
    let bytes = value.as_bytes();
    if bytes.len() != 38 || bytes[0] != open || bytes[37] != close {
        return None;
    }
    parse_exact_d(value.get(1..37)?)
}

/// `TryParseExactX` — `{0xdddddddd,0xdddd,0xdddd,{0xdd,0xdd,0xdd,0xdd,0xdd,0xdd,0xdd,0xdd}}`.
fn parse_exact_x(value: &str) -> Option<Uuid> {
    // "Eat all of the whitespace. Unlike the other forms, X allows for any
    // amount of whitespace anywhere, not just at the beginning and end."
    let compact: Vec<u8> = value
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| {
            let mut buf = [0_u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        })
        .collect();
    if compact.first() != Some(&b'{') {
        return None;
    }
    let mut cursor = 1_usize;
    let a = read_hex_component(&compact, &mut cursor, b',')?;
    // The C# parses the two `short` components as 32 bits and stores the low
    // half, so a value that overflows a `short` (but not an `int`) is accepted.
    let b = u16::try_from(read_hex_component(&compact, &mut cursor, b',')? & 0xFFFF).ok()?;
    let c = u16::try_from(read_hex_component(&compact, &mut cursor, b',')? & 0xFFFF).ok()?;
    if compact.get(cursor) != Some(&b'{') {
        return None;
    }
    cursor += 1;
    let mut tail = [0_u8; 8];
    for (index, slot) in tail.iter_mut().enumerate() {
        let terminator = if index < 7 { b',' } else { b'}' };
        // Unlike the wider components, a byte component above 0xFF is rejected
        // outright (`byteVal > byte.MaxValue`).
        *slot = u8::try_from(read_hex_component(&compact, &mut cursor, terminator)?).ok()?;
    }
    // The closing brace of the whole literal, with nothing after it.
    if compact.get(cursor) != Some(&b'}') || cursor + 1 != compact.len() {
        return None;
    }
    let mut out = [0_u8; 16];
    out[0..4].copy_from_slice(&a.to_be_bytes());
    out[4..6].copy_from_slice(&b.to_be_bytes());
    out[6..8].copy_from_slice(&c.to_be_bytes());
    out[8..16].copy_from_slice(&tail);
    Some(Uuid::from_bytes(out))
}

/// Reads one `0x…` component of the `X` form up to `terminator`, leaving the
/// cursor just past the terminator. Mirrors the `IsHexPrefix` guard plus the
/// `numLen <= 0` rejection of an empty component.
fn read_hex_component(bytes: &[u8], cursor: &mut usize, terminator: u8) -> Option<u32> {
    if bytes.get(*cursor) != Some(&b'0') || (*bytes.get(*cursor + 1)? | 0x20) != b'x' {
        return None;
    }
    let start = *cursor + 2;
    let length = bytes.get(start..)?.iter().position(|&c| c == terminator)?;
    if length == 0 {
        return None;
    }
    let value = parse_hex(bytes.get(start..start + length)?)?;
    *cursor = start + length + 1;
    Some(value)
}

/// Port of `Guid.TryParseHex`: an optional `+`, an optional (second) `0x`,
/// unlimited leading zeros, and at most eight significant hex digits.
fn parse_hex(mut bytes: &[u8]) -> Option<u32> {
    if bytes.first() == Some(&b'+') {
        bytes = bytes.get(1..)?;
    }
    if bytes.len() > 1 && bytes[0] == b'0' && (bytes[1] | 0x20) == b'x' {
        bytes = bytes.get(2..)?;
    }
    let mut index = 0_usize;
    while bytes.get(index) == Some(&b'0') {
        index += 1;
    }
    let mut significant = 0_usize;
    let mut value = 0_u32;
    while let Some(&byte) = bytes.get(index) {
        let digit = char::from(byte).to_digit(16)?;
        value = value.wrapping_mul(16).wrapping_add(digit);
        significant += 1;
        index += 1;
    }
    if significant > 8 {
        return None;
    }
    Some(value)
}

/// Folds exactly 32 hex digits into a UUID, rejecting any non-hex character.
fn uuid_from_hex_digits(digits: &[u8]) -> Option<Uuid> {
    let mut out = [0_u8; 16];
    for (index, pair) in digits.chunks_exact(2).enumerate() {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        *out.get_mut(index)? = u8::try_from((high << 4) | low).ok()?;
    }
    Some(Uuid::from_bytes(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nil_is_empty() {
        assert!(is_empty(&Uuid::nil()));
    }

    #[test]
    fn non_nil_is_not_empty() {
        assert!(!is_empty(&Uuid::from_u128(1)));
    }

    #[test]
    fn null_or_empty() {
        assert!(is_null_or_empty(None));
        assert!(is_null_or_empty(Some(&Uuid::nil())));
        assert!(!is_null_or_empty(Some(&Uuid::from_u128(1))));
    }

    /// Every case below was measured live against a Jellyfin 10.11.8 oracle on
    /// 2026-08-30 (`GET /Packages/Bookshelf?assemblyGuid=…`, guid
    /// `9c4e63f1-031b-4f25-988b-4f7d78a8b53e`): the expectation is the oracle's
    /// status, not a reading of the C# alone.
    const OK: Uuid = Uuid::from_u128(0x9c4e_63f1_031b_4f25_988b_4f7d_78a8_b53e);

    #[test]
    fn accepts_the_n_d_b_p_spellings() {
        for spelling in [
            "9c4e63f1031b4f25988b4f7d78a8b53e",
            "9c4e63f1-031b-4f25-988b-4f7d78a8b53e",
            "9C4E63F1-031B-4F25-988B-4F7D78A8B53E",
            "{9c4e63f1-031b-4f25-988b-4f7d78a8b53e}",
            "(9c4e63f1-031b-4f25-988b-4f7d78a8b53e)",
        ] {
            assert_eq!(parse_dotnet_guid(spelling), Some(OK), "{spelling}");
        }
    }

    #[test]
    fn accepts_the_x_spelling_and_its_compat_quirks() {
        for spelling in [
            // canonical
            "{0x9c4e63f1,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            // components need not be full width, and may carry any leading zeros
            "{0x9c4e63f1,0x31b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            "{0x009c4e63f1,0x0031b,0x4f25,{0x098,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            // the `short` components are parsed as 32 bits and truncated
            "{0x9c4e63f1,0x1031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            "{0x9c4e63f1,0x031b,0x14f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            // `0X` is a hex prefix too, and `TryParseHex` strips a second one
            "{0X9c4e63f1,0X031b,0X4f25,{0X98,0X8b,0X4f,0X7d,0X78,0Xa8,0Xb5,0X3e}}",
            "{0x0x9c4e63f1,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            // whitespace ANYWHERE, including inside a component
            "{0x9c4e63f1, 0x031b, 0x4f25, {0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            "{0x9c4e 63f1,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            "  {  0x9c4e63f1 , 0x031b , 0x4f25 , {  0x98 , 0x8b , 0x4f , 0x7d , 0x78 , \
0xa8 , 0xb5 , 0x3e } }  ",
        ] {
            assert_eq!(parse_dotnet_guid(spelling), Some(OK), "{spelling}");
        }
    }

    #[test]
    fn rejects_the_x_spellings_the_oracle_rejects() {
        for spelling in [
            // more than eight significant digits overflows the 32-bit read
            "{0x19c4e63f1,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            // a byte component above 0xFF is rejected outright
            "{0x9c4e63f1,0x031b,0x4f25,{0x198,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            // the `0x` prefix is structural, not optional
            "{9c4e63f1,031b,4f25,{98,8b,4f,7d,78,a8,b5,3e}}",
            // a leading `+` sits where `IsHexPrefix` demands the `0`
            "{+0x9c4e63f1,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            // an empty component (`numLen <= 0`)
            "{0x,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
            // seven or nine byte components
            "{0x9c4e63f1,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5}}",
            "{0x9c4e63f1,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e,0x11}}",
            // anything after the closing brace
            "{0x9c4e63f1,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}x",
            "{0x9c4e63f1,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e},}",
            // truncated before the byte group
            "{0x9c4e63f1,0x031b,0x4f25,",
            "{0x9c4e63f1,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}",
            "{",
        ] {
            assert_eq!(parse_dotnet_guid(spelling), None, "{spelling}");
        }
    }

    #[test]
    fn trims_whitespace_at_both_ends_but_never_inside() {
        for spelling in [
            "  9c4e63f1031b4f25988b4f7d78a8b53e  ",
            "\t9c4e63f1-031b-4f25-988b-4f7d78a8b53e\n",
            "\u{000c}9c4e63f1-031b-4f25-988b-4f7d78a8b53e\u{000b}",
        ] {
            assert_eq!(parse_dotnet_guid(spelling), Some(OK), "{spelling:?}");
        }
        // Only the X form eats interior whitespace.
        assert_eq!(
            parse_dotnet_guid("9c4e63f1-031b-4f25-988b-4f7d 78a8b53e"),
            None
        );
        assert_eq!(
            parse_dotnet_guid("( 9c4e63f1-031b-4f25-988b-4f7d78a8b53e )"),
            None
        );
    }

    #[test]
    fn rejects_what_dotnet_rejects_and_uuid_parse_str_accepts() {
        // The whole point of the port: `Uuid::parse_str` takes the URN form.
        assert!(Uuid::parse_str("urn:uuid:9c4e63f1-031b-4f25-988b-4f7d78a8b53e").is_ok());
        assert_eq!(
            parse_dotnet_guid("urn:uuid:9c4e63f1-031b-4f25-988b-4f7d78a8b53e"),
            None
        );
        assert_eq!(
            parse_dotnet_guid("urn:uuid:9c4e63f1031b4f25988b4f7d78a8b53e"),
            None
        );
    }

    #[test]
    fn rejects_malformed_n_d_b_p() {
        for spelling in [
            "",
            "   ",
            "notaguid",
            "9c4e63f1031b4f25988b4f7d78a8b53",
            "9c4e63f1031b4f25988b4f7d78a8b53ea",
            "+9c4e63f1031b4f25988b4f7d78a8b53e",
            "9c4e63f1031b4f25988b4f7d78a8b53g",
            "9c4e63f10-31b-4f25-988b-4f7d78a8b53e",
            "9c4e63f1-031b-4f25-988b-4f7d78a8b53e-",
            "9c4e63f1-031b-4f25-988b-4f7d78a8b53g",
            // `{…}` with no hyphen takes the X arm, so the braced N form fails
            "{9c4e63f1031b4f25988b4f7d78a8b53e}",
            "(9c4e63f1031b4f25988b4f7d78a8b53e)",
            "(9c4e63f1-031b-4f25-988b-4f7d78a8b53e}",
            "{{9c4e63f1-031b-4f25-988b-4f7d78a8b53e}}",
        ] {
            assert_eq!(parse_dotnet_guid(spelling), None, "{spelling:?}");
        }
    }

    #[test]
    fn the_nil_guid_parses_in_every_form() {
        for spelling in [
            "00000000000000000000000000000000",
            "00000000-0000-0000-0000-000000000000",
            "{00000000-0000-0000-0000-000000000000}",
            "(00000000-0000-0000-0000-000000000000)",
            "{0x0,0x0,0x0,{0x0,0x0,0x0,0x0,0x0,0x0,0x0,0x0}}",
        ] {
            assert_eq!(parse_dotnet_guid(spelling), Some(Uuid::nil()), "{spelling}");
        }
    }
}
