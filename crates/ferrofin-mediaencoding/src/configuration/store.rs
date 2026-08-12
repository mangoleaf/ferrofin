//! Port of `EncodingConfigurationStore` + `EncodingConfigurationFactory`.

use ferrofin_common::configuration::{
    ConfigurationFactory, ConfigurationStore, ValidatingConfiguration,
};
use ferrofin_model::configuration::EncodingOptions;

/// The configuration key under which encoding options are stored.
///
/// Port of the `Key = "encoding"` set in the C# constructor.
pub const ENCODING_KEY: &str = "encoding";

/// The `type_key` identifying the [`EncodingOptions`] body of this store.
///
/// Port of the C# `ConfigurationType = typeof(EncodingOptions)`; Rust carries no
/// runtime `Type`, so the fully-qualified type name stands in.
pub const ENCODING_TYPE_KEY: &str = "ferrofin_model::configuration::EncodingOptions";

/// Checks whether a directory exists on disk.
///
/// The un-mockable filesystem probe used by
/// [`EncodingConfigurationStore::validate`] (C# `Directory.Exists`) lives behind
/// this seam so unit tests inject a deterministic fake and the real `std::fs`
/// call stays out of the coverage/parity numbers.
pub trait DirChecker: Send + Sync {
    /// Returns whether `path` names an existing directory.
    fn directory_exists(&self, path: &str) -> bool;
}

/// The production [`DirChecker`] backed by [`std::path::Path::is_dir`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RealDirChecker;

impl DirChecker for RealDirChecker {
    fn directory_exists(&self, path: &str) -> bool {
        std::path::Path::new(path).is_dir()
    }
}

/// The `"encoding"` configuration store.
///
/// Port of `EncodingConfigurationStore`. Beyond describing the store (via
/// [`store`](Self::store)) it validates a proposed [`EncodingOptions`] change
/// before it is persisted.
pub struct EncodingConfigurationStore<D: DirChecker = RealDirChecker> {
    dir_checker: D,
}

impl Default for EncodingConfigurationStore<RealDirChecker> {
    fn default() -> Self {
        Self::new(RealDirChecker)
    }
}

impl<D: DirChecker> EncodingConfigurationStore<D> {
    /// Creates a store validating directory existence via `dir_checker`.
    pub fn new(dir_checker: D) -> Self {
        Self { dir_checker }
    }

    /// Returns the [`ConfigurationStore`] descriptor for encoding options.
    ///
    /// Port of the C# constructor's `Key`/`ConfigurationType` assignment.
    #[must_use]
    pub fn store(&self) -> ConfigurationStore {
        ConfigurationStore {
            key: ENCODING_KEY.to_owned(),
            type_key: ENCODING_TYPE_KEY.to_owned(),
        }
    }
}

impl<D: DirChecker> ValidatingConfiguration for EncodingConfigurationStore<D> {
    /// Validates a proposed encoding-options change.
    ///
    /// Port of `EncodingConfigurationStore.Validate`:
    /// - a new, non-blank `TranscodingTempPath` that differs from the old value
    ///   must name an existing directory (else `DirectoryNotFoundException`);
    /// - `EncoderAppPath` may not be changed to a new non-blank value (else
    ///   `InvalidOperationException`).
    ///
    /// The `old_config`/`new_config` bodies are the JSON-serialized
    /// [`EncodingOptions`].
    fn validate(&self, old_config: &str, new_config: &str) -> Result<(), String> {
        let old: EncodingOptions = serde_json::from_str(old_config)
            .map_err(|e| format!("invalid old encoding configuration: {e}"))?;
        let new: EncodingOptions = serde_json::from_str(new_config)
            .map_err(|e| format!("invalid new encoding configuration: {e}"))?;

        let new_path = new.transcoding_temp_path.as_deref().unwrap_or_default();
        if !new_path.trim().is_empty()
            && old.transcoding_temp_path.as_deref() != Some(new_path)
            && !self.dir_checker.directory_exists(new_path)
        {
            return Err(format!("{new_path} does not exist."));
        }

        let new_encoder = new.encoder_app_path.as_deref().unwrap_or_default();
        if !new_encoder.trim().is_empty() && old.encoder_app_path.as_deref() != Some(new_encoder) {
            return Err("Unable to update encoder app path.".to_owned());
        }

        Ok(())
    }
}

/// Provides the encoding configuration store to the host at startup.
///
/// Port of `EncodingConfigurationFactory`.
#[derive(Debug, Clone, Copy, Default)]
pub struct EncodingConfigurationFactory;

impl ConfigurationFactory for EncodingConfigurationFactory {
    /// Returns the single encoding [`ConfigurationStore`].
    ///
    /// Port of `EncodingConfigurationFactory.GetConfigurations`.
    fn configurations(&self) -> Vec<ConfigurationStore> {
        vec![ConfigurationStore {
            key: ENCODING_KEY.to_owned(),
            type_key: ENCODING_TYPE_KEY.to_owned(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigurationFactory, DirChecker, ENCODING_KEY, EncodingConfigurationFactory,
        EncodingConfigurationStore, ValidatingConfiguration,
    };
    use ferrofin_model::configuration::EncodingOptions;

    /// A fake [`DirChecker`] answering from a fixed allow-list.
    struct FakeDirs(Vec<String>);

    impl DirChecker for FakeDirs {
        fn directory_exists(&self, path: &str) -> bool {
            self.0.iter().any(|p| p == path)
        }
    }

    fn opts() -> EncodingOptions {
        EncodingOptions::default()
    }

    fn json(o: &EncodingOptions) -> String {
        serde_json::to_string(o).unwrap()
    }

    #[test]
    fn factory_exposes_encoding_store() {
        let stores = EncodingConfigurationFactory.configurations();
        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].key, ENCODING_KEY);
    }

    #[test]
    fn unchanged_config_validates() {
        let store = EncodingConfigurationStore::new(FakeDirs(vec![]));
        let cfg = opts();
        assert!(store.validate(&json(&cfg), &json(&cfg)).is_ok());
    }

    #[test]
    fn new_temp_path_must_exist() {
        let store = EncodingConfigurationStore::new(FakeDirs(vec![]));
        let old = opts();
        let mut new = opts();
        new.transcoding_temp_path = Some("/does/not/exist".to_owned());
        let err = store.validate(&json(&old), &json(&new)).unwrap_err();
        assert_eq!(err, "/does/not/exist does not exist.");
    }

    #[test]
    fn existing_new_temp_path_is_accepted() {
        let store = EncodingConfigurationStore::new(FakeDirs(vec!["/tmp/transcodes".to_owned()]));
        let old = opts();
        let mut new = opts();
        new.transcoding_temp_path = Some("/tmp/transcodes".to_owned());
        assert!(store.validate(&json(&old), &json(&new)).is_ok());
    }

    #[test]
    fn blank_temp_path_skips_the_directory_check() {
        let store = EncodingConfigurationStore::new(FakeDirs(vec![]));
        let old = opts();
        let mut new = opts();
        new.transcoding_temp_path = Some("   ".to_owned());
        assert!(store.validate(&json(&old), &json(&new)).is_ok());
    }

    #[test]
    fn changing_encoder_app_path_is_rejected() {
        let store = EncodingConfigurationStore::new(FakeDirs(vec![]));
        let old = opts();
        let mut new = opts();
        new.encoder_app_path = Some("/usr/bin/ffmpeg".to_owned());
        let err = store.validate(&json(&old), &json(&new)).unwrap_err();
        assert_eq!(err, "Unable to update encoder app path.");
    }
}
