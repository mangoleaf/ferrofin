//! The database half of an export: a consistent copy of the source SQLite
//! database, taken without opening the source.
//!
//! The export promises never to write the source directory, and SQLite does
//! not keep that promise for it: a connection to a database in WAL mode, even
//! a read-only one, may create the `-wal` and `-shm` files beside it. So the
//! database, its write-ahead log and any rollback journal are copied byte for
//! byte next to the output first, and the snapshot is a `VACUUM INTO` from that
//! copy. Opening the copy recovers the log (or rolls back the journal) into it,
//! so commits a stopped or killed server left in the log are in the snapshot.
//! The `-shm` index is not copied: it is only shared memory, and SQLite
//! rebuilds it from the log.
//!
//! A copy of a database another process is still writing could be torn. The
//! export is for a stopped server; the refusal rules make sure of it.

use std::path::{Path, PathBuf};

use sqlx::ConnectOptions as _;
use sqlx::sqlite::SqliteConnectOptions;

use super::{blocking, failed, tree, with_suffix};
use crate::Result;
use crate::database::{
    abandon_on_error, init_sqlite, publish_snapshot, start_snapshot, vacuum_into,
};

/// Writes a consistent snapshot of the SQLite database at `source` (its real
/// path, where SQLite keeps its log), including what its write-ahead log or
/// journal holds, to `dest`, replacing a file already there, with `source`'s
/// permissions. `source` and its directory are only read.
///
/// # Errors
/// [`DbError::Export`](crate::DbError::Export) when the database cannot be
/// staged, the directory for `dest` cannot be made or is a symbolic link, or
/// the staged copy cannot be removed; [`DbError::Sqlx`](crate::DbError::Sqlx)
/// or [`DbError::Backup`](crate::DbError::Backup) when the snapshot cannot be
/// taken or published.
pub(super) async fn snapshot_database(source: &Path, dest: &Path) -> Result<()> {
    let staged = with_suffix(dest, ".export-source");
    let leftovers = staged_files(&staged);
    let (from, to, files) = (source.to_owned(), staged.clone(), leftovers.clone());
    let dir = dest.parent().map(Path::to_owned);
    let staging = blocking(move || {
        if let Some(dir) = &dir {
            output_dir(dir)?;
        }
        remove_all(&files)?;
        for suffix in ["", "-wal", "-journal"] {
            let file = with_suffix(&from, suffix);
            if suffix.is_empty() || file.try_exists().map_err(failed(&file))? {
                let staged = with_suffix(&to, suffix);
                copy(&file, &staged)?;
                // SQLite recovers the log into the copy, so it must be writable.
                let mode = std::fs::metadata(&staged)
                    .map_err(failed(&staged))?
                    .permissions();
                std::fs::set_permissions(&staged, writable(&mode)).map_err(failed(&staged))?;
            }
        }
        std::fs::metadata(&from)
            .map(|metadata| metadata.permissions())
            .map_err(failed(&from))
    })
    .await;
    let written = match staging {
        Ok(permissions) => vacuum(&staged, dest, permissions).await,
        Err(error) => Err(error),
    };
    // The staged copy goes whether the snapshot was written or not.
    let removed = blocking(move || remove_all(&leftovers)).await;
    written.and(removed)
}

/// `VACUUM INTO` from the database at `from` to `dest`, with `permissions`,
/// written under a temporary name and renamed into place once complete.
async fn vacuum(from: &Path, dest: &Path, permissions: std::fs::Permissions) -> Result<()> {
    init_sqlite();
    let partial = start_snapshot(dest)?;
    let written = async {
        // `VACUUM INTO` writes into an empty file that is there, so the
        // snapshot is never readable by more than the source while written;
        // writable by the owner until then, its exact mode once complete.
        tree::create(&partial, &writable(&permissions)).map_err(failed(&partial))?;
        let mut conn = SqliteConnectOptions::new()
            .filename(from)
            .create_if_missing(false)
            .connect()
            .await?;
        sqlx::query(sqlx::AssertSqlSafe(vacuum_into(&partial)))
            .execute(&mut conn)
            .await?;
        sqlx::Connection::close(conn).await?;
        let (partial, dest) = (partial.clone(), dest.to_owned());
        blocking(move || {
            // After the rename: the publish opens the file for writing to flush it.
            publish_snapshot(&partial, &dest)?;
            std::fs::set_permissions(&dest, permissions).map_err(failed(&dest))
        })
        .await
    }
    .await;
    abandon_on_error(&partial, written)
}

/// Makes the directory the snapshot goes into, refusing one that is a symbolic
/// link: the snapshot would be written through it, possibly into the source.
/// Directories it makes are private to the owner.
fn output_dir(dir: &Path) -> Result<()> {
    tree::private_dir(dir).map_err(failed(dir))?;
    if std::fs::symlink_metadata(dir)
        .map_err(failed(dir))?
        .file_type()
        .is_symlink()
    {
        return Err(failed(dir)(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a symbolic link, which the snapshot would be written through",
        )));
    }
    Ok(())
}

/// `permissions` with the owner's read and write added, for a copy SQLite
/// must open for writing whatever the source's mode.
fn writable(permissions: &std::fs::Permissions) -> std::fs::Permissions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::Permissions::from_mode(permissions.mode() | 0o600)
    }
    #[cfg(not(unix))]
    {
        let mut permissions = permissions.clone();
        permissions.set_readonly(false);
        permissions
    }
}

/// The staged database and every file SQLite may leave beside it.
fn staged_files(staged: &Path) -> [PathBuf; 4] {
    ["", "-wal", "-shm", "-journal"].map(|suffix| with_suffix(staged, suffix))
}

/// Copies `from` to `to` byte for byte.
fn copy(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to)
        .map(drop)
        .map_err(|source| crate::DbError::Export {
            path: format!("{} -> {}", from.display(), to.display()),
            source,
        })
}

/// Removes each of `files` that exists.
fn remove_all(files: &[PathBuf]) -> Result<()> {
    for file in files {
        match std::fs::remove_file(file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(failed(file)(error));
            }
            _ => {}
        }
    }
    Ok(())
}
