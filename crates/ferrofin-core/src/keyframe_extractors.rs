//! The keyframe extractors and the cache in front of them.
//!
//! Ports of `Jellyfin.MediaEncoding.Hls`:
//! - [`MatroskaKeyframeExtractor`] — `Extractors.MatroskaKeyframeExtractor`;
//! - [`FfProbeKeyframeExtractor`] — `Extractors.FfProbeKeyframeExtractor`;
//! - [`CachedKeyframeExtractor`] — `Cache.CacheDecorator`;
//! - [`keyframe_extractors`] — the registrations in
//!   `Extensions.MediaEncodingHlsServiceCollectionExtensions.AddHlsPlaylistGenerator`.
//!
//! The extractors read files and run ffprobe, so both work on the blocking
//! pool. Upstream logs an extraction failure at error level; here it is
//! per-item detail (LOGGING.md, rule 1), so it goes to debug: the caller falls
//! back, to the next extractor or to equal-length segments.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::KeyframeDataEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_keyframes::error::KeyframesError;
use ferrofin_keyframes::keyframe_data::KeyframeData;
use ferrofin_naming::common::NamingOptions;
use ferrofin_traits::media_encoding::{KeyframeExtractor, MediaEncoder};
use ferrofin_traits::persistence::KeyframeRepository;
use uuid::Uuid;

/// Runs `extract` on the blocking pool and keeps a non-empty result, as both
/// upstream extractors return `KeyframeTicks.Count > 0`.
async fn extract_on_blocking_pool(
    extractor: &'static str,
    file_path: &str,
    extract: impl FnOnce() -> Result<KeyframeData, KeyframesError> + Send + 'static,
) -> Option<KeyframeData> {
    match tokio::task::spawn_blocking(extract).await {
        Ok(Ok(data)) => (!data.keyframe_ticks.is_empty()).then_some(data),
        Ok(Err(error)) => {
            tracing::debug!(extractor, path = file_path, error = %error, "extracting keyframes failed");
            None
        }
        Err(error) => {
            // A panic in the parser is a bug, and must stay visible (LOGGING.md, rule 7).
            tracing::error!(
                task = "keyframe extraction",
                extractor,
                path = file_path,
                error = %error,
                "keyframe extraction panicked"
            );
            None
        }
    }
}

/// Port of `MatroskaKeyframeExtractor`: reads a `.mkv` file's cue index.
#[derive(Debug, Default, Clone, Copy)]
pub struct MatroskaKeyframeExtractor;

#[async_trait]
impl KeyframeExtractor for MatroskaKeyframeExtractor {
    fn is_metadata_based(&self) -> bool {
        true
    }

    async fn try_extract_keyframes(&self, _item_id: Uuid, file_path: &str) -> Option<KeyframeData> {
        // `filePath.EndsWith(".mkv", OrdinalIgnoreCase)`.
        let is_mkv = file_path
            .len()
            .checked_sub(4)
            .and_then(|start| file_path.get(start..))
            .is_some_and(|tail| tail.eq_ignore_ascii_case(".mkv"));
        if !is_mkv {
            return None;
        }

        let path = file_path.to_owned();
        extract_on_blocking_pool("matroska", file_path, move || {
            ferrofin_keyframes::matroska::get_keyframe_data(&path)
        })
        .await
    }
}

/// Port of `FfProbeKeyframeExtractor`: runs ffprobe over a video file.
pub struct FfProbeKeyframeExtractor {
    encoder: Arc<dyn MediaEncoder>,
    video_file_extensions: Vec<String>,
}

impl std::fmt::Debug for FfProbeKeyframeExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfProbeKeyframeExtractor")
            .finish_non_exhaustive()
    }
}

impl FfProbeKeyframeExtractor {
    /// An extractor running `encoder`'s ffprobe over files with one of the
    /// naming options' video extensions.
    #[must_use]
    pub fn new(encoder: Arc<dyn MediaEncoder>, naming_options: &NamingOptions) -> Self {
        Self {
            encoder,
            video_file_extensions: naming_options.video_file_extensions.clone(),
        }
    }
}

#[async_trait]
impl KeyframeExtractor for FfProbeKeyframeExtractor {
    fn is_metadata_based(&self) -> bool {
        false
    }

    async fn try_extract_keyframes(&self, _item_id: Uuid, file_path: &str) -> Option<KeyframeData> {
        // `VideoFileExtensions.Contains(Path.GetExtension(filePath), OrdinalIgnoreCase)`.
        let extension = Path::new(file_path)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))?;
        if !self
            .video_file_extensions
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&extension))
        {
            return None;
        }

        let probe_path = self.encoder.probe_path();
        let path = file_path.to_owned();
        extract_on_blocking_pool("ffprobe", file_path, move || {
            ferrofin_keyframes::ff_probe::get_keyframe_data(&probe_path, &path)
        })
        .await
    }
}

/// Port of `CacheDecorator`: answers from the keyframe repository, and stores
/// what the wrapped extractor extracts.
pub struct CachedKeyframeExtractor {
    repository: Arc<dyn KeyframeRepository>,
    extractor: Arc<dyn KeyframeExtractor>,
    extractor_name: &'static str,
}

impl std::fmt::Debug for CachedKeyframeExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedKeyframeExtractor")
            .field("extractor_name", &self.extractor_name)
            .finish_non_exhaustive()
    }
}

impl CachedKeyframeExtractor {
    /// `extractor` behind `repository`. `extractor_name` names it in the log,
    /// as upstream logs the wrapped type's name.
    #[must_use]
    pub fn new(
        repository: Arc<dyn KeyframeRepository>,
        extractor: Arc<dyn KeyframeExtractor>,
        extractor_name: &'static str,
    ) -> Self {
        Self {
            repository,
            extractor,
            extractor_name,
        }
    }

    /// The item's stored keyframes (`GetKeyframeData(itemId).FirstOrDefault()`).
    ///
    /// A stored row without ticks counts as absent. Upstream never stores one,
    /// but Ferrofin's Keyframe Extractor task used to store ffprobe's result
    /// even when it held no keyframes, and serving that row would turn a
    /// whole file into one segment.
    async fn stored(&self, item_id: Uuid) -> Option<KeyframeData> {
        // Upstream lets a repository failure throw, which fails the Keyframe
        // Extractor task. The trait has no error channel, so a failed read
        // extracts again and a failed save still returns what was extracted.
        // Both are per item, so they log at debug (LOGGING.md, rule 1).
        let rows = match self.repository.get_keyframe_data(item_id).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::debug!(item_id = %item_id, error = %error, "reading stored keyframes failed; extracting them again");
                return None;
            }
        };
        let row = rows.into_iter().next()?;
        let ticks: Vec<i64> = match row.keyframe_ticks.as_deref().map(serde_json::from_str) {
            Some(Ok(ticks)) => ticks,
            Some(Err(error)) => {
                tracing::debug!(item_id = %item_id, error = %error, "stored keyframes are not a list of ticks");
                return None;
            }
            None => Vec::new(),
        };
        (!ticks.is_empty()).then(|| KeyframeData::new(row.total_duration, ticks))
    }
}

#[async_trait]
impl KeyframeExtractor for CachedKeyframeExtractor {
    fn is_metadata_based(&self) -> bool {
        self.extractor.is_metadata_based()
    }

    async fn try_extract_keyframes(&self, item_id: Uuid, file_path: &str) -> Option<KeyframeData> {
        if let Some(data) = self.stored(item_id).await {
            return Some(data);
        }

        let Some(data) = self
            .extractor
            .try_extract_keyframes(item_id, file_path)
            .await
        else {
            tracing::debug!(
                item_id = %item_id,
                extractor = self.extractor_name,
                "failed to extract keyframes"
            );
            return None;
        };
        tracing::debug!(item_id = %item_id, extractor = self.extractor_name, "extracted keyframes");

        match serde_json::to_string(&data.keyframe_ticks) {
            Ok(ticks) => {
                let entity = KeyframeDataEntity {
                    item_id: guid_to_db(item_id),
                    keyframe_ticks: Some(ticks),
                    total_duration: data.total_duration,
                };
                if let Err(error) = self.repository.save_keyframe_data(item_id, &entity).await {
                    tracing::debug!(item_id = %item_id, error = %error, "storing extracted keyframes failed");
                }
            }
            Err(error) => {
                tracing::debug!(item_id = %item_id, error = %error, "storing extracted keyframes failed");
            }
        }
        Some(data)
    }
}

/// `AddHlsPlaylistGenerator`'s extractors, in its registration order:
/// ffprobe, then Matroska, each behind the keyframe-repository cache.
///
/// The playlist generator keeps the metadata-based one; the Keyframe
/// Extractor task tries it first.
#[must_use]
pub fn keyframe_extractors(
    repository: Arc<dyn KeyframeRepository>,
    encoder: Arc<dyn MediaEncoder>,
) -> Vec<Arc<dyn KeyframeExtractor>> {
    let ffprobe = FfProbeKeyframeExtractor::new(encoder, &NamingOptions::new());
    vec![
        Arc::new(CachedKeyframeExtractor::new(
            Arc::clone(&repository),
            Arc::new(ffprobe),
            "FfProbeKeyframeExtractor",
        )),
        Arc::new(CachedKeyframeExtractor::new(
            repository,
            Arc::new(MatroskaKeyframeExtractor),
            "MatroskaKeyframeExtractor",
        )),
    ]
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::dto::MediaSourceInfo;
    use ferrofin_model::entities_media::MediaStream;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::media_encoding::MediaInfoRequest;

    use super::*;
    use crate::FerrofinKeyframeRepository;
    use crate::test_support::{seed_item, test_db};

    /// An encoder whose ffprobe is `probe_path`; nothing else is used.
    struct ProbeEncoder {
        probe_path: String,
    }

    #[async_trait]
    impl MediaEncoder for ProbeEncoder {
        fn encoder_path(&self) -> String {
            "ffmpeg".to_owned()
        }
        fn probe_path(&self) -> String {
            self.probe_path.clone()
        }
        async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn get_media_info(
            &self,
            _request: &MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            unreachable!("not used in these tests")
        }
        async fn extract_audio_image(
            &self,
            _path: &str,
            _image_stream_index: Option<i32>,
        ) -> Result<String, ServiceError> {
            unreachable!("not used in these tests")
        }
        async fn extract_video_image(
            &self,
            _input_file: &str,
            _container: &str,
            _media_source: &MediaSourceInfo,
            _video_stream: &MediaStream,
            _threed_format: Option<ferrofin_model::entities::Video3DFormat>,
            _offset_ticks: Option<i64>,
        ) -> Result<String, ServiceError> {
            unreachable!("not used in these tests")
        }
        fn get_input_argument(&self, input_file: &str, _media_source: &MediaSourceInfo) -> String {
            input_file.to_owned()
        }
        fn get_time_parameter(&self, _ticks: i64) -> String {
            String::new()
        }
        async fn convert_image(
            &self,
            _input_path: &str,
            _output_path: &str,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// An ffprobe that ignores its arguments and prints keyframes at 0 s and
    /// 4.2 s of a 10 s stream.
    fn fake_ffprobe(dir: &Path) -> String {
        let script = dir.join("ffprobe");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' 'packet,0.000000,K_' 'packet,4.200000,K_' 'stream,10.000000'\n",
        )
        .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        script.to_string_lossy().into_owned()
    }

    /// An extractor that answers `answer` and counts its calls.
    struct CountingExtractor {
        answer: Option<KeyframeData>,
        calls: Mutex<u32>,
    }

    impl CountingExtractor {
        fn new(answer: Option<KeyframeData>) -> Arc<Self> {
            Arc::new(Self {
                answer,
                calls: Mutex::new(0),
            })
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().expect("calls")
        }
    }

    #[async_trait]
    impl KeyframeExtractor for CountingExtractor {
        fn is_metadata_based(&self) -> bool {
            true
        }

        async fn try_extract_keyframes(
            &self,
            _item_id: Uuid,
            _file_path: &str,
        ) -> Option<KeyframeData> {
            *self.calls.lock().expect("calls") += 1;
            self.answer.clone()
        }
    }

    async fn repository_with_item(item: Uuid) -> Arc<dyn KeyframeRepository> {
        let db = test_db().await;
        seed_item(&db, item, BaseItemKind::Movie).await;
        Arc::new(FerrofinKeyframeRepository::new(db))
    }

    #[tokio::test]
    async fn the_ffprobe_extractor_reads_keyframes_from_a_video_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let extractor = FfProbeKeyframeExtractor::new(
            Arc::new(ProbeEncoder {
                probe_path: fake_ffprobe(dir.path()),
            }),
            &NamingOptions::new(),
        );

        let data = extractor
            .try_extract_keyframes(Uuid::nil(), "/media/Movie (2020).MP4")
            .await
            .expect("keyframes");

        assert!(!extractor.is_metadata_based());
        assert_eq!(data.keyframe_ticks, vec![0, 42_000_000]);
        assert_eq!(data.total_duration, 100_000_000);
    }

    #[tokio::test]
    async fn the_ffprobe_extractor_skips_files_that_are_not_video() {
        let dir = tempfile::tempdir().expect("tempdir");
        let extractor = FfProbeKeyframeExtractor::new(
            Arc::new(ProbeEncoder {
                probe_path: fake_ffprobe(dir.path()),
            }),
            &NamingOptions::new(),
        );

        for path in [
            "/media/notes.txt",
            "/media/no-extension",
            "/media/song.flac",
        ] {
            assert_eq!(
                extractor.try_extract_keyframes(Uuid::nil(), path).await,
                None,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn the_ffprobe_extractor_treats_no_keyframes_as_a_failure() {
        let extractor = FfProbeKeyframeExtractor::new(
            Arc::new(ProbeEncoder {
                probe_path: "/bin/true".to_owned(),
            }),
            &NamingOptions::new(),
        );

        assert_eq!(
            extractor
                .try_extract_keyframes(Uuid::nil(), "/media/a.mkv")
                .await,
            None
        );
    }

    #[tokio::test]
    async fn the_matroska_extractor_reads_only_mkv_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let not_matroska = dir.path().join("broken.MKV");
        std::fs::write(&not_matroska, b"not a matroska file").expect("write");

        assert!(MatroskaKeyframeExtractor.is_metadata_based());
        assert_eq!(
            MatroskaKeyframeExtractor
                .try_extract_keyframes(Uuid::nil(), "/media/a.mp4")
                .await,
            None
        );
        // An `.mkv` it cannot read is a failure, not an error.
        assert_eq!(
            MatroskaKeyframeExtractor
                .try_extract_keyframes(Uuid::nil(), &not_matroska.to_string_lossy())
                .await,
            None
        );
    }

    #[tokio::test]
    async fn the_cache_stores_an_extraction_and_serves_it_after() {
        let item = Uuid::from_u128(0xC1);
        let repository = repository_with_item(item).await;
        let inner = CountingExtractor::new(Some(KeyframeData::new(90, vec![0, 30, 60])));
        let cached = CachedKeyframeExtractor::new(
            Arc::clone(&repository),
            Arc::clone(&inner) as Arc<dyn KeyframeExtractor>,
            "CountingExtractor",
        );

        let first = cached.try_extract_keyframes(item, "/media/a.mkv").await;
        let second = cached.try_extract_keyframes(item, "/media/a.mkv").await;

        assert_eq!(first, Some(KeyframeData::new(90, vec![0, 30, 60])));
        assert_eq!(second, first);
        assert_eq!(
            inner.calls(),
            1,
            "the second answer comes from the repository"
        );
        let stored = repository.get_keyframe_data(item).await.expect("stored");
        assert_eq!(stored[0].keyframe_ticks.as_deref(), Some("[0,30,60]"));
        assert_eq!(stored[0].total_duration, 90);
        assert!(cached.is_metadata_based());
    }

    #[tokio::test]
    async fn the_cache_serves_rows_any_extractor_stored() {
        let item = Uuid::from_u128(0xC2);
        let repository = repository_with_item(item).await;
        let row = KeyframeDataEntity {
            item_id: guid_to_db(item),
            keyframe_ticks: Some("[0,5,9]".to_owned()),
            total_duration: 12,
        };
        repository
            .save_keyframe_data(item, &row)
            .await
            .expect("seed");
        let inner = CountingExtractor::new(None);
        let cached = CachedKeyframeExtractor::new(
            repository,
            Arc::clone(&inner) as Arc<dyn KeyframeExtractor>,
            "CountingExtractor",
        );

        let data = cached.try_extract_keyframes(item, "/media/a.mkv").await;

        assert_eq!(data, Some(KeyframeData::new(12, vec![0, 5, 9])));
        assert_eq!(inner.calls(), 0);
    }

    /// Rows with no ticks, which the old task stored, and unreadable rows are
    /// extracted again rather than served.
    #[rstest::rstest]
    #[case(Some("[]"))]
    #[case(None)]
    #[case(Some("not json"))]
    #[tokio::test]
    async fn the_cache_extracts_again_over_a_row_without_ticks(#[case] ticks: Option<&str>) {
        let item = Uuid::from_u128(0xC3);
        let repository = repository_with_item(item).await;
        let row = KeyframeDataEntity {
            item_id: guid_to_db(item),
            keyframe_ticks: ticks.map(str::to_owned),
            total_duration: 12,
        };
        repository
            .save_keyframe_data(item, &row)
            .await
            .expect("seed");
        let inner = CountingExtractor::new(Some(KeyframeData::new(20, vec![0, 10])));
        let cached = CachedKeyframeExtractor::new(
            Arc::clone(&repository),
            Arc::clone(&inner) as Arc<dyn KeyframeExtractor>,
            "CountingExtractor",
        );

        let data = cached.try_extract_keyframes(item, "/media/a.mkv").await;

        assert_eq!(data, Some(KeyframeData::new(20, vec![0, 10])));
        assert_eq!(inner.calls(), 1);
        let stored = repository.get_keyframe_data(item).await.expect("stored");
        assert_eq!(stored[0].keyframe_ticks.as_deref(), Some("[0,10]"));
    }

    #[tokio::test]
    async fn the_cache_stores_nothing_when_extraction_fails() {
        let item = Uuid::from_u128(0xC4);
        let repository = repository_with_item(item).await;
        let cached = CachedKeyframeExtractor::new(
            Arc::clone(&repository),
            CountingExtractor::new(None) as Arc<dyn KeyframeExtractor>,
            "CountingExtractor",
        );

        assert_eq!(
            cached.try_extract_keyframes(item, "/media/a.mkv").await,
            None
        );
        assert!(
            repository
                .get_keyframe_data(item)
                .await
                .expect("read")
                .is_empty()
        );
    }

    /// A keyframe repository whose every call fails.
    struct BrokenRepository;

    #[async_trait]
    impl KeyframeRepository for BrokenRepository {
        async fn get_keyframe_data(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<KeyframeDataEntity>, ServiceError> {
            Err(ServiceError::backend("database is locked"))
        }
        async fn save_keyframe_data(
            &self,
            _item_id: Uuid,
            _data: &KeyframeDataEntity,
        ) -> Result<(), ServiceError> {
            Err(ServiceError::backend("database is locked"))
        }
        async fn delete_keyframe_data(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            Err(ServiceError::backend("database is locked"))
        }
    }

    /// Upstream throws on a repository failure; here a failed read extracts
    /// and a failed save still answers with what was extracted.
    #[tokio::test]
    async fn a_failing_repository_still_answers_with_the_extraction() {
        let inner = CountingExtractor::new(Some(KeyframeData::new(20, vec![0, 10])));
        let cached = CachedKeyframeExtractor::new(
            Arc::new(BrokenRepository),
            Arc::clone(&inner) as Arc<dyn KeyframeExtractor>,
            "CountingExtractor",
        );

        let data = cached
            .try_extract_keyframes(Uuid::nil(), "/media/a.mkv")
            .await;

        assert_eq!(data, Some(KeyframeData::new(20, vec![0, 10])));
        assert_eq!(inner.calls(), 1);
    }

    #[tokio::test]
    async fn the_registered_extractors_are_ffprobe_then_matroska() {
        let db = test_db().await;
        let extractors = keyframe_extractors(
            Arc::new(FerrofinKeyframeRepository::new(db)),
            Arc::new(ProbeEncoder {
                probe_path: "/bin/true".to_owned(),
            }),
        );

        let metadata_based: Vec<bool> = extractors.iter().map(|e| e.is_metadata_based()).collect();
        assert_eq!(metadata_based, vec![false, true]);
    }
}
