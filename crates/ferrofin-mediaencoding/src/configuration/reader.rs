//! Repeated reads of the persisted `encoding` named configuration.
//!
//! The transcode planner and subtitle extraction read the live document once
//! per job, and the transcode throttle once per due timer of every running job,
//! so a dashboard change applies to the next one. A document that does not
//! parse already reads as [`EncodingOptions::default`] in the configuration
//! manager, as upstream's `BaseConfigurationManager.LoadConfiguration` does.
//! This module covers the remaining failure, a file that cannot be read at all,
//! where upstream lets the `IOException` through: a transcode is not failed
//! over its settings file, so its readers carry on with the defaults, and
//! [`EncodingOptionsReader`] keeps that failure visible without repeating it
//! for every job or tick.

use std::sync::{Mutex, PoisonError};

use ferrofin_model::configuration::EncodingOptions;
use ferrofin_traits::configuration::ServerConfigurationManager;

/// Reads the `encoding` named configuration for a caller that reads it again
/// and again, falling back to the defaults when the document cannot be read.
///
/// The document is reread on every call; a `warn` on each failed read would
/// follow the request or timer rate, which LOGGING.md §1 rules out. So a
/// failure is logged at `warn` the first time it is seen and at `debug` while
/// the same failure repeats. A successful read re-arms the warning, so a file
/// that breaks again later is reported again.
#[derive(Debug)]
pub struct EncodingOptionsReader {
    /// What reads the options, named in the log line.
    reader: &'static str,
    /// The failure last logged at `warn`; cleared by a successful read.
    reported: Mutex<Option<String>>,
}

impl EncodingOptionsReader {
    /// A reader that names itself `reader` in its log lines.
    #[must_use]
    pub const fn new(reader: &'static str) -> Self {
        Self {
            reader,
            reported: Mutex::new(None),
        }
    }

    /// The live encoding options, or the defaults when they cannot be read.
    pub async fn read(&self, config: &dyn ServerConfigurationManager) -> EncodingOptions {
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
pub(crate) mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use ferrofin_model::branding::BrandingOptions;
    use ferrofin_model::configuration::ServerConfiguration;
    use ferrofin_traits::error::ServiceError;

    use super::*;

    /// Log lines written while a [`CapturedLogs::install`] guard is held.
    #[derive(Clone, Default)]
    pub(crate) struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl CapturedLogs {
        /// Captures this thread's events at every level until the guard drops.
        pub(crate) fn install(&self) -> tracing::subscriber::DefaultGuard {
            tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_writer(self.clone())
                    .with_ansi(false)
                    .with_max_level(tracing::Level::DEBUG)
                    .finish(),
            )
        }

        /// The captured lines logged at `level` (`"WARN"`, `"DEBUG"`, …).
        pub(crate) fn lines_at(&self, level: &str) -> Vec<String> {
            let text = String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned();
            text.lines()
                .filter(|line| line.contains(&format!(" {level} ")))
                .map(str::to_owned)
                .collect()
        }
    }

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// A configuration manager whose encoding read answers what the test set.
    struct Scripted(Mutex<Result<EncodingOptions, String>>);

    #[async_trait]
    impl ServerConfigurationManager for Scripted {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            panic!("this test only reads encoding options")
        }
        async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
            Err(ServiceError::backend("unexpected general config read"))
        }
        async fn update_configuration(
            &self,
            _configuration: &ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Err(ServiceError::backend("unexpected general config write"))
        }
        async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
            Err(ServiceError::backend("unexpected branding read"))
        }
        async fn update_branding(&self, _branding: &BrandingOptions) -> Result<(), ServiceError> {
            Err(ServiceError::backend("unexpected branding write"))
        }
        async fn get_encoding_options(&self) -> Result<EncodingOptions, ServiceError> {
            self.0
                .lock()
                .unwrap()
                .clone()
                .map_err(ServiceError::backend)
        }
    }

    /// One `warn` per distinct read failure, `debug` while it repeats, and a
    /// fresh `warn` once the document has read again and then breaks.
    #[tokio::test]
    async fn a_failure_is_warned_about_once_until_the_document_reads_again() {
        let saved = EncodingOptions {
            encoding_thread_count: 3,
            ..EncodingOptions::default()
        };
        let config = Scripted(Mutex::new(Err("is a directory".to_owned())));
        let reader = EncodingOptionsReader::new("test");
        let logs = CapturedLogs::default();
        let guard = logs.install();

        assert_eq!(reader.read(&config).await, EncodingOptions::default());
        assert_eq!(reader.read(&config).await, EncodingOptions::default());
        assert_eq!(logs.lines_at("WARN").len(), 1);
        assert_eq!(logs.lines_at("DEBUG").len(), 1);

        // A different failure is news.
        *config.0.lock().unwrap() = Err("permission denied".to_owned());
        reader.read(&config).await;
        assert_eq!(logs.lines_at("WARN").len(), 2);

        *config.0.lock().unwrap() = Ok(saved.clone());
        assert_eq!(reader.read(&config).await, saved);

        // Broken the same way again after a good read: warned again.
        *config.0.lock().unwrap() = Err("permission denied".to_owned());
        reader.read(&config).await;
        drop(guard);

        let warnings = logs.lines_at("WARN");
        assert_eq!(warnings.len(), 3, "{warnings:#?}");
        assert!(
            warnings.iter().all(|line| line.contains("reader=\"test\"")),
            "{warnings:#?}"
        );
        let lines = logs.lines_at("ERROR");
        assert!(lines.is_empty(), "{lines:?}");
    }
}
