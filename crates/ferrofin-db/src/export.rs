//! Export of a stopped Ferrofin data directory, the way back to Jellyfin
//! (`ferrofin-server export-jellyfin`).
//!
//! The export copies; it never converts in place. It reads the source data
//! directory and writes only under the output directory, and it digests the
//! source before and after to show that it left the source as it found it.
//! It writes a copy of the directory with a consistent snapshot of the
//! database, whose migration history it rewrites for the Jellyfin release
//! `UPSTREAM_TAG` names and from which it removes Ferrofin's own objects, so
//! the file is a Jellyfin database and nothing else.
//!
//! Each concern has its own file, so the phases that build the export each
//! touch their own:
//! - `layout`: the source's directories, and the layout the output is in;
//! - `snapshot`: the consistent copy of the database;
//! - `tree`: the copy of every other file, and the before/after digest;
//! - `history`: the `__EFMigrationsHistory` the target release must find;
//! - `classify`: which migrations are schema and which are code migrations;
//! - `proof`: what proves Ferrofin did the work of a code migration the history
//!   records as applied;
//! - `drop_objects`: removing Ferrofin's own tables and indexes from the copy;
//! - `paths`: the stored paths, rewritten onto the output's layout;
//! - `carried`: what Ferrofin's own tables hold that Jellyfin keeps in files
//!   (Live TV tuners, guides and timers, LDAP-Auth's uid map), read before
//!   they are removed;
//! - `containers`: the playlists' and box sets' metadata files, left out of
//!   the copy where they may be stale, and those in media libraries;
//! - `report`: what the removed tables held and what Jellyfin's first start
//!   changes, for the operator's loss report;
//! - `counts`: the rows Jellyfin's code migrations change on its first start,
//!   for the same report;
//! - `refusal`: the conditions under which an export must not run.
//!
//! The database steps are SQLite-only by design: the export takes a SQLite
//! file, never a backend-neutral handle.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use sqlx::SqliteConnection;

use crate::{DbError, Result};

pub use carried::{
    CarriedRows, IdentityLink, MirroredLiveTv, RecordingRow, SeriesTimerRow, TimerRow,
};
pub use classify::{
    CODE_PRODUCT_VERSION, Disposition, JELLYFIN_MIGRATIONS, SCHEMA_PRODUCT_VERSION,
};
pub use containers::{ContainerFile, LibraryContainer};
pub use counts::FirstStartCounts;
pub use drop_objects::{DroppedObjects, DroppedTable};
pub use history::HistoryRewrite;
pub use layout::{DOCKER, Layout, LeftOut, SourceDirs};
pub use paths::PathRewrite;
pub use refusal::Preflight;
pub use report::{first_start_effects, recorded_as_applied, table_note};

mod carried;
mod classify;
mod containers;
mod counts;
mod drop_objects;
mod history;
mod layout;
mod paths;
mod proof;
mod refusal;
mod report;
mod snapshot;
mod tree;

/// Where an export writes the database, relative to its output directory:
/// Jellyfin's `{DataPath}/jellyfin.db`.
pub const EXPORTED_DATABASE: &str = "data/jellyfin.db";

/// Where `ferrofin-server export-jellyfin` writes its loss report, relative to
/// its output directory: what the export left behind, which it also prints.
pub const LOSS_REPORT: &str = "ferrofin-export-report.txt";

/// Jellyfin's list of the migrations it applied before 10.11, under the
/// configuration directory of the docker layout. Jellyfin 10.11 moved its
/// entries into the history table and renamed the file. One still there that
/// lists any makes Jellyfin 12.4 seed the history from it, and stop when the
/// history already holds every entry, as an exported one does
/// (`JellyfinMigrationService.cs`).
const LEGACY_MIGRATIONS_FILE: &str = "config/migrations.xml";

/// Its file name, in a configuration directory kept outside the data
/// directory, which the export copies to the layout's.
const LEGACY_MIGRATIONS_NAME: &str = "migrations.xml";

/// The first bytes of every SQLite database file.
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// The directory an export into `target` (the output as
/// [`refuse_unsafe`] resolved it) writes before renaming it to `target` whole:
/// `target` with `.partial` appended, beside it. One left behind is an export
/// that was interrupted, which [`refuse_unsafe`] reports rather than reuses.
#[must_use]
pub fn partial_output(target: &Path) -> PathBuf {
    with_suffix(target, ".partial")
}

/// Refuses, before anything is written, an export from the directories `dirs`
/// (with the database `database`) into `out` that would be unsafe or
/// meaningless, and returns the preflight: the output resolved (links
/// followed as far as it exists, `.` and `..` applied, no trailing `/`), the
/// path to rename the [`partial_output`] to, with the size of each class of
/// file the export copies and the free space. It refuses:
/// - an output that is not a new or empty directory, is the source, sits
///   inside it or holds it (or a directory copied besides it), or is a mount
///   point, and one an interrupted export left a [`partial_output`] beside;
/// - a database another process holds open (a running server);
/// - an output inside a directory the source reaches through a link;
/// - less free space beside the output than the export needs.
///
/// The database itself (its log, its integrity, its foreign keys, whether it
/// is Ferrofin's and from which Jellyfin release) is checked on the copy the
/// snapshot stages, by [`export_data_dir`], so the source is never opened.
/// Call this from a process that does not have the database open: it can only
/// see another process's hold on it.
///
/// # Errors
/// [`DbError::ExportRefused`] naming the cause, or [`DbError::Export`] when a
/// path cannot be inspected (on a platform without the POSIX interfaces the
/// checks use, always).
pub async fn refuse_unsafe(
    dirs: &SourceDirs,
    layout: &Layout,
    database: &Path,
    out: &Path,
) -> Result<Preflight> {
    let (dirs, layout, database, out) =
        (dirs.clone(), *layout, database.to_owned(), out.to_owned());
    blocking(move || {
        let target = refusal::paths(&dirs.program_data, &out)?;
        let trees = dirs.trees(&layout)?;
        for (tree, _) in trees.copied.iter().skip(1) {
            refusal::apart(tree, &target)?;
        }
        let real = std::fs::canonicalize(&database).map_err(failed(&database))?;
        refusal::not_in_use(&real)?;
        refusal::reach(&trees, (&database, &real), target)
    })
    .await
}

/// What [`export_data_dir`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSummary {
    /// The files copied besides the database.
    pub files: u64,
    /// Their size in bytes.
    pub bytes: u64,
    /// Where the database snapshot was written.
    pub database: PathBuf,
    /// What the export changed in the database it copied.
    pub edits: DatabaseEdits,
    /// The directories inside the data directory it left out, relative to
    /// it, with why: the server's cache, log and web client directories, and
    /// its own `config` or `metadata` when the server used one kept elsewhere.
    pub left_out: Vec<(PathBuf, LeftOut)>,
    /// The directories it copied besides the data directory, each with its
    /// place in the output: a configuration or metadata directory kept
    /// elsewhere.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// The playlists' and box sets' metadata files in Jellyfin's own folders
    /// of the data directory, relative to it, sorted, each left out or kept
    /// ([`ContainerFile`]).
    pub container_files: Vec<(PathBuf, ContainerFile)>,
    /// The playlists and box sets the database stores in media libraries
    /// whose metadata file was there where the export ran, or whose path was
    /// not ([`LibraryContainer`]).
    pub library_containers: Vec<LibraryContainer>,
}

/// What the export changed in its copy of the database, step by step
/// (`edit_staged`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseEdits {
    /// The rewrite of `__EFMigrationsHistory`.
    pub history: HistoryRewrite,
    /// Ferrofin's own tables and indexes, removed.
    pub dropped: DroppedObjects,
    /// The stored paths, rewritten onto the output's layout.
    pub paths: PathRewrite,
    /// What Ferrofin's own tables held that Jellyfin keeps in files, read
    /// before they were removed.
    pub carried: CarriedRows,
    /// The rows Jellyfin's code migrations change on its first start, for the
    /// report of an adopted source (on a native one Jellyfin runs none).
    pub counts: FirstStartCounts,
}

/// The export's changes to its staged copy of the database, in order, on the
/// one connection the snapshot holds to it: after the checks, which read the
/// source's Jellyfin `history` ([`refusal::database`]), and before `VACUUM
/// INTO` writes the snapshot, so the snapshot holds no free pages from them
/// and two exports of one source stay equal. Each step is a file of its own; a
/// step added later goes here, with its field in [`DatabaseEdits`]. The paths
/// are rewritten onto `layout` from `dirs` and the directories the rows name.
async fn edit_staged(
    conn: &mut SqliteConnection,
    history: Option<&history::SourceHistory>,
    (dirs, layout): (&SourceDirs, &Layout),
) -> Result<DatabaseEdits> {
    let mut tx = sqlx::Connection::begin(conn).await?;
    let history = history::rewrite(&mut tx, history).await?;
    // Before the drop, which removes the tables it reads.
    let carried = carried::read(&mut tx).await?;
    let edits = DatabaseEdits {
        history,
        dropped: drop_objects::remove(&mut tx).await?,
        paths: paths::rewrite(&mut tx, dirs, layout).await?,
        carried,
        counts: counts::read(&mut tx).await?,
    };
    tx.commit().await?;
    Ok(edits)
}

/// Copies the stopped server's directories `dirs` into `out`, in `layout`
/// (the export writes [`DOCKER`]), with a consistent snapshot of its
/// database `database` (a path under the data directory) at
/// [`EXPORTED_DATABASE`] under `out`.
///
/// Run [`refuse_unsafe`] first. `database` is checked before anything is
/// written: it must exist and be a SQLite file. Then every file and directory
/// under the data directory is copied to the same place under `out`, with its
/// permissions and modification time, and a configuration or metadata
/// directory kept elsewhere to `config` or `metadata` ([`SourceDirs`]); the
/// data directory's own one, and the directories of `dirs.not_needed` inside
/// it, are left out ([`ExportSummary::left_out`]).
/// Symbolic links are followed, so the output holds no link back into the
/// source. Left out are the database files: `database`, the file it links to,
/// the source's own [`EXPORTED_DATABASE`] (a native database's leftover would
/// otherwise sit under the snapshot), and the `-wal`, `-shm` and `-journal`
/// files of each. The snapshot holds what they do. Also left out is a
/// `migrations.xml` in the configuration directory (the data directory's own,
/// or the one copied to the layout's), which Jellyfin would read again: from
/// one listing migrations it writes history rows, or stops its start. So are
/// the `playlist.xml` of each playlist in the data directory's
/// `data/playlists` and the `collection.xml` of each `[boxset]` folder in its
/// `data/collections`, which may hold what Jellyfin last wrote rather than
/// the database's changes since, and which Jellyfin's metadata refresh would
/// merge back ([`ExportSummary::container_files`]). So is each library's
/// `options.json` in the data directory's `root/default`, Ferrofin's own file:
/// the export writes the `options.xml` Jellyfin reads beside it, and a later
/// adoption by Ferrofin would take the stale copy over that.
/// The snapshot's `__EFMigrationsHistory` is rewritten ([`HistoryRewrite`]),
/// what Ferrofin's own tables hold that Jellyfin keeps in files is read
/// ([`CarriedRows`]) before those tables and Ferrofin's indexes are removed
/// from it ([`DroppedObjects`]), and its stored paths are rewritten onto the
/// layout ([`PathRewrite`]).
///
/// What is copied is digested while it is copied and again once the database
/// snapshot is written, and so are the files SQLite keeps beside the
/// database's real file when a link puts it outside the source. Any file or
/// directory added, removed or changed in between fails the export: the export
/// then cannot vouch for having left the source untouched, nor for its output
/// being a copy of one state of it. A failed export may leave a partial copy
/// in `out`; its directories stay private to the owner unless it failed only on
/// that comparison.
///
/// # Errors
/// [`DbError::Export`] when `database` is missing or not SQLite, when a source
/// file cannot be read or its copy written, or when the source holds something
/// that is neither a file nor a directory, a broken link, or a directory
/// reached twice; [`DbError::Backup`] or [`DbError::Sqlx`] when the database
/// snapshot fails; [`DbError::ExportRefused`] when the copy of the database the
/// snapshot stages fails its checks (a log it cannot fold in whole, a damaged
/// file, `integrity_check`, `foreign_key_check`, not Ferrofin's, a Jellyfin
/// history from no release Ferrofin adopts, no proof of the work of a code
/// migration the history records as applied); and
/// [`DbError::ExportSourceChanged`] when the source changed during the export.
pub async fn export_data_dir(
    dirs: &SourceDirs,
    layout: &Layout,
    database: &Path,
    out: &Path,
) -> Result<ExportSummary> {
    let source = &dirs.program_data;
    let (db, from, source_dirs, into) = (
        database.to_owned(),
        source.to_owned(),
        dirs.clone(),
        *layout,
    );
    let (resolved, real_source, mut trees) = blocking(move || {
        let real_source = std::fs::canonicalize(&from).map_err(failed(&from))?;
        Ok((
            resolve_database(&db)?,
            real_source,
            source_dirs.trees(&into)?,
        ))
    })
    .await?;
    // SQLite keeps a database's log beside its real file. Inside the source the
    // tree digest covers it; a link may put it outside.
    let companions: Vec<PathBuf> = database_files(&resolved)
        .split_off(1)
        .into_iter()
        .filter(|file| !file.starts_with(&real_source))
        .collect();
    let mut skipped = [database, &source.join(EXPORTED_DATABASE), &resolved]
        .into_iter()
        .flat_map(database_files)
        .collect::<Vec<_>>();
    skipped.push(source.join(LEGACY_MIGRATIONS_FILE));
    skipped.push(dirs.config.join(LEGACY_MIGRATIONS_NAME));
    let (to, files, copied_trees, left_out) = (
        out.to_owned(),
        companions.clone(),
        trees.copied.clone(),
        trees.left_out_dirs(),
    );
    let (companions_before, copied, skip) = blocking(move || {
        // The data directory's walk leaves directories out; the others' do not.
        let skip = (
            tree::Skip::new(skipped.clone())?
                .leaving_out(left_out)
                .with_containers(),
            tree::Skip::new(skipped)?,
        );
        let companions = tree::digest_files(&files)?;
        let copied = tree::walk_all(&copied_trees, Some(&to), &skip.0, &skip.1)?;
        Ok((companions, copied, skip))
    })
    .await?;
    let exported = out.join(EXPORTED_DATABASE);
    let (edits, library_containers) =
        snapshot::snapshot_database(&resolved, &exported, (dirs, layout)).await?;
    let mut dirs_copied = copied.dirs;
    // A source with no `data/` (a native one) has none to take the mode from:
    // the snapshot's directory takes the source's own.
    let data = exported.parent().map(Path::to_owned);
    if let Some(data) = data.filter(|data| !dirs_copied.iter().any(|(to, _)| to == data)) {
        dirs_copied.push((data, source.to_owned()));
    }
    let copied_trees = trees.copied.clone();
    let (companions_after, after) = blocking(move || {
        // Last: the snapshot wrote into one of these directories.
        tree::finish(&dirs_copied)?;
        Ok((
            tree::digest_files(&companions)?,
            tree::walk_all(&copied_trees, None, &skip.0, &skip.1)?.digest,
        ))
    })
    .await?;
    let mut changed = copied.digest.changes(&after);
    changed.extend(companions_before.changes(&companions_after));
    if !changed.is_empty() {
        return Err(DbError::ExportSourceChanged { changed });
    }
    Ok(ExportSummary {
        files: copied.files,
        bytes: copied.bytes,
        database: exported,
        edits,
        left_out: trees.left_out,
        moved: trees.copied.split_off(1),
        container_files: copied.containers,
        library_containers,
    })
}

/// The real path of `database`, links resolved, once it is known to exist and
/// to be a SQLite file.
fn resolve_database(database: &Path) -> Result<PathBuf> {
    let resolved = std::fs::canonicalize(database).map_err(failed(database))?;
    let mut header = Vec::with_capacity(SQLITE_HEADER.len());
    std::fs::File::open(&resolved)
        .and_then(|file| file.take(16).read_to_end(&mut header))
        .map_err(failed(&resolved))?;
    if header != SQLITE_HEADER {
        return Err(failed(&resolved)(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a SQLite database",
        )));
    }
    Ok(resolved)
}

/// A SQLite database file and the files SQLite keeps beside it.
fn database_files(database: &Path) -> Vec<PathBuf> {
    ["", "-wal", "-shm", "-journal"]
        .map(|suffix| with_suffix(database, suffix))
        .into()
}

/// `path` with `suffix` appended to its file name (`jellyfin.db` → `jellyfin.db-wal`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Maps an I/O failure at `path` into [`DbError::Export`]; the path is only
/// formatted when there is a failure.
fn failed(path: &Path) -> impl FnOnce(std::io::Error) -> DbError + '_ {
    move |source| DbError::Export {
        path: path.display().to_string(),
        source,
    }
}

/// Runs the filesystem work `f` off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| DbError::Export {
            path: "(the export's file work)".to_owned(),
            source: std::io::Error::other(error),
        })?
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{database_files, resolve_database, with_suffix};

    #[test]
    fn database_files_are_the_file_and_its_sqlite_companions() {
        assert_eq!(
            database_files(Path::new("/d/data/jellyfin.db")),
            ["", "-wal", "-shm", "-journal"]
                .map(|suffix| PathBuf::from(format!("/d/data/jellyfin.db{suffix}")))
        );
        assert_eq!(
            with_suffix(Path::new("a.db"), ".x"),
            PathBuf::from("a.db.x")
        );
    }

    #[test]
    fn only_an_existing_sqlite_file_is_a_database_to_export() {
        let dir = tempfile::tempdir().unwrap();
        let (sqlite, text, empty) = (
            dir.path().join("a.db"),
            dir.path().join("b.db"),
            dir.path().join("c.db"),
        );
        std::fs::write(&sqlite, b"SQLite format 3\0and the rest").unwrap();
        std::fs::write(&text, b"just some text, longer than a header").unwrap();
        std::fs::write(&empty, b"").unwrap();

        assert_eq!(
            resolve_database(&sqlite).unwrap(),
            std::fs::canonicalize(&sqlite).unwrap()
        );
        for refused in [&text, &empty] {
            let error = resolve_database(refused).unwrap_err().to_string();
            assert!(error.contains("not a SQLite database"), "{error}");
        }
        let missing = resolve_database(&dir.path().join("none.db"))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("none.db"), "{missing}");
    }
}
