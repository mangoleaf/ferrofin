//! The .NET and libse helpers the subtitle writers are built on.
//!
//! Every expected value here was printed by .NET 10 on Linux (the runtime Jellyfin
//! 12.2's image uses) or by libse 4.0.12 for the same input.

use ferrofin_mediaencoding::subtitles::dotnet::{
    Color, Decimal, color_from_html, convert_to_int32_hex, eq_ordinal_ignore_case, float_try_parse,
    format_double_custom, hex_number_try_parse, int_try_parse, is_digit, is_letter,
    is_letter_or_digit,
};
use ferrofin_mediaencoding::subtitles::libse_text::{
    html_color_from_string, remove_control_characters_but_white_space,
    remove_recursive_line_breaks, remove_ssa_tags, split_to_lines,
};
use ferrofin_mediaencoding::subtitles::ssa_style::{
    ssa_color, ssa_style_from_header, styles_from_header,
};
use rstest::rstest;

fn argb(c: Color) -> u32 {
    u32::from_be_bytes([c.a, c.r, c.g, c.b])
}

#[rstest]
#[case("#ff0000", Some(0xFFFF_0000))]
#[case("#f00", Some(0xFFFF_0000))]
#[case::eight_digits_are_argb("#ff0000ff", Some(0xFF00_00FF))]
#[case::one_digit_is_an_int("#1", Some(0x0000_0001))]
#[case("#12345", Some(0x0001_2345))]
#[case("#", None)]
#[case("#1234567890", None)]
#[case("red", Some(0xFFFF_0000))]
#[case("Red", Some(0xFFFF_0000))]
#[case("LightGrey", Some(0xFFD3_D3D3))]
#[case("#ABCDEF", Some(0xFFAB_CDEF))]
#[case("notacolor", None)]
#[case("0x00ff00", Some(0xFF00_FF00))]
#[case("&h00ff00", Some(0xFF00_FF00))]
#[case("12,34,56", Some(0xFF0C_2238))]
#[case("255", Some(0x0000_00FF))]
#[case("'red'", Some(0xFFFF_0000))]
#[case("transparent", Some(0x00FF_FFFF))]
#[case("Control", Some(0xFFEC_E9D8))]
#[case::html_system_colour("buttonface", Some(0xFFEC_E9D8))]
fn color_translator_from_html(#[case] html: &str, #[case] expected: Option<u32>) {
    assert_eq!(color_from_html(html).ok().map(argb), expected);
}

#[test]
fn color_from_name_is_case_insensitive_and_known_only() {
    assert_eq!(
        Color::from_known_name("LIGHTBLUE").map(argb),
        Some(0xFFAD_D8E6)
    );
    assert_eq!(Color::from_known_name("nope"), None);
}

#[test]
fn color_packings() {
    let c = Color {
        a: 255,
        r: 0xFC,
        g: 0xFC,
        b: 0xB4,
    };
    assert_eq!(c.to_win32(), 0x00B4_FCFC);
    assert_eq!(Color::WHITE.to_argb(), -1);
}

#[rstest]
#[case("1", "1", "1", "1")]
#[case("1.00", "1.00", "1", "1")]
#[case("0.5", "0.5", "0.5", "0.5")]
#[case("28.75", "28.75", "28.8", "28.75")]
#[case("1.250", "1.250", "1.3", "1.25")]
#[case(".5", "0.5", "0.5", "0.5")]
#[case("5.", "5", "5", "5")]
#[case("00.10", "0.10", "0.1", "0.1")]
fn decimal_keeps_its_scale_and_rounds_half_away(
    #[case] text: &str,
    #[case] invariant: &str,
    #[case] one_place: &str,
    #[case] two_places: &str,
) {
    let d = Decimal::try_parse(text, false).unwrap();
    assert_eq!(d.to_invariant_string(), invariant);
    assert_eq!(d.to_hash_format(1), one_place);
    assert_eq!(d.to_hash_format(2), two_places);
}

#[rstest]
#[case::rounds_to_28_places(
    "0.00000000000000000000000000000000000001",
    Some("0.0000000000000000000000000000")
)]
#[case::below_half_rounds_down(
    "0.000000000000000000000000000045",
    Some("0.0000000000000000000000000000")
)]
#[case::half_of_even_stays(
    "0.000000000000000000000000000050",
    Some("0.0000000000000000000000000000")
)]
#[case::half_of_odd_rounds_up(
    "0.000000000000000000000000000150",
    Some("0.0000000000000000000000000002")
)]
#[case::half_of_even_stays_two(
    "0.000000000000000000000000000250",
    Some("0.0000000000000000000000000002")
)]
#[case::above_half_rounds_up(
    "0.000000000000000000000000000051",
    Some("0.0000000000000000000000000001")
)]
#[case::twenty_nine_digits_fit(
    "1.2345678901234567890123456789",
    Some("1.2345678901234567890123456789")
)]
#[case::too_wide_drops_a_place(
    "9.9999999999999999999999999999",
    Some("10.000000000000000000000000000")
)]
#[case::half_rounds_to_even_up(
    "1.23456789012345678901234567895",
    Some("1.2345678901234567890123456790")
)]
#[case::half_rounds_to_even_down(
    "1.23456789012345678901234567885",
    Some("1.2345678901234567890123456788")
)]
#[case::max_value("79228162514264337593543950335", Some("79228162514264337593543950335"))]
#[case::above_max("79228162514264337593543950336", None)]
#[case::max_value_rounded_down(
    "79228162514264337593543950335.4",
    Some("79228162514264337593543950335")
)]
#[case::max_value_rounded_up_overflows("79228162514264337593543950335.5", None)]
#[case::rounding_overflow_shortens(
    "7922816251426433759354395033.55",
    Some("7922816251426433759354395034")
)]
#[case::integer_too_large("123456789012345678901234567890.5", None)]
#[case::leading_zeros("0000000000000000000000000000000000000001.5", Some("1.5"))]
#[case::trailing_zeros_capped(
    "1.0000000000000000000000000000000000000000",
    Some("1.0000000000000000000000000000")
)]
#[case::zero_keeps_its_places("0.000", Some("0.000"))]
fn decimal_parses_like_dotnet(#[case] text: &str, #[case] expected: Option<&str>) {
    assert_eq!(
        Decimal::try_parse(text, false)
            .map(Decimal::to_invariant_string)
            .as_deref(),
        expected
    );
}

#[rstest]
#[case("NaN", true)]
#[case("nan", true)]
#[case("Infinity", true)]
#[case("infinity", true)]
#[case("-Infinity", true)]
#[case("+Infinity", true)]
#[case("1.", true)]
#[case("30", true)]
#[case("∞", false)]
#[case("1e5", false)]
#[case(" 1", false)]
#[case(".", false)]
#[case("-1", false)]
#[case(" NaN", true)]
#[case("NaN ", true)]
#[case("\tNaN", true)]
#[case("+NaN", true)]
#[case("-NaN", true)]
#[case(" Infinity", true)]
#[case("- Infinity", false)]
#[case("+ NaN", false)]
#[case("1\0", true)]
#[case::symbols_keep_their_nul("NaN\0", false)]
#[case("Infinity\0", false)]
#[case(" NaN\0", false)]
fn float_try_parse_matches_dotnet(#[case] text: &str, #[case] parses: bool) {
    assert_eq!(float_try_parse(text).is_some(), parses);
}

#[test]
fn decimal_takes_a_sign_only_when_allowed() {
    assert_eq!(Decimal::try_parse("-12.5", false), None);
    assert_eq!(
        Decimal::try_parse("-12.5", true)
            .unwrap()
            .to_invariant_string(),
        "-12.5"
    );
    assert_eq!(Decimal::try_parse("1,5", false), None);
}

#[rstest]
#[case("ff", Ok(255), Some(255))]
#[case("+f", Ok(15), None)]
#[case("-f", Err(()), None)]
#[case(" f", Err(()), Some(15))]
#[case("f ", Err(()), Some(15))]
#[case("0x1f", Ok(31), None)]
#[case("0X1F", Ok(31), None)]
#[case("+0x1f", Ok(31), None)]
#[case("0x", Err(()), None)]
#[case("x1", Err(()), None)]
#[case("ffffffff", Ok(-1), Some(-1))]
#[case("fffffffff", Err(()), None)]
#[case("", Err(()), None)]
#[case("+", Err(()), None)]
#[case("日0", Err(()), None)]
#[case::leading_zeros_are_free("0000000041", Ok(65), Some(65))]
#[case("000000000ff", Ok(255), Some(255))]
#[case("00000000000", Ok(0), Some(0))]
#[case::trailing_nul("ff\0", Err(()), Some(255))]
#[case("ff\0\0", Err(()), Some(255))]
#[case("ff\0 ", Err(()), None)]
fn hex_parsing_matches_dotnet(
    #[case] text: &str,
    #[case] convert: Result<i32, ()>,
    #[case] hex_number: Option<i32>,
) {
    assert_eq!(
        convert_to_int32_hex(text).map_err(|_| ()),
        convert,
        "Convert.ToInt32({text:?}, 16)"
    );
    assert_eq!(
        hex_number_try_parse(text),
        hex_number,
        "int.TryParse({text:?}, HexNumber)"
    );
}

#[rstest]
#[case(" 12", Some(12))]
#[case("12 ", Some(12))]
#[case("+12", Some(12))]
#[case("-0", Some(0))]
#[case("0012", Some(12))]
#[case("1_0", None)]
#[case("١٢", None)]
#[case("2147483648", None)]
#[case("-2147483648", Some(i32::MIN))]
#[case("", None)]
#[case("12\0", Some(12))]
#[case("12 \0", Some(12))]
#[case("12\0 ", None)]
fn int_try_parse_matches_dotnet(#[case] text: &str, #[case] expected: Option<i32>) {
    assert_eq!(int_try_parse(text), expected);
}

#[rstest]
#[case::rounds_the_decimal_digits_not_the_binary_value(1.0005, "1.001")]
#[case(0.998_765_5, "0.999")]
#[case(1.234_567_8, "1.235")]
#[case(0.001, "0.001")]
#[case(0.0, "0.0")]
#[case(0.000_001_5, "0.0")]
#[case(1006.465, "1006.465")]
#[case(1011.47, "1011.47")]
#[case(9.999_6, "10.0")]
fn format_double_custom_is_dotnets_0_0_hash_hash(#[case] value: f64, #[case] expected: &str) {
    assert_eq!(format_double_custom(value, 1, 3), expected);
}

#[test]
fn ordinal_ignore_case() {
    assert!(eq_ordinal_ignore_case("Default", "DEFAULT"));
    assert!(eq_ordinal_ignore_case("Ärger", "ärger"));
    assert!(!eq_ordinal_ignore_case("Default", "Defaults"));
}

#[test]
fn split_to_lines_splits_like_libse() {
    assert_eq!(
        split_to_lines("a\r\nb\rc\nd\u{2028}e"),
        vec!["a", "b", "c", "d", "e"]
    );
    assert_eq!(split_to_lines("a\n"), vec!["a", ""]);
    assert_eq!(split_to_lines(""), vec![""]);
}

#[rstest]
#[case("{\\an8}{\\pos(1,2)}text", "text")]
#[case::later_braces_go_too("{\\b1}a {plain} b", "a  b")]
#[case("a\\Nb\\nc\\hd", "a\nb\nc d")]
#[case("{Kara Effector x}{\\k1}sing", "sing")]
#[case::unclosed("{\\b1 open", "{\\b1 open")]
fn remove_ssa_tags_like_libse(#[case] input: &str, #[case] expected: &str) {
    assert_eq!(remove_ssa_tags(input), expected);
}

#[test]
fn remove_recursive_line_breaks_collapses_runs() {
    assert_eq!(remove_recursive_line_breaks("a\n\n\nb\nc"), "a\nb\nc");
    assert_eq!(remove_recursive_line_breaks("a\r\n\r\nb"), "a\r\nb");
}

#[test]
fn remove_control_characters_keeps_white_space() {
    assert_eq!(
        remove_control_characters_but_white_space("a\u{1}\tb\r\n\u{7f}c\u{85}"),
        "a\tb\r\nc"
    );
}

#[rstest]
#[case("rgb(255,0,0)", 0xFFFF_0000)]
#[case("rgba(0, 0, 255, 0.5)", 0x7F00_00FF)]
#[case("#ffffff80", 0x80FF_FFFF)]
#[case("#00ff00ff", 0xFF00_FF00)]
#[case("nope", 0xFFFF_FFFF)]
#[case("rgb(1,2)", 0xFFFF_FFFF)]
fn html_get_color_from_string(#[case] text: &str, #[case] expected: u32) {
    assert_eq!(argb(html_color_from_string(text)), expected);
}

#[rstest]
#[case::ass_bgr("&H0000FF&", 0xFFFF_0000)]
#[case::ass_alpha_inverted("&H80FF0000", 0x7F00_00FF)]
#[case::short_padded("&HFF", 0xFFFF_0000)]
#[case::ssa_decimal_bgr("65535", 0xFFFF_FF00)]
#[case::default_otherwise("bogus", 0xFF00_0000)]
// libse's `Substring` counts characters: the `日` lands in the alpha digits, which do
// not parse, so the alpha is 255 and the colour still reads.
#[case::multibyte_character_in_the_alpha("&H日0011223&", 0xFF23_1201)]
#[case::multibyte_character_in_the_colour("&H00日11223&", 0xFF00_0000)]
// The alpha digits go through `int.TryParse(HexNumber)` (white space allowed, no sign),
// the colour pieces through `Convert.ToInt32(.., 16)` (a `+` allowed).
#[case::white_space_in_the_alpha("&H 5FF0000&", 0xFA00_00FF)]
#[case::plus_in_the_alpha("&H+5FF0000&", 0xFF00_00FF)]
#[case::plus_in_a_colour_piece("&H+F0000&", 0xFF00_000F)]
fn ssa_color_like_libse(#[case] text: &str, #[case] expected: u32) {
    assert_eq!(argb(ssa_color(text, Color::BLACK)), expected);
}

#[test]
fn ssa_style_from_header_reads_columns_by_the_format_line() {
    let header = "[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, Bold, Outline, Alignment, MarginL\n\
                  Style: Sign,Arial Black,36.75,&H0000FFFF,-1,2.50,8,12\nStyle: Other,X,1,&H0,0,0,2,0\n";
    assert_eq!(styles_from_header(header), vec!["Sign", "Other"]);
    let style = ssa_style_from_header("sign", header);
    assert_eq!(style.name, "Sign");
    assert_eq!(style.font_name, "Arial Black");
    assert_eq!(style.font_size.to_invariant_string(), "36.75");
    assert_eq!(argb(style.primary), 0xFFFF_FF00);
    assert!(style.bold);
    assert_eq!(style.outline_width.to_invariant_string(), "2.50");
    assert_eq!(style.alignment, "8");
    assert_eq!(style.margin_left, 12);
    // A style the header lacks is libse's default style under that name.
    let missing = ssa_style_from_header("Missing", header);
    assert_eq!(
        (missing.name.as_str(), missing.font_name.as_str()),
        ("Missing", "Arial")
    );
}

#[rstest]
#[case('5', true, false)]
#[case('\u{663}', true, false)] // Arabic-Indic three, a decimal digit
#[case('²', false, false)]
#[case('½', false, false)]
#[case('Ⅷ', false, false)]
#[case('ⅷ', false, false)]
#[case('é', false, true)]
#[case('中', false, true)]
#[case('ª', false, true)]
#[case('\u{345}', false, false)] // a combining mark
#[case('_', false, false)]
#[case::beyond_the_bmp_is_two_surrogate_units('𝐀', false, false)]
fn character_classes_match_dotnet(#[case] c: char, #[case] digit: bool, #[case] letter: bool) {
    assert_eq!(is_digit(c), digit, "char.IsDigit({c:?})");
    assert_eq!(is_letter(c), letter, "char.IsLetter({c:?})");
    assert_eq!(
        is_letter_or_digit(c),
        digit || letter,
        "char.IsLetterOrDigit({c:?})"
    );
}
