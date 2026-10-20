//! Per-job reads of the persisted `encoding` named configuration.
//!
//! The transcode planner and subtitle extraction read the live document once
//! per job, so a dashboard change applies to the next one. When it does not
//! parse they carry on with [`EncodingOptions::default`], as upstream's
//! `BaseConfigurationManager.LoadConfiguration` does. They also carry on when
//! it cannot be read at all, where upstream lets the `IOException` through; a
//! transcode is not failed over its settings file. [`EncodingOptionsReader`]
//! keeps either failure visible without repeating it for every job.

use std::sync::{Mutex, PoisonError};

use ferrofin_model::configuration::EncodingOptions;
use ferrofin_traits::configuration::ServerConfigurationManager;

/// Reads the `encoding` named configuration for a caller that runs once per
/// job, falling back to the defaults when the document is unusable.
///
/// For a document that does not parse, upstream logs `Error loading
/// configuration file: {Path}` and caches the default instance it falls back
/// to, so a broken file is reported once per process. Ferrofin rereads the
/// file for every job; a `warn` on each failed read would follow the request
/// rate, which LOGGING.md §1 rules out. So a failure is logged at `warn` the
/// first time it is seen and at `debug` while the same failure repeats. A
/// successful read re-arms the warning, so a file that breaks again later is
/// reported again.
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

    /// One `warn` per distinct failure, `debug` while it repeats, and a fresh
    /// `warn` once the file has read again and then breaks.
    #[tokio::test]
    async fn a_failure_is_warned_about_once_until_the_file_reads_again() {
        let root = tempfile::tempdir().expect("root");
        let (config, file) = manager(root.path()).await;
        let reader = EncodingOptionsReader::new("test");
        let saved = EncodingOptions {
            encoding_thread_count: 3,
            ..EncodingOptions::default()
        };

        let logs = CapturedLogs::default();
        let guard = logs.install();
        std::fs::write(&file, b"{ not json").expect("broken");
        assert_eq!(reader.read(&config).await, EncodingOptions::default());
        assert_eq!(reader.read(&config).await, EncodingOptions::default());
        assert_eq!(logs.lines_at("WARN").len(), 1);
        assert_eq!(logs.lines_at("DEBUG").len(), 1);

        // A different failure is news.
        std::fs::write(&file, b"\"text\"").expect("wrong shape");
        reader.read(&config).await;
        assert_eq!(logs.lines_at("WARN").len(), 2);

        std::fs::write(&file, serde_json::to_vec(&saved).expect("json")).expect("fixed");
        assert_eq!(reader.read(&config).await, saved);

        // Broken the same way again after a good read: warned again.
        std::fs::write(&file, b"{ not json").expect("broken again");
        reader.read(&config).await;
        drop(guard);

        let warnings = logs.lines_at("WARN");
        assert_eq!(warnings.len(), 3, "{warnings:#?}");
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
