//! Device-layer trait — client device registration and capabilities.
//!
//! Port of `MediaBrowser.Controller.Devices.IDeviceManager`.
//!
//! Port rules applied throughout:
//! - Persisted devices are the [`DeviceEntity`] / [`DeviceOptionsEntity`]
//!   `hermit-db` rows; API-facing reads surface the [`DeviceInfoDto`] /
//!   [`DeviceInfo`] wire DTOs. The C# `GetDeviceOptions` returns a
//!   `DeviceOptionsDto` (not yet ported to `hermit-model`); until then it returns
//!   the [`DeviceOptionsEntity`] row (flagged in the port report).
//! - The `CanAccessDevice(User, …)` domain-`User` argument becomes a
//!   [`UserEntity`] row; identity arguments become [`uuid::Uuid`].
//! - `Jellyfin.Data.Queries.DeviceQuery` is ported as [`DeviceQuery`] (a small
//!   service param, not a wire DTO).
//! - The `DeviceOptionsUpdated` event is dropped (event wiring is `hermit-core`).
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; the synchronous C#
//!   methods stay `async fn` for a uniform, fallible surface.
//!
//! The trait is object-safe and carries a `_assert_object_safe_*` assertion.

use async_trait::async_trait;
use hermit_db::entities::security::{DeviceEntity, DeviceOptionsEntity};
use hermit_db::entities::users::UserEntity;
use hermit_model::devices::DeviceInfo;
use hermit_model::dto::{ClientCapabilitiesDto, DeviceInfoDto};
use hermit_model::querying::QueryResult;
use hermit_model::session::ClientCapabilities;
use uuid::Uuid;

use crate::error::ServiceError;

/// A query over registered devices.
///
/// Port of `Jellyfin.Data.Queries.DeviceQuery` — the filter/pagination shape
/// accepted by [`DeviceManager::get_devices`] and friends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceQuery {
    /// Restrict to a single device id, if set.
    pub device_id: Option<String>,

    /// Restrict to devices owned by this user, if set.
    pub user_id: Option<Uuid>,

    /// Zero-based index of the first result to return.
    pub start_index: Option<i32>,

    /// Maximum number of results to return.
    pub limit: Option<i32>,
}

/// Registers client devices, tracks their capabilities, and enforces access.
///
/// Port of `IDeviceManager` (the object-safe subset). Persisted state is
/// `hermit-db` rows; API reads are `hermit-model` DTOs.
#[async_trait]
pub trait DeviceManager: Send + Sync {
    /// Creates a new device, returning the persisted row.
    async fn create_device(&self, device: &DeviceEntity) -> Result<DeviceEntity, ServiceError>;

    /// Records the reported capabilities for a device.
    async fn save_capabilities(
        &self,
        device_id: &str,
        capabilities: &ClientCapabilities,
    ) -> Result<(), ServiceError>;

    /// Gets the recorded capabilities for a device (defaults when unknown).
    async fn get_capabilities(
        &self,
        device_id: Option<&str>,
    ) -> Result<ClientCapabilities, ServiceError>;

    /// Gets the device info DTO for a device id, or `None`.
    async fn get_device(&self, id: &str) -> Result<Option<DeviceInfoDto>, ServiceError>;

    /// Queries persisted device rows.
    async fn get_devices(
        &self,
        query: &DeviceQuery,
    ) -> Result<QueryResult<DeviceEntity>, ServiceError>;

    /// Queries device info (capability-enriched) records.
    async fn get_device_infos(
        &self,
        query: &DeviceQuery,
    ) -> Result<QueryResult<DeviceInfo>, ServiceError>;

    /// Gets the device info DTOs visible to a user (or all when `None`).
    async fn get_devices_for_user(
        &self,
        user_id: Option<Uuid>,
    ) -> Result<QueryResult<DeviceInfoDto>, ServiceError>;

    /// Deletes a device.
    async fn delete_device(&self, device: &DeviceEntity) -> Result<(), ServiceError>;

    /// Persists changes to a device row.
    async fn update_device(&self, device: &DeviceEntity) -> Result<(), ServiceError>;

    /// Whether a user is permitted to use a given device.
    async fn can_access_device(
        &self,
        user: &UserEntity,
        device_id: &str,
    ) -> Result<bool, ServiceError>;

    /// Updates a device's options (currently just its custom name).
    async fn update_device_options(
        &self,
        device_id: &str,
        device_name: Option<&str>,
    ) -> Result<(), ServiceError>;

    /// Gets a device's options row, or `None`.
    ///
    /// Returns the [`DeviceOptionsEntity`] row pending a ported
    /// `DeviceOptionsDto` (see the module docs).
    async fn get_device_options(
        &self,
        device_id: &str,
    ) -> Result<Option<DeviceOptionsEntity>, ServiceError>;

    /// Maps recorded capabilities to their client-facing DTO.
    async fn to_client_capabilities_dto(
        &self,
        capabilities: &ClientCapabilities,
    ) -> Result<ClientCapabilitiesDto, ServiceError>;
}

fn _assert_object_safe_device_manager(_: &dyn DeviceManager) {}

#[cfg(test)]
mod tests {
    use super::DeviceQuery;
    use uuid::Uuid;

    #[test]
    fn device_query_default_is_empty() {
        let q = DeviceQuery::default();
        assert_eq!(q.device_id, None);
        assert_eq!(q.user_id, None);
        assert_eq!(q.start_index, None);
        assert_eq!(q.limit, None);
    }

    #[test]
    fn device_query_carries_filters() {
        let id = Uuid::from_u128(7);
        let q = DeviceQuery {
            user_id: Some(id),
            limit: Some(10),
            ..Default::default()
        };
        assert_eq!(q.user_id, Some(id));
        assert_eq!(q.limit, Some(10));
    }
}
