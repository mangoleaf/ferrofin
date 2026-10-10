//! Automatic subtitle downloads shared by scans and the daily task.

use std::collections::HashSet;
use std::sync::{Arc, Weak};

use ferrofin_db::entities::base_items::{BaseItemEntity, MediaStreamInfoEntity};
use ferrofin_model::configuration::LibraryOptions;
use ferrofin_model::entities::{MediaStreamType, VideoType};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::persistence::{MediaStreamQuery, MediaStreamRepository};
use ferrofin_traits::stubs::LiveTvManager;
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
    live_tv: Option<Weak<dyn LiveTvManager>>,
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
            live_tv: None,
            gate: tokio::sync::Mutex::new(()),
        }
    }

    /// Connects the current DVR captures so incomplete recordings do not spend
    /// subtitle download quota. A weak reference avoids the library/DVR cycle.
    #[must_use]
    pub fn with_live_tv(mut self, live_tv: &Arc<dyn LiveTvManager>) -> Self {
        self.live_tv = Some(Arc::downgrade(live_tv));
        self
    }

    /// The scheduled task first queries for videos missing at least one saved
    /// language, using any audio track and external/all subtitle tracks. Its
    /// subsequent download uses the stricter default-audio/text rules below.
    pub(crate) async fn is_task_candidate(
        &self,
        video: &BaseItemEntity,
        options: &LibraryOptions,
    ) -> Result<bool, ServiceError> {
        let item_id = Uuid::parse_str(&video.id)
            .map_err(|error| ServiceError::invalid_input(error.to_string()))?;
        let streams = self
            .streams
            .get_media_streams(&MediaStreamQuery {
                item_id,
                stream_type: None,
                index: None,
            })
            .await?;
        Ok(task_needs_language(&streams, options))
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
        self.download_missing_using_streams_locked(video, options, cancel, None)
            .await;
    }

    /// Scan downloads consider embedded streams before the library's display
    /// filter, as FFProbeVideoInfo.AddExternalSubtitlesAsync does. External
    /// streams are still read afresh for every language.
    pub(crate) async fn download_missing_from_probe_locked(
        &self,
        video: &BaseItemEntity,
        options: &LibraryOptions,
        cancel: &ScanCancel,
        probed: &[MediaStreamInfoEntity],
    ) {
        self.download_missing_using_streams_locked(video, options, cancel, Some(probed))
            .await;
    }

    async fn download_missing_using_streams_locked(
        &self,
        video: &BaseItemEntity,
        options: &LibraryOptions,
        cancel: &ScanCancel,
        probed: Option<&[MediaStreamInfoEntity]>,
    ) {
        let Some(languages) = options.subtitle_download_languages.as_ref() else {
            return;
        };
        if languages.is_empty() || !eligible_video(video, false) {
            return;
        }
        if let Some(live_tv) = self.live_tv.as_ref().and_then(Weak::upgrade) {
            let Some(recordings) = cancel
                .unless_cancelled(live_tv.active_recording_paths())
                .await
            else {
                return;
            };
            match recordings {
                Ok(paths) if paths.iter().any(|path| video.path.as_ref() == Some(path)) => {
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(item_id = %video.id, %error, "subtitle recording lookup failed");
                    return;
                }
            }
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
                search_all_providers: Some(false),
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
                .unless_cancelled(self.fetch_missing(manager.as_ref(), &request, options, probed))
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
        probed: Option<&[MediaStreamInfoEntity]>,
    ) -> Result<Option<ferrofin_traits::subtitles::SubtitleResponse>, ServiceError> {
        let mut streams = self
            .streams
            .get_media_streams(&MediaStreamQuery {
                item_id: request.item_id,
                stream_type: None,
                index: None,
            })
            .await?;
        if let Some(probed) = probed {
            // Preserve external rows attached since the probe began, including
            // earlier languages in this download pass. Only the embedded
            // eligibility snapshot replaces the filtered persisted rows.
            streams.retain(|s| s.is_external);
            streams.extend(probed.iter().filter(|s| !s.is_external).cloned());
        }
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

/// `SubtitleDownloader` accepts complete movie/episode video files only.
/// Disc rips/images and channel livestreams cannot be matched by file hash.
fn eligible_video(video: &BaseItemEntity, is_active_recording: bool) -> bool {
    if video.is_virtual_item
        || is_active_recording
        || !matches!(video.type_.rsplit('.').next(), Some("Movie" | "Episode"))
        || video.path.as_deref().is_none_or(str::is_empty)
    {
        return false;
    }
    let data = crate::item_data::parse_data(video.data.as_deref());
    let video_type = data
        .get("VideoType")
        .and_then(|value| serde_json::from_value::<VideoType>(value.clone()).ok())
        .unwrap_or(VideoType::VideoFile);
    if video_type != VideoType::VideoFile {
        return false;
    }
    let channel = video
        .channel_id
        .as_deref()
        .and_then(|id| Uuid::parse_str(id).ok())
        .is_some_and(|id| !id.is_nil());
    !(channel
        && video
            .tags
            .as_deref()
            .into_iter()
            .flat_map(|tags| tags.split('|'))
            .any(|tag| tag.eq_ignore_ascii_case("livestream")))
}

/// Port of the scheduled task's union of language-specific candidate queries:
/// unlike the downloader, its audio test considers every audio track.
fn task_needs_language(streams: &[MediaStreamInfoEntity], options: &LibraryOptions) -> bool {
    options
        .subtitle_download_languages
        .as_ref()
        .is_some_and(|languages| {
            languages.iter().any(|language| {
                !streams.iter().any(|stream| {
                    stream
                        .language
                        .as_deref()
                        .is_some_and(|stored| stored.eq_ignore_ascii_case(language))
                        && match media_stream_type_from_disc(stream.stream_type) {
                            MediaStreamType::Audio => options.skip_subtitles_if_audio_track_matches,
                            MediaStreamType::Subtitle => {
                                stream.is_external
                                    || options.skip_subtitles_if_embedded_subtitles_present
                            }
                            _ => false,
                        }
                })
            })
        })
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
            && (((s.is_external || s.codec.as_deref().is_some_and(|c| !c.is_empty()))
                && MediaStream::is_text_format(s.codec.as_deref()))
                || (!s.is_external && options.skip_subtitles_if_embedded_subtitles_present))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ferrofin_model::providers::RemoteSubtitleInfo;
    use ferrofin_traits::persistence::ItemPersistenceService;
    use ferrofin_traits::subtitles::{SubtitleProvider, SubtitleResponse};
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

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
    #[case("hdmv_pgs_subtitle", true, false, true)]
    #[case("hdmv_pgs_subtitle", true, true, true)]
    #[case("dvdsub", true, true, true)]
    #[case("subrip", true, false, false)]
    #[case("subrip", true, true, false)]
    #[case("", false, false, true)]
    #[case("", false, true, false)]
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
        non_matching: AtomicBool,
        requests: std::sync::Mutex<Vec<SubtitleSearchRequest>>,
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
            self.requests.lock().unwrap().push(request.clone());
            if self.hang.load(Ordering::SeqCst) == 1 {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(vec![RemoteSubtitleInfo {
                id: Some(request.language.clone()),
                is_hash_match: Some(!self.non_matching.load(Ordering::SeqCst)),
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
        temp: tempfile::TempDir,
        _manager: Arc<dyn SubtitleManager>,
        db: ferrofin_db::Database,
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
            crate::test_support::library_manager_over(db.clone()),
            streams.clone(),
            vec![provider.clone()],
            temp.path().join("metadata"),
        ));
        Harness {
            downloader: SubtitleDownloader::new(&manager, streams),
            _manager: manager,
            temp,
            db,
            provider,
            video,
            options: LibraryOptions {
                subtitle_download_languages: Some(vec!["eng".to_owned(), "ENG".to_owned()]),
                ..Default::default()
            },
        }
    }

    #[rstest::rstest]
    #[case::hidden_text("subrip", false, 0)]
    #[case::hidden_image_downloads("hdmv_pgs_subtitle", false, 1)]
    #[case::hidden_image_satisfies_skip("hdmv_pgs_subtitle", true, 0)]
    #[tokio::test]
    async fn scan_download_eligibility_uses_embedded_streams_before_display_filter(
        #[case] codec: &str,
        #[case] skip_embedded: bool,
        #[case] expected_searches: usize,
    ) {
        let mut h = harness().await;
        h.options.allow_embedded_subtitles =
            ferrofin_model::configuration::EmbeddedSubtitleOptions::AllowNone;
        h.options.skip_subtitles_if_embedded_subtitles_present = skip_embedded;
        let id = Uuid::parse_str(&h.video.id).unwrap();
        h.downloader
            .streams
            .save_media_streams(id, &[stream(1, "")])
            .await
            .unwrap();
        let mut hidden = stream(2, "ENG");
        hidden.codec = Some(codec.to_owned());
        let probed = vec![stream(1, ""), hidden];
        let cancel = ScanCancel::new();
        let _guard = h.downloader.lock(&cancel).await.unwrap();
        h.downloader
            .download_missing_from_probe_locked(&h.video, &h.options, &cancel, &probed)
            .await;
        assert_eq!(
            h.provider.searches.load(Ordering::SeqCst),
            expected_searches
        );
        let persisted = h
            .downloader
            .streams
            .get_media_streams(&MediaStreamQuery {
                item_id: id,
                stream_type: None,
                index: None,
            })
            .await
            .unwrap();
        assert!(
            persisted
                .iter()
                .all(|s| s.is_external || s.stream_type != 2),
            "hidden embedded rows must stay out of persistence"
        );
    }

    #[tokio::test]
    async fn scan_download_eligibility_keeps_external_streams_newer_than_probe() {
        let h = harness().await;
        let id = Uuid::parse_str(&h.video.id).unwrap();
        let mut attached = stream(2, "eng");
        attached.is_external = true;
        attached.codec = Some("subrip".to_owned());
        h.downloader
            .streams
            .save_media_streams(id, &[attached])
            .await
            .unwrap();
        // The stale probe had no sidecar. A live external row must still satisfy
        // this language when the raw embedded snapshot is used for eligibility.
        let cancel = ScanCancel::new();
        let _guard = h.downloader.lock(&cancel).await.unwrap();
        h.downloader
            .download_missing_from_probe_locked(&h.video, &h.options, &cancel, &[stream(1, "")])
            .await;
        assert_eq!(h.provider.searches.load(Ordering::SeqCst), 0);
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

    #[rstest::rstest]
    #[case(None, true)]
    #[case(Some(r#"{"VideoType":"VideoFile"}"#), true)]
    #[case(Some(r#"{"VideoType":0}"#), true)]
    #[case(Some(r#"{"VideoType":"Dvd"}"#), false)]
    #[case(Some(r#"{"VideoType":"BluRay"}"#), false)]
    #[case(Some(r#"{"VideoType":"Iso"}"#), false)]
    #[case(Some(r#"{"VideoType":2}"#), false)]
    fn automatic_download_only_accepts_complete_video_files(
        #[case] data: Option<&str>,
        #[case] eligible: bool,
    ) {
        let mut video = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
            path: Some("/media/Movie.mkv".to_owned()),
            data: data.map(str::to_owned),
            ..Default::default()
        };
        assert_eq!(eligible_video(&video, false), eligible);
        assert!(
            !eligible_video(&video, true),
            "active DVR capture is incomplete"
        );
        video.type_ = "MediaBrowser.Controller.Entities.TV.Episode".to_owned();
        assert_eq!(eligible_video(&video, false), eligible);
        video.channel_id = Some(Uuid::new_v4().to_string());
        video.tags = Some("drama|LiVeStReAm".to_owned());
        assert!(!eligible_video(&video, false));
        video.channel_id = Some(Uuid::nil().to_string());
        assert_eq!(
            eligible_video(&video, false),
            eligible,
            "library livestream tag alone is harmless"
        );
        video.type_ = "MediaBrowser.Controller.Entities.Video".to_owned();
        assert!(!eligible_video(&video, false));
    }

    #[test]
    fn scheduled_language_preselection_uses_any_audio_and_subtitle_scope() {
        let mut streams = vec![stream(0, "fra"), stream(0, "ENG"), stream(2, "ger")];
        streams[0].is_default = true;
        streams[2].codec = Some("hdmv_pgs_subtitle".to_owned());
        let mut options = LibraryOptions {
            subtitle_download_languages: Some(vec!["eng".to_owned()]),
            skip_subtitles_if_audio_track_matches: true,
            ..Default::default()
        };
        assert!(!task_needs_language(&streams, &options));
        assert!(
            needs_language(&streams, "eng", &options),
            "scan uses the default audio instead"
        );
        options.subtitle_download_languages = Some(vec!["eng".to_owned(), "ger".to_owned()]);
        assert!(
            task_needs_language(&streams, &options),
            "union includes unsatisfied German"
        );
        options.skip_subtitles_if_embedded_subtitles_present = true;
        assert!(!task_needs_language(&streams, &options));
        options.skip_subtitles_if_embedded_subtitles_present = false;
        streams[2].is_external = true;
        assert!(
            !task_needs_language(&streams, &options),
            "task excludes any matching external subtitle"
        );
    }

    #[tokio::test]
    async fn perfect_match_and_saved_languages_change_the_next_download() {
        let mut h = harness().await;
        let cancel = ScanCancel::new();
        h.options.require_perfect_subtitle_match = true;
        h.provider.non_matching.store(true, Ordering::SeqCst);
        h.downloader
            .download_missing(&h.video, &h.options, &cancel)
            .await;
        assert_eq!(h.provider.downloads.load(Ordering::SeqCst), 0);
        h.options.require_perfect_subtitle_match = false;
        h.downloader
            .download_missing(&h.video, &h.options, &cancel)
            .await;
        assert_eq!(h.provider.downloads.load(Ordering::SeqCst), 1);
        h.options.subtitle_download_languages = Some(vec!["fra".to_owned()]);
        h.downloader
            .download_missing(&h.video, &h.options, &cancel)
            .await;
        assert_eq!(h.provider.downloads.load(Ordering::SeqCst), 2);
        let requests = h.provider.requests.lock().unwrap();
        assert_eq!(requests[0].is_perfect_match, Some(true));
        assert_eq!(requests.last().unwrap().language, "fra");
        assert!(requests.iter().all(|request| request.is_automated));
    }

    #[rstest::rstest]
    #[case(false, false, 1)]
    #[case(false, true, 0)]
    #[case(true, false, 1)]
    #[case(true, true, 1)]
    #[tokio::test]
    async fn image_subtitle_and_live_embedded_skip_option_choose_downloads(
        #[case] external: bool,
        #[case] skip_embedded: bool,
        #[case] expected_downloads: usize,
    ) {
        let mut h = harness().await;
        let mut existing = stream(2, "ENG");
        existing.codec = Some("hdmv_pgs_subtitle".to_owned());
        existing.is_external = external;
        h.downloader
            .streams
            .save_media_streams(Uuid::parse_str(&h.video.id).unwrap(), &[existing])
            .await
            .unwrap();
        h.options.skip_subtitles_if_embedded_subtitles_present = skip_embedded;
        h.downloader
            .download_missing(&h.video, &h.options, &ScanCancel::new())
            .await;
        assert_eq!(
            h.provider.downloads.load(Ordering::SeqCst),
            expected_downloads
        );
    }

    #[tokio::test]
    async fn matching_audio_skip_option_can_be_switched_without_restarting() {
        let mut h = harness().await;
        h.downloader
            .streams
            .save_media_streams(Uuid::parse_str(&h.video.id).unwrap(), &[stream(0, "ENG")])
            .await
            .unwrap();
        h.options.skip_subtitles_if_audio_track_matches = true;
        h.downloader
            .download_missing(&h.video, &h.options, &ScanCancel::new())
            .await;
        assert_eq!(h.provider.downloads.load(Ordering::SeqCst), 0);
        h.options.skip_subtitles_if_audio_track_matches = false;
        h.downloader
            .download_missing(&h.video, &h.options, &ScanCancel::new())
            .await;
        assert_eq!(h.provider.downloads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn scheduled_downloads_follow_saved_nested_library_options() {
        use crate::scheduled_tasks::{ScheduledTask, TaskProgress};
        use ferrofin_model::configuration::MediaPathInfo;
        use ferrofin_model::entities::CollectionTypeOptions;
        use ferrofin_traits::library::VirtualFolderManager;
        let h = harness().await;
        let outer = h.temp.path().join("movies");
        let inner = outer.join("other");
        std::fs::create_dir_all(&inner).unwrap();
        let mut video = h.video.clone();
        video.path = Some(inner.join("Film.mkv").to_string_lossy().into_owned());
        std::fs::write(video.path.as_ref().unwrap(), b"video").unwrap();
        crate::FerrofinItemPersistenceService::new(h.db.clone())
            .save_items(&[video])
            .await
            .unwrap();
        let folders = Arc::new(
            crate::FerrofinVirtualFolderManager::new(h.temp.path().join("libraries"))
                .with_item_store(Arc::new(crate::FerrofinItemPersistenceService::new(
                    h.db.clone(),
                ))),
        );
        let outer_options = LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: outer.to_string_lossy().into_owned(),
            }],
            subtitle_download_languages: Some(vec!["eng".to_owned()]),
            ..Default::default()
        };
        let mut inner_options = LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: inner.to_string_lossy().into_owned(),
            }],
            ..Default::default()
        };
        folders
            .add_virtual_folder("Outer", Some(CollectionTypeOptions::movies), &outer_options)
            .await
            .unwrap();
        folders
            .add_virtual_folder("Inner", Some(CollectionTypeOptions::movies), &inner_options)
            .await
            .unwrap();
        let task = crate::scheduled_tasks::library::SubtitleDownloadTask::new(
            crate::test_support::library_manager_over(h.db.clone()),
            folders.clone(),
            Arc::new(h.downloader),
        );
        task.execute(&TaskProgress::default()).await.unwrap();
        assert_eq!(
            h.provider.searches.load(Ordering::SeqCst),
            0,
            "inner disabled library must not inherit outer languages"
        );
        inner_options.subtitle_download_languages = Some(vec!["fra".to_owned()]);
        folders
            .update_library_options("Inner", &inner_options)
            .await
            .unwrap();
        task.execute(&TaskProgress::default()).await.unwrap();
        assert_eq!(h.provider.downloads.load(Ordering::SeqCst), 1);
        assert_eq!(h.provider.requests.lock().unwrap()[0].language, "fra");
        inner_options.subtitle_download_languages = None;
        folders
            .update_library_options("Inner", &inner_options)
            .await
            .unwrap();
        task.execute(&TaskProgress::default()).await.unwrap();
        assert_eq!(h.provider.searches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn automatic_download_passes_saved_provider_controls_and_stops_after_success() {
        let mut h = harness().await;
        h.options.subtitle_fetcher_order = vec!["fake".to_owned()];
        h.options.disabled_subtitle_fetchers = vec!["other".to_owned()];
        h.downloader
            .download_missing(&h.video, &h.options, &ScanCancel::new())
            .await;
        let request = h.provider.requests.lock().unwrap()[0].clone();
        assert_eq!(request.subtitle_fetcher_order, ["fake"]);
        assert_eq!(request.disabled_subtitle_fetchers, ["other"]);
        assert_eq!(request.search_all_providers, Some(false));
        assert!(request.is_automated);
        assert_eq!(h.provider.downloads.load(Ordering::SeqCst), 1);
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
