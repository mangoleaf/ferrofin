//! Server settings manager trait — the DI seam for the Ferrofin settings
//! page's `GET /Ferrofin/Settings`.
//!
//! The settings are Ferrofin's own bootstrap configuration (CLI flags,
//! `FERROFIN_*` environment variables, `config.toml`), which only the
//! composition root holds, so the server binary implements this trait;
//! `ferrofin-api` reaches it only through this object-safe trait.

use async_trait::async_trait;
use ferrofin_model::server_settings::ServerSettingsDto;

use crate::error::ServiceError;

/// Reports the Ferrofin bootstrap settings to the settings page.
///
/// Every method self-gates on the Ferrofin plugin's enabled flag, reporting
/// [`ServiceError::NotFound`] while it is disabled, the rule the other
/// compiled-in extensions follow (a disabled plugin's routes answer `404`).
#[async_trait]
pub trait ServerSettingsManager: Send + Sync {
    /// Every bootstrap setting with its running value, its default, where the
    /// value comes from, and whether the settings page may change it.
    ///
    /// # Errors
    ///
    /// [`ServiceError::NotFound`] while the Ferrofin plugin is disabled, and
    /// [`ServiceError::Conflict`] when `config.toml` changed since the server
    /// started and no longer loads.
    async fn snapshot(&self) -> Result<ServerSettingsDto, ServiceError>;
}

fn _assert_object_safe(_: &dyn ServerSettingsManager) {}

/// A [`ServerSettingsManager`] for a server with no settings wired: every
/// method answers [`ServiceError::NotFound`], as a disabled Ferrofin plugin
/// does. The [`AppState`](../../ferrofin_api/state) default, so test
/// constructors compile; the composition root injects the real manager.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledServerSettings;

#[async_trait]
impl ServerSettingsManager for DisabledServerSettings {
    async fn snapshot(&self) -> Result<ServerSettingsDto, ServiceError> {
        Err(ServiceError::not_found(
            "the Ferrofin settings are not available",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{DisabledServerSettings, ServerSettingsManager};
    use crate::error::ServiceError;

    #[tokio::test]
    async fn the_disabled_manager_answers_not_found() {
        let result = DisabledServerSettings.snapshot().await;
        assert!(
            matches!(result, Err(ServiceError::NotFound(_))),
            "{result:?}"
        );
    }
}
