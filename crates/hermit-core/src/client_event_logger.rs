//! [`HermitClientEventLogger`] — persists client-uploaded diagnostic documents.
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
//!   [`write_document`](hermit_traits::events::ClientEventLogger::write_document)
//!   taking owned bytes (`&[u8]`) — the trait already folded the `Stream` to
//!   `Send`-safe bytes.
//! - `PathHelper.GetSafeLeafFileName` (strip path separators / invalid chars) is
//!   ported as the local [`safe_leaf`] helper; `IsContainedIn` becomes the
//!   [`Path::starts_with`] containment check. Both keep an attacker-supplied
//!   `clientName`/`clientVersion` from escaping the log directory.
//! - File creation uses `create_new` (fail if it already exists), matching the
//!   C# `FileMode.CreateNew`; the `Guid` in the name makes a collision
//!   effectively impossible.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::events::ClientEventLogger;
use hermit_traits::system::ServerApplicationPaths;

/// The UTC timestamp format used in the generated log file name (C#
/// `yyyyMMddHHmmss`).
const TIMESTAMP_FORMAT: &str = "%Y%m%d%H%M%S";

/// The concrete client-event logger.
///
/// Writes each uploaded document into the injected paths' log directory.
#[derive(Clone)]
pub struct HermitClientEventLogger {
    paths: Arc<dyn ServerApplicationPaths>,
}

impl std::fmt::Debug for HermitClientEventLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitClientEventLogger")
            .finish_non_exhaustive()
    }
}

impl HermitClientEventLogger {
    /// Creates a client-event logger writing under the injected paths' log
    /// directory.
    #[must_use]
    pub fn new(paths: Arc<dyn ServerApplicationPaths>) -> Self {
        Self { paths }
    }
}

/// Reduces an arbitrary string to a safe single path segment, or `None` when it
/// contains no usable characters.
///
/// Port of `PathHelper.GetSafeLeafFileName`: it drops path separators and the
/// characters the host filesystem rejects in a leaf name, so a malicious
/// `clientName` like `../../etc/passwd` cannot introduce directory traversal.
fn safe_leaf(value: &str) -> Option<String> {
    let cleaned: String = value
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    c,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
                )
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

#[async_trait]
impl ClientEventLogger for HermitClientEventLogger {
    async fn write_document(
        &self,
        client_name: &str,
        client_version: &str,
        contents: &[u8],
    ) -> Result<String, ServiceError> {
        let safe_client_name =
            safe_leaf(client_name).unwrap_or_else(|| "unknown-client".to_owned());
        let safe_client_version =
            safe_leaf(client_version).unwrap_or_else(|| "unknown-version".to_owned());
        let timestamp = Utc::now().format(TIMESTAMP_FORMAT);
        let unique = Uuid::new_v4().simple();
        let file_name =
            format!("upload_{safe_client_name}_{safe_client_version}_{timestamp}_{unique}.log");

        let log_dir = PathBuf::from(self.paths.log_directory_path());
        let log_file_path = log_dir.join(&file_name);

        // Defence in depth: `safe_leaf` already strips separators, but re-check
        // containment (C# `PathHelper.IsContainedIn`) so the file can never land
        // outside the log directory.
        if !is_contained_in(&log_dir, &log_file_path) {
            return Err(ServiceError::invalid_input(
                "path resolved to filename not in log directory",
            ));
        }

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

/// Whether `candidate` resolves to a path inside `parent` (C#
/// `PathHelper.IsContainedIn`).
fn is_contained_in(parent: &Path, candidate: &Path) -> bool {
    candidate.starts_with(parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_paths::HermitServerApplicationPaths;

    fn paths_in(dir: &Path) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(HermitServerApplicationPaths::new(
            dir.join("program-data"),
            dir.join("logs"),
            dir.join("config"),
            dir.join("cache"),
            dir.join("web"),
        ))
    }

    #[test]
    fn safe_leaf_strips_traversal() {
        assert_eq!(safe_leaf("../../etc/passwd").as_deref(), Some("etcpasswd"));
        assert_eq!(safe_leaf("Jellyfin Web").as_deref(), Some("Jellyfin Web"));
        assert_eq!(safe_leaf("..").as_deref(), None);
        assert_eq!(safe_leaf("   ").as_deref(), None);
        assert!(
            safe_leaf("a/b:c*d")
                .unwrap()
                .chars()
                .all(char::is_alphanumeric)
        );
    }

    #[tokio::test]
    async fn write_document_creates_a_contained_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        tokio::fs::create_dir_all(&logs).await.unwrap();

        let logger = HermitClientEventLogger::new(paths_in(tmp.path()));
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
    async fn write_document_falls_back_for_empty_client() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(tmp.path().join("logs"))
            .await
            .unwrap();

        let logger = HermitClientEventLogger::new(paths_in(tmp.path()));
        let name = logger.write_document("..", "", b"x").await.unwrap();
        assert!(name.starts_with("upload_unknown-client_unknown-version_"));
    }

    #[test]
    fn containment_check_rejects_escape() {
        let parent = Path::new("/var/log/hermit");
        assert!(is_contained_in(parent, Path::new("/var/log/hermit/a.log")));
        assert!(!is_contained_in(parent, Path::new("/var/log/other/a.log")));
    }
}
