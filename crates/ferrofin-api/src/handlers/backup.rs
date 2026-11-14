//! `BackupController` — create, list, inspect, and restore server backups.
//!
//! Ports Jellyfin's `BackupController`: the administrator gate, the request
//! bodies and query, and the answers. The engine (archive layout, manifest,
//! the restore marker the next boot applies) is `ferrofin-core`'s `backup`
//! module, reached through [`BackupService`]:
//!
//! - `GET  /Backup` — list the available backups' manifests.
//! - `GET  /Backup/Manifest?path=` — read one backup's manifest.
//! - `POST /Backup/Create` — write a new backup and return its manifest.
//! - `POST /Backup/Restore` (`BackupRestoreRequestDto` body) — schedule the
//!   archive for restore and restart; the next boot extracts it over the live
//!   tree before the database is opened, exactly as Jellyfin's
//!   `ScheduleRestoreAndRestartServer` does. Restoring in-process would
//!   overwrite an open SQLite file.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::backup::{BackupManifest, BackupOptions};
use ferrofin_traits::backup::{BackupError, BackupService};
use ferrofin_traits::error::ServiceError;
use serde::Deserialize;

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::extract::{JsonBody, Query};
use crate::state::AppState;

/// The `POST /Backup/Restore` body. Port of `BackupRestoreRequestDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BackupRestoreRequest {
    /// The archive file name (must be present in the backups directory).
    #[serde(default)]
    archive_file_name: Option<String>,
}

/// The backup service, or a `500` on a state the host wired none into.
fn backups(state: &AppState) -> Result<&dyn BackupService, ApiError> {
    state
        .backup
        .as_deref()
        .ok_or_else(|| ServiceError::backend("no backup service is wired").into())
}

/// Answers a [`BackupError`]: a create already running is the contract's
/// `503`, an archive the engine cannot restore a `400` with the reason.
fn api_error(error: BackupError) -> ApiError {
    match error {
        BackupError::InProgress => ApiError::ServiceUnavailable(error.to_string()),
        BackupError::Unrestorable(reason) => ApiError::BadRequest(reason),
        BackupError::Service(error) => error.into(),
    }
}

/// `GET /Backup` — the available backups.
///
/// Port of `BackupController.ListBackups`: every `.zip` under the backups
/// directory with its manifest. A missing directory is an empty list.
#[utoipa::path(
    get,
    path = "/Backup",
    responses((status = 200, description = "Backups available (BackupManifestDto[])")),
    tag = "ferrofin"
)]
async fn list_backups(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
) -> Result<Json<Vec<BackupManifest>>, ApiError> {
    backups(&state)?
        .list_backups()
        .await
        .map(Json)
        .map_err(api_error)
}

/// Query for `GET /Backup/Manifest`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestQuery {
    /// The archive file name (or path) to inspect.
    #[serde(default)]
    path: Option<String>,
}

/// `GET /Backup/Manifest?path=` — one backup's manifest.
///
/// Port of `BackupController.GetBackup`: `path` is reduced to a file name in
/// the backups directory (`SanitizePath`), `404` when that is not a file and
/// `204` when the file carries no readable manifest.
#[utoipa::path(
    get,
    path = "/Backup/Manifest",
    params(("path" = String, Query, description = "The archive file name")),
    responses(
        (status = 200, description = "Backup manifest returned"),
        (status = 204, description = "Not a valid backup archive"),
        (status = 404, description = "Backup not found")
    ),
    tag = "ferrofin"
)]
async fn get_backup_manifest(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(query): Query<ManifestQuery>,
) -> Result<Response, ApiError> {
    let path = query
        .path
        .ok_or_else(|| ApiError::BadRequest("missing 'path'".to_owned()))?;
    let manifest = backups(&state)?
        .get_backup_manifest(&path)
        .await
        .map_err(api_error)?;
    Ok(match manifest {
        None => StatusCode::NO_CONTENT.into_response(),
        Some(manifest) => Json(manifest).into_response(),
    })
}

/// `POST /Backup/Create` — writes a backup archive and returns its manifest.
///
/// Port of `BackupController.CreateBackup`: the body is required
/// (`[FromBody] BackupOptionsDto`): none is a `415` or `400`, and `{}` takes
/// the defaults. A create already running is a `503`; refused while a library
/// scan runs, and when the backups directory has too little free space.
#[utoipa::path(
    post,
    path = "/Backup/Create",
    responses((status = 200, description = "Backup created (BackupManifestDto)")),
    tag = "ferrofin"
)]
async fn create_backup(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    JsonBody(options): JsonBody<BackupOptions>,
) -> Result<Json<BackupManifest>, ApiError> {
    backups(&state)?
        .create_backup(options)
        .await
        .map(Json)
        .map_err(api_error)
}

/// `POST /Backup/Restore` — schedules a backup restore and restarts.
///
/// Port of `BackupController.StartRestoreBackup`: the body is required, its
/// `ArchiveFileName` is reduced to a file name inside the backups directory
/// (`SanitizePath`; `404` when that is not a file), recorded in the
/// restore-pending marker, and the server restarts; the next boot applies it
/// before the database is opened.
#[utoipa::path(
    post,
    path = "/Backup/Restore",
    responses(
        (status = 204, description = "Restore scheduled; the server restarts to apply it"),
        (status = 404, description = "Backup not found")
    ),
    tag = "ferrofin"
)]
async fn restore_backup(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    JsonBody(body): JsonBody<BackupRestoreRequest>,
) -> Result<StatusCode, ApiError> {
    // An empty name is upstream's `GetFullPath("")` `ArgumentException`, a 400.
    let requested = body
        .archive_file_name
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::BadRequest("missing 'ArchiveFileName'".to_owned()))?;
    backups(&state)?
        .schedule_restore(&requested)
        .await
        .map_err(api_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Backup", get(list_backups))
        .route("/Backup/Manifest", get(get_backup_manifest))
        .route("/Backup/Create", post(create_backup))
        .route("/Backup/Restore", post(restore_backup))
}

crate::query::query_parameters! {
    ManifestQuery {} => [("get", "/Backup/Manifest")];
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrofin_model::backup::{BackupManifest, BackupOptions};
    use ferrofin_traits::backup::{BackupError, BackupService};
    use ferrofin_traits::error::ServiceError;

    use crate::state::AppState;
    use crate::test_support::{authed_fake_state, elevated_fake_state};

    /// One request to the backup routes on `state`: its status and body. A
    /// `body` is sent as JSON; `None` sends no content type and no body.
    async fn send(state: AppState, method: &str, uri: &str, body: Option<&str>) -> (u16, String) {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt as _;

        let mut request = Request::builder().method(method).uri(uri);
        if body.is_some() {
            request = request.header("Content-Type", "application/json");
        }
        let response = crate::create_router(state)
            .oneshot(
                request
                    .body(Body::from(body.unwrap_or_default().to_owned()))
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Every `/Backup*` route is `RequiresElevation` in the contract (each
    /// documents a `403`), and restore in particular replaces the live
    /// database — an ordinary account must never reach it.
    #[tokio::test]
    async fn backup_routes_reject_a_non_elevated_caller() {
        // `authed_fake_state` authenticates a plain user — not an API key and
        // not an administrator.
        for (method, uri) in [
            ("GET", "/Backup"),
            ("GET", "/Backup/Manifest?path=x.zip"),
            ("POST", "/Backup/Create"),
            ("POST", "/Backup/Restore"),
        ] {
            let (status, body) = send(authed_fake_state(), method, uri, Some("{}")).await;
            assert_eq!(status, 403, "{method} {uri} must require elevation: {body}");
        }
    }

    /// `CreateBackup([FromBody] BackupOptionsDto)` and
    /// `StartRestoreBackup([FromBody, BindRequired] BackupRestoreRequestDto)`
    /// take a required body, and `GetBackup([BindRequired] string path)` a
    /// path. Measured on Jellyfin 12.2: no content type is a 415, an empty or
    /// `null` body, a body without a name and a blank path a 400. None reaches
    /// the service: the state has none, which would answer 500.
    #[tokio::test]
    async fn requests_without_their_body_or_path_are_refused() {
        for (method, uri, body, status) in [
            ("POST", "/Backup/Create", None, 415),
            ("POST", "/Backup/Create", Some(""), 400),
            ("POST", "/Backup/Create", Some("null"), 400),
            ("POST", "/Backup/Restore", None, 415),
            ("POST", "/Backup/Restore", Some("{}"), 400),
            (
                "POST",
                "/Backup/Restore",
                Some(r#"{"ArchiveFileName":""}"#),
                400,
            ),
            ("GET", "/Backup/Manifest?path=%20%20", None, 400),
        ] {
            let (got, text) = send(elevated_fake_state(), method, uri, body).await;
            assert_eq!(got, status, "{method} {uri} {body:?}: {text}");
        }
    }

    /// A manifest the fake service hands out.
    fn manifest(options: BackupOptions) -> BackupManifest {
        BackupManifest {
            server_version: "9.9.9".to_owned(),
            backup_engine_version: "1.0.0".to_owned(),
            date_created: "2024-05-06T07:08:09Z".parse().expect("date"),
            path: "/data/backups/some.zip".to_owned(),
            options,
        }
    }

    /// A [`BackupService`] whose answer each archive name picks: `some.zip`
    /// has a manifest, `empty.zip` none, `foreign.zip` cannot be restored and
    /// any other name is not found. A create asking for the metadata finds
    /// another create running.
    struct FakeBackups;

    /// The not-found answer for `name`, as the service gives it.
    fn missing<T>(name: &str) -> Result<T, BackupError> {
        Err(ServiceError::not_found(format!("backup {name}")).into())
    }

    #[async_trait::async_trait]
    impl BackupService for FakeBackups {
        async fn list_backups(&self) -> Result<Vec<BackupManifest>, BackupError> {
            Ok(vec![manifest(BackupOptions::default())])
        }
        async fn get_backup_manifest(
            &self,
            path: &str,
        ) -> Result<Option<BackupManifest>, BackupError> {
            match path {
                "some.zip" => Ok(Some(manifest(BackupOptions::default()))),
                "empty.zip" => Ok(None),
                _ => missing(path),
            }
        }
        async fn create_backup(
            &self,
            options: BackupOptions,
        ) -> Result<BackupManifest, BackupError> {
            if options.metadata {
                return Err(BackupError::InProgress);
            }
            Ok(manifest(options))
        }
        async fn schedule_restore(&self, archive_file_name: &str) -> Result<(), BackupError> {
            match archive_file_name {
                "some.zip" => Ok(()),
                "foreign.zip" => Err(BackupError::Unrestorable(
                    "backup foreign.zip is not a Ferrofin 1.0.0 archive".to_owned(),
                )),
                _ => missing(archive_file_name),
            }
        }
    }

    /// Each answer of the service comes out as `BackupController` answers it:
    /// the manifest as `BackupManifestDto`, a missing manifest as `204`, a
    /// missing archive as `404`, a create already running as `503` and an
    /// archive the engine cannot restore as `400` with the reason. The name
    /// reaches the service as the client sent it.
    #[tokio::test]
    async fn service_answers_map_onto_the_controllers() {
        let manifest_json = r#"{"ServerVersion":"9.9.9","BackupEngineVersion":"1.0.0","DateCreated":"2024-05-06T07:08:09+00:00","Path":"/data/backups/some.zip","Options":{"Metadata":false,"Trickplay":false,"Subtitles":false,"Database":true}}"#;
        for (method, uri, body, status, answer) in [
            ("GET", "/Backup", None, 200, format!("[{manifest_json}]")),
            (
                "GET",
                "/Backup/Manifest?path=some.zip",
                None,
                200,
                manifest_json.to_owned(),
            ),
            (
                "GET",
                "/Backup/Manifest?path=empty.zip",
                None,
                204,
                String::new(),
            ),
            (
                "GET",
                "/Backup/Manifest?path=a/../nope.zip",
                None,
                404,
                r#"{"error":"not found: backup a/../nope.zip"}"#.to_owned(),
            ),
            (
                "POST",
                "/Backup/Create",
                Some("{}"),
                200,
                manifest_json.to_owned(),
            ),
            (
                "POST",
                "/Backup/Create",
                Some(r#"{"Metadata":true}"#),
                503,
                r#"{"error":"internal server error"}"#.to_owned(),
            ),
            (
                "POST",
                "/Backup/Restore",
                Some(r#"{"ArchiveFileName":"some.zip"}"#),
                204,
                String::new(),
            ),
            (
                "POST",
                "/Backup/Restore",
                Some(r#"{"ArchiveFileName":"foreign.zip"}"#),
                400,
                r#"{"error":"bad request: backup foreign.zip is not a Ferrofin 1.0.0 archive"}"#
                    .to_owned(),
            ),
            (
                "POST",
                "/Backup/Restore",
                Some(r#"{"ArchiveFileName":"nope.zip"}"#),
                404,
                r#"{"error":"not found: backup nope.zip"}"#.to_owned(),
            ),
        ] {
            let state = elevated_fake_state().with_backup(Arc::new(FakeBackups));
            let (got, text) = send(state, method, uri, body).await;
            assert_eq!((got, text), (status, answer), "{method} {uri} {body:?}");
        }
    }
}
