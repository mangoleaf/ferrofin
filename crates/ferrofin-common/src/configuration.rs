//! Configuration abstractions ported from `MediaBrowser.Common.Configuration`.
//!
//! Jellyfin registers a set of *configuration stores* at startup, each keyed by
//! a name and a .NET `Type`, through `IConfigurationFactory`. Ferrofin matches
//! each named store by its key instead (reads in `ferrofin-core`'s configuration
//! manager, saves in `ferrofin-api`'s `handlers/config.rs`), so only the
//! validation and post-save check interfaces live here: the runtime
//! `IConfigurationManager` is the `ServerConfigurationManager` trait in
//! `ferrofin-traits`, implemented in `ferrofin-core`.

/// A configuration store that can validate a proposed change before saving.
///
/// Port of `IValidatingConfiguration`. `old_config`/`new_config` are the
/// serialized bodies of the previous and proposed configuration.
pub trait ValidatingConfiguration: Send + Sync {
    /// Validates `new_config` against `old_config` prior to persisting it.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a validation message when the proposed configuration
    /// is rejected; the host aborts the save in that case.
    fn validate(
        &self,
        old_config: &str,
        new_config: &str,
    ) -> Result<(), ConfigurationValidationError>;
}

/// A check run after a named configuration save has been committed: one of
/// Jellyfin's `NamedConfigurationUpdated` handlers, which run synchronously
/// once the file is written, so the exception one throws answers the save
/// while the configuration stays saved.
pub trait CommittedConfigurationCheck: Send + Sync {
    /// Checks the server after the store `key` was saved.
    ///
    /// # Errors
    ///
    /// Returns the error the save is answered with. The saved configuration
    /// is not rolled back.
    fn check(&self, key: &str) -> Result<(), ConfigurationValidationError>;
}

/// A rejected configuration update, retaining Jellyfin's exception category.
#[derive(Debug, thiserror::Error)]
pub enum ConfigurationValidationError {
    /// A newly selected directory does not exist (`DirectoryNotFoundException`).
    #[error("{0}")]
    DirectoryNotFound(String),
    /// A newly selected file does not exist (`FileNotFoundException`).
    #[error("{0}")]
    FileNotFound(String),
    /// The store cannot accept the operation (`InvalidOperationException`).
    #[error("{0}")]
    InvalidOperation(String),
}
