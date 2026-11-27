//! A Ferrofin-native database's Jellyfin tables, rebuilt in the shape
//! Jellyfin's own have (gl-1969). `0001` created several with Jellyfin's
//! columns in another order, and with an integer key that is not
//! `AUTOINCREMENT`, so they have no `sqlite_sequence` row. Both Jellyfin and
//! Ferrofin read them by column name, but a Jellyfin database has neither
//! difference.
//!
//! The step rebuilds each table of Jellyfin's schema that holds the same
//! columns in another order, or lacks its `AUTOINCREMENT`. It follows SQLite's
//! procedure for changing a table: Jellyfin's `CREATE TABLE` under another
//! name, the rows copied by column name, the old table dropped, the new one
//! renamed, and then Jellyfin's indexes created on it. SQLite sets the new
//! table's `sqlite_sequence` to the highest id copied. It needs foreign keys
//! off: dropping a table that others reference (`ItemValues`) would otherwise
//! delete their rows. An adopted database keeps the tables Jellyfin made, so
//! the step runs only on a native one.

use sqlx::ConnectOptions as _;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqliteConnectOptions;

use super::drop_objects::quoted;
use crate::Result;

/// Jellyfin's schema: the 12.2 pin, which 12.3 and 12.4 do not change. The
/// tests' fixture is the one copy, which the schema tests compare a fresh
/// database with too.
const JELLYFIN_SCHEMA: &str = include_str!("../../tests/data/jellyfin-12.2-schema.sql");

/// A table's columns in order (name, type, not null, default, key position),
/// and whether its key is `AUTOINCREMENT`.
type Shape = (Vec<(String, String, i64, Option<String>, i64)>, bool);

/// Rebuilds each table of the staged database that holds Jellyfin's columns
/// in another order or without Jellyfin's `AUTOINCREMENT`, and returns their
/// names in name order. A table the database lacks, or one with other
/// columns, is left as it is: the step converges shapes, it never adds or
/// drops a column. Call it with foreign keys off.
pub(super) async fn rebuild(conn: &mut SqliteConnection) -> Result<Vec<String>> {
    let mut jellyfin = SqliteConnectOptions::new()
        .in_memory(true)
        .connect()
        .await?;
    sqlx::raw_sql(JELLYFIN_SCHEMA)
        .execute(&mut jellyfin)
        .await?;
    let tables: Vec<(String, String)> = sqlx::query_as(
        "SELECT name, sql FROM sqlite_master WHERE type = 'table' \
         AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(&mut jellyfin)
    .await?;
    let mut rebuilt = Vec::new();
    for (name, create) in tables {
        let Some(ours) = shape(conn, &name).await? else {
            continue;
        };
        let Some(theirs) = shape(&mut jellyfin, &name).await? else {
            continue;
        };
        // Only Jellyfin's own columns in another order, or the key without
        // `AUTOINCREMENT`: other columns are not a shape this step converges,
        // and the copy would read a missing one as its quoted name.
        if ours == theirs || sorted(&ours) != sorted(&theirs) {
            continue;
        }
        let (table, new) = (quoted(&name), quoted(&format!("{name}_jellyfin")));
        let columns = theirs
            .0
            .iter()
            .map(|(column, ..)| quoted(column))
            .collect::<Vec<_>>()
            .join(", ");
        let indexes: Vec<String> = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 \
             AND sql IS NOT NULL ORDER BY name",
        )
        .bind(&name)
        .fetch_all(&mut jellyfin)
        .await?;
        for statement in [
            create.replacen(&table, &new, 1),
            format!("INSERT INTO {new} ({columns}) SELECT {columns} FROM {table}"),
            format!("DROP TABLE {table}"),
            format!("ALTER TABLE {new} RENAME TO {table}"),
        ]
        .into_iter()
        .chain(indexes)
        {
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(&mut *conn)
                .await?;
        }
        rebuilt.push(name);
    }
    sqlx::Connection::close(jellyfin).await?;
    Ok(rebuilt)
}

/// The columns of `shape` in name order.
fn sorted(shape: &Shape) -> Vec<&(String, String, i64, Option<String>, i64)> {
    let mut columns: Vec<_> = shape.0.iter().collect();
    columns.sort();
    columns
}

/// The shape of `table` in `conn`'s database; `None` when it has no such
/// table.
async fn shape(conn: &mut SqliteConnection, table: &str) -> Result<Option<Shape>> {
    let Some(create): Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .bind(table)
            .fetch_optional(&mut *conn)
            .await?
    else {
        return Ok(None);
    };
    let columns = sqlx::query_as(
        r#"SELECT name, upper(type), "notnull", dflt_value, pk FROM pragma_table_info(?1)"#,
    )
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Some((
        columns,
        create.to_ascii_uppercase().contains("AUTOINCREMENT"),
    )))
}
