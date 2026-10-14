//! Refuses to start a server that has been set up before but whose database is
//! missing or empty.
//!
//! Port of Jellyfin 12.2's `JellyfinMigrationService.EnsureExistingDatabaseAsync`.
//! Without it, a set-up server whose database went missing (a mount that did
//! not come up, the wrong data directory, a deleted file) creates a new, empty
//! database and [`crate::seed::seed_default_admin`] gives it an administrator
//! with no password: anyone who can reach the server logs in as `admin` with an
//! empty password and owns it.
//!
//! Jellyfin runs the check when `IsStartupWizardCompleted` is true, the start
//! mode is `MediaServer` and no `--restore-archive` is pending. Ferrofin has no
//! start modes — every start is a media-server start — so that condition always
//! holds. It has no `--restore-archive` either: its only restore is the one
//! `POST /Backup/Restore` schedules, which [`crate::serve_once`] applies before
//! this check runs. Jellyfin skips the check for a pending restore only because
//! the restore replaces the database later in its startup; here it already has,
//! so the check reads the restored database instead of being skipped. A
//! restore that brought a database back passes it; one that did not refuses,
//! just as a Jellyfin restore that fails never reaches the media server.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use ferrofin_db::MigrationHistory;

use crate::config::Config;

/// Why the database of a server that has been set up cannot be used, in
/// Jellyfin's wording; `None` when it records at least one migration.
fn database_problem(history: MigrationHistory) -> Option<&'static str> {
    match history {
        MigrationHistory::Missing => Some("the database does not exist"),
        MigrationHistory::NoHistory => Some("the database has no migration history"),
        MigrationHistory::Empty => Some("the migration history of the database is empty"),
        MigrationHistory::Recorded => None,
    }
}

/// The refusal for a server whose configuration files `completed_in` record
/// setup as completed, when its database at `database` has `history`; `None`
/// when the server may start (setup never completed, or the database records
/// its migrations). `adoptable` are the other places a missing database may be
/// restored to: where a database adopted from Jellyfin is opened in place.
fn refusal(
    completed_in: &[PathBuf],
    history: MigrationHistory,
    database: &Path,
    adoptable: &[PathBuf],
    data_dir: &Path,
) -> Option<String> {
    if completed_in.is_empty() {
        return None;
    }
    let problem = database_problem(history)?;
    let files = join_paths(completed_in, " and ");
    // A missing database resolves to `ferrofin.db`, but one adopted from
    // Jellyfin was opened where it lay, so restoring it there works as well. A
    // file that exists is the one the server opens and the place to restore to.
    let restore_to = if history == MigrationHistory::Missing && !adoptable.is_empty() {
        format!(
            "{} (or, for a database adopted from Jellyfin, to {})",
            database.display(),
            join_paths(adoptable, " or ")
        )
    } else {
        database.display().to_string()
    };
    Some(format!(
        "This server has been set up before (IsStartupWizardCompleted is true in {files}), but \
         {problem} ({database}). Ferrofin will not start an existing server with an empty \
         database. To continue, either restore the previous database to {restore_to}; or start \
         over and keep this server's settings by setting IsStartupWizardCompleted to false in \
         {files} (users, watch history and everything else stored in the database will not come \
         back); or set up a new server with empty configuration and data directories (the data \
         directory is {data_dir}, set by --data-dir, FERROFIN_DATA_DIR or data_dir in \
         config.toml; the configuration directory by FERROFIN_CONFIG_DIR or config_dir in \
         config.toml).",
        database = database.display(),
        data_dir = data_dir.display(),
    ))
}

/// The displayed `paths`, joined by `separator`.
fn join_paths(paths: &[PathBuf], separator: &str) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(separator)
}

/// Fails when the persisted configuration says setup was completed but the
/// database [`Config::database_path`] resolves to is missing or has no
/// recorded migrations. Must run before the database is opened, which would
/// create and migrate an empty one; reads both without writing anything.
///
/// # Errors
///
/// The refusal, naming the three ways out; or the failure to read the
/// configuration or the database.
pub(crate) async fn ensure_existing_database(config: &Config) -> anyhow::Result<()> {
    let completed_in =
        ferrofin_core::configuration_manager::startup_wizard_completed_files(&config.config_dir)
            .context("failed to read whether the startup wizard was completed")?;
    if completed_in.is_empty() {
        return Ok(());
    }
    let database = config.database_path();
    let history = ferrofin_db::inspect_migration_history(&database)
        .await
        .with_context(|| format!("failed to read the database `{}`", database.display()))?;
    let adoptable = config.jellyfin_database_paths();
    match refusal(
        &completed_in,
        history,
        &database,
        &adoptable,
        &config.data_dir,
    ) {
        Some(message) => Err(anyhow::anyhow!(message)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{MigrationHistory, ensure_existing_database, refusal};
    use crate::config::Config;
    use std::path::{Path, PathBuf};

    fn system_json(root: &Path) -> PathBuf {
        root.join("config").join("system.json")
    }

    #[rstest::rstest]
    #[case::missing(MigrationHistory::Missing, Some("the database does not exist"))]
    #[case::no_history(
        MigrationHistory::NoHistory,
        Some("the database has no migration history")
    )]
    #[case::empty_history(
        MigrationHistory::Empty,
        Some("the migration history of the database is empty")
    )]
    #[case::recorded(MigrationHistory::Recorded, None)]
    fn a_set_up_server_needs_a_recorded_history(
        #[case] history: MigrationHistory,
        #[case] problem: Option<&str>,
    ) {
        let flag = PathBuf::from("/srv/config/system.json");
        let database = PathBuf::from("/srv/ferrofin.db");
        let message = refusal(
            std::slice::from_ref(&flag),
            history,
            &database,
            &[],
            Path::new("/srv"),
        );
        assert_eq!(message.is_some(), problem.is_some());
        if let (Some(message), Some(problem)) = (message, problem) {
            assert!(message.contains(problem), "{message}");
            assert!(message.contains("/srv/config/system.json"), "{message}");
            assert!(message.contains("/srv/ferrofin.db"), "{message}");
            // The three ways out.
            assert!(
                message.contains("restore the previous database"),
                "{message}"
            );
            assert!(
                message.contains("setting IsStartupWizardCompleted to false"),
                "{message}"
            );
            assert!(
                message.contains("empty configuration and data directories"),
                "{message}"
            );
            assert!(message.contains("FERROFIN_DATA_DIR"), "{message}");
            assert!(message.contains("data_dir in config.toml"), "{message}");
            assert!(message.contains("config_dir in config.toml"), "{message}");
        }
    }

    #[rstest::rstest]
    #[case::missing(MigrationHistory::Missing)]
    #[case::no_history(MigrationHistory::NoHistory)]
    #[case::empty_history(MigrationHistory::Empty)]
    #[case::recorded(MigrationHistory::Recorded)]
    fn a_server_never_set_up_is_never_refused(#[case] history: MigrationHistory) {
        assert_eq!(
            refusal(
                &[],
                history,
                Path::new("/srv/ferrofin.db"),
                &[],
                Path::new("/srv")
            ),
            None
        );
    }

    #[test]
    fn every_file_that_records_the_flag_is_named() {
        let files = [
            PathBuf::from("/srv/config/system.json"),
            PathBuf::from("/srv/config/system.xml"),
        ];
        let message = refusal(
            &files,
            MigrationHistory::Missing,
            Path::new("/srv/ferrofin.db"),
            &[],
            Path::new("/srv"),
        )
        .expect("refused");
        assert!(
            message.contains("/srv/config/system.json and /srv/config/system.xml"),
            "{message}"
        );
    }

    /// A missing database may be restored where an adopted Jellyfin database
    /// is opened; a file that exists names only itself.
    #[rstest::rstest]
    #[case::missing(MigrationHistory::Missing, true)]
    #[case::empty_history(MigrationHistory::Empty, false)]
    fn a_missing_database_may_be_restored_to_the_adopted_location(
        #[case] history: MigrationHistory,
        #[case] names_adopted: bool,
    ) {
        let adoptable = [
            PathBuf::from("/srv/jellyfin.db"),
            PathBuf::from("/srv/data/jellyfin.db"),
        ];
        let message = refusal(
            &[PathBuf::from("/srv/config/system.json")],
            history,
            Path::new("/srv/ferrofin.db"),
            &adoptable,
            Path::new("/srv"),
        )
        .expect("refused");
        assert_eq!(
            message.contains(
                "restore the previous database to /srv/ferrofin.db (or, for a database adopted \
                 from Jellyfin, to /srv/jellyfin.db or /srv/data/jellyfin.db)"
            ),
            names_adopted,
            "{message}"
        );
    }

    /// Writes a `system.json` recording `completed` under `root`.
    fn write_wizard_flag(root: &Path, completed: bool) {
        let path = system_json(root);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("config dir");
        let mut configuration =
            ferrofin_core::configuration_manager::default_server_configuration();
        configuration.is_startup_wizard_completed = completed;
        std::fs::write(
            &path,
            serde_json::to_vec(&configuration).expect("serialize"),
        )
        .expect("system.json");
    }

    /// Upstream `CheckFirstTimeRunOrMigration_SetUpServerWithoutDatabase_ThrowsWithoutCreatingIt`.
    #[tokio::test]
    async fn set_up_server_without_database_refuses_without_creating_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config::test_stub(tmp.path());
        write_wizard_flag(tmp.path(), true);
        let before = std::fs::read(system_json(tmp.path())).expect("read");

        let error = ensure_existing_database(&config)
            .await
            .expect_err("refused");

        let message = format!("{error:#}");
        assert!(message.contains("the database does not exist"), "{message}");
        assert!(
            message.contains(&system_json(tmp.path()).display().to_string()),
            "{message}"
        );
        assert!(!config.database_path().exists(), "no database created");
        assert!(!config.data_dir.exists(), "nothing created in the data dir");
        assert_eq!(
            std::fs::read(system_json(tmp.path())).expect("read"),
            before
        );
    }

    /// Upstream `CheckFirstTimeRunOrMigration_SetUpServerWithEmptyDatabaseFile_Throws`.
    #[tokio::test]
    async fn set_up_server_with_empty_database_file_refuses() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config::test_stub(tmp.path());
        write_wizard_flag(tmp.path(), true);
        std::fs::create_dir_all(&config.data_dir).expect("data dir");
        std::fs::write(config.database_path(), []).expect("empty file");

        let error = ensure_existing_database(&config)
            .await
            .expect_err("refused");

        let message = format!("{error:#}");
        assert!(
            message.contains("the database has no migration history"),
            "{message}"
        );
        assert_eq!(
            std::fs::metadata(config.database_path())
                .expect("metadata")
                .len(),
            0
        );
    }

    /// Upstream `CheckFirstTimeRunOrMigration_SetUpServerWithEmptyHistory_Throws`.
    #[tokio::test]
    async fn set_up_server_with_empty_history_refuses() {
        use sqlx::ConnectOptions as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config::test_stub(tmp.path());
        write_wizard_flag(tmp.path(), true);
        std::fs::create_dir_all(&config.data_dir).expect("data dir");
        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(config.database_path())
            .create_if_missing(true)
            .connect()
            .await
            .expect("create");
        sqlx::raw_sql(
            r#"CREATE TABLE "__EFMigrationsHistory" ("MigrationId" TEXT, "ProductVersion" TEXT)"#,
        )
        .execute(&mut conn)
        .await
        .expect("history table");
        sqlx::Connection::close(conn).await.expect("close");

        let error = ensure_existing_database(&config)
            .await
            .expect_err("refused");

        let message = format!("{error:#}");
        assert!(
            message.contains("the migration history of the database is empty"),
            "{message}"
        );
    }

    /// Upstream `CheckFirstTimeRunOrMigration_SetUpServerWithHistory_Passes`: a
    /// server set up on a database that records its migrations starts, and the
    /// check leaves the history as it found it.
    #[tokio::test]
    async fn set_up_server_with_history_starts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config::test_stub(tmp.path());
        write_wizard_flag(tmp.path(), false);
        ensure_existing_database(&config)
            .await
            .expect("a server never set up starts");
        let db = crate::bootstrap::open_database(&config)
            .await
            .expect("open");
        db.close().await;
        let before = std::fs::read(config.database_path()).expect("read");
        write_wizard_flag(tmp.path(), true);

        ensure_existing_database(&config)
            .await
            .expect("a set-up server with its database starts");

        assert_eq!(std::fs::read(config.database_path()).expect("read"), before);
    }

    /// A fresh data directory (no configuration at all) starts, as today.
    #[tokio::test]
    async fn fresh_data_dir_starts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config::test_stub(tmp.path());
        ensure_existing_database(&config).await.expect("starts");
        assert!(!config.data_dir.exists(), "nothing created");
    }

    /// The start-over path: with the flag reset, a missing database is a new
    /// server again (upstream `StartOverTests.DeletedDatabase_WizardReset_StartsAsNewServer`).
    #[tokio::test]
    async fn wizard_reset_starts_over() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config::test_stub(tmp.path());
        write_wizard_flag(tmp.path(), false);
        ensure_existing_database(&config).await.expect("starts");
    }

    /// A Jellyfin configuration directory not yet imported carries the flag in
    /// `system.xml`; its adopted database passes, a missing one refuses.
    #[tokio::test]
    async fn adopted_jellyfin_config_is_checked_against_its_database() {
        use sqlx::ConnectOptions as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config::test_stub(tmp.path());
        std::fs::create_dir_all(&config.config_dir).expect("config dir");
        let xml = config.config_dir.join("system.xml");
        std::fs::write(
            &xml,
            "<ServerConfiguration><IsStartupWizardCompleted>true</IsStartupWizardCompleted>\
             </ServerConfiguration>",
        )
        .expect("system.xml");

        let message = format!(
            "{:#}",
            ensure_existing_database(&config)
                .await
                .expect_err("refused")
        );
        assert!(message.contains(&xml.display().to_string()), "{message}");
        assert!(
            !config.config_dir.join("system.json").exists(),
            "the configuration is not imported by the check"
        );

        let jellyfin_db = config.data_dir.join("data").join("jellyfin.db");
        std::fs::create_dir_all(jellyfin_db.parent().expect("parent")).expect("data dir");
        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&jellyfin_db)
            .create_if_missing(true)
            .connect()
            .await
            .expect("create");
        sqlx::raw_sql(
            r#"CREATE TABLE "__EFMigrationsHistory" ("MigrationId" TEXT, "ProductVersion" TEXT);
               INSERT INTO "__EFMigrationsHistory" VALUES ('20250000000000_Initial', '9.0.0');"#,
        )
        .execute(&mut conn)
        .await
        .expect("history");
        sqlx::Connection::close(conn).await.expect("close");
        assert_eq!(config.database_path(), jellyfin_db);

        ensure_existing_database(&config)
            .await
            .expect("the adopted database passes");
    }
}
