//! Configuration-layer traits — server configuration and display preferences.
//!
//! Port of `MediaBrowser.Controller.Configuration.IServerConfigurationManager`
//! ([`ServerConfigurationManager`]) and
//! `MediaBrowser.Controller.IDisplayPreferencesManager`
//! ([`DisplayPreferencesManager`]).
//!
//! Port rules applied throughout:
//! - `IServerConfigurationManager` extends `IConfigurationManager` (a generic
//!   `GetConfiguration<T>` / `SaveConfiguration` seam). That generic base is
//!   **not** object-safe, so it is collapsed to the two concrete accessors the
//!   server actually needs — the strongly-typed [`ServerConfiguration`] and the
//!   application paths — plus a persist call.
//! - `IDisplayPreferencesManager` returns EF entities; those become the
//!   `hermit-db` display-preferences rows. Identity arguments become
//!   [`uuid::Uuid`]. The C# methods are synchronous but stay `async fn ->
//!   Result` here so implementations may hit the database and surface failures
//!   uniformly.
//! - `Task`/synchronous void → `async fn -> Result<(), ServiceError>`.
//!
//! Both traits are object-safe and carry `_assert_object_safe_*` assertions.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::entities::display_preferences::{
    DisplayPreferencesEntity, ItemDisplayPreferencesEntity,
};
use hermit_model::branding::BrandingOptions;
use hermit_model::configuration::{EncodingOptions, ServerConfiguration};
use uuid::Uuid;

use crate::error::ServiceError;
use crate::system::ServerApplicationPaths;

/// Provides access to the server's configuration and application paths.
///
/// Port of `IServerConfigurationManager` with its generic `IConfigurationManager`
/// base collapsed to concrete accessors (see the module docs).
#[async_trait]
pub trait ServerConfigurationManager: Send + Sync {
    /// The resolved application paths.
    ///
    /// Returned as an `Arc<dyn>` so the paths object stays shareable and this
    /// trait remains object-safe.
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths>;

    /// The current, strongly-typed server configuration.
    async fn configuration(&self) -> Result<ServerConfiguration, ServiceError>;

    /// Persists a replacement server configuration.
    async fn update_configuration(
        &self,
        configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError>;

    /// The current branding options (C# `GetConfiguration<BrandingOptions>("branding")`).
    ///
    /// Jellyfin stores this as a named configuration in a pluggable store;
    /// Hermit persists it alongside the main configuration. A never-configured
    /// server returns [`BrandingOptions::default`].
    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError>;

    /// Persists a replacement branding configuration
    /// (C# `SaveConfiguration("branding", …)`).
    async fn update_branding(&self, branding: &BrandingOptions) -> Result<(), ServiceError>;

    /// The current ffmpeg encoding options (C# `GetEncodingOptions()`, i.e.
    /// `GetConfiguration<EncodingOptions>("encoding")`).
    ///
    /// Jellyfin stores this as a named configuration in a pluggable store;
    /// Hermit persists it alongside the main configuration. The default
    /// implementation returns [`EncodingOptions::default`] so the many manager
    /// impls that do not surface encoding config keep compiling; the concrete
    /// `HermitServerConfigurationManager` overrides it to read the persisted
    /// document. Consumed by the `FallbackFont` endpoints to resolve
    /// `FallbackFontPath`.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] if the persisted configuration cannot be read or
    /// parsed.
    async fn get_encoding_options(&self) -> Result<EncodingOptions, ServiceError> {
        Ok(EncodingOptions::default())
    }
}

fn _assert_object_safe_server_configuration_manager(_: &dyn ServerConfigurationManager) {}

/// Stores and retrieves per-user, per-client display preferences.
///
/// Port of `IDisplayPreferencesManager`. Getters create the row if absent but do
/// not auto-persist (matching the C# contract); the `update_*` methods commit.
#[async_trait]
pub trait DisplayPreferencesManager: Send + Sync {
    /// Gets the display preferences for a user/item/client (creating a default
    /// row in memory if none exists).
    async fn get_display_preferences(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        client: &str,
    ) -> Result<DisplayPreferencesEntity, ServiceError>;

    /// Gets the item display preferences for a user/item/client.
    async fn get_item_display_preferences(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        client: &str,
    ) -> Result<ItemDisplayPreferencesEntity, ServiceError>;

    /// Lists all item display preferences for a user/client.
    async fn list_item_display_preferences(
        &self,
        user_id: Uuid,
        client: &str,
    ) -> Result<Vec<ItemDisplayPreferencesEntity>, ServiceError>;

    /// Lists the custom (key/value) display preferences for a user/item/client.
    async fn list_custom_item_display_preferences(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        client: &str,
    ) -> Result<HashMap<String, Option<String>>, ServiceError>;

    /// Replaces the custom display preferences for a user/item/client.
    async fn set_custom_item_display_preferences(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        client: &str,
        custom_preferences: &HashMap<String, Option<String>>,
    ) -> Result<(), ServiceError>;

    /// Creates or updates a display-preferences row.
    async fn update_display_preferences(
        &self,
        display_preferences: &DisplayPreferencesEntity,
    ) -> Result<(), ServiceError>;

    /// Creates or updates an item display-preferences row.
    async fn update_item_display_preferences(
        &self,
        item_display_preferences: &ItemDisplayPreferencesEntity,
    ) -> Result<(), ServiceError>;
}

fn _assert_object_safe_display_preferences_manager(_: &dyn DisplayPreferencesManager) {}
