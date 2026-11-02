//! Conversions between SSA/ASS override tags and the HTML-style tags libse keeps
//! cue text in.
//!
//! Ports of libse 4.0.12's `AdvancedSubStationAlpha.GetFormattedText` (override
//! tags to HTML, which `SubStationAlpha.LoadSubtitle` applies to every SSA cue) and
//! `AdvancedSubStationAlpha.FormatText` (HTML to override tags, which both SSA/ASS
//! writers apply). The index arithmetic follows libse's: every search is for an ASCII
//! pattern, so the byte indexes used here fall on the same characters as its UTF-16 ones.

use super::dotnet::{Throws, float_try_parse};
use super::libse_text::html_color_from_string;
use super::srt::NEWLINE;
use std::fmt::Write as _;

/// The override tags `ContainsUnsupportedTags` gives up on: with any of them present,
/// `GetFormattedText` converts only italic, underline and bold.
const UNSUPPORTED_TAGS: &[&str] = &[
    "\\alpha", "\\be0", "\\be1", "\\bord", "\\blur", "\\clip", "\\fad", "\\fa", "\\fade", "\\fscx",
    "\\fscy", "\\fr", "\\iclip", "\\k", "\\K", "\\kf", "\\ko", "\\move", "\\org", "\\p", "\\pos",
    "\\s0", "\\s1", "\\t(", "\\xbord", "\\ybord", "\\xshad", "\\yshad",
];

/// `AdvancedSubStationAlpha.NormalizeNewLines`: `\N` and `\n` to line breaks.
#[must_use]
pub fn normalize_new_lines(text: &str) -> String {
    text.replace("\\N", NEWLINE).replace("\\n", NEWLINE)
}

/// `Utilities.CountTagInText(text, tag)`: non-overlapping occurrences.
fn count_tag(text: &str, tag: &str) -> usize {
    text.matches(tag).count()
}

/// `str.find` from a byte offset.
fn find_from(text: &str, pattern: &str, from: usize) -> Option<usize> {
    text.get(from..)?.find(pattern).map(|i| i + from)
}

/// `AdvancedSubStationAlpha.GetFormattedText`: `\N` to line breaks, then `{\fn…}`,
/// `{\fs…}`, `{\c…}` and `{\1c…}` to `<font>` (unless the cue uses a tag libse does
/// not convert), and `{\i1}`/`{\u1}`/`{\b1}` to `<i>`/`<u>`/`<b>`, closing any left open.
///
/// # Errors
///
/// Returns [`Throws`] where libse throws (a malformed colour tag); the SSA loader
/// then counts the cue as an error and drops it.
pub fn get_formatted_text(input: &str) -> Result<String, Throws> {
    let mut text = normalize_new_lines(input);
    let too_complex = text.contains("{\\") && UNSUPPORTED_TAGS.iter().any(|tag| text.contains(tag));

    if !too_complex {
        for _ in 0..10 {
            if let Some(start) = text.find("{\\i1\\") {
                text.replace_range(start..start + 4, "<i>{");
            }
            convert_font_tag(&mut text, "{\\fn", "{\\fn}", "face", false)?;
            convert_font_tag(&mut text, "{\\fs", "{\\fs}", "size", true)?;
            convert_color_tag(&mut text, "{\\c", "{\\c}", "{\\c&", 4)?;
            convert_color_tag(&mut text, "{\\1c", "{\\1c}", "{\\1c&", 5)?;
        }
    }

    for (open_ass, close_ass, bare_close, open, close) in [
        ("{\\i1}", "{\\i0}", "{\\i}", "<i>", "</i>"),
        ("{\\u1}", "{\\u0}", "{\\u}", "<u>", "</u>"),
        ("{\\b1}", "{\\b0}", "{\\b}", "<b>", "</b>"),
    ] {
        text = text
            .replace(open_ass, open)
            .replace(close_ass, close)
            .replace(bare_close, close);
        if count_tag(&text, open) > count_tag(&text, close) {
            text.push_str(close);
        }
    }
    Ok(text)
}

/// The `{\fn…}` / `{\fs…}` arm of `GetFormattedText`: the first such tag becomes
/// `<font face|size="…">`, closed at its `{\fn}`/`{\fs}`, the next tag of its kind,
/// a `{\c}`, or the end of the text.
fn convert_font_tag(
    text: &mut String,
    open: &str,
    close: &str,
    attribute: &str,
    numeric: bool,
) -> Result<(), Throws> {
    let Some(start) = text.find(open) else {
        return Ok(());
    };
    let Some(end) = find_from(text, "}", start) else {
        return Ok(());
    };
    if end == 0 || text[start..].starts_with(close) {
        return Ok(());
    }
    let mut value = text[start + open.len()..end].to_owned();
    let (extra_tags, unknown_tags, italic) = check_and_add_sub_tags(&mut value)?;
    if numeric && float_try_parse(&value).is_none() {
        return Ok(());
    }
    text.replace_range(start..=end, "");
    let italic_tag = if italic { "<i>" } else { "" };
    text.insert_str(
        start,
        &format!("<font {attribute}=\"{value}\"{extra_tags}>{unknown_tags}{italic_tag}"),
    );
    if let Some(end_tag) = find_from(text, close, start).filter(|&i| i > 0) {
        text.replace_range(end_tag..end_tag + close.len(), "</font>");
    } else {
        let next_same = find_from(text, open, start).filter(|&i| i > 0);
        let next_color_close = find_from(text, "{\\c}", start).filter(|&i| i > 0);
        if let Some(next) = next_same {
            text.insert_str(next, "</font>");
        } else if let Some(next) = next_color_close
            .filter(|&next| find_from(text, "{\\", start).is_some_and(|tag| tag >= next))
        {
            text.insert_str(next, "</font>");
        } else {
            text.push_str("</font>");
        }
    }
    Ok(())
}

/// The `{\c…}` / `{\1c…}` arm of `GetFormattedText`: the first such tag (not `{\clip`)
/// becomes `<font color="#rrggbb">`, closed before the next colour tag, at its
/// closing tag, or at the end of the text.
fn convert_color_tag(
    text: &mut String,
    open: &str,
    close: &str,
    next_color: &str,
    skip: usize,
) -> Result<(), Throws> {
    let Some(start) = text.find(open) else {
        return Ok(());
    };
    let Some(end) = find_from(text, "}", start) else {
        return Ok(());
    };
    if end == 0
        || text[start..].starts_with(close)
        || (open == "{\\c" && text[start..].starts_with("{\\clip"))
    {
        return Ok(());
    }
    // `Substring(start + skip, end - (start + skip))`, which counts characters: the
    // tag's own characters plus the one after it (normally the `&` of `&H`).
    let from = text[start..]
        .char_indices()
        .nth(skip)
        .map_or(text.len(), |(i, _)| start + i);
    let mut color = text.get(from..end).ok_or(Throws)?.to_owned();
    let (extra_tags, unknown_tags, italic) = check_and_add_sub_tags(&mut color)?;
    let color = bgr_to_html(&color);
    text.replace_range(start..=end, "");
    let italic_tag = if italic { "<i>" } else { "" };
    text.insert_str(
        start,
        &format!("<font color=\"{color}\"{extra_tags}>{unknown_tags}{italic_tag}"),
    );
    let end_tag = find_from(text, close, start);
    let next = find_from(text, next_color, start).filter(|&i| i > 0);
    if let Some(next) = next.filter(|&next| end_tag.is_none_or(|end_tag| next < end_tag)) {
        text.insert_str(next, "</font>");
    } else if let Some(end_tag) = end_tag.filter(|&i| i > 0) {
        text.replace_range(end_tag..end_tag + close.len(), "</font>");
    } else {
        text.push_str("</font>");
    }
    Ok(())
}

/// The colour rewrite libse repeats in three places: drop `&`, drop leading `H`s,
/// pad to six digits, swap `bbggrr` to `#rrggbb` (anything before the last six digits
/// stays in front), lower-case.
fn bgr_to_html(color: &str) -> String {
    let color: String = color.chars().filter(|&c| c != '&').collect();
    let color = color.trim_start_matches('H');
    let mut chars: Vec<char> = color.chars().collect();
    while chars.len() < 6 {
        chars.insert(0, '0');
    }
    let n = chars.len();
    let take = |from: usize, len: usize| chars[from..from + len].iter().collect::<String>();
    format!(
        "#{}{}{}{}",
        take(0, n - 6),
        take(n - 2, 2),
        take(n - 4, 2),
        take(n - 6, 2)
    )
    .to_lowercase()
}

/// `CheckAndAddSubTags`: splits `tag` at its first `\`, keeps the head as the tag
/// value, and turns the rest into font attributes (`fs`, `fn`, `c`), an italic flag
/// (`i1`), or unknown tags kept as an override block. Returns
/// `(extra attributes, unknown tags, italic)`, or [`Throws`] where libse's
/// `fontColor.Substring(2)` throws (a bare `c` sub-tag).
fn check_and_add_sub_tags(tag: &mut String) -> Result<(String, String, bool), Throws> {
    let mut italic = false;
    let mut extra_tags = String::new();
    let mut unknown_tags = String::new();
    if let Some(split) = tag.find('\\').filter(|&i| i > 0) {
        let mut rest = tag[split..].trim_start_matches('\\').to_owned();
        tag.truncate(split);
        // Takes the part of `rest` up to its next `\`, leaving what follows.
        let take = |rest: &mut String| -> String {
            match rest.find('\\').filter(|&i| i > 0) {
                Some(i) => {
                    let head = rest[..i].to_owned();
                    *rest = rest[i..].trim_start_matches('\\').to_owned();
                    head
                }
                None => std::mem::take(rest),
            }
        };
        for _ in 0..10 {
            if rest.starts_with("fs") && rest.len() > 2 {
                let font_size = take(&mut rest);
                let _ = write!(extra_tags, " size=\"{}\"", &font_size[2..]);
            } else if rest.starts_with("fn") && rest.len() > 2 {
                let font_name = take(&mut rest);
                let _ = write!(extra_tags, " face=\"{}\"", &font_name[2..]);
            } else if rest.starts_with('c') && rest.chars().count() > 2 {
                let font_color = take(&mut rest);
                if font_color.chars().count() < 2 {
                    return Err(Throws);
                }
                let color = font_color.chars().skip(2).collect::<String>();
                let _ = write!(extra_tags, " color=\"{}\"", bgr_to_html(&color));
            } else if rest.starts_with("i1") {
                italic = true;
                take(&mut rest);
            } else if !rest.is_empty() && rest.contains('\\') {
                let i = rest.find('\\').unwrap_or(0);
                unknown_tags.push('\\');
                unknown_tags.push_str(&rest[..i]);
                rest = rest[i..].trim_start_matches('\\').to_owned();
            } else if !rest.is_empty() {
                unknown_tags.push('\\');
                unknown_tags.push_str(&rest);
                rest.clear();
            }
        }
    }
    if !unknown_tags.is_empty() {
        unknown_tags = format!("{{{unknown_tags}}}");
    }
    Ok((extra_tags, unknown_tags, italic))
}

/// `AdvancedSubStationAlpha.FormatText`: `<i>`/`<u>`/`<b>` to `{\i1}`/`{\u1}`/`{\b1}`
/// and their closings, and each `<font face|size|color>` to `{\fn…}`/`{\fs…}`/`{\c&H…&}`
/// with `{\fn}`/`{\fs}`/`{\c}` where it closed; adjacent override blocks are merged.
#[must_use]
pub fn format_text(input: &str) -> String {
    let mut text = input.to_owned();
    if !text.contains('<') {
        return text;
    }
    text = text
        .replace("<i>", "{\\i1}")
        .replace("</i>", "{\\i0}")
        .replace("<u>", "{\\u1}")
        .replace("</u>", "{\\u0}")
        .replace("<b>", "{\\b1}")
        .replace("</b>", "{\\b0}");
    let mut count = 0;
    while text.contains("<font ") && count < 100 {
        if let Some(start) = text.find("<font ")
            && let Some(end) = find_from(&text, ">", start).filter(|&e| e > 0)
        {
            // From the space after `<font` through the closing `>`.
            let mut font_tag = text[start + 5..=end].to_owned();
            text.replace_range(start..=end, "");
            if let Some(end_font) = find_from(&text, "</font>", start).filter(|&i| i > 0) {
                text.replace_range(end_font..end_font + 7, "");
                if end_font < text.len() {
                    if font_tag.contains(" size=") {
                        text.insert_str(end_font, "{\\fs}");
                    }
                    if font_tag.contains(" face=") {
                        text.insert_str(end_font, "{\\fn}");
                    }
                    if font_tag.contains(" color=") {
                        text.insert_str(end_font, "{\\c}");
                    }
                }
            }
            for (attribute, name, end_tag) in [
                ("face=\"", "fn", "}"),
                ("face='", "fn", "}"),
                ("face=", "fn", "}"),
                ("size=\"", "fs", "}"),
                ("size='", "fs", "}"),
                ("size=", "fs", "}"),
                ("color=\"", "c&H", "&}"),
                ("color='", "c&H", "&}"),
                ("color=", "c&H", "&}"),
            ] {
                font_tag = format_tag(&mut text, start, font_tag, attribute, name, end_tag);
            }
        }
        count += 1;
    }
    text = text
        .replace("{\\c}", "@___@@")
        .replace("}{", "")
        .replace("@___@@", "{\\c}")
        .replace("{\\c}{\\c&", "{\\c&");
    while text.ends_with("{\\c}") {
        text.truncate(text.len() - 4);
    }
    for (doubled, single) in [
        ("\\fs\\fs", "\\fs"),
        ("\\fn\\fn", "\\fn"),
        ("\\c\\c&H", "\\c&H"),
    ] {
        while text.contains(doubled) {
            text = text.replace(doubled, single);
        }
    }
    text
}

/// `AdvancedSubStationAlpha.FormatTag`: when `font_tag` has `attribute`, inserts
/// `{\<name><value><end_tag>` at `start` (a colour as `bbggrr`) and returns the font
/// tag with that attribute removed.
fn format_tag(
    text: &mut String,
    start: usize,
    font_tag: String,
    attribute: &str,
    name: &str,
    end_tag: &str,
) -> String {
    let Some(font_start) = font_tag.find(attribute) else {
        return font_tag;
    };
    let value_start = font_start + attribute.len();
    let after = &font_tag[value_start..];
    let font_end = after
        .find(['"', '\''])
        .or_else(|| after.find([' ', '>']))
        .map(|i| i + value_start);
    let Some(font_end) = font_end.filter(|&e| e > 0) else {
        return font_tag;
    };
    let mut sub_tag = font_tag[value_start..font_end].to_owned();
    if attribute.contains("color") {
        let c = html_color_from_string(&sub_tag);
        sub_tag = format!("{:02x}{:02x}{:02x}", c.b, c.g, c.r);
    }
    let mut font_tag = font_tag;
    font_tag.replace_range(font_start..=font_end, "");
    if start < text.len() {
        text.insert_str(start, &format!("{{\\{name}{sub_tag}{end_tag}"));
    }
    font_tag
}
