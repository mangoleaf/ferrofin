//! The libse 4.0.12 text helpers its subtitle writers call.
//!
//! Ports of `Nikse.SubtitleEdit.Core.Common` members (`StringExtensions`,
//! `Utilities`, `HtmlUtil`) used by the SubRip, WebVTT, SSA/ASS and TTML writers.

use super::dotnet::{Color, Throws, color_from_html, is_digit, is_letter_or_digit};
use super::srt::NEWLINE;

/// `StringExtensions.SplitToLines`: splits on `\r\n`, a lone `\r`, `\n` and U+2028.
/// The last line is kept even when empty, as libse does.
#[must_use]
pub fn split_to_lines(s: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                lines.push(std::mem::take(&mut current));
            }
            '\n' | '\u{2028}' => lines.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    lines.push(current);
    lines
}

/// `Utilities.RemoveSsaTags(input)` with `removeDrawingTags` false, the only form the
/// writers use: removes every `{\…}` override block (and a `{Kara Effector…}` block
/// that comes first), then turns `\n`/`\N` into line breaks and `\h` into a space.
#[must_use]
pub fn remove_ssa_tags(input: &str) -> String {
    let mut s = input.to_owned();
    let mut k = s.find("{\\");
    let karaoke_start = s.find("{Kara Effector");
    if k.is_none() || karaoke_start.is_some_and(|ks| k.is_some_and(|k| ks < k)) {
        k = karaoke_start;
    }
    while let Some(start) = k {
        let Some(end) = s[start + 1..].find('}').map(|i| i + start + 1) else {
            break;
        };
        s.replace_range(start..=end, "");
        k = s[start..].find('{').map(|i| i + start);
    }
    if s.contains('\\') {
        s = s
            .replace("\\n", NEWLINE)
            .replace("\\N", NEWLINE)
            .replace("\\h", " ");
    }
    s
}

/// `StringExtensions.RemoveRecursiveLineBreaks`: collapses each run of line breaks
/// into one (a `\r\n` pair counts as one break when the text has any `\r`).
#[must_use]
pub fn remove_recursive_line_breaks(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let has_carriage_return = chars.contains(&'\r');
    let mut kept: Vec<char> = Vec::with_capacity(chars.len());
    let mut line_break_adjacent = false;
    for &ch in chars.iter().rev() {
        if (has_carriage_return && ch == '\r') || ch == '\n' {
            if line_break_adjacent {
                continue;
            }
            kept.push(ch);
            line_break_adjacent = ch == '\r' || (!has_carriage_return && ch == '\n');
        } else {
            kept.push(ch);
            line_break_adjacent = false;
        }
    }
    kept.iter().rev().collect()
}

/// `StringExtensions.RemoveControlCharactersButWhiteSpace`: drops every control
/// character (`char.IsControl`, category Cc) except CR, LF and TAB.
#[must_use]
pub fn remove_control_characters_but_white_space(s: &str) -> String {
    s.chars()
        .filter(|&ch| !ch.is_control() || matches!(ch, '\r' | '\n' | '\t'))
        .collect()
}

/// `Utilities.ColorToHex`: `#rrggbb`.
#[must_use]
pub fn color_to_hex(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

/// `Utilities.ColorToHexWithTransparency`: `#rrggbbaa`.
#[must_use]
pub fn color_to_hex_with_transparency(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
}

/// `HtmlUtil.GetColorFromString`: `rgb(…)`, `rgba(…)`, `#rrggbbaa`, else
/// `ColorTranslator.FromHtml`; anything that throws is white.
#[must_use]
pub fn html_color_from_string(s: &str) -> Color {
    html_color_from_string_inner(s).unwrap_or(Color::WHITE)
}

fn html_color_from_string_inner(s: &str) -> Result<Color, Throws> {
    let lower = s.to_lowercase();
    let components = |prefix_len: usize| -> Vec<String> {
        s.replace(' ', "")
            .get(prefix_len..)
            .unwrap_or("")
            .trim_end_matches(')')
            .split(',')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let int = |v: Option<&String>| -> Result<i64, Throws> {
        v.and_then(|v| super::dotnet::int_try_parse(v))
            .map(i64::from)
            .ok_or(Throws)
    };
    if lower.starts_with("rgb(") {
        let parts = components(4);
        return Color::from_components(
            255,
            int(parts.first())?,
            int(parts.get(1))?,
            int(parts.get(2))?,
        );
    }
    if lower.starts_with("rgba(") {
        let parts = components(5);
        let mut alpha = 255;
        if parts.len() == 4
            && let Some(f) = super::dotnet::float_try_parse(&parts[3])
            && (0.0..1.0).contains(&f)
        {
            // `(byte)(f * byte.MaxValue)` truncates.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                alpha = (f * 255.0) as i64;
            }
        }
        return Color::from_components(
            alpha,
            int(parts.first())?,
            int(parts.get(1))?,
            int(parts.get(2))?,
        );
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() == 9 && chars[0] == '#' {
        let alpha_text: String = chars[7..9].iter().collect();
        let alpha = super::dotnet::hex_number_try_parse(&alpha_text).map_or(255, i64::from);
        let rgb: String = chars[1..7].iter().collect();
        return color_from_html(&format!("#{rgb}"))?.with_alpha(alpha);
    }
    color_from_html(s)
}

/// `HtmlUtil`'s `BeginTagVariations`: misspelt `<i>` tags `FixInvalidItalicTags` repairs.
const ITALIC_BEGIN_VARIATIONS: [&str; 9] = [
    "< i >", "< i>", "<i >", "< I >", "< I>", "<I >", "<i<", "<I<", "<I>",
];

/// `HtmlUtil`'s `EndTagVariations`: misspelt `</i>` tags `FixInvalidItalicTags` repairs.
const ITALIC_END_VARIATIONS: [&str; 18] = [
    "< / i >", "< /i>", "</ i>", "< /i >", "</i >", "</ i >", "< / i>", "</I>", "< / I >", "< /I>",
    "</ I>", "< /I >", "</I >", "</ I >", "< / I>", "</i<", "</I<", "</I>",
];

/// `HtmlUtil.UppercaseTags`, in libse's search order.
const UPPERCASE_TAGS: [&str; 8] = [
    "<I>", "<U>", "<B>", "<FONT", "</I>", "</U>", "</B>", "</FONT>",
];

/// `HtmlUtil.TagOpenRegex`: an opening or closing tag and its name.
static TAG_OPEN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"<\s*(?:/\s*)?(\w+)[^>]*>").expect("valid regex")
});

/// `Utilities.GetNumberOfLines`: one more than the number of `\n`, zero for "".
fn number_of_lines(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        1 + text.matches('\n').count()
    }
}

/// The character index of byte offset `byte` (libse indexes UTF-16 units; the two
/// agree outside the astral planes).
fn char_index(text: &str, byte: usize) -> usize {
    text[..byte].chars().count()
}

/// `text.Substring(n)` with `n` counted in characters.
fn skip_chars(text: &str, n: usize) -> &str {
    text.char_indices().nth(n).map_or("", |(i, _)| &text[i..])
}

/// `string.LastIndexOf(value, startIndex, count, Ordinal)` over characters: the last
/// occurrence of `value` lying wholly within `[startIndex - count + 1, startIndex]`.
fn last_index_of_in(
    chars: &[char],
    value: &str,
    start_index: usize,
    count: usize,
) -> Option<usize> {
    let value: Vec<char> = value.chars().collect();
    if count == 0 || value.is_empty() {
        return None;
    }
    let low = (start_index + 1).checked_sub(count)?;
    (low..=start_index).rev().find(|&i| {
        i + value.len() <= start_index + 1 && chars.get(i..i + value.len()) == Some(&value[..])
    })
}

/// `Utilities.StartsAndEndsWithTag`.
fn starts_and_ends_with_tag(text: &str, start_tag: &str, end_tag: &str) -> bool {
    if text.trim().is_empty() || !text.contains(start_tag) || !text.contains(end_tag) {
        return false;
    }
    let mut text = text.to_owned();
    while text.contains("  ") {
        text = text.replace("  ", " ");
    }
    let starts = [
        start_tag.to_owned(),
        format!("- {start_tag}"),
        format!("-{start_tag}"),
        format!("- ...{start_tag}"),
        format!("- {start_tag}..."),
    ];
    let ends = [
        end_tag.to_owned(),
        format!("{end_tag}."),
        format!("{end_tag}!"),
        format!("{end_tag}?"),
        format!("{end_tag}..."),
        format!("{end_tag}-"),
    ];
    starts.iter().any(|s| text.starts_with(s.as_str()))
        && ends.iter().any(|e| text.ends_with(e.as_str()))
}

/// `HtmlUtil.IsTextFormattable`: whether a letter or digit (`char.IsLetterOrDigit`)
/// sits between the tags a text starts and ends with.
fn is_text_formattable(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    if len == 0 {
        return false;
    }
    // Past the tags the text starts with (`None` stands for libse's -1).
    let mut index = Some(0);
    while let Some(i) = index.filter(|&i| i < len && chars[i] == '<') {
        index = chars[i + 1..]
            .iter()
            .position(|&c| c == '>')
            .map(|p| i + p + 2);
    }
    let index = index.unwrap_or(0);
    // Before the tags it ends with.
    let mut from_len = Some(len - 1);
    while let Some(f) = from_len.filter(|&f| chars[f] == '>') {
        from_len = chars[..=f]
            .iter()
            .rposition(|&c| c == '<')
            .and_then(|p| p.checked_sub(1));
    }
    let from_len = from_len.filter(|&f| f > 0).unwrap_or(len - 1);
    if from_len < index {
        return false;
    }
    chars[index..=from_len]
        .iter()
        .any(|&c| is_letter_or_digit(c))
}

/// `HtmlUtil.RemoveOpenCloseTags(source, tags)`: drops each opening or closing tag
/// whose name is one of `tags` (case ignored).
fn remove_open_close_tags(source: &str, tags: &[&str]) -> String {
    if source.is_empty() || !source.contains('<') {
        return source.to_owned();
    }
    TAG_OPEN
        .replace_all(source, |caps: &regex::Captures<'_>| {
            let name = caps.get(1).map_or("", |m| m.as_str());
            if tags
                .iter()
                .any(|t| super::dotnet::eq_ordinal_ignore_case(t, name))
            {
                String::new()
            } else {
                caps[0].to_owned()
            }
        })
        .into_owned()
}

/// `HtmlUtil.FixUpperTags`: lower-cases each `<I>`, `<U>`, `<B>`, `<FONT…`, and their
/// closing tags, up to its `>`.
fn fix_upper_tags(input: &str) -> String {
    if input.is_empty() || !input.contains('<') {
        return input.to_owned();
    }
    let mut text = input.to_owned();
    // `IndexOfAny(string[])`: the first tag in list order that occurs anywhere.
    let find = |text: &str| UPPERCASE_TAGS.iter().find_map(|t| text.find(t));
    while let Some(idx) = find(&text) {
        let Some(end) = text
            .get(idx + 2..)
            .and_then(|rest| rest.find('>'))
            .map(|i| i + idx + 2)
        else {
            break;
        };
        let lowered = text[idx..end].to_lowercase();
        text.replace_range(idx..end, &lowered);
    }
    text
}

/// `Utilities.RemoveSpaceBeforeAfterTag(input, "<i>")`, the only tag
/// `FixInvalidItalicTags` passes.
fn remove_space_before_after_italic_tag(input: &str) -> String {
    let (open, close) = ("<i>", "</i>");
    let nl = NEWLINE;
    let mut text = fix_upper_tags(input);
    let close1 = format!("! {close}{nl}");
    let close2 = format!("? {close}{nl}");
    let close3 = format!(" {close}");
    let close4 = format!(" {close}{nl}");
    let close5 = format!("{nl}{close}");
    text = text.replace(&close1, &format!("!{close}{nl}"));
    text = text.replace(&close2, &format!("?{close}{nl}"));
    if let Some(head) = text.strip_suffix(&close3) {
        text = format!("{head}{close}");
    }
    text = text.replace(&close4, &format!("{close}{nl}"));
    if let Some(rest) = text.strip_prefix("<i> ") {
        text = format!("{open}{rest}");
    }
    if text.starts_with(&format!("{open}{nl}")) {
        text.replace_range(open.len()..open.len() + nl.len(), "");
    }
    if text.ends_with(&close5) {
        // `Remove(Length - openTag.Length - NewLine.Length - 1, NewLine.Length)`.
        let at = text.len() - open.len() - nl.len() - 1;
        text.replace_range(at..at + nl.len(), "");
    }
    text = text.replace(&format!("{nl}{open} "), &format!("{nl}{open}"));
    text = text.replace(&format!(" {open} "), &format!(" {open}"));
    text = text.replace(&format!("{nl}{open} "), &format!("{nl}{open}"));
    text = text.replace(&format!(" {close} "), &format!("{close} "));
    text = text.replace(&format!(" {close}{nl}"), &format!("{close}{nl}"));
    let mut text = text.trim().to_owned();
    if let Some(rest) = text.strip_prefix("<i> ") {
        text = format!("{open}{rest}");
    }
    text
}

/// `HtmlUtil.FixInvalidItalicTags`: repairs misspelt, doubled, unbalanced and
/// mis-nested `<i>` tags the way libse does, quirks included (the tag counts are
/// taken once, before any repair, and every branch reads them).
///
/// One libse failure is not reproduced: for one `</i>` and no `<i>` over two lines
/// whose line break is the last character, libse's `Substring(newLineIndex + 2)`
/// throws and the whole conversion fails; here the second line is empty.
///
/// libse recurses once for every leading `NAME:` it peels off two italic lines; the
/// recursion runs here on an explicit stack, so a cue with thousands of them is
/// handled as libse handles it instead of overflowing the worker's stack.
#[must_use]
pub fn fix_invalid_italic_tags(input: &str) -> String {
    let mut frames = Vec::new();
    let mut current = input.to_owned();
    let mut text = loop {
        match italic_fix_head(&current) {
            ItalicStep::Done(text) => break text,
            ItalicStep::Recurse { frame, temp } => {
                frames.push(frame);
                current = temp;
            }
        }
    };
    while let Some(frame) = frames.pop() {
        let mut fixed = text.trim().to_owned();
        if fixed
            .get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("<i> "))
        {
            fixed = remove_space_before_after_italic_tag(&fixed);
        }
        text = italic_fix_rest(
            format!("{} {fixed}", frame.pre),
            frame.begin_count,
            frame.end_count,
            frame.no_of_lines,
            &frame.pre_tags,
        );
    }
    text
}

/// One level of `FixInvalidItalicTags` waiting for its recursive call's result.
struct ItalicFrame {
    /// The `{\…}` block the level set aside.
    pre_tags: String,
    /// The text up to and including the `NAME:` colon.
    pre: String,
    /// The level's tag and line counts, taken once before any repair.
    begin_count: usize,
    end_count: usize,
    no_of_lines: usize,
}

/// Where [`italic_fix_head`] stopped: finished, or at libse's recursive call.
enum ItalicStep {
    Done(String),
    Recurse { frame: ItalicFrame, temp: String },
}

/// `FixInvalidItalicTags` up to its recursion on the text after a `NAME:`.
#[allow(clippy::too_many_lines)] // one function, kept in libse's order
fn italic_fix_head(input: &str) -> ItalicStep {
    const B: &str = "<i>";
    const E: &str = "</i>";
    let nl = NEWLINE;
    let mut text = input.to_owned();

    let mut pre_tags = String::new();
    if text.starts_with("{\\")
        && let Some(end) = text[2..].find('}').map(|i| i + 2)
        && end > 2
    {
        text[..=end].clone_into(&mut pre_tags);
        text.replace_range(..=end, "");
    }

    for v in ITALIC_BEGIN_VARIATIONS {
        text = text.replace(v, B);
    }
    for v in ITALIC_END_VARIATIONS {
        text = text.replace(v, E);
    }
    text = text
        .replace("</i> <i>", "_@_")
        .replace(" _@_", "_@_")
        .replace(" _@_ ", "_@_")
        .replace("_@_", " ")
        .replace(&format!(" {E}{nl}"), &format!("{E}{nl}"));
    if text.contains(B) {
        text = text.replace("<i/>", E).replace("<I/>", E);
    } else {
        text = text.replace("<i/>", "").replace("<I/>", "");
    }
    text = text
        .replace("]<i> ", "] <i>")
        .replace(")<i> ", ") <i>")
        // libse also replaces `] </i>` and `) </i>` with themselves.
        .replace("<i><i>", B)
        .replace("</i></i>", E);

    let begin_count = text.matches(B).count();
    let end_count = text.matches(E).count();
    let no_of_lines = number_of_lines(&text);
    if begin_count + end_count == 0 {
        return ItalicStep::Done(format!("{pre_tags}{text}"));
    }

    if begin_count == 1 && end_count == 1 && text.find(B) > text.find(E) {
        let pattern = "___________@";
        text = text.replace(B, pattern).replace(E, B).replace(pattern, E);
    }

    if begin_count == 2 && end_count == 0 {
        let first = text.find(B).unwrap_or(0);
        let last = text.rfind(B).unwrap_or(0);
        let last_with_new_line = text.rfind(&format!("{nl}{B}")).map_or(0, |i| i + nl.len());
        if no_of_lines == 2 && last == last_with_new_line && char_index(&text, first) < 2 {
            text = format!("{}{E}", text.replace(nl, &format!("{E}{nl}")));
        } else {
            text.replace_range(last..last + B.len(), E);
        }
    }

    if begin_count == 1 && end_count == 2 {
        let first = text.find(E);
        if ["</i>-<i>-", "</i>- <i>-", "</i>- <i> -", "</i>-<i> -"]
            .iter()
            .any(|p| text.starts_with(p))
        {
            text.replace_range(..5, "");
        } else if first == Some(0) {
            text.replace_range(..4, "");
        } else if let Some(first) = first {
            text = format!("{}{}", &text[..first], &text[first + E.len()..]);
        }
    }

    if begin_count == 2 && end_count == 1 {
        let lines = split_to_lines(&text);
        if lines.len() == 2
            && lines[0].starts_with(B)
            && lines[0].ends_with(E)
            && lines[1].starts_with(B)
        {
            text = format!("{}{E}", text.trim_end());
        } else if let Some(last) = text.rfind(B) {
            if text[last..].chars().count() > E.len() {
                // `Substring(lastIndex - 1 + endTag.Length)`: past the `<i>`.
                text = format!("{}{}", &text[..last], &text[last + 3..]);
            } else {
                // `Substring(0, lastIndex - 1)` also drops the character before the tag.
                let head = &text[..last];
                let cut = head.char_indices().last().map_or(0, |(i, _)| i);
                text = format!("{}{E}", &head[..cut]);
            }
        }
        if text.starts_with(B) && text.ends_with(E) && text.contains(&format!("{E}{nl}{B}")) {
            text = text.replace(&format!("{E}{nl}{B}"), nl);
        }
    }

    if begin_count == 1 && end_count == 0 {
        let mut lines = split_to_lines(&text);
        for line in &mut lines {
            let Some(italic_byte) = line.rfind(B) else {
                continue;
            };
            if is_text_formattable(&line[italic_byte + 3..]) {
                // `CalculateEarlyInsertIndex`: before the first closing tag after the `<i>`.
                let chars: Vec<char> = line.chars().collect();
                let len = chars.len();
                let italic = char_index(line, italic_byte);
                let mut last_closing = last_index_of_in(&chars, "</", len - 1, len - italic - 3);
                while let Some(lc) = last_closing.filter(|&lc| lc > italic + 3) {
                    match last_index_of_in(&chars, "</", lc, lc - italic - 3) {
                        Some(temp) => last_closing = Some(temp),
                        None => break,
                    }
                }
                let insert = last_closing.filter(|&lc| lc > italic).unwrap_or(len);
                let mut new_line: String = chars[..insert].iter().collect();
                new_line.push_str(E);
                new_line.extend(&chars[insert..]);
                *line = new_line;
            } else {
                *line = line.replace(B, "");
            }
            break;
        }
        text = lines.join(nl);
    }

    if begin_count == 0 && end_count == 1 {
        let clean = remove_open_close_tags(&text, &["i", "b", "u", "\u{0456}"]);
        let mut is_fixed = false;
        if text.ends_with(E) && !clean.starts_with('-') && !clean.contains(&format!("{nl}-")) {
            text = format!("{B}{text}");
            is_fixed = true;
        }
        if !is_fixed
            && number_of_lines(&clean) == 2
            && let Some(new_line) = text.find(nl).filter(|&i| i > 0)
        {
            let mut first = text[..new_line].trim().to_owned();
            // libse skips two characters (`NewLine` assumed to be CRLF).
            let mut second = skip_chars(&text[new_line..], 2).trim().to_owned();
            if first.ends_with(E) {
                first = format!("{B}{first}");
                is_fixed = true;
            }
            if second.ends_with(E) {
                second = format!("{B}{second}");
                is_fixed = true;
            }
            text = format!("{first}{nl}{second}");
        }
        if !is_fixed {
            text = text.replace(E, "");
        }
    }

    if begin_count == 0 && end_count == 2 && text.contains(&format!("{E}{nl}")) && text.ends_with(E)
    {
        text = format!("{B}{}{E}", text.replace(E, ""));
    }

    if begin_count == 0
        && end_count == 2
        && let Some(first) = text.find(E)
    {
        text.replace_range(first..first + E.len(), B);
    }

    if begin_count == 2 && end_count == 2 && no_of_lines == 2 {
        let mut index = text.find(nl);
        if let Some(i) = index.filter(|&i| i > 0)
            && text.chars().count() > char_index(&text, i) + B.len() + E.len()
        {
            let mut first = text[..i].trim().to_owned();
            let mut second = text[i + nl.len()..].trim().to_owned();
            if first.chars().count() > 10 && first.starts_with("- <i>") && first.ends_with(E) {
                text = format!("<i>- {}{nl}{second}", &first[5..]).replace("<i>-  ", "<i>- ");
                index = text.find(nl);
                let i = index.unwrap_or(0);
                text[..i].trim().clone_into(&mut first);
                text[i + nl.len()..].trim().clone_into(&mut second);
            }
            if second.chars().count() > 10 && second.starts_with("- <i>") && second.ends_with(E) {
                text = format!("{first}{nl}<i>- {}", &second[5..]).replace("<i>-  ", "<i>- ");
                index = text.find(nl);
                let i = index.unwrap_or(0);
                text[..i].trim().clone_into(&mut first);
                text[i + nl.len()..].trim().clone_into(&mut second);
            }
            if starts_and_ends_with_tag(&first, B, E) && starts_and_ends_with_tag(&second, B, E) {
                text = format!("{B}{}{E}", text.replace(B, "").replace(E, "").trim());
            }
        }

        // FALCONE:<i> I didn't think</i><br /><i>it was going to be you,</i>
        if let Some(col) = text.find(':')
            && text.matches(B).count() + text.matches(E).count() == 4
            && text[col + 1..].chars().next().is_some_and(|c| !is_digit(c))
        {
            let index = index.unwrap_or(0).min(text.len());
            let first_line = &text[..index];
            let second_line = text[index..].trim_start();
            let sec_col = second_line.find(':');
            let between_numbers = sec_col.is_some_and(|c| {
                c >= 1
                    && second_line[..c].chars().last().is_some_and(is_digit)
                    && second_line[c + 1..].chars().next().is_some_and(is_digit)
            });
            if (sec_col.is_none() || !between_numbers)
                && let Some(idx) = first_line
                    .find(':')
                    .filter(|&idx| char_index(first_line, idx) > 1)
            {
                let pre = text[..=idx].trim_start().to_owned();
                let temp = text[idx + 1..].to_owned();
                if !temp.starts_with(']')
                    && !temp.starts_with(')')
                    && !temp.starts_with(nl)
                    && !temp.starts_with(&format!("{E}{nl}"))
                {
                    return ItalicStep::Recurse {
                        frame: ItalicFrame {
                            pre_tags,
                            pre,
                            begin_count,
                            end_count,
                            no_of_lines,
                        },
                        temp,
                    };
                }
            }
        }
    }

    ItalicStep::Done(italic_fix_rest(
        text,
        begin_count,
        end_count,
        no_of_lines,
        &pre_tags,
    ))
}

/// `FixInvalidItalicTags` after its recursion: the repairs that read the level's
/// counts, the tag clean-up, and the set-aside `{\…}` block put back.
fn italic_fix_rest(
    mut text: String,
    begin_count: usize,
    end_count: usize,
    no_of_lines: usize,
    pre_tags: &str,
) -> String {
    const B: &str = "<i>";
    const E: &str = "</i>";
    let nl = NEWLINE;

    if begin_count == 3 && end_count == 1 && no_of_lines == 2 {
        let new_line = text.find(nl).unwrap_or(0);
        let first = text[..new_line].trim().to_owned();
        let second = text[new_line..].trim().to_owned();
        if (starts_and_ends_with_tag(&first, B, B) && starts_and_ends_with_tag(&second, B, E))
            || (starts_and_ends_with_tag(&second, B, B) && starts_and_ends_with_tag(&first, B, E))
        {
            text = format!(
                "{B}{}{E}",
                text.replace(B, "").replace(E, "").replace("  ", " ").trim()
            );
        }
    }

    for (tag, count) in [(E, (0, 4)), (B, (4, 0))] {
        if no_of_lines == 2 && (begin_count, end_count) == count {
            let lines = split_to_lines(&text);
            if lines.len() == 2
                && lines[0].starts_with(tag)
                && lines[1].ends_with(tag)
                && lines[1].starts_with(tag)
                && lines[1].ends_with(tag)
            {
                let s1 = lines[0].replace(tag, "");
                let s2 = lines[1].replace(tag, "");
                text = format!("{B}{}{E}{nl}{B}{}{E}", s1.trim(), s2.trim());
            }
        }
    }

    if no_of_lines == 3 && matches!((begin_count, end_count), (3, 2) | (2, 3)) {
        let italics: usize = split_to_lines(&text)
            .iter()
            .map(|line| usize::from(line.starts_with(B)) + usize::from(line.ends_with(E)))
            .sum();
        if italics == 5 {
            text = format!("{B}{}{E}", text.replace(B, "").replace(E, ""));
        }
    }

    text = text
        .replace("<i></i>", "")
        .replace("</i><i>", "")
        .replace(&format!("{B}{nl}{E}"), nl);
    if text.contains('@') {
        text = text
            .replace("</i> <i>", " ")
            .replace("<i> </i>", " ")
            .replace("<i>  </i>", " ")
            .replace("  ", " ")
            .replace("  ", " ");
    } else {
        text = text
            .replace("</i> <i>", "@")
            .replace("<i> </i>", "@")
            .replace("<i>  </i>", "@")
            .replace("@ ", " ")
            .replace("@ ", " ")
            .replace(" @", " ")
            .replace(" @", " ")
            .replace('@', " ");
    }
    format!("{pre_tags}{text}")
}
