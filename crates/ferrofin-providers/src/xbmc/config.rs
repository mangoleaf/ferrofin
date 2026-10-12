//! NFO parser configuration — port of
//! `MediaBrowser.Model.Configuration.XbmcMetadataOptions` (the subset the NFO
//! parsers read) plus the `GetNfoConfiguration` accessor.
//!
//! Upstream this type lives in `MediaBrowser.Model.Configuration` and is fetched
//! via `IConfigurationManager.GetConfiguration<XbmcMetadataOptions>("xbmcmetadata")`.
//! The parser uses this reduced view of the model configuration; the automatic
//! saver reads the full saved `XbmcMetadataOptions` at runtime.

/// The default release-date format the parsers use for `aired`/`premiered`/
/// `releasedate`/`formed`/`enddate` tags.
///
/// Port of the `ReleaseDateFormat = "yyyy-MM-dd"` set in the C# constructor.
pub const DEFAULT_RELEASE_DATE_FORMAT: &str = "yyyy-MM-dd";

/// Options controlling how NFO metadata is read and written.
///
/// Port of `MediaBrowser.Model.Configuration.XbmcMetadataOptions`, reduced to the
/// fields the parsers actually consult (`UserId`, `ReleaseDateFormat`). The
/// save-time flags (`SaveImagePathsInNfo`, `EnablePathSubstitution`,
/// `EnableExtraThumbsDuplication`) are read separately by the automatic saver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfoConfiguration {
    /// The user whose watched/playcount state `watched`/`playcount`/`lastplayed`
    /// tags apply to (`UserId`). `None`/unparseable disables that import.
    pub user_id: Option<String>,
    /// The `DateTime.TryParseExact` format for release-date tags
    /// (`ReleaseDateFormat`; defaults to `"yyyy-MM-dd"`).
    pub release_date_format: String,
}

impl Default for NfoConfiguration {
    fn default() -> Self {
        Self {
            user_id: None,
            release_date_format: DEFAULT_RELEASE_DATE_FORMAT.to_owned(),
        }
    }
}

impl NfoConfiguration {
    /// Creates the default NFO configuration (matching the C# constructor).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// The saved `xbmcmetadata` configuration as the NFO parsers see it
/// (`IConfigurationManager.GetNfoConfiguration()`), re-read when the file
/// changes so a dashboard edit applies to the next item without a restart.
///
/// The file is stat-checked on every call and re-parsed only when its modified
/// time or size moved; a missing or unparseable file reads as the defaults,
/// as an unset named configuration does upstream.
#[derive(Debug)]
pub struct NfoConfigurationFile {
    path: std::path::PathBuf,
    cached: std::sync::Mutex<Option<(std::time::SystemTime, u64, NfoConfiguration)>>,
}

impl NfoConfigurationFile {
    /// Reads `path` (`{config}/named/xbmcmetadata.json`).
    #[must_use]
    pub fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            cached: std::sync::Mutex::new(None),
        }
    }

    /// The current configuration.
    #[must_use]
    pub fn current(&self) -> NfoConfiguration {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return NfoConfiguration::default();
        };
        let stamp = (metadata.modified().ok(), metadata.len());
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let (Some(modified), Some((at, len, config))) = (stamp.0, cached.as_ref())
            && *at == modified
            && *len == stamp.1
        {
            return config.clone();
        }
        let config = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<ferrofin_model::configuration::XbmcMetadataOptions>(&bytes)
                    .ok()
            })
            .map_or_else(NfoConfiguration::default, |options| NfoConfiguration {
                user_id: options.user_id,
                release_date_format: options.release_date_format,
            });
        if let Some(modified) = stamp.0 {
            *cached = Some((modified, stamp.1, config.clone()));
        }
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_follows_the_file_and_defaults_when_it_is_absent_or_bad() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("xbmcmetadata.json");
        let source = NfoConfigurationFile::new(path.clone());
        assert_eq!(source.current(), NfoConfiguration::default());

        std::fs::write(
            &path,
            r#"{"UserId":"abc","ReleaseDateFormat":"dd.MM.yyyy"}"#,
        )
        .unwrap();
        let config = source.current();
        assert_eq!(config.release_date_format, "dd.MM.yyyy");
        assert_eq!(config.user_id.as_deref(), Some("abc"));

        // A change of size is picked up even within one timestamp tick.
        std::fs::write(&path, r#"{"ReleaseDateFormat":"yyyy/M/d"}"#).unwrap();
        assert_eq!(source.current().release_date_format, "yyyy/M/d");

        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(source.current(), NfoConfiguration::default());
    }
}
