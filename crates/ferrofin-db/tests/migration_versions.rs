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
//! Fixing it: give the migration that is not yet on `main` or a `release/*` branch the next
//! free number, and regenerate its checksum line (`migration_immutability.rs`). A migration
//! already on one of those branches is never renumbered.

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
                 Renumber the one not yet on main or a release/* branch to the next free number \
                 and regenerate its checksum line.",
                names.len(),
                names.join(", ")
            )
        })
        .collect();
    assert!(shared.is_empty(), "{}", shared.join("\n"));
}
