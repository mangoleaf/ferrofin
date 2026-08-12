//! Ported from `ExternalFiles/ExternalPathParserTests.cs`.

use ferrofin_model::dlna::DlnaProfileType;
use ferrofin_model::globalization::CultureDto;
use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::external_files::{ExternalPathParser, LocalizationManager};
use rstest::rstest;

/// Mock localization manager mirroring the Moq setup: `en.*`→English,
/// `fr.*`→French, `hi.*`→Hindi (case-insensitive prefix), else `None`.
struct MockLocalization;

fn culture(name: &str, three: &[&str]) -> CultureDto {
    CultureDto {
        name: name.to_string(),
        display_name: name.to_string(),
        two_letter_iso_language_name: name.to_string(),
        three_letter_iso_language_name: three.first().map(|s| (*s).to_string()),
        three_letter_iso_language_names: three.iter().map(|s| (*s).to_string()).collect(),
    }
}

impl LocalizationManager for MockLocalization {
    fn find_language_info(&self, language: &str) -> Option<CultureDto> {
        let lower = language.to_ascii_lowercase();
        if lower.starts_with("en") {
            Some(culture("English", &["eng"]))
        } else if lower.starts_with("fr") {
            Some(culture("French", &["fre", "fra"]))
        } else if lower.starts_with("hi") {
            Some(culture("Hindi", &["hin"]))
        } else {
            None
        }
    }
}

fn audio_parser<'a>(
    options: &'a NamingOptions,
    loc: &'a MockLocalization,
) -> ExternalPathParser<'a, MockLocalization> {
    ExternalPathParser::new(options, loc, DlnaProfileType::Audio)
}

fn subtitle_parser<'a>(
    options: &'a NamingOptions,
    loc: &'a MockLocalization,
) -> ExternalPathParser<'a, MockLocalization> {
    ExternalPathParser::new(options, loc, DlnaProfileType::Subtitle)
}

#[rstest]
#[case("")]
#[case("MyVideo.ass")]
#[case("MyVideo.mks")]
#[case("MyVideo.sami")]
#[case("MyVideo.srt")]
#[case("MyVideo.m4v")]
fn parse_file_audio_extensions_not_matched_returns_null(#[case] path: &str) {
    let options = NamingOptions::new();
    let loc = MockLocalization;
    assert!(
        audio_parser(&options, &loc)
            .parse_file(path, Some(""))
            .is_none()
    );
}

#[rstest]
#[case("MyVideo.aa")]
#[case("MyVideo.aac")]
#[case("MyVideo.flac")]
#[case("MyVideo.m4a")]
#[case("MyVideo.mka")]
#[case("MyVideo.mp3")]
fn parse_file_audio_extensions_matched_returns_path(#[case] path: &str) {
    let options = NamingOptions::new();
    let loc = MockLocalization;
    let actual = audio_parser(&options, &loc).parse_file(path, Some(""));
    let actual = actual.expect("should parse");
    assert_eq!(actual.path, path);
}

#[rstest]
#[case("")]
#[case("MyVideo.aa")]
#[case("MyVideo.aac")]
#[case("MyVideo.flac")]
#[case("MyVideo.mka")]
#[case("MyVideo.m4v")]
fn parse_file_subtitle_extensions_not_matched_returns_null(#[case] path: &str) {
    let options = NamingOptions::new();
    let loc = MockLocalization;
    assert!(
        subtitle_parser(&options, &loc)
            .parse_file(path, Some(""))
            .is_none()
    );
}

#[rstest]
#[case("MyVideo.ass")]
#[case("MyVideo.mks")]
#[case("MyVideo.sami")]
#[case("MyVideo.srt")]
#[case("MyVideo.vtt")]
fn parse_file_subtitle_extensions_matched_returns_path(#[case] path: &str) {
    let options = NamingOptions::new();
    let loc = MockLocalization;
    let actual = subtitle_parser(&options, &loc).parse_file(path, Some(""));
    let actual = actual.expect("should parse");
    assert_eq!(actual.path, path);
}

#[allow(clippy::too_many_arguments)]
#[rstest]
#[case("", None, None, false, false, false)]
#[case(".default", None, None, true, false, false)]
#[case(".forced", None, None, false, true, false)]
#[case(".foreign", None, None, false, true, false)]
#[case(".default.forced", None, None, true, true, false)]
#[case(".forced.default", None, None, true, true, false)]
#[case(".DEFAULT.FORCED", None, None, true, true, false)]
#[case(".en", None, Some("eng"), false, false, false)]
#[case(".EN", None, Some("eng"), false, false, false)]
#[case(".hi", None, Some("hin"), false, false, false)]
#[case(".fr.en", Some("fr"), Some("eng"), false, false, false)]
#[case(".en.fr", Some("en"), Some("fre"), false, false, false)]
#[case(".title.en.fr", Some("title.en"), Some("fre"), false, false, false)]
#[case(".Title Goes Here", Some("Title Goes Here"), None, false, false, false)]
#[case(
    ".Title.with.Separator",
    Some("Title.with.Separator"),
    None,
    false,
    false,
    false
)]
#[case(
    ".title.en.default.forced",
    Some("title"),
    Some("eng"),
    true,
    true,
    false
)]
#[case(
    ".forced.default.en.title",
    Some("title"),
    Some("eng"),
    true,
    true,
    false
)]
#[case(".sdh.en.title", Some("title"), Some("eng"), false, false, true)]
#[case(".en.cc.title", Some("title"), Some("eng"), false, false, true)]
#[case(".hi.en.title", Some("title"), Some("eng"), false, false, true)]
#[case(".en.hi.title", Some("title"), Some("eng"), false, false, true)]
#[case(
    ".Subs for Chinese Audio.eng",
    Some("Subs for Chinese Audio"),
    Some("eng"),
    false,
    false,
    false
)]
fn parse_file_extra_tokens_parse_to_values(
    #[case] tokens: &str,
    #[case] title: Option<&str>,
    #[case] language: Option<&str>,
    #[case] is_default: bool,
    #[case] is_forced: bool,
    #[case] is_hearing_impaired: bool,
) {
    let options = NamingOptions::new();
    let loc = MockLocalization;
    let path = format!("My.Video{tokens}.srt");

    let actual = subtitle_parser(&options, &loc).parse_file(&path, Some(tokens));

    let actual = actual.expect("should parse");
    assert_eq!(actual.title.as_deref(), title);
    assert_eq!(actual.language.as_deref(), language);
    assert_eq!(actual.is_default, is_default);
    assert_eq!(actual.is_forced, is_forced);
    assert_eq!(actual.is_hearing_impaired, is_hearing_impaired);
}
