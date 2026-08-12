//! Port of `StringBuilderExtensions.cs` — appends single-quoted, delimiter-
//! separated values into a `String` (Rust's stand-in for `StringBuilder`), with
//! no trailing delimiter.

use std::fmt::Write;

/// Concatenates and appends the members of `values` in single quotes, joined by
/// `delimiter`, with no trailing delimiter.
///
/// Faithful to the C#: each value is written as `'value'delimiter`, then the
/// final delimiter is trimmed off.
///
/// # Panics
///
/// Panics if `values` is empty, mirroring the C# `builder.Length--` which
/// underflows on an empty collection.
pub fn append_join_in_single_quotes(builder: &mut String, delimiter: char, values: &[&str]) {
    assert!(!values.is_empty(), "values must not be empty");
    for value in values {
        // Writing into a String is infallible.
        let _ = write!(builder, "'{value}'{delimiter}");
    }

    // Remove the last delimiter.
    builder.truncate(builder.len() - delimiter.len_utf8());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_with_no_trailing_delimiter() {
        let mut sb = String::new();
        append_join_in_single_quotes(&mut sb, ',', &["a", "b", "c"]);
        assert_eq!("'a','b','c'", sb);
    }

    #[test]
    fn single_value_has_no_delimiter() {
        let mut sb = String::new();
        append_join_in_single_quotes(&mut sb, ',', &["only"]);
        assert_eq!("'only'", sb);
    }

    #[test]
    fn appends_to_existing_content() {
        let mut sb = String::from("prefix ");
        append_join_in_single_quotes(&mut sb, '|', &["x", "y"]);
        assert_eq!("prefix 'x'|'y'", sb);
    }
}
