//! SQL boundary ratchet.
//!
//! Raw `sqlx::query` calls are allowed without limit only where querying *is*
//! the job: `hermit-db` itself, `*_repository.rs` modules, `translate_query.rs`,
//! and the designated persistence/count services. Every other file carries a
//! checked-in ceiling equal to its current call count. The test fails when a
//! file exceeds its ceiling (new SQL belongs in a repository/persistence
//! module), when a new file grows SQL, and when a file drops below its ceiling
//! (lower the ceiling in the same commit — that's the ratchet).

use std::path::{Path, PathBuf};

/// Files where raw SQL is the module's job — no ceiling.
fn is_exempt(rel: &str) -> bool {
    rel.starts_with("crates/hermit-db/src/")
        || rel.ends_with("_repository.rs")
        || rel.ends_with("/translate_query.rs")
        || rel.ends_with("/item_persistence_service.rs")
        || rel.ends_with("/item_count_service.rs")
        || rel.ends_with("/user_data_manager.rs")
}

/// Current `sqlx::query` occurrence ceilings, per workspace-relative file.
/// Only lower these (in the same commit as the cleanup); never raise them.
const CEILINGS: &[(&str, usize)] = &[
    ("crates/hermit-core/src/activity_manager.rs", 5),
    ("crates/hermit-core/src/api_key_manager.rs", 3),
    ("crates/hermit-core/src/authorization_context.rs", 6),
    ("crates/hermit-core/src/collection_manager.rs", 16),
    ("crates/hermit-core/src/device_manager.rs", 13),
    ("crates/hermit-core/src/display_preferences_manager.rs", 12),
    ("crates/hermit-core/src/dto_service.rs", 17),
    ("crates/hermit-core/src/library_manager.rs", 9),
    ("crates/hermit-core/src/library_scan.rs", 24),
    ("crates/hermit-core/src/linked_children_service.rs", 8),
    ("crates/hermit-core/src/lyric_manager.rs", 1),
    ("crates/hermit-core/src/media_segment_manager.rs", 7),
    ("crates/hermit-core/src/media_source_manager.rs", 1),
    ("crates/hermit-core/src/music_manager.rs", 1),
    ("crates/hermit-core/src/next_up_service.rs", 5),
    ("crates/hermit-core/src/playback_metrics.rs", 4),
    ("crates/hermit-core/src/scheduled_tasks/library.rs", 17),
    ("crates/hermit-core/src/scheduled_tasks/maintenance.rs", 7),
    ("crates/hermit-core/src/session_manager.rs", 1),
    ("crates/hermit-core/src/session_manager/tests.rs", 5),
    ("crates/hermit-core/src/similar_items_manager.rs", 1),
    ("crates/hermit-core/src/subtitle_manager.rs", 3),
    ("crates/hermit-core/src/test_support.rs", 10),
    ("crates/hermit-core/src/trickplay_manager.rs", 7),
    ("crates/hermit-core/src/tv_series_manager.rs", 2),
    ("crates/hermit-core/src/user_entity_ext.rs", 11),
    ("crates/hermit-core/src/user_manager.rs", 29),
    ("crates/hermit-core/src/virtual_folder_manager.rs", 4),
    ("crates/hermit-livetv/src/manager.rs", 23),
];

/// Collects every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn sql_stays_behind_the_repository_boundary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for group in ["crates", "apps"] {
        let Ok(entries) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        for entry in entries.flatten() {
            rust_files(&entry.path().join("src"), &mut files);
        }
    }
    assert!(!files.is_empty(), "workspace walk found no source files");

    let mut violations = Vec::new();
    for path in files {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if is_exempt(&rel) {
            continue;
        }
        let count = std::fs::read_to_string(&path).map_or(0, |s| s.matches("sqlx::query").count());
        let ceiling = CEILINGS.iter().find(|(f, _)| *f == rel).map(|&(_, c)| c);
        match ceiling {
            None if count > 0 => violations.push(format!(
                "{rel}: {count} sqlx::query call(s) in a file outside the SQL boundary"
            )),
            Some(c) if count > c => violations.push(format!(
                "{rel}: {count} sqlx::query call(s), ceiling is {c}"
            )),
            Some(c) if count < c => violations.push(format!(
                "{rel}: {count} sqlx::query call(s), below its ceiling of {c} — \
                 lower the ceiling in CEILINGS to {count} in this same commit"
            )),
            _ => {}
        }
    }

    assert!(
        violations.is_empty(),
        "SQL boundary ratchet violated:\n  {}\n\n\
         Rule: new SQL goes in a repository/persistence module (hermit-db, \
         *_repository.rs, translate_query.rs, item_persistence_service.rs, \
         item_count_service.rs, user_data_manager.rs), not in managers/handlers. \
         Ceilings in crates/hermit-db/tests/sql_boundary.rs only ratchet down: \
         when you remove SQL from a file, lower its ceiling in the same commit; \
         never raise one.",
        violations.join("\n  ")
    );
}
