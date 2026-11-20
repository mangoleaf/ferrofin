//! `ferrofin-server export-jellyfin`: copies a stopped data directory and a
//! consistent snapshot of its database into a new directory, leaving the
//! source untouched, and writes Ferrofin's configuration over the copy as the
//! XML Jellyfin reads. It is the way back to Jellyfin once the export is
//! complete; until then the subcommand is hidden (see [`crate::config::Command`]).
//!
//! The copy is [`ferrofin_db::export::export_data_dir`] and the configuration
//! is [`ferrofin_core::config_export`]; this file holds the subcommand's
//! arguments and its loss report, so the phases that extend the export add
//! their flags and their report lines here.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use ferrofin_core::config_export::{self, ConfigExport};
use ferrofin_db::export::{DatabaseEdits, EXPORTED_DATABASE, HistoryRewrite, LOSS_REPORT};

use crate::config::Config;

/// The `export-jellyfin` subcommand's arguments. The source is the data
/// directory the server would use (`--data-dir`, `FERROFIN_DATA_DIR`,
/// `config.toml`, the default), and its database the one the server would open.
#[derive(Debug, Clone, clap::Args)]
pub struct ExportJellyfinArgs {
    /// The directory to write the export into.
    #[arg(long = "out", value_name = "DIR")]
    pub out: PathBuf,
}

/// How long the check for a running server waits on a listener that accepted
/// the connection to answer `GET /System/Info/Public`. A local server answers
/// within milliseconds; something that accepts and never answers is not one.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The most of the answer the check reads. A server's `/System/Info/Public`
/// is well under a kilobyte; something else streaming on the port is not
/// read without end.
const PROBE_ANSWER_LIMIT: usize = 64 * 1024;

/// Jellyfin's backups directory under the output (`{DataPath}/backups`).
const JELLYFIN_BACKUPS: &str = "data/backups";

/// Exports `config`'s data directory into `args.out`, writes the loss report
/// into it ([`LOSS_REPORT`]) and prints what it wrote and the report.
///
/// Before anything is written, the export refuses to run when completing it
/// would be unsafe or meaningless ([`ferrofin_db::export::refuse_unsafe`], a
/// server for this data directory answering on its port), and reads the
/// configuration, so a directory Ferrofin never served (it has none) is refused
/// too. The export is then written into `<out>.partial` and renamed to `out`
/// whole once complete; a failed one removes it, so it leaves no output.
///
/// # Errors
/// Returns the refusal, or the export's error with the source and output
/// named, when a file cannot be copied, the database snapshot fails or the
/// staged database fails its checks, the source changed while it was
/// exported, the configuration cannot be written, or the loss report cannot
/// be gathered (the exported database, the backups directory or the
/// scheduled-task files cannot be read) or written.
pub async fn run(config: &Config, args: &ExportJellyfinArgs) -> anyhow::Result<()> {
    log_warnings_to_stderr();
    let (source, out) = (&config.data_dir, &args.out);
    let nothing_written = || {
        format!(
            "failed to export {} into {}; nothing was written",
            source.display(),
            out.display()
        )
    };
    let target = ferrofin_db::export::refuse_unsafe(source, &config.database_path(), out)
        .await
        .with_context(nothing_written)?;
    // Where the server would listen and mount: the Networking page's saved
    // port and base URL outrank `config.toml`'s, and a flag or variable
    // outranks both.
    let saved =
        crate::state::load_network_configuration(&crate::bootstrap::application_paths(config))
            .await;
    let port = crate::listen::ports(config, &saved).0;
    let base_url = crate::base_url::resolve(&config.base_url, config.base_url_from_env, &saved);
    let probed = config.clone();
    tokio::task::spawn_blocking(move || refuse_running_server(&probed, port, &base_url))
        .await
        .context("the check for a running server did not finish")
        .and_then(|refused| refused)
        .with_context(nothing_written)?;
    let configuration = read_configuration(config)
        .await
        .context("its configuration cannot be read")
        .with_context(nothing_written)?;
    let replaced = target.exists();
    let partial = ferrofin_db::export::partial_output(&target);
    let created = create_partial(&partial)
        .await
        .with_context(nothing_written)?;
    let written = async {
        let summary =
            ferrofin_db::export::export_data_dir(source, &config.database_path(), &partial).await?;
        write_configuration(&configuration, &partial).await?;
        let report = loss_report(
            &summary.edits,
            &configuration,
            &report_inputs(config, &partial).await?,
        );
        let written = partial.join(LOSS_REPORT);
        tokio::fs::write(&written, &report)
            .await
            .with_context(|| format!("could not write {}", written.display()))?;
        tokio::fs::rename(&partial, &target)
            .await
            .with_context(|| {
                format!(
                    "could not rename {} to {}",
                    partial.display(),
                    target.display()
                )
            })?;
        anyhow::Ok((summary, report))
    }
    .await;
    let (summary, report) = match written {
        Ok(written) => written,
        Err(error) => {
            let cleanup = partial.clone();
            let removed = match tokio::task::spawn_blocking(move || {
                remove_partial(&cleanup, created.as_deref())
            })
            .await
            {
                Ok(Ok(())) => "its partial output was removed".to_owned(),
                Ok(Err(e)) => format!(
                    "delete {} by hand: it could not be removed ({e})",
                    partial.display()
                ),
                Err(e) => format!(
                    "delete {} by hand: its removal did not finish ({e})",
                    partial.display()
                ),
            };
            return Err(error.context(format!(
                "failed to export {} into {}; {removed}",
                source.display(),
                out.display()
            )));
        }
    };
    print_summary(source, out, &target, &summary, &configuration, replaced);
    print!("\n{report}");
    Ok(())
}

/// Prints what the export of `source` into `out` (resolved to `target`) wrote:
/// `summary`'s copy and database changes, and `configuration`'s files.
fn print_summary(
    source: &Path,
    out: &Path,
    target: &Path,
    summary: &ferrofin_db::export::ExportSummary,
    configuration: &ConfigExport,
    replaced: bool,
) {
    println!(
        "Exported {} into {}: the database snapshot at {} and {} other files ({} bytes), then {} configuration files for Jellyfin over them. The source is unchanged.",
        source.display(),
        out.display(),
        target.join(EXPORTED_DATABASE).display(),
        summary.files,
        summary.bytes,
        configuration.files.len()
    );
    let history = &summary.edits.history;
    if let Some(generation) = history.generation {
        println!(
            "Its Jellyfin migration history (from a database of the Jellyfin {generation} generation): {} recorded that Jellyfin would otherwise apply again, {} code migrations left for Jellyfin to run on its first start.",
            history.recorded.len(),
            history.left_to_run.len()
        );
    }
    if replaced {
        println!(
            "The export replaced the empty directory {} with a new directory that has the source's permissions; a shell inside it must change into it again to see the files.",
            target.display()
        );
    }
    println!(
        "What it left behind is below, and in {}.",
        target.join(LOSS_REPORT).display()
    );
}

/// What the loss report reads from the exported database file and the
/// source's directories.
#[derive(Debug, Default)]
struct ReportInputs {
    /// The size of the exported database, in bytes.
    database_bytes: u64,
    /// The backup archives in the output that Jellyfin cannot restore.
    backups: usize,
    /// Whether the source keeps scheduled-task state Jellyfin does not read.
    task_state: bool,
}

/// Reads what the loss report needs from the files: the size of
/// the database exported into `partial`, the backups copied there, and
/// whether `config`'s directories keep scheduled-task state.
async fn report_inputs(config: &Config, partial: &Path) -> anyhow::Result<ReportInputs> {
    let database = partial.join(EXPORTED_DATABASE);
    let database_bytes = tokio::fs::metadata(&database)
        .await
        .with_context(|| format!("could not read {}", database.display()))?
        .len();
    let dir = partial.join(JELLYFIN_BACKUPS);
    let backups = tokio::task::spawn_blocking(move || {
        ferrofin_core::backup::archives_jellyfin_cannot_restore(&dir)
            .with_context(|| format!("could not list the backups in {}", dir.display()))
    })
    .await
    .context("the backups' listing did not finish")??;
    let mut task_state = false;
    for file in [
        config.config_dir.join("task_triggers.json"),
        config.data_dir.join("task_results.json"),
    ] {
        task_state |= tokio::fs::try_exists(&file)
            .await
            .with_context(|| format!("could not read {}", file.display()))?;
    }
    Ok(ReportInputs {
        database_bytes,
        backups,
        task_state,
    })
}

/// The loss report: what the export removed from the database (`edits`), what
/// Jellyfin's first start will change, the settings `configuration` does not
/// carry, and, from `inputs`, the room the first start needs and the backups
/// and scheduled-task state Jellyfin cannot use. It names no path of the
/// source or the output, so two exports of one source write the same.
fn loss_report(
    edits: &DatabaseEdits,
    configuration: &ConfigExport,
    inputs: &ReportInputs,
) -> String {
    use std::fmt::Write as _;

    let mut report = String::from("What the export to Jellyfin left behind\n");
    report.push_str("\nFerrofin-only data in the database, removed from the export:\n");
    for table in &edits.dropped.tables {
        let _ = writeln!(
            report,
            "  {} ({}): {}.",
            table.name,
            count(table.rows, "row", "rows"),
            ferrofin_db::export::table_note(&table.name)
        );
    }
    if !edits.dropped.indexes.is_empty() {
        let _ = writeln!(
            report,
            "  {} of Ferrofin's own on Jellyfin's tables (an index holds no data of its own).",
            count(edits.dropped.indexes.len() as u64, "index", "indexes")
        );
    }
    if edits.dropped.tables.is_empty() && edits.dropped.indexes.is_empty() {
        report.push_str("  None.\n");
    }

    first_start(&mut report, &edits.history, inputs.database_bytes);

    report.push_str("\nSettings Jellyfin is not given:\n");
    for setting in &configuration.not_exported {
        let _ = writeln!(
            report,
            "  {} in {}: {}.",
            setting.setting, setting.file, setting.reason
        );
    }
    report.push_str(
        "  Settings given to Ferrofin in config.toml, FERROFIN_* environment variables or command-line flags are not exported.\n",
    );

    report.push_str("\nBackups and scheduled tasks:\n");
    if inputs.backups > 0 {
        let _ = writeln!(
            report,
            "  {} in {JELLYFIN_BACKUPS} made by Ferrofin, which Jellyfin cannot restore.",
            count(inputs.backups as u64, "backup archive", "backup archives")
        );
    }
    if inputs.task_state {
        report.push_str(
            "  Scheduled tasks: Ferrofin keeps their triggers in task_triggers.json and their last results in task_results.json, which Jellyfin does not read. Its tasks keep what its own files held before adoption, or its defaults.\n",
        );
    }
    if inputs.backups == 0 && !inputs.task_state {
        report.push_str("  None.\n");
    }
    report
}

/// The loss report's section on what Jellyfin changes on its first start: the
/// code migrations `history` leaves to run, and the room they take beside a
/// database of `database_bytes`.
fn first_start(report: &mut String, history: &HistoryRewrite, database_bytes: u64) {
    use std::fmt::Write as _;

    report.push_str("\nWhat Jellyfin will change on its first start");
    let Some(generation) = history.generation else {
        report.push_str(
            ":\n  Not known: the database was never adopted from Jellyfin, so it has no Jellyfin migration history.\n",
        );
        return;
    };
    let _ = writeln!(
        report,
        ", running the code migrations its history leaves out (the database was adopted as Jellyfin {generation}):"
    );
    for (routine, effect) in ferrofin_db::export::first_start_effects(&history.left_to_run) {
        let _ = writeln!(report, "  {routine}: {effect}.");
    }
    if history.left_to_run.is_empty() {
        report.push_str("  None.\n");
        return;
    }
    // One copy for the backup, one for the VACUUM that follows the migrations.
    // Jellyfin copies only for a routine marked `JellyfinMigrationBackup(JellyfinDb
    // = true)`; every set a supported source leaves holds one
    // (HarmonizeConflictingUserData among them), so every non-empty one copies.
    let _ = writeln!(
        report,
        "  Before them Jellyfin copies the database ({}) into data/SQLiteBackups, and after them it rebuilds it, so the output's volume needs about {} free besides the export. A start whose migrations fail restores the database from that copy and keeps the copy, one more each time.",
        mebibytes(database_bytes),
        mebibytes(database_bytes.saturating_mul(2))
    );
}

/// `n` and the noun for it, `one` or `many`.
fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `bytes` in mebibytes, to one decimal, rounded up: a size the report gives
/// is space the operator must find.
fn mebibytes(bytes: u64) -> String {
    let tenths = bytes.saturating_mul(10).div_ceil(1024 * 1024);
    format!("{}.{} MiB", tenths / 10, tenths % 10)
}

/// Sends warnings to stderr. The export runs without the server's logging,
/// which would create `{data}/log` in the source, so the warnings the
/// configuration reads give (a document that does not parse exports its
/// defaults) go there. sqlx is left out: it warns about any statement over a
/// second, which the snapshot's `VACUUM INTO` of a real library always is.
fn log_warnings_to_stderr() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn,sqlx=off"))
        // Colour only to a terminal, and never under `NO_COLOR`, which an
        // explicit `with_ansi` would otherwise override.
        .with_ansi(
            std::io::IsTerminal::is_terminal(&std::io::stderr())
                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
        )
        .without_time()
        .with_target(false)
        .try_init();
}

/// Creates the export's working directory, private to its owner (the copy
/// gives it the source's mode once complete), and refuses one that is already
/// there: another export started writing it since the checks ran. Returns the
/// outermost directory above it that this created, for a failed export to
/// remove again.
async fn create_partial(partial: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut created = None;
    let mut ancestor = partial.parent();
    while let Some(dir) = ancestor.filter(|dir| !dir.as_os_str().is_empty() && !dir.exists()) {
        created = Some(dir.to_owned());
        ancestor = dir.parent();
    }
    if let Some(parent) = partial.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let mut builder = tokio::fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    match builder.create(partial).await {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => anyhow::bail!(
            "another export is writing {}; let it finish, or delete it once it has stopped",
            partial.display()
        ),
        created_dir => created_dir.with_context(|| {
            format!(
                "could not create {} (an export is written beside its output and renamed onto \
                 it, so the output's parent must be writable)",
                partial.display()
            )
        })?,
    }
    Ok(created)
}

/// Removes a failed export's `partial`, and the directories above it the
/// export created (`created` the outermost) while they are empty. The copy
/// keeps the source's modes, so a directory in it may be read-only; the
/// export made every one of them, and makes each writable to empty it.
fn remove_partial(partial: &Path, created: Option<&Path>) -> std::io::Result<()> {
    match std::fs::remove_dir_all(partial) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            make_writable(partial)?;
            std::fs::remove_dir_all(partial)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        removed => removed?,
    }
    if let Some(created) = created {
        let mut dir = partial.parent();
        while let Some(empty) = dir.filter(|dir| dir.starts_with(created)) {
            if std::fs::remove_dir(empty).is_err() {
                break;
            }
            dir = empty.parent();
        }
    }
    Ok(())
}

/// Gives the owner write and search permission on `dir` and every directory
/// under it, links not followed.
#[cfg(unix)]
fn make_writable(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        let mode = std::fs::symlink_metadata(&dir)?.permissions().mode();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode | 0o700))?;
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_writable(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Refuses an export while this data directory's server answers on `port`,
/// the one it would listen on ([`crate::listen::ports`]): `GET
/// {base_url}/System/Info/Public` there, under the base URL it would mount
/// ([`crate::base_url::resolve`]), returns the id in `{config}/system_id`.
/// Another server on that port (a Jellyfin the operator started already, say)
/// is not this one, and is no reason to refuse. The database lock check finds
/// a server on this data directory however it is reached; this one also finds
/// it when the database is on a filesystem that does not share locks, if it
/// answers plain HTTP on `config.bind_addr` (localhost when unspecified): a
/// server that only redirects to HTTPS, or listens on other addresses only,
/// is left to the lock check.
fn refuse_running_server(config: &Config, port: u16, base_url: &str) -> anyhow::Result<()> {
    use std::io::{Read as _, Write as _};

    let Ok(id) = std::fs::read_to_string(config.config_dir.join("system_id")) else {
        return Ok(());
    };
    let id = id.trim();
    let host = match config.bind_addr {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => std::net::Ipv4Addr::LOCALHOST.into(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => std::net::Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    let address = std::net::SocketAddr::new(host, port);
    // A refused, failed or unanswered connection is no server at all.
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&address, PROBE_TIMEOUT) else {
        return Ok(());
    };
    // `resolve` normalizes (a leading `/`, no trailing one, or empty) but does
    // not escape: `my jellyfin` would make an invalid request line.
    let request = format!(
        "GET {}/System/Info/Public HTTP/1.0\r\nHost: localhost\r\n\r\n",
        ferrofin_networking::escape_base_url(base_url)
    );
    let sent = stream
        .set_write_timeout(Some(PROBE_TIMEOUT))
        .and_then(|()| stream.write_all(request.as_bytes()));
    if sent.is_err() {
        return Ok(());
    }
    // An HTTP/1.0 answer ends when the server closes; what arrived before the
    // deadline or the limit still counts.
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    let mut response = Vec::new();
    let mut chunk = [0_u8; 4096];
    while response.len() < PROBE_ANSWER_LIMIT {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => response.extend_from_slice(&chunk[..read]),
        }
    }
    let body = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(&[][..], |at| &response[at + 4..]);
    let answering = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|info| info.get("Id")?.as_str().map(str::to_owned));
    if answering.is_some_and(|answering| answering.eq_ignore_ascii_case(id)) {
        anyhow::bail!(
            "this data directory's server is running at http://{address} (it answers with its server id {id}); stop it first"
        );
    }
    Ok(())
}

/// Reads Ferrofin's configuration as the files Jellyfin reads: the six
/// configuration documents, the server id and each library's `options.xml`.
///
/// It is read from the directories the server uses, so a library's media
/// paths under them take Jellyfin's `%AppDataPath%` and `%MetadataPath%` forms.
async fn read_configuration(config: &Config) -> anyhow::Result<ConfigExport> {
    let mut export = config_export::export_configuration(&config.config_dir)?;
    let libraries =
        config_export::export_library_options(&config.data_dir, &config.config_dir).await?;
    export.files.extend(libraries.files);
    export.not_exported.extend(libraries.not_exported);
    Ok(export)
}

/// Writes `export`'s files under `out`. Each goes through a `.partial` file and
/// a rename, which also replaces a copy the source left read-only. A directory
/// the source left read-only (the copy keeps its mode) cannot take one, and the
/// export fails, naming it.
async fn write_configuration(export: &ConfigExport, out: &Path) -> anyhow::Result<()> {
    for file in &export.files {
        let path = out.join(&file.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let mut partial = path.clone().into_os_string();
        partial.push(".partial");
        tokio::fs::write(&partial, &file.contents)
            .await
            .with_context(|| format!("could not write {}", Path::new(&partial).display()))?;
        tokio::fs::rename(&partial, &path)
            .await
            .with_context(|| format!("could not replace {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ferrofin_core::config_export::{ConfigExport, NotExported};
    use ferrofin_db::export::{DatabaseEdits, DroppedObjects, DroppedTable, HistoryRewrite};

    use super::{ReportInputs, loss_report, mebibytes};

    fn table(name: &str, rows: u64) -> DroppedTable {
        DroppedTable {
            name: name.to_owned(),
            rows,
        }
    }

    /// Every kind of line the report has, in its order: the golden file is
    /// what an operator reads.
    #[test]
    fn the_loss_report_reads_as_its_golden_file() {
        let edits = DatabaseEdits {
            history: HistoryRewrite {
                generation: Some("10.11.8"),
                recorded: vec!["20260113102337_AddLinkedChildrenTable"],
                left_to_run: vec![
                    "20260522092304_UpdateNormalizedUsername",
                    "20260531160000_DisableLegacyAuthorization",
                    "20260912120000_HarmonizeConflictingUserData",
                ],
            },
            dropped: DroppedObjects {
                tables: vec![
                    table("FerrofinIntroSkipperSegments", 3),
                    table("FerrofinMeta", 1),
                    table("FerrofinNew", 0),
                    table("_sqlx_migrations", 52),
                ],
                indexes: vec![
                    "FerrofinIX_BaseItems_Name".to_owned(),
                    "FerrofinIX_UserData_UserId".to_owned(),
                ],
            },
        };
        let configuration = ConfigExport {
            files: Vec::new(),
            not_exported: vec![NotExported {
                file: "config/network.xml".to_owned(),
                setting: "CertificatePath".to_owned(),
                reason: "a path in Ferrofin's filesystem",
            }],
        };
        let inputs = ReportInputs {
            database_bytes: 43_198_464,
            backups: 2,
            task_state: true,
        };
        assert_eq!(
            loss_report(&edits, &configuration, &inputs),
            include_str!("export_cmd/testdata/loss_report.txt")
        );
    }

    /// A Ferrofin-native database has no Jellyfin history to tell what
    /// Jellyfin runs from, and a source without backups or task state leaves
    /// nothing outside the database.
    #[test]
    fn an_empty_section_says_so() {
        let edits = DatabaseEdits {
            history: HistoryRewrite::default(),
            dropped: DroppedObjects {
                tables: vec![table("_sqlx_migrations", 1)],
                indexes: Vec::new(),
            },
        };
        let report = loss_report(&edits, &ConfigExport::default(), &ReportInputs::default());
        assert!(
            report.contains(
                "What Jellyfin will change on its first start:\n  Not known: the database was never adopted from Jellyfin, so it has no Jellyfin migration history.\n\nSettings Jellyfin is not given:\n  Settings given"
            ),
            "{report}"
        );
        assert!(
            report.ends_with("Backups and scheduled tasks:\n  None.\n"),
            "{report}"
        );
        assert!(report.contains("  _sqlx_migrations (1 row): "), "{report}");
        assert!(!report.contains("indexes"), "{report}");

        // An adopted database whose history leaves nothing to run.
        let edits = DatabaseEdits {
            history: HistoryRewrite {
                generation: Some("12.2.0"),
                ..HistoryRewrite::default()
            },
            ..edits
        };
        let report = loss_report(&edits, &ConfigExport::default(), &ReportInputs::default());
        assert!(
            report.contains("(the database was adopted as Jellyfin 12.2.0):\n  None.\n\n"),
            "{report}"
        );
        assert!(!report.contains("SQLiteBackups"), "{report}");
    }

    #[test]
    fn sizes_are_mebibytes_rounded_up_to_one_decimal() {
        assert_eq!(mebibytes(0), "0.0 MiB");
        assert_eq!(mebibytes(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(mebibytes(1), "0.1 MiB", "rounded up");
        assert_eq!(mebibytes(u64::MAX), "1759218604441.6 MiB");
    }
}
