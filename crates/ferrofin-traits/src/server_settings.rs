//! Server settings manager trait — the DI seam for the Ferrofin settings
//! page's `GET /Ferrofin/Settings` and `POST /Ferrofin/Settings`.
//!
//! The settings are Ferrofin's own bootstrap configuration (CLI flags,
//! `FERROFIN_*` environment variables, `config.toml`), which only the
//! composition root holds, so the server binary implements this trait;
//! `ferrofin-api` reaches it only through this object-safe trait.

use async_trait::async_trait;
use ferrofin_model::server_settings::{
    ServerSettingsDto, ServerSettingsUpdateDto, ServerSettingsUpdateResultDto,
};

use crate::error::ServiceError;

/// Reports the Ferrofin bootstrap settings to the settings page, and saves
/// its changes to `config.toml`.
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

    /// Saves `update`'s changes to `config.toml`, each checked by the rules the
    /// server applies at boot, and marks a restart pending. Nothing is written
    /// unless every change is accepted.
    ///
    /// # Errors
    ///
    /// [`ServiceError::NotFound`] while the Ferrofin plugin is disabled;
    /// [`ServiceError::InvalidInput`] naming the key and the reason for an
    /// unknown key, a setting the page may not change (read-only, or set by an
    /// environment variable or a flag) and an invalid value; and
    /// [`ServiceError::Conflict`] naming the path when `config.toml` or its
    /// directory cannot be written, or the file no longer loads.
    async fn update(
        &self,
        update: ServerSettingsUpdateDto,
    ) -> Result<ServerSettingsUpdateResultDto, ServiceError>;
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

    async fn update(
        &self,
        _update: ServerSettingsUpdateDto,
    ) -> Result<ServerSettingsUpdateResultDto, ServiceError> {
        Err(ServiceError::not_found(
            "the Ferrofin settings are not available",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{DisabledServerSettings, ServerSettingsManager, ServerSettingsUpdateDto};
    use crate::error::ServiceError;

    #[tokio::test]
    async fn the_disabled_manager_answers_not_found() {
        let result = DisabledServerSettings.snapshot().await;
        assert!(
            matches!(result, Err(ServiceError::NotFound(_))),
            "{result:?}"
        );
        let result = DisabledServerSettings
            .update(ServerSettingsUpdateDto::default())
            .await;
        assert!(
            matches!(result, Err(ServiceError::NotFound(_))),
            "{result:?}"
        );
    }
}
