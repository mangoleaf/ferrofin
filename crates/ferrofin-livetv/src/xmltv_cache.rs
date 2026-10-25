//! The on-disk XMLTV document cache every guide reader shares.
//!
//! Port of `XmlTvListingsProvider.GetXml` (`Jellyfin.LiveTv/Listings/
//! XmlTvListingsProvider.cs`): a listings provider's document is kept at
//! `{cache}/xmltv/{id}.xml` and reused while it is younger than
//! [`MAX_CACHE_AGE`], so listing lineups, opening the mapping dialog, saving a
//! mapping and refreshing the guide do not each download the whole guide. A
//! remote download that failed is not retried for [`MAX_CACHE_AGE`], and any
//! failed download falls back to the previous document when one is cached.
//! Saving or deleting a provider drops its file
//! (`ListingsManager.InvalidateListingsProviderCache`), and saving also drops
//! its path's failure record (`XmlTvListingsProvider.Validate`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ferrofin_model::live_tv::ListingsProviderInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_util::directory_path::DirectoryPath;
use uuid::Uuid;

use crate::error::LiveTvError;
use crate::fetch::SourceFetcher;
use crate::schedules_direct::cache_is_fresh;

/// How long a cached document is reused, and how long a failed remote download
/// is not retried (`XmlTvListingsProvider._maxCacheAge`).
pub const MAX_CACHE_AGE: Duration = Duration::from_hours(1);

/// The cap on one download (`XmlTvListingsProvider._downloadTimeout`).
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(15);

/// The cache subdirectory under the application cache path.
const CACHE_SUBDIR: &str = "xmltv";

/// The shared XMLTV document cache. Cloning shares the failure record.
#[derive(Clone)]
pub struct XmlTvCache {
    fetcher: Arc<dyn SourceFetcher>,
    cache_dir: DirectoryPath,
    /// `_lastDownloadFailures`: when each remote path last failed to download.
    failures: Arc<Mutex<HashMap<String, Instant>>>,
}

impl std::fmt::Debug for XmlTvCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XmlTvCache")
            .field("cache_dir", &self.cache_dir)
            .finish_non_exhaustive()
    }
}

impl XmlTvCache {
    /// Creates the cache over `fetcher`, storing documents under
    /// `{cache_dir}/xmltv` (`IApplicationPaths.CachePath` upstream).
    #[must_use]
    pub fn new(fetcher: Arc<dyn SourceFetcher>, cache_dir: impl Into<DirectoryPath>) -> Self {
        Self {
            fetcher,
            cache_dir: cache_dir.into(),
            failures: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The cache file for provider `id`. Provider ids are generated as
    /// `Guid.ToString("N")`; anything that is not a GUID is kept out of the
    /// path, as `InvalidateListingsProviderCache` keeps it out.
    fn cache_file(&self, id: &str) -> Option<PathBuf> {
        let id = Uuid::try_parse(id).ok()?.simple().to_string();
        Some(self.cache_dir.join(CACHE_SUBDIR).join(format!("{id}.xml")))
    }

    /// [`cache_file`](Self::cache_file) for a known GUID, for tests.
    #[cfg(test)]
    pub(crate) fn cache_file_for_test(&self, id: &str) -> PathBuf {
        self.cache_file(id).expect("a GUID provider id")
    }

    /// The provider's XMLTV document: the cached copy while it is fresh,
    /// otherwise a download that replaces it.
    ///
    /// # Errors
    ///
    /// A failed download (transport, status, I/O, or [`DOWNLOAD_TIMEOUT`])
    /// with no cached copy to fall back to, or a remote path that failed
    /// within [`MAX_CACHE_AGE`] and has no cached copy.
    pub async fn get_xml(&self, info: &ListingsProviderInfo) -> Result<String, ServiceError> {
        let path = info.path.as_deref().unwrap_or_default();
        let Some(cache_file) = info.id.as_deref().and_then(|id| self.cache_file(id)) else {
            return self.download(path).await;
        };
        if cache_is_fresh(&cache_file, MAX_CACHE_AGE).await
            && let Ok(body) = tokio::fs::read_to_string(&cache_file).await
        {
            return Ok(body);
        }

        if crate::fetch::is_http(path) && self.failed_recently(path) {
            return match tokio::fs::read_to_string(&cache_file).await {
                Ok(body) => Ok(body),
                Err(_) => Err(ServiceError::Backend(format!(
                    "Skipping the XMLTV download after a recent failure: {path}"
                ))),
            };
        }

        match self.download_to(path, &cache_file).await {
            Ok(body) => {
                self.failure_record().remove(path);
                Ok(body)
            }
            Err(err) => {
                self.failure_record()
                    .insert(path.to_owned(), Instant::now());
                match tokio::fs::read_to_string(&cache_file).await {
                    Ok(body) => {
                        tracing::warn!(
                            %path,
                            error = %err,
                            "live tv: XMLTV download failed; using the previously downloaded file"
                        );
                        Ok(body)
                    }
                    Err(_) => Err(err),
                }
            }
        }
    }

    /// Drops provider `id`'s cached document, so the next read downloads it.
    pub async fn invalidate(&self, id: &str) {
        let Some(cache_file) = self.cache_file(id) else {
            return;
        };
        match tokio::fs::remove_file(&cache_file).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(
                path = %cache_file.display(),
                error = %err,
                "live tv: could not delete the XMLTV cache file"
            ),
        }
    }

    /// Forgets `path`'s recent download failure, so the next read retries it.
    ///
    /// `XmlTvListingsProvider.Validate`: saving a provider is an explicit
    /// retry, so its backoff goes with the cached file.
    pub fn forget_failure(&self, path: &str) {
        self.failure_record().remove(path);
    }

    /// Whether `path` failed to download within [`MAX_CACHE_AGE`].
    fn failed_recently(&self, path: &str) -> bool {
        self.failure_record()
            .get(path)
            .is_some_and(|failed| failed.elapsed() < MAX_CACHE_AGE)
    }

    fn failure_record(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        self.failures.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Downloads `path` within [`DOWNLOAD_TIMEOUT`], decompressing a gzipped
    /// guide ([`SourceFetcher::fetch_listings`]), so the cache holds the
    /// document `UnzipIfNeededAndCopy` would have written.
    async fn download(&self, path: &str) -> Result<String, ServiceError> {
        tokio::time::timeout(DOWNLOAD_TIMEOUT, self.fetcher.fetch_listings(path))
            .await
            .map_err(|_| {
                ServiceError::Backend(format!("Timed out downloading the XMLTV file from {path}"))
            })?
    }

    /// Downloads `path` and replaces `cache_file` with it through a temporary
    /// file, so a failed write never leaves a truncated document behind.
    async fn download_to(&self, path: &str, cache_file: &Path) -> Result<String, ServiceError> {
        let body = self.download(path).await?;
        // `UnzipIfNeededAndCopy`: an empty document is a failed download, so it
        // never replaces a good cached copy.
        if body.is_empty() {
            return Err(ServiceError::Backend(format!(
                "Downloaded XMLTV file is empty: {path}"
            )));
        }
        if let Some(dir) = cache_file.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|e| LiveTvError::io(format!("create {}", dir.display()), e))?;
        }
        let temp = cache_file.with_extension("xml.tmp");
        let written = tokio::fs::write(&temp, &body).await;
        let replaced = match written {
            Ok(()) => tokio::fs::rename(&temp, cache_file).await,
            Err(e) => Err(e),
        };
        if let Err(e) = replaced {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(LiveTvError::io(format!("write {}", cache_file.display()), e).into());
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use ferrofin_model::live_tv::ListingsProviderInfo;
    use ferrofin_traits::error::ServiceError;

    use super::XmlTvCache;
    use crate::fetch::SourceFetcher;

    /// A guide host that never answers, counting the downloads it was asked for.
    #[derive(Default)]
    struct HangingFetcher(AtomicUsize);

    #[async_trait]
    impl SourceFetcher for HangingFetcher {
        async fn fetch(&self, _url: &str) -> Result<String, ServiceError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }
    }

    /// Upstream `GetProgramsAsync_DownloadTimesOut_DoesNotSurfaceAsCancellation`:
    /// a download that outlives `_downloadTimeout` is an ordinary listings
    /// error (`TimeoutException`), so the guide refresh carries on with the
    /// other providers, and the remote path is then backed off. Upstream's
    /// cancellation-vs-timeout distinction has no Rust counterpart: the
    /// timeout is an ordinary `ServiceError::Backend`.
    #[tokio::test(start_paused = true)]
    async fn a_download_timeout_is_an_ordinary_error() {
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let fetcher = Arc::new(HangingFetcher::default());
        let cache = XmlTvCache::new(
            Arc::clone(&fetcher) as Arc<dyn SourceFetcher>,
            cache_dir.path(),
        );
        let info = ListingsProviderInfo {
            id: Some(uuid::Uuid::new_v4().simple().to_string()),
            path: Some("http://guide/xmltv.xml".to_owned()),
            ..ListingsProviderInfo::default()
        };

        // The outer bound turns a missing download timeout into a failure, not a hang.
        let err = tokio::time::timeout(2 * super::DOWNLOAD_TIMEOUT, cache.get_xml(&info))
            .await
            .expect("the download timeout fired first")
            .expect_err("timed out");
        assert!(
            matches!(&err, ServiceError::Backend(message) if message.starts_with("Timed out")),
            "{err:?}"
        );
        assert!(matches!(
            cache.get_xml(&info).await,
            Err(ServiceError::Backend(message)) if message.starts_with("Skipping")
        ));
        assert_eq!(fetcher.0.load(Ordering::SeqCst), 1);
    }

    /// `GetXml` → `UnzipIfNeededAndCopy`: a gzipped guide is decompressed on
    /// download, so readers get the XML and the cached copy is the XML too,
    /// never the compressed bytes.
    #[tokio::test]
    async fn a_gzipped_guide_is_cached_decompressed() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let guide = dir.path().join("guide.xml.gz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"<tv></tv>").expect("compress");
        std::fs::write(&guide, gz.finish().expect("finish")).expect("write guide");
        let cache = XmlTvCache::new(
            Arc::new(crate::fetch::ReqwestFetcher::new()) as Arc<dyn SourceFetcher>,
            dir.path().join("cache"),
        );
        let id = uuid::Uuid::new_v4().simple().to_string();
        let info = ListingsProviderInfo {
            id: Some(id.clone()),
            path: Some(guide.to_str().expect("utf-8 path").to_owned()),
            ..ListingsProviderInfo::default()
        };

        assert_eq!(cache.get_xml(&info).await.expect("guide"), "<tv></tv>");
        assert_eq!(
            std::fs::read(cache.cache_file_for_test(&id)).expect("cached copy"),
            b"<tv></tv>"
        );
    }
}
