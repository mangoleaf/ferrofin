//! Crate-version ratchet.
//!
//! The workspace manifest's version is a placeholder: the git tag is the
//! release's version, and [`ferrofin_health::build_version()`] (with
//! `RELEASE_VERSION` and `USER_AGENT` beside it) is the value derived from it.
//! Code that reads the crate version (Cargo's [`NEEDLE`], through `env!`,
//! `option_env!` or a build script's environment) reports that placeholder on
//! every release. The test fails when a file under `crates/` or `apps/` holds
//! more mentions of it than [`ALLOWED`] grants, and when it holds fewer (lower
//! the entry in the same commit: that is the ratchet).
//!
//! Every mention counts, comments and doc text included: a lexer is not worth
//! it, and prose can name `build_version()` instead. `examples/` is not
//! scanned: a WASM guest's version is its own package's. A read the source
//! does not spell out (a bare clap `version`, utoipa's default `info.version`)
//! is invisible to this test.

use std::path::{Path, PathBuf};

/// The text counted, split so this file does not count itself.
const NEEDLE: &str = concat!("CARGO_PKG", "_VERSION");

/// Files allowed to mention the crate version, with their mention counts.
/// Only lower or remove an entry, in the commit that removes the use; never
/// add or raise one.
const ALLOWED: &[(&str, usize)] = &[
    // `build_version()`'s own last resort, for a build with neither git nor
    // `FERROFIN_GIT_DESCRIBE`.
    ("crates/ferrofin-health/build.rs", 1),
    // The plugin version of a build with no release tag in its history, which
    // has no release number to use, and that fallback's two test cases.
    ("crates/ferrofin-extensions/src/ferrofin_settings.rs", 3),
];

/// Collects every `.rs` file under `dir`, recursively. A directory that
/// cannot be read fails the test rather than hiding the files under it.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn crate_version_uses_stay_on_the_allowlist() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for group in ["crates", "apps"] {
        rust_files(&root.join(group), &mut files);
    }

    let mut violations = Vec::new();
    for path in &files {
        let rel = path.trim_prefix(&root).to_string_lossy().replace('\\', "/");
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{rel}: {e}"));
        let count = text.matches(NEEDLE).count();
        let allowed = ALLOWED
            .iter()
            .find(|(f, _)| *f == rel)
            .map_or(0, |&(_, c)| c);
        if count > allowed {
            violations.push(format!(
                "{rel}: {count} mention(s) of {NEEDLE}, {allowed} allowed. It is the \
                 manifest's placeholder, not the release: use \
                 ferrofin_health::build_version() (or RELEASE_VERSION / USER_AGENT)"
            ));
        } else if count < allowed {
            violations.push(format!(
                "{rel}: {count} mention(s) of {NEEDLE}, below its allowance of {allowed}: \
                 lower its ALLOWED entry to {count} (remove it at 0) in this same commit"
            ));
        }
    }
    // Also what keeps a walk that found nothing from passing.
    for (file, _) in ALLOWED {
        if !files.iter().any(|f| f.ends_with(file)) {
            violations.push(format!(
                "{file}: not found by the walk: remove its ALLOWED entry if the file is gone"
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "crate-version ratchet:\n{}",
        violations.join("\n")
    );
}
