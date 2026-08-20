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
use crate::device_repository::DeviceRepository;
use crate::user_entity_ext::{get_preference, has_permission};

/// The concrete device manager.
#[derive(Clone)]
pub struct FerrofinDeviceManager {
    db: Database,
    /// The batched reads behind the device listing (see
    /// [`crate::device_repository`]).
    repo: DeviceRepository,
    /// Transient per-device reported capabilities (device id → capabilities),
    /// mirroring the C# `_capabilitiesMap` `ConcurrentDictionary`.
    ///
    /// Entries are **never removed**, including by `delete_device` —
    /// deliberately, matching upstream (C# `DeleteDevice` drops the device from
    /// `_devices`, never from `_capabilitiesMap`), because two behaviours still
    /// read this map after the `Devices` row is gone:
    /// - [`Self::access_allows`] lets a restricted user reach a device that
    ///   reported `SupportsPersistentIdentifier: false`. Forgetting the entry
    ///   falls back to the default (`true`, as C# `new ClientCapabilities()`)
    ///   and turns that allow into a deny — deleting one device row would
    ///   silently lock a user out of a device they could use before.
    /// - a session created for that device id inherits these capabilities (C#
    ///   `OnSessionStarted`), which is what keeps a client remote-controllable
    ///   across a session that ended, before it re-posts
    ///   `/Sessions/Capabilities/Full`.
    ///
    /// Growth is bounded by the number of device ids that ever authenticated —
    /// the same population as the persisted `Devices` table, at a few hundred
    /// bytes each — so this is not a leak that outruns the database.
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
            repo: DeviceRepository::new(db.clone()),
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
        let names = self.repo.usernames_for(&[device.user_id.as_str()]).await?;
        self.device_info_with_name(device, options, names.get(&device.user_id).cloned())
    }

    /// The pure half of [`Self::to_device_info`]: merges an already-resolved
    /// owner name with the device row, its options, and the reported
    /// capabilities. A `None` name is the missing-`Users`-row case and is the
    /// documented `404` (C# `ToDeviceInfo` dereferences the user).
    fn device_info_with_name(
        &self,
        device: &DeviceEntity,
        options: Option<&DeviceOptionsEntity>,
        last_user_name: Option<String>,
    ) -> Result<DeviceInfo, ServiceError> {
        let caps = self.capabilities_for(&device.device_id);
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

    /// Resolves the **user-scoped** half of
    /// [`DeviceManager::can_access_device`] once, so a listing pays for it once
    /// instead of once per device. Neither permission nor the `EnabledDevices`
    /// list depends on the device being tested.
    async fn resolve_access(
        &self,
        user: Option<&UserEntity>,
    ) -> Result<DeviceAccess, ServiceError> {
        let Some(user) = user else {
            return Ok(DeviceAccess::Unfiltered);
        };
        let pool = self.db.pool();
        if has_permission(pool, &user.id, PermissionKind::EnableAllDevices).await?
            || has_permission(pool, &user.id, PermissionKind::IsAdministrator).await?
        {
            return Ok(DeviceAccess::All);
        }
        Ok(DeviceAccess::Enabled(
            get_preference(pool, &user.id, PreferenceKind::EnabledDevices).await?,
        ))
    }

    /// Applies a resolved [`DeviceAccess`] to one device id — the per-device
    /// half of `can_access_device`, with every query hoisted out of it.
    ///
    /// The membership test matches `preference_contains` (ASCII
    /// case-insensitive), and the empty-id rejection stays per device so a
    /// listing still fails the way the per-device call failed.
    fn access_allows(&self, access: &DeviceAccess, device_id: &str) -> Result<bool, ServiceError> {
        if matches!(access, DeviceAccess::Unfiltered) {
            return Ok(true);
        }
        if device_id.is_empty() {
            return Err(ServiceError::invalid_input("deviceId must not be empty"));
        }
        match access {
            DeviceAccess::Unfiltered | DeviceAccess::All => Ok(true),
            DeviceAccess::Enabled(enabled) => Ok(enabled
                .iter()
                .any(|v| v.eq_ignore_ascii_case(device_id))
                // Devices that don't advertise a persistent identifier are
                // always usable (C# `!GetCapabilities(id).SupportsPersistentIdentifier`).
                || !self.capabilities_for(device_id).supports_persistent_identifier),
        }
    }

    /// Batch-resolves the `DeviceOptions` row and owner name for `devices`, then
    /// assembles them in order. Two queries total, whatever the device count.
    async fn device_infos_for(
        &self,
        devices: &[&DeviceEntity],
    ) -> Result<Vec<DeviceInfo>, ServiceError> {
        let device_ids: Vec<&str> = devices.iter().map(|d| d.device_id.as_str()).collect();
        let options = self.repo.options_for(&device_ids).await?;

        let mut user_ids: Vec<&str> = devices.iter().map(|d| d.user_id.as_str()).collect();
        user_ids.sort_unstable();
        user_ids.dedup();
        let names = self.repo.usernames_for(&user_ids).await?;

        let mut infos = Vec::with_capacity(devices.len());
        for device in devices {
            infos.push(self.device_info_with_name(
                device,
                options.get(&device.device_id),
                names.get(&device.user_id).cloned(),
            )?);
        }
        Ok(infos)
    }
}

/// The user-scoped half of the per-device access check, resolved once per
/// listing (see [`FerrofinDeviceManager::resolve_access`]).
enum DeviceAccess {
    /// No user was named, so no check runs at all — every device is visible and
    /// even an empty device id is not rejected.
    Unfiltered,
    /// `EnableAllDevices` or `IsAdministrator`: every device is visible.
    All,
    /// Neither permission — the user's `EnabledDevices` list, tested in memory.
    Enabled(Vec<String>),
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
        let rows: Vec<&DeviceEntity> = devices.items.iter().collect();
        let infos = self.device_infos_for(&rows).await?;
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
        let devices = self.repo.all_by_last_activity().await?;

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

        // `can_access_device` is user-scoped apart from the device id, so
        // resolve it once and test each device in memory. The scan stops at the
        // first device the check rejects outright (an empty id), keeping the
        // original per-device ordering of errors: any missing-user `404` among
        // the devices *before* it still surfaces first.
        let access = self.resolve_access(user.as_ref()).await?;
        let mut visible: Vec<&DeviceEntity> = Vec::new();
        let mut rejected = None;
        for device in &devices {
            match self.access_allows(&access, &device.device_id) {
                Ok(true) => visible.push(device),
                Ok(false) => {}
                Err(err) => {
                    rejected = Some(err);
                    break;
                }
            }
        }

        let infos = self.device_infos_for(&visible).await?;
        if let Some(err) = rejected {
            return Err(err);
        }

        Ok(QueryResult::from_items(
            infos.into_iter().map(Self::to_device_info_dto).collect(),
        ))
    }

    async fn delete_device(&self, device: &DeviceEntity) -> Result<(), ServiceError> {
        // The `capabilities` entry for `device.device_id` deliberately survives
        // (C# `DeleteDevice` drops only `_devices`) — see the field's docs: it
        // still gates `can_access_device` for restricted users.
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

        // One device, so the "resolve once" split costs the same queries it
        // always did — but the rule lives in exactly one place.
        let access = self.resolve_access(Some(user)).await?;
        self.access_allows(&access, device_id)
    }

    async fn update_device_options(
        &self,
        device_id: &str,
        device_name: Option<&str>,
    ) -> Result<(), ServiceError> {
        // ONE statement: `IX_DeviceOptions_DeviceId` is unique, so a
        // read-then-branch let two concurrent renames of a device that has no
        // options row yet both take the insert leg, and the loser failed the
        // index (a 500 on `POST /Devices/Options`).
        sqlx::query(
            r#"INSERT INTO "DeviceOptions" ("CustomName", "DeviceId") VALUES (?2, ?1)
               ON CONFLICT("DeviceId") DO UPDATE SET "CustomName" = excluded."CustomName""#,
        )
        .bind(device_id)
        .bind(device_name)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
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
    use crate::test_support::{seed_named_user, seed_user, test_db};
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

    /// Two renames of a device with no options row yet must both succeed:
    /// read-then-branch let both take the insert leg and the loser failed the
    /// unique `IX_DeviceOptions_DeviceId`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_option_writes_do_not_collide() {
        let db = test_db().await;
        let mgr = Arc::new(FerrofinDeviceManager::new(db));

        let mut tasks = Vec::new();
        for i in 0..8 {
            let mgr = Arc::clone(&mgr);
            tasks.push(tokio::spawn(async move {
                mgr.update_device_options("dev-1", Some(&format!("Room {i}")))
                    .await
            }));
        }
        for task in tasks {
            task.await
                .expect("join")
                .expect("a concurrent first option write must not fail");
        }
        assert!(
            mgr.get_device_options("dev-1")
                .await
                .expect("get")
                .is_some(),
            "exactly one options row, whichever writer landed last"
        );
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

    /// Registers a device for `user` with an explicit last-activity stamp, so a
    /// test can pin the listing order.
    async fn device_at(
        mgr: &FerrofinDeviceManager,
        user: Uuid,
        device_id: &str,
        activity: chrono::DateTime<Utc>,
    ) {
        let mut row = device_row(user, device_id, &format!("tok-{device_id}"));
        row.date_last_activity = activity;
        mgr.create_device(&row).await.expect("create");
    }

    /// The device ids of a listing, in the order it returned them.
    fn ids(result: &QueryResult<DeviceInfoDto>) -> Vec<String> {
        result
            .items
            .iter()
            .map(|d| d.id.clone().unwrap_or_default())
            .collect()
    }

    /// The access check is now resolved once per listing instead of once per
    /// device; this pins every branch of it as `GET /Devices` observes it —
    /// the `EnabledDevices` membership test, the no-persistent-identifier
    /// fallback, and the `EnableAllDevices` override — plus the
    /// `DateLastActivity DESC` ordering the hoist must not disturb.
    #[tokio::test]
    async fn devices_for_user_filters_and_orders_like_the_per_device_check() {
        let db = test_db().await;
        let uid = Uuid::from_u128(10);
        let user = seed_user(&db, uid).await;
        let mgr = FerrofinDeviceManager::new(db.clone());

        let now = Utc::now();
        device_at(&mgr, uid, "dev-new", now).await;
        device_at(&mgr, uid, "dev-mid", now - chrono::Duration::hours(1)).await;
        device_at(&mgr, uid, "dev-old", now - chrono::Duration::hours(2)).await;

        // Every device advertises a persistent identifier by default, so with
        // no permissions and an empty enabled list nothing is visible.
        assert!(
            ids(&mgr.get_devices_for_user(Some(uid)).await.expect("list")).is_empty(),
            "default capabilities gate every device"
        );

        // Only the explicitly enabled device becomes visible.
        crate::user_entity_ext::set_preference(
            db.pool(),
            &user.id,
            PreferenceKind::EnabledDevices,
            &["dev-mid".to_owned()],
        )
        .await
        .expect("enable");
        assert_eq!(
            ids(&mgr.get_devices_for_user(Some(uid)).await.expect("list")),
            vec!["dev-mid"]
        );

        // A device that does not advertise a persistent identifier is visible
        // regardless, and sorts ahead of the enabled one by activity.
        mgr.save_capabilities(
            "dev-new",
            &ClientCapabilities {
                supports_persistent_identifier: false,
                ..Default::default()
            },
        )
        .await
        .expect("save");
        assert_eq!(
            ids(&mgr.get_devices_for_user(Some(uid)).await.expect("list")),
            vec!["dev-new", "dev-mid"]
        );

        // `EnableAllDevices` opens all three, still newest-activity first.
        crate::user_entity_ext::set_permission(
            db.pool(),
            &user.id,
            PermissionKind::EnableAllDevices,
            true,
        )
        .await
        .expect("grant");
        assert_eq!(
            ids(&mgr.get_devices_for_user(Some(uid)).await.expect("list")),
            vec!["dev-new", "dev-mid", "dev-old"]
        );
    }

    /// `IsAdministrator` is the second permission the hoisted check consults;
    /// without it the admin-only branch would silently stop opening devices.
    #[tokio::test]
    async fn administrator_sees_every_device() {
        let db = test_db().await;
        let uid = Uuid::from_u128(11);
        let user = seed_user(&db, uid).await;
        let mgr = FerrofinDeviceManager::new(db.clone());

        let now = Utc::now();
        device_at(&mgr, uid, "dev-a", now).await;
        device_at(&mgr, uid, "dev-b", now - chrono::Duration::hours(1)).await;

        assert!(ids(&mgr.get_devices_for_user(Some(uid)).await.expect("list")).is_empty());

        crate::user_entity_ext::set_permission(
            db.pool(),
            &user.id,
            PermissionKind::IsAdministrator,
            true,
        )
        .await
        .expect("grant");
        assert_eq!(
            ids(&mgr.get_devices_for_user(Some(uid)).await.expect("list")),
            vec!["dev-a", "dev-b"]
        );
    }

    /// Equal activity stamps fall back to `DeviceId`, and no user id means no
    /// access check at all — both observable in the listing.
    #[tokio::test]
    async fn equal_activity_tiebreaks_by_device_id_and_no_user_skips_the_check() {
        let db = test_db().await;
        let uid = Uuid::from_u128(12);
        seed_user(&db, uid).await;
        let mgr = FerrofinDeviceManager::new(db);

        let now = Utc::now();
        device_at(&mgr, uid, "dev-b", now).await;
        device_at(&mgr, uid, "dev-a", now).await;

        assert_eq!(
            ids(&mgr.get_devices_for_user(None).await.expect("list")),
            vec!["dev-a", "dev-b"]
        );
    }

    /// The per-device `DeviceOptions` and `Users` reads became one batched read
    /// each; this pins that every device still gets *its own* custom name and
    /// *its own* owner name (a mis-keyed batch map would cross them over).
    #[tokio::test]
    async fn batched_options_and_owner_names_stay_paired_to_their_device() {
        let db = test_db().await;
        let owner_a = Uuid::from_u128(13);
        let owner_b = Uuid::from_u128(14);
        let user_a = seed_named_user(&db, owner_a, "ay").await;
        seed_named_user(&db, owner_b, "bee").await;

        let mgr = FerrofinDeviceManager::new(db.clone());
        let now = Utc::now();
        device_at(&mgr, owner_a, "dev-a", now).await;
        device_at(&mgr, owner_b, "dev-b", now - chrono::Duration::hours(1)).await;
        mgr.update_device_options("dev-b", Some("Living Room"))
            .await
            .expect("options");

        crate::user_entity_ext::set_permission(
            db.pool(),
            &user_a.id,
            PermissionKind::IsAdministrator,
            true,
        )
        .await
        .expect("grant");

        let listed = mgr.get_devices_for_user(Some(owner_a)).await.expect("list");
        assert_eq!(ids(&listed), vec!["dev-a", "dev-b"]);
        assert_eq!(listed.items[0].last_user_name.as_deref(), Some("ay"));
        assert_eq!(listed.items[0].custom_name, None);
        assert_eq!(listed.items[1].last_user_name.as_deref(), Some("bee"));
        assert_eq!(listed.items[1].custom_name.as_deref(), Some("Living Room"));

        // The un-filtered listing path (`GET /Devices/Info` bulk form) batches
        // the same two reads and must agree.
        let infos = mgr
            .get_device_infos(&DeviceQuery::default())
            .await
            .expect("infos");
        let by_id: HashMap<_, _> = infos
            .items
            .iter()
            .map(|i| (i.id.clone().unwrap_or_default(), i))
            .collect();
        assert_eq!(by_id["dev-a"].last_user_name.as_deref(), Some("ay"));
        assert_eq!(by_id["dev-b"].last_user_name.as_deref(), Some("bee"));
        assert_eq!(by_id["dev-b"].custom_name.as_deref(), Some("Living Room"));
    }

    /// A device whose owning `Users` row is missing is a `404`, not a silently
    /// dropped row — the contract the batched name lookup had to preserve.
    #[tokio::test]
    async fn missing_owner_row_is_still_not_found() {
        let db = test_db().await;
        let uid = Uuid::from_u128(15);
        seed_user(&db, uid).await;
        let mgr = FerrofinDeviceManager::new(db);
        let row = device_row(uid, "dev-a", "tok-a");

        assert!(matches!(
            mgr.device_info_with_name(&row, None, None),
            Err(ServiceError::NotFound(_))
        ));
        assert!(
            mgr.device_info_with_name(&row, None, Some("u".to_owned()))
                .is_ok()
        );
    }

    /// Deleting a device must NOT forget its reported capabilities. They are not
    /// bookkeeping: they decide whether a restricted user may use that device id
    /// (a device that reports no persistent identifier is freely usable, while
    /// the *default* capabilities gate it), and they seed the capabilities of
    /// any session later created for the id. Evicting the entry with the row
    /// would flip a working device into "access denied" for exactly the users an
    /// admin never touched — upstream keeps it for the same reason.
    #[tokio::test]
    async fn deleting_a_device_keeps_its_capabilities() {
        let db = test_db().await;
        let uid = Uuid::from_u128(7);
        let user = seed_user(&db, uid).await;
        let mgr = FerrofinDeviceManager::new(db);

        // A device with no persistent identifier: usable by a user with no
        // device permissions at all.
        mgr.save_capabilities(
            "dev-transient",
            &ClientCapabilities {
                supports_persistent_identifier: false,
                ..Default::default()
            },
        )
        .await
        .expect("save");
        let device = mgr
            .create_device(&device_row(uid, "dev-transient", "tok-7"))
            .await
            .expect("create");
        assert!(
            mgr.can_access_device(&user, "dev-transient")
                .await
                .expect("access")
        );

        mgr.delete_device(&device).await.expect("delete");

        assert!(
            !mgr.get_capabilities(Some("dev-transient"))
                .await
                .expect("get")
                .supports_persistent_identifier,
            "the reported capabilities outlive the device row"
        );
        assert!(
            mgr.can_access_device(&user, "dev-transient")
                .await
                .expect("access"),
            "deleting the row must not turn an allowed device into a denied one"
        );
    }
}
