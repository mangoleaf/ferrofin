//! Every [`InternalItemsQuery`](ferrofin_traits::options::InternalItemsQuery)
//! field is read somewhere outside `has_filters` and `Default`.
//!
//! A filter field nothing translates is a filter a caller sets and never gets:
//! `has_filters` alone can change which path a query takes without narrowing
//! its results. That is how `person_types` and the ten fields of gl-811 went
//! unnoticed. This test reads the workspace's sources for a read of each
//! field, `.field` that is not an assignment or a push, leaving out test code
//! (test modules, test-named files) and comments. The
//! field names are not unique to this struct, so a read of a same-named
//! field elsewhere can satisfy the guard; it catches a distinctive field read
//! nowhere, which is the case that slipped through.

use std::path::{Path, PathBuf};

/// Fields nothing reads, each for the reason given. The test fails when one
/// of them gains a read, so this list only shrinks.
const UNREAD: &[(&str, &str)] = &[
    (
        "min_sort_name",
        "v12.2 reads `MinSortName` nowhere outside `HasFilters` either",
    ),
    (
        "preset_views",
        "v12.2's `UserRootFolder` copies `PresetViews` into a `UserViewQuery`, but nothing \
         sets it on an `InternalItemsQuery`, so it is always empty there",
    ),
    (
        "source_types",
        "v12.2's scheduled tasks set `SourceTypes`, and nothing reads it",
    ),
    (
        "display_album_folders",
        "v12.2's `Folder` reads `DisplayAlbumFolders`, but nothing sets it on a query",
    ),
    (
        "skip_deserialization",
        "a C# shortcut past `DeserializeBaseItem`; Ferrofin decodes rows with `FromRow`",
    ),
    (
        "include_extras",
        "an EF `Include(e => e.Extras)` navigation load, not a filter; Ferrofin reads extras \
         by owner id",
    ),
];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "tests") {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs")
            && !path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(is_test_file)
        {
            out.push(path);
        }
    }
}

/// Whether a source file named `stem` holds test code only.
fn is_test_file(stem: &str) -> bool {
    stem == "tests"
        || stem == "test_support"
        || stem.starts_with("test_")
        || stem.ends_with("_tests")
}

/// `text` up to its inline test module (`#[cfg(test)] mod name {`), if it
/// has one, without its `//` comment lines. An out-of-line `mod name;` is a
/// declaration, not the module's code, and does not end the scan.
fn without_tests(text: &str) -> String {
    let code = text
        .match_indices("#[cfg(test)]\nmod ")
        .find(|(at, _)| {
            text[*at..]
                .lines()
                .nth(1)
                .is_some_and(|line| line.trim_end().ends_with('{'))
        })
        .map_or(text, |(end, _)| &text[..end]);
    code.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text` without the brace-delimited item that starts at `header`.
fn without_item(text: &str, header: &str) -> String {
    let start = text
        .find(header)
        .unwrap_or_else(|| panic!("{header:?} not found"));
    let open = start + text[start..].find('{').expect("an opening brace");
    let mut depth = 0_usize;
    for (offset, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return format!("{}{}", &text[..start], &text[open + offset + 1..]);
                }
            }
            _ => {}
        }
    }
    panic!("{header:?} is not closed");
}

/// The struct's field names, in declaration order.
fn fields(source: &str) -> Vec<String> {
    let start = source
        .find("pub struct InternalItemsQuery {")
        .expect("the struct");
    let body = &source[start..start + source[start..].find("\n}").expect("its end")];
    body.lines()
        .filter_map(|line| line.strip_prefix("    pub "))
        .filter_map(|rest| rest.split_once(':'))
        .map(|(name, _)| name.to_owned())
        .collect()
}

/// Whether `text` reads `.field`: not an assignment (`.field =`) and not a
/// collection being filled or emptied.
fn reads(text: &str, field: &str) -> bool {
    let needle = format!(".{field}");
    text.match_indices(&needle).any(|(at, _)| {
        let rest = &text[at + needle.len()..];
        if rest
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            return false;
        }
        let rest = rest.trim_start();
        let assignment = rest.starts_with('=') && !rest.starts_with("==");
        let fill = [".push(", ".clear()", ".extend(", ".insert(", ".retain("]
            .iter()
            .any(|call| rest.starts_with(call));
        !assignment && !fill
    })
}

/// The sources a read may come from: every crate's and app's `src`, test
/// code left out, and the struct's own file without the struct,
/// `has_filters` and `Default`.
fn sources() -> (Vec<String>, String) {
    let root = workspace();
    let own_path = root.join("crates/ferrofin-traits/src/options/internal_items_query.rs");
    let own_source = std::fs::read_to_string(&own_path).expect("read the struct's file");
    let mut own = without_tests(&own_source);
    for header in [
        "pub struct InternalItemsQuery {",
        "pub fn has_filters(&self) -> bool",
        "impl Default for InternalItemsQuery",
    ] {
        own = without_item(&own, header);
    }
    let mut files = Vec::new();
    for group in ["crates", "apps"] {
        let entries = std::fs::read_dir(root.join(group)).expect("read the workspace");
        for entry in entries.flatten() {
            rust_files(&entry.path().join("src"), &mut files);
        }
    }
    let mut texts = vec![own];
    for path in files {
        if path.canonicalize().ok() == own_path.canonicalize().ok() {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read a source file");
        texts.push(without_tests(&text));
    }
    (texts, own_source)
}

#[test]
fn every_internal_items_query_field_is_read() {
    let (texts, own) = sources();
    let fields = fields(&own);
    assert!(fields.len() > 100, "parsed {} fields", fields.len());
    let read = |field: &str| texts.iter().any(|text| reads(text, field));

    let unread: Vec<&str> = fields
        .iter()
        .map(String::as_str)
        .filter(|field| !read(field) && !UNREAD.iter().any(|(name, _)| name == field))
        .collect();
    assert!(
        unread.is_empty(),
        "InternalItemsQuery fields nothing reads outside has_filters and Default: {unread:?}. \
         Translate each where upstream does (translate_query.rs), or list it in UNREAD with \
         the reason upstream does not read it either"
    );

    let stale: Vec<&str> = UNREAD
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !fields.iter().any(|field| field == name) || read(name))
        .collect();
    assert!(
        stale.is_empty(),
        "UNREAD entries that are read now, or are no field: {stale:?}; delete them"
    );
}

#[test]
fn a_read_is_told_from_a_write() {
    assert!(reads("if query.trailer_types.is_empty()", "trailer_types"));
    assert!(reads("let x = &q.trailer_types;", "trailer_types"));
    assert!(reads("q.trailer_types == other", "trailer_types"));
    assert!(!reads("q.trailer_types = vec![];", "trailer_types"));
    assert!(!reads("q.trailer_types.push(t);", "trailer_types"));
    assert!(!reads("q.trailer_types_extra", "trailer_types"));
    assert!(!reads("trailer_types: vec![],", "trailer_types"));
}
