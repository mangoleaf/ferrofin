//! `BackupController` — server backup archives.
//!
//! `GET /Backup` lists the backup archives available to restore. Hermit has no
//! backup-creation subsystem yet, so — like a fresh Jellyfin install that has
//! never run a backup — the list is empty. This unblocks the dashboard's Backup
//! page (which `501`d before).
//!
//! ponytail: read-only empty list for now. `GET /Backup/Manifest` +
//! `POST /Backup/Create|Restore` (the archive itself — zip the DB + config) are
//! the follow-up; they need a real backup subsystem, not just a handler.

use axum::routing::get;
use axum::{Json, Router};
use serde_json::Value;

use crate::auth::RequireAuth;
use crate::state::AppState;

/// `GET /Backup` — the available backup archives.
///
/// Port of `BackupController.ListBackups`. No backup subsystem → empty list
/// (`BackupManifestDto[]`).
#[utoipa::path(
    get,
    path = "/Backup",
    responses((status = 200, description = "Backups available (BackupManifestDto[])")),
    tag = "hermit"
)]
async fn list_backups(RequireAuth(_auth): RequireAuth) -> Json<Vec<Value>> {
    Json(Vec::new())
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/Backup", get(list_backups))
}
