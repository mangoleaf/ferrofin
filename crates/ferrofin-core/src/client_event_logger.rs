//! [`FerrofinClientEventLogger`] — persists client-uploaded diagnostic documents.
//!
//! Port of `MediaBrowser.Controller.ClientEvent.ClientEventLogger`. The C# class
//! builds a safe file name from the client name/version, a UTC timestamp, and a
//! fresh `Guid`, then copies the uploaded stream into a **new** file under the
//! server's log directory — refusing (via `PathHelper.IsContainedIn`) any name
//! that would escape it.
//!
//! Port rules applied:
//! - The `IServerApplicationPaths` dependency is taken as
//!   `Arc<dyn `[`ServerApplicationPaths`]`>` (dependency injection) so the log
//!   directory comes from the composition root.
//! - `WriteDocumentAsync(Stream)` becomes
//!   [`write_document`](ferrofin_traits::events::ClientEventLogger::write_document)
//!   taking owned bytes (`&[u8]`) — the trait already folded the `Stream` to
//!   `Send`-safe bytes.
//! - `PathHelper.GetSafeLeafFileName` and `IsContainedIn` are the shared
//!   [`ferrofin_util::path_helper`] ports. Both keep an attacker-supplied
//!   `clientName`/`clientVersion` from escaping the log directory.
//! - File creation uses `create_new` (fail if it already exists), matching the
//!   C# `FileMode.CreateNew`; the `Guid` in the name makes a collision
//!   effectively impossible.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::events::ClientEventLogger;
use ferrofin_traits::system::ServerApplicationPaths;
use ferrofin_util::path_helper::{get_safe_leaf_file_name, is_contained_in};

/// The UTC timestamp format used in the generated log file name (C#
/// `yyyyMMddHHmmss`).
const TIMESTAMP_FORMAT: &str = "%Y%m%d%H%M%S";

/// The concrete client-event logger.
///
/// Writes each uploaded document into the injected paths' log directory.
#[derive(Clone)]
pub struct FerrofinClientEventLogger {
    paths: Arc<dyn ServerApplicationPaths>,
}

impl std::fmt::Debug for FerrofinClientEventLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinClientEventLogger")
            .finish_non_exhaustive()
    }
}

impl FerrofinClientEventLogger {
    /// Creates a client-event logger writing under the injected paths' log
    /// directory.
    #[must_use]
    pub fn new(paths: Arc<dyn ServerApplicationPaths>) -> Self {
        Self { paths }
    }
}

#[async_trait]
impl ClientEventLogger for FerrofinClientEventLogger {
    async fn write_document(
        &self,
        client_name: &str,
        client_version: &str,
        contents: &[u8],
    ) -> Result<String, ServiceError> {
        let safe_client_name = get_safe_leaf_file_name(Some(client_name))
            .unwrap_or_else(|| "unknown-client".to_owned());
        let safe_client_version = get_safe_leaf_file_name(Some(client_version))
            .unwrap_or_else(|| "unknown-version".to_owned());
        let timestamp = Utc::now().format(TIMESTAMP_FORMAT);
        let unique = Uuid::new_v4().simple();
        let file_name =
            format!("upload_{safe_client_name}_{safe_client_version}_{timestamp}_{unique}.log");

        let log_dir = PathBuf::from(self.paths.log_directory_path());
        let log_file_path = log_dir.join(&file_name);

        // Defence in depth: the leaves carry no separator, but re-check
        // containment (C# `PathHelper.IsContainedIn`) so the file can never land
        // outside the log directory.
        if !is_contained_in(&log_dir.to_string_lossy(), &log_file_path.to_string_lossy()) {
            return Err(ServiceError::invalid_input(
                "path resolved to filename not in log directory",
            ));
        }

        // Jellyfin's `ClientEventLogger` assumes the log dir exists (Serilog
        // creates it at startup). Don't depend on that side-effect: a missing
        // dir here silently drops the client's crash/diagnostic upload — the very
        // data this endpoint exists to capture — as a 500.
        tokio::fs::create_dir_all(&log_dir)
            .await
            .map_err(|e| ServiceError::backend(format!("create log directory: {e}")))?;

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_file_path)
            .await
            .map_err(|e| ServiceError::backend(format!("create log file: {e}")))?;
        file.write_all(contents)
            .await
            .map_err(|e| ServiceError::backend(format!("write log file: {e}")))?;
        file.flush()
            .await
            .map_err(|e| ServiceError::backend(format!("flush log file: {e}")))?;

        Ok(file_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_paths::FerrofinServerApplicationPaths;
    use std::path::Path;

    fn paths_in(dir: &Path) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(FerrofinServerApplicationPaths::new(
            dir.join("program-data"),
            dir.join("logs"),
            dir.join("config"),
            dir.join("cache"),
            dir.join("web"),
        ))
    }

    #[tokio::test]
    async fn write_document_keeps_the_leaf_of_a_traversal_client_name() {
        // `ClientEventLogger.WriteDocumentAsync` names the file with
        // `PathHelper.GetSafeLeafFileName`, so only the leaf survives.
        let tmp = tempfile::tempdir().unwrap();
        let logger = FerrofinClientEventLogger::new(paths_in(tmp.path()));
        let name = logger
            .write_document("../../etc/passwd", "1.0/../2.0", b"x")
            .await
            .unwrap();
        assert!(name.starts_with("upload_passwd_2.0_"), "{name}");
        assert!(tmp.path().join("logs").join(&name).is_file());
    }

    #[tokio::test]
    async fn write_document_creates_a_contained_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        tokio::fs::create_dir_all(&logs).await.unwrap();

        let logger = FerrofinClientEventLogger::new(paths_in(tmp.path()));
        let name = logger
            .write_document("Some Client", "10.9.0", b"hello diagnostics")
            .await
            .unwrap();

        assert!(name.starts_with("upload_Some Client_10.9.0_"));
        assert!(
            std::path::Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
        );
        let written = tokio::fs::read(logs.join(&name)).await.unwrap();
        assert_eq!(written, b"hello diagnostics");
    }

    #[tokio::test]
    async fn write_document_creates_missing_log_dir() {
        // Regression: the log dir not existing must not drop the upload (it did
        // in prod — client crash logs 500'd with "No such file or directory").
        let tmp = tempfile::tempdir().unwrap();
        // Note: no `logs` subdir created here.
        let logs = tmp.path().join("logs");
        assert!(!logs.exists());

        let logger = FerrofinClientEventLogger::new(paths_in(tmp.path()));
        let name = logger
            .write_document("Jellyfin Android TV", "0.18.0", b"crash trace")
            .await
            .expect("upload succeeds even when log dir is absent");
        assert_eq!(
            tokio::fs::read(logs.join(&name)).await.unwrap(),
            b"crash trace"
        );
    }

    #[tokio::test]
    async fn write_document_falls_back_for_empty_client() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("logs"))
            .await
            .unwrap();

        let logger = FerrofinClientEventLogger::new(paths_in(tmp.path()));
        let name = logger.write_document("..", "", b"x").await.unwrap();
        assert!(name.starts_with("upload_unknown-client_unknown-version_"));
    }
}
