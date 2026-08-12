//! Ported from `Common/NamingOptionsTest.cs`.

use ferrofin_naming::common::{EpisodeExpression, NamingOptions};

#[test]
fn test_naming_options_compile() {
    let options = NamingOptions::new();

    assert!(!options.clean_date_time_regexes.is_empty());
    assert!(!options.clean_string_regexes.is_empty());
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
