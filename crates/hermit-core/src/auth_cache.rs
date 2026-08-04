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

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};
use std::time::{Duration, Instant};

use hermit_db::entities::security::DeviceEntity;
use hermit_db::entities::users::UserEntity;

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
#[derive(Debug, Clone)]
struct CachedAuth {
    device: DeviceEntity,
    user: Option<UserEntity>,
    cached_at: Instant,
}

/// The shared token-resolution cache. See the module docs for the sharing and
/// invalidation contract.
#[derive(Debug)]
pub struct AuthCache {
    ttl: Duration,
    entries: RwLock<HashMap<String, CachedAuth>>,
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
        }
    }

    /// The cached resolution for `token`, if present and fresher than the TTL.
    #[must_use]
    pub fn get(&self, token: &str) -> Option<(DeviceEntity, Option<UserEntity>)> {
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        let hit = entries.get(token)?;
        if hit.cached_at.elapsed() > self.ttl {
            return None;
        }
        Some((hit.device.clone(), hit.user.clone()))
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
                device,
                user,
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
