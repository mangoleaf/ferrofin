//! `ferrofin-server export-jellyfin`: copies a stopped data directory and a
//! consistent snapshot of its database into a new directory in the layout of
//! Jellyfin's docker image, leaving the source untouched, and writes
//! Ferrofin's configuration over the copy as the XML Jellyfin reads. It is the
//! way back to Jellyfin once the export is complete; until then the subcommand
//! is hidden (see [`crate::config::Command`]).
//!
//! The copy is [`ferrofin_db::export::export_data_dir`] and the configuration
//! is [`ferrofin_core::config_export`]; this file holds the subcommand's
//! arguments and its loss report, so the phases that extend the export add
//! their flags and their report lines here.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use ferrofin_core::config_export::{self, ConfigExport};
use ferrofin_db::export::{
    ContainerFile, DOCKER, DatabaseEdits, EXPORTED_DATABASE, ExportSummary, HistoryRewrite,
    LOSS_REPORT, LeftOut, LibraryContainer, Preflight, SourceDirs,
};

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

/// Exports `config`'s data directory into `args.out`, in the layout of
/// Jellyfin's docker image ([`DOCKER`]), writes the loss report into it
/// ([`LOSS_REPORT`]) and prints what it wrote and the report.
///
/// Before anything is written, the export refuses to run when completing it
/// would be unsafe or meaningless ([`ferrofin_db::export::refuse_unsafe`], a
/// server for this data directory answering on its port), and reads the
/// configuration, so a directory Ferrofin never served (it has none) is refused
/// too; then it prints the preflight, the size of each class of file it
/// copies and the free space. The export is then written into `<out>.partial`
/// and renamed to `out` whole once complete; a failed one removes it, so it
/// leaves no output. Each library's options are read once the database's rows
/// have said where the server ran.
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
    let dirs = source_dirs(config);
    let preflight =
        ferrofin_db::export::refuse_unsafe(&dirs, &DOCKER, &config.database_path(), out)
            .await
            .with_context(nothing_written)?;
    let target = preflight.target.clone();
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
    let (mut configuration, libraries) = read_configuration(config)
        .await
        .with_context(nothing_written)?;
    print_preflight(&preflight, config, &dirs);
    let replaced = target.exists();
    let partial = ferrofin_db::export::partial_output(&target);
    let created = create_partial(&partial)
        .await
        .with_context(nothing_written)?;
    let written = async {
        let summary =
            ferrofin_db::export::export_data_dir(&dirs, &DOCKER, &config.database_path(), &partial)
                .await?;
        let libraries = library_options(config, &summary, libraries).await?;
        configuration.files.extend(libraries.files);
        configuration.not_exported.extend(libraries.not_exported);
        write_configuration(&configuration, &partial).await?;
        let report = loss_report(
            &summary.edits,
            &configuration,
            &report_inputs(config, &dirs, &summary, &partial).await?,
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
    print_paths(config, &target, &summary);
    print!("\n{report}");
    Ok(())
}

/// Reads Ferrofin's configuration as the files Jellyfin reads: the six
/// configuration documents and the server id, which a directory Ferrofin never
/// served lacks, and each library's `options.xml`, read now so a library the
/// export cannot write is refused before anything is copied
/// ([`library_options`] reads them again when the server ran elsewhere).
async fn read_configuration(config: &Config) -> anyhow::Result<(ConfigExport, ConfigExport)> {
    let documents = config_export::export_configuration(&config.config_dir)
        .context("its configuration cannot be read")?;
    let libraries = config_export::export_library_options(
        &config.data_dir,
        &config.config_dir,
        &config.data_dir,
    )
    .await
    .context("its libraries' options cannot be read")?;
    Ok((documents, libraries))
}

/// Each library's options with their media paths reversed from where the
/// server ran (the data directory the aggregate root row names,
/// [`ferrofin_db::export::PathRewrite::ran_at`]) rather than from where they
/// are read: `read`, read from the data directory, unless the rows name
/// another one.
async fn library_options(
    config: &Config,
    summary: &ExportSummary,
    read: ConfigExport,
) -> anyhow::Result<ConfigExport> {
    let Some(ran_at) = summary.edits.paths.ran_at.as_deref().map(Path::new) else {
        return Ok(read);
    };
    if ran_at == config.data_dir {
        return Ok(read);
    }
    config_export::export_library_options(&config.data_dir, &config.config_dir, ran_at)
        .await
        .context("its libraries' options cannot be read")
}

/// The directories of `config`'s server, as the export reads them: its
/// metadata directory as its settings name it (the default when they cannot
/// be read, which the configuration's read then refuses), and its cache, log
/// and web client directories, which hold nothing Jellyfin reads.
fn source_dirs(config: &Config) -> SourceDirs {
    SourceDirs {
        program_data: config.data_dir.clone(),
        config: config.config_dir.clone(),
        metadata: config_export::metadata_dir(&config.data_dir, &config.config_dir)
            .unwrap_or_else(|_| config.data_dir.join("metadata")),
        not_needed: vec![
            config.cache_dir.clone(),
            config.data_dir.join("log"),
            config.web_dir.clone(),
        ],
    }
}

/// Whether the metadata directory the server's settings name, outside its
/// default place, is not there (or cannot be looked at).
fn metadata_missing(config: &Config, dirs: &SourceDirs) -> bool {
    dirs.metadata != SourceDirs::new(&config.data_dir).metadata
        && !dirs.metadata.try_exists().unwrap_or(false)
}

/// Prints the preflight: what each class of file the export copies holds, and
/// the room beside the output, before anything is copied; and a warning when
/// the metadata directory of `dirs` is missing, while it can still be stopped.
fn print_preflight(preflight: &Preflight, config: &Config, dirs: &SourceDirs) {
    println!(
        "Preflight: the export needs about {} beside {} (what it copies, and a second copy of the database while it is taken), and {} is free there. What it copies:",
        amount(preflight.needed),
        preflight.target.display(),
        amount(preflight.available)
    );
    for (class, bytes) in &preflight.classes {
        println!("  {class}: {}", amount(*bytes));
    }
    if metadata_missing(config, dirs) {
        println!(
            "Warning: the metadata directory the server's settings name, {}, is not there, so no artwork or other metadata will be carried; stop this export (Ctrl-C) and run it where that directory is reachable at that path.",
            dirs.metadata.display()
        );
    }
}

/// Prints how the export laid out the source and rewrote the stored paths.
fn print_paths(config: &Config, target: &Path, summary: &ExportSummary) {
    let paths = &summary.edits.paths;
    println!(
        "It is in the layout of Jellyfin's docker image, for a volume mounted at {}: {} rewritten onto it{}.",
        DOCKER.program_data,
        count(paths.rows, "stored path was", "stored paths were"),
        if paths.stored.is_empty() {
            String::new()
        } else {
            format!(
                " from the data directory the database's root and library rows name ({})",
                paths.stored.join(", ")
            )
        }
    );
    let elsewhere = ran_elsewhere(config, summary);
    if !elsewhere.is_empty() {
        println!(
            "The database's root and library rows name {} as the data directory the server ran at, not {}, the one exported: the paths under each were rewritten.",
            elsewhere.join(", "),
            config.data_dir.display()
        );
    }
    for (dir, place) in &summary.moved {
        println!(
            "{} was copied to {}, where Jellyfin reads it.",
            dir.display(),
            target.join(place).display()
        );
    }
}

/// The data directories the database's root and library rows name that are
/// not the one exported (as given or as its real path).
fn ran_elsewhere(config: &Config, summary: &ExportSummary) -> Vec<String> {
    let real = std::fs::canonicalize(&config.data_dir).ok();
    summary
        .edits
        .paths
        .stored
        .iter()
        .filter(|dir| {
            Path::new(dir) != config.data_dir && real.as_deref() != Some(Path::new(dir.as_str()))
        })
        .cloned()
        .collect()
}

/// Prints what the export of `source` into `out` (resolved to `target`) wrote:
/// `summary`'s copy and database changes, and `configuration`'s files.
fn print_summary(
    source: &Path,
    out: &Path,
    target: &Path,
    summary: &ExportSummary,
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
    /// The directories of the data directory left out of the copy, relative
    /// to it, each with why.
    left_out: Vec<(PathBuf, &'static str)>,
    /// Whether the database's root and library rows name another data
    /// directory than the one exported.
    ran_elsewhere: bool,
    /// Whether the metadata directory the server's settings name, kept outside
    /// the data directory, was not found.
    metadata_missing: bool,
    /// The playlists' and box sets' metadata files in Jellyfin's own folders
    /// of the data directory, relative to it, each left out or kept.
    container_files: Vec<(PathBuf, ContainerFile)>,
    /// The playlists and box sets in media libraries with a metadata file
    /// Jellyfin may read, or whose path was not found.
    library_containers: Vec<LibraryContainer>,
}

/// Reads what the loss report needs from the files: the size of
/// the database exported into `partial`, the backups copied there, whether
/// `config`'s directories keep scheduled-task state, and how the export laid
/// out `dirs` (`summary`).
async fn report_inputs(
    config: &Config,
    dirs: &SourceDirs,
    summary: &ExportSummary,
    partial: &Path,
) -> anyhow::Result<ReportInputs> {
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
    let left_out = summary
        .left_out
        .iter()
        .map(|(dir, why)| {
            let why = match why {
                LeftOut::NotUsed => "not the directory the server's settings name for it",
                LeftOut::Moved => "copied where Jellyfin reads it instead",
                LeftOut::NotNeeded => {
                    "the server's cache, log or web client, which Jellyfin does not read"
                }
            };
            (dir.clone(), why)
        })
        .collect();
    Ok(ReportInputs {
        database_bytes,
        backups,
        task_state,
        left_out,
        ran_elsewhere: !ran_elsewhere(config, summary).is_empty(),
        metadata_missing: metadata_missing(config, dirs),
        container_files: summary.container_files.clone(),
        library_containers: summary.library_containers.clone(),
    })
}

/// The loss report: what the export removed from the database (`edits`), what
/// Jellyfin's first start will change, the settings `configuration` does not
/// carry, and, from `inputs`, the room the first start needs and the backups
/// and scheduled-task state Jellyfin cannot use. It names no path of the
/// source or the output, so two exports of one source write the same where
/// the same directories are reachable (the media paths it names are the
/// database's, and whether their files were found depends on what is mounted
/// where the export runs).
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

    report.push_str("\nFiles and stored paths:\n");
    for (dir, why) in &inputs.left_out {
        let _ = writeln!(report, "  {}: left out of the copy; {why}.", dir.display());
    }
    if inputs.metadata_missing {
        report.push_str(
            "  The metadata directory the server's settings name was not found where the export ran, so the output's metadata holds none of it: every image and folder Jellyfin looks for there through %MetadataPath% is missing. Export again where that directory is reachable at the path the settings give.\n",
        );
    }
    if inputs.ran_elsewhere {
        report.push_str(
            "  The database's root and library rows name another data directory than the one exported: the server ran at another path, or the directory moved since. The stored paths under it were rewritten for Jellyfin too; the export printed both directories.\n",
        );
    }
    container_files(&mut report, inputs);
    if inputs.left_out.is_empty()
        && !inputs.metadata_missing
        && !inputs.ran_elsewhere
        && inputs.container_files.is_empty()
        && inputs.library_containers.is_empty()
    {
        report.push_str("  None.\n");
    }

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

/// The loss report's lines on the playlists' and box sets' metadata files
/// (gl-1903): those left out of the copy and why, a `collection.xml` kept, and
/// those in media libraries, which the export never writes.
fn container_files(report: &mut String, inputs: &ReportInputs) {
    use std::fmt::Write as _;

    let left_out = |name: &str| {
        inputs
            .container_files
            .iter()
            .filter(|(path, what)| {
                *what == ContainerFile::LeftOut && path.file_name().is_some_and(|file| file == name)
            })
            .count() as u64
    };
    let files: Vec<String> = [
        (
            left_out("playlist.xml"),
            "playlist.xml of",
            "playlist in data/playlists",
            "playlists in data/playlists",
        ),
        (
            left_out("collection.xml"),
            "collection.xml of",
            "collection in data/collections",
            "collections in data/collections",
        ),
    ]
    .into_iter()
    .filter(|(n, ..)| *n > 0)
    .map(|(n, file, one, many)| format!("{file} {}", count(n, one, many)))
    .collect();
    if !files.is_empty() {
        let _ = writeln!(
            report,
            "  {}: left out of the copy. Each may hold what Jellyfin last wrote rather than the changes made in Ferrofin since (a share or a member removed, a rename), which Jellyfin's metadata refresh would merge back; Jellyfin finds the playlists and collections without them, and writes each file again from the database when it next saves the playlist or collection.",
            files.join(", ")
        );
    }
    for (path, _) in inputs
        .container_files
        .iter()
        .filter(|(_, what)| *what == ContainerFile::Kept)
    {
        let _ = writeln!(
            report,
            "  {}: kept, though it may hold what Jellyfin last wrote rather than the changes made in Ferrofin since: its folder's name lacks [boxset], so the file is what makes the folder a collection for Jellyfin.",
            path.display()
        );
    }
    for container in inputs.library_containers.iter().filter(|c| c.found) {
        let what = if container.kind == "playlist" {
            "the members and shares it lists back into the playlist"
        } else {
            "the members it lists back into the collection"
        };
        let _ = writeln!(
            report,
            "  {}: the {} it belongs to is in a media library, which the export never writes, so the file stays; Jellyfin's metadata refresh may merge {what}.",
            container.file.display(),
            container.kind
        );
    }
    // A library not mounted where the export ran hides every one of them: one line.
    let unchecked: Vec<&LibraryContainer> = inputs
        .library_containers
        .iter()
        .filter(|c| !c.found)
        .collect();
    if let Some(first) = unchecked.first() {
        let _ = writeln!(
            report,
            "  {} in media libraries not found where the export ran (the first: {}): whether each has a metadata file that Jellyfin's metadata refresh may merge back is not known. Export where the media is mounted at the paths Ferrofin used, so each can be checked.",
            count(
                unchecked.len() as u64,
                "playlist or collection",
                "playlists and collections"
            ),
            first.file.display()
        );
    }
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

/// `bytes` in gibibytes from 1 GiB up, else in mebibytes ([`mebibytes`]), to
/// one decimal, rounded up.
fn amount(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    if bytes < GIB {
        return mebibytes(bytes);
    }
    let tenths = bytes.saturating_mul(10).div_ceil(GIB);
    format!("{}.{} GiB", tenths / 10, tenths % 10)
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
    use ferrofin_db::export::{
        ContainerFile, DatabaseEdits, DroppedObjects, DroppedTable, HistoryRewrite,
        LibraryContainer,
    };

    use super::{ReportInputs, amount, loss_report, mebibytes};

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
            ..DatabaseEdits::default()
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
            left_out: vec![
                (
                    "cache".into(),
                    "the server's cache, log or web client, which Jellyfin does not read",
                ),
                (
                    "metadata".into(),
                    "not the directory the server's settings name for it",
                ),
            ],
            ran_elsewhere: true,
            metadata_missing: true,
            container_files: vec![
                (
                    "data/collections/Odd/collection.xml".into(),
                    ContainerFile::Kept,
                ),
                (
                    "data/collections/Saga [boxset]/collection.xml".into(),
                    ContainerFile::LeftOut,
                ),
                (
                    "data/playlists/Mix/playlist.xml".into(),
                    ContainerFile::LeftOut,
                ),
                (
                    "data/playlists/Road/playlist.xml".into(),
                    ContainerFile::LeftOut,
                ),
            ],
            library_containers: vec![
                LibraryContainer {
                    kind: "collection",
                    file: "/media/movies/Heist [boxset]/collection.xml".into(),
                    found: true,
                },
                LibraryContainer {
                    kind: "playlist",
                    file: "/media/music/Gone [playlist]/playlist.xml".into(),
                    found: false,
                },
                LibraryContainer {
                    kind: "playlist",
                    file: "/media/music/Road.xml".into(),
                    found: true,
                },
            ],
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
            ..DatabaseEdits::default()
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
        assert_eq!(amount((1 << 30) - 1), "1024.0 MiB");
        assert_eq!(amount(1 << 30), "1.0 GiB");
        assert_eq!(amount((3 << 30) + 1), "3.1 GiB", "rounded up");
    }
}
