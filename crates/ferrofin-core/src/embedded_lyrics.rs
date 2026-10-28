//! Lyrics embedded in an audio file's tags: the text Jellyfin's
//! `AudioFileProber.FetchDataFromTags` (`:562-572`) saves as an `.lrc`
//! sidecar during the probe.
//!
//! Upstream reads the tags with ATL (`z440.atl.core` 7.18), which runs each
//! lyric tag's text through `LyricsInfo.Parse`: text holding `[0` is parsed as
//! LRC, text holding `--> 0` as SRT, anything else is kept unsynchronised.
//! `FetchDataFromTags` drops the SRT ones, prefers a synchronised lyric
//! (rendered back to LRC by `LyricsInfo.FormatSynch`) and falls back to the
//! plain text. Both halves are ported here, with ATL's quirks kept, so the
//! saved file is the one Jellyfin writes.
//!
//! [`from_file`] reads the lyric tags from the file itself (with `lofty`), as
//! ATL does: an ID3v2 `USLT` or `SYLT` frame, an APE or Vorbis/FLAC `LYRICS`
//! item and an MP4 `©lyr` atom, in file order. [`from_tags`] is the fallback
//! over the tags ffprobe reports, for a file `lofty` cannot read (an ASF
//! `WM/Lyrics` attribute, Matroska, a malformed tag). ffprobe names an ID3v2
//! `USLT` frame `lyrics-<lang>` or `lyrics-<description>-<lang>`, a `LYRICS`
//! item or `©lyr` atom `lyrics`, and `WM/Lyrics` `wm/lyrics`; its map has no
//! order and no `SYLT`, so the fallback takes the keys in sorted order.
//!
//! Accepted divergence (owner decision 2026-10-19): lyrics in an MP3's `TXXX`
//! frame described `USLT` (how ffmpeg writes a `lyrics` key) or `lyrics`,
//! which ffprobe reports as `lyrics` and ATL reads as an ordinary custom
//! field, so Jellyfin shows no lyrics for such a file. Ferrofin saves them,
//! but only when no tag holds a real lyric, so the pick never differs from
//! Jellyfin's for a file Jellyfin shows lyrics for.

use std::collections::HashMap;
use std::io::Seek;
use std::path::Path;

use lofty::ape::{ApeFile, ApeTag};
use lofty::config::ParseOptions;
use lofty::error::{FileParseError, UnknownFormatError};
use lofty::file::{AudioFile, FileType};
use lofty::flac::FlacFile;
use lofty::id3::v2::{Frame, FrameFlags, Id3v2Tag, SynchronizedTextFrame};
use lofty::iff::aiff::AiffFile;
use lofty::iff::wav::WavFile;
use lofty::mp4::{AtomData, AtomIdent, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::musepack::MpcFile;
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OpusFile, SpeexFile, VorbisFile};
use lofty::probe::Probe;
use lofty::wavpack::WavPackFile;

/// The lyric text to save for the track at `path`, read from the file's own
/// tags — `FetchDataFromTags`' pick over ATL's `Track.Lyrics`.
///
/// ATL takes every lyric of the first tag that has any, in its default tag
/// priority (`MetaDataIOFactory.TagPriority`: ID3v2, APE, then the
/// container's own tag; `CrossMetadataReader.Lyrics`), each tag's in file
/// order. A `TXXX` lyric frame comes last, after every tag (see the module
/// doc's accepted divergence).
///
/// # Errors
///
/// The file cannot be read: a container `lofty` does not support, or a tag
/// it cannot parse. The caller then picks from ffprobe's tags ([`from_tags`]).
pub(crate) fn from_file(path: &Path) -> Result<Option<String>, FileParseError> {
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);
    let probe = Probe::open(path)?.options(options).guess_file_type()?;
    let file_type = probe.file_type();
    let mut reader = probe.into_inner();
    reader.rewind()?;
    let r = &mut reader;
    let by_tag = match file_type {
        Some(FileType::Mpeg) => {
            let f = MpegFile::read_from(r, options)?;
            [id3v2(f.id3v2()), ape(f.ape()), Vec::new(), txxx(f.id3v2())]
        }
        Some(FileType::Ape) => {
            let f = ApeFile::read_from(r, options)?;
            [id3v2(f.id3v2()), ape(f.ape()), Vec::new(), txxx(f.id3v2())]
        }
        Some(FileType::Mpc) => {
            let f = MpcFile::read_from(r, options)?;
            [id3v2(f.id3v2()), ape(f.ape()), Vec::new(), txxx(f.id3v2())]
        }
        Some(FileType::WavPack) => {
            let f = WavPackFile::read_from(r, options)?;
            [Vec::new(), ape(f.ape()), Vec::new(), Vec::new()]
        }
        Some(FileType::Flac) => {
            let f = FlacFile::read_from(r, options)?;
            let native = vorbis(f.vorbis_comments());
            [id3v2(f.id3v2()), Vec::new(), native, txxx(f.id3v2())]
        }
        Some(FileType::Opus) => {
            let f = OpusFile::read_from(r, options)?;
            [
                Vec::new(),
                Vec::new(),
                vorbis(Some(f.vorbis_comments())),
                Vec::new(),
            ]
        }
        Some(FileType::Vorbis) => {
            let f = VorbisFile::read_from(r, options)?;
            [
                Vec::new(),
                Vec::new(),
                vorbis(Some(f.vorbis_comments())),
                Vec::new(),
            ]
        }
        Some(FileType::Speex) => {
            let f = SpeexFile::read_from(r, options)?;
            [
                Vec::new(),
                Vec::new(),
                vorbis(Some(f.vorbis_comments())),
                Vec::new(),
            ]
        }
        Some(FileType::Mp4) => {
            let f = Mp4File::read_from(r, options)?;
            [Vec::new(), Vec::new(), mp4(f.ilst()), Vec::new()]
        }
        Some(FileType::Wav) => {
            let f = WavFile::read_from(r, options)?;
            [id3v2(f.id3v2()), Vec::new(), Vec::new(), txxx(f.id3v2())]
        }
        Some(FileType::Aiff) => {
            let f = AiffFile::read_from(r, options)?;
            [id3v2(f.id3v2()), Vec::new(), Vec::new(), txxx(f.id3v2())]
        }
        _ => return Err(UnknownFormatError.into()),
    };
    Ok(by_tag.into_iter().find(|l| !l.is_empty()).and_then(pick))
}

/// An ID3v2 tag's lyrics, in frame order, as ATL's `ID3v2.readFrame` reads
/// them: a `USLT` frame's text through `LyricsInfo.Parse`, a `SYLT` frame as
/// its timed lines.
fn id3v2(tag: Option<&Id3v2Tag>) -> Vec<Lyrics> {
    tag.into_iter()
        .flatten()
        .filter_map(|frame| match frame {
            Frame::UnsynchronizedText(f) => Some(Lyrics::parse(&f.content)),
            // A body `lofty` cannot parse is dropped, where ATL would keep an
            // empty entry: only a corrupt `SYLT` frame tells the two apart.
            Frame::Binary(f) if frame.id_str() == "SYLT" => Lyrics::from_sylt(&f.data),
            _ => None,
        })
        .collect()
}

/// The text of the `TXXX` frames ffprobe reports as `lyrics` (described
/// `USLT`, as ffmpeg writes them, or `lyrics`), for the accepted divergence.
fn txxx(tag: Option<&Id3v2Tag>) -> Vec<Lyrics> {
    tag.into_iter()
        .flatten()
        .filter_map(|frame| match frame {
            Frame::UserText(f)
                if f.description.eq_ignore_ascii_case("USLT")
                    || f.description.eq_ignore_ascii_case("lyrics") =>
            {
                Some(Lyrics::parse(&f.content))
            }
            _ => None,
        })
        .collect()
}

/// An APE tag's `LYRICS` item (APE keys are unique, compared ignoring case).
fn ape(tag: Option<&ApeTag>) -> Vec<Lyrics> {
    tag.and_then(|t| t.get("LYRICS"))
        .and_then(|item| item.value().text())
        .map(Lyrics::parse)
        .into_iter()
        .collect()
}

/// A Vorbis comment block's `LYRICS` comments: ATL's `VorbisTag.SetMetaField`
/// joins repeated ones with no separator into one lyric.
fn vorbis(tag: Option<&VorbisComments>) -> Vec<Lyrics> {
    match tag {
        Some(t) if t.get("LYRICS").is_some() => {
            vec![Lyrics::parse(&t.get_all("LYRICS").collect::<String>())]
        }
        _ => Vec::new(),
    }
}

/// An MP4 `©lyr` atom's text.
fn mp4(tag: Option<&Ilst>) -> Vec<Lyrics> {
    let Some(atom) = tag.and_then(|t| t.get(&AtomIdent::Fourcc(*b"\xa9lyr"))) else {
        return Vec::new();
    };
    let text: String = atom
        .data()
        .filter_map(|d| match d {
            AtomData::UTF8(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    vec![Lyrics::parse(&text)]
}

/// The lyric text to save for a track with these embedded tags (ffprobe's,
/// keys lowercased), or `None` when none is usable: [`pick`] over the lyric
/// keys in sorted order. The fallback for a file [`from_file`] cannot read.
pub(crate) fn from_tags(tags: &HashMap<String, String>) -> Option<String> {
    let mut keys: Vec<&String> = tags
        .keys()
        .filter(|k| *k == "lyrics" || k.starts_with("lyrics-") || *k == "wm/lyrics")
        .collect();
    keys.sort();
    pick(keys.into_iter().map(|k| Lyrics::parse(&tags[k])).collect())
}

/// `FetchDataFromTags`' candidate pick over a track's lyrics, in order: SRT
/// is dropped, the first synchronised lyric wins (rendered to LRC), else the
/// first plain one, and blank text saves nothing (`IsNullOrWhiteSpace`).
fn pick(lyrics: Vec<Lyrics>) -> Option<String> {
    let supported: Vec<Lyrics> = lyrics
        .into_iter()
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

/// `LyricsInfo.LyricsFormat`, as far as `Parse` and the `SYLT` reader set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Unsynchronized,
    Lrc,
    LrcA2,
    Srt,
    /// `SYNCHRONIZED`: an ID3v2 `SYLT` frame.
    Synchronized,
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

    /// A `SYLT` frame body, as ATL's `ID3v2.readFrame` reads it: every line
    /// timed in milliseconds whatever the frame's timestamp format says, and
    /// one newline before a line's text skipped (`readLyricsPhrase`). ATL
    /// reads past a language, timestamp format or content type `lofty`
    /// rejects, so those bytes are normalised first. `None` for a body `lofty`
    /// still cannot parse.
    fn from_sylt(data: &[u8]) -> Option<Self> {
        let mut data = data.to_vec();
        if let Some(header) = data.get_mut(1..6) {
            header.copy_from_slice(b"xxx\x02\x01");
        }
        let frame = SynchronizedTextFrame::parse(&data, FrameFlags::default()).ok()?;
        let synchronized = frame
            .content
            .into_iter()
            .map(|(start, text)| Phrase {
                // ATL reads the timestamp as a signed big-endian int.
                start: start.cast_signed(),
                end: -1,
                text: text
                    .strip_prefix('\n')
                    .map_or_else(|| text.clone(), str::to_owned),
                beats: Vec::new(),
            })
            .collect();
        Some(Self {
            format: Format::Synchronized,
            synchronized,
            ..Self::unsynchronized(String::new())
        })
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

    // ------------------------------------------------- the file's own tags

    use lofty::TextEncoding;
    use lofty::config::WriteOptions;
    use lofty::id3::v2::{
        BinaryFrame, ExtendedTextFrame, FrameId, SyncTextContentType, TimestampFormat,
        UnsynchronizedTextFrame,
    };
    use lofty::tag::TagExt;

    /// Three silent MPEG-1 Layer III frames (128 kbit/s, 44.1 kHz).
    fn mpeg_frames() -> Vec<u8> {
        let mut bytes = Vec::new();
        for _ in 0..3 {
            bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
            bytes.extend(std::iter::repeat_n(0u8, 413));
        }
        bytes
    }

    /// An MP3 whose ID3v2 tag holds `frames` in this order, and an APE tag
    /// with `ape_lyrics` as its `LYRICS` item when given.
    fn mp3(frames: Vec<Frame<'static>>, ape_lyrics: Option<&str>) -> Vec<u8> {
        let mut bytes = Vec::new();
        if !frames.is_empty() {
            let mut tag = Id3v2Tag::new();
            for frame in frames {
                let _ = tag.insert(frame);
            }
            tag.dump_to(&mut bytes, WriteOptions::default())
                .expect("dump id3v2");
        }
        bytes.extend(mpeg_frames());
        if let Some(text) = ape_lyrics {
            let mut tag = ApeTag::new();
            tag.insert(
                lofty::ape::ApeItem::new(
                    "LYRICS".to_owned(),
                    lofty::tag::ItemValue::Text(text.to_owned()),
                )
                .expect("ape item"),
            );
            tag.dump_to(&mut bytes, WriteOptions::default())
                .expect("dump ape");
        }
        bytes
    }

    fn uslt(language: [u8; 3], text: &str) -> Frame<'static> {
        Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
            TextEncoding::UTF8,
            language,
            "",
            text.to_owned(),
        ))
    }

    /// A `SYLT` frame with these timestamps and lines; `patch` rewrites its
    /// body (language at 1..4, timestamp format at 4, content type at 5).
    fn sylt_with(lines: &[(u32, &str)], patch: &[(usize, u8)]) -> Frame<'static> {
        let mut body = SynchronizedTextFrame::new(
            TextEncoding::UTF8,
            *b"eng",
            TimestampFormat::MS,
            SyncTextContentType::Lyrics,
            None,
            lines.iter().map(|(t, s)| (*t, (*s).to_owned())).collect(),
        )
        .as_bytes(WriteOptions::default())
        .expect("sylt");
        for (at, byte) in patch {
            body[*at] = *byte;
        }
        Frame::Binary(BinaryFrame::new(FrameId::new("SYLT").expect("id"), body))
    }

    fn sylt(lines: &[(u32, &str)]) -> Frame<'static> {
        sylt_with(lines, &[])
    }

    fn txxx(description: &str, text: &str) -> Frame<'static> {
        Frame::UserText(ExtendedTextFrame::new(
            TextEncoding::UTF8,
            description.to_owned(),
            text.to_owned(),
        ))
    }

    fn read(bytes: &[u8]) -> Result<Option<String>, FileParseError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("track.mp3");
        std::fs::write(&path, bytes).expect("write");
        from_file(&path)
    }

    #[rstest]
    // A `SYLT` frame is timed LRC; ATL skips a newline before a line.
    #[case(vec![sylt(&[(1_000, "Hello"), (65_250, "\nWorld")])], None, Some("\n[00:01.00]Hello\n[01:05.25]World\n"))]
    // ATL reads past a language, timestamp format and content type lofty
    // rejects, and takes MPEG-frame timestamps as milliseconds.
    #[case(vec![sylt_with(&[(2_000, "Hi")], &[(1, 0), (2, 0), (3, 0), (4, 1), (5, 0x20)])], None, Some("\n[00:02.00]Hi\n"))]
    // Two `USLT` languages: the first in file order.
    #[case(vec![uslt(*b"eng", "English"), uslt(*b"deu", "Deutsch")], None, Some("English"))]
    #[case(vec![uslt(*b"deu", "Deutsch"), uslt(*b"eng", "English")], None, Some("Deutsch"))]
    // A synchronised lyric wins over an earlier plain one.
    #[case(vec![uslt(*b"eng", "Plain"), sylt(&[(0, "Timed")])], None, Some("\n[00:00.00]Timed\n"))]
    // An empty first `USLT` is the plain candidate, so nothing is saved (ATL).
    #[case(vec![uslt(*b"eng", ""), uslt(*b"deu", "Deutsch")], None, None)]
    // A `TXXX` lyric frame only when no tag has a lyric (the accepted divergence).
    #[case(vec![txxx("LYRICS", "Custom")], None, Some("Custom"))]
    // ffmpeg's own `lyrics` key is a `TXXX` described `USLT`.
    #[case(vec![txxx("USLT", "From ffmpeg")], None, Some("From ffmpeg"))]
    #[case(vec![txxx("lyrics", "[00:01.00]Custom"), uslt(*b"eng", "Real")], None, Some("Real"))]
    #[case(vec![uslt(*b"eng", "1\n00:00:01,000 --> 00:00:02,000\nHi"), txxx("lyrics", "Custom")], None, None)]
    #[case(vec![txxx("comment", "Not lyrics")], None, None)]
    // Tag priority: ID3v2, then APE.
    #[case(vec![uslt(*b"eng", "From ID3v2")], Some("From APE"), Some("From ID3v2"))]
    #[case(vec![txxx("lyrics", "Custom")], Some("From APE"), Some("From APE"))]
    #[case(Vec::new(), Some("[00:03.00]From APE"), Some("\n[00:03.00]From APE\n"))]
    #[case(Vec::new(), None, None)]
    fn reads_the_lyric_atl_takes_from_the_file(
        #[case] frames: Vec<Frame<'static>>,
        #[case] ape_lyrics: Option<&str>,
        #[case] expected: Option<&str>,
    ) {
        let lyrics = read(&mp3(frames, ape_lyrics)).expect("readable");
        assert_eq!(lyrics.as_deref(), expected);
    }

    /// A FLAC holding a zeroed `STREAMINFO` and a Vorbis comment block with
    /// these `KEY=value` comments, in order.
    fn flac(comments: &[&str]) -> Vec<u8> {
        let mut block = Vec::new();
        let vendor = b"ferrofin";
        block.extend(u32::try_from(vendor.len()).expect("len").to_le_bytes());
        block.extend(vendor);
        block.extend(u32::try_from(comments.len()).expect("count").to_le_bytes());
        for comment in comments {
            block.extend(u32::try_from(comment.len()).expect("len").to_le_bytes());
            block.extend(comment.as_bytes());
        }
        let mut bytes = b"fLaC".to_vec();
        bytes.extend([0, 0, 0, 34]);
        bytes.extend([0u8; 34]);
        // The last metadata block: VORBIS_COMMENT (type 4).
        let len = u32::try_from(block.len()).expect("len").to_be_bytes();
        bytes.extend([0x84, len[1], len[2], len[3]]);
        bytes.extend(block);
        bytes
    }

    #[rstest]
    // ATL joins repeated `LYRICS` comments with no separator.
    #[case(&["TITLE=Song", "LYRICS=Part one, ", "lyrics=part two"], Some("Part one, part two"))]
    #[case(&["TITLE=Song"], None)]
    fn reads_vorbis_comment_lyrics(#[case] comments: &[&str], #[case] expected: Option<&str>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("track.flac");
        std::fs::write(&path, flac(comments)).expect("write");
        assert_eq!(from_file(&path).expect("readable").as_deref(), expected);
    }

    /// An ISO BMFF box: big-endian size, type, payload.
    fn mp4_box(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(payload.len() + 8).expect("size");
        let mut out = size.to_be_bytes().to_vec();
        out.extend(kind);
        out.extend(payload);
        out
    }

    /// An M4A whose `ilst` holds a `©lyr` atom with this UTF-8 text, or no
    /// lyric atom at all.
    fn m4a(lyrics: Option<&str>) -> Vec<u8> {
        let mut ilst = Vec::new();
        let title = [&[0, 0, 0, 1, 0, 0, 0, 0][..], b"Song"].concat();
        ilst.extend(mp4_box(*b"\xa9nam", &mp4_box(*b"data", &title)));
        if let Some(text) = lyrics {
            // `data`: a version-and-type word (1 = UTF-8), then a locale.
            let data = [&[0, 0, 0, 1, 0, 0, 0, 0][..], text.as_bytes()].concat();
            ilst.extend(mp4_box(*b"\xa9lyr", &mp4_box(*b"data", &data)));
        }
        let hdlr = [&[0u8; 8][..], b"mdirappl", &[0u8; 9]].concat();
        let meta = [
            &[0u8; 4][..],
            &mp4_box(*b"hdlr", &hdlr),
            &mp4_box(*b"ilst", &ilst),
        ]
        .concat();
        let moov = mp4_box(*b"moov", &mp4_box(*b"udta", &mp4_box(*b"meta", &meta)));
        let ftyp = mp4_box(*b"ftyp", b"M4A \0\0\0\0M4A isom");
        [ftyp, moov].concat()
    }

    #[rstest]
    #[case(Some("[00:04.00]From MP4"), Some("\n[00:04.00]From MP4\n"))]
    #[case(None, None)]
    fn reads_the_mp4_lyrics_atom(#[case] lyrics: Option<&str>, #[case] expected: Option<&str>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("track.m4a");
        std::fs::write(&path, m4a(lyrics)).expect("write");
        assert_eq!(from_file(&path).expect("readable").as_deref(), expected);
    }

    /// A WAV whose `id3 ` chunk holds an ID3v2 tag with these frames.
    fn wav(frames: Vec<Frame<'static>>) -> Vec<u8> {
        let mut tag = Id3v2Tag::new();
        for frame in frames {
            let _ = tag.insert(frame);
        }
        let mut id3 = Vec::new();
        tag.dump_to(&mut id3, WriteOptions::default())
            .expect("dump id3v2");
        let chunk = |kind: &[u8; 4], body: &[u8]| {
            let mut out = kind.to_vec();
            out.extend(u32::try_from(body.len()).expect("len").to_le_bytes());
            out.extend(body);
            if body.len() % 2 == 1 {
                out.push(0);
            }
            out
        };
        // PCM, mono, 44.1 kHz, 16-bit.
        let fmt = [1u16.to_le_bytes(), 1u16.to_le_bytes()]
            .concat()
            .into_iter()
            .chain(44_100u32.to_le_bytes())
            .chain(88_200u32.to_le_bytes())
            .chain(2u16.to_le_bytes())
            .chain(16u16.to_le_bytes())
            .collect::<Vec<u8>>();
        let body = [
            b"WAVE".to_vec(),
            chunk(b"fmt ", &fmt),
            chunk(b"data", &[0u8; 4]),
            chunk(b"id3 ", &id3),
        ]
        .concat();
        let mut out = b"RIFF".to_vec();
        out.extend(u32::try_from(body.len()).expect("len").to_le_bytes());
        out.extend(body);
        out
    }

    #[test]
    fn reads_the_id3v2_chunk_of_a_wav() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("track.wav");
        std::fs::write(
            &path,
            wav(vec![uslt(*b"deu", "Erste"), uslt(*b"eng", "Second")]),
        )
        .expect("write");
        assert_eq!(
            from_file(&path).expect("readable").as_deref(),
            Some("Erste")
        );
    }

    #[rstest]
    #[case(b"not audio at all".as_slice())]
    #[case(b"ID3\x04\x00\x00\x7f\x7f\x7f\x7fnot a tag".as_slice())]
    fn an_unreadable_file_is_an_error(#[case] bytes: &[u8]) {
        assert!(read(bytes).is_err());
    }

    #[test]
    fn a_beat_timecode_loses_its_first_character_as_in_atl() {
        // `part[1..endIdx]` reads `<12:00.00>` as `2:00.00`.
        let lyrics = Lyrics::parse("[00:01.00]<12:00.00>Word");
        assert_eq!(lyrics.format, Format::LrcA2);
        assert_eq!(lyrics.synchronized[0].beats[0].start, 2 * 60 * 1000);
    }
}
