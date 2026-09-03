//! `/Shows/NextUp` — the series-keys aggregate's plan, pinned.
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

/// The aggregate is driven from `BaseItems` through a `Type = ?` index and
/// reaches `UserData` by `(UserId, ItemId)` on its covering index, with no
/// join-order pin and no table scan.
///
/// The planner takes the `(Type, SeriesPresentationUniqueKey, …)` index — its
/// order serves the `GROUP BY` — and walks the episodes of the libraries in
/// scope once, seeking the user's row for each: 6 ms for 7,490 episodes on
/// the adopted bench database, 40 keys out. The `CROSS JOIN` this statement
/// used to carry seeded from `UserData (UserId = ?)` instead and iterated the
/// `TopParentId IN (…)` list once per `UserData` row — fine for one row
/// against three parents, 5,044 × 1,975 ≈ 10 M probes and 1.4 s once the
/// scope was every folder in the library and the user had a history. With the
/// scope fixed to the library folders both orders are single-digit
/// milliseconds; the v12 shape is kept because it is v12's, and this test
/// pins that no pin crept back.
#[tokio::test]
async fn series_keys_are_index_driven_with_no_join_pin() {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");

    // user/type/placeholder + three `TopParentId` placeholders + cutoff.
    let sql = next_up_series_keys_sql(3, false);
    assert!(!sql.contains("CROSS JOIN"), "no join-order pin: {sql}");
    let steps = plan(&db, &sql, 7).await;

    assert!(
        steps
            .iter()
            .any(|d| d.starts_with("SEARCH bi USING INDEX") && d.contains("Type=?")),
        "BaseItems must be read through a Type-led index, got: {steps:?}"
    );
    assert!(
        steps
            .iter()
            .any(|d| d.contains(" ud ") && d.contains("UserId=? AND ItemId=?")),
        "UserData must be reached by (UserId, ItemId) from the BaseItems row, got: {steps:?}"
    );
    assert!(
        !steps.iter().any(|d| d.contains("SCAN")),
        "no full scan of either table: {steps:?}"
    );

    // With a limit the statement carries one more bind and a LIMIT.
    let limited = next_up_series_keys_sql(3, true);
    assert!(limited.ends_with("LIMIT ?"), "bound LIMIT: {limited}");
    plan(&db, &limited, 8).await;
}
