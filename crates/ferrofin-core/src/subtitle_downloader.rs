//! Automatic subtitle downloads shared by scans and the daily task.

use std::collections::HashSet;
use std::sync::{Arc, Weak};

use ferrofin_db::entities::base_items::{BaseItemEntity, MediaStreamInfoEntity};
use ferrofin_model::configuration::LibraryOptions;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::persistence::{MediaStreamQuery, MediaStreamRepository};
use ferrofin_traits::subtitles::{SubtitleManager, SubtitleMediaType, SubtitleSearchRequest};
use uuid::Uuid;

use crate::ScanCancel;
use crate::db_error::media_stream_type_from_disc;

/// Downloads missing languages using Jellyfin's `SubtitleDownloader` rules.
/// A shared gate prevents the scan and scheduled task from spending download
/// quota twice for the same missing subtitle. The manager is weak because it
/// owns the library, which in turn owns the scanner.
pub struct SubtitleDownloader {
    manager: Weak<dyn SubtitleManager>,
    streams: Arc<dyn MediaStreamRepository>,
    gate: tokio::sync::Mutex<()>,
}

impl SubtitleDownloader {
    /// Builds the downloader. The caller must keep `manager` alive.
    #[must_use]
    pub fn new(
        manager: &Arc<dyn SubtitleManager>,
        streams: Arc<dyn MediaStreamRepository>,
    ) -> Self {
        Self {
            manager: Arc::downgrade(manager),
            streams,
            gate: tokio::sync::Mutex::new(()),
        }
    }

    /// Saves a probe without losing subtitles a scheduled download attached
    /// after the probe's sidecar snapshot. The caller holds the download gate.
    pub(crate) async fn save_probed_streams(
        &self,
        item_id: Uuid,
        probed: &[MediaStreamInfoEntity],
    ) -> Result<(), ServiceError> {
        let current = self
            .streams
            .get_media_streams(&MediaStreamQuery {
                item_id,
                stream_type: None,
                index: None,
            })
            .await?;
        let mut merged = probed.to_vec();
        for mut stream in current {
            if stream.is_external
                && media_stream_type_from_disc(stream.stream_type) == MediaStreamType::Subtitle
                && let Some(path) = &stream.path
                && !merged
                    .iter()
                    .any(|s| s.is_external && s.path.as_ref() == Some(path))
                && tokio::fs::try_exists(path).await.unwrap_or(false)
            {
                stream.stream_index = merged
                    .iter()
                    .map(|s| s.stream_index)
                    .max()
                    .map_or(0, |i| i + 1);
                merged.push(stream);
            }
        }
        self.streams.save_media_streams(item_id, &merged).await
    }

    /// Checks stored streams before each language. Provider failures are
    /// best-effort; cancellation interrupts network waits but lets a local
    /// attachment finish so the sidecar and its stream row stay together.
    pub async fn download_missing(
        &self,
        video: &BaseItemEntity,
        options: &LibraryOptions,
        cancel: &ScanCancel,
    ) {
        let Some(_guard) = self.lock(cancel).await else {
            return;
        };
        self.download_missing_locked(video, options, cancel).await;
    }

    pub(crate) async fn lock(
        &self,
        cancel: &ScanCancel,
    ) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        cancel.unless_cancelled(self.gate.lock()).await
    }

    /// Caller holds the automatic-download gate through stream persistence.
    pub(crate) async fn download_missing_locked(
        &self,
        video: &BaseItemEntity,
        options: &LibraryOptions,
        cancel: &ScanCancel,
    ) {
        let Some(languages) = options.subtitle_download_languages.as_ref() else {
            return;
        };
        if languages.is_empty()
            || video.is_virtual_item
            || !matches!(video.type_.rsplit('.').next(), Some("Movie" | "Episode"))
            || video.path.as_deref().is_none_or(str::is_empty)
        {
            return;
        }
        let (Ok(item_id), Some(manager)) = (Uuid::parse_str(&video.id), self.manager.upgrade())
        else {
            return;
        };
        let mut seen = HashSet::new();
        for language in languages {
            let language = language.trim();
            if language.is_empty() || !seen.insert(language.to_ascii_lowercase()) {
                continue;
            }
            let request = SubtitleSearchRequest {
                item_id,
                language: language.to_owned(),
                is_perfect_match: options.require_perfect_subtitle_match.then_some(true),
                is_automated: true,
                content_type: if video.type_.ends_with("Episode") {
                    SubtitleMediaType::Episode
                } else {
                    SubtitleMediaType::Movie
                },
                name: video.name.clone(),
                series_name: video.series_name.clone(),
                production_year: video.production_year.and_then(|y| i32::try_from(y).ok()),
                parent_index_number: video
                    .parent_index_number
                    .and_then(|n| i32::try_from(n).ok()),
                index_number: video.index_number.and_then(|n| i32::try_from(n).ok()),
                runtime_ticks: video.run_time_ticks,
                media_path: video.path.clone(),
                disabled_subtitle_fetchers: options.disabled_subtitle_fetchers.clone(),
                subtitle_fetcher_order: options.subtitle_fetcher_order.clone(),
                ..SubtitleSearchRequest::default()
            };
            // Only reads and network requests can be dropped on cancellation.
            let fetched = cancel
                .unless_cancelled(self.fetch_missing(manager.as_ref(), &request, options))
                .await;
            match fetched {
                None => break,
                Some(Ok(None)) => {}
                Some(Ok(Some(response))) => {
                    if let Err(error) = manager.upload_subtitle(item_id, &response).await {
                        tracing::warn!(%item_id, language, %error, "subtitle save failed");
                    }
                }
                Some(Err(error)) => {
                    tracing::warn!(%item_id, language, %error, "subtitle download failed");
                }
            }
        }
    }

    async fn fetch_missing(
        &self,
        manager: &dyn SubtitleManager,
        request: &SubtitleSearchRequest,
        options: &LibraryOptions,
    ) -> Result<Option<ferrofin_traits::subtitles::SubtitleResponse>, ServiceError> {
        let streams = self
            .streams
            .get_media_streams(&MediaStreamQuery {
                item_id: request.item_id,
                stream_type: None,
                index: None,
            })
            .await?;
        if !needs_language(&streams, &request.language, options) {
            return Ok(None);
        }
        let results = manager.search_subtitles(request).await?;
        let Some(id) = results
            .iter()
            .filter(|r| !options.require_perfect_subtitle_match || r.is_hash_match == Some(true))
            .find_map(|r| r.id.as_deref())
        else {
            return Ok(None);
        };
        manager.get_remote_subtitles(id).await.map(Some)
    }
}

/// Text subtitles always satisfy a language. Embedded image subtitles do so
/// only when requested; matching audio considers default tracks (or the first
/// audio track when no default is marked), as Jellyfin does.
fn needs_language(
    streams: &[MediaStreamInfoEntity],
    language: &str,
    options: &LibraryOptions,
) -> bool {
    let matches = |s: &MediaStreamInfoEntity| {
        s.language
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case(language))
    };
    let audio = |s: &&MediaStreamInfoEntity| {
        media_stream_type_from_disc(s.stream_type) == MediaStreamType::Audio
    };
    let has_default = streams.iter().filter(audio).any(|s| s.is_default);
    if options.skip_subtitles_if_audio_track_matches {
        let mut candidates = streams.iter().filter(audio);
        if if has_default {
            candidates.any(|s| s.is_default && matches(s))
        } else {
            candidates.next().is_some_and(matches)
        } {
            return false;
        }
    }
    !streams.iter().any(|s| {
        media_stream_type_from_disc(s.stream_type) == MediaStreamType::Subtitle
            && matches(s)
            && (s.is_external
                || (s.codec.as_deref().is_some_and(|c| !c.is_empty())
                    && MediaStream::is_text_format(s.codec.as_deref()))
                || options.skip_subtitles_if_embedded_subtitles_present)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ferrofin_model::providers::RemoteSubtitleInfo;
    use ferrofin_traits::persistence::ItemPersistenceService;
    use ferrofin_traits::subtitles::{SubtitleProvider, SubtitleResponse};
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    fn stream(kind: i32, language: &str) -> MediaStreamInfoEntity {
        MediaStreamInfoEntity {
            stream_type: kind,
            language: Some(language.to_owned()),
            ..Default::default()
        }
    }

    #[rstest::rstest]
    #[case("subrip", false, false, false)]
    #[case("ass", false, false, false)]
    #[case("hdmv_pgs_subtitle", false, false, true)]
    #[case("hdmv_pgs_subtitle", false, true, false)]
    #[case("hdmv_pgs_subtitle", true, false, false)]
    #[case("", false, false, true)]
    #[case("", true, false, false)]
    fn existing_subtitles(
        #[case] codec: &str,
        #[case] external: bool,
        #[case] skip_embedded: bool,
        #[case] needed: bool,
    ) {
        let mut s = stream(2, "ENG");
        s.codec = Some(codec.to_owned());
        s.is_external = external;
        let options = LibraryOptions {
            skip_subtitles_if_embedded_subtitles_present: skip_embedded,
            ..Default::default()
        };
        assert_eq!(needs_language(&[s.clone()], "eng", &options), needed);
        assert!(needs_language(&[s], "fra", &options));
    }

    #[test]
    fn only_default_audio_or_first_audio_satisfies_language() {
        let mut streams = vec![stream(1, ""), stream(0, "fra"), stream(0, "eng")];
        let mut options = LibraryOptions {
            skip_subtitles_if_audio_track_matches: true,
            ..Default::default()
        };
        assert!(needs_language(&streams, "eng", &options));
        assert!(!needs_language(&streams, "fra", &options));
        streams[2].is_default = true;
        assert!(!needs_language(&streams, "eng", &options));
        assert!(needs_language(&streams, "fra", &options));
        options.skip_subtitles_if_audio_track_matches = false;
        assert!(needs_language(&streams, "eng", &options));
    }

    #[derive(Default)]
    struct Provider {
        searches: AtomicUsize,
        downloads: AtomicUsize,
        hang: AtomicU8,
        entered: tokio::sync::Notify,
    }

    #[async_trait]
    impl SubtitleProvider for Provider {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn search(
            &self,
            request: &SubtitleSearchRequest,
        ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
            self.searches.fetch_add(1, Ordering::SeqCst);
            if self.hang.load(Ordering::SeqCst) == 1 {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(vec![RemoteSubtitleInfo {
                id: Some(format!("fake_{}", request.language)),
                is_hash_match: Some(true),
                ..Default::default()
            }])
        }
        async fn get_subtitles(&self, id: &str) -> Result<SubtitleResponse, ServiceError> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            if self.hang.load(Ordering::SeqCst) == 2 {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(SubtitleResponse {
                language: id.to_owned(),
                format: "srt".to_owned(),
                content: b"subtitle".to_vec(),
                ..Default::default()
            })
        }
    }

    struct Harness {
        _temp: tempfile::TempDir,
        _manager: Arc<dyn SubtitleManager>,
        downloader: SubtitleDownloader,
        provider: Arc<Provider>,
        video: BaseItemEntity,
        options: LibraryOptions,
    }

    async fn harness() -> Harness {
        let temp = tempfile::tempdir().unwrap();
        let db = ferrofin_db::Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let path = temp.path().join("Movie.mkv");
        std::fs::write(&path, b"video").unwrap();
        let video = BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(Uuid::new_v4()),
            type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
            path: Some(path.to_string_lossy().into_owned()),
            name: Some("Movie".to_owned()),
            ..Default::default()
        };
        crate::FerrofinItemPersistenceService::new(db.clone())
            .save_items(std::slice::from_ref(&video))
            .await
            .unwrap();
        let streams: Arc<dyn MediaStreamRepository> =
            Arc::new(crate::FerrofinMediaStreamRepository::new(db.clone()));
        let provider = Arc::new(Provider::default());
        let manager: Arc<dyn SubtitleManager> = Arc::new(crate::FerrofinSubtitleManager::new(
            db.clone(),
            crate::test_support::library_manager_over(db),
            streams.clone(),
            vec![provider.clone()],
            temp.path().join("metadata"),
        ));
        Harness {
            downloader: SubtitleDownloader::new(&manager, streams),
            _manager: manager,
            _temp: temp,
            provider,
            video,
            options: LibraryOptions {
                subtitle_download_languages: Some(vec!["eng".to_owned(), "ENG".to_owned()]),
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn concurrent_downloads_and_stale_probe_do_not_duplicate_or_lose_subtitles() {
        let h = harness().await;
        let cancel = ScanCancel::new();
        tokio::join!(
            h.downloader.download_missing(&h.video, &h.options, &cancel),
            h.downloader.download_missing(&h.video, &h.options, &cancel)
        );
        assert_eq!(h.provider.searches.load(Ordering::SeqCst), 1);
        assert_eq!(h.provider.downloads.load(Ordering::SeqCst), 1);
        let id = Uuid::parse_str(&h.video.id).unwrap();
        // A probe started before the download and saw no external subtitle.
        {
            let _guard = h.downloader.lock(&cancel).await.unwrap();
            h.downloader
                .save_probed_streams(id, &[stream(1, "")])
                .await
                .unwrap();
        }
        h.downloader
            .download_missing(&h.video, &h.options, &cancel)
            .await;
        assert_eq!(h.provider.downloads.load(Ordering::SeqCst), 1);
        let rows = h
            .downloader
            .streams
            .get_media_streams(&MediaStreamQuery {
                item_id: id,
                stream_type: None,
                index: None,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        let subtitle = rows.iter().find(|s| s.is_external).unwrap();
        assert_ne!(
            subtitle.stream_index,
            rows.iter().find(|s| !s.is_external).unwrap().stream_index
        );
        std::fs::remove_file(subtitle.path.as_ref().unwrap()).unwrap();
        {
            let _guard = h.downloader.lock(&cancel).await.unwrap();
            h.downloader
                .save_probed_streams(id, &[stream(1, "")])
                .await
                .unwrap();
        }
        h.downloader
            .download_missing(&h.video, &h.options, &cancel)
            .await;
        assert_eq!(h.provider.downloads.load(Ordering::SeqCst), 2);
    }

    #[rstest::rstest]
    #[case(1)]
    #[case(2)]
    #[tokio::test]
    async fn cancellation_stops_network_waits_without_attaching_partial_subtitles(
        #[case] stage: u8,
    ) {
        let h = harness().await;
        h.provider.hang.store(stage, Ordering::SeqCst);
        let cancel = ScanCancel::new();
        let work = async {
            tokio::join!(
                h.downloader.download_missing(&h.video, &h.options, &cancel),
                async {
                    h.provider.entered.notified().await;
                    cancel.cancel();
                }
            );
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), work)
            .await
            .unwrap();
        assert!(
            !std::path::Path::new(h.video.path.as_ref().unwrap())
                .with_extension("eng.srt")
                .exists()
        );
        // Cancellation releases the gate and a later attempt can succeed.
        h.provider.hang.store(0, Ordering::SeqCst);
        h.downloader
            .download_missing(&h.video, &h.options, &ScanCancel::new())
            .await;
        assert!(
            std::path::Path::new(h.video.path.as_ref().unwrap())
                .with_extension("eng.srt")
                .exists()
        );
    }

    #[tokio::test]
    async fn absent_languages_and_disabled_provider_are_quiet() {
        let mut h = harness().await;
        let cancel = ScanCancel::new();
        h.downloader
            .download_missing(&h.video, &LibraryOptions::default(), &cancel)
            .await;
        h.options.disabled_subtitle_fetchers = vec!["FAKE".to_owned()];
        h.downloader
            .download_missing(&h.video, &h.options, &cancel)
            .await;
        assert_eq!(h.provider.searches.load(Ordering::SeqCst), 0);
    }
}
