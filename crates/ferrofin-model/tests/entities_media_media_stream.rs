//! Verbatim port of `Jellyfin.Model.Tests/Entities/MediaStreamTests.cs`.
//!
//! The C# expected values are the oracle; assertions are not weakened.

use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use rstest::rstest;

/// Builds a subtitle stream with the common test fields.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn subtitle(
    title: Option<&str>,
    language: Option<&str>,
    localized_language: Option<&str>,
    is_forced: bool,
    is_default: bool,
    is_hearing_impaired: bool,
    is_external: bool,
    codec: Option<&str>,
) -> MediaStream {
    MediaStream {
        stream_type: MediaStreamType::Subtitle,
        title: title.map(ToOwned::to_owned),
        language: language.map(ToOwned::to_owned),
        localized_language: localized_language.map(ToOwned::to_owned),
        is_forced,
        is_default,
        is_hearing_impaired,
        is_external,
        codec: codec.map(ToOwned::to_owned),
        ..Default::default()
    }
}

#[test]
fn display_title_subtitle_english_und_ass() {
    // "English - Und - ASS"
    let s = subtitle(
        Some("English"),
        Some(""),
        None,
        false,
        false,
        false,
        false,
        Some("ASS"),
    );
    assert_eq!(s.display_title().as_deref(), Some("English - Und - ASS"));
}

#[test]
fn display_title_subtitle_english_und() {
    // "English - Und"
    let s = subtitle(
        Some("English"),
        Some(""),
        None,
        false,
        false,
        false,
        false,
        Some(""),
    );
    assert_eq!(s.display_title().as_deref(), Some("English - Und"));
}

#[test]
fn display_title_subtitle_english() {
    // "English"
    let s = subtitle(
        Some("English"),
        Some("EN"),
        None,
        false,
        false,
        false,
        false,
        Some(""),
    );
    assert_eq!(s.display_title().as_deref(), Some("English"));
}

#[test]
fn display_title_subtitle_default_forced_srt() {
    // "English - Default - Forced - SRT"
    let s = subtitle(
        Some("English"),
        Some("EN"),
        None,
        true,
        true,
        false,
        false,
        Some("SRT"),
    );
    assert_eq!(
        s.display_title().as_deref(),
        Some("English - Default - Forced - SRT")
    );
}

#[test]
fn display_title_subtitle_title_en_default_forced_srt_external() {
    // "Title - EN - Default - Forced - SRT - External"
    let s = subtitle(
        Some("Title"),
        Some("EN"),
        None,
        true,
        true,
        false,
        true,
        Some("SRT"),
    );
    assert_eq!(
        s.display_title().as_deref(),
        Some("Title - EN - Default - Forced - SRT - External")
    );
}

#[test]
fn display_title_subtitle_und() {
    // "Und"
    let s = subtitle(None, None, None, false, false, false, false, None);
    assert_eq!(s.display_title().as_deref(), Some("Und"));
}

#[test]
fn display_title_subtitle_title_en_hearing_impaired_default_forced_srt() {
    // "Title - EN - Hearing Impaired - Default - Forced - SRT"
    let s = subtitle(
        Some("Title"),
        Some("EN"),
        None,
        true,
        true,
        true,
        false,
        Some("SRT"),
    );
    assert_eq!(
        s.display_title().as_deref(),
        Some("Title - EN - Hearing Impaired - Default - Forced - SRT")
    );
}

#[test]
fn display_title_audio_title_aac_default_external() {
    // "Title - AAC - Default - External"
    let s = MediaStream {
        stream_type: MediaStreamType::Audio,
        title: Some("Title".to_owned()),
        language: None,
        is_forced: false,
        is_default: true,
        codec: Some("AAC".to_owned()),
        is_external: true,
        ..Default::default()
    };
    assert_eq!(
        s.display_title().as_deref(),
        Some("Title - AAC - Default - External")
    );
}

#[test]
fn display_title_subtitle_localized_language_zh_cn() {
    // "Chinese (Simplified) - SRT" — fixes zh-CN display issue #15935.
    let s = subtitle(
        None,
        Some("zh-CN"),
        Some("Chinese (Simplified)"),
        false,
        false,
        false,
        false,
        Some("SRT"),
    );
    assert_eq!(
        s.display_title().as_deref(),
        Some("Chinese (Simplified) - SRT")
    );
}

#[test]
fn display_title_audio_localized_language_japanese() {
    // "Japanese - AAC - Stereo"
    let s = MediaStream {
        stream_type: MediaStreamType::Audio,
        title: None,
        language: Some("jpn".to_owned()),
        localized_language: Some("Japanese".to_owned()),
        is_forced: false,
        is_default: false,
        codec: Some("AAC".to_owned()),
        channel_layout: Some("stereo".to_owned()),
        ..Default::default()
    };
    assert_eq!(
        s.display_title().as_deref(),
        Some("Japanese - AAC - Stereo")
    );
}

#[test]
fn display_title_subtitle_fallback_to_language() {
    // "Eng - ASS" — fallback to Language when LocalizedLanguage is null.
    let s = subtitle(
        None,
        Some("eng"),
        None,
        false,
        false,
        false,
        false,
        Some("ASS"),
    );
    assert_eq!(s.display_title().as_deref(), Some("Eng - ASS"));
}

#[rstest]
#[case(None, None, false, None)]
#[case(None, Some(0), false, None)]
#[case(Some(0), None, false, None)]
#[case(Some(256), Some(144), false, Some("144p"))]
#[case(Some(256), Some(144), true, Some("144i"))]
#[case(Some(426), Some(240), false, Some("240p"))]
#[case(Some(426), Some(240), true, Some("240i"))]
#[case(Some(640), Some(360), false, Some("360p"))]
#[case(Some(640), Some(360), true, Some("360i"))]
#[case(Some(854), Some(480), false, Some("480p"))]
#[case(Some(854), Some(480), true, Some("480i"))]
#[case(Some(960), Some(540), false, Some("540p"))]
#[case(Some(960), Some(540), true, Some("540i"))]
#[case(Some(1024), Some(576), false, Some("576p"))]
#[case(Some(1024), Some(576), true, Some("576i"))]
#[case(Some(1280), Some(720), false, Some("720p"))]
#[case(Some(1280), Some(720), true, Some("720i"))]
#[case(Some(2560), Some(1080), false, Some("1080p"))]
#[case(Some(2560), Some(1080), true, Some("1080i"))]
#[case(Some(4096), Some(3072), false, Some("4K"))]
#[case(Some(8192), Some(6144), false, Some("8K"))]
#[case(Some(512), Some(384), false, Some("384p"))]
#[case(Some(576), Some(336), false, Some("360p"))]
#[case(Some(576), Some(336), true, Some("360i"))]
#[case(Some(624), Some(352), false, Some("360p"))]
#[case(Some(640), Some(352), false, Some("360p"))]
#[case(Some(640), Some(480), false, Some("480p"))]
#[case(Some(704), Some(396), false, Some("404p"))]
#[case(Some(720), Some(404), false, Some("404p"))]
#[case(Some(720), Some(480), false, Some("480p"))]
#[case(Some(720), Some(576), false, Some("576p"))]
#[case(Some(768), Some(576), false, Some("576p"))]
#[case(Some(960), Some(544), false, Some("540p"))]
#[case(Some(960), Some(544), true, Some("540i"))]
#[case(Some(960), Some(720), false, Some("720p"))]
#[case(Some(1280), Some(528), false, Some("720p"))]
#[case(Some(1280), Some(532), false, Some("720p"))]
#[case(Some(1280), Some(534), false, Some("720p"))]
#[case(Some(1280), Some(536), false, Some("720p"))]
#[case(Some(1280), Some(544), false, Some("720p"))]
#[case(Some(1280), Some(690), false, Some("720p"))]
#[case(Some(1280), Some(694), false, Some("720p"))]
#[case(Some(1280), Some(696), false, Some("720p"))]
#[case(Some(1280), Some(716), false, Some("720p"))]
#[case(Some(1280), Some(718), false, Some("720p"))]
#[case(Some(1920), Some(1080), false, Some("1080p"))]
#[case(Some(1440), Some(1070), false, Some("1080p"))]
#[case(Some(1440), Some(1072), false, Some("1080p"))]
#[case(Some(1440), Some(1080), false, Some("1080p"))]
#[case(Some(1440), Some(1440), false, Some("1080p"))]
#[case(Some(1912), Some(792), false, Some("1080p"))]
#[case(Some(1916), Some(1076), false, Some("1080p"))]
#[case(Some(1918), Some(1080), false, Some("1080p"))]
#[case(Some(1920), Some(796), false, Some("1080p"))]
#[case(Some(1920), Some(800), false, Some("1080p"))]
#[case(Some(1920), Some(802), false, Some("1080p"))]
#[case(Some(1920), Some(804), false, Some("1080p"))]
#[case(Some(1920), Some(808), false, Some("1080p"))]
#[case(Some(1920), Some(816), false, Some("1080p"))]
#[case(Some(1920), Some(856), false, Some("1080p"))]
#[case(Some(1920), Some(960), false, Some("1080p"))]
#[case(Some(1920), Some(1024), false, Some("1080p"))]
#[case(Some(1920), Some(1040), false, Some("1080p"))]
#[case(Some(1920), Some(1070), false, Some("1080p"))]
#[case(Some(1920), Some(1072), false, Some("1080p"))]
#[case(Some(1920), Some(1440), false, Some("1080p"))]
#[case(Some(3840), Some(1600), false, Some("4K"))]
#[case(Some(3840), Some(1606), false, Some("4K"))]
#[case(Some(3840), Some(1608), false, Some("4K"))]
#[case(Some(3840), Some(2160), false, Some("4K"))]
#[case(Some(4090), Some(3070), false, Some("4K"))]
#[case(Some(7680), Some(4320), false, Some("8K"))]
#[case(Some(8190), Some(6140), false, Some("8K"))]
fn get_resolution_text_valid(
    #[case] width: Option<i32>,
    #[case] height: Option<i32>,
    #[case] interlaced: bool,
    #[case] expected: Option<&str>,
) {
    let s = MediaStream {
        width,
        height,
        is_interlaced: interlaced,
        ..Default::default()
    };
    assert_eq!(s.get_resolution_text().as_deref(), expected);
}
