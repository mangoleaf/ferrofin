//! Per-job reads of the persisted `encoding` named configuration.
//!
//! [`EncodingOptionsReader`] lives in `ferrofin-mediaencoding`, because the
//! transcode throttle there reads the same document on every due timer. The
//! transcode planner and subtitle extraction use it from here; the test below
//! pins it against the real configuration manager, whose read failures name
//! the file.

pub(crate) use ferrofin_mediaencoding::EncodingOptionsReader;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrofin_core::{FerrofinServerApplicationPaths, FerrofinServerConfigurationManager};
    use ferrofin_model::configuration::EncodingOptions;

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
