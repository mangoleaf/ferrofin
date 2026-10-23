//! WebVTT (`.vtt`) loader and writer.
//!
//! The loader is libse 4.0.12's `WebVTT.LoadSubtitle`, the format Jellyfin's
//! `SubtitleEditParser` registers for `vtt`.
//!
//! The writer is libse 4.0.12's `Nikse.SubtitleEdit.Core.SubtitleFormats.WebVTT.ToText`,
//! which Jellyfin 12's `SubtitleEncoder` writes with: the `WEBVTT` signature (or a
//! source WebVTT header), then per cue its timing, the cue settings an `{\anN}` tag
//! asks for, and the text with SSA tags removed, runs of line breaks collapsed,
//! `<font color>` turned into WebVTT colour classes, and stray `<`, `>` and `&`
//! escaped. No BOM, no region block, and no line end after the last cue.

use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;

use super::dotnet::{Color, color_from_html, is_letter};
use super::libse_text::{
    color_to_hex_with_transparency, remove_recursive_line_breaks, remove_ssa_tags,
};
use super::model::{Paragraph, Subtitle, TimeCode, dotnet_int};
use super::srt::NEWLINE;

/// libse's `SubtitleSettings.WebVttCueAn1` … `WebVttCueAn9` defaults, which Jellyfin
/// never changes: the cue settings for each `{\anN}` alignment (`An2`, bottom
/// centre, is WebVTT's own default and has none).
const CUE_SETTINGS: [(&str, &str); 9] = [
    ("{\\an1", "position:20%"),
    ("{\\an2", ""),
    ("{\\an3", "position:80%"),
    ("{\\an4", "position:20% line:50%"),
    ("{\\an5", "line:50%"),
    ("{\\an6", "position:80% line:50%"),
    ("{\\an7", "position:20% line:20%"),
    ("{\\an8", "line:20%"),
    ("{\\an9", "position:80% line:20%"),
];

/// `WebVTT.DefaultColorClasses`, in libse's order (the first close match wins).
const COLOR_CLASSES: [(&str, (u8, u8, u8)); 8] = [
    ("white", (255, 255, 255)),
    ("lime", (0, 255, 0)),
    ("cyan", (0, 255, 255)),
    ("red", (255, 0, 0)),
    ("yellow", (255, 255, 0)),
    ("magenta", (255, 0, 255)),
    ("blue", (0, 0, 255)),
    ("black", (0, 0, 0)),
];

/// `RegexHtmlColor`: a quoted lower-case colour name.
static HTML_COLOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<font color="[a-z]*">"#).expect("valid regex"));
/// `RegexHtmlColor2`: an unquoted lower-case colour name.
static HTML_COLOR_2: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<font color=[a-z]*>").expect("valid regex"));
/// `RegexHtmlColor3`: a quoted `#` hex colour (.NET's `\d` is any Unicode digit, as here).
static HTML_COLOR_3: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r##"<font color="#[ABCDEFabcdef\d]*">"##).expect("valid regex"));
/// `RegexHtmlColor4`: a quoted colour name in any case.
static HTML_COLOR_4: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<font color="[A-Za-z]*">"#).expect("valid regex"));

/// Serializes `subtitle` to WebVTT text.
///
/// Port of `WebVTT.ToText`.
#[must_use]
pub fn to_text(subtitle: &Subtitle) -> String {
    let webvtt_header = subtitle.header.starts_with("WEBVTT");
    let mut out = String::new();
    out.push_str(if webvtt_header {
        subtitle.header.trim()
    } else {
        "WEBVTT"
    });
    out.push_str(NEWLINE);
    out.push_str(NEWLINE);
    for p in &subtitle.paragraphs {
        let mut position = position_info_from_ass_tag(p);
        if webvtt_header && let Some(region) = p.region.as_deref().filter(|r| !r.is_empty()) {
            let with_region = format!(" region:{region} {position}").replace("  ", " ");
            with_region.trim_end().clone_into(&mut position);
        }
        let _ = write!(
            out,
            "{} --> {}{position}{NEWLINE}{}{NEWLINE}{NEWLINE}",
            format_timecode(p.start_time),
            format_timecode(p.end_time),
            format_text(p)
        );
    }
    out.trim().to_owned()
}

/// `{0:00}:{1:00}:{2:00}.{3:000}` of `TimeCode.Hours/Minutes/Seconds/Milliseconds`:
/// hours do not wrap at 24.
fn format_timecode(tc: TimeCode) -> String {
    format!(
        "{}:{}:{}.{}",
        dotnet_int(tc.hours(), 2),
        dotnet_int(tc.minutes(), 2),
        dotnet_int(tc.seconds(), 2),
        dotnet_int(tc.milliseconds(), 3)
    )
}

/// `WebVTT.GetPositionInfoFromAssTag`: the cue settings for the `{\anN` the text
/// starts with (bottom centre otherwise), with a leading space when non-empty.
fn position_info_from_ass_tag(p: &Paragraph) -> String {
    let setting = CUE_SETTINGS
        .iter()
        .find(|(tag, _)| p.text.starts_with(tag))
        .map_or("", |(_, setting)| setting);
    format!(" {setting}").trim_end().to_owned()
}

/// `WebVTT.FormatText`.
fn format_text(p: &Paragraph) -> String {
    let text = remove_ssa_tags(&p.text);
    let text = remove_recursive_line_breaks(&text);
    let text = color_html_to_webvtt(&text);
    escape_encode_text(&text)
}

/// Replaces `m` in `res` with `replacement` and returns the new text.
fn splice(res: &str, start: usize, end: usize, replacement: &str) -> String {
    format!("{}{replacement}{}", &res[..start], &res[end..])
}

/// `fontString.Trim('"').Trim('\'')`.
fn trim_quotes(s: &str) -> String {
    s.trim_matches('"').trim_matches('\'').to_owned()
}

/// `WebVTT.ColorHtmlToWebVtt`: `</font>` to `</c>`, and each `<font color>` to a
/// `<c.…>` class, twice over: a lower-case name as itself, a hex colour as the
/// nearest of [`COLOR_CLASSES`] (else `c.color<hex>`), and another known colour name
/// through its `#rrggbbaa` hex.
fn color_html_to_webvtt(text: &str) -> String {
    let mut res = text.replace("</font>", "</c>");
    for _ in 0..2 {
        while let Some(m) = HTML_COLOR.find(&res) {
            let value = &m.as_str()[13..m.as_str().len() - 2];
            let font = trim_quotes(&format!("<c.{value}>"));
            res = splice(&res, m.start(), m.end(), &font);
        }
        while let Some(m) = HTML_COLOR_2.find(&res) {
            let value = &m.as_str()[12..m.as_str().len() - 1];
            let font = trim_quotes(&format!("<c.{value}>"));
            res = splice(&res, m.start(), m.end(), &font);
        }
        while let Some(m) = HTML_COLOR_3.find(&res) {
            let tag = &m.as_str()[14..m.as_str().len() - 2];
            let font = close_color(tag)
                .map_or_else(|| format!("<c.color{tag}>"), |close| format!("<c.{close}>"));
            res = splice(&res, m.start(), m.end(), &trim_quotes(&font));
        }
        while let Some(m) = HTML_COLOR_4.find(&res) {
            let name = &m.as_str()[13..m.as_str().len() - 2];
            let Some(color) = Color::from_known_name(name) else {
                break;
            };
            let tag = format!(
                ".{}",
                color_to_hex_with_transparency(color).trim_start_matches('#')
            );
            let font = close_color(&tag)
                .map_or_else(|| format!("<c{tag}>"), |close| format!("<c.{close}>"));
            res = splice(&res, m.start(), m.end(), &trim_quotes(&font));
        }
    }
    res
}

/// `WebVTT.GetCloseColor`: the first colour class within 25 of `tag` on every
/// channel, read through `ColorTranslator.FromHtml("#" + tag)`.
fn close_color(tag: &str) -> Option<&'static str> {
    let c = color_from_html(&format!("#{}", tag.trim_matches(['#', '.']))).ok()?;
    COLOR_CLASSES.iter().find_map(|&(name, (r, g, b))| {
        let near = |a: u8, b: u8| a.abs_diff(b) <= 25;
        (near(r, c.r) && near(g, c.g) && near(b, c.b)).then_some(name)
    })
}

/// `WebVTT.EscapeEncodeText`: keeps the tags WebVTT knows and the character
/// references that look like one, and escapes every other `<`, `>` and `&`.
fn escape_encode_text(input: &str) -> String {
    if !input.contains(['<', '>', '&']) {
        return input.to_owned();
    }
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut tag_on = false;
    let mut i = 0;
    let starts = |i: usize, prefix: &str| -> bool {
        let n = prefix.chars().count();
        i + n <= chars.len()
            && chars[i..i + n]
                .iter()
                .zip(prefix.chars())
                .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
    };
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '<' => {
                if ["<i>", "<b>", "<u>", "<c>", "<v>"]
                    .iter()
                    .any(|t| starts(i, t))
                {
                    out.extend(&chars[i..i + 3]);
                    i += 3;
                } else if starts(i, "</") {
                    out.push_str("</");
                    i += 2;
                    tag_on = true;
                } else if [
                    "<ruby", "<font", "<v.", "<v ", "<c.", "<c ", "<lang.", "<lang ",
                ]
                .iter()
                .any(|t| starts(i, t))
                {
                    out.push(ch);
                    i += 1;
                    tag_on = true;
                } else {
                    out.push_str("&lt;");
                    i += 1;
                }
            }
            '>' => {
                out.push_str(if tag_on { ">" } else { "&gt;" });
                tag_on = false;
                i += 1;
            }
            '&' => {
                let rest = &chars[i..];
                let letter = |j: usize| rest.get(j).copied().is_some_and(is_letter);
                let known = ["&lrm;", "&amp;", "&lt;", "&gt;"]
                    .iter()
                    .any(|t| starts(i, t))
                    || (rest.len() > 3 && rest[3] == ';' && letter(2) && letter(1))
                    || (rest.len() > 4 && rest[4] == ';' && letter(3) && letter(2) && letter(1));
                out.push_str(if known { "&" } else { "&amp;" });
                i += 1;
            }
            _ => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
}

// ---- loader: libse 4.0.12 `Nikse.SubtitleEdit.Core.SubtitleFormats.WebVTT` ----

/// `WebVTT.RegexTimeCodes`: `hh:mm:ss.fff --> hh:mm:ss.fff`.
static TIME_CODES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^-?\d+:-?\d+:-?\d+\.-?\d+\s*-->\s*-?\d+:-?\d+:-?\d+\.-?\d+")
        .expect("WebVTT timing regex is valid")
});
/// `WebVTT.RegexTimeCodesMiddle`: a start without hours, an end with them.
static TIME_CODES_MIDDLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^-?\d+:-?\d+\.-?\d+\s*-->\s*-?\d+:-?\d+:-?\d+\.-?\d+")
        .expect("WebVTT timing regex is valid")
});
/// `WebVTT.RegexTimeCodesShort`: neither has hours.
static TIME_CODES_SHORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^-?\d+:-?\d+\.-?\d+\s*-->\s*-?\d+:-?\d+\.-?\d+")
        .expect("WebVTT timing regex is valid")
});

/// libse's `SubtitleSettings.WebVttTimescale` default: the MPEG-TS clock of an
/// `X-TIMESTAMP-MAP` (90 kHz, the MPEG-TS standard).
const MPEGTS_TIMESCALE: f64 = 90_000.0;

/// Parses WebVTT `lines` into `subtitle`, returning the number of errors.
///
/// Port of `WebVTT.LoadSubtitle` with libse's default settings
/// (`WebVttUseXTimestampMap` and `WebVttUseMultipleXTimestampMap` on,
/// `WebVttMergeLinesWithSameText` off). Each cue's `line`/`position` settings
/// become an `{\anN}` tag at the start of its text (`GetPositionInfo`), and the
/// raw settings, the region and the header blocks are kept.
///
/// # Errors
///
/// An `X-TIMESTAMP-MAP` whose `LOCAL` time does not parse, which upstream
/// throws out of the load.
#[allow(clippy::too_many_lines)] // a literal port of libse's single-loop loader
pub fn load(subtitle: &mut Subtitle, lines: &[String]) -> Result<i32, String> {
    let mut errors = 0;
    let mut p: Option<Paragraph> = None;
    let mut position_info = String::new();
    let mut had_empty_line = false;
    let mut numbers = 0_usize;
    let mut add_seconds = 0.0_f64;
    let (mut note_on, mut style_on, mut region_on) = (false, false, false);
    let mut header = format!("WEBVTT{NEWLINE}{NEWLINE}");
    let blank_lines = format!("{NEWLINE}{NEWLINE}");

    for (index, line) in lines.iter().enumerate() {
        let line = line.as_str();
        let next = lines.get(index + 1).map_or("", String::as_str);
        let is_next_time_code = next.contains("-->");

        if index == 0 && line.starts_with("WEBVTT") {
            header = format!("{line}{NEWLINE}{NEWLINE}");
            continue;
        }

        if index > 0 && lines[index - 1].is_empty() && (line == "NOTE" || line.starts_with("NOTE "))
        {
            note_on = true;
            if subtitle.paragraphs.is_empty() {
                header.push_str(&blank_lines);
            }
        } else if (line == "STYLE" || line.starts_with("STYLE ")) && subtitle.paragraphs.is_empty()
        {
            style_on = true;
            header.push_str(&blank_lines);
        } else if (line == "REGION" || line.starts_with("REGION "))
            && subtitle.paragraphs.is_empty()
        {
            region_on = true;
            header.push_str(&blank_lines);
        }

        if (style_on || region_on) && !line.is_empty() {
            header.push_str(line);
            header.push_str(NEWLINE);
            continue;
        }
        if note_on && !line.is_empty() {
            if subtitle.paragraphs.is_empty() {
                header.push_str(line);
                header.push_str(NEWLINE);
            }
            continue;
        }

        if index > 1 {
            if is_x_timestamp_map(line) {
                add_seconds = x_timestamp_seconds(line)?;
                continue;
            }
            if line == "WEBVTT" {
                // badly formatted web vtt file
                continue;
            }
        }

        note_on = false;
        style_on = false;
        region_on = false;

        let mut s = line.to_owned();
        let is_time_code = line.contains("-->");
        if is_time_code && TIME_CODES_MIDDLE.is_match(&s) {
            s = format!("00:{s}"); // start is without hours, end is with hours
        }
        if is_time_code && TIME_CODES_SHORT.is_match(&s) {
            s = format!("00:{}", s.replace("--> ", "--> 00:"));
        }

        if is_next_time_code && is_number(&s) && p.as_ref().is_some_and(|p| !p.text.is_empty()) {
            numbers += 1;
        } else if index == 1 && is_x_timestamp_map(&s) {
            add_seconds = x_timestamp_seconds(&s)?;
        } else if is_time_code && TIME_CODES.is_match(s.trim_start()) {
            if let Some(done) = p.take() {
                push_paragraph(subtitle, done);
            }
            match cue_times(&s) {
                Some((start, end)) => {
                    // `TotalMilliseconds += addSeconds * 1000`, on libse's double.
                    let shift = |time: TimeCode| {
                        TimeCode::from_milliseconds(
                            time.total_milliseconds() + add_seconds * 1000.0,
                        )
                    };
                    let mut paragraph = Paragraph::new(String::new(), shift(start), shift(end));
                    position_info_tag(&s).clone_into(&mut position_info);
                    paragraph.style = Some(position_info_raw(&s).to_owned());
                    paragraph.region = tag(&s, "region:").map(str::to_owned);
                    p = Some(paragraph);
                }
                None => {
                    errors += 1;
                }
            }
            had_empty_line = false;
        } else if p.is_some()
            && had_empty_line
            && (TIME_CODES_MIDDLE.is_match(next)
                || TIME_CODES_SHORT.is_match(next)
                || TIME_CODES.is_match(next))
        {
            // can both be number or an "identifier" which can be text
            numbers += 1;
        } else if let Some(paragraph) = p.as_mut() {
            let text = format!("{position_info}{}", line.trim());
            if text.is_empty() {
                had_empty_line = true;
            }
            paragraph.text.push_str(&text);
            paragraph.text.push_str(NEWLINE);
            position_info.clear();
        }
    }

    if let Some(done) = p {
        push_paragraph(subtitle, done);
    }

    // let format WebVTTFileWithLineNumber take the subtitle
    let hand_off = subtitle.paragraphs.len() > 3
        && numbers >= subtitle.paragraphs.len() - 1
        && lines.first().map(String::as_str) == Some("WEBVTT FILE");

    // libse returns before this on a hand-off. It runs anyway here: until
    // WebVTTFileWithLineNumber is ported (gl-768) these cues are the ones
    // served, and they must be decoded and numbered; once it is, the parser
    // drops this result for its clean one.
    for paragraph in &mut subtitle.paragraphs {
        paragraph.text = remove_weird_repeating_header(&escape_decode_text(&paragraph.text));
    }
    subtitle.renumber();
    header
        .replace(&blank_lines, NEWLINE)
        .trim()
        .clone_into(&mut subtitle.header);
    if hand_off {
        return Ok(i32::try_from(subtitle.paragraphs.len()).unwrap_or(i32::MAX - 1) + 1);
    }
    Ok(errors)
}

/// `p.Text = p.Text.TrimEnd(); subtitle.Paragraphs.Add(p)`.
fn push_paragraph(subtitle: &mut Subtitle, mut paragraph: Paragraph) {
    paragraph.text.truncate(paragraph.text.trim_end().len());
    subtitle.paragraphs.push(paragraph);
}

/// A line that starts `X-TIMESTAMP-MAP=` and names `MPEGTS:` after it, both
/// ignoring case.
fn is_x_timestamp_map(line: &str) -> bool {
    const PREFIX: &str = "X-TIMESTAMP-MAP=";
    line.get(..PREFIX.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(PREFIX))
        && line.to_ascii_uppercase().contains("MPEGTS:")
}

/// A cue's start and end, from its timing line: the split on `-->` then
/// `WebVTT.GetTimeCodeFromString` on each half. `None` is upstream's caught
/// exception (a missing half or field, or a number that is not an `int`).
fn cue_times(s: &str) -> Option<(TimeCode, TimeCode)> {
    let replaced = s.trim_start().replace("-->", "@");
    let mut parts = replaced.split('@').filter(|part| !part.is_empty());
    Some((
        time_code_from_string(parts.next()?)?,
        time_code_from_string(parts.next()?)?,
    ))
}

/// `WebVTT.GetTimeCodeFromString`: `hh:mm:ss.fff`, split on `:`, `.` and a
/// space, whose first four fields are hours, minutes, seconds and
/// milliseconds (`int.Parse` each, so `20.84` is 84 milliseconds), then
/// `new TimeCode(hours, minutes, seconds, milliseconds)`.
fn time_code_from_string(time: &str) -> Option<TimeCode> {
    let fields: Vec<&str> = time.trim().split([':', '.', ' ']).collect();
    let field = |i: usize| fields.get(i)?.trim().parse::<i32>().ok();
    Some(TimeCode::from_hms(
        field(0)?,
        field(1)?,
        field(2)?,
        field(3)?,
    ))
}

/// `WebVTT.GetXTimeStampSeconds`: the seconds an `X-TIMESTAMP-MAP` shifts every
/// later cue by, `MPEGTS / 90000 - LOCAL`, or 0 when that is not in
/// `(0, 90000)`.
///
/// # Errors
///
/// A `LOCAL` time with three or four fields that do not all parse
/// (`DecodeTimeCodeMsFourParts`' `int.Parse` throws upstream).
fn x_timestamp_seconds(input: &str) -> Result<f64, String> {
    let chars: Vec<char> = input.chars().filter(|c| *c != ' ').collect();
    let upper: Vec<char> = chars.iter().map(char::to_ascii_uppercase).collect();
    let index_of = |needle: &str| -> Option<usize> {
        let needle: Vec<char> = needle.chars().collect();
        upper
            .windows(needle.len())
            .position(|w| w == needle.as_slice())
    };
    // `IndexOf` answers -1 when there is no `LOCAL:`, and the scan then starts
    // at index 5, as upstream's does.
    let local_from = index_of("LOCAL:").map_or(5, |i| i + 6);
    let local: String = chars
        .iter()
        .skip(local_from)
        .take_while(|c| c.is_numeric() || **c == ':' || **c == '.')
        .collect();
    let mut subtract_seconds = 0.0;
    let mut fields: Vec<String> = local.split([':', '.']).map(str::to_owned).collect();
    if fields.len() == 3 {
        fields = format!("00:{local}")
            .split([':', '.'])
            .map(str::to_owned)
            .collect();
    }
    if fields.len() == 4 {
        let mut parsed = [0_i64; 4];
        for (slot, field) in parsed.iter_mut().zip(&fields) {
            *slot = field
                .trim()
                .parse::<i32>()
                .map(i64::from)
                .map_err(|_| format!("X-TIMESTAMP-MAP LOCAL time does not parse: {local}"))?;
        }
        let ms = parsed[0] * 3_600_000 + parsed[1] * 60_000 + parsed[2] * 1000 + parsed[3];
        #[allow(clippy::cast_precision_loss)]
        {
            subtract_seconds = ms as f64 / 1000.0;
        }
    }
    let ts_from = index_of("MPEGTS:").map_or(6, |i| i + 7);
    let ts: String = chars
        .iter()
        .skip(ts_from)
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if let Ok(number) = ts.parse::<i64>() {
        #[allow(clippy::cast_precision_loss)]
        let seconds = number as f64 / MPEGTS_TIMESCALE - subtract_seconds;
        // max 25 hours - or wrong timescale
        if seconds > 0.0 && seconds < 90_000.0 {
            return Ok(seconds);
        }
    }
    Ok(0.0)
}

/// `WebVTT.GetTag`: the value after `tag` up to the next space, or up to and
/// including the `%` of a `%,` pair.
fn tag<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let at = s.find(tag)?;
    let mut v = s[at + tag.len()..].trim();
    if let Some(end) = v.find("%,") {
        v = &v[..=end];
    }
    if let Some(end) = v.find(' ') {
        v = &v[..end];
    }
    Some(v)
}

/// .NET `double.TryParse(s, NumberStyles.AllowDecimalPoint, Invariant)`:
/// digits with at most one `.`, nothing else.
fn parse_decimal(s: &str) -> Option<f64> {
    let digits = s.chars().filter(char::is_ascii_digit).count();
    let points = s.chars().filter(|c| *c == '.').count();
    (digits > 0 && points <= 1 && digits + points == s.chars().count())
        .then(|| s.parse().ok())
        .flatten()
}

/// `WebVTT.GetPositionInfo`: the `{\anN}` alignment tag a cue's `position:`
/// (horizontal: under 25 % left, over 75 % right) and `line:` (vertical: under
/// 25 % or lines 0–7 top, under 75 % or lines 8–10 middle) settings stand for,
/// or nothing for bottom centre.
fn position_info_tag(s: &str) -> &'static str {
    let percent = |value: &str| {
        value
            .ends_with('%')
            .then(|| parse_decimal(value.trim_end_matches('%')))
            .flatten()
    };
    let (mut left, mut right, mut top, mut middle) = (false, false, false, false);
    if let Some(number) = tag(s, "position:").and_then(percent) {
        left = number < 25.0;
        right = number > 75.0;
    }
    if let Some(line) = tag(s, "line:").map(str::trim) {
        if line.ends_with('%') {
            if let Some(number) = percent(line) {
                top = number < 25.0;
                middle = !top && number < 75.0;
            }
        } else if let Some(number) = parse_decimal(line) {
            top = (0.0..=7.0).contains(&number); // Positive numbers indicate top down
            middle = number > 7.0 && number < 11.0;
        }
    }
    match (left, right, top, middle) {
        (true, _, true, _) => "{\\an7}",
        (true, _, _, true) => "{\\an4}",
        (true, ..) => "{\\an1}",
        (_, true, true, _) => "{\\an9}",
        (_, true, _, true) => "{\\an6}",
        (_, true, ..) => "{\\an3}",
        (_, _, true, _) => "{\\an8}",
        (_, _, _, true) => "{\\an5}",
        _ => "",
    }
}

/// `WebVTT.GetPositionInfoRaw`: the timing line from its first `line:`,
/// `align:`, `position:` or `size:` setting on, or nothing.
fn position_info_raw(s: &str) -> &str {
    ["line:", "align:", "position:", "size:"]
        .iter()
        .filter_map(|key| s.find(key))
        .min()
        .map_or("", |at| &s[at..])
}

/// `WebVTT.EscapeDecodeText`.
fn escape_decode_text(input: &str) -> String {
    input
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}

/// `WebVTT.RemoveWeirdRepeatingHeader`: extra spaces are collapsed, and a
/// `WEBVTT` or `STYLE` block that leaked into a cue's text is cut off. (Its
/// middle arm, text ending in a `WEBVTT` line, is unreachable after the first.)
fn remove_weird_repeating_header(input: &str) -> String {
    let text = fix_extra_spaces(input);
    let webvtt = format!("{NEWLINE}WEBVTT");
    let style = format!("{NEWLINE}STYLE{NEWLINE}");
    let cut = if text.contains(&webvtt) {
        (text.trim_end().ends_with('}') && text.contains("STYLE"))
            .then(|| text.find(&webvtt))
            .flatten()
    } else if text.contains(&style) {
        text.trim_end()
            .ends_with('}')
            .then(|| text.find(&style))
            .flatten()
    } else {
        None
    };
    match cut {
        Some(at) => text[..at].trim().to_owned(),
        None => text,
    }
}

/// libse `StringExtensions.FixExtraSpaces`: a run of spaces becomes one, or
/// none beside a line break.
fn fix_extra_spaces(input: &str) -> String {
    let mut s: Vec<char> = input.chars().collect();
    let mut k: Option<usize> = None;
    let mut i = s.len();
    while i > 0 {
        i -= 1;
        let ch = s[i];
        match k {
            // `k < 2`: no run open, or one that reaches the start.
            None | Some(0 | 1) => {
                if ch == ' ' {
                    k = Some(i + 1);
                }
            }
            Some(end) if ch != ' ' => {
                // only keep white space if it doesn't succeed/precede CRLF
                let by_break = ch == '\n'
                    || ch == '\r'
                    || s.get(end).is_some_and(|c| *c == '\n' || *c == '\r');
                let keep = if by_break { 1 } else { 2 };
                if end > i + keep {
                    s.drain(i + 1..i + 1 + (end - (i + keep)));
                }
                k = None;
            }
            Some(_) => {}
        }
    }
    s.into_iter().collect()
}

/// libse `Utilities.RegexIsNumber` / `RegexIsEpisodeNumber`.
static IS_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+$|^\d+x\d+$").expect("number regex is valid"));

/// libse `Utilities.IsNumber`: digits, or `NxN`, once `$£¥%*` are trimmed.
fn is_number(s: &str) -> bool {
    IS_NUMBER.is_match(s.trim_matches(['$', '£', '¥', '%', '*']))
}

#[cfg(test)]
mod tests {
    use super::{TimeCode, fix_extra_spaces, position_info_tag, time_code_from_string};

    /// libse `StringExtensionsTest.FixExtraSpaces` .. `FixExtraSpaces6`.
    #[rstest::rstest]
    #[case("Hallo  world!", "Hallo world!")]
    #[case("Hallo   world!", "Hallo world!")]
    #[case("Hallo world!  ", "Hallo world! ")]
    #[case("Hallo \n world!", "Hallo\nworld!")]
    #[case("a  \nb", "a\nb")]
    #[case("a\n   b", "a\nb")]
    fn fix_extra_spaces_matches_libse(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(fix_extra_spaces(input), expected);
    }

    #[rstest::rstest]
    #[case("00:00:01.000 --> 00:00:02.000 line:0", "{\\an8}")]
    #[case("00:00:01.000 --> 00:00:02.000 line:9", "{\\an5}")]
    #[case("00:00:01.000 --> 00:00:02.000 line:-1", "")]
    #[case("00:00:01.000 --> 00:00:02.000 line:50% position:20%", "{\\an4}")]
    #[case(
        "00:00:01.000 --> 00:00:02.000 position:80%,line-left line:5%",
        "{\\an9}"
    )]
    #[case("00:00:01.000 --> 00:00:02.000 position:10%", "{\\an1}")]
    #[case("00:00:01.000 --> 00:00:02.000 align:middle", "")]
    fn position_info_is_the_alignment_tag_of_line_and_position(
        #[case] timing: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(position_info_tag(timing), expected);
    }

    #[test]
    fn a_time_code_is_four_int_fields() {
        let ms = |time: &str| time_code_from_string(time).map(TimeCode::total_milliseconds);
        assert_eq!(ms(" 01:02:03.045 "), Some(3_723_045.0));
        // `int.Parse` of the fraction: `.84` is 84 milliseconds.
        assert_eq!(ms("00:00:20.84"), Some(20_084.0));
        assert_eq!(ms("00:00:20"), None);
        assert_eq!(ms("00:00:99999999999.000"), None);
    }
}
