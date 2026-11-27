//! .NET composite formatting over string arguments: `string.Format(
//! CultureInfo.InvariantCulture, format, args)`, which Jellyfin fills its
//! localized templates through ("User {0} has been created").

/// `string.Format(CultureInfo.InvariantCulture, format, args)` over string
/// arguments. Each `{index}` item (spaces may follow the index) is replaced by
/// `args[index]` in one pass, so an argument's own braces are never read as
/// items; `{{` and `}}` are literal braces; an argument no item names is
/// ignored. `None` where .NET throws `FormatException`: a `{` not followed by
/// an ASCII digit, a `}` that closes nothing, an item left open, or an index
/// past `args`.
///
/// shortcut: an item with an alignment or a format string (`{0,5}`, `{0:N}`)
/// is refused too, since no template filled through this has one; parse them
/// when one does.
#[must_use]
pub fn format(format: &str, args: &[&str]) -> Option<String> {
    let mut out = String::with_capacity(format.len());
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' | '}' if chars.peek() == Some(&c) => {
                chars.next();
                out.push(c);
            }
            '{' => {
                let mut index: Option<usize> = None;
                while let Some(digit) = chars.peek().and_then(|d| d.to_digit(10)) {
                    chars.next();
                    let digit = usize::try_from(digit).ok()?;
                    index = Some(index.unwrap_or(0).checked_mul(10)?.checked_add(digit)?);
                }
                while chars.next_if_eq(&' ').is_some() {}
                if chars.next() != Some('}') {
                    return None;
                }
                out.push_str(args.get(index?)?);
            }
            '}' => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::format;
    use rstest::rstest;

    #[rstest]
    #[case::one_item("User {0} has been created", &["bob"], "User bob has been created")]
    #[case::three_items("{0} is playing {1} on {2}", &["a", "b", "c"], "a is playing b on c")]
    #[case::out_of_order("{1} before {0}", &["a", "b"], "b before a")]
    #[case::repeated("{0}{0}", &["a"], "aa")]
    #[case::unused_argument("{0} successfully authenticated", &["a", "dev"], "a successfully authenticated")]
    #[case::no_items("plain", &[], "plain")]
    #[case::escaped_braces("{{0}} {0}", &["a"], "{0} a")]
    #[case::spaces_after_the_index("{0  }", &["a"], "a")]
    #[case::an_argument_is_not_reparsed("{0} {1}", &["{1}", "x"], "{1} x")]
    #[case::empty_argument("IP address: {0}", &[""], "IP address: ")]
    #[case::multi_digit_index("{10}", &["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "ten"], "ten")]
    fn fills_each_item_with_its_argument(
        #[case] template: &str,
        #[case] args: &[&str],
        #[case] expected: &str,
    ) {
        assert_eq!(format(template, args).as_deref(), Some(expected));
    }

    /// Where .NET's `string.Format` throws `FormatException`. The first four
    /// are the malformed templates Jellyfin 12.4 ships (Cornish and Malayalam).
    #[rstest]
    #[case::no_digit_after_the_brace("Trigva PK: {)}", &["1.2.3.4"])]
    #[case::wrong_closing_bracket("Istitlow {0] rag {1}", &["a", "b"])]
    #[case::item_not_closed("ഉപയോക്താവ് {0 created സൃഷ്ടിച്ചു", &["a"])]
    #[case::item_not_closed_mid_template("{0} {2} ൽ {1 play", &["a", "b", "c"])]
    #[case::space_before_the_index("{ 0}", &["a"])]
    #[case::open_at_the_end("{0", &["a"])]
    #[case::lone_open_brace("{", &[])]
    #[case::lone_closing_brace("a } b", &[])]
    #[case::index_past_the_arguments("{3}", &["a", "b", "c"])]
    #[case::alignment("{0,5}", &["a"])]
    #[case::format_string("{0:N}", &["a"])]
    #[case::index_overflow("{99999999999999999999999}", &["a"])]
    fn refuses_what_dotnet_cannot_format(#[case] template: &str, #[case] args: &[&str]) {
        assert_eq!(format(template, args), None);
    }
}
