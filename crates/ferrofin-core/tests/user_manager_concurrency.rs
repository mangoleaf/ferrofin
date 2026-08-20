//! Concurrency regressions for the `Users` guards.
//!
//! Every guard here is an invariant over the whole `Users` table ("this name is
//! free", "at least one user remains", "at least one admin remains"). Read from
//! the *reader* pool and acted on through the *writer* pool they are a
//! check-then-act: with real parallelism both requests read the pre-mutation
//! world, both pass, and both act.
//!
//! Three things let these tests see that at all, and all three are load
//! bearing:
//!
//! * a **file-backed** database — the production shape, a multi-connection
//!   reader pool plus a separate single-connection writer. An in-memory
//!   `Database` shares ONE pool between the two and hides the window entirely.
//! * a **barrier**, so every request in a round reaches the guard before any of
//!   them reaches its write.
//! * **repeated rounds**: whether a given round interleaves badly is up to the
//!   scheduler, so one round is a coin toss. A guard that is actually atomic
//!   survives every round; a check-then-act loses one within a handful.
//!
//! Each test fails against the pre-fix code with exactly the symptom seen over
//! live HTTP — `UNIQUE constraint failed: Users.Username` surfacing as a `500`,
//! and a `Users` table emptied by two concurrent deletes.

use std::sync::Arc;

use ferrofin_core::FerrofinUserManager;
use ferrofin_db::Database;
use ferrofin_db::enums::PermissionKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::UserManager;
use uuid::Uuid;

/// Rounds per test. A check-then-act guard loses well within this; an atomic
/// one survives any number, so this only trades runtime for reproduction odds.
const ROUNDS: usize = 24;

/// A migrated database on disk: a real reader pool plus the single writer, the
/// only shape in which the reader/writer split can be observed.
async fn file_db(dir: &tempfile::TempDir) -> Database {
    let url = format!("sqlite://{}", dir.path().join("concurrency.db").display());
    let db = Database::connect(&url).await.expect("file connect");
    db.run_migrations().await.expect("migrations apply");
    db
}

/// Grants the administrator permission to `user_id`.
async fn make_admin(db: &Database, user_id: &str) {
    sqlx::query(r#"UPDATE "Permissions" SET "Value" = 1 WHERE "UserId" = ?1 AND "Kind" = ?2"#)
        .bind(user_id)
        .bind(i32::from(PermissionKind::IsAdministrator))
        .execute(db.writer())
        .await
        .expect("grant admin");
}

/// The number of administrators, read straight from the table.
async fn admin_rows(db: &Database) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"SELECT COUNT(*) FROM "Permissions" WHERE "Kind" = ?1 AND "Value" = 1"#,
    )
    .bind(i32::from(PermissionKind::IsAdministrator))
    .fetch_one(db.pool())
    .await
    .expect("count admins")
}

/// The number of user rows.
async fn user_rows(db: &Database) -> i64 {
    sqlx::query_scalar::<_, i64>(r#"SELECT COUNT(*) FROM "Users""#)
        .fetch_one(db.pool())
        .await
        .expect("count users")
}

/// Runs `count` copies of `op` released together by a barrier, collecting their
/// results.
async fn in_lockstep<F, Fut, T>(count: usize, op: F) -> Vec<T>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let gate = Arc::new(tokio::sync::Barrier::new(count));
    let mut tasks = Vec::with_capacity(count);
    for i in 0..count {
        let gate = Arc::clone(&gate);
        let fut = op(i);
        tasks.push(tokio::spawn(async move {
            gate.wait().await;
            fut.await
        }));
    }
    let mut out = Vec::with_capacity(count);
    for task in tasks {
        out.push(task.await.expect("join"));
    }
    out
}

/// Concurrent creations of ONE name: exactly one wins, and every loser is told
/// the name is taken. A loser that reaches the UNIQUE `IX_Users_Username`
/// instead gets a database error — which the API layer renders as `500`, not the
/// contract's `400`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_creates_of_one_name_yield_one_user_and_no_database_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(&dir).await;
    let mgr = Arc::new(FerrofinUserManager::new(db.clone()));

    for round in 0..ROUNDS {
        let name = format!("contested{round}");
        let results = in_lockstep(8, |_| {
            let mgr = Arc::clone(&mgr);
            let name = name.clone();
            async move { mgr.create_user(&name).await }
        })
        .await;

        let mut created = 0;
        for result in results {
            match result {
                Ok(_) => created += 1,
                Err(ServiceError::InvalidInput(msg)) => {
                    assert!(
                        msg.contains("already exists"),
                        "unexpected rejection: {msg}"
                    );
                }
                Err(other) => panic!(
                    "round {round}: a losing create must be rejected as invalid input \
                     (HTTP 400), not surfaced as {other} (HTTP 500)"
                ),
            }
        }
        assert_eq!(
            created, 1,
            "round {round}: exactly one creation may succeed"
        );
    }

    assert_eq!(
        user_rows(&db).await,
        i64::try_from(ROUNDS).expect("round count fits"),
        "one row per contested name"
    );
}

/// Concurrent renames onto ONE free name: same guard, same requirement.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_renames_onto_one_name_reject_the_losers_as_invalid_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(&dir).await;
    let mgr = Arc::new(FerrofinUserManager::new(db.clone()));

    for round in 0..ROUNDS {
        let mut ids = Vec::new();
        for i in 0..6 {
            let user = mgr
                .create_user(&format!("user{round}-{i}"))
                .await
                .expect("create");
            ids.push((Uuid::parse_str(&user.id).expect("uuid"), user.username));
        }
        let target = format!("contested{round}");

        let results = in_lockstep(ids.len(), |i| {
            let mgr = Arc::clone(&mgr);
            let (id, old) = ids[i].clone();
            let target = target.clone();
            async move { mgr.rename_user(id, &old, &target).await }
        })
        .await;

        let mut renamed = 0;
        for result in results {
            match result {
                Ok(()) => renamed += 1,
                Err(ServiceError::InvalidInput(msg)) => {
                    assert!(
                        msg.contains("already exists"),
                        "unexpected rejection: {msg}"
                    );
                }
                Err(other) => panic!(
                    "round {round}: a losing rename must be rejected as invalid input \
                     (HTTP 400), not surfaced as {other} (HTTP 500)"
                ),
            }
        }
        assert_eq!(renamed, 1, "round {round}: exactly one rename may succeed");
    }
}

/// The last two accounts, deleted at once: the "at least one user" guard has to
/// survive both. Losing it empties the table — every account and its data gone,
/// with nobody left who can log in.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_deletes_of_the_last_two_users_leave_one_behind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(&dir).await;
    let mgr = Arc::new(FerrofinUserManager::new(db.clone()));

    for round in 0..ROUNDS {
        // Top the table back up to exactly two accounts.
        while user_rows(&db).await < 2 {
            mgr.create_user(&Uuid::new_v4().simple().to_string())
                .await
                .expect("create");
        }
        let ids: Vec<Uuid> = mgr.get_user_ids().await.expect("ids");
        assert_eq!(ids.len(), 2, "round {round}: two accounts to contend over");

        let results = in_lockstep(2, |i| {
            let mgr = Arc::clone(&mgr);
            let id = ids[i];
            async move { mgr.delete_user(id).await }
        })
        .await;
        // Either outcome is fine per request; the table is what must hold.
        drop(results);

        assert_eq!(
            user_rows(&db).await,
            1,
            "round {round}: the last user may never be deleted, \
             however the two deletes interleave"
        );
    }
}

/// The same for the admin guard. Ordinary accounts exist alongside so the
/// user-count guard is not what saves us: two administrators deleted at once
/// must leave one, or the server is unrecoverable through its own API.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_deletes_of_the_last_two_admins_leave_one_admin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = file_db(&dir).await;
    let mgr = Arc::new(FerrofinUserManager::new(db.clone()));

    for i in 0..4 {
        mgr.create_user(&format!("ordinary{i}"))
            .await
            .expect("create ordinary");
    }

    for round in 0..ROUNDS {
        // Top back up to exactly two administrators.
        let mut admins = Vec::new();
        while admin_rows(&db).await < 2 {
            let user = mgr
                .create_user(&Uuid::new_v4().simple().to_string())
                .await
                .expect("create");
            make_admin(&db, &user.id).await;
        }
        for id in mgr.get_user_ids().await.expect("ids") {
            let user = mgr.get_user_by_id(id).await.expect("get").expect("present");
            if ferrofin_core::user_entity_ext::has_permission(
                db.pool(),
                &user.id,
                PermissionKind::IsAdministrator,
            )
            .await
            .expect("permission")
            {
                admins.push(id);
            }
        }
        assert_eq!(admins.len(), 2, "round {round}: two admins to contend over");

        let results = in_lockstep(2, |i| {
            let mgr = Arc::clone(&mgr);
            let id = admins[i];
            async move { mgr.delete_user(id).await }
        })
        .await;
        drop(results);

        assert_eq!(
            admin_rows(&db).await,
            1,
            "round {round}: the last administrator may never be deleted"
        );
    }
}
