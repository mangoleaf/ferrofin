//! [`FerrofinDeviceManager`] — the concrete [`DeviceManager`] over `ferrofin-db`.
//!
//! Port of `Jellyfin.Server.Implementations.Devices.DeviceManager`. Devices and
//! their per-device options persist in the `Devices` / `DeviceOptions` tables;
//! reported [`ClientCapabilities`] are transient (the C# class keeps them in a
//! `ConcurrentDictionary`, never persisted), so they live in an in-memory map
//! here too.
//!
//! Differences from upstream, all faithful simplifications:
//! - The C# constructor eagerly loads every device into concurrent maps; this
//!   port queries the tables on demand instead (no cache-coherency window).
//! - The `DeviceOptionsUpdated` event is dropped (event wiring is out of scope).
//! - `GetDeviceOptions` returns the [`DeviceOptionsEntity`] row (the trait's
//!   documented stopgap pending a ported `DeviceOptionsDto`).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::security::{DeviceEntity, DeviceOptionsEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::{PermissionKind, PreferenceKind};
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::devices::DeviceInfo;
use ferrofin_model::dto::{ClientCapabilitiesDto, DeviceInfoDto};
use ferrofin_model::querying::QueryResult;
use ferrofin_model::session::ClientCapabilities;
use uuid::Uuid;

use ferrofin_traits::devices::{DeviceManager, DeviceQuery};
use ferrofin_traits::error::ServiceError;

use crate::db_error::db_err;
use crate::user_entity_ext::{has_permission, preference_contains};

/// The concrete device manager.
#[derive(Clone)]
pub struct FerrofinDeviceManager {
    db: Database,
    /// Transient per-device reported capabilities (device id → capabilities),
    /// mirroring the C# `_capabilitiesMap` `ConcurrentDictionary`.
    capabilities: Arc<RwLock<HashMap<String, ClientCapabilities>>>,
    /// The shared token-resolution cache — cleared on every device mutation so
    /// a deleted/rewritten token can never be served from cache (revocation is
    /// immediate, not TTL-bounded).
    auth_cache: Arc<crate::auth_cache::AuthCache>,
}

impl std::fmt::Debug for FerrofinDeviceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinDeviceManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinDeviceManager {
    /// Creates a device manager over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self {
            db,
            capabilities: Arc::new(RwLock::new(HashMap::new())),
            auth_cache: Arc::new(crate::auth_cache::AuthCache::default()),
        }
    }

    /// Installs the shared [`crate::auth_cache::AuthCache`] (composition root
    /// only) — must be the instance the authorization context reads through.
    #[must_use]
    pub fn with_auth_cache(mut self, auth_cache: Arc<crate::auth_cache::AuthCache>) -> Self {
        self.auth_cache = auth_cache;
        self
    }

    /// Reads the capabilities snapshot for a device id (defaults when unknown).
    fn capabilities_for(&self, device_id: &str) -> ClientCapabilities {
        self.capabilities
            .read()
            .expect("capabilities lock poisoned")
            .get(device_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Looks up a device by its (unique) session access token — the lookup
    /// `AuthorizationContext` performs. Beyond the trait surface because the
    /// trait's [`DeviceQuery`] has no token filter.
    ///
    /// # Errors
    /// Returns [`ServiceError::Db`] if the query fails.
    pub async fn get_device_by_access_token(
        &self,
        token: &str,
    ) -> Result<Option<DeviceEntity>, ServiceError> {
        sqlx::query_as::<_, DeviceEntity>(
            r#"SELECT * FROM "Devices" WHERE "AccessToken" = ?1 ORDER BY "Id" LIMIT 1"#,
        )
        .bind(token)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)
    }

    /// Reads a device options row by device id.
    async fn options_row(
        &self,
        device_id: &str,
    ) -> Result<Option<DeviceOptionsEntity>, ServiceError> {
        sqlx::query_as::<_, DeviceOptionsEntity>(
            r#"SELECT * FROM "DeviceOptions" WHERE "DeviceId" = ?1 LIMIT 1"#,
        )
        .bind(device_id)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)
    }

    /// Assembles a [`DeviceInfo`] from a device row, resolving the owning user's
    /// name and merging capabilities + custom name (C# `ToDeviceInfo`).
    async fn to_device_info(
        &self,
        device: &DeviceEntity,
        options: Option<&DeviceOptionsEntity>,
    ) -> Result<DeviceInfo, ServiceError> {
        let caps = self.capabilities_for(&device.device_id);
        let last_user_name: Option<String> =
            sqlx::query_scalar(r#"SELECT "Username" FROM "Users" WHERE "Id" = ?1 LIMIT 1"#)
                .bind(&device.user_id)
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        let last_user_name = last_user_name.ok_or_else(|| {
            ServiceError::not_found(format!("User with UserId {} not found", device.user_id))
        })?;

        Ok(DeviceInfo {
            name: Some(device.device_name.clone()),
            custom_name: options.and_then(|o| o.custom_name.clone()),
            access_token: Some(device.access_token.clone().into()),
            id: Some(device.device_id.clone()),
            last_user_name: Some(last_user_name),
            app_name: Some(device.app_name.clone()),
            app_version: Some(device.app_version.clone()),
            last_user_id: Uuid::parse_str(&device.user_id).ok(),
            date_last_activity: Some(device.date_last_activity),
            capabilities: caps.clone(),
            icon_url: caps.icon_url,
        })
    }

    /// Maps a [`DeviceInfo`] to its wire DTO (C# `ToDeviceInfoDto`).
    fn to_device_info_dto(info: DeviceInfo) -> DeviceInfoDto {
        DeviceInfoDto {
            name: info.name,
            custom_name: info.custom_name,
            access_token: info.access_token,
            id: info.id,
            last_user_name: info.last_user_name,
            app_name: info.app_name,
            app_version: info.app_version,
            last_user_id: info.last_user_id,
            date_last_activity: info.date_last_activity,
            capabilities: client_capabilities_to_dto(&info.capabilities),
            icon_url: info.icon_url,
        }
    }

    /// Runs a device query, returning the filtered/paginated rows plus the total
    /// count (C# `GetDevices`). Ordered by surrogate `Id` for a stable page.
    async fn query_devices(
        &self,
        query: &DeviceQuery,
    ) -> Result<QueryResult<DeviceEntity>, ServiceError> {
        let mut all: Vec<DeviceEntity> =
            sqlx::query_as::<_, DeviceEntity>(r#"SELECT * FROM "Devices" ORDER BY "Id""#)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;

        if let Some(user_id) = query.user_id {
            // Compare in the canonical storage form so stored (uppercase) GUIDs
            // match regardless of the caller's formatting.
            let uid = guid_to_db(user_id);
            all.retain(|d| d.user_id.eq_ignore_ascii_case(&uid));
        }
        if let Some(device_id) = &query.device_id {
            all.retain(|d| &d.device_id == device_id);
        }

        let count = i32::try_from(all.len()).unwrap_or(i32::MAX);

        let start = usize::try_from(query.start_index.unwrap_or(0).max(0)).unwrap_or(0);
        let mut page: Vec<DeviceEntity> = all.into_iter().skip(start).collect();
        if let Some(limit) = query.limit.filter(|l| *l > 0) {
            page.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }

        Ok(QueryResult::new(query.start_index, Some(count), page))
    }
}

/// Maps recorded [`ClientCapabilities`] to their wire DTO (C#
/// `ToClientCapabilitiesDto`).
pub(crate) fn client_capabilities_to_dto(caps: &ClientCapabilities) -> ClientCapabilitiesDto {
    ClientCapabilitiesDto {
        playable_media_types: caps.playable_media_types.clone(),
        supported_commands: caps.supported_commands.clone(),
        supports_media_control: caps.supports_media_control,
        supports_persistent_identifier: caps.supports_persistent_identifier,
        device_profile: caps.device_profile.clone(),
        app_store_url: caps.app_store_url.clone(),
        icon_url: caps.icon_url.clone(),
    }
}

#[async_trait]
impl DeviceManager for FerrofinDeviceManager {
    async fn create_device(&self, device: &DeviceEntity) -> Result<DeviceEntity, ServiceError> {
        sqlx::query(
            r#"INSERT INTO "Devices"
               ("AccessToken", "AppName", "AppVersion", "DateCreated",
                "DateLastActivity", "DateModified", "DeviceId", "DeviceName",
                "IsActive", "UserId")
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
        )
        .bind(&device.access_token)
        .bind(&device.app_name)
        .bind(&device.app_version)
        .bind(datetime_to_db(device.date_created))
        .bind(datetime_to_db(device.date_last_activity))
        .bind(datetime_to_db(device.date_modified))
        .bind(&device.device_id)
        .bind(&device.device_name)
        .bind(device.is_active)
        .bind(&device.user_id)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;

        sqlx::query_as::<_, DeviceEntity>(
            r#"SELECT * FROM "Devices" WHERE "AccessToken" = ?1 ORDER BY "Id" DESC LIMIT 1"#,
        )
        .bind(&device.access_token)
        .fetch_one(self.db.pool())
        .await
        .map_err(db_err)
    }

    async fn save_capabilities(
        &self,
        device_id: &str,
        capabilities: &ClientCapabilities,
    ) -> Result<(), ServiceError> {
        self.capabilities
            .write()
            .expect("capabilities lock poisoned")
            .insert(device_id.to_owned(), capabilities.clone());
        Ok(())
    }

    async fn get_capabilities(
        &self,
        device_id: Option<&str>,
    ) -> Result<ClientCapabilities, ServiceError> {
        Ok(match device_id {
            Some(id) => self.capabilities_for(id),
            None => ClientCapabilities::default(),
        })
    }

    async fn get_device(&self, id: &str) -> Result<Option<DeviceInfoDto>, ServiceError> {
        // Most recently active row for this reported device id (C# orders by
        // DateLastActivity descending).
        let device = sqlx::query_as::<_, DeviceEntity>(
            r#"SELECT * FROM "Devices" WHERE "DeviceId" = ?1
               ORDER BY "DateLastActivity" DESC LIMIT 1"#,
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;

        let Some(device) = device else {
            return Ok(None);
        };
        let options = self.options_row(id).await?;
        let info = self.to_device_info(&device, options.as_ref()).await?;
        Ok(Some(Self::to_device_info_dto(info)))
    }

    async fn get_devices(
        &self,
        query: &DeviceQuery,
    ) -> Result<QueryResult<DeviceEntity>, ServiceError> {
        self.query_devices(query).await
    }

    async fn get_device_infos(
        &self,
        query: &DeviceQuery,
    ) -> Result<QueryResult<DeviceInfo>, ServiceError> {
        let devices = self.query_devices(query).await?;
        let mut infos = Vec::with_capacity(devices.items.len());
        for device in &devices.items {
            let options = self.options_row(&device.device_id).await?;
            infos.push(self.to_device_info(device, options.as_ref()).await?);
        }
        Ok(QueryResult::new(
            Some(devices.start_index),
            Some(devices.total_record_count),
            infos,
        ))
    }

    async fn get_devices_for_user(
        &self,
        user_id: Option<Uuid>,
    ) -> Result<QueryResult<DeviceInfoDto>, ServiceError> {
        // All devices, most recently active first (C# ordering).
        let devices: Vec<DeviceEntity> = sqlx::query_as::<_, DeviceEntity>(
            r#"SELECT * FROM "Devices" ORDER BY "DateLastActivity" DESC, "DeviceId""#,
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        let user = match user_id {
            Some(id) => {
                let row = sqlx::query_as::<_, UserEntity>(
                    r#"SELECT * FROM "Users" WHERE "Id" = ?1 LIMIT 1"#,
                )
                .bind(guid_to_db(id))
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
                Some(row.ok_or_else(|| ServiceError::not_found("user"))?)
            }
            None => None,
        };

        let mut dtos = Vec::new();
        for device in &devices {
            let denied = match &user {
                Some(user) => !self.can_access_device(user, &device.device_id).await?,
                None => false,
            };
            if denied {
                continue;
            }
            let options = self.options_row(&device.device_id).await?;
            let info = self.to_device_info(device, options.as_ref()).await?;
            dtos.push(Self::to_device_info_dto(info));
        }

        Ok(QueryResult::from_items(dtos))
    }

    async fn delete_device(&self, device: &DeviceEntity) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "Devices" WHERE "Id" = ?1"#)
            .bind(device.id)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        // Revocation: the deleted token must stop authenticating NOW.
        self.auth_cache.clear();
        Ok(())
    }

    async fn update_device(&self, device: &DeviceEntity) -> Result<(), ServiceError> {
        sqlx::query(
            r#"UPDATE "Devices" SET
                "AccessToken" = ?2, "AppName" = ?3, "AppVersion" = ?4,
                "DateCreated" = ?5, "DateLastActivity" = ?6, "DateModified" = ?7,
                "DeviceId" = ?8, "DeviceName" = ?9, "IsActive" = ?10, "UserId" = ?11
               WHERE "Id" = ?1"#,
        )
        .bind(device.id)
        .bind(&device.access_token)
        .bind(&device.app_name)
        .bind(&device.app_version)
        .bind(datetime_to_db(device.date_created))
        .bind(datetime_to_db(device.date_last_activity))
        .bind(datetime_to_db(device.date_modified))
        .bind(&device.device_id)
        .bind(&device.device_name)
        .bind(device.is_active)
        .bind(&device.user_id)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        // Token/user/name may have changed — drop cached resolutions.
        self.auth_cache.clear();
        Ok(())
    }

    async fn can_access_device(
        &self,
        user: &UserEntity,
        device_id: &str,
    ) -> Result<bool, ServiceError> {
        if device_id.is_empty() {
            return Err(ServiceError::invalid_input("deviceId must not be empty"));
        }

        let pool = self.db.pool();
        if has_permission(pool, &user.id, PermissionKind::EnableAllDevices).await?
            || has_permission(pool, &user.id, PermissionKind::IsAdministrator).await?
        {
            return Ok(true);
        }

        if preference_contains(pool, &user.id, PreferenceKind::EnabledDevices, device_id).await? {
            return Ok(true);
        }

        // Devices that don't advertise a persistent identifier are always usable
        // (C# `!GetCapabilities(deviceId).SupportsPersistentIdentifier`).
        Ok(!self
            .capabilities_for(device_id)
            .supports_persistent_identifier)
    }

    async fn update_device_options(
        &self,
        device_id: &str,
        device_name: Option<&str>,
    ) -> Result<(), ServiceError> {
        if self.options_row(device_id).await?.is_some() {
            sqlx::query(r#"UPDATE "DeviceOptions" SET "CustomName" = ?2 WHERE "DeviceId" = ?1"#)
                .bind(device_id)
                .bind(device_name)
                .execute(self.db.writer())
                .await
                .map_err(db_err)?;
        } else {
            sqlx::query(
                r#"INSERT INTO "DeviceOptions" ("CustomName", "DeviceId") VALUES (?2, ?1)"#,
            )
            .bind(device_id)
            .bind(device_name)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        }
        Ok(())
    }

    async fn get_device_options(
        &self,
        device_id: &str,
    ) -> Result<Option<DeviceOptionsEntity>, ServiceError> {
        self.options_row(device_id).await
    }

    async fn to_client_capabilities_dto(
        &self,
        capabilities: &ClientCapabilities,
    ) -> Result<ClientCapabilitiesDto, ServiceError> {
        Ok(client_capabilities_to_dto(capabilities))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{seed_user, test_db};
    use chrono::Utc;

    /// Builds a device row for `user` with the given reported device id/token.
    fn device_row(user: Uuid, device_id: &str, token: &str) -> DeviceEntity {
        let now = Utc::now();
        DeviceEntity {
            id: 0,
            access_token: token.to_owned(),
            app_name: "App".to_owned(),
            app_version: "1.0".to_owned(),
            date_created: now,
            date_last_activity: now,
            date_modified: now,
            device_id: device_id.to_owned(),
            device_name: "Phone".to_owned(),
            is_active: true,
            user_id: guid_to_db(user),
        }
    }

    #[tokio::test]
    async fn create_then_query_and_token_lookup() {
        let db = test_db().await;
        let uid = Uuid::from_u128(1);
        seed_user(&db, uid).await;
        let mgr = FerrofinDeviceManager::new(db);

        let created = mgr
            .create_device(&device_row(uid, "dev-1", "tok-1"))
            .await
            .expect("create");
        assert!(created.id > 0);

        let by_token = mgr
            .get_device_by_access_token("tok-1")
            .await
            .expect("token lookup")
            .expect("device found");
        assert_eq!(by_token.device_id, "dev-1");

        let page = mgr
            .get_devices(&DeviceQuery {
                user_id: Some(uid),
                ..Default::default()
            })
            .await
            .expect("query");
        assert_eq!(page.total_record_count, 1);
        assert_eq!(page.items.len(), 1);
    }

    #[tokio::test]
    async fn capabilities_round_trip_in_memory() {
        let db = test_db().await;
        let mgr = FerrofinDeviceManager::new(db);
        let caps = ClientCapabilities {
            supports_media_control: true,
            ..Default::default()
        };
        mgr.save_capabilities("dev-x", &caps).await.expect("save");
        let read = mgr.get_capabilities(Some("dev-x")).await.expect("get");
        assert!(read.supports_media_control);
        // Unknown id defaults.
        assert!(
            !mgr.get_capabilities(Some("nope"))
                .await
                .expect("get")
                .supports_media_control
        );
    }

    #[tokio::test]
    async fn device_options_upsert() {
        let db = test_db().await;
        let mgr = FerrofinDeviceManager::new(db);
        mgr.update_device_options("dev-1", Some("Living Room"))
            .await
            .expect("insert");
        let opts = mgr
            .get_device_options("dev-1")
            .await
            .expect("get")
            .expect("some");
        assert_eq!(opts.custom_name.as_deref(), Some("Living Room"));

        mgr.update_device_options("dev-1", Some("Bedroom"))
            .await
            .expect("update");
        let opts = mgr
            .get_device_options("dev-1")
            .await
            .expect("get")
            .expect("some");
        assert_eq!(opts.custom_name.as_deref(), Some("Bedroom"));
    }

    #[tokio::test]
    async fn can_access_device_honors_admin_and_all_devices() {
        let db = test_db().await;
        let uid = Uuid::from_u128(2);
        let user = seed_user(&db, uid).await;
        let mgr = FerrofinDeviceManager::new(db.clone());

        // No permissions and no recorded capabilities: capabilities default to
        // advertising a persistent identifier (matching Jellyfin's
        // `new ClientCapabilities()`), so an unregistered device is gated.
        assert!(!mgr.can_access_device(&user, "dev-1").await.expect("access"));

        // A device that explicitly does *not* advertise a persistent identifier
        // is freely accessible.
        let transient = ClientCapabilities {
            supports_persistent_identifier: false,
            ..Default::default()
        };
        mgr.save_capabilities("dev-1", &transient)
            .await
            .expect("save");
        assert!(mgr.can_access_device(&user, "dev-1").await.expect("access"));

        // Reset to the persistent-identifier default: gated again.
        mgr.save_capabilities("dev-1", &ClientCapabilities::default())
            .await
            .expect("save");
        assert!(!mgr.can_access_device(&user, "dev-1").await.expect("access"));

        // Granting EnableAllDevices re-opens it.
        crate::user_entity_ext::set_permission(
            db.pool(),
            &user.id,
            PermissionKind::EnableAllDevices,
            true,
        )
        .await
        .expect("grant");
        assert!(mgr.can_access_device(&user, "dev-1").await.expect("access"));
    }

    #[tokio::test]
    async fn empty_device_id_is_invalid() {
        let db = test_db().await;
        let uid = Uuid::from_u128(3);
        let user = seed_user(&db, uid).await;
        let mgr = FerrofinDeviceManager::new(db);
        assert!(matches!(
            mgr.can_access_device(&user, "").await,
            Err(ServiceError::InvalidInput(_))
        ));
    }
}
