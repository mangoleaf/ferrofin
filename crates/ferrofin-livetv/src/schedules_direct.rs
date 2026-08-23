//! The Schedules Direct listings-provider surface Ferrofin serves.
//!
//! Port of the account-less slice of `Jellyfin.LiveTv/Listings/SchedulesDirect.cs`:
//! [`SchedulesDirect::get_available_countries`] backs
//! `GET /LiveTv/ListingProviders/SchedulesDirect/Countries`, which the dashboard's
//! SD setup page calls before any SD credentials exist. The document is passed
//! through byte-for-byte — the server never parses it — behind the same two-level
//! cache upstream keeps: a process-memory copy first, then the on-disk
//! `{cache}/sd-countries.json` while it is younger than [`COUNTRY_CACHE_DAYS`],
//! then a fresh fetch that rewrites both.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use ferrofin_traits::error::ServiceError;

use crate::error::LiveTvError;
use crate::fetch::SourceFetcher;

/// The Schedules Direct JSON API root (`SchedulesDirect.ApiUrl`,
/// `Jellyfin.LiveTv/Listings/SchedulesDirect.cs`).
pub const API_URL: &str = "https://json.schedulesdirect.org/20141201";

/// How long the on-disk country list stays valid before it is re-fetched.
///
/// Jellyfin's own constant (`SchedulesDirect.CountryCacheDays = 7`,
/// `Jellyfin.LiveTv/Listings/SchedulesDirect.cs`): the country set changes
/// rarely, so a weekly refresh keeps SD traffic negligible while never serving
/// a stale list for long.
pub const COUNTRY_CACHE_DAYS: u64 = 7;

/// The cache file name under the cache directory (`sd-countries.json` upstream).
const COUNTRIES_CACHE_FILE: &str = "sd-countries.json";

/// The account-less Schedules Direct client.
///
/// Cloning shares the in-memory country cache, so one fetch serves every clone
/// (the manager is cloned into each request's state).
#[derive(Clone)]
pub struct SchedulesDirect {
    fetcher: Arc<dyn SourceFetcher>,
    cache_dir: PathBuf,
    /// `SchedulesDirect._countriesCache`: the bytes last read or fetched.
    countries: Arc<RwLock<Option<Vec<u8>>>>,
}

impl std::fmt::Debug for SchedulesDirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulesDirect")
            .field("cache_dir", &self.cache_dir)
            .finish_non_exhaustive()
    }
}

impl SchedulesDirect {
    /// Creates the client over `fetcher`, caching on disk under `cache_dir`
    /// (the application cache path — `IApplicationPaths.CachePath` upstream).
    #[must_use]
    pub fn new(fetcher: Arc<dyn SourceFetcher>, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            fetcher,
            cache_dir: cache_dir.into(),
            countries: Arc::new(RwLock::new(None)),
        }
    }

    /// The on-disk cache file for the country list.
    #[must_use]
    pub fn countries_cache_path(&self) -> PathBuf {
        self.cache_dir.join(COUNTRIES_CACHE_FILE)
    }

    /// The SD "available countries" document as raw JSON bytes.
    ///
    /// Port of `SchedulesDirect.GetAvailableCountries`: memory → disk (within
    /// [`COUNTRY_CACHE_DAYS`]; an unreadable file is deleted and treated as
    /// absent) → `GET {API_URL}/available/countries`, with a successful fetch
    /// written to disk and kept in memory.
    ///
    /// # Errors
    ///
    /// A transport/status failure on the fetch, or a failure creating the cache
    /// directory or writing the cache file, is a backend error (HTTP `500`) —
    /// as the corresponding exceptions are upstream.
    pub async fn get_available_countries(&self) -> Result<Vec<u8>, ServiceError> {
        if let Some(cached) = self.memory_cached() {
            return Ok(cached);
        }

        let cache_path = self.countries_cache_path();
        if cache_is_fresh(&cache_path).await {
            match tokio::fs::read(&cache_path).await {
                Ok(bytes) => {
                    self.remember(bytes.clone());
                    return Ok(bytes);
                }
                Err(err) => {
                    // Corrupt or unreadable — delete and re-fetch.
                    tracing::debug!(
                        path = %cache_path.display(),
                        error = %err,
                        "unreadable Schedules Direct country cache; refetching"
                    );
                    let _ = tokio::fs::remove_file(&cache_path).await;
                }
            }
        }

        let url = format!("{API_URL}/available/countries");
        let bytes = self.fetcher.fetch_bytes(&url).await?;

        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .map_err(|e| LiveTvError::io(format!("create {}", self.cache_dir.display()), e))?;
        tokio::fs::write(&cache_path, &bytes)
            .await
            .map_err(|e| LiveTvError::io(format!("write {}", cache_path.display()), e))?;

        self.remember(bytes.clone());
        Ok(bytes)
    }

    /// The in-memory copy, when one has been read or fetched this process.
    fn memory_cached(&self) -> Option<Vec<u8>> {
        self.countries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stores `bytes` as the in-memory copy.
    fn remember(&self, bytes: Vec<u8>) {
        *self
            .countries
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(bytes);
    }
}

/// Whether the on-disk cache exists and was written less than
/// [`COUNTRY_CACHE_DAYS`] ago (`File.Exists && UtcNow - LastWriteTimeUtc <
/// CountryCacheDays` upstream). A missing file, an unreadable mtime, or an mtime
/// that cannot be compared all read as "not fresh".
async fn cache_is_fresh(path: &Path) -> bool {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    // A future mtime (clock skew) has a negative age upstream, which is "fresh".
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    age < Duration::from_secs(COUNTRY_CACHE_DAYS * 24 * 60 * 60)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    use ferrofin_traits::error::ServiceError;

    use super::{API_URL, COUNTRY_CACHE_DAYS, SchedulesDirect};
    use crate::fetch::SourceFetcher;

    const COUNTRIES: &[u8] =
        br#"{"North America":[{"fullName":"United States","shortName":"USA"}]}"#;

    /// A fetcher that records how often it was hit and what it was asked for,
    /// answering with `body` (or a backend failure when `body` is `None`).
    struct CountingFetcher {
        body: Option<Vec<u8>>,
        calls: AtomicUsize,
        last_url: std::sync::Mutex<Option<String>>,
    }

    impl CountingFetcher {
        fn serving(body: &[u8]) -> Arc<Self> {
            Arc::new(Self {
                body: Some(body.to_vec()),
                calls: AtomicUsize::new(0),
                last_url: std::sync::Mutex::new(None),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                body: None,
                calls: AtomicUsize::new(0),
                last_url: std::sync::Mutex::new(None),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl SourceFetcher for CountingFetcher {
        async fn fetch(&self, _url: &str) -> Result<String, ServiceError> {
            unreachable!("the country list is fetched as bytes")
        }

        async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, ServiceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_url.lock().unwrap() = Some(url.to_owned());
            self.body
                .clone()
                .ok_or_else(|| ServiceError::backend("schedulesdirect.org: 503"))
        }
    }

    fn sd(fetcher: &Arc<CountingFetcher>, dir: &tempfile::TempDir) -> SchedulesDirect {
        let fetcher: Arc<dyn SourceFetcher> = fetcher.clone();
        SchedulesDirect::new(fetcher, dir.path())
    }

    /// Backdates the cache file's mtime by `days`.
    fn age_cache(sd: &SchedulesDirect, days: u64) {
        let file = std::fs::File::options()
            .write(true)
            .open(sd.countries_cache_path())
            .expect("open cache");
        file.set_modified(SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60))
            .expect("set mtime");
    }

    #[tokio::test]
    async fn fresh_fetch_hits_sd_and_writes_the_cache_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fetcher = CountingFetcher::serving(COUNTRIES);
        // The cache directory need not pre-exist (upstream `CreateDirectory`).
        let nested = dir.path().join("cache");
        let fetcher_dyn: Arc<dyn SourceFetcher> = fetcher.clone();
        let sd = SchedulesDirect::new(fetcher_dyn, &nested);

        let bytes = sd.get_available_countries().await.expect("countries");
        assert_eq!(bytes, COUNTRIES);
        assert_eq!(fetcher.calls(), 1);
        assert_eq!(
            fetcher.last_url.lock().unwrap().as_deref(),
            Some(format!("{API_URL}/available/countries").as_str())
        );
        assert_eq!(
            std::fs::read(nested.join("sd-countries.json")).expect("cache file"),
            COUNTRIES
        );
    }

    #[tokio::test]
    async fn second_call_is_served_from_memory_without_refetching() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fetcher = CountingFetcher::serving(COUNTRIES);
        let sd = sd(&fetcher, &dir);

        sd.get_available_countries().await.expect("first");
        // Clones share the memory cache.
        let again = sd.clone().get_available_countries().await.expect("second");
        assert_eq!(again, COUNTRIES);
        assert_eq!(fetcher.calls(), 1);
    }

    #[tokio::test]
    async fn a_new_process_reads_the_disk_cache_within_the_ttl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = CountingFetcher::serving(COUNTRIES);
        sd(&first, &dir)
            .get_available_countries()
            .await
            .expect("seed the disk cache");

        // A fresh instance (no memory copy) must not hit SD while the file is
        // within its TTL — even when SD would answer differently now.
        let second = CountingFetcher::serving(b"{}");
        let sd = sd(&second, &dir);
        age_cache(&sd, COUNTRY_CACHE_DAYS - 1);
        let bytes = sd.get_available_countries().await.expect("cached");
        assert_eq!(bytes, COUNTRIES);
        assert_eq!(second.calls(), 0);
    }

    #[tokio::test]
    async fn an_expired_disk_cache_is_refetched_and_rewritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = CountingFetcher::serving(COUNTRIES);
        sd(&first, &dir)
            .get_available_countries()
            .await
            .expect("seed the disk cache");

        let second = CountingFetcher::serving(b"{\"updated\":true}");
        let sd = sd(&second, &dir);
        age_cache(&sd, COUNTRY_CACHE_DAYS);
        let bytes = sd.get_available_countries().await.expect("refetched");
        assert_eq!(bytes, b"{\"updated\":true}");
        assert_eq!(second.calls(), 1);
        assert_eq!(
            std::fs::read(sd.countries_cache_path()).expect("cache file"),
            b"{\"updated\":true}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_cache_file_is_removed_and_refetched() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let fetcher = CountingFetcher::serving(COUNTRIES);
        let sd = sd(&fetcher, &dir);
        // A fresh cache file the process cannot read — upstream's `IOException`
        // branch: delete it and fetch anew.
        let path = sd.countries_cache_path();
        std::fs::write(&path, b"stale").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        if std::fs::read(&path).is_ok() {
            // Running as root: permissions do not bite, so the branch cannot be
            // provoked this way.
            return;
        }

        let bytes = sd.get_available_countries().await.expect("refetched");
        assert_eq!(bytes, COUNTRIES);
        assert_eq!(fetcher.calls(), 1);
        // The unreadable file was replaced by the fetched document.
        assert_eq!(std::fs::read(&path).expect("rewritten"), COUNTRIES);
    }

    #[tokio::test]
    async fn upstream_failure_surfaces_a_backend_error_and_caches_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fetcher = CountingFetcher::failing();
        let sd = sd(&fetcher, &dir);

        let err = sd.get_available_countries().await.expect_err("must fail");
        assert!(matches!(err, ServiceError::Backend(_)), "{err:?}");
        assert!(!sd.countries_cache_path().exists());

        // Nothing was memoised: the next call tries SD again.
        let _ = sd.get_available_countries().await;
        assert_eq!(fetcher.calls(), 2);
    }

    #[tokio::test]
    async fn an_unwritable_cache_dir_is_a_backend_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Put a *file* where the cache directory must be created.
        let blocked = dir.path().join("cache");
        std::fs::write(&blocked, b"not a dir").expect("block");
        let fetcher = CountingFetcher::serving(COUNTRIES);
        let fetcher_dyn: Arc<dyn SourceFetcher> = fetcher.clone();
        let sd = SchedulesDirect::new(fetcher_dyn, &blocked);

        let err = sd.get_available_countries().await.expect_err("must fail");
        assert!(matches!(err, ServiceError::BackendSource(_)), "{err:?}");
        assert!(err.to_string().starts_with("create "), "{err}");
    }

    #[test]
    fn debug_names_the_cache_dir_only() {
        let fetcher = CountingFetcher::serving(COUNTRIES);
        let fetcher_dyn: Arc<dyn SourceFetcher> = fetcher;
        let sd = SchedulesDirect::new(fetcher_dyn, "/var/cache/ferrofin");
        let dbg = format!("{sd:?}");
        assert!(dbg.contains("/var/cache/ferrofin"), "{dbg}");
        assert!(!dbg.contains("countries"), "{dbg}");
    }
}
