//! Configuration abstractions ported from `MediaBrowser.Common.Configuration`.
//!
//! Jellyfin registers a set of *configuration stores* at startup, each keyed by
//! a name and a .NET `Type`. Rust has no runtime `Type`, so the store's type is
//! represented as a `type_key` string (e.g. the fully-qualified config type
//! name) that the host resolves. The DI/runtime `IConfigurationManager` and the
//! `IApplicationHost`-bound pieces are deferred; the portable value/interface
//! shapes live here.

/// Describes a single entry in the application configuration.
///
/// Port of `ConfigurationStore`. The C# `Type ConfigurationType` field becomes
/// a `type_key` string because Rust carries no runtime type token.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigurationStore {
    /// The unique identifier for the configuration.
    pub key: String,
    /// A key identifying the type used to store this configuration entry.
    pub type_key: String,
}

/// Payload for the "configuration updated" event.
///
/// Port of `ConfigurationUpdateEventArgs`. The C# `object NewConfiguration`
/// becomes a `type_key` plus the serialized configuration body, keeping the
/// struct free of a runtime `Type` while still identifying what changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigurationUpdateEventArgs {
    /// The configuration key that changed.
    pub key: String,
    /// A key identifying the type of the new configuration.
    pub type_key: String,
}

impl ConfigurationUpdateEventArgs {
    /// Creates a new event payload for `key`.
    #[must_use]
    pub fn new(key: impl Into<String>, type_key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            type_key: type_key.into(),
        }
    }
}

/// Provides a set of configuration stores for a module or plugin.
///
/// Port of `IConfigurationFactory`. Scanned at startup to dynamically register
/// configuration for various modules/plugins.
pub trait ConfigurationFactory {
    /// Returns the configuration stores for this module.
    fn configurations(&self) -> Vec<ConfigurationStore>;
}

/// A configuration store that can validate a proposed change before saving.
///
/// Port of `IValidatingConfiguration`. `old_config`/`new_config` are the
/// serialized bodies of the previous and proposed configuration.
pub trait ValidatingConfiguration {
    /// Validates `new_config` against `old_config` prior to persisting it.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a validation message when the proposed configuration
    /// is rejected; the host aborts the save in that case.
    fn validate(&self, old_config: &str, new_config: &str) -> Result<(), String>;
}
