//! Backup service trait — the DI seam for the `/Backup*` routes.
//!
//! Port of `IBackupService`
//! (`MediaBrowser.Controller/SystemBackupService/IBackupService.cs`). The
//! engine (archive layout, manifest, the restore marker the next boot
//! applies) lives in `ferrofin-core`'s `backup` module; `ferrofin-api`
//! handlers reach it only through this object-safe trait.

use async_trait::async_trait;
use ferrofin_model::backup::{BackupManifest, BackupOptions};

use crate::error::ServiceError;

/// Why the backup service turned a request down.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// Another create holds the only slot: `POST /Backup/Create` answers the
    /// `503` the contract documents rather than queueing.
    #[error("a backup is already in progress")]
    InProgress,
    /// The archive exists but this engine cannot restore it (a foreign or
    /// unreadable archive, or one from a newer Ferrofin): a `400` carrying the
    /// reason.
    #[error("{0}")]
    Unrestorable(String),
    /// Any other failure, a missing archive's [`ServiceError::NotFound`]
    /// included.
    #[error(transparent)]
    Service(#[from] ServiceError),
}

/// Create, list, inspect and schedule the restore of server backups.
///
/// An archive is named as the client gave it: the service reduces the name
/// to a file in its backups directory (`BackupController.SanitizePath`), and
/// a name that is not a file there is [`ServiceError::NotFound`] with the
/// message `backup {name}`.
#[async_trait]
pub trait BackupService: Send + Sync {
    /// The manifest of every archive in the backups directory, in directory
    /// order. Port of `BackupService.EnumerateBackups`.
    async fn list_backups(&self) -> Result<Vec<BackupManifest>, BackupError>;

    /// The manifest of the archive `path` names, or `None` when that file
    /// carries no readable manifest. Port of `BackupController.GetBackup`.
    async fn get_backup_manifest(&self, path: &str) -> Result<Option<BackupManifest>, BackupError>;

    /// Writes a new archive with `options` and returns its manifest. Port of
    /// `BackupService.CreateBackupAsync`.
    async fn create_backup(&self, options: BackupOptions) -> Result<BackupManifest, BackupError>;

    /// Schedules the archive `archive_file_name` names for restore and asks
    /// for a restart, the next boot applying it before the database opens.
    /// Port of `BackupService.ScheduleRestoreAndRestartServer`.
    async fn schedule_restore(&self, archive_file_name: &str) -> Result<(), BackupError>;
}

fn _assert_object_safe_backup_service(_: &dyn BackupService) {}
