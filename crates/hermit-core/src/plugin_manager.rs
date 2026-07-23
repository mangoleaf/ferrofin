//! [`HermitPluginManager`] — a **null** [`PluginManager`] for the deferred plugin
//! subsystem.
//!
//! Port of `Emby.Server.Implementations.Plugins.PluginManager` reduced to its
//! null shape (the C# `NullPluginManager` used when plugin loading is
//! disabled). The assembly-loading / DI-registration / manifest machinery and
//! plugin *updates* are explicitly out of scope for this wave, so no plugins are
//! ever installed: [`list_plugins`](PluginManager::list_plugins) returns `[]`,
//! lookups return `None`, and the enable/disable/remove mutators reject any id
//! as not found.
//!
//! The seam exists so the DI graph can name an `Arc<dyn PluginManager>`; a real
//! plugin host is a future wave.

use async_trait::async_trait;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::stubs::PluginManager;
use hermit_traits::stubs::plugins::PluginDescriptor;

/// The null plugin manager for the deferred plugin subsystem.
///
/// Reports no installed plugins; every id-addressed mutator is a
/// [`ServiceError::NotFound`].
#[derive(Debug, Clone, Copy, Default)]
pub struct HermitPluginManager;

impl HermitPluginManager {
    /// Creates the null plugin manager.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// The shared "no such plugin" error for the id-addressed mutators.
    fn not_found(id: Uuid) -> ServiceError {
        ServiceError::not_found(format!("plugin {id}"))
    }
}

#[async_trait]
impl PluginManager for HermitPluginManager {
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
        // Plugins are deferred: none are installed.
        Ok(Vec::new())
    }

    async fn get_plugin(&self, _id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
        Ok(None)
    }

    async fn enable_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        Err(Self::not_found(id))
    }

    async fn disable_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        Err(Self::not_found(id))
    }

    async fn remove_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        Err(Self::not_found(id))
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use hermit_traits::error::ServiceError;
    use hermit_traits::stubs::PluginManager;

    use super::HermitPluginManager;

    #[tokio::test]
    async fn no_plugins_installed() {
        let mgr = HermitPluginManager::new();
        let id = Uuid::new_v4();
        assert!(mgr.list_plugins().await.expect("list").is_empty());
        assert!(mgr.get_plugin(id).await.expect("get").is_none());
        assert!(matches!(
            mgr.enable_plugin(id).await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(matches!(
            mgr.disable_plugin(id).await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(matches!(
            mgr.remove_plugin(id).await,
            Err(ServiceError::NotFound(_))
        ));
    }
}
