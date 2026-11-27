//! The `__EFMigrationsHistory` an export records: every id the source's
//! history holds, plus every schema migration of the target release (and any
//! code migration the table records as applied) it lacks, so that Jellyfin
//! runs only its own code migrations, as it does on any upgrade. The rule is
//! [`super::classify::JELLYFIN_MIGRATIONS`]; this step writes it on the
//! staged copy.
//!
//! A database Ferrofin created itself has no history: it gets exactly the one
//! a fresh install of the target release records ([`fresh_install`], owner
//! decision on gl-1552), so Jellyfin's first start runs no migration at all.
//! Of the code migrations a fresh install records, all but five are seeded
//! without running. The five it runs (`RunMigrationOnSetup`) leave Jellyfin's
//! own plugin repository, which the configuration export always writes, and
//! its two cast receivers, Ferrofin's default too, which the export writes
//! from Ferrofin's settings; and they move trickplay folders out of two older
//! layouts, neither of which Ferrofin writes (it keeps them under
//! `{data}/trickplay/{id[..2]}/{id}`, where the routine moves them to).

use sqlx::SqliteConnection;

use super::classify::{
    CODE_PRODUCT_VERSION, Disposition, JELLYFIN_MIGRATIONS, SCHEMA_PRODUCT_VERSION,
};
use crate::Result;
use crate::database::JellyfinGeneration;

/// The Jellyfin history of a database Ferrofin adopted, as the checks read it
/// from the staged copy.
pub(super) struct SourceHistory {
    /// The release whose history it matches.
    pub(super) generation: &'static JellyfinGeneration,
    /// Its `__EFMigrationsHistory` ids.
    pub(super) ids: Vec<String>,
}

/// What the export did to the database's `__EFMigrationsHistory`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryRewrite {
    /// The Jellyfin release the database was adopted from
    /// ([`JellyfinGeneration::name`]); `None` for a Ferrofin-native database,
    /// which has no history and is given a fresh install's.
    pub generation: Option<&'static str>,
    /// The ids the export added: the schema migrations (and any code
    /// migration recorded as applied) the source's history lacked; for a
    /// Ferrofin-native database, every id a fresh install records.
    pub recorded: Vec<&'static str>,
    /// The code migrations the history leaves out, which Jellyfin runs on its
    /// first start.
    pub left_to_run: Vec<&'static str>,
}

/// Jellyfin's bookkeeping tables as a fresh Jellyfin 12.4 database holds them
/// (their `sqlite_master` text, as EF Core creates them), which a
/// Ferrofin-native database lacks.
const EF_TABLES: [&str; 2] = [
    "CREATE TABLE \"__EFMigrationsHistory\" (\n    \"MigrationId\" TEXT NOT NULL CONSTRAINT \"PK___EFMigrationsHistory\" PRIMARY KEY,\n    \"ProductVersion\" TEXT NOT NULL\n)",
    "CREATE TABLE \"__EFMigrationsLock\" (\n    \"Id\" INTEGER NOT NULL CONSTRAINT \"PK___EFMigrationsLock\" PRIMARY KEY,\n    \"Timestamp\" TEXT NOT NULL\n)",
];

/// Records in the history of the staged database the ids [`to_record`] names
/// that `source` lacks, each with the `ProductVersion` Jellyfin gives it, and
/// empties `__EFMigrationsLock`, so no lock left behind holds Jellyfin's first
/// start. A database with no Jellyfin history (`source` is `None`, a
/// Ferrofin-native one) is given Jellyfin's bookkeeping tables, with the
/// history of a fresh install ([`fresh_install`]) and no lock.
pub(super) async fn rewrite(
    conn: &mut SqliteConnection,
    source: Option<&SourceHistory>,
) -> Result<HistoryRewrite> {
    let Some(source) = source else {
        for table in EF_TABLES {
            sqlx::query(table).execute(&mut *conn).await?;
        }
        return Ok(HistoryRewrite {
            generation: None,
            recorded: record(conn, fresh_install(&JELLYFIN_MIGRATIONS)).await?,
            left_to_run: Vec::new(),
        });
    };
    let has = |id: &str| source.ids.iter().any(|held| held == id);
    let recorded = record(
        conn,
        to_record(&JELLYFIN_MIGRATIONS).filter(|(id, _)| !has(id)),
    )
    .await?;
    let lock: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '__EFMigrationsLock'",
    )
    .fetch_optional(&mut *conn)
    .await?;
    // Every release Ferrofin adopts has one; a database without it has no lock to empty.
    if lock.is_some() {
        sqlx::query(r#"DELETE FROM "__EFMigrationsLock""#)
            .execute(&mut *conn)
            .await?;
    }
    let left_to_run = JELLYFIN_MIGRATIONS
        .iter()
        .filter(|(id, disposition)| *disposition == Disposition::CodeLeftToRun && !has(id))
        .map(|(id, _)| *id)
        .collect();
    Ok(HistoryRewrite {
        generation: Some(source.generation.name),
        recorded,
        left_to_run,
    })
}

/// Inserts each id of `rows` into the history with its `ProductVersion`, and
/// returns the ids, in order.
async fn record(
    conn: &mut SqliteConnection,
    rows: impl Iterator<Item = (&'static str, &'static str)>,
) -> Result<Vec<&'static str>> {
    let mut recorded = Vec::new();
    for (id, version) in rows {
        sqlx::query(
            r#"INSERT INTO "__EFMigrationsHistory" ("MigrationId", "ProductVersion") VALUES (?1, ?2)"#,
        )
        .bind(id)
        .bind(version)
        .execute(&mut *conn)
        .await?;
        recorded.push(id);
    }
    Ok(recorded)
}

/// Every id a fresh install of the target release records, each with the
/// `ProductVersion` it records it with: every id of `table` but the retired
/// ones, the code migrations as applied.
fn fresh_install<'a>(
    table: &'a [(&'static str, Disposition)],
) -> impl Iterator<Item = (&'static str, &'static str)> + 'a {
    table
        .iter()
        .filter_map(|&(id, disposition)| match disposition {
            Disposition::Schema => Some((id, SCHEMA_PRODUCT_VERSION)),
            Disposition::CodeLeftToRun | Disposition::CodeRecordedAsApplied => {
                Some((id, CODE_PRODUCT_VERSION))
            }
            Disposition::Retired => None,
        })
}

/// The ids `table` records whatever the source's history holds, each with the
/// `ProductVersion` Jellyfin records it with.
fn to_record<'a>(
    table: &'a [(&'static str, Disposition)],
) -> impl Iterator<Item = (&'static str, &'static str)> + 'a {
    table
        .iter()
        .filter_map(|&(id, disposition)| match disposition {
            Disposition::Schema => Some((id, SCHEMA_PRODUCT_VERSION)),
            Disposition::CodeRecordedAsApplied => Some((id, CODE_PRODUCT_VERSION)),
            Disposition::CodeLeftToRun | Disposition::Retired => None,
        })
}

#[cfg(test)]
mod tests {
    use super::{
        CODE_PRODUCT_VERSION, Disposition, SCHEMA_PRODUCT_VERSION, fresh_install, to_record,
    };

    #[test]
    fn a_fresh_install_records_every_id_but_the_retired_ones_the_code_ones_as_applied() {
        let table = [
            ("1_Schema", Disposition::Schema),
            ("2_Runs", Disposition::CodeLeftToRun),
            ("3_Applied", Disposition::CodeRecordedAsApplied),
            ("4_Gone", Disposition::Retired),
        ];
        assert_eq!(
            fresh_install(&table).collect::<Vec<_>>(),
            [
                ("1_Schema", SCHEMA_PRODUCT_VERSION),
                ("2_Runs", CODE_PRODUCT_VERSION),
                ("3_Applied", CODE_PRODUCT_VERSION)
            ]
        );
    }

    #[test]
    fn schema_ids_and_code_ids_recorded_as_applied_are_recorded_with_their_version() {
        let table = [
            ("1_Schema", Disposition::Schema),
            ("2_Runs", Disposition::CodeLeftToRun),
            ("3_Applied", Disposition::CodeRecordedAsApplied),
            ("4_Gone", Disposition::Retired),
        ];
        assert_eq!(
            to_record(&table).collect::<Vec<_>>(),
            [
                ("1_Schema", SCHEMA_PRODUCT_VERSION),
                ("3_Applied", CODE_PRODUCT_VERSION)
            ]
        );
    }
}
