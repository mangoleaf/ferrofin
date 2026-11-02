//! No two migrations share a version.
//!
//! sqlx takes a migration's version from the number before the first `_` in its file name
//! and records each applied version once in `_sqlx_migrations`. Two pull requests that each
//! took the next free number add two different files, so git merges them without a
//! conflict and `sqlx::migrate!` builds both into the chain. A fresh database then fails
//! to boot when the second one's row hits the table's primary key, and a database that
//! already applied one of them refuses the other (`VersionMismatch`). This test fails CI
//! instead, naming both.
//!
//! Fixing it, so that every database runs the chain in number order:
//! - a pull request merged into its base: the one that merged second renumbers its own
//!   migration above the base's last migration and every open pull request's into that base;
//! - `main` merged into a release branch: `main`'s keeps its number, and the release branch's
//!   unreleased migrations move above `main`'s last migration, keeping their order.
//!
//! Update every reference to a moved number (`REBUILD_MIGRATIONS`, `.pre-00NN` snapshot names,
//! docs) and regenerate its checksum line (`migration_immutability.rs`).

use std::collections::BTreeMap;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[test]
fn no_two_migrations_share_a_version() {
    let mut by_version: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    // A reversible migration's `.down.sql` shares its `.up.sql`'s version by design.
    for m in MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
    {
        by_version
            .entry(m.version)
            .or_default()
            .push(format!("\"{}\"", m.description));
    }
    let shared: Vec<String> = by_version
        .iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(v, names)| {
            format!(
                "migration version {v:04} is used by {} migrations ({}; the files in migrations/ numbered {v}). \
                 A pull request that merged second renumbers its own above the base's last migration and \
                 every open pull request's into that base; when main merges into a release branch, main's \
                 keeps its number and the release branch's unreleased migrations move above main's last \
                 migration, in order. Update every reference to a moved number (REBUILD_MIGRATIONS, \
                 .pre-00NN snapshot names, docs) and regenerate each moved migration's checksum line \
                 (migration_immutability.rs; crates/ferrofin-db/tests/migration_versions.rs explains).",
                names.len(),
                names.join(", ")
            )
        })
        .collect();
    assert!(shared.is_empty(), "{}", shared.join("\n"));
}
