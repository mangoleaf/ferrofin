//! The database pools the composition root opens from its configuration.

use ferrofin_server::bootstrap::open_database;
use ferrofin_server::config::Config;

/// The configured statement cache (`db_statement_cache`) reaches the reader
/// and the writer pools: past it, a connection keeps no more prepared
/// statements, however many distinct ones it runs.
#[tokio::test]
async fn the_configured_statement_cache_reaches_both_pools() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = Config {
        db_pool: Some(1),
        db_statement_cache: Some(120),
        ..Config::test_stub(tmp.path())
    };
    let db = open_database(&config).await.expect("database opens");
    for pool in [db.pool(), db.writer()] {
        let mut conn = pool.acquire().await.expect("connection");
        for i in 0..300 {
            sqlx::query(sqlx::AssertSqlSafe(format!("SELECT {i}")))
                .execute(&mut *conn)
                .await
                .expect("select");
        }
        assert_eq!(sqlx::Connection::cached_statements_size(&*conn), 120);
    }
}
