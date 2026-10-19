//! Lyrics embedded in an audio file's tags: the text Jellyfin's
//! `AudioFileProber.FetchDataFromTags` (`:562-572`) saves as an `.lrc`
//! sidecar during the probe.
//!
//! Upstream reads the tags with ATL (`z440.atl.core` 7.18), which runs each
//! lyric tag's text through `LyricsInfo.Parse`: text holding `[0` is parsed as
//! LRC, text holding `--> 0` as SRT, anything else is kept unsynchronised.
//! `FetchDataFromTags` drops the SRT ones, prefers a synchronised lyric
//! (rendered back to LRC by `LyricsInfo.FormatSynch`) and falls back to the
//! plain text. Both halves are ported here over the tags ffprobe reports, with
//! ATL's quirks kept, so the saved file is the one Jellyfin writes.
//!
//! The tags ATL maps to its lyrics field, as ffprobe names them (lowercased):
//! an ID3v2 `USLT` frame is `lyrics-<lang>` or `lyrics-<description>-<lang>`,
//! a Vorbis/FLAC/APE `LYRICS` comment and an MP4 `©lyr` atom are `lyrics`,
//! and an ASF `WM/Lyrics` attribute is `wm/lyrics`.
//!
//! Known divergences, each with its fix path:
//! - ffprobe does not expose an ID3v2 `SYLT` (synchronised lyrics) frame, so
//!   a track whose only lyrics are a `SYLT` frame extracts none. To port: read
//!   the `SYLT` frame from the file's ID3v2 tag (ATL `ID3v2.readSynchedLyrics`)
//!   and render it with [`Lyrics::format_synch`].
//! - ffprobe's tags carry no frame order, so between several lyric tags
//!   (two `USLT` languages) the first is taken in key order, not file order.
//!   To port: keep ffprobe's tag order in `MediaInfo::raw_audio_tags`.
//! - ffmpeg writes a `lyrics` key into an MP3 as `TXXX:USLT`, which ffprobe
//!   reports as `lyrics` and ATL reads as an ordinary custom field, so such a
//!   file has lyrics here and none upstream. To port: tell `TXXX` from `USLT`
//!   in the probe's tags.

use std::collections::HashMap;

/// The lyric text to save for a track with these embedded tags (ffprobe's,
/// keys lowercased), or `None` when none is usable — `FetchDataFromTags`'
/// candidate pick, with its `IsNullOrWhiteSpace` gate.
pub(crate) fn from_tags(tags: &HashMap<String, String>) -> Option<String> {
    let mut keys: Vec<&String> = tags
        .keys()
        .filter(|k| *k == "lyrics" || k.starts_with("lyrics-") || *k == "wm/lyrics")
        .collect();
    keys.sort();
    let supported: Vec<Lyrics> = keys
        .into_iter()
        .map(|k| Lyrics::parse(&tags[k]))
        .filter(|l| l.format != Format::Srt)
        .collect();
    let lyrics = supported
        .iter()
        .find(|l| l.format != Format::Unsynchronized)
        .map(Lyrics::format_synch)
        .or_else(|| {
            supported
                .iter()
                .find(|l| l.format == Format::Unsynchronized)
                .map(|l| l.unsynchronized.clone())
        })?;
    (!lyrics.trim().is_empty()).then_some(lyrics)
}

/// `LyricsInfo.LyricsFormat`, as far as `Parse` sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Unsynchronized,
    Lrc,
    LrcA2,
    Srt,
}

/// `LyricsInfo.LyricsPhrase`: a timestamp in milliseconds (`-1` when its
/// timecode had no `:`), and the LRC A2 beats of a line that has them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Phrase {
    start: i32,
    end: i32,
    text: String,
    beats: Vec<Phrase>,
}

/// One lyric tag after `LyricsInfo.Parse`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lyrics {
    format: Format,
    unsynchronized: String,
    synchronized: Vec<Phrase>,
    /// LRC `[key:value]` lines, in first-seen order (a .NET `Dictionary` with
    /// no removals enumerates in insertion order).
    metadata: Vec<(String, String)>,
}

/// A .NET exception inside `Parse`, which then keeps the text unsynchronised.
struct Malformed;

impl Lyrics {
    /// `LyricsInfo.Parse`.
    fn parse(data: &str) -> Self {
        let parsed = if data.contains("[0") {
            parse_lrc(data).map(Some)
        } else if (data.contains("--> 0") || data.contains("-->0")) && srt_parses(data) {
            Ok(Some(Self {
                format: Format::Srt,
                ..Self::unsynchronized(String::new())
            }))
        } else {
            Ok(None)
        };
        match parsed {
            Ok(Some(lyrics)) => lyrics,
            Ok(None) | Err(Malformed) => Self::unsynchronized(data.to_owned()),
        }
    }

    fn unsynchronized(text: String) -> Self {
        Self {
            format: Format::Unsynchronized,
            unsynchronized: text,
            synchronized: Vec::new(),
            metadata: Vec::new(),
        }
    }

    /// `LyricsInfo.FormatSynchToLRC` (`FormatSynch` for every format but SRT,
    /// which is never rendered here). A beat's closing tag repeats its start,
    /// as ATL writes it.
    fn format_synch(&self) -> String {
        let mut out = String::new();
        for (key, value) in &self.metadata {
            out.extend(["[", key, ":", value, "]\n"]);
        }
        if !self.synchronized.is_empty() {
            out.push('\n');
        }
        for line in &self.synchronized {
            out.extend(["[", &encode_timecode_ms(line.start), "]"]);
            if line.beats.is_empty() {
                out.push_str(&line.text);
            } else {
                for beat in &line.beats {
                    let start = encode_timecode_ms(beat.start);
                    out.extend(["<", &start, ">", &beat.text]);
                    if beat.end > -1 {
                        out.extend(["<", &start, ">"]);
                    }
                }
            }
            out.push('\n');
        }
        out
    }
}

/// `LyricsInfo.ParseLRC`. Never `Ok` with a failure: ATL returns `true` for
/// any text it gets through without an exception.
fn parse_lrc(data: &str) -> Result<Lyrics, Malformed> {
    // A line without a timestamp joins the line before it.
    let mut lines: Vec<String> = Vec::new();
    for line in data.split('\n').map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            lines.push(line.to_owned());
        } else if let Some(last) = lines.last_mut() {
            last.push_str(line);
        }
    }
    let mut lyrics = Lyrics {
        format: Format::Lrc,
        ..Lyrics::unsynchronized(String::new())
    };
    for line in &lines {
        let Some(end) = line.find(']') else {
            continue;
        };
        if end == line.len() - 1 {
            let Some(colon) = line.find(':') else {
                continue;
            };
            let key = line[1..colon].trim().to_owned();
            let value = line[colon + 1..end].trim().to_owned();
            match lyrics.metadata.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 = value,
                None => lyrics.metadata.push((key, value)),
            }
            continue;
        }
        let start = &line[1..end];
        let text = line[end + 1..].trim();
        let mut phrase = Phrase {
            start: decode_timecode_ms(start)?,
            end: -1,
            text: text.to_owned(),
            beats: Vec::new(),
        };
        if text.contains('<') && text.contains('>') && has_lrc_a2_beat(text) {
            lyrics.format = Format::LrcA2;
            phrase.beats = parse_beats(text)?;
            phrase.text = phrase
                .beats
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
        }
        lyrics.synchronized.push(phrase);
    }
    Ok(lyrics)
}

/// The LRC A2 beats of one line (`<mm:ss.xx>word`), as `ParseLRC` reads them.
/// ATL's `part[1..endIdx]` drops the first character of each timecode (the
/// `<` it skips is already gone with the split), kept here.
fn parse_beats(text: &str) -> Result<Vec<Phrase>, Malformed> {
    let mut beats: Vec<Phrase> = Vec::new();
    for part in text.split('<').map(str::trim) {
        let Some(close) = part.find('>') else {
            continue;
        };
        let first = part.chars().next().map_or(0, char::len_utf8);
        if close < 1 {
            return Err(Malformed);
        }
        let beat_start = &part[first..close];
        let beat_text = part[close + 1..].trim();
        if beat_text.is_empty() {
            // The timestamp ending the last beat.
            let end = decode_timecode_ms(beat_start)?;
            if end > 0 {
                let last = beats.last_mut().ok_or(Malformed)?;
                last.end = end;
            }
        } else {
            let beat = Phrase {
                start: decode_timecode_ms(beat_start)?,
                end: -1,
                text: beat_text.to_owned(),
                beats: Vec::new(),
            };
            if let Some(last) = beats.last_mut() {
                last.end = beat.start;
            }
            beats.push(beat);
        }
    }
    Ok(beats)
}

/// ATL's `rxLRCA2Beat` (`<\d+:\d+\.\d+>`) found anywhere in `text`.
fn has_lrc_a2_beat(text: &str) -> bool {
    fn digits(s: &str) -> usize {
        s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len()
    }
    text.match_indices('<').any(|(at, _)| {
        let rest = &text[at + 1..];
        let minutes = digits(rest);
        let Some(rest) = rest[minutes..].strip_prefix(':').filter(|_| minutes > 0) else {
            return false;
        };
        let seconds = digits(rest);
        let Some(rest) = rest[seconds..].strip_prefix('.').filter(|_| seconds > 0) else {
            return false;
        };
        let fraction = digits(rest);
        fraction > 0 && rest[fraction..].starts_with('>')
    })
}

/// Whether `LyricsInfo.ParseSRT` gets through `data`: it throws on a line
/// that starts with `-->` (`line[..(arrowIdx - 1)]`).
fn srt_parses(data: &str) -> bool {
    data.split('\n')
        .map(str::trim)
        .all(|line| line.find("-->") != Some(0))
}

/// .NET `int.Parse` (`NumberStyles.Integer`): ASCII whitespace around an
/// optionally signed decimal.
fn int_parse(s: &str) -> Result<i32, Malformed> {
    s.trim_matches(|c| matches!(c, '\t'..='\r' | ' '))
        .parse()
        .map_err(|_| Malformed)
}

/// ATL `Utils.DecodeTimecodeToMs`: `-1` for a timecode with no `:`, an
/// exception for one whose numbers do not parse. Overflow wraps, as unchecked
/// C# arithmetic does.
fn decode_timecode_ms(code: &str) -> Result<i32, Malformed> {
    if !code.contains(':') {
        return Ok(-1);
    }
    let mut parts: Vec<&str> = code.split(':').collect();
    let mut milliseconds = 0i32;
    let last = parts.len() - 1;
    if let Some(separator) = ['.', ','].into_iter().find(|&c| parts[last].contains(c)) {
        let mut sub = parts[last].split(separator);
        let whole = sub.next().unwrap_or_default();
        let fraction = sub.next().unwrap_or_default();
        parts[last] = whole;
        milliseconds = int_parse(fraction)?;
        // Centiseconds: two digits after the second.
        if fraction.encode_utf16().count() == 2 {
            milliseconds = milliseconds.wrapping_mul(10);
        }
    }
    let seconds = int_parse(parts[last])?;
    let minutes = int_parse(parts[last - 1])?;
    let (mut days, mut hours) = (0i32, 0i32);
    if parts.len() >= 3 {
        let sub: Vec<&str> = parts[last - 2].split('d').collect();
        if sub.len() > 1 {
            days = int_parse(sub[0])?;
            hours = int_parse(sub[1])?;
        } else {
            hours = int_parse(sub[0])?;
        }
    }
    Ok(milliseconds
        .wrapping_add(seconds.wrapping_mul(1000))
        .wrapping_add(minutes.wrapping_mul(60 * 1000))
        .wrapping_add(hours.wrapping_mul(60 * 60 * 1000))
        .wrapping_add(days.wrapping_mul(24 * 60 * 60 * 1000)))
}

/// ATL `Utils.EncodeTimecode_ms(ms, MM_SS_XX)`: `mm:ss.xx`, minutes beyond two
/// digits as needed, centiseconds rounded half up (`995` ms renders `100`).
fn encode_timecode_ms(milliseconds: i32) -> String {
    let milliseconds = i64::from(milliseconds);
    let seconds = milliseconds.div_euclid(1000);
    let minutes = seconds.div_euclid(60);
    let pad = |n: i64| {
        let s = n.to_string();
        if s.len() == 1 { format!("0{s}") } else { s }
    };
    let centis = (milliseconds - seconds * 1000 + 5) / 10;
    format!(
        "{}:{}.{centis:02}",
        pad(minutes),
        pad(seconds - 60 * minutes)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn tags(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    // ATL `UtilsTest.Utils_DecodeTimecodeToMs`.
    #[rstest]
    #[case("00:02.2", 2 * 1000 + 2)]
    #[case("01:02.2", 62 * 1000 + 2)]
    #[case("00:21.05", 21 * 1000 + 5 * 10)]
    #[case("00:25.00", 25 * 1000)]
    #[case("00:29.67", 29 * 1000 + 67 * 10)]
    #[case("01:01:16.612", 60 * 60 * 1000 + 60 * 1000 + 16 * 1000 + 612)]
    #[case("01:01:16,612", 60 * 60 * 1000 + 60 * 1000 + 16 * 1000 + 612)]
    #[case("2d 01:01:00.0", 48 * 60 * 60 * 1000 + 60 * 60 * 1000 + 60 * 1000)]
    #[case("12", -1)]
    #[case("", -1)]
    fn decodes_timecodes_as_atl(#[case] code: &str, #[case] ms: i32) {
        assert_eq!(decode_timecode_ms(code).ok(), Some(ms));
    }

    #[rstest]
    #[case("ti:Title")]
    #[case("00:01.")]
    #[case("aa:01.00")]
    fn an_unparsable_timecode_is_an_exception(#[case] code: &str) {
        assert!(decode_timecode_ms(code).is_err());
    }

    // ATL `UtilsTest.Utils_FormatTime` (the `MM_SS_XX` cases), plus the
    // rounding carry and a `-1` (no-`:`) timestamp.
    #[rstest]
    #[case(21 * 1000 + 50, "00:21.05")]
    #[case(21 * 1000 + 5, "00:21.01")]
    #[case(21 * 1000 + 1, "00:21.00")]
    #[case(29 * 1000 + 67 * 10, "00:29.67")]
    #[case(62 * 1000, "01:02.00")]
    #[case(3 * 60 * 60 * 1000, "180:00.00")]
    #[case(995, "00:00.100")]
    #[case(-1, "-1:59.100")]
    fn encodes_lrc_timecodes_as_atl(#[case] ms: i32, #[case] code: &str) {
        assert_eq!(encode_timecode_ms(ms), code);
    }

    #[rstest]
    // Plain text is saved as it is.
    #[case(&[("lyrics-eng", "Line one\nLine two")], Some("Line one\nLine two"))]
    // LRC is re-rendered: metadata first, a blank line, then the lines with
    // two-digit centiseconds; a line with no timestamp joins the one before.
    #[case(
        &[("lyrics", "[ar:Someone]\r\n[00:01.5]Hello\r\nthere\r\n\r\n[01:02.25]World")],
        Some("[ar:Someone]\n\n[00:01.01]Hellothere\n[01:02.25]World\n")
    )]
    // A synchronised lyric wins over a plain one, whatever the key order.
    #[case(
        &[("lyrics-aaa", "Plain"), ("lyrics-eng", "[00:01.00]Synced")],
        Some("\n[00:01.00]Synced\n")
    )]
    // SRT is never saved; the plain fallback still is.
    #[case(
        &[("lyrics-aaa", "1\n00:00:01,000 --> 00:00:02,000\nHi"), ("lyrics-eng", "Plain")],
        Some("Plain")
    )]
    #[case(&[("wm/lyrics", "1\n00:00:01,000 --> 00:00:02,000\nHi")], None)]
    // A tag that only looks like LRC renders to nothing, which is not saved.
    #[case(&[("lyrics", "Intro [0 then words")], None)]
    // An LRC line ATL cannot parse keeps the whole text unsynchronised.
    #[case(&[("lyrics", "[00:01.00]ok\n[by:me] tail")], Some("[00:01.00]ok\n[by:me] tail"))]
    // An SRT arrow at a line start throws in ParseSRT: kept as plain text.
    #[case(&[("lyrics", "--> 0 odd")], Some("--> 0 odd"))]
    // Blank text and non-lyric tags save nothing.
    #[case(&[("lyrics-eng", "  \n ")], None)]
    #[case(&[("title", "Song"), ("unsyncedlyrics", "Text")], None)]
    fn picks_the_lyric_fetch_data_from_tags_saves(
        #[case] pairs: &[(&str, &str)],
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(from_tags(&tags(pairs)).as_deref(), expected);
    }

    #[test]
    fn renders_lrc_a2_beats_as_atl_writes_them() {
        let lyrics = from_tags(&tags(&[(
            "lyrics",
            "[00:28.581]<00:28.581>I'm <00:28.981>wishing<00:29.797>\n[00:31.877]<00:31.877>And",
        )]));
        assert_eq!(
            lyrics.as_deref(),
            Some(
                "\n[00:28.58]<00:28.58>I'm<00:28.58><00:28.98>wishing<00:28.98>\n\
                 [00:31.88]<00:31.88>And\n"
            )
        );
    }

    #[test]
    fn a_beat_timecode_loses_its_first_character_as_in_atl() {
        // `part[1..endIdx]` reads `<12:00.00>` as `2:00.00`.
        let lyrics = Lyrics::parse("[00:01.00]<12:00.00>Word");
        assert_eq!(lyrics.format, Format::LrcA2);
        assert_eq!(lyrics.synchronized[0].beats[0].start, 2 * 60 * 1000);
    }
}
