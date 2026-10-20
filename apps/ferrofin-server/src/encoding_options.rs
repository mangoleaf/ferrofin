//! Per-job reads of the persisted `encoding` named configuration.
//!
//! The transcode planner and subtitle extraction read the live document once
//! per job, so a dashboard change applies to the next one. A document that does
//! not parse already reads as [`EncodingOptions::default`] in the
//! configuration manager, as upstream's `BaseConfigurationManager.
//! LoadConfiguration` does. This module covers the remaining failure, a file
//! that cannot be read at all, where upstream lets the `IOException` through:
//! a transcode is not failed over its settings file, so they carry on with the
//! defaults, and [`EncodingOptionsReader`] keeps that failure visible without
//! repeating it for every job.

use std::sync::{Mutex, PoisonError};

use ferrofin_model::configuration::EncodingOptions;
use ferrofin_traits::configuration::ServerConfigurationManager;

/// Reads the `encoding` named configuration for a caller that runs once per
/// job, falling back to the defaults when the document cannot be read.
///
/// Ferrofin rereads the file for every job; a `warn` on each failed read would
/// follow the request rate, which LOGGING.md §1 rules out. So a failure is
/// logged at `warn` the first time it is seen and at `debug` while the same
/// failure repeats. A successful read re-arms the warning, so a file that
/// breaks again later is reported again.
pub(crate) struct EncodingOptionsReader {
    /// What reads the options, named in the log line.
    reader: &'static str,
    /// The failure last logged at `warn`; cleared by a successful read.
    reported: Mutex<Option<String>>,
}

impl EncodingOptionsReader {
    /// A reader that names itself `reader` in its log lines.
    pub(crate) const fn new(reader: &'static str) -> Self {
        Self {
            reader,
            reported: Mutex::new(None),
        }
    }

    /// The live encoding options, or the defaults when they cannot be read.
    pub(crate) async fn read(&self, config: &dyn ServerConfigurationManager) -> EncodingOptions {
        match config.get_encoding_options().await {
            Ok(options) => {
                self.reported
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                options
            }
            Err(error) => {
                let error = error.to_string();
                let first = self
                    .reported
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .replace(error.clone())
                    .as_ref()
                    != Some(&error);
                if first {
                    tracing::warn!(
                        reader = self.reader,
                        %error,
                        "cannot read the encoding configuration; using its defaults until it is fixed"
                    );
                } else {
                    tracing::debug!(
                        reader = self.reader,
                        %error,
                        "cannot read the encoding configuration; using its defaults"
                    );
                }
                EncodingOptions::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrofin_core::{FerrofinServerApplicationPaths, FerrofinServerConfigurationManager};

    use super::*;
    use crate::test_logs::CapturedLogs;

    /// The real configuration manager over `root`, and its `encoding.json`.
    async fn manager(
        root: &std::path::Path,
    ) -> (FerrofinServerConfigurationManager, std::path::PathBuf) {
        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            root.join("data"),
            root.join("log"),
            root.join("config"),
            root.join("cache"),
            root.join("web"),
        ));
        let file = root.join("config/users/named/encoding.json");
        let manager = FerrofinServerConfigurationManager::load(paths)
            .await
            .expect("configuration");
        std::fs::create_dir_all(file.parent().expect("named dir")).expect("named dir");
        (manager, file)
    }

    /// One `warn` per distinct read failure, `debug` while it repeats, and a
    /// fresh `warn` once the file has read again and then breaks. (A document
    /// that does not parse never reaches the reader: the manager serves the
    /// defaults for it.)
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failure_is_warned_about_once_until_the_file_reads_again() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("root");
        let (config, file) = manager(root.path()).await;
        let reader = EncodingOptionsReader::new("test");
        let saved = EncodingOptions {
            encoding_thread_count: 3,
            ..EncodingOptions::default()
        };

        let logs = CapturedLogs::default();
        let guard = logs.install();
        // A directory where the file should be cannot be read.
        std::fs::create_dir(&file).expect("unreadable");
        assert_eq!(reader.read(&config).await, EncodingOptions::default());
        assert_eq!(reader.read(&config).await, EncodingOptions::default());
        assert_eq!(logs.lines_at("WARN").len(), 1);
        assert_eq!(logs.lines_at("DEBUG").len(), 1);

        // A different failure is news: a file the server may not read.
        std::fs::remove_dir(&file).expect("remove dir");
        std::fs::write(&file, serde_json::to_vec(&saved).expect("json")).expect("file");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let readable_anyway = std::fs::read(&file).is_ok();
        reader.read(&config).await;
        if !readable_anyway {
            assert_eq!(logs.lines_at("WARN").len(), 2);
        }

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(reader.read(&config).await, saved);

        // Broken the same way again after a good read: warned again.
        std::fs::remove_file(&file).expect("remove file");
        std::fs::create_dir(&file).expect("unreadable again");
        reader.read(&config).await;
        drop(guard);

        let warnings = logs.lines_at("WARN");
        // A root test run reads a mode-000 file, so that step warns nothing.
        assert_eq!(
            warnings.len(),
            if readable_anyway { 2 } else { 3 },
            "{warnings:#?}"
        );
        let path = file.display().to_string();
        assert!(
            warnings.iter().all(|line| line.contains(&path)),
            "{warnings:#?}"
        );
        assert!(
            warnings.iter().all(|line| line.contains("reader=\"test\"")),
            "{warnings:#?}"
        );
    }
}
