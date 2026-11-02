//! SubStation Alpha (`.ssa`) and Advanced SubStation Alpha (`.ass`) parsers and
//! writers.
//!
//! Hand-port of libse 4.0.12's `Nikse.SubtitleEdit.Core.SubtitleFormats.SubStationAlpha`
//! and `AdvancedSubStationAlpha` (`libse` has no Rust equivalent): the
//! `LoadSubtitle` paths Jellyfin's `SubtitleEditParser` drives and the `ToText`
//! writers Jellyfin 12's `SubtitleEncoder.TryGetWriter` returns.
//!
//! The encoder serves an ASS or SSA source verbatim for an ASS request, and an SSA
//! source verbatim for an SSA request, so the ASS writer sees a source without a
//! script header (SubRip) or with a WebVTT one, and the SSA writer one without a
//! header, with a WebVTT one (which it treats as none, as libse does) or with an ASS
//! header.
// TODO(gl-816): the ASS writer sends a WebVTT header through libse's
// `WebVttToAssa.Convert` (the source's `STYLE` classes become ASS styles and inline
// tags, every event its own layer); it takes the default-header branch here instead.
// TODO(gl-768): both writers also read a TTML source header
// (`LoadStylesFromTimedText10`); port that branch with the TTML loader that reaches it.

use super::dotnet::int_try_parse;
use super::model::{Paragraph, Subtitle, TimeCode, dotnet_int};
use super::srt::NEWLINE;
use super::ssa_style::{
    DEFAULT_ASS_STYLE_FORMAT, SsaStyle, ssa_style_from_header, styles_from_header,
};
use super::ssa_tags::{format_text, get_formatted_text, normalize_new_lines};
use std::fmt::Write as _;

/// The `[Fonts]`/`[Graphics]`/`[Aegisub Extradata]` section markers both writers
/// look for before carrying a source footer over.
fn footer_is_carried(footer: &str) -> bool {
    ["[Fonts]", "[Graphics]", "[Aegisub Extradata]"]
        .iter()
        .any(|section| footer.contains(&format!("{section}{NEWLINE}")))
}

/// Which libse loader [`load`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// `SubStationAlpha.LoadSubtitle`.
    Ssa,
    /// `AdvancedSubStationAlpha.LoadSubtitle`.
    Ass,
}

/// Parses ASS `lines` into `subtitle`, returning the number of errors.
///
/// Port of `AdvancedSubStationAlpha.LoadSubtitle`: the header is every line before
/// the events, the `[Fonts]`/`[Graphics]`/`[Aegisub Extradata]` sections are the
/// footer, and each event keeps its style, actor, margins, effect and layer. The text
/// keeps its override tags; only `\N`/`\n` become line breaks.
pub fn load_ass(subtitle: &mut Subtitle, lines: &[String]) -> i32 {
    load(subtitle, lines, Variant::Ass)
}

/// Parses SSA `lines` into `subtitle`, returning the number of errors.
///
/// Port of `SubStationAlpha.LoadSubtitle`: as [`load_ass`], but the style, name and
/// effect columns keep their spacing, and the text goes through `GetFormattedText`
/// (override tags to `<i>`/`<b>`/`<u>`/`<font>`).
pub fn load_ssa(subtitle: &mut Subtitle, lines: &[String]) -> i32 {
    load(subtitle, lines, Variant::Ssa)
}

/// The column indexes of an event line.
struct EventColumns {
    layer: Option<usize>,
    start: Option<usize>,
    end: Option<usize>,
    style: Option<usize>,
    actor: Option<usize>,
    name: Option<usize>,
    margin_l: Option<usize>,
    margin_r: Option<usize>,
    margin_v: Option<usize>,
    effect: Option<usize>,
    text: Option<usize>,
}

impl EventColumns {
    /// The defaults both loaders start from: `Layer, Start, End, Style, Name,
    /// MarginL, MarginR, MarginV, Effect, Text`.
    fn defaults() -> Self {
        Self {
            layer: Some(0),
            start: Some(1),
            end: Some(2),
            style: Some(3),
            actor: None,
            name: Some(4),
            margin_l: Some(5),
            margin_r: Some(6),
            margin_v: Some(7),
            effect: Some(8),
            text: Some(9),
        }
    }
}

#[allow(clippy::too_many_lines)] // one loop, kept in libse's order
fn load(subtitle: &mut Subtitle, lines: &[String], variant: Variant) -> i32 {
    let mut errors = 0;
    let mut events_started = false;
    let mut fonts_started = false;
    let mut graphics_started = false;
    subtitle.paragraphs.clear();
    let mut columns = EventColumns::defaults();
    let mut header = String::new();
    let mut footer = String::new();

    for line in lines {
        let trimmed = line.trim();
        let section = |name: &str| trimmed.eq_ignore_ascii_case(name);
        match variant {
            Variant::Ass => {
                if !events_started
                    && !fonts_started
                    && !graphics_started
                    && !section("[fonts]")
                    && !section("[graphics]")
                {
                    header.push_str(line);
                    header.push_str(NEWLINE);
                }
                let start = line.trim_start().to_lowercase();
                if !(line.trim().is_empty() || line.trim_start().starts_with(';'))
                    && (start.starts_with("dialog:") || start.starts_with("dialogue:"))
                {
                    events_started = true;
                    fonts_started = false;
                    graphics_started = false;
                }
            }
            Variant::Ssa => {
                if !events_started {
                    header.push_str(line);
                    header.push_str(NEWLINE);
                }
                if !line.is_empty() && line.trim_start().starts_with(';') {
                    continue;
                }
            }
        }

        if section("[events]") {
            if variant == Variant::Ass
                && !header
                    .to_lowercase()
                    .contains(&format!("{NEWLINE}[events]"))
            {
                let h = header.trim_end().to_owned();
                header = format!("{h}{NEWLINE}{NEWLINE}[Events]{NEWLINE}");
            }
            events_started = true;
            fonts_started = false;
            graphics_started = false;
        } else if section("[fonts]") {
            events_started = false;
            fonts_started = true;
            graphics_started = false;
            let _ = write!(footer, "{NEWLINE}[Fonts]{NEWLINE}");
        } else if section("[graphics]") {
            events_started = false;
            fonts_started = false;
            graphics_started = true;
            let _ = write!(footer, "{NEWLINE}[Graphics]{NEWLINE}");
        } else if variant == Variant::Ass && section("[Aegisub Extradata]") {
            events_started = false;
            fonts_started = false;
            graphics_started = true;
            let _ = write!(footer, "{NEWLINE}[Aegisub Extradata]{NEWLINE}");
        } else if variant == Variant::Ass && (fonts_started || graphics_started) {
            footer.push_str(line);
            footer.push_str(NEWLINE);
        } else if events_started && !(variant == Variant::Ssa && line.trim().is_empty()) {
            let s = line.trim().to_lowercase();
            if line.chars().count() > 10 && s.starts_with("format:") {
                read_format(&mut columns, &s, variant);
            } else if variant == Variant::Ssa && (fonts_started || graphics_started) {
                footer.push_str(line);
                footer.push_str(NEWLINE);
            } else if !s.is_empty() {
                match read_event(line, &s, &columns, variant) {
                    Some(paragraph) => subtitle.paragraphs.push(paragraph),
                    None => errors += 1,
                }
            }
        }
    }

    if !header.is_empty() {
        subtitle.header = header;
    }
    if !footer.is_empty() {
        footer.trim().clone_into(&mut subtitle.footer);
    }
    number_cues(subtitle);
    errors
}

/// libse's `Renumber()`, but counting only the cues that are not `Comment:` events
/// (those are numbered 0). The encoder writes comments back only into SSA and ASS,
/// which print no numbers, and drops them for every other writer, whose numbers then
/// match libse's for the same script without its comments.
fn number_cues(subtitle: &mut Subtitle) {
    let mut number = 0;
    for p in &mut subtitle.paragraphs {
        if p.is_comment {
            p.number = 0;
        } else {
            number += 1;
            p.number = number;
        }
    }
}

/// An events-section `Format:` line: the ASS loader resets every index first, the
/// SSA loader only overrides the ones it names (and never moves `Name`).
fn read_format(columns: &mut EventColumns, lower_line: &str, variant: Variant) {
    if variant == Variant::Ass {
        *columns = EventColumns {
            layer: None,
            start: None,
            end: None,
            style: None,
            actor: None,
            name: None,
            margin_l: None,
            margin_r: None,
            margin_v: None,
            effect: None,
            text: None,
        };
    }
    for (i, f) in lower_line.get(8..).unwrap_or("").split(',').enumerate() {
        let slot = match f.trim() {
            "start" => &mut columns.start,
            "end" => &mut columns.end,
            "text" => &mut columns.text,
            "style" => &mut columns.style,
            "actor" if variant == Variant::Ass => &mut columns.actor,
            "name" if variant == Variant::Ass => &mut columns.name,
            "marginl" => &mut columns.margin_l,
            "marginr" => &mut columns.margin_r,
            "marginv" => &mut columns.margin_v,
            "effect" => &mut columns.effect,
            "layer" => &mut columns.layer,
            _ => continue,
        };
        *slot = Some(i);
    }
}

/// One event line to a paragraph, or `None` where libse counts an error.
fn read_event(
    line: &str,
    lower: &str,
    columns: &EventColumns,
    variant: Variant,
) -> Option<Paragraph> {
    // libse checks the trimmed line but removes the prefix's length from the line
    // itself, counted in characters.
    let skip_chars = |n: usize| line.char_indices().nth(n).map_or("", |(i, _)| &line[i..]);
    let body = if lower.starts_with("dialog:") {
        Some(skip_chars(7))
    } else if lower.starts_with("dialogue:") {
        Some(skip_chars(9))
    } else {
        None
    };
    let body = match (body, variant) {
        // The SSA loader also drops one space after the colon.
        (Some(body), Variant::Ssa) => body.strip_prefix(' ').unwrap_or(body),
        (Some(body), Variant::Ass) => body,
        (None, _) => line,
    };

    let mut text = String::new();
    let (mut start, mut end) = (String::new(), String::new());
    let (mut style, mut actor, mut effect) = (String::new(), String::new(), String::new());
    let (mut margin_l, mut margin_r, mut margin_v) = (String::new(), String::new(), String::new());
    let mut layer = 0;
    let at = |slot: Option<usize>, i: usize| slot == Some(i);
    for (i, field) in body.split(',').enumerate() {
        match variant {
            Variant::Ass => {
                if at(columns.start, i) {
                    field.trim().clone_into(&mut start);
                } else if at(columns.end, i) {
                    field.trim().clone_into(&mut end);
                } else if at(columns.style, i) {
                    field.trim().clone_into(&mut style);
                } else if (at(columns.actor, i) && columns.name.is_none()) || at(columns.name, i) {
                    // `Actor` stands in for `Name` only when there is no `Name` column.
                    field.trim().clone_into(&mut actor);
                } else if at(columns.margin_l, i) {
                    field.trim().clone_into(&mut margin_l);
                } else if at(columns.margin_r, i) {
                    field.trim().clone_into(&mut margin_r);
                } else if at(columns.margin_v, i) {
                    field.trim().clone_into(&mut margin_v);
                } else if at(columns.effect, i) {
                    field.trim().clone_into(&mut effect);
                } else if at(columns.layer, i) {
                    layer = int_try_parse(field.replace("Comment:", "").trim()).unwrap_or(0);
                } else if at(columns.text, i) {
                    field.clone_into(&mut text);
                } else if columns.text.is_none_or(|t| i > t) {
                    // libse's `i > indexText` holds for every field once a `Format:`
                    // line without `Text` has set the index to -1.
                    text.push(',');
                    text.push_str(field);
                }
            }
            Variant::Ssa => {
                if at(columns.start, i) {
                    field.trim().clone_into(&mut start);
                } else if at(columns.end, i) {
                    field.trim().clone_into(&mut end);
                } else if at(columns.layer, i) {
                    layer = int_try_parse(field).unwrap_or(0);
                } else if at(columns.effect, i) {
                    field.clone_into(&mut effect);
                } else if at(columns.text, i) {
                    field.clone_into(&mut text);
                } else if at(columns.style, i) {
                    field.clone_into(&mut style);
                } else if at(columns.margin_l, i) {
                    field.trim().clone_into(&mut margin_l);
                } else if at(columns.margin_r, i) {
                    field.trim().clone_into(&mut margin_r);
                } else if at(columns.margin_v, i) {
                    field.trim().clone_into(&mut margin_v);
                } else if i == 4 {
                    field.clone_into(&mut actor);
                } else if columns.text.is_some_and(|t| i > t) {
                    text.push(',');
                    text.push_str(field);
                }
            }
        }
    }

    let text = match variant {
        Variant::Ass => normalize_new_lines(&text),
        Variant::Ssa => get_formatted_text(&text).ok()?,
    };
    let mut paragraph = Paragraph::new(text, parse_timecode(&start)?, parse_timecode(&end)?);
    let some = |s: String| (!s.is_empty()).then_some(s);
    paragraph.extra = some(style);
    paragraph.actor = some(actor);
    paragraph.margin_l = some(margin_l);
    paragraph.margin_r = some(margin_r);
    paragraph.margin_v = some(margin_v);
    paragraph.effect = some(effect);
    paragraph.layer = layer;
    paragraph.is_comment = lower.starts_with("comment:");
    Some(paragraph)
}

/// `GetTimeCodeFromString`: `h:mm:ss.cc`, each part an `int.Parse` (which throws on
/// anything else), the last in centiseconds.
fn parse_timecode(time: &str) -> Option<TimeCode> {
    let parts: Vec<&str> = time.split([':', '.']).collect();
    let part = |i: usize| parts.get(i).and_then(|p| int_try_parse(p));
    // `int.Parse(...) * 10` is unchecked in C#.
    Some(TimeCode::from_hms(
        part(0)?,
        part(1)?,
        part(2)?,
        part(3)?.wrapping_mul(10),
    ))
}

/// Whether `[Events]`/`Dialogue:` content is present in `lines`.
///
/// Both the SSA and ASS libse formats gate a successful load on finding at
/// least one `Dialogue:` line inside the `[Events]` section.
#[must_use]
pub fn is_ssa_like(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|l| l.trim().to_ascii_lowercase().starts_with("dialogue:"))
}

/// `MakeTimeCode` of the ASS writer: `h:mm:ss.cc`, the hundredths rounded half away
/// from zero, carrying into the seconds at 100.
fn ass_timecode(tc: TimeCode) -> String {
    let mut whole = TimeCode::from_hms(tc.hours(), tc.minutes(), tc.seconds(), tc.milliseconds());
    let mut fragment = (f64::from(tc.milliseconds()) / 10.0).round();
    if fragment >= 100.0 {
        whole = TimeCode::from_hms(whole.hours(), whole.minutes(), whole.seconds() + 1, 0);
        fragment = 0.0;
    }
    #[allow(clippy::cast_possible_truncation)] // 0–99
    let fragment = fragment as i32;
    format!(
        "{}:{}:{}.{}",
        whole.hours(),
        dotnet_int(whole.minutes(), 2),
        dotnet_int(whole.seconds(), 2),
        dotnet_int(fragment, 2)
    )
}

/// The SSA writer's `h:mm:ss.cc`: the hundredths truncated (`Milliseconds / 10`).
fn ssa_timecode(tc: TimeCode) -> String {
    format!(
        "{}:{}:{}.{}",
        tc.hours(),
        dotnet_int(tc.minutes(), 2),
        dotnet_int(tc.seconds(), 2),
        dotnet_int(tc.milliseconds() / 10, 2)
    )
}

/// A margin column as both writers write it: the source value when it is an
/// integer, else `default`; the SSA writer also pads it to four digits.
fn margin(value: Option<&str>, default: &str, pad: bool) -> String {
    match value {
        Some(v) if !v.is_empty() && int_try_parse(v).is_some() => {
            if pad && v.chars().count() < 4 {
                format!("{}{v}", "0".repeat(4 - v.chars().count()))
            } else {
                v.to_owned()
            }
        }
        _ => default.to_owned(),
    }
}

/// The events `Format:` line of the ASS writer.
const ASS_EVENTS_FORMAT: &str =
    "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";

/// The events `Format:` line of the SSA writer.
const SSA_EVENTS_FORMAT: &str =
    "Format: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";

/// Serializes `subtitle` to Advanced SubStation Alpha (v4+) text.
///
/// Port of `AdvancedSubStationAlpha.ToText(subtitle, title)`. A source ASS header is
/// reused; any other source gets libse's default header, whose verbatim string
/// literal carries the CRLF line ends of libse's source file while the lines libse
/// appends end in LF.
#[must_use]
pub fn to_text_ass(subtitle: &Subtitle, title: &str) -> String {
    let mut sb = String::new();
    let is_valid_ass_header = subtitle.header.contains("[V4+ Styles]");
    let styles = if is_valid_ass_header {
        sb.push_str(subtitle.header.trim());
        sb.push_str(NEWLINE);
        sb.push_str(ASS_EVENTS_FORMAT);
        sb.push_str(NEWLINE);
        styles_from_header(&subtitle.header)
    } else {
        // `AssaShowPlayDepth` and `AssaShowScaledBorderAndShadow` are on by default.
        let header = format!(
            "[Script Info]\r\n; This is an Advanced Sub Station Alpha v4+ script.\r\nTitle: {title}\r\nScriptType: v4.00+\
             {NEWLINE}PlayDepth: 0{NEWLINE}ScaledBorderAndShadow: Yes\r\n\r\n[V4+ Styles]\r\n{DEFAULT_ASS_STYLE_FORMAT}\r\n{}\
             \r\n\r\n[Events]\r\n{ASS_EVENTS_FORMAT}",
            SsaStyle::default().to_raw_ass()
        );
        sb.push_str(&header);
        sb.push_str(NEWLINE);
        styles_from_header(&header)
    };

    for p in &subtitle.paragraphs {
        let extra = p.extra.as_deref().unwrap_or("");
        let has = |name: &str| styles.iter().any(|s| s == name);
        let mut style = "Default";
        if (!extra.is_empty() && is_valid_ass_header && has(extra))
            || (!styles.is_empty() && !has(style) && has(extra))
        {
            style = extra;
        } else if !styles.is_empty() && extra.is_empty() {
            style = &styles[0];
        }
        let text = format_text(&p.text.replace(NEWLINE, "\\N"));
        let kind = if p.is_comment { "Comment" } else { "Dialogue" };
        let _ = write!(
            sb,
            "{kind}: {},{},{},{style},{},{},{},{},{},{text}{NEWLINE}",
            p.layer,
            ass_timecode(p.start_time),
            ass_timecode(p.end_time),
            p.actor.as_deref().unwrap_or(""),
            margin(p.margin_l.as_deref(), "0", false),
            margin(p.margin_r.as_deref(), "0", false),
            margin(p.margin_v.as_deref(), "0", false),
            p.effect.as_deref().unwrap_or(""),
        );
    }

    if footer_is_carried(&subtitle.footer) {
        sb.push_str(NEWLINE);
        sb.push_str(&subtitle.footer);
        sb.push_str(NEWLINE);
    }
    format!("{}{NEWLINE}", sb.trim())
}

/// The SSA writer's `HeaderNoStyles`, the header it builds around a source ASS
/// script's styles (a verbatim literal, so CRLF).
fn ssa_header_no_styles(title: &str, styles: &str) -> String {
    format!(
        "[Script Info]\r\n; This is a Sub Station Alpha v4 script.\r\nTitle: {title}\r\nScriptType: v4.00\r\nCollisions: Normal\r\n\
         PlayDepth: 0\r\n\r\n[V4 Styles]\r\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, \
         BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding\r\n\
         {styles}\r\n\r\n[Events]\r\n{SSA_EVENTS_FORMAT}"
    )
}

/// `SubStationAlpha.GetStyle(header)`: every style of an ASS header as an SSA v4
/// `Style:` line (colours as `ToArgb` integers, the alignment renumbered).
fn ssa_styles_from_ass_header(header: &str) -> String {
    let mut out = String::new();
    for name in styles_from_header(header) {
        let style = ssa_style_from_header(&name, header);
        let flag = |v: bool| if v { "-1" } else { "0" };
        let alignment = match style.alignment.as_str() {
            "1" => "1",
            "3" => "3",
            "4" => "9",
            "5" => "10",
            "6" => "11",
            "7" => "5",
            "8" => "6",
            "9" => "7",
            _ => "2",
        };
        let _ = write!(
            out,
            "Style: {},{},{},{},{},{},{},{},{},{},{},{},{alignment},{},{},{},0,1{NEWLINE}",
            style.name,
            style.font_name,
            style.font_size.to_hash_format(1),
            style.primary.to_argb(),
            style.secondary.to_argb(),
            style.outline.to_argb(),
            style.background.to_argb(),
            flag(style.bold),
            flag(style.italic),
            style.border_style,
            style.outline_width.to_invariant_string(),
            style.shadow_width.to_invariant_string(),
            style.margin_left,
            style.margin_right,
            style.margin_vertical,
        );
    }
    out
}

/// Serializes `subtitle` to SSA (v4) text.
///
/// Port of `SubStationAlpha.ToText(subtitle, title)`: a source SSA header is reused, a
/// source ASS header has its styles converted to SSA, and any other source gets the
/// default header and style.
#[must_use]
pub fn to_text_ssa(subtitle: &Subtitle, title: &str) -> String {
    let mut sb = String::new();
    let mut is_valid_ssa_header = subtitle.header.contains("[V4 Styles]");
    let mut styles: Vec<String> = Vec::new();
    if is_valid_ssa_header {
        sb.push_str(subtitle.header.trim());
        sb.push_str(NEWLINE);
        if !subtitle.header.contains(SSA_EVENTS_FORMAT) {
            sb.push_str(SSA_EVENTS_FORMAT);
            sb.push_str(NEWLINE);
        }
        styles = styles_from_header(&subtitle.header);
    } else if subtitle.header.contains("[V4+ Styles]") {
        // `LoadStylesFromAdvancedSubstationAlpha`: the converted header replaces the
        // subtitle's own, and its styles are what the events are matched against.
        let converted = ssa_styles_from_ass_header(&subtitle.header);
        if converted.is_empty() {
            // libse formats the source header itself here (`string.Format(header, title)`).
            sb.push_str(&subtitle.header.replace("{0}", title));
        } else {
            sb.push_str(&ssa_header_no_styles(title, &converted));
        }
        sb.push_str(NEWLINE);
        is_valid_ssa_header = true;
        styles = styles_from_header(if converted.is_empty() {
            &subtitle.header
        } else {
            &sb
        });
    } else {
        sb.push_str(&ssa_default_header(title));
        sb.push_str(NEWLINE);
    }

    for p in &subtitle.paragraphs {
        let extra = p.extra.as_deref().unwrap_or("");
        let mut style = "Default";
        if !extra.is_empty() && is_valid_ssa_header && styles.iter().any(|s| s == extra) {
            style = extra;
        }
        if style == "Default" {
            style = "*Default";
        }
        let actor = p
            .actor
            .as_deref()
            .filter(|a| !a.is_empty())
            .unwrap_or("NTP");
        let text = format_text(&p.text.replace(NEWLINE, "\\N"));
        let kind = if p.is_comment { "Comment" } else { "Dialogue" };
        let _ = write!(
            sb,
            "{kind}: Marked={},{},{},{style},{actor},{},{},{},{},{text}{NEWLINE}",
            p.layer,
            ssa_timecode(p.start_time),
            ssa_timecode(p.end_time),
            margin(p.margin_l.as_deref(), "0000", true),
            margin(p.margin_r.as_deref(), "0000", true),
            margin(p.margin_v.as_deref(), "0000", true),
            p.effect.as_deref().unwrap_or(""),
        );
    }

    if footer_is_carried(&subtitle.footer) {
        sb.push_str(NEWLINE);
        sb.push_str(&subtitle.footer);
        sb.push_str(NEWLINE);
    }
    format!("{}{NEWLINE}", sb.trim())
}

/// The SSA writer's default header: its own `Style: Default` line from libse's
/// default style (`GetDefaultStyle`), colours as `ColorTranslator.ToWin32`.
fn ssa_default_header(title: &str) -> String {
    let style = SsaStyle::default();
    format!(
        "[Script Info]\r\n; This is a Sub Station Alpha v4 script.\r\nTitle: {title}\r\nScriptType: v4.00\r\nCollisions: Normal\r\n\
         PlayDepth: 0\r\n\r\n[V4 Styles]\r\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, \
         BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding\r\n\
         Style: Default,{},{},{},{},{},{},{},0,1,{},{},2,{},{},{},0,1\r\n\r\n[Events]\r\n{SSA_EVENTS_FORMAT}",
        style.font_name,
        style.font_size.to_invariant_string(),
        style.primary.to_win32(),
        style.secondary.to_win32(),
        style.tertiary.to_win32(),
        style.background.to_win32(),
        if style.bold { "-1" } else { "0" },
        style.outline_width.to_hash_format(2),
        style.shadow_width.to_hash_format(2),
        style.margin_left,
        style.margin_right,
        style.margin_vertical,
    )
}
