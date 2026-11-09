//! Released migrations are immutable — including their comments.
//!
//! sqlx records a checksum of every applied migration file in `_sqlx_migrations`
//! and refuses to open a database whose file has since changed
//! (`migration N was previously applied but has been modified`). A one-word
//! comment edit to `0006_playback_sessions.sql` shipped in v1.0.0 and locked every
//! existing Ferrofin database out of the upgrade. This test pins the checksum of
//! every migration so that edit fails CI instead of a user's boot.
//!
//! Adding a migration: append its line by running
//! `FERROFIN_WRITE_MIGRATION_CHECKSUMS=1 cargo test -p ferrofin-db --test migration_immutability`
//! and commit the updated `tests/data/migration_checksums.txt`. That mode only appends:
//! it fails, and writes nothing, while a pinned migration has changed, is gone or is pinned
//! twice. Changing an existing line is never right: write a new migration instead. The only
//! lines you may delete by hand first are those of migrations this branch adds that are not on
//! its base yet, and of migrations moved as `migration_versions.rs` describes.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

const PINNED: &str = "tests/data/migration_checksums.txt";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn current() -> BTreeMap<i64, (String, String)> {
    MIGRATOR
        .iter()
        .map(|m| (m.version, (m.description.to_string(), hex(&m.checksum))))
        .collect()
}

/// The pinned checksums, and each version pinned more than once (both sides of a merge kept).
fn parse(text: &str) -> (BTreeMap<i64, String>, Vec<i64>) {
    let mut pinned = BTreeMap::new();
    let mut repeated = Vec::new();
    for l in text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
    {
        let mut it = l.split_whitespace();
        let (Some(v), Some(sum)) = (it.next().and_then(|v| v.parse().ok()), it.next()) else {
            continue;
        };
        if pinned.insert(v, sum.to_string()).is_some() {
            repeated.push(v);
        }
    }
    (pinned, repeated)
}

/// Every problem with the pin file `text` except a migration not pinned yet, and the
/// versions not pinned yet.
fn check(now: &BTreeMap<i64, (String, String)>, text: &str) -> (Vec<String>, Vec<i64>) {
    let (pinned, repeated) = parse(text);
    let mut problems: Vec<String> = repeated
        .iter()
        .map(|v| {
            format!(
                "migration {v:04} is pinned more than once — two migrations took the same number \
                 (see migration_versions.rs); keep one line per version"
            )
        })
        .collect();
    let mut unpinned = Vec::new();
    for (v, (desc, sum)) in now {
        match pinned.get(v) {
            None => unpinned.push(*v),
            Some(p) if p != sum => problems.push(format!(
                "migration {v:04} ({desc}) CHANGED since it was released: a database that already applied it \
                 will refuse to open. Revert the file byte-for-byte and put the change in a NEW migration."
            )),
            Some(_) => {}
        }
    }
    for v in pinned.keys() {
        if !now.contains_key(v) {
            problems.push(format!(
                "pinned migration {v:04} no longer exists — released migrations are never removed"
            ));
        }
    }
    (problems, unpinned)
}

/// `text` with a line appended for each migration not pinned yet. Refuses, rather than
/// re-pin, while any pinned line no longer matches its migration.
fn append_unpinned(now: &BTreeMap<i64, (String, String)>, text: &str) -> Result<String, String> {
    let (problems, unpinned) = check(now, text);
    if !problems.is_empty() {
        return Err(format!(
            "{}\nFERROFIN_WRITE_MIGRATION_CHECKSUMS only appends, so it wrote nothing. Only the lines of \
             migrations this branch adds that are not on its base yet, and of migrations moved as \
             migration_versions.rs describes, may be deleted from {PINNED} by hand before running it again.",
            problems.join("\n")
        ));
    }
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for v in unpinned {
        let (desc, sum) = &now[&v];
        let _ = writeln!(out, "{v:04}  {sum}  {desc}");
    }
    Ok(out)
}

#[test]
fn released_migrations_have_not_changed() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(PINNED);
    let now = current();
    let text = std::fs::read_to_string(&path).expect("tests/data/migration_checksums.txt exists");
    if std::env::var_os("FERROFIN_WRITE_MIGRATION_CHECKSUMS").is_some() {
        let appended = append_unpinned(&now, &text).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&path, appended).expect("write the pinned checksums");
        return;
    }
    let (mut problems, unpinned) = check(&now, &text);
    problems.extend(unpinned.iter().map(|v| {
        format!(
            "migration {v:04} ({}) is not pinned — append it with FERROFIN_WRITE_MIGRATION_CHECKSUMS=1",
            now[v].0
        )
    }));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The pin file `released_migrations_have_not_changed` expects for `now`.
fn pins(now: &BTreeMap<i64, (String, String)>) -> String {
    append_unpinned(now, "# header\n").expect("an empty pin file has nothing to refuse")
}

#[test]
fn regenerating_appends_only_the_unpinned_migrations() {
    let now = current();
    let last = *now.keys().next_back().expect("at least one migration");
    let mut older = now.clone();
    older.remove(&last);
    let text = pins(&older);
    let appended = append_unpinned(&now, &text).expect("nothing pinned has changed");
    assert_eq!(appended, pins(&now));
    assert!(
        appended.starts_with(&text),
        "the existing lines are kept as they were"
    );
    let unterminated = append_unpinned(&now, text.trim_end()).expect("nothing pinned has changed");
    assert_eq!(
        unterminated, appended,
        "the new line starts a line of its own"
    );
    assert_eq!(
        append_unpinned(&now, &appended),
        Ok(appended),
        "nothing left to append"
    );
}

#[test]
fn regenerating_refuses_a_version_pinned_twice() {
    let now = current();
    let text = pins(&now);
    let first = text.lines().nth(1).expect("the 0001 line");
    let err =
        append_unpinned(&now, &format!("{text}{first}\n")).expect_err("a repeated 0001 is kept");
    assert!(
        err.contains("migration 0001 is pinned more than once"),
        "{err}"
    );
}

#[test]
fn regenerating_refuses_an_edited_released_migration() {
    let now = current();
    let mut edited = now.clone();
    edited.get_mut(&1).expect("migration 0001").1 = "0".repeat(96);
    let err = append_unpinned(&now, &pins(&edited)).expect_err("an edited 0001 is not re-pinned");
    assert!(err.contains("migration 0001 (initial) CHANGED"), "{err}");
}

#[test]
fn regenerating_refuses_a_removed_migration() {
    let now = current();
    let mut more = now.clone();
    more.insert(9999, ("gone".to_string(), "0".repeat(96)));
    let err = append_unpinned(&now, &pins(&more)).expect_err("a removed 9999 is not dropped");
    assert!(
        err.contains("pinned migration 9999 no longer exists"),
        "{err}"
    );
}
