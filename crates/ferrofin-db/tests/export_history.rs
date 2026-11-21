//! The `__EFMigrationsHistory` an export records (gl-1549, gl-1551): every id
//! of the target Jellyfin release classified once, as that release records it,
//! and the history written through `ferrofin_db::export::export_data_dir` for a
//! source of every generation Ferrofin adopts, with each combination of the
//! ids it may or may not carry.

use std::path::{Path, PathBuf};

use ferrofin_db::export::{
    CODE_PRODUCT_VERSION, DOCKER, Disposition, EXPORTED_DATABASE, JELLYFIN_MIGRATIONS,
    SCHEMA_PRODUCT_VERSION, SourceDirs, export_data_dir,
};
use ferrofin_db::{JELLYFIN_GENERATIONS, JellyfinGeneration};
use sqlx::ConnectOptions as _;
use sqlx::sqlite::SqliteConnectOptions;

/// `MigrationId` and `ProductVersion` of every row a fresh Jellyfin 12.4
/// install records, tab-separated, captured from `jellyfin/jellyfin:12.4`
/// started on an empty directory.
const FRESH_12_4_HISTORY: &str = include_str!("data/jellyfin-12.4-fresh-history.tsv");

fn classified(id: &str) -> Option<Disposition> {
    JELLYFIN_MIGRATIONS
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(_, disposition)| *disposition)
}

#[test]
fn every_id_a_fresh_install_records_is_classified_once_as_jellyfin_records_it() {
    assert!(
        JELLYFIN_MIGRATIONS.windows(2).all(|w| w[0].0 < w[1].0),
        "in id order, and no id classified twice"
    );
    let fresh: Vec<(&str, &str)> = FRESH_12_4_HISTORY
        .lines()
        .map(|line| line.split_once('\t').expect("id and version"))
        .collect();
    assert_eq!(fresh.len(), 105);
    for (id, version) in &fresh {
        let disposition = classified(id).unwrap_or_else(|| panic!("{id} is not classified"));
        match *version {
            SCHEMA_PRODUCT_VERSION => assert_eq!(disposition, Disposition::Schema, "{id}"),
            CODE_PRODUCT_VERSION => assert!(
                matches!(
                    disposition,
                    Disposition::CodeLeftToRun | Disposition::CodeRecordedAsApplied
                ),
                "{id} is a code migration, classified {disposition:?}"
            ),
            other => panic!("{id} has the version {other}, neither EF's nor Jellyfin's"),
        }
    }
    // Nothing more: what a fresh install does not record is a retired id.
    for (id, disposition) in JELLYFIN_MIGRATIONS {
        let recorded = fresh.iter().any(|(fresh, _)| *fresh == id);
        assert_eq!(recorded, disposition != Disposition::Retired, "{id}");
    }
    let count = |wanted: Disposition| {
        JELLYFIN_MIGRATIONS
            .iter()
            .filter(|(_, d)| *d == wanted)
            .count()
    };
    assert_eq!(count(Disposition::Schema), 56);
    assert_eq!(
        count(Disposition::CodeLeftToRun) + count(Disposition::CodeRecordedAsApplied),
        49
    );
}

#[test]
fn every_id_a_supported_source_may_carry_is_classified() {
    for generation in &JELLYFIN_GENERATIONS {
        for id in generation
            .migration_ids
            .iter()
            .chain(generation.additional_ids)
            .chain(generation.optional_ids)
        {
            assert!(classified(id).is_some(), "{} carries {id}", generation.name);
        }
    }
}

/// The ids a database of `generation` records: those it always carries, and
/// each of its optional ids whose bit is set in `optional`.
fn ids_of(generation: &JellyfinGeneration, optional: usize) -> Vec<&'static str> {
    let mut ids: Vec<&str> = generation
        .migration_ids
        .iter()
        .chain(generation.additional_ids)
        .filter(|id| !generation.optional_ids.contains(id))
        .chain(
            generation
                .optional_ids
                .iter()
                .enumerate()
                .filter(|(bit, _)| optional & (1 << bit) != 0)
                .map(|(_, id)| id),
        )
        .copied()
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Every id `generation` may record, optional ones included: the history of
/// an install upgraded through every release before it.
fn every_id_of(generation: &str) -> Vec<&'static str> {
    let generation = JELLYFIN_GENERATIONS
        .iter()
        .find(|g| g.name == generation)
        .unwrap();
    ids_of(generation, usize::MAX)
}

/// The code migrations Jellyfin 12.4 ran on its first start on a pristine
/// 10.11.8 database (its `InternalCodeMigration` log lines).
const RUN_ON_10_11_8: [&str; 19] = [
    "20260113120000_MigrateLinkedChildren",
    "20260113230000_CleanupOrphanedExtras",
    "20260115120000_FixIncorrectOwnerIdRelationships",
    "20260508120000_MergeDuplicateMusicArtists",
    "20260508130000_MergeDuplicatePeople",
    "20260522092304_UpdateNormalizedUsername",
    "20260525010000_CleanupOrphanedExternalData",
    "20260531160000_DisableLegacyAuthorization",
    "20260610120000_RefreshCleanNamesAndValues",
    "20260722120000_RefreshForcedSortNames",
    "20260729120000_RestorePlaylistChildrenFromMetadata",
    "20260821120000_RecomputeSeriesPresentationKey",
    "20260825200000_ConsolidateLocalizedUserViews",
    "20260831100000_EnableLocalSimilarityProviders",
    "20260908120000_RepairAlternateVersionLinks",
    "20260911120000_StripEmbeddedLinkedChildren",
    "20260912120000_HarmonizeConflictingUserData",
    "20260915104305_FixNullEncoderPreset",
    "20260915120000_MigrateRatingLevels",
];

/// The same less `UpdateNormalizedUsername`, which a 10.11.10 or 10.11.11
/// database records. Jellyfin 12.4's own first start runs it there too, but
/// only because the first schema migration it applies reverts every later
/// one, and `AddNormalizedUsername`'s `Down` drops that routine's row
/// (`JellyfinMigrationService.MigrateStepAsync`); on an export no schema
/// migration is pending, so nothing reverts and it stays recorded.
const RUN_ON_10_11_11: [&str; 18] = [
    "20260113120000_MigrateLinkedChildren",
    "20260113230000_CleanupOrphanedExtras",
    "20260115120000_FixIncorrectOwnerIdRelationships",
    "20260508120000_MergeDuplicateMusicArtists",
    "20260508130000_MergeDuplicatePeople",
    "20260525010000_CleanupOrphanedExternalData",
    "20260531160000_DisableLegacyAuthorization",
    "20260610120000_RefreshCleanNamesAndValues",
    "20260722120000_RefreshForcedSortNames",
    "20260729120000_RestorePlaylistChildrenFromMetadata",
    "20260821120000_RecomputeSeriesPresentationKey",
    "20260825200000_ConsolidateLocalizedUserViews",
    "20260831100000_EnableLocalSimilarityProviders",
    "20260908120000_RepairAlternateVersionLinks",
    "20260911120000_StripEmbeddedLinkedChildren",
    "20260912120000_HarmonizeConflictingUserData",
    "20260915104305_FixNullEncoderPreset",
    "20260915120000_MigrateRatingLevels",
];

/// For each generation, in [`JELLYFIN_GENERATIONS`]' order, what Jellyfin
/// 12.4's first start on a database of it did, less what the database records
/// (the first-start logs `gates/roundtrip/routes.sh` names: 10.11.9 records
/// 10.11.8's ids, 10.11.10 records 10.11.11's, and 12.3 and 12.4 record
/// 12.2's): how many schema migrations it applied, which the export records,
/// and the code migrations it ran, which the export leaves out of the history
/// for it to run.
const LEFT_TO_RUN: [(&str, usize, &[&str]); 5] = [
    ("10.11.8", 20, &RUN_ON_10_11_8),
    ("10.11.11", 18, &RUN_ON_10_11_11),
    (
        "12.0.0",
        1,
        &[
            "20260908120000_RepairAlternateVersionLinks",
            "20260911120000_StripEmbeddedLinkedChildren",
            "20260912120000_HarmonizeConflictingUserData",
            "20260915104305_FixNullEncoderPreset",
            "20260915120000_MigrateRatingLevels",
        ],
    ),
    (
        "12.1.0",
        1,
        &[
            "20260912120000_HarmonizeConflictingUserData",
            "20260915104305_FixNullEncoderPreset",
            "20260915120000_MigrateRatingLevels",
        ],
    ),
    ("12.2.0", 0, &[]),
];

/// How many of the target release's schema migrations `history` records.
fn schema_ids_in(history: &[(String, String)]) -> usize {
    history
        .iter()
        .filter(|(id, _)| classified(id) == Some(Disposition::Schema))
        .count()
}

/// The version an adopted source's rows carry, which the export must keep.
const SOURCE_VERSION: &str = "9.0.11";

/// A data directory whose database Ferrofin adopted from a Jellyfin that
/// recorded `ids`, with a lock a crashed Jellyfin left behind, or with no lock
/// table at all.
async fn source_with(scratch: &Path, ids: &[&str], lock_table: bool) -> PathBuf {
    let source = scratch.join("source");
    let database = source.join(EXPORTED_DATABASE);
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let mut conn = SqliteConnectOptions::new()
        .filename(&database)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    let lock = [
        r#"CREATE TABLE "__EFMigrationsLock" ("Id" INTEGER NOT NULL CONSTRAINT "PK___EFMigrationsLock" PRIMARY KEY, "Timestamp" TEXT NOT NULL)"#,
        r#"INSERT INTO "__EFMigrationsLock" VALUES (1, '2026-11-01 00:00:00')"#,
    ];
    for statement in [
        "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)",
        r#"CREATE TABLE "__EFMigrationsHistory" ("MigrationId" TEXT NOT NULL CONSTRAINT "PK___EFMigrationsHistory" PRIMARY KEY, "ProductVersion" TEXT NOT NULL)"#,
    ]
    .into_iter()
    .chain(lock.into_iter().filter(|_| lock_table))
    {
        sqlx::query(statement).execute(&mut conn).await.unwrap();
    }
    for id in ids {
        sqlx::query(r#"INSERT INTO "__EFMigrationsHistory" VALUES (?1, ?2)"#)
            .bind(id)
            .bind(SOURCE_VERSION)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    sqlx::Connection::close(conn).await.unwrap();
    source
}

/// [`source_with`] a lock table.
async fn source(scratch: &Path, ids: &[&str]) -> PathBuf {
    source_with(scratch, ids, true).await
}

/// The answers of `query` on the exported database.
async fn exported<T>(out: &Path, query: &'static str) -> Vec<T>
where
    T: Send + Unpin + for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow>,
{
    let mut conn = SqliteConnectOptions::new()
        .filename(out.join(EXPORTED_DATABASE))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let rows = sqlx::query_as(query).fetch_all(&mut conn).await.unwrap();
    sqlx::Connection::close(conn).await.unwrap();
    rows
}

/// The exported database's history, in id order.
async fn exported_history(out: &Path) -> Vec<(String, String)> {
    exported(
        out,
        r#"SELECT "MigrationId", "ProductVersion" FROM "__EFMigrationsHistory" ORDER BY 1"#,
    )
    .await
}

/// How many locks the exported database holds.
async fn exported_locks(out: &Path) -> i64 {
    exported::<(i64,)>(out, r#"SELECT COUNT(*) FROM "__EFMigrationsLock""#).await[0].0
}

/// The first schema id `history` leaves out below one it records. Jellyfin
/// applies every EF id its history lacks, and applying one that sorts before
/// a recorded one first reverts every later one (`JellyfinMigrationService.cs`).
fn schema_gap(history: &[(String, String)]) -> Option<&'static str> {
    let recorded = |id: &str| history.iter().any(|(held, _)| held == id);
    let schema = || {
        JELLYFIN_MIGRATIONS
            .iter()
            .filter(|(_, d)| *d == Disposition::Schema)
            .map(|(id, _)| *id)
    };
    let last = schema().rfind(|id| recorded(id))?;
    schema().find(|id| !recorded(id) && *id < last)
}

#[tokio::test]
async fn every_generation_records_the_target_schema_and_leaves_the_code_migrations_it_lacks() {
    assert_eq!(
        LEFT_TO_RUN.map(|(name, _, _)| name),
        JELLYFIN_GENERATIONS.map(|g| g.name)
    );
    let fresh: Vec<&str> = FRESH_12_4_HISTORY
        .lines()
        .map(|line| line.split_once('\t').expect("id and version").0)
        .collect();
    let schema: Vec<&str> = JELLYFIN_MIGRATIONS
        .iter()
        .filter(|(_, d)| *d == Disposition::Schema)
        .map(|(id, _)| *id)
        .collect();
    for (generation, (_, applied, left_to_run)) in JELLYFIN_GENERATIONS.iter().zip(LEFT_TO_RUN) {
        // every combination of the ids it may or may not carry
        for optional in 0..1_usize << generation.optional_ids.len() {
            let case = format!("{} with optional ids {optional:#b}", generation.name);
            let ids = ids_of(generation, optional);
            let scratch = tempfile::tempdir().unwrap();
            let source = source(scratch.path(), &ids).await;
            let out = scratch.path().join("out");

            let summary = export_data_dir(
                &SourceDirs::new(&source),
                &DOCKER,
                &source.join(EXPORTED_DATABASE),
                &out,
            )
            .await
            .unwrap();

            // The source's rows as they were, and every schema id of the
            // target it lacks, exactly as the target spells it.
            let added: Vec<&str> = schema
                .iter()
                .copied()
                .filter(|id| !ids.contains(id))
                .collect();
            let mut expected: Vec<(String, String)> = ids
                .iter()
                .map(|id| ((*id).to_owned(), SOURCE_VERSION.to_owned()))
                .chain(
                    added
                        .iter()
                        .map(|id| ((*id).to_owned(), SCHEMA_PRODUCT_VERSION.to_owned())),
                )
                .collect();
            expected.sort();
            let history = exported_history(&out).await;
            assert_eq!(history, expected, "{case}");
            assert_eq!(schema_ids_in(&history), 56, "{case}");
            assert_eq!(schema_gap(&history), None, "{case}");
            assert_eq!(
                exported_locks(&out).await,
                0,
                "{case}: the lock a crashed Jellyfin left is gone"
            );
            let rewrite = &summary.edits.history;
            assert_eq!(rewrite.generation, Some(generation.name), "{case}");
            assert_eq!(rewrite.recorded, added, "{case}");
            assert_eq!(added.len(), applied, "{case}");
            assert_eq!(rewrite.left_to_run, *left_to_run, "{case}");
            // Once Jellyfin has run them, its history is the one its own
            // first start on the source leaves: the source's ids and every id
            // a fresh install records.
            let mut after: Vec<&str> = history
                .iter()
                .map(|(id, _)| id.as_str())
                .chain(left_to_run.iter().copied())
                .collect();
            after.sort_unstable();
            let mut first_start: Vec<&str> = ids.iter().chain(&fresh).copied().collect();
            first_start.sort_unstable();
            first_start.dedup();
            assert_eq!(after, first_start, "{case}");
        }
    }
}

#[tokio::test]
async fn a_database_without_a_lock_table_is_rewritten_without_one() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source_with(scratch.path(), &every_id_of("10.11.8"), false).await;
    let out = scratch.path().join("out");

    export_data_dir(
        &SourceDirs::new(&source),
        &DOCKER,
        &source.join(EXPORTED_DATABASE),
        &out,
    )
    .await
    .unwrap();

    assert_eq!(schema_ids_in(&exported_history(&out).await), 56);
    let lock: Vec<(i64,)> = exported(
        &out,
        "SELECT 1 FROM sqlite_master WHERE name = '__EFMigrationsLock'",
    )
    .await;
    assert_eq!(lock, []);
}

#[test]
fn a_schema_id_left_out_below_a_recorded_one_is_a_gap() {
    let history = [
        "20200514181226_AddActivityLog",
        "20261002210711_MakeOwnerIdIndexesPartial",
    ]
    .map(|id| (id.to_owned(), SCHEMA_PRODUCT_VERSION.to_owned()));
    assert_eq!(schema_gap(&history), Some("20200613202153_AddUsers"));
    assert_eq!(schema_gap(&history[..1]), None);
}
