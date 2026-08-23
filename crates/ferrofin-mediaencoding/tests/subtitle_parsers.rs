//! Transliteration of `Jellyfin.MediaEncoding.Subtitles.Tests`
//! `SrtParserTests` / `SsaParserTests` / `AssParserTests`. Expected values are
//! the C# oracle verbatim (ticks are `TimeSpan.Parse(...).Ticks`).

use ferrofin_mediaencoding::subtitles::{SubtitleEditParser, SubtitleParser};

/// The `Environment.NewLine` libse joins cue text with — a bare LF on the Linux
/// reference server (the C# oracle is written against `Environment.NewLine`).
const NL: &str = "\n";

fn load(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// `TimeSpan.Parse("HH:MM:SS.fff").Ticks` — 100 ns ticks for a wall-clock time.
fn ticks(h: i64, m: i64, s: i64, millis: i64) -> i64 {
    ((((h * 60) + m) * 60 + s) * 1000 + millis) * 10_000
}

#[test]
fn srt_parse_treats_every_net_line_terminator_alike() {
    // `StreamReader.ReadLine` ends a line at `\r\n`, `\n` or a bare `\r`; the
    // joined cue text is the same whichever the file used.
    let parser = SubtitleEditParser::new();
    for data in [
        "1\r\n00:00:01,000 --> 00:00:02,000\r\nfirst\r\nsecond\r\n",
        "1\n00:00:01,000 --> 00:00:02,000\nfirst\nsecond\n",
        "1\r00:00:01,000 --> 00:00:02,000\rfirst\rsecond\r",
    ] {
        let parsed = parser.parse(data.as_bytes(), "srt").unwrap();
        assert_eq!(parsed.paragraphs.len(), 1, "{data:?}");
        assert_eq!(
            parsed.paragraphs[0].text,
            format!("first{NL}second"),
            "{data:?}"
        );
    }
}

// SrtParserTests.Parse_Valid_Success
#[test]
fn srt_parse_valid_success() {
    let parser = SubtitleEditParser::new();
    let parsed = parser.parse(&load("example.srt"), "srt").unwrap();
    assert_eq!(parsed.paragraphs.len(), 2);

    let p1 = &parsed.paragraphs[0];
    assert_eq!(p1.number, 1);
    assert_eq!(p1.start_time.ticks(), ticks(0, 2, 17, 440));
    assert_eq!(p1.end_time.ticks(), ticks(0, 2, 20, 375));
    assert_eq!(
        p1.text,
        format!("Senator, we're making{NL}our final approach into Coruscant.")
    );

    let p2 = &parsed.paragraphs[1];
    assert_eq!(p2.number, 2);
    assert_eq!(p2.start_time.ticks(), ticks(0, 2, 20, 476));
    assert_eq!(p2.end_time.ticks(), ticks(0, 2, 22, 501));
    assert_eq!(p2.text, "Very good, Lieutenant.");
}

// SrtParserTests.Parse_EmptyNewlineBetweenText_Success
#[test]
fn srt_parse_empty_newline_between_text_success() {
    let parser = SubtitleEditParser::new();
    let parsed = parser.parse(&load("example2.srt"), "srt").unwrap();
    assert_eq!(parsed.paragraphs.len(), 2);

    let p1 = &parsed.paragraphs[0];
    assert_eq!(p1.number, 311);
    assert_eq!(p1.start_time.ticks(), ticks(0, 16, 46, 465));
    assert_eq!(p1.end_time.ticks(), ticks(0, 16, 49, 9));
    assert_eq!(
        p1.text,
        format!("Una vez que la gente se entere{NL}{NL}de que ustedes están aquí,")
    );

    let p2 = &parsed.paragraphs[1];
    assert_eq!(p2.number, 312);
    assert_eq!(p2.start_time.ticks(), ticks(0, 16, 49, 92));
    assert_eq!(p2.end_time.ticks(), ticks(0, 16, 51, 470));
    assert_eq!(
        p2.text,
        format!("este lugar se convertirá{NL}{NL}en un maldito zoológico.")
    );
}

// SsaParserTests.Parse_MultipleDialogues_Success
#[test]
fn ssa_parse_multiple_dialogues_success() {
    let ssa = "[Events]
                Format: Layer, Start, End, Text
                Dialogue: ,0:00:01.18,0:00:01.85,dialogue1
                Dialogue: ,0:00:02.18,0:00:02.85,dialogue2
                Dialogue: ,0:00:03.18,0:00:03.85,dialogue3
                ";

    let parser = SubtitleEditParser::new();
    let parsed = parser.parse(ssa.as_bytes(), "ssa").unwrap();

    let expected: [(&str, &str, i64, i64); 3] = [
        ("1", "dialogue1", 11_800_000, 18_500_000),
        ("2", "dialogue2", 21_800_000, 28_500_000),
        ("3", "dialogue3", 31_800_000, 38_500_000),
    ];
    assert_eq!(parsed.paragraphs.len(), expected.len());

    for (p, (id, text, start, end)) in parsed.paragraphs.iter().zip(expected) {
        assert_eq!(p.number.to_string(), id);
        assert_eq!(p.text, text);
        assert_eq!(p.start_time.ticks(), start);
        assert_eq!(p.end_time.ticks(), end);
    }
}

// SsaParserTests.Parse_Valid_Success
#[test]
fn ssa_parse_valid_success() {
    let parser = SubtitleEditParser::new();
    let parsed = parser.parse(&load("example.ssa"), "ssa").unwrap();
    assert_eq!(parsed.paragraphs.len(), 1);

    let p = &parsed.paragraphs[0];
    assert_eq!(p.number, 1);
    assert_eq!(p.start_time.ticks(), ticks(0, 0, 1, 180));
    assert_eq!(p.end_time.ticks(), ticks(0, 0, 6, 850));
    assert_eq!(p.text, "{\\pos(400,570)}Like an angel with pity on nobody");
}

// AssParserTests.Parse_Valid_Success
#[test]
fn ass_parse_valid_success() {
    let parser = SubtitleEditParser::new();
    let parsed = parser.parse(&load("example.ass"), "ass").unwrap();
    assert_eq!(parsed.paragraphs.len(), 1);

    let p = &parsed.paragraphs[0];
    assert_eq!(p.number, 1);
    assert_eq!(p.start_time.ticks(), ticks(0, 0, 1, 180));
    assert_eq!(p.end_time.ticks(), ticks(0, 0, 6, 850));
    assert_eq!(
        p.text,
        format!(
            "{{\\pos(400,570)}}Like an Angel with pity on nobody{NL}The second line in subtitle"
        )
    );
}
