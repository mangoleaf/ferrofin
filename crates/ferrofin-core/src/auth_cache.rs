//! [`AuthCache`] — the short-TTL token → (device, user) read-through cache.
//!
//! Every authenticated request used to cost two SQLite round-trips before its
//! handler ran (`Devices` by token, `Users` by id). Jellyfin pays neither: its
//! `DeviceManager`/`UserManager` hold all devices and users in in-memory
//! dictionaries. This cache closes that gap while staying bounded and — the
//! part that matters at a trust boundary — **revocation-correct**: the
//! composition root shares ONE instance between the authorization context
//! (read-through) and the user/device managers, which [`AuthCache::clear`] it
//! on every auth-relevant mutation (device delete/rename/options, user
//! update/delete, password/policy/configuration change). Clearing wholesale
//! instead of per-entry keeps the invalidation story impossible to get wrong;
//! the price is one 2-query re-read per live token afterwards.
//!
//! The TTL bounds staleness for anything that *isn't* hooked (e.g. a row
//! edited by hand in the DB); it is deliberately short. A revoked token never
//! waits for the TTL — the delete path clears synchronously before it returns.
//!
//! The cache also holds the assembled [`UserDto`] per user (the other half of
//! Jellyfin's in-memory `UserManager`): building it costs two more round-trips
//! (permissions/preferences union + access schedules) on every `/Users*`
//! request, which convoy on the read pool under load. A hit is validated
//! against the caller's [`UserEntity`] (`PartialEq`), so a DTO can never be
//! served against a fresher user row than the one it was built from; the
//! perms/prefs/schedule inputs are covered by the same [`AuthCache::clear`]
//! contract (every mutating path clears) plus the TTL.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use ferrofin_db::entities::security::DeviceEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::UserDto;

/// How long a cached token resolution may be served before the next request
/// re-reads it from the database.
///
/// 30 s: long enough that a busy client's request bursts (a home-screen load
/// is dozens of requests in a few seconds) hit the cache, short enough that
/// un-hooked out-of-band changes converge quickly. FLAGGED as a candidate
/// setting — surface as a config knob if an operator ever needs to tune it.
pub const AUTH_CACHE_TTL: Duration = Duration::from_secs(30);

/// Hard entry cap — a runaway bound, not a working-set tuner. Tokens are one
/// per device row, so a real server sits orders of magnitude below this; if
/// something floods distinct tokens the whole map is dropped rather than
/// growing without bound.
const AUTH_CACHE_MAX_ENTRIES: usize = 4096;

/// One cached token resolution.
///
/// The device row is held behind an [`Arc`] because a hit only ever *reads* it
/// (to fill the auth info's blank client/device fields), so handing out a
/// refcount bump instead of a deep copy saves one allocation per `String`
/// column on every authenticated request.
#[derive(Debug, Clone)]
struct CachedAuth {
    device: Arc<DeviceEntity>,
    user: Option<UserEntity>,
    cached_at: Instant,
}

/// One cached user-DTO assembly, keyed by user id and validated against the
/// exact [`UserEntity`] it was built from.
#[derive(Debug, Clone)]
struct CachedUserDto {
    entity: UserEntity,
    dto: UserDto,
    cached_at: Instant,
}

/// The shared token-resolution cache. See the module docs for the sharing and
/// invalidation contract.
#[derive(Debug)]
pub struct AuthCache {
    ttl: Duration,
    entries: RwLock<HashMap<String, CachedAuth>>,
    user_dtos: RwLock<HashMap<String, CachedUserDto>>,
}

impl Default for AuthCache {
    fn default() -> Self {
        Self::new(AUTH_CACHE_TTL)
    }
}

impl AuthCache {
    /// Creates an empty cache with the given TTL.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: RwLock::new(HashMap::new()),
            user_dtos: RwLock::new(HashMap::new()),
        }
    }

    /// The cached resolution for `token`, if present and fresher than the TTL.
    ///
    /// The device row comes back as a shared [`Arc`] (callers only read it), so
    /// a hit costs one refcount bump rather than a deep copy of every column.
    /// The user entity is still cloned because [`AuthorizationInfo`] owns it
    /// outright.
    ///
    /// [`AuthorizationInfo`]: ferrofin_traits::options::AuthorizationInfo
    #[must_use]
    pub fn get(&self, token: &str) -> Option<(Arc<DeviceEntity>, Option<UserEntity>)> {
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        let hit = entries.get(token)?;
        if hit.cached_at.elapsed() > self.ttl {
            return None;
        }
        Some((Arc::clone(&hit.device), hit.user.clone()))
    }

    /// Caches a successful token resolution.
    pub fn put(&self, token: &str, device: DeviceEntity, user: Option<UserEntity>) {
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= AUTH_CACHE_MAX_ENTRIES {
            entries.clear();
        }
        entries.insert(
            token.to_owned(),
            CachedAuth {
                device: Arc::new(device),
                user,
                cached_at: Instant::now(),
            },
        );
    }

    /// The cached [`UserDto`] for `entity`'s user, if present, fresher than the
    /// TTL, and built from an identical entity (a row that changed since —
    /// login stamp, rename, config column — misses and is rebuilt).
    #[must_use]
    pub fn get_user_dto(&self, entity: &UserEntity) -> Option<UserDto> {
        let dtos = self
            .user_dtos
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let hit = dtos.get(&entity.id)?;
        if hit.cached_at.elapsed() > self.ttl || hit.entity != *entity {
            return None;
        }
        Some(hit.dto.clone())
    }

    /// Caches a user's assembled DTO alongside the entity it was built from.
    pub fn put_user_dto(&self, entity: &UserEntity, dto: UserDto) {
        let mut dtos = self
            .user_dtos
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if dtos.len() >= AUTH_CACHE_MAX_ENTRIES {
            dtos.clear();
        }
        dtos.insert(
            entity.id.clone(),
            CachedUserDto {
                entity: entity.clone(),
                dto,
                cached_at: Instant::now(),
            },
        );
    }

    /// Drops every cached resolution. Called by the user/device managers on any
    /// auth-relevant mutation — revocation must be immediate, never TTL-bounded.
    pub fn clear(&self) {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.user_dtos
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// The number of live entries (TTL-expired entries included until evicted).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether the cache holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(token: &str) -> DeviceEntity {
        DeviceEntity {
            id: 1,
            access_token: token.to_owned(),
            app_name: "app".into(),
            app_version: "1.0".into(),
            date_created: chrono::Utc::now(),
            date_last_activity: chrono::Utc::now(),
            date_modified: chrono::Utc::now(),
            device_id: "dev".into(),
            device_name: "Device".into(),
            is_active: true,
            user_id: "u-1".into(),
        }
    }

    #[test]
    fn hit_within_ttl_miss_after_expiry() {
        let cache = AuthCache::new(Duration::from_millis(30));
        cache.put("tok", device("tok"), None);
        assert!(cache.get("tok").is_some(), "fresh entry hits");
        std::thread::sleep(Duration::from_millis(40));
        assert!(cache.get("tok").is_none(), "expired entry misses");
    }

    /// Finding #3: a hit must hand out a shared handle to the device row, not a
    /// deep copy of it. Pointer identity across two hits is the proof — a
    /// per-hit clone would give two distinct allocations.
    #[test]
    fn hits_share_the_device_row_instead_of_deep_copying_it() {
        let cache = AuthCache::default();
        cache.put("tok", device("tok"), None);

        let (first, _) = cache.get("tok").expect("first hit");
        let (second, _) = cache.get("tok").expect("second hit");
        assert!(
            Arc::ptr_eq(&first, &second),
            "both hits point at the one cached device row"
        );
        // And the string columns are the same allocation, not copies.
        assert_eq!(
            first.device_name.as_ptr(),
            second.device_name.as_ptr(),
            "no per-hit String reallocation"
        );
        assert_eq!(first.access_token, "tok", "the shared row is the right one");
    }

    #[test]
    fn clear_revokes_immediately() {
        let cache = AuthCache::default();
        cache.put("tok", device("tok"), None);
        assert!(cache.get("tok").is_some());
        cache.clear();
        assert!(cache.get("tok").is_none(), "cleared entry never served");
        assert!(cache.is_empty());
    }

    #[test]
    fn cap_drops_the_map_instead_of_growing() {
        let cache = AuthCache::default();
        for i in 0..AUTH_CACHE_MAX_ENTRIES {
            cache.put(&format!("t{i}"), device("t"), None);
        }
        assert_eq!(cache.len(), AUTH_CACHE_MAX_ENTRIES);
        cache.put("one-more", device("t"), None);
        assert_eq!(
            cache.len(),
            1,
            "cap sweep dropped the map, kept the newcomer"
        );
    }
}
