//! WebVTT (`.vtt`) writer.
//!
//! Port of libse 4.0.12's `Nikse.SubtitleEdit.Core.SubtitleFormats.WebVTT.ToText`,
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
