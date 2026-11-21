//! Removing Ferrofin's own objects from the exported database (gl-1553):
//! `_sqlx_migrations`, every table whose name starts with `Ferrofin` and every
//! `FerrofinIX_` index. They are found in `sqlite_master`, never listed here,
//! so a table a later migration adds cannot stay behind; Jellyfin's schema is
//! what remains. Each table's rows are counted first, for the loss report.

use sqlx::SqliteConnection;

use crate::Result;

/// One table the export removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedTable {
    /// Its name.
    pub name: String,
    /// How many rows it held.
    pub rows: u64,
}

/// What the export removed from its copy of the database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DroppedObjects {
    /// Ferrofin's tables, `_sqlx_migrations` included, in name order.
    pub tables: Vec<DroppedTable>,
    /// The `FerrofinIX_` indexes on Jellyfin's tables, in name order (those on
    /// Ferrofin's tables go with them).
    pub indexes: Vec<String>,
}

/// Counts the rows of each of Ferrofin's tables, then drops the tables and
/// the `FerrofinIX_` indexes left on Jellyfin's. Every count comes before any
/// drop: dropping a table deletes its rows first, and the foreign keys cascade
/// that into the Ferrofin tables that reference it.
pub(super) async fn remove(conn: &mut SqliteConnection) -> Result<DroppedObjects> {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND (name GLOB 'Ferrofin*' OR name = '_sqlx_migrations') ORDER BY name",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut tables = Vec::with_capacity(names.len());
    for name in names {
        let rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {}",
            quoted(&name)
        )))
        .fetch_one(&mut *conn)
        .await?;
        tables.push(DroppedTable {
            name,
            rows: rows.unsigned_abs(),
        });
    }
    for table in &tables {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE {}",
            quoted(&table.name)
        )))
        .execute(&mut *conn)
        .await?;
    }
    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND name GLOB 'FerrofinIX_*' \
         ORDER BY name",
    )
    .fetch_all(&mut *conn)
    .await?;
    for index in &indexes {
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP INDEX {}", quoted(index))))
            .execute(&mut *conn)
            .await?;
    }
    Ok(DroppedObjects { tables, indexes })
}

/// `name` as an SQLite identifier, any `"` in it doubled.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use sqlx::ConnectOptions as _;
    use sqlx::sqlite::SqliteConnectOptions;

    use super::{DroppedTable, quoted, remove};

    #[test]
    fn a_quote_in_a_name_is_doubled() {
        assert_eq!(quoted(r#"Ferrofin"X"#), r#""Ferrofin""X""#);
    }

    /// Rows are counted before any drop: dropping the parent first would
    /// cascade into the child and count it empty.
    #[tokio::test]
    async fn counts_every_table_before_a_cascade_empties_it() {
        let mut conn = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true)
            .connect()
            .await
            .unwrap();
        for statement in [
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)",
            "CREATE TABLE Items (Id INTEGER PRIMARY KEY, Name TEXT)",
            r#"CREATE TABLE "FerrofinA" ("Id" INTEGER PRIMARY KEY AUTOINCREMENT)"#,
            r#"CREATE TABLE "FerrofinB" ("AId" INTEGER REFERENCES "FerrofinA" ("Id") ON DELETE CASCADE)"#,
            r#"CREATE INDEX "FerrofinIX_Items_Name" ON Items (Name)"#,
            r#"CREATE INDEX "FerrofinIX_B_AId" ON "FerrofinB" ("AId")"#,
            r#"CREATE INDEX "IX_Items_Name" ON Items (Name)"#,
            r#"CREATE TABLE "ferrofinlower" ("X" TEXT)"#,
            r#"INSERT INTO "FerrofinA" DEFAULT VALUES"#,
            r#"INSERT INTO "FerrofinB" VALUES (1), (1)"#,
            "INSERT INTO _sqlx_migrations VALUES (1)",
        ] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }

        let dropped = remove(&mut conn).await.unwrap();

        let table = |name: &str, rows| DroppedTable {
            name: name.to_owned(),
            rows,
        };
        assert_eq!(
            dropped.tables,
            [
                table("FerrofinA", 1),
                table("FerrofinB", 2),
                table("_sqlx_migrations", 1)
            ]
        );
        assert_eq!(dropped.indexes, ["FerrofinIX_Items_Name"]);
        let left: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE name NOT LIKE 'sqlite_autoindex%' ORDER BY name",
        )
        .fetch_all(&mut conn)
        .await
        .unwrap();
        // The match is case-sensitive, as Ferrofin's names are; the sequence
        // row of the AUTOINCREMENT table went with it.
        assert_eq!(
            left,
            ["IX_Items_Name", "Items", "ferrofinlower", "sqlite_sequence"]
        );
        let sequences: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_sequence")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(sequences, 0);
    }
}
