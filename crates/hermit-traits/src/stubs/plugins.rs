//! Minimal plugin manager trait (deferred subsystem).
//!
//! Port of a representative slice of
//! `MediaBrowser.Common.Plugins.IPluginManager`. Plugins are deferred, so the
//! assembly-loading / DI-registration / manifest machinery and the `LocalPlugin`
//! domain type are **not** ported. A plugin is identified by [`uuid::Uuid`] and
//! described by the local [`PluginDescriptor`].
//!
//! Port rules applied: synchronous C# methods become `async fn -> Result`;
//! `Guid` → [`uuid::Uuid`].

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::ServiceError;

/// A minimal description of an installed plugin.
///
/// Stands in for the un-ported `LocalPlugin`/`PluginManifest` domain types: just
/// the id, name, version and enabled state the manager surface needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginDescriptor {
    /// The plugin's stable id.
    pub id: Uuid,
    /// The plugin's display name.
    pub name: String,
    /// The plugin's version string.
    pub version: String,
    /// Whether the plugin is currently enabled.
    pub enabled: bool,
}

/// The (deferred) plugin manager.
///
/// Port of `IPluginManager` (minimal slice).
#[async_trait]
pub trait PluginManager: Send + Sync {
    /// Lists all installed plugins.
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError>;

    /// Gets a single plugin by id, if installed.
    async fn get_plugin(&self, id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError>;

    /// Enables the plugin with the given id.
    async fn enable_plugin(&self, id: Uuid) -> Result<(), ServiceError>;

    /// Disables the plugin with the given id.
    async fn disable_plugin(&self, id: Uuid) -> Result<(), ServiceError>;

    /// Removes the plugin with the given id.
    async fn remove_plugin(&self, id: Uuid) -> Result<(), ServiceError>;
}

fn _assert_object_safe_plugin_manager(_: &dyn PluginManager) {}

#[cfg(test)]
mod tests {
    use super::PluginDescriptor;

    #[test]
    fn descriptor_default_is_empty_and_disabled() {
        let d = PluginDescriptor::default();
        assert!(d.name.is_empty());
        assert!(d.version.is_empty());
        assert!(!d.enabled);
        assert!(d.id.is_nil());
    }
}
