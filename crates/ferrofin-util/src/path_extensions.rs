//! Port of the sub-path rewriting in `Emby.Server.Implementations.Library.PathExtensions`
//! (`TryReplaceSubPath` and the `NormalizePath` overloads it uses).

/// Port of `PathExtensions.TryReplaceSubPath`: `path` with its leading
/// `sub_path` replaced by `new_sub_path`, or `None` when `sub_path` is not a
/// leading sub path of it (or any argument is empty).
///
/// Both paths are normalized to one separator first: `/` when `sub_path`
/// contains one, else `\`. The match ignores case (`OrdinalIgnoreCase`) and
/// stops at a path component, so `/media` does not match `/media-other`. The
/// replacement keeps the separator the sub path consumed and drops trailing
/// separators from `new_sub_path`.
#[must_use]
pub fn try_replace_sub_path(path: &str, sub_path: &str, new_sub_path: &str) -> Option<String> {
    // `string.Length` counts UTF-16 code units.
    if path.is_empty()
        || sub_path.is_empty()
        || new_sub_path.is_empty()
        || sub_path.encode_utf16().count() > path.encode_utf16().count()
    {
        return None;
    }
    let separator = if sub_path.contains('/') { '/' } else { '\\' };
    let sub_path = normalize_path(sub_path, separator);
    let path = normalize_path(path, separator);
    let ends_with_separator = sub_path.ends_with(separator);
    let tail = strip_prefix_ignore_case(&path, &sub_path)?;
    if !tail.is_empty() && !ends_with_separator && !tail.starts_with(separator) {
        return None;
    }
    // `idx = oldSubPathEndsWithSeparator ? subPath.Length - 1 : subPath.Length`:
    // the remainder always starts with the separator.
    let rest = if ends_with_separator {
        &path[path.len() - tail.len() - separator.len_utf8()..]
    } else {
        tail
    };
    Some(format!(
        "{}{rest}",
        new_sub_path.trim_end_matches(separator)
    ))
}

/// Port of `PathExtensions.NormalizePath(path, newSeparator)`: every `/` and
/// `\` becomes `separator`.
fn normalize_path(path: &str, separator: char) -> String {
    path.replace(['/', '\\'], &separator.to_string())
}

/// `path` without a leading `prefix`, compared per character ignoring case
/// (.NET `StringComparison.OrdinalIgnoreCase`), or `None`.
fn strip_prefix_ignore_case<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let mut rest = path.chars();
    for expected in prefix.chars() {
        let actual = rest.next()?;
        if actual != expected && !actual.to_uppercase().eq(expected.to_uppercase()) {
            return None;
        }
    }
    Some(rest.as_str())
}

#[cfg(test)]
mod tests {
    use super::try_replace_sub_path;
    use rstest::rstest;

    /// `PathExtensionsTests.TryReplaceSubPath_ValidArgs_Correct`.
    #[rstest]
    #[case(
        "C:/Users/jeff/myfile.mkv",
        "C:/Users/jeff",
        "/home/jeff",
        "/home/jeff/myfile.mkv"
    )]
    #[case(
        "C:/Users/jeff/myfile.mkv",
        "C:/Users/jeff/",
        "/home/jeff",
        "/home/jeff/myfile.mkv"
    )]
    #[case(
        "/home/jeff/music/jeff's band/consistently inconsistent.mp3",
        "/home/jeff/music/jeff's band",
        "/home/not jeff",
        "/home/not jeff/consistently inconsistent.mp3"
    )]
    #[case(
        r"C:\Users\jeff\myfile.mkv",
        "C:\\Users/jeff",
        "/home/jeff",
        "/home/jeff/myfile.mkv"
    )]
    #[case(
        r"C:\Users\jeff\myfile.mkv",
        "C:\\Users/jeff",
        "/home/jeff/",
        "/home/jeff/myfile.mkv"
    )]
    #[case(
        r"C:\Users\jeff\myfile.mkv",
        "C:\\Users/jeff/",
        "/home/jeff/",
        "/home/jeff/myfile.mkv"
    )]
    #[case(r"C:\Users\jeff\myfile.mkv", "C:\\Users/jeff/", "/", "/myfile.mkv")]
    #[case("/o", "/o", "/s", "/s")] // regression test for #5977
    fn try_replace_sub_path_valid_args_correct(
        #[case] path: &str,
        #[case] sub_path: &str,
        #[case] new_sub_path: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(
            try_replace_sub_path(path, sub_path, new_sub_path).as_deref(),
            Some(expected)
        );
    }

    /// `PathExtensionsTests.TryReplaceSubPath_InvalidInput_ReturnsFalseAndNull`
    /// (the `null` cases have no `&str` form; empty strings stand for them).
    #[rstest]
    #[case("", "", "")]
    #[case("/my/path", "", "")]
    #[case("", "/another/path", "")]
    #[case("", "", "/new/subpath")]
    #[case("", "/my/path", "/another/path")]
    #[case("/my/path", "", "/another/path")]
    #[case("/my/path", "/another/path", "")]
    #[case(
        "/home/jeff/music/jeff's band/consistently inconsistent.mp3",
        "/home/jeff/music/not jeff's band",
        "/home/not jeff"
    )]
    fn try_replace_sub_path_invalid_input_returns_none(
        #[case] path: &str,
        #[case] sub_path: &str,
        #[case] new_sub_path: &str,
    ) {
        assert_eq!(try_replace_sub_path(path, sub_path, new_sub_path), None);
    }

    /// The match ignores case and stops at a path component.
    #[test]
    fn matches_whole_components_ignoring_case() {
        assert_eq!(
            try_replace_sub_path("/Media/Movie/a.mkv", "/media", "//nas/media").as_deref(),
            Some("//nas/media/Movie/a.mkv")
        );
        assert_eq!(
            try_replace_sub_path("/media-other/a.mkv", "/media", "/x"),
            None
        );
    }
}
