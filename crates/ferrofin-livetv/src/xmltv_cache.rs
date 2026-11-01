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

    /// The provider's XMLTV document, opened: the cached copy while it is
    /// fresh, otherwise a download that replaces it. The document is never
    /// read into memory here; [`XmlTvDocument::reader`] reads it as a stream.
    ///
    /// # Errors
    ///
    /// A failed download (transport, status, I/O, or [`DOWNLOAD_TIMEOUT`])
    /// with no cached copy to fall back to, or a remote path that failed
    /// within [`MAX_CACHE_AGE`] and has no cached copy.
    pub async fn get_xml(
        &self,
        info: &ListingsProviderInfo,
    ) -> Result<XmlTvDocument, ServiceError> {
        let path = info.path.as_deref().unwrap_or_default();
        let Some(cache_file) = info.id.as_deref().and_then(|id| self.cache_file(id)) else {
            return self.download_unnamed(path).await;
        };
        if cache_is_fresh(&cache_file, MAX_CACHE_AGE).await
            && let Ok(document) = XmlTvDocument::open(&cache_file).await
        {
            return Ok(document);
        }

        if crate::fetch::is_http(path) && self.failed_recently(path) {
            return match XmlTvDocument::open(&cache_file).await {
                Ok(document) => Ok(document),
                Err(_) => Err(ServiceError::Backend(format!(
                    "Skipping the XMLTV download after a recent failure: {}",
                    crate::fetch::redact_url(path)
                ))),
            };
        }

        match self.download_to(path, &cache_file).await {
            Ok(document) => {
                self.failure_record().remove(path);
                Ok(document)
            }
            Err(err) => {
                self.failure_record()
                    .insert(path.to_owned(), Instant::now());
                match XmlTvDocument::open(&cache_file).await {
                    Ok(document) => {
                        tracing::warn!(
                            listings_provider = info.id.as_deref().unwrap_or_default(),
                            path = %crate::fetch::redact_url(path),
                            error = %err,
                            "live tv: XMLTV download failed; using the previously downloaded file"
                        );
                        Ok(document)
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

    /// Downloads `path` for a provider with no cache file (no id) into a
    /// temporary file, which is unlinked once open so it goes when the
    /// document is dropped.
    async fn download_unnamed(&self, path: &str) -> Result<XmlTvDocument, ServiceError> {
        let dir = self.cache_dir.join(CACHE_SUBDIR);
        prepare_dir(&dir).await?;
        let temp = TempFile(dir.join(format!("{}{TEMP_SUFFIX}", Uuid::new_v4().simple())));
        self.download_into(path, &temp.0).await
    }

    /// Downloads `path` and replaces `cache_file` with it through a temporary
    /// file of its own, so a failed, abandoned or concurrent download never
    /// leaves a truncated or interleaved document behind.
    async fn download_to(
        &self,
        path: &str,
        cache_file: &Path,
    ) -> Result<XmlTvDocument, ServiceError> {
        if let Some(dir) = cache_file.parent() {
            prepare_dir(dir).await?;
        }
        let temp = TempFile(
            cache_file.with_extension(format!("{}{TEMP_SUFFIX}", Uuid::new_v4().simple())),
        );
        let document = self.download_into(path, &temp.0).await?;
        // The open handle keeps reading the document after the rename.
        tokio::fs::rename(&temp.0, cache_file)
            .await
            .map_err(|e| LiveTvError::io(format!("write {}", cache_file.display()), e))?;
        Ok(document)
    }

    /// Streams `path` into `temp` within [`DOWNLOAD_TIMEOUT`], decompressing a
    /// gzipped guide ([`SourceFetcher::fetch_listings`]), so the file holds the
    /// document `UnzipIfNeededAndCopy` would have written, and answers it
    /// open.
    async fn download_into(&self, path: &str, temp: &Path) -> Result<XmlTvDocument, ServiceError> {
        use tokio::io::AsyncWriteExt as _;
        let write_error =
            |e| ServiceError::from(LiveTvError::io(format!("write {}", temp.display()), e));
        let download = async {
            let mut body = self.fetcher.fetch_listings(path).await?;
            let mut file = tokio::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(temp)
                .await
                .map_err(write_error)?;
            let mut written = 0_usize;
            while let Some(chunk) = body.next_chunk().await? {
                file.write_all(&chunk).await.map_err(write_error)?;
                written += chunk.len();
            }
            file.flush().await.map_err(write_error)?;
            Ok::<_, ServiceError>((file, written))
        };
        let (file, written) = tokio::time::timeout(DOWNLOAD_TIMEOUT, download)
            .await
            .map_err(|_| {
                ServiceError::Backend(format!(
                    "Timed out downloading the XMLTV file from {}",
                    crate::fetch::redact_url(path)
                ))
            })??;
        // `UnzipIfNeededAndCopy`: an empty document is a failed download, so it
        // never replaces a good cached copy.
        if written == 0 {
            return Err(ServiceError::Backend(format!(
                "Downloaded XMLTV file is empty: {}",
                crate::fetch::redact_url(path)
            )));
        }
        Ok(XmlTvDocument {
            file: file.into_std().await,
        })
    }
}

/// The ending of a download's temporary file name.
const TEMP_SUFFIX: &str = ".xml.tmp";

/// A download's temporary file, removed when this drops: after a failed or
/// abandoned download (the future dropped mid-way, as `GetXml` deletes its
/// temp file when cancelled), and for a document with no cache file once it is
/// open. After a rename into the cache there is nothing left to remove.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                path = %self.0.display(),
                error = %e,
                "live tv: could not delete a temporary XMLTV file"
            ),
        }
    }
}

/// Creates the cache directory `dir` and deletes the temporary files a
/// download that died with the process left in it. A file is stale once it is
/// older than [`DOWNLOAD_TIMEOUT`], which no live download outlasts.
async fn prepare_dir(dir: &Path) -> Result<(), ServiceError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| LiveTvError::io(format!("create {}", dir.display()), e))?;
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Ok(());
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let stale = entry.file_name().to_string_lossy().ends_with(TEMP_SUFFIX)
            && entry
                .metadata()
                .await
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > DOWNLOAD_TIMEOUT));
        if stale {
            drop(TempFile(entry.path()));
        }
    }
    Ok(())
}

/// A provider's XMLTV document, open on disk: the cached copy, or a download
/// the cache has no file for.
///
/// It stays readable while the cache replaces or deletes its file, so every
/// read of one guide refresh sees the same document.
#[derive(Debug)]
pub struct XmlTvDocument {
    file: std::fs::File,
}

impl XmlTvDocument {
    /// Opens the document at `path`.
    async fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: tokio::fs::File::open(path).await?.into_std().await,
        })
    }

    /// The document from its start, for one read. Reads share the file's
    /// position, so they are made one after another, never at once.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be duplicated or rewound.
    pub fn reader(&self) -> std::io::Result<std::io::BufReader<std::fs::File>> {
        use std::io::Seek as _;
        let mut file = self.file.try_clone()?;
        file.rewind()?;
        Ok(std::io::BufReader::new(file))
    }

    /// An empty document, for tests.
    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Self {
        Self {
            file: tempfile::tempfile().expect("temporary file"),
        }
    }

    /// The whole document as text, for tests.
    #[cfg(test)]
    pub(crate) fn text_for_test(&self) -> String {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut self.reader().expect("reader"), &mut text)
            .expect("read");
        text
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
            path: Some("http://guide/xmltv.xml?password=query-secret".to_owned()),
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
        assert!(err.to_string().ends_with("from http://guide"), "{err}");
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

        assert_eq!(
            cache.get_xml(&info).await.expect("guide").text_for_test(),
            "<tv></tv>"
        );
        assert_eq!(
            std::fs::read(cache.cache_file_for_test(&id)).expect("cached copy"),
            b"<tv></tv>"
        );
    }

    /// The files left in the cache directory, by name.
    fn cache_entries(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir.join("xmltv")).map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .map(|entry| {
                        entry
                            .expect("entry")
                            .file_name()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect()
            },
        )
    }

    /// A guide host that sends the start of a guide and then never another
    /// byte, or sends nothing at all.
    struct StallingHost {
        start: Option<&'static [u8]>,
        /// Signalled once the body has handed out its chunk and stalls, so the
        /// temporary file exists by then.
        stalled: Arc<tokio::sync::Notify>,
    }

    /// A body that hands out its one chunk, then waits for ever.
    struct Stalls {
        chunk: Option<Vec<u8>>,
        stalled: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl crate::fetch::BodyChunks for Stalls {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
            if let Some(chunk) = self.chunk.take() {
                return Ok(Some(chunk));
            }
            self.stalled.notify_one();
            std::future::pending().await
        }
    }

    #[async_trait]
    impl SourceFetcher for StallingHost {
        async fn fetch(&self, _url: &str) -> Result<String, ServiceError> {
            unreachable!("guides are fetched through fetch_listings")
        }

        async fn fetch_listings(
            &self,
            _url: &str,
        ) -> Result<Box<dyn crate::fetch::BodyChunks>, ServiceError> {
            Ok(match self.start {
                Some(start) => Box::new(Stalls {
                    chunk: Some(start.to_vec()),
                    stalled: Arc::clone(&self.stalled),
                }),
                None => crate::fetch::SourceBody::from_bytes(200, Vec::new()).body,
            })
        }
    }

    fn provider(id: Option<&str>, path: &str) -> ListingsProviderInfo {
        ListingsProviderInfo {
            id: id.map(str::to_owned),
            path: Some(path.to_owned()),
            ..ListingsProviderInfo::default()
        }
    }

    /// `GetXml` deletes its temporary file when the download fails or is
    /// cancelled, so neither leaves one behind, and an empty download leaves
    /// no cached copy either.
    #[tokio::test]
    async fn a_failed_or_abandoned_download_leaves_no_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = uuid::Uuid::new_v4().simple().to_string();

        let stalled = Arc::new(tokio::sync::Notify::new());
        let stalling = XmlTvCache::new(
            Arc::new(StallingHost {
                start: Some(b"<tv><channel id=\"a\">"),
                stalled: Arc::clone(&stalled),
            }) as Arc<dyn SourceFetcher>,
            dir.path(),
        );
        for info in [
            provider(Some(&id), "http://guide/a.xml"),
            provider(None, "http://guide/b.xml"),
        ] {
            let mut download = Box::pin(stalling.get_xml(&info));
            tokio::select! {
                _ = &mut download => panic!("the download never ends"),
                () = stalled.notified() => {}
            }
            let partial = cache_entries(dir.path());
            assert!(
                partial.len() == 1 && partial[0].ends_with(".xml.tmp"),
                "the download is under way in its temporary file: {partial:?}"
            );
            drop(download);
            assert_eq!(cache_entries(dir.path()), Vec::<String>::new());
        }

        let empty = XmlTvCache::new(
            Arc::new(StallingHost {
                start: None,
                stalled: Arc::default(),
            }) as Arc<dyn SourceFetcher>,
            dir.path(),
        );
        let err = empty
            .get_xml(&provider(Some(&id), "http://guide/a.xml"))
            .await
            .expect_err("an empty download is a failure");
        assert!(
            matches!(&err, ServiceError::Backend(m) if m.contains("empty")),
            "{err:?}"
        );
        assert_eq!(cache_entries(dir.path()), Vec::<String>::new());
    }

    /// A temporary file a download that died with the process left behind is
    /// deleted by the next download once it is older than any live one; a
    /// newer one may be a download in flight and stays.
    #[tokio::test]
    async fn a_stale_temporary_file_is_swept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guide = dir.path().join("guide.xml");
        std::fs::write(&guide, "<tv/>").expect("write guide");
        let xmltv = dir.path().join("xmltv");
        std::fs::create_dir_all(&xmltv).expect("mkdir");
        let stale = std::fs::File::create(xmltv.join("old.xml.tmp")).expect("stale");
        stale
            .set_modified(std::time::SystemTime::now() - 2 * super::DOWNLOAD_TIMEOUT)
            .expect("age it");
        std::fs::write(xmltv.join("live.xml.tmp"), "").expect("live");

        let cache = XmlTvCache::new(
            Arc::new(crate::fetch::ReqwestFetcher::new()) as Arc<dyn SourceFetcher>,
            dir.path(),
        );
        let id = uuid::Uuid::new_v4().simple().to_string();
        cache
            .get_xml(&provider(Some(&id), guide.to_str().expect("utf-8")))
            .await
            .expect("guide");
        let mut left = cache_entries(dir.path());
        left.sort();
        assert_eq!(left, vec![format!("{id}.xml"), "live.xml.tmp".to_owned()]);
    }

    /// A provider with no id has no cache file: its download is read from a
    /// temporary file that is already gone from the directory.
    #[tokio::test]
    async fn a_provider_without_an_id_is_read_from_an_unlinked_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guide = dir.path().join("guide.xml");
        std::fs::write(&guide, "<tv/>").expect("write guide");
        let cache = XmlTvCache::new(
            Arc::new(crate::fetch::ReqwestFetcher::new()) as Arc<dyn SourceFetcher>,
            dir.path(),
        );
        let document = cache
            .get_xml(&provider(None, guide.to_str().expect("utf-8")))
            .await
            .expect("guide");
        assert_eq!(cache_entries(dir.path()), Vec::<String>::new());
        assert_eq!(document.text_for_test(), "<tv/>");
    }

    /// A document stays readable when the cache drops or replaces its file, so
    /// both reads of one refresh see the same guide, and two downloads of one
    /// provider at once each get a whole document.
    #[tokio::test]
    async fn an_open_document_outlives_its_cache_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guide = dir.path().join("guide.xml");
        std::fs::write(&guide, "<tv>first</tv>").expect("write guide");
        let cache = XmlTvCache::new(
            Arc::new(crate::fetch::ReqwestFetcher::new()) as Arc<dyn SourceFetcher>,
            dir.path(),
        );
        let id = uuid::Uuid::new_v4().simple().to_string();
        let info = provider(Some(&id), guide.to_str().expect("utf-8"));

        let (a, b) = tokio::join!(cache.get_xml(&info), cache.get_xml(&info));
        let (a, b) = (a.expect("first"), b.expect("second"));
        assert_eq!(a.text_for_test(), "<tv>first</tv>");
        assert_eq!(b.text_for_test(), "<tv>first</tv>");

        cache.invalidate(&id).await;
        std::fs::write(&guide, "<tv>second</tv>").expect("rewrite guide");
        let replaced = cache.get_xml(&info).await.expect("replaced");
        assert_eq!(replaced.text_for_test(), "<tv>second</tv>");
        assert_eq!(
            a.text_for_test(),
            "<tv>first</tv>",
            "read again from the start"
        );
        assert_eq!(cache_entries(dir.path()), vec![format!("{id}.xml")]);
    }

    /// A local guide in the encoding it declares loads through the cache, as
    /// `XmlTvReader` reads it; before, a local guide had to be UTF-8.
    #[tokio::test]
    async fn a_local_guide_in_its_declared_encoding_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guide = dir.path().join("guide.xml");
        std::fs::write(
            &guide,
            b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n<tv>\
              <channel id=\"a\"><display-name>T\xe9l\xe9</display-name></channel></tv>",
        )
        .expect("write guide");
        let cache = XmlTvCache::new(
            Arc::new(crate::fetch::ReqwestFetcher::new()) as Arc<dyn SourceFetcher>,
            dir.path(),
        );
        let id = uuid::Uuid::new_v4().simple().to_string();
        let document = cache
            .get_xml(&provider(Some(&id), guide.to_str().expect("utf-8")))
            .await
            .expect("guide");
        let read =
            crate::xmltv::read_xmltv_where(document.reader().expect("reader"), None, |_| true);
        assert_eq!(read.error, None);
        assert_eq!(read.channels[0].display_name, "Télé");
    }
}
