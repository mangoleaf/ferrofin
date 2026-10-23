//! Folder content-type overrides: `ServerConfiguration.ContentTypes`.
//!
//! The metadata editor's **Content type** selector stores one path → collection
//! type pair per folder (`ItemUpdateController.UpdateItemContentType`), and the
//! library resolves a folder's contents by it (`LibraryManager.GetContentType`,
//! `ResolvePath`, `SeriesResolver`). These are the list operations and the path
//! comparisons they use, ported from `LibraryManager` and `ManagedFileSystem`.
//!
//! Paths compare as Jellyfin compares them on Linux: ordinally, with `/` as the
//! only separator.

use crate::data::CollectionType;
use crate::dto::NameValuePair;

/// `LibraryManager.GetContentTypeOverride(path, inherit)`: the collection type
/// of the first entry naming `path`, or with `inherit` the first naming `path`
/// or a folder above it.
///
/// The value parses as `Enum.TryParse<CollectionType>` does: case-sensitively,
/// with integers accepted. When the first matching entry does not parse, the
/// answer is `None`, even if a later entry would match.
#[must_use]
pub fn content_type_override(
    entries: &[NameValuePair],
    path: &str,
    inherit: bool,
) -> Option<CollectionType> {
    let entry = entries.iter().find(|entry| {
        entry.name.as_deref().is_some_and(|name| {
            paths_equal(name, path)
                || (inherit && !name.is_empty() && contains_sub_path(name, path))
        })
    })?;
    crate::json::enums::try_parse(entry.value.as_deref()?, false)
}

/// The list `ItemUpdateController.UpdateItemContentType` saves for `folder`
/// (an item's `ContainingFolderPath`, `None` for an item without a path):
/// every entry with a name that is not `folder` (compared ignoring case), then
/// `folder` → `content_type` unless that is blank. The value is stored as sent.
#[must_use]
pub fn with_content_type_override(
    entries: &[NameValuePair],
    folder: Option<&str>,
    content_type: Option<&str>,
) -> Vec<NameValuePair> {
    let mut entries: Vec<NameValuePair> = entries
        .iter()
        .filter(|entry| {
            entry.name.as_deref().is_some_and(|name| {
                !name.trim().is_empty()
                    && !folder.is_some_and(|folder| {
                        ferrofin_util::string_extensions::equals_ordinal_ignore_case(name, folder)
                    })
            })
        })
        .cloned()
        .collect();
    if let Some(content_type) = content_type.filter(|value| !value.trim().is_empty()) {
        entries.push(NameValuePair {
            name: folder.map(str::to_owned),
            value: Some(content_type.to_owned()),
        });
    }
    entries
}

/// `LibraryManager.RemoveContentTypeOverrides(path)`, run when `path` becomes a
/// library location: drops every entry for `path` or a folder below it, and
/// every entry without a name. Returns whether anything was dropped, which is
/// when upstream saves the configuration.
pub fn remove_content_type_overrides(entries: &mut Vec<NameValuePair>, path: &str) -> bool {
    let before = entries.len();
    entries.retain(|entry| {
        entry.name.as_deref().is_some_and(|name| {
            !name.trim().is_empty() && !paths_equal(path, name) && !contains_sub_path(path, name)
        })
    });
    entries.len() != before
}

/// `IFileSystem.AreEqual`: both paths are not blank and are equal once one
/// trailing separator is trimmed from each.
#[must_use]
pub fn paths_equal(left: &str, right: &str) -> bool {
    !left.trim().is_empty()
        && !right.trim().is_empty()
        && trim_ending_separator(left) == trim_ending_separator(right)
}

/// `IFileSystem.ContainsSubPath`: `path` lies below `parent`, which must be an
/// anchored prefix ending at a separator, so `/media` does not contain
/// `/media2` or `/data/media/tv`. Empty paths contain and are contained by
/// nothing (upstream throws on them).
#[must_use]
pub fn contains_sub_path(parent: &str, path: &str) -> bool {
    if parent.is_empty() || path.is_empty() {
        return false;
    }
    let parent = trim_ending_separator(parent);
    path.len() > parent.len()
        && path.starts_with(parent)
        && (parent.ends_with('/') || path.as_bytes()[parent.len()] == b'/')
}

/// `Path.TrimEndingDirectorySeparator`: one trailing separator beyond the
/// root, so `/` stays `/`.
fn trim_ending_separator(path: &str) -> &str {
    match path.strip_suffix('/') {
        Some(trimmed) if !trimmed.is_empty() => trimmed,
        _ => path,
    }
}
