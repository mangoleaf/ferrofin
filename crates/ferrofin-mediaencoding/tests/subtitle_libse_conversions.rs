//! Subtitle conversion checked byte for byte against Jellyfin 12.2.
//!
//! Each expected file under `tests/data/libse/expected/<input>/<variant>/` is the
//! output of Jellyfin 12.2's own conversion path: `SubtitleEditParser.Parse`,
//! `SubtitleEncoder.FilterEvents`, then the libse 4.0.12 writer `TryGetWriter`
//! returns, encoded with `Encoding.UTF8.GetBytes`, run on .NET 10 for Linux with the
//! image's `en-US` locale. Only the conversions Jellyfin can reach are listed: an SRT
//! source reaches the SubRip writer only through the `subrip` alias, and an SSA or
//! ASS source asked for its own format (or SSA asked for ASS) is served verbatim.
//!
//! JSON is compared as parsed values: .NET's `Utf8JsonWriter` escapes `<`, `>`, `&`,
//! `'`, `"`, `+` and every non-ASCII character as `\uXXXX`, and `serde_json` writes
//! them as they are, which decodes to the same strings. Jellyfin 10.11's writer
//! escaped the same way, so that difference predates the libse writers.

use std::path::PathBuf;

use ferrofin_mediaencoding::subtitles::{
    NoopSubtitleIo, SubtitleEditParser, SubtitleEncoder, SubtitleInfo,
};
use ferrofin_model::media_info::MediaProtocol;
use rstest::rstest;

fn data(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/libse")
        .join(path)
}

/// The writer formats an input of `ext` can be converted to, and the request format
/// that reaches each.
fn targets(ext: &str) -> &'static [(&'static str, &'static str)] {
    match ext {
        "srt" => &[
            ("subrip", "srt"),
            ("vtt", "vtt"),
            ("ass", "ass"),
            ("ssa", "ssa"),
            ("json", "json"),
            ("ttml", "ttml"),
        ],
        "ass" => &[
            ("srt", "srt"),
            ("vtt", "vtt"),
            ("ssa", "ssa"),
            ("json", "json"),
            ("ttml", "ttml"),
        ],
        "ssa" => &[
            ("srt", "srt"),
            ("vtt", "vtt"),
            ("json", "json"),
            ("ttml", "ttml"),
        ],
        other => panic!("no targets for {other}"),
    }
}

/// `input` converted to `request` the way a `Stream.{request}` request converts it.
async fn convert(
    input: &str,
    request: &str,
    start_ticks: i64,
    end_ticks: i64,
    preserve: bool,
) -> String {
    let ext = input.rsplit('.').next().unwrap();
    let source = std::fs::read(data(&format!("input/{input}"))).unwrap();
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), NoopSubtitleIo);
    let info = SubtitleInfo {
        path: format!("/media/{input}"),
        protocol: MediaProtocol::File,
        format: ext.to_owned(),
        is_external: true,
    };
    let bytes = encoder
        .convert_subtitles(&source, &info, request, start_ticks, end_ticks, preserve)
        .await
        .unwrap();
    String::from_utf8(bytes).unwrap()
}

/// Jellyfin 12.2's output for `input` in `writer`'s format.
fn expected(input: &str, variant: &str, writer: &str) -> String {
    std::fs::read_to_string(data(&format!("expected/{input}/{variant}/{writer}"))).unwrap()
}

/// Byte equality, except JSON, compared as parsed values (see the module doc).
fn same(writer: &str, actual: &str, expected: &str) -> bool {
    if writer == "json" {
        serde_json::from_str::<serde_json::Value>(actual).unwrap()
            == serde_json::from_str::<serde_json::Value>(expected).unwrap()
    } else {
        actual == expected
    }
}

/// The first line where `actual` and `expected` differ, for the failure message.
fn first_difference(actual: &str, expected: &str) -> String {
    let (a, e): (Vec<&str>, Vec<&str>) =
        (actual.split('\n').collect(), expected.split('\n').collect());
    for i in 0..a.len().max(e.len()) {
        if a.get(i) != e.get(i) {
            return format!(
                "line {}: got {:?}, Jellyfin {:?}",
                i + 1,
                a.get(i),
                e.get(i)
            );
        }
    }
    "identical lines".to_owned()
}

#[rstest]
#[case::tags("tags.srt", "plain", 0, 0, false)]
#[case::tags_offset_by_a_fraction_of_a_millisecond("tags.srt", "offset", 12_345, 0, false)]
#[case::positions("positions.srt", "plain", 0, 0, false)]
#[case::colors("colors.srt", "plain", 0, 0, false)]
#[case::styled_ass("styled.ass", "plain", 0, 0, false)]
#[case::styled_ass_window_preserving_timestamps(
    "styled.ass",
    "window",
    40_000_000,
    90_000_000,
    true
)]
#[case::styled_ssa("styled.ssa", "plain", 0, 0, false)]
#[case::styled_ssa_offset("styled.ssa", "offset", 25_000_000, 0, false)]
#[case::ttml_markup("ttml_markup.srt", "plain", 0, 0, false)]
#[case::ttml_markup_every_cue_filtered_out("ttml_markup.srt", "empty", 900_000_000, 0, false)]
#[case::ttml_styles_from_an_ass_script("ttml_styles.ass", "plain", 0, 0, false)]
#[case::italic_tag_repairs("italics.srt", "plain", 0, 0, false)]
#[case::a_multibyte_colour_and_out_of_range_decimals_in_ass_styles(
    "edges.ass",
    "plain",
    0,
    0,
    false
)]
#[case::ssa_tags_libse_reads_oddly_or_rejects("edges.ssa", "plain", 0, 0, false)]
#[case::an_events_format_without_a_text_column("notext.ass", "plain", 0, 0, false)]
#[case::markup_nested_two_hundred_deep("nested.srt", "plain", 0, 0, false)]
#[case::name_prefixes_before_two_italic_lines("colons.srt", "plain", 0, 0, false)]
#[case::dotnet_digit_and_letter_classes("charclass.srt", "plain", 0, 0, false)]
#[tokio::test]
async fn conversion_matches_jellyfin_byte_for_byte(
    #[case] input: &str,
    #[case] variant: &str,
    #[case] start_ticks: i64,
    #[case] end_ticks: i64,
    #[case] preserve: bool,
) {
    let ext = input.rsplit('.').next().unwrap();
    let mut mismatches = Vec::new();
    for (request, writer) in targets(ext) {
        let expected = expected(input, variant, writer);
        let actual = convert(input, request, start_ticks, end_ticks, preserve).await;
        if !same(writer, &actual, &expected) {
            mismatches.push(format!(
                "{input} → {writer}: {}",
                first_difference(&actual, &expected)
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "differs from Jellyfin 12.2:\n{}",
        mismatches.join("\n")
    );
}

/// Jellyfin 12.2 shows an SSA/ASS script's `Comment:` events as subtitle lines in
/// every format but SSA/ASS, a bug Ferrofin does not port: those formats come out as
/// Jellyfin writes the script without its comments, and SSA keeps them as `Comment:`
/// lines exactly as Jellyfin writes them.
#[tokio::test]
async fn comment_events_are_kept_for_ssa_and_left_out_of_every_other_format() {
    let ssa = convert("comments.ass", "ssa", 0, 0, false).await;
    assert_eq!(ssa, expected("comments.ass", "plain", "ssa"));
    assert!(ssa.contains("\nComment: Marked=0,0:00:00.50,"), "{ssa}");
    for (request, writer) in [
        ("srt", "srt"),
        ("vtt", "vtt"),
        ("json", "json"),
        ("ttml", "ttml"),
    ] {
        let actual = convert("comments.ass", request, 0, 0, false).await;
        let without = expected("comments_removed.ass", "plain", writer);
        assert!(
            same(writer, &actual, &without),
            "{writer}: {}",
            first_difference(&actual, &without)
        );
        assert!(
            !actual.contains("note"),
            "{writer} kept a comment: {actual}"
        );
    }
}

/// libse recurses as deep as a cue's markup nests; Ferrofin's TTML writer gives markup
/// nested past its limit libse's "wrong markup" fallback instead of overflowing the
/// worker's stack, and every other writer handles any depth.
#[tokio::test]
async fn markup_nested_thousands_deep_converts_without_overflowing_the_stack() {
    let input = format!(
        "1\n00:00:01,000 --> 00:00:02,000\nx{}y\n",
        "<a>".repeat(30_000)
    );
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), NoopSubtitleIo);
    let info = SubtitleInfo {
        path: "/media/deep.srt".to_owned(),
        protocol: MediaProtocol::File,
        format: "srt".to_owned(),
        is_external: true,
    };
    for request in ["ttml", "vtt", "ass", "ssa", "json", "subrip"] {
        let out = encoder
            .convert_subtitles(input.as_bytes(), &info, request, 0, 0, false)
            .await;
        assert!(out.is_ok(), "{request}: {out:?}");
    }
    let ttml = encoder
        .convert_subtitles(input.as_bytes(), &info, "ttml", 0, 0, false)
        .await
        .unwrap();
    let ttml = String::from_utf8(ttml).unwrap();
    // The fallback strips `<` and `>` from the text.
    assert!(
        ttml.contains(&format!("x{}y</p>", "a".repeat(30_000))),
        "{}",
        &ttml[..ttml.len().min(2000)]
    );
}

/// libse's `FixInvalidItalicTags` recurses once per `NAME:` before two italic lines,
/// which the TTML writer runs on every cue; Ferrofin walks the same recursion on an
/// explicit stack.
#[tokio::test]
async fn thousands_of_name_prefixes_convert_without_overflowing_the_stack() {
    let input = format!(
        "1\n00:00:01,000 --> 00:00:02,000\n{}<i>x</i>\n<i>y</i>\n",
        "ab:".repeat(5_000)
    );
    let encoder = SubtitleEncoder::new(SubtitleEditParser::new(), NoopSubtitleIo);
    let info = SubtitleInfo {
        path: "/media/colons.srt".to_owned(),
        protocol: MediaProtocol::File,
        format: "srt".to_owned(),
        is_external: true,
    };
    let ttml = encoder
        .convert_subtitles(input.as_bytes(), &info, "ttml", 0, 0, false)
        .await
        .unwrap();
    let ttml = String::from_utf8(ttml).unwrap();
    assert!(
        ttml.contains(&format!(
            "{}<span tts:fontStyle=\"italic\">x<br />y</span></p>",
            "ab: ".repeat(5_000)
        )),
        "{}",
        &ttml[ttml.len().saturating_sub(400)..]
    );
}
