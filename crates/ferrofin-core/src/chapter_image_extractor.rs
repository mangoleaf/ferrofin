//! Chapter-image reconciliation shared by the scheduled task and video probes.

use std::path::Path;
use std::sync::{Arc, Weak};

use chrono::{DateTime, Utc};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{MediaStreamType, Video3DFormat, VideoType};
use ferrofin_model::entities_media::{ChapterInfo, MediaStream, owning_library};
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::MediaEncoder;
use ferrofin_traits::persistence::{MediaStreamQuery, MediaStreamRepository};
use ferrofin_traits::stubs::LiveTvManager;
use ferrofin_traits::system::PathManager;
use uuid::Uuid;

use crate::ScanCancel;

const TICKS_PER_SECOND: i64 = 10_000_000;

/// Extraction and reconciliation without ownership of the library or chapters.
/// This can be shared with the scanner without a library ownership cycle.
pub struct ChapterImageExtractor {
    folders: Arc<dyn VirtualFolderManager>,
    streams: Arc<dyn MediaStreamRepository>,
    encoder: Arc<dyn MediaEncoder>,
    paths: Arc<dyn PathManager>,
    live_tv: Option<Weak<dyn LiveTvManager>>,
}

/// Refresh result; persist changed rows before pruning their unused images.
pub struct ChapterImageRefresh {
    /// Whether every requested extraction succeeded.
    pub success: bool,
    /// Cancellation before a new extraction stops save/prune; encoder errors remain partial failures.
    pub cancelled: bool,
    /// Whether a chapter image path or its modification date changed.
    pub changed: bool,
    current_images: Vec<String>,
}

impl ChapterImageRefresh {
    /// Deletes saved image files no longer referenced by a chapter. Other files
    /// in the directory are preserved, as in `ChapterManager.DeleteDeadImages`.
    pub fn prune_unused_images(&self, chapters: &[ChapterInfo]) {
        if self.cancelled {
            return;
        }
        for path in &self.current_images {
            let referenced = chapters.iter().any(|chapter| {
                chapter
                    .image_path
                    .as_deref()
                    .is_some_and(|image| !image.is_empty() && image.eq_ignore_ascii_case(path))
            });
            let image = Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    ["png", "jpg", "jpeg", "webp", "tbn", "gif", "svg"]
                        .iter()
                        .any(|supported| extension.eq_ignore_ascii_case(supported))
                });
            if !referenced
                && image
                && let Err(error) = std::fs::remove_file(path)
            {
                tracing::warn!(%path, %error, "cannot delete unused chapter image");
            }
        }
    }
}

struct RefreshContext<'a> {
    video: &'a BaseItemEntity,
    item_id: Uuid,
    stream: Option<MediaStream>,
    enabled: bool,
    extract: bool,
    cancel: &'a ScanCancel,
}

impl ChapterImageExtractor {
    /// Creates the shared extractor over its storage, encoder and path seams.
    #[must_use]
    pub fn new(
        folders: Arc<dyn VirtualFolderManager>,
        streams: Arc<dyn MediaStreamRepository>,
        encoder: Arc<dyn MediaEncoder>,
        paths: Arc<dyn PathManager>,
    ) -> Self {
        Self {
            folders,
            streams,
            encoder,
            paths,
            live_tv: None,
        }
    }

    /// Excludes current DVR captures. The weak reference avoids a DVR cycle.
    #[must_use]
    pub fn with_live_tv(mut self, live_tv: &Arc<dyn LiveTvManager>) -> Self {
        self.live_tv = Some(Arc::downgrade(live_tv));
        self
    }

    /// Refreshes image references even when extraction is disabled or a video
    /// is ineligible. The caller decides when to save the changed chapter rows.
    ///
    /// # Errors
    /// Returns failures reading library options or stored video streams.
    pub async fn refresh(
        &self,
        video: &BaseItemEntity,
        chapters: &mut [ChapterInfo],
        extract: bool,
        cancel: &ScanCancel,
    ) -> Result<ChapterImageRefresh, ServiceError> {
        let mut outcome = ChapterImageRefresh {
            success: true,
            cancelled: false,
            changed: false,
            current_images: Vec::new(),
        };
        if chapters.is_empty() {
            return Ok(outcome);
        }
        let item_id = Uuid::parse_str(&video.id)
            .map_err(|error| ServiceError::invalid_input(error.to_string()))?;
        let folders = self.folders.get_virtual_folders().await?;
        let enabled = owning_library(
            &folders,
            video.top_parent_id.as_deref(),
            video.path.as_deref(),
        )
        .and_then(|folder| folder.library_options.as_ref())
        .is_some_and(|options| options.enable_chapter_image_extraction);
        let streams = self
            .streams
            .get_media_streams(&MediaStreamQuery {
                item_id,
                stream_type: Some(MediaStreamType::Video),
                index: None,
            })
            .await?;
        let data = crate::item_data::parse_data(video.data.as_deref());
        let stream = data
            .get("DefaultVideoStreamIndex")
            .and_then(serde_json::Value::as_i64)
            .and_then(|index| {
                streams
                    .into_iter()
                    .find(|stream| stream.stream_index == index)
            })
            .map(crate::media_source_manager::stream_to_dto);
        let active_recording = self.is_active_recording(video).await;
        let context = RefreshContext {
            video,
            item_id,
            stream,
            enabled,
            cancel,
            extract: extract
                && enabled
                && eligible_video(video, active_recording)
                && !(chapters.len() >= 2 && average_chapter_duration(chapters) < TICKS_PER_SECOND),
        };
        let directory = self
            .paths
            .chapter_image_folder_path(item_id, video.path.as_deref().unwrap_or_default());
        outcome.current_images = saved_images(&directory);
        for chapter in chapters {
            if chapter.start_position_ticks >= video.run_time_ticks.unwrap_or(0) {
                break;
            }
            // Source checks cancellation only before entering a new extraction,
            // outside the encoder's try/catch. Cached and disabled rows still
            // reconcile normally without asking the encoder to do more work.
            if context.extract && context.stream.is_some() && cancel.is_cancelled() {
                let target = self.paths.chapter_image_path(
                    context.item_id,
                    video.path.as_deref().unwrap_or_default(),
                    chapter.start_position_ticks,
                    date_modified_ticks(video.date_modified),
                );
                if !outcome
                    .current_images
                    .iter()
                    .any(|path| path.eq_ignore_ascii_case(&target))
                {
                    outcome.cancelled = true;
                    outcome.success = false;
                    break;
                }
            }
            let result = self
                .refresh_chapter(chapter, &outcome.current_images, &context)
                .await;
            match result {
                Ok(changed) => outcome.changed |= changed,
                Err(error) => {
                    tracing::warn!(item = %video.id, %error, "chapter image could not be extracted or stored");
                    outcome.success = false;
                    break;
                }
            }
        }
        Ok(outcome)
    }

    async fn is_active_recording(&self, video: &BaseItemEntity) -> bool {
        let Some(live_tv) = self.live_tv.as_ref().and_then(Weak::upgrade) else {
            return false;
        };
        match live_tv.active_recording_paths().await {
            Ok(paths) => paths.iter().any(|path| video.path.as_ref() == Some(path)),
            Err(error) => {
                tracing::warn!(item = %video.id, %error, "chapter image recording lookup failed");
                true
            }
        }
    }

    async fn refresh_chapter(
        &self,
        chapter: &mut ChapterInfo,
        current_images: &[String],
        context: &RefreshContext<'_>,
    ) -> Result<bool, ServiceError> {
        let target = self.paths.chapter_image_path(
            context.item_id,
            context.video.path.as_deref().unwrap_or_default(),
            chapter.start_position_ticks,
            date_modified_ticks(context.video.date_modified),
        );
        let exists = current_images
            .iter()
            .any(|path| path.eq_ignore_ascii_case(&target));
        if !exists {
            if context.extract
                && let Some(stream) = &context.stream
            {
                if !self
                    .extract_image(context, stream, chapter.start_position_ticks, &target)
                    .await?
                {
                    return Ok(false);
                }
                chapter.image_date_modified = last_write_time(&target);
                chapter.image_path = Some(target);
                return Ok(true);
            }
            if chapter
                .image_path
                .as_deref()
                .is_some_and(|path| !path.is_empty())
            {
                chapter.image_path = None;
                return Ok(true);
            }
        } else if !chapter
            .image_path
            .as_deref()
            .is_some_and(|path| path.eq_ignore_ascii_case(&target))
        {
            chapter.image_date_modified = last_write_time(&target);
            chapter.image_path = Some(target);
            return Ok(true);
        } else if !context.enabled {
            chapter.image_path = None;
            return Ok(true);
        }
        Ok(false)
    }

    async fn extract_image(
        &self,
        context: &RefreshContext<'_>,
        stream: &MediaStream,
        position: i64,
        target: &str,
    ) -> Result<bool, ServiceError> {
        if let Some(directory) = Path::new(target).parent() {
            std::fs::create_dir_all(directory)
                .map_err(|error| ServiceError::backend(error.to_string()))?;
        }
        let video = context.video;
        let data = crate::item_data::parse_data(video.data.as_deref());
        let container = data
            .get("Container")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let source = MediaSourceInfo {
            video_type: data
                .get("VideoType")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .or(Some(VideoType::VideoFile)),
            iso_type: data
                .get("IsoType")
                .and_then(|value| serde_json::from_value(value.clone()).ok()),
            protocol: data
                .get("PathProtocol")
                .and_then(|value| serde_json::from_value::<MediaProtocol>(value.clone()).ok())
                .unwrap_or_default(),
            ..MediaSourceInfo::default()
        };
        let threed = data
            .get("Video3DFormat")
            .and_then(|value| serde_json::from_value::<Video3DFormat>(value.clone()).ok());
        let offset = if position == 0 {
            (15 * TICKS_PER_SECOND).min(video.run_time_ticks.unwrap_or(0))
        } else {
            position
        };
        let Some(frame) = context
            .cancel
            .unless_cancelled(self.encoder.extract_video_image(
                video.path.as_deref().unwrap_or_default(),
                container,
                &source,
                stream,
                threed,
                Some(offset),
            ))
            .await
        else {
            // Source catches cancellation thrown inside the encoder as an
            // extraction failure, then reconciles/saves earlier completed frames.
            return Err(ServiceError::backend("chapter image extraction cancelled"));
        };
        let frame = frame?;
        std::fs::copy(&frame, target).map_err(|error| ServiceError::backend(error.to_string()))?;
        if let Err(error) = std::fs::remove_file(&frame) {
            tracing::warn!(%frame, %error, "cannot delete temporary chapter image");
        }
        Ok(true)
    }
}

fn eligible_video(video: &BaseItemEntity, is_active_recording: bool) -> bool {
    let data = crate::item_data::parse_data(video.data.as_deref());
    let flag = |name| {
        data.get(name)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    let shortcut = video.path.as_deref().is_some_and(|path| {
        Path::new(path)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("strm"))
    });
    let channel = video
        .channel_id
        .as_deref()
        .and_then(|id| Uuid::parse_str(id).ok())
        .is_some_and(|id| !id.is_nil());
    let livestream = video
        .tags
        .as_deref()
        .into_iter()
        .flat_map(|tags| tags.split('|'))
        .any(|tag| tag.eq_ignore_ascii_case("livestream"));
    !(video.is_virtual_item
        || flag("IsPlaceHolder")
        || flag("IsShortcut")
        || shortcut
        || is_active_recording
        || channel && livestream)
}

fn average_chapter_duration(chapters: &[ChapterInfo]) -> i64 {
    if chapters.len() < 2 {
        return 0;
    }
    let sum = chapters.windows(2).fold(0_i64, |sum, pair| {
        sum.saturating_add(
            pair[1]
                .start_position_ticks
                .saturating_sub(pair[0].start_position_ticks),
        )
    });
    sum / i64::try_from(chapters.len()).unwrap_or(i64::MAX)
}

pub(crate) fn date_modified_ticks(date: Option<DateTime<Utc>>) -> i64 {
    date.map_or(0, |date| {
        621_355_968_000_000_000
            + date.timestamp() * TICKS_PER_SECOND
            + i64::from(date.timestamp_subsec_nanos() / 100)
    })
}

fn last_write_time(path: &str) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or_default()
}

fn saved_images(directory: &str) -> Vec<String> {
    match std::fs::read_dir(directory) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .map(|entry| entry.path().to_string_lossy().into_owned())
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            tracing::warn!(%directory, %error, "cannot list saved chapter images");
            Vec::new()
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
    use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
    use ferrofin_model::entities::CollectionTypeOptions;
    use ferrofin_traits::media_encoding::MediaInfoRequest;
    use ferrofin_traits::persistence::ItemPersistenceService;

    use super::*;
    use crate::db_error::media_stream_type_to_disc;
    use crate::test_support::test_db;

    #[derive(Default)]
    pub(crate) struct RecordingEncoder {
        pub(crate) calls: Mutex<Vec<(i64, String, MediaSourceInfo, MediaStream)>>,
        fail_on: Option<usize>,
        missing_frame: bool,
        cancel_on: Option<(usize, ScanCancel)>,
        cancel_after: Option<(usize, ScanCancel)>,
    }

    impl RecordingEncoder {
        pub(crate) fn cancelling_after(call: usize, cancel: ScanCancel) -> Self {
            Self {
                cancel_after: Some((call, cancel)),
                ..Default::default()
            }
        }
        pub(crate) fn cancelling_on(call: usize, cancel: ScanCancel) -> Self {
            Self {
                cancel_on: Some((call, cancel)),
                ..Default::default()
            }
        }
    }

    #[async_trait]
    impl MediaEncoder for RecordingEncoder {
        fn encoder_path(&self) -> String {
            "ffmpeg".to_owned()
        }
        fn probe_path(&self) -> String {
            "ffprobe".to_owned()
        }
        async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn get_media_info(
            &self,
            _: &MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            unimplemented!("not a probe")
        }
        async fn extract_audio_image(
            &self,
            _: &str,
            _: Option<i32>,
        ) -> Result<String, ServiceError> {
            unimplemented!("video fixture")
        }
        async fn extract_video_image(
            &self,
            input: &str,
            container: &str,
            source: &MediaSourceInfo,
            stream: &MediaStream,
            _: Option<Video3DFormat>,
            offset: Option<i64>,
        ) -> Result<String, ServiceError> {
            let count = {
                let mut calls = self.calls.lock().unwrap();
                calls.push((
                    offset.unwrap(),
                    container.to_owned(),
                    source.clone(),
                    stream.clone(),
                ));
                calls.len()
            };
            if let Some((call, cancel)) = &self.cancel_on
                && *call == count
            {
                cancel.cancel();
                std::future::pending::<()>().await;
            }
            if self.fail_on == Some(count) {
                return Err(ServiceError::backend("broken frame"));
            }
            let path = format!("{input}.frame-{count}.jpg");
            if !self.missing_frame {
                std::fs::write(&path, b"frame").unwrap();
            }
            if let Some((call, cancel)) = &self.cancel_after
                && *call == count
            {
                cancel.cancel();
            }
            Ok(path)
        }
        fn get_input_argument(&self, input: &str, _: &MediaSourceInfo) -> String {
            input.to_owned()
        }
        fn get_time_parameter(&self, ticks: i64) -> String {
            ticks.to_string()
        }
        async fn convert_image(&self, _: &str, _: &str) -> Result<(), ServiceError> {
            unimplemented!("video fixture")
        }
    }

    pub(crate) struct Fixture {
        temp: tempfile::TempDir,
        pub(crate) db: ferrofin_db::Database,
        pub(crate) folders: Arc<crate::FerrofinVirtualFolderManager>,
        pub(crate) streams: Arc<crate::FerrofinMediaStreamRepository>,
        pub(crate) encoder: Arc<RecordingEncoder>,
        pub(crate) extractor: Arc<ChapterImageExtractor>,
        pub(crate) video: BaseItemEntity,
        pub(crate) options: LibraryOptions,
        pub(crate) paths: Arc<crate::FerrofinPathManager>,
    }

    impl Fixture {
        pub(crate) async fn new(encoder: RecordingEncoder) -> Self {
            let db = test_db().await;
            let temp = tempfile::tempdir().unwrap();
            let media = temp.path().join("media");
            std::fs::create_dir_all(&media).unwrap();
            let persistence = Arc::new(crate::FerrofinItemPersistenceService::new(db.clone()));
            let folders = Arc::new(
                crate::FerrofinVirtualFolderManager::new(temp.path().join("libraries"))
                    .with_item_store(persistence.clone()),
            );
            let options = LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                enable_chapter_image_extraction: true,
                ..Default::default()
            };
            folders
                .add_virtual_folder("Videos", Some(CollectionTypeOptions::movies), &options)
                .await
                .unwrap();
            let owning = folders.get_virtual_folders().await.unwrap()[0]
                .item_id
                .clone();
            let path = media.join("film.mkv");
            std::fs::write(&path, b"video").unwrap();
            let id = Uuid::from_u128(0xC28);
            let video = BaseItemEntity {
                id: ferrofin_db::store::guid_to_db(id),
                type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
                media_type: Some("Video".to_owned()),
                path: Some(path.to_string_lossy().into_owned()),
                top_parent_id: owning,
                run_time_ticks: Some(600_000_000),
                date_modified: Some(DateTime::from_timestamp(1_700_000_000, 123_456_700).unwrap()),
                data: Some(
                    r#"{"Container":"mkv","VideoType":"VideoFile","DefaultVideoStreamIndex":2}"#
                        .to_owned(),
                ),
                ..Default::default()
            };
            persistence
                .save_items(std::slice::from_ref(&video))
                .await
                .unwrap();
            let streams = Arc::new(crate::FerrofinMediaStreamRepository::new(db.clone()));
            streams
                .save_media_streams(
                    id,
                    &[MediaStreamInfoEntity {
                        item_id: ferrofin_db::store::guid_to_db(id),
                        stream_index: 2,
                        codec: Some("h264".to_owned()),
                        width: Some(320),
                        height: Some(240),
                        stream_type: media_stream_type_to_disc(MediaStreamType::Video),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            let app_paths = Arc::new(crate::FerrofinServerApplicationPaths::new(
                temp.path().join("data"),
                temp.path().join("log"),
                temp.path().join("config"),
                temp.path().join("cache"),
                temp.path().join("web"),
            ));
            let paths = Arc::new(crate::FerrofinPathManager::new(app_paths));
            let encoder = Arc::new(encoder);
            let extractor = Arc::new(ChapterImageExtractor::new(
                folders.clone(),
                streams.clone(),
                encoder.clone(),
                paths.clone(),
            ));
            Self {
                temp,
                db,
                folders,
                streams,
                encoder,
                extractor,
                video,
                options,
                paths,
            }
        }
        pub(crate) fn target(&self, position: i64) -> String {
            self.paths.chapter_image_path(
                Uuid::parse_str(&self.video.id).unwrap(),
                self.video.path.as_deref().unwrap(),
                position,
                date_modified_ticks(self.video.date_modified),
            )
        }
        pub(crate) async fn refresh(
            &self,
            chapters: &mut [ChapterInfo],
            extract: bool,
        ) -> ChapterImageRefresh {
            self.extractor
                .refresh(&self.video, chapters, extract, &ScanCancel::default())
                .await
                .unwrap()
        }
    }

    pub(crate) fn chapters(positions: &[i64]) -> Vec<ChapterInfo> {
        positions
            .iter()
            .map(|position| ChapterInfo {
                start_position_ticks: *position,
                ..Default::default()
            })
            .collect()
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn live_library_flags_reuse_clear_and_invalidate_chapter_images() {
        let mut fixture = Fixture::new(RecordingEncoder::default()).await;
        fixture.options.enable_chapter_image_extraction = false;
        fixture
            .folders
            .update_library_options("Videos", &fixture.options)
            .await
            .unwrap();
        let mut chapters = chapters(&[0, 550_000_000]);
        assert!(!fixture.refresh(&mut chapters, true).await.changed);
        assert!(fixture.encoder.calls.lock().unwrap().is_empty());
        fixture.options.enable_chapter_image_extraction = true;
        fixture
            .folders
            .update_library_options("Videos", &fixture.options)
            .await
            .unwrap();
        let outcome = fixture.refresh(&mut chapters, true).await;
        assert!(outcome.success && outcome.changed);
        let calls = fixture.encoder.calls.lock().unwrap().clone();
        assert_eq!(
            calls.iter().map(|call| call.0).collect::<Vec<_>>(),
            [150_000_000, 550_000_000]
        );
        assert_eq!(calls[0].1, "mkv");
        assert_eq!(calls[0].2.protocol, MediaProtocol::File);
        assert_eq!(calls[0].2.video_type, Some(VideoType::VideoFile));
        assert_eq!(calls[0].3.index, 2);
        assert_eq!(calls[0].3.width, Some(320));
        let old_images = chapters
            .iter()
            .map(|chapter| chapter.image_path.clone().unwrap())
            .collect::<Vec<_>>();
        for chapter in &chapters {
            assert_eq!(
                chapter.image_date_modified,
                last_write_time(chapter.image_path.as_deref().unwrap())
            );
        }
        assert!(old_images[0].ends_with("/638355968001234567_0.jpg"));
        assert!(
            !Path::new(&format!(
                "{}.frame-1.jpg",
                fixture.video.path.as_deref().unwrap()
            ))
            .exists()
        );
        let directory = Path::new(&old_images[0]).parent().unwrap();
        let orphan = directory.join("orphan.PNG");
        let other = directory.join("notes.txt");
        std::fs::write(&orphan, b"old image").unwrap();
        std::fs::write(&other, b"keep").unwrap();
        let cached = fixture.refresh(&mut chapters, true).await;
        assert!(!cached.changed);
        cached.prune_unused_images(&chapters);
        assert_eq!(fixture.encoder.calls.lock().unwrap().len(), 2);
        assert!(!orphan.exists());
        assert!(other.exists());
        fixture.video.date_modified = fixture
            .video
            .date_modified
            .map(|date| date + chrono::Duration::seconds(1));
        let updated = fixture.refresh(&mut chapters, true).await;
        assert!(updated.changed);
        updated.prune_unused_images(&chapters);
        assert!(old_images.iter().all(|path| !Path::new(path).exists()));
        let new_images = chapters
            .iter()
            .map(|chapter| chapter.image_path.clone().unwrap())
            .collect::<Vec<_>>();
        fixture.options.enable_chapter_image_extraction = false;
        fixture
            .folders
            .update_library_options("Videos", &fixture.options)
            .await
            .unwrap();
        let disabled = fixture.refresh(&mut chapters, true).await;
        assert!(disabled.success && disabled.changed);
        assert!(chapters.iter().all(|chapter| chapter.image_path.is_none()));
        disabled.prune_unused_images(&chapters);
        assert!(new_images.iter().all(|path| !Path::new(path).exists()));
        assert_eq!(fixture.encoder.calls.lock().unwrap().len(), 4);
        assert!(fixture.temp.path().exists());
    }

    #[tokio::test]
    async fn reconciliation_adopts_existing_images_even_without_scan_extraction() {
        let mut fixture = Fixture::new(RecordingEncoder::default()).await;
        let mut chapters = chapters(&[0]);
        let image = fixture.target(0);
        std::fs::create_dir_all(Path::new(&image).parent().unwrap()).unwrap();
        std::fs::write(&image, b"saved image").unwrap();
        let adopted = fixture.refresh(&mut chapters, false).await;
        assert!(adopted.changed);
        assert_eq!(chapters[0].image_path.as_deref(), Some(image.as_str()));
        assert!(fixture.encoder.calls.lock().unwrap().is_empty());
        chapters[0].image_path = Some(image.to_uppercase());
        assert!(!fixture.refresh(&mut chapters, false).await.changed);
        fixture.options.enable_chapter_image_extraction = false;
        fixture
            .folders
            .update_library_options("Videos", &fixture.options)
            .await
            .unwrap();
        chapters[0].image_path = None;
        let first = fixture.refresh(&mut chapters, false).await;
        assert!(
            first.changed,
            "upstream adopts an existing canonical target first"
        );
        first.prune_unused_images(&chapters);
        assert!(Path::new(&image).exists());
        let second = fixture.refresh(&mut chapters, false).await;
        second.prune_unused_images(&chapters);
        assert!(chapters[0].image_path.is_none());
        assert!(!Path::new(&image).exists());
    }

    #[tokio::test]
    async fn nested_library_and_explicit_missing_default_video_prevent_extraction() {
        let mut fixture = Fixture::new(RecordingEncoder::default()).await;
        let inner = Path::new(fixture.video.path.as_deref().unwrap())
            .parent()
            .unwrap()
            .join("nested");
        std::fs::create_dir_all(&inner).unwrap();
        fixture.video.path = Some(inner.join("film.mkv").to_string_lossy().into_owned());
        fixture.video.top_parent_id = None;
        fixture
            .folders
            .add_virtual_folder(
                "Inner",
                Some(CollectionTypeOptions::movies),
                &LibraryOptions {
                    path_infos: vec![MediaPathInfo {
                        path: inner.to_string_lossy().into_owned(),
                    }],
                    enable_chapter_image_extraction: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut chapters = chapters(&[0]);
        assert!(!fixture.refresh(&mut chapters, true).await.changed);
        assert!(fixture.encoder.calls.lock().unwrap().is_empty());
        fixture.video.top_parent_id = fixture
            .folders
            .get_virtual_folders()
            .await
            .unwrap()
            .into_iter()
            .find(|folder| folder.name.as_deref() == Some("Videos"))
            .unwrap()
            .item_id;
        fixture.video.data = Some(r#"{"DefaultVideoStreamIndex":null}"#.to_owned());
        chapters[0].image_path = Some("missing.jpg".to_owned());
        assert!(fixture.refresh(&mut chapters, true).await.changed);
        assert!(chapters[0].image_path.is_none());
        fixture.video.data = Some(r#"{"DefaultVideoStreamIndex":99}"#.to_owned());
        assert!(!fixture.refresh(&mut chapters, true).await.changed);
        fixture.video.data = None;
        fixture
            .streams
            .save_media_streams(Uuid::parse_str(&fixture.video.id).unwrap(), &[])
            .await
            .unwrap();
        assert!(!fixture.refresh(&mut chapters, true).await.changed);
        assert!(fixture.encoder.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn omitted_or_null_default_video_index_reuses_existing_images_without_extraction() {
        let mut fixture = Fixture::new(RecordingEncoder::default()).await;
        let target = fixture.target(0);
        std::fs::create_dir_all(Path::new(&target).parent().unwrap()).unwrap();
        std::fs::write(&target, b"cached frame").unwrap();
        for data in [None, Some(r#"{"DefaultVideoStreamIndex":null}"#.to_owned())] {
            fixture.video.data = data;
            let mut rows = chapters(&[0, 200_000_000]);
            rows[0].image_path = Some(target.clone());
            let outcome = fixture.refresh(&mut rows, true).await;
            assert!(outcome.success && !outcome.changed);
            outcome.prune_unused_images(&rows);
            assert_eq!(rows[0].image_path.as_deref(), Some(target.as_str()));
            assert!(Path::new(&target).exists());
            assert!(rows[1].image_path.is_none());
            assert!(
                fixture.encoder.calls.lock().unwrap().is_empty(),
                "source requires an explicit default stream index"
            );
        }
    }

    #[tokio::test]
    async fn pre_cancelled_extraction_stops_before_encoding_but_cached_rows_reconcile() {
        let fixture = Fixture::new(RecordingEncoder::default()).await;
        let target = fixture.target(0);
        std::fs::create_dir_all(Path::new(&target).parent().unwrap()).unwrap();
        std::fs::write(&target, b"cached").unwrap();
        let cancel = ScanCancel::new();
        cancel.cancel();

        let mut cached = chapters(&[0]);
        let outcome = fixture
            .extractor
            .refresh(&fixture.video, &mut cached, true, &cancel)
            .await
            .unwrap();
        assert!(outcome.success && outcome.changed && !outcome.cancelled);
        assert_eq!(cached[0].image_path.as_deref(), Some(target.as_str()));
        outcome.prune_unused_images(&cached);
        assert!(Path::new(&target).exists());

        let mut uncached = chapters(&[200_000_000]);
        let outcome = fixture
            .extractor
            .refresh(&fixture.video, &mut uncached, true, &cancel)
            .await
            .unwrap();
        assert!(!outcome.success && !outcome.changed && outcome.cancelled);
        outcome.prune_unused_images(&uncached);
        assert!(
            Path::new(&target).exists(),
            "pre-extraction cancellation skips pruning"
        );
        assert!(fixture.encoder.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn chapter_density_and_runtime_boundaries_match_the_pinned_rules() {
        let mut fixture = Fixture::new(RecordingEncoder::default()).await;
        let mut dense = chapters(&[0, 15_000_000]);
        assert!(!fixture.refresh(&mut dense, true).await.changed);
        assert!(fixture.encoder.calls.lock().unwrap().is_empty());
        fixture.video.run_time_ticks = Some(30_000_000);
        let mut sparse = chapters(&[0, 20_000_000, 30_000_000, 10_000_000]);
        // The unsorted final pair lowers the source's count-divided average.
        assert!(average_chapter_duration(&sparse) < TICKS_PER_SECOND);
        sparse.pop();
        let outcome = fixture.refresh(&mut sparse, true).await;
        assert!(outcome.success && outcome.changed);
        assert_eq!(
            fixture
                .encoder
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| call.0)
                .collect::<Vec<_>>(),
            [30_000_000, 20_000_000]
        );
        assert!(sparse[2].image_path.is_none());
        fixture.video.run_time_ticks = None;
        assert!(!fixture.refresh(&mut sparse, true).await.changed);
        assert!(!fixture.refresh(&mut [], true).await.changed);
    }

    #[tokio::test]
    async fn extraction_failure_preserves_earlier_chapters_and_stops_the_video() {
        let fixture = Fixture::new(RecordingEncoder {
            fail_on: Some(2),
            ..Default::default()
        })
        .await;
        let mut chapters = chapters(&[0, 200_000_000, 400_000_000]);
        let outcome = fixture.refresh(&mut chapters, true).await;
        assert!(!outcome.success && outcome.changed);
        assert!(chapters[0].image_path.is_some());
        assert!(
            chapters[1..]
                .iter()
                .all(|chapter| chapter.image_path.is_none())
        );
        assert_eq!(fixture.encoder.calls.lock().unwrap().len(), 2);
        let retry = fixture.refresh(&mut chapters, false).await;
        assert!(retry.success && !retry.changed);
        let missing = Fixture::new(RecordingEncoder {
            missing_frame: true,
            ..Default::default()
        })
        .await;
        let mut one = super::tests::chapters(&[0]);
        assert!(!missing.refresh(&mut one, true).await.success);
        assert!(one[0].image_path.is_none());
    }

    #[tokio::test]
    async fn a_failed_video_still_reconciles_images_and_disabled_task_prunes_them() {
        use crate::scheduled_tasks::ScheduledTask;
        use ferrofin_traits::chapters::ChapterManager;
        let mut fixture = Fixture::new(RecordingEncoder {
            fail_on: Some(2),
            ..Default::default()
        })
        .await;
        let library = crate::test_support::library_manager_over(fixture.db.clone());
        let manager = Arc::new(crate::FerrofinChapterManager::new(
            Arc::new(crate::FerrofinChapterRepository::new(fixture.db.clone())),
            library.clone(),
        ));
        let id = Uuid::parse_str(&fixture.video.id).unwrap();
        manager
            .save_chapters(id, &chapters(&[0, 200_000_000, 400_000_000]))
            .await
            .unwrap();
        let app_paths = Arc::new(crate::FerrofinServerApplicationPaths::new(
            fixture.temp.path().join("data"),
            fixture.temp.path().join("log"),
            fixture.temp.path().join("config"),
            fixture.temp.path().join("cache"),
            fixture.temp.path().join("web"),
        ));
        let task = crate::scheduled_tasks::library::ChapterImagesTask::new(
            library,
            fixture.folders.clone(),
            manager.clone(),
            fixture.extractor.clone(),
            app_paths,
        );
        task.execute(&crate::scheduled_tasks::TaskProgress::default())
            .await
            .unwrap();
        let saved = manager.get_chapters(id).await.unwrap();
        assert!(saved[0].image_path.is_some());
        assert!(saved[1].image_path.is_none());
        let history = fixture.temp.path().join("cache/chapter-failures.txt");
        let failure = std::fs::read_to_string(&history).unwrap();
        assert!(failure.contains("638355968001234567"));
        let adopted = fixture.target(200_000_000);
        std::fs::write(&adopted, b"existing target").unwrap();
        task.execute(&crate::scheduled_tasks::TaskProgress::default())
            .await
            .unwrap();
        assert_eq!(
            fixture.encoder.calls.lock().unwrap().len(),
            2,
            "failure history only suppresses extraction"
        );
        assert_eq!(
            manager.get_chapters(id).await.unwrap()[1]
                .image_path
                .as_deref(),
            Some(adopted.as_str())
        );
        assert_eq!(std::fs::read_to_string(&history).unwrap(), failure);
        fixture.options.enable_chapter_image_extraction = false;
        fixture
            .folders
            .update_library_options("Videos", &fixture.options)
            .await
            .unwrap();
        task.execute(&crate::scheduled_tasks::TaskProgress::default())
            .await
            .unwrap();
        assert!(
            manager
                .get_chapters(id)
                .await
                .unwrap()
                .iter()
                .all(|chapter| chapter.image_path.is_none())
        );
        assert!(!Path::new(&adopted).exists());
    }

    #[test]
    fn eligibility_rejects_placeholders_shortcuts_and_incomplete_media() {
        let mut video = BaseItemEntity {
            path: Some("film.mkv".to_owned()),
            ..Default::default()
        };
        assert!(eligible_video(&video, false));
        assert!(!eligible_video(&video, true));
        for data in [r#"{"IsPlaceHolder":true}"#, r#"{"IsShortcut":true}"#] {
            video.data = Some(data.to_owned());
            assert!(!eligible_video(&video, false));
        }
        video.data = None;
        video.path = Some("film.STRM".to_owned());
        assert!(!eligible_video(&video, false));
        video.path = Some("film.mkv".to_owned());
        video.tags = Some("genre|LiVeStReAm".to_owned());
        assert!(eligible_video(&video, false));
        video.channel_id = Some(Uuid::from_u128(1).to_string());
        assert!(!eligible_video(&video, false));
        video.channel_id = Some(Uuid::nil().to_string());
        assert!(eligible_video(&video, false));
        video.is_virtual_item = true;
        assert!(!eligible_video(&video, false));
    }
}
