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

// ---------------------------------------------------------------------------
// Jellyfin 12.x `IsOriginal` / `LocalizedOriginal`. Upstream `MediaStreamTests`
// has no `[InlineData]` for them, so these cases are derived from the v12.2
// `DisplayTitle` getter (`MediaStream.cs:271-331`, audio; `:358-373`, video).
// ---------------------------------------------------------------------------

/// Builds an audio stream for the `IsOriginal` `DisplayTitle` cases.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn audio(
    title: Option<&str>,
    language: Option<&str>,
    localized_language: Option<&str>,
    codec: Option<&str>,
    channel_layout: Option<&str>,
    is_default: bool,
    is_external: bool,
    is_original: bool,
    localized_original: Option<&str>,
) -> MediaStream {
    MediaStream {
        stream_type: MediaStreamType::Audio,
        title: title.map(ToOwned::to_owned),
        language: language.map(ToOwned::to_owned),
        localized_language: localized_language.map(ToOwned::to_owned),
        codec: codec.map(ToOwned::to_owned),
        channel_layout: channel_layout.map(ToOwned::to_owned),
        is_default,
        is_external,
        is_original,
        localized_original: localized_original.map(ToOwned::to_owned),
        ..Default::default()
    }
}

// MediaStream.cs:310-313: `Original` follows `External`, and an empty
// `LocalizedOriginal` falls back to "Original" (`string.IsNullOrEmpty`).
#[rstest]
#[case(
    audio(
        None,
        Some("eng"),
        Some("English"),
        Some("aac"),
        Some("stereo"),
        false,
        false,
        true,
        None
    ),
    "English - AAC - Stereo - Original"
)]
#[case(
    audio(Some("Title"), None, None, Some("AAC"), None, true, true, true, None),
    "Title - AAC - Default - External - Original"
)]
#[case(
    audio(
        None,
        Some("eng"),
        Some("English"),
        Some("aac"),
        Some("stereo"),
        false,
        false,
        true,
        Some("Originál")
    ),
    "English - AAC - Stereo - Originál"
)]
#[case(
    audio(
        None,
        Some("eng"),
        Some("English"),
        Some("aac"),
        Some("stereo"),
        false,
        false,
        true,
        Some("")
    ),
    "English - AAC - Stereo - Original"
)]
#[case(
    audio(
        Some("Original Mix"),
        None,
        None,
        Some("aac"),
        None,
        false,
        false,
        true,
        None
    ),
    "Original Mix - AAC"
)]
#[case(
    audio(
        None,
        Some("eng"),
        Some("English"),
        Some("aac"),
        Some("stereo"),
        false,
        false,
        false,
        Some("Original")
    ),
    "English - AAC - Stereo"
)]
fn display_title_audio_is_original(#[case] stream: MediaStream, #[case] expected: &str) {
    assert_eq!(stream.display_title().as_deref(), Some(expected));
}

// The subtitle arm (MediaStream.cs:376-431) never reads `IsOriginal`.
#[test]
fn display_title_subtitle_ignores_is_original() {
    let mut s = subtitle(
        None,
        Some("eng"),
        None,
        false,
        false,
        false,
        false,
        Some("ASS"),
    );
    s.is_original = true;
    s.localized_original = Some("Original".to_owned());
    assert_eq!(s.display_title().as_deref(), Some("Eng - ASS"));
}

// MediaStream.cs:358-373: with a Title the missing tags are appended with
// " - " (:366); without one they are joined with a single space (:373).
#[rstest]
#[case(Some("Main"), "Main - 1080p - H264 - SDR")]
#[case(None, "1080p H264 SDR")]
#[case(Some("Main 1080p"), "Main 1080p - H264 - SDR")]
fn display_title_video_separators(#[case] title: Option<&str>, #[case] expected: &str) {
    let s = MediaStream {
        stream_type: MediaStreamType::Video,
        title: title.map(ToOwned::to_owned),
        codec: Some("h264".to_owned()),
        width: Some(1920),
        height: Some(1080),
        ..Default::default()
    };
    assert_eq!(s.display_title().as_deref(), Some(expected));
}

// JsonDefaults `WhenWritingNull`: the non-nullable `IsOriginal` is always
// written; null `LocalizedLanguage`/`LocalizedOriginal` are omitted (12.1 spec:
// `boolean` vs `string, nullable`).
#[test]
fn is_original_and_localized_labels_serialize_like_jellyfin_12() {
    let audio = audio(
        None,
        Some("eng"),
        Some("English"),
        Some("aac"),
        None,
        false,
        false,
        true,
        Some("Original"),
    );
    let v = serde_json::to_value(&audio).unwrap();
    assert_eq!(v["IsOriginal"], serde_json::json!(true));
    assert_eq!(v["LocalizedLanguage"], serde_json::json!("English"));
    assert_eq!(v["LocalizedOriginal"], serde_json::json!("Original"));

    let video = MediaStream {
        stream_type: MediaStreamType::Video,
        ..Default::default()
    };
    let v = serde_json::to_value(&video).unwrap();
    assert_eq!(v["IsOriginal"], serde_json::json!(false));
    assert!(v.get("LocalizedLanguage").is_none());
    assert!(v.get("LocalizedOriginal").is_none());
}

#[test]
fn is_original_and_localized_original_deserialize() {
    let s: MediaStream = serde_json::from_str(
        r#"{"Type":"Audio","IsOriginal":true,"LocalizedOriginal":"Original"}"#,
    )
    .unwrap();
    assert!(s.is_original);
    assert_eq!(s.localized_original.as_deref(), Some("Original"));
    assert_eq!(s.stream_type, MediaStreamType::Audio);
}
