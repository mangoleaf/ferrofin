//! Port of `Emby.Naming.Video.StubResolver`.

use crate::common::NamingOptions;
use crate::path;

/// Result of a stub resolution: whether the file is a stub and its type.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StubResult {
    /// Whether the file is a stub (`.disc`).
    pub is_stub: bool,
    /// The resolved stub type, if any token matched.
    pub stub_type: Option<String>,
}

/// Tries to resolve whether a file is a stub (`.disc`).
///
/// Mirrors C# `TryResolveFile`, which returns `bool` and an out `stubType`.
#[must_use]
pub fn try_resolve_file(path_str: &str, options: &NamingOptions) -> StubResult {
    if path_str.is_empty() {
        return StubResult::default();
    }

    let extension = path::extension(path_str);

    if !options
        .stub_file_extensions
        .iter()
        .any(|e| e.eq_ignore_ascii_case(extension))
    {
        return StubResult::default();
    }

    // Token is the extension of the file name without its (.disc) extension,
    // with the leading '.' stripped.
    let stem = path::file_name_without_extension(path_str);
    let token = path::extension(stem).trim_start_matches('.');

    for rule in &options.stub_types {
        if token.eq_ignore_ascii_case(&rule.token) {
            return StubResult {
                is_stub: true,
                stub_type: Some(rule.stub_type.clone()),
            };
        }
    }

    StubResult {
        is_stub: true,
        stub_type: None,
    }
}
