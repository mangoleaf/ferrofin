//! `/Shows/NextUp` — the series-keys aggregate's join order, pinned.
//!
//! This lives in `tests/` rather than beside the code because the SQL-boundary
//! ratchet (`crates/ferrofin-db/tests/sql_boundary.rs`) counts `sqlx::query`
//! calls per file under `src/`, and `next_up_service.rs` is already at its
//! ceiling. Ceilings only ratchet down, so a plan test that issues its own
//! `EXPLAIN` belongs outside `src/`.

use ferrofin_core::next_up_service::next_up_series_keys_sql;
use ferrofin_db::Database;

/// Runs `EXPLAIN QUERY PLAN` over `sql`, binding `binds` dummy parameters.
async fn plan(db: &Database, sql: &str, binds: usize) -> Vec<String> {
    let explain = format!("EXPLAIN QUERY PLAN {sql}");
    let mut q = sqlx::query_as::<_, (i64, i64, i64, String)>(&explain);
    for _ in 0..binds {
        q = q.bind("x");
    }
    q.fetch_all(db.pool())
        .await
        .expect("explain query plan")
        .into_iter()
        .map(|(_, _, _, detail)| detail)
        .collect()
}

/// The aggregate must seed from `UserData`, never from `BaseItems`.
///
/// Left to itself SQLite drives this from
/// `(Type, SeriesPresentationUniqueKey)` and then seeks `UserData` once per
/// episode **in the library** — 1,997 seeks on the bench fixture, for a user
/// with a single `UserData` row. Seeded from `UserData (UserId = ?)` the work
/// scales with what the user has actually watched.
///
/// That one statement was 0.92 ms of the endpoint's 2.33 ms CPU per request,
/// which is what pushed `/Shows/NextUp` past its 4-core budget at the
/// benchmark's calibrated 1849 rps and collapsed p50 to 1.5-2 s.
#[tokio::test]
async fn series_keys_seed_from_user_data_not_the_library() {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");

    // Three `TopParentId` placeholders + type/user/placeholder/cutoff.
    let sql = next_up_series_keys_sql(3);
    let plan = plan(&db, &sql, 7).await;

    let first = plan.first().cloned().unwrap_or_default();
    assert!(
        first.contains(" ud ") && first.contains("UserId=?"),
        "the OUTERMOST loop must be UserData seeded on UserId, got: {plan:?}"
    );
    assert!(
        !plan
            .iter()
            .any(|d| d.contains(" bi ") && d.contains("Type=?") && !d.contains("Id=?")),
        "BaseItems must be reached by Id from the UserData row, never driven by \
         Type — that walks every episode in the library: {plan:?}"
    );
}
