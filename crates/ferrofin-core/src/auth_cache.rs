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
//! Clearing alone is not enough, because this is a **read-through** cache: a
//! request that missed is *already holding* a database read taken before the
//! clear, and it stores that read afterwards. [`AuthCache::clear`] therefore
//! bumps a **generation**; a writer passes the generation it read at
//! ([`AuthCache::generation`], captured before its database read) and a write
//! whose generation is stale is dropped instead of resurrecting a revoked
//! token for a whole TTL.
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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Bumped by [`AuthCache::clear`] before it empties the maps. A `put` whose
    /// caller-captured generation no longer matches is dropped — see the module
    /// docs for why a read-through cache needs this on top of the clear.
    generation: AtomicU64,
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
            generation: AtomicU64::new(0),
            entries: RwLock::new(HashMap::new()),
            user_dtos: RwLock::new(HashMap::new()),
        }
    }

    /// The cache's current generation.
    ///
    /// A read-through caller captures this **before** the database read it is
    /// about to cache, and hands it back to [`AuthCache::put`] /
    /// [`AuthCache::put_user_dto`]. Anything cleared in between makes the write
    /// a no-op, so a revocation can never be undone by a request that was
    /// already in flight when it landed.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Whether a write tagged `generation` is still current.
    ///
    /// Called with the target map's write guard held: [`AuthCache::clear`]
    /// bumps the generation *before* it takes that guard, so either this load
    /// already sees the bump (the write is dropped) or the clear is still
    /// waiting on the guard and will drop the just-written entry itself.
    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
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

    /// Caches a successful token resolution, unless the cache was cleared since
    /// `generation` was captured (see [`AuthCache::generation`]).
    pub fn put(
        &self,
        generation: u64,
        token: &str,
        device: DeviceEntity,
        user: Option<UserEntity>,
    ) {
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        if !self.is_current(generation) {
            return;
        }
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

    /// Caches a user's assembled DTO alongside the entity it was built from,
    /// unless the cache was cleared since `generation` was captured (a policy
    /// or permission change lands as a clear, and the DTO's perms/prefs inputs
    /// are not covered by the entity comparison).
    pub fn put_user_dto(&self, generation: u64, entity: &UserEntity, dto: UserDto) {
        let mut dtos = self
            .user_dtos
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if !self.is_current(generation) {
            return;
        }
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
    ///
    /// The generation bump comes FIRST and outside both locks: that is what
    /// also cancels the writes of requests that read the database before this
    /// call and have not stored their result yet.
    pub fn clear(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
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
        cache.put(cache.generation(), "tok", device("tok"), None);
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
        cache.put(cache.generation(), "tok", device("tok"), None);

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
        cache.put(cache.generation(), "tok", device("tok"), None);
        assert!(cache.get("tok").is_some());
        cache.clear();
        assert!(cache.get("tok").is_none(), "cleared entry never served");
        assert!(cache.is_empty());
    }

    /// The read-through race: a request misses, reads the (still valid) device
    /// row, and only *then* stores it — a revocation that lands in between must
    /// win. Without the generation tag the `put` re-seats the revoked token and
    /// it authenticates for a whole TTL.
    #[test]
    fn a_put_that_started_before_a_clear_is_dropped() {
        let cache = AuthCache::default();

        // What the resolver captures before its database read.
        let generation = cache.generation();
        // …the revocation lands while that read is in flight…
        cache.clear();
        // …and the in-flight resolver stores what it read.
        cache.put(generation, "tok", device("tok"), None);

        assert!(
            cache.get("tok").is_none(),
            "a revoked token must not be resurrected by an in-flight resolution"
        );
        assert!(cache.is_empty());
    }

    /// The same window on the user-DTO half: a policy/permission change clears,
    /// and an assembly that read the pre-change rows must not store them (the
    /// entity comparison cannot catch it — permissions live in other tables).
    #[tokio::test]
    async fn a_user_dto_put_that_started_before_a_clear_is_dropped() {
        let db = crate::test_support::test_db().await;
        let entity = crate::test_support::seed_user(&db, uuid::Uuid::from_u128(0x5e)).await;
        let cache = AuthCache::default();

        let generation = cache.generation();
        cache.clear();
        cache.put_user_dto(generation, &entity, UserDto::default());

        assert!(
            cache.get_user_dto(&entity).is_none(),
            "a stale policy assembly must not survive the clear that raced it"
        );
    }

    /// A write that started *after* the clear is current and must be kept —
    /// the guard above must not degrade the cache into a no-op.
    #[test]
    fn a_put_after_a_clear_is_kept() {
        let cache = AuthCache::default();
        cache.clear();
        cache.put(cache.generation(), "tok", device("tok"), None);
        assert!(
            cache.get("tok").is_some(),
            "a fresh resolution still caches"
        );
    }

    #[test]
    fn cap_drops_the_map_instead_of_growing() {
        let cache = AuthCache::default();
        for i in 0..AUTH_CACHE_MAX_ENTRIES {
            cache.put(cache.generation(), &format!("t{i}"), device("t"), None);
        }
        assert_eq!(cache.len(), AUTH_CACHE_MAX_ENTRIES);
        cache.put(cache.generation(), "one-more", device("t"), None);
        assert_eq!(
            cache.len(),
            1,
            "cap sweep dropped the map, kept the newcomer"
        );
    }
}
