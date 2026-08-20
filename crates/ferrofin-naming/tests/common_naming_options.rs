//! Ported from `Common/NamingOptionsTest.cs`.

use ferrofin_naming::common::{EpisodeExpression, NamingOptions};

#[test]
fn test_naming_options_compile() {
    let options = NamingOptions::new();

    assert!(!options.clean_date_time_regexes.is_empty());
    assert!(!options.clean_string_regexes.is_empty());
}

/// Every raw expression table that has a compiled twin must be compiled by both
/// `new()` and `compile()` — the parsers read only the compiled vectors, so a
/// table left uncompiled silently stops matching anything.
#[test]
fn compile_refreshes_every_compiled_table() {
    let mut options = NamingOptions::new();

    assert_eq!(
        options.audio_book_parts_regexes.len(),
        options.audio_book_parts_expressions.len()
    );
    assert_eq!(
        options.audio_book_names_regexes.len(),
        options.audio_book_names_expressions.len()
    );

    options
        .audio_book_parts_expressions
        .push(r"(?<part>[0-9]+)".to_owned());
    options
        .audio_book_names_expressions
        .push(r"(?<name>.+)".to_owned());
    options.clean_date_times.push(r"(a)(1999)(b)(c)".to_owned());
    options.clean_strings.push(r"(?<cleaned>.+)".to_owned());
    options.compile();

    assert_eq!(
        options.audio_book_parts_regexes.len(),
        options.audio_book_parts_expressions.len()
    );
    assert_eq!(
        options.audio_book_names_regexes.len(),
        options.audio_book_names_expressions.len()
    );
    assert_eq!(
        options.clean_date_time_regexes.len(),
        options.clean_date_times.len()
    );
    assert_eq!(
        options.clean_string_regexes.len(),
        options.clean_strings.len()
    );
}

#[test]
fn test_naming_options_episode_expressions() {
    let mut exp = EpisodeExpression::new(String::new(), false);

    assert!(!exp.is_optimistic);
    exp.is_optimistic = true;
    assert!(exp.is_optimistic);

    assert_eq!(exp.expression(), "");
    let _ = exp.regex();
    exp.set_expression("test");
    assert_eq!(exp.expression(), "test");
    let _ = exp.regex();
}
