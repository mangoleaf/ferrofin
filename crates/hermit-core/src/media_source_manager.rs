//! [`HermitMediaSourceManager`] — the concrete [`MediaSourceManager`].
//!
//! Port of `Emby.Server.Implementations.Library.MediaSourceManager` (the
//! API-facing subset). Responsibilities that carry over to this seam:
//! - **streams/attachments as DTOs:** read the persisted
//!   [`MediaStreamInfoEntity`](hermit_db::entities::base_items::MediaStreamInfoEntity)
//!   / attachment rows through the injected persistence repositories and map them
//!   to the wire [`MediaStream`]/[`MediaAttachment`] DTOs;
//! - **static + playback media sources:** assemble a [`MediaSourceInfo`] for a
//!   playable item from its stored path/container/runtime plus its streams;
//! - **live streams:** hold the open-live-stream table in memory, opening via the
//!   injected [`MediaEncoder`] probe.
//!
//! Injected siblings (`Arc<dyn MediaEncoder>` for probing, `Arc<dyn
//! ProviderManager>` for the metadata this DTO layer would enrich with) are taken
//! by dependency injection, never constructed here — they land at the Wave 8
//! composition root. The `MediaStreamSelector` / `LiveStreamHelper` bitrate and
//! profile negotiation is deferred; a static source is returned as-is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hermit_db::entities::base_items::{
    AttachmentStreamInfoEntity, BaseItemEntity, MediaStreamInfoEntity,
};
use hermit_model::dto::{MediaSourceInfo, MediaSourceType};
use hermit_model::entities_media::{MediaAttachment, MediaStream};
use hermit_model::media_info::{LiveStreamRequest, MediaProtocol};
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::library::MediaSourceManager;
use hermit_traits::media_encoding::MediaEncoder;
use hermit_traits::persistence::{
    ItemRepository, MediaAttachmentQuery, MediaAttachmentRepository, MediaStreamQuery,
    MediaStreamRepository,
};
use hermit_traits::providers::ProviderManager;

use crate::db_error::media_stream_type_from_disc;

/// The concrete media-source manager.
///
/// Holds injected persistence repositories plus the sibling `MediaEncoder` /
/// `ProviderManager` (both by `Arc<dyn _>`), and the in-memory open-live-stream
/// table keyed by the live-stream id.
#[derive(Clone)]
pub struct HermitMediaSourceManager {
    items: Arc<dyn ItemRepository>,
    streams: Arc<dyn MediaStreamRepository>,
    attachments: Arc<dyn MediaAttachmentRepository>,
    encoder: Arc<dyn MediaEncoder>,
    #[allow(dead_code)]
    provider: Arc<dyn ProviderManager>,
    /// Open live streams keyed by their live-stream id. Guarded by a
    /// `std::sync::Mutex` because the guard never spans an `.await`.
    open_streams: Arc<Mutex<HashMap<String, MediaSourceInfo>>>,
}

impl std::fmt::Debug for HermitMediaSourceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitMediaSourceManager")
            .finish_non_exhaustive()
    }
}

impl HermitMediaSourceManager {
    /// Creates a media-source manager over the injected repositories and siblings.
    #[must_use]
    pub fn new(
        items: Arc<dyn ItemRepository>,
        streams: Arc<dyn MediaStreamRepository>,
        attachments: Arc<dyn MediaAttachmentRepository>,
        encoder: Arc<dyn MediaEncoder>,
        provider: Arc<dyn ProviderManager>,
    ) -> Self {
        Self {
            items,
            streams,
            attachments,
            encoder,
            provider,
            open_streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Reads an item's media streams as DTOs.
    async fn streams_dto(&self, item_id: Uuid) -> Result<Vec<MediaStream>, ServiceError> {
        let rows = self
            .streams
            .get_media_streams(&MediaStreamQuery {
                item_id,
                ..Default::default()
            })
            .await?;
        Ok(rows.iter().map(stream_to_dto).collect())
    }

    /// Builds the static [`MediaSourceInfo`] for a resolved item row and its
    /// streams (C# `GetStaticMediaSources` inner assembly).
    fn static_source(item: &BaseItemEntity, streams: Vec<MediaStream>) -> MediaSourceInfo {
        MediaSourceInfo {
            id: Some(item.id.clone()),
            path: item.path.clone(),
            name: item.name.clone(),
            container: container_of(item),
            size: item.size,
            run_time_ticks: item.run_time_ticks,
            media_streams: streams,
            protocol: MediaProtocol::File,
            type_: MediaSourceType::Default,
            supports_direct_play: true,
            supports_direct_stream: true,
            supports_transcoding: true,
            ..Default::default()
        }
    }
}

/// The container of an item, derived from its stored path extension when present.
fn container_of(item: &BaseItemEntity) -> Option<String> {
    item.path
        .as_deref()
        .and_then(|p| std::path::Path::new(p).extension())
        .and_then(|ext| ext.to_str())
        .map(str::to_lowercase)
        .filter(|ext| !ext.is_empty())
}

/// Maps a persisted media-stream row to the wire [`MediaStream`] DTO. Fields the
/// entity does not carry are left at their [`Default`].
fn stream_to_dto(row: &MediaStreamInfoEntity) -> MediaStream {
    MediaStream {
        index: i32::try_from(row.stream_index).unwrap_or(0),
        stream_type: media_stream_type_from_disc(row.stream_type),
        codec: row.codec.clone(),
        codec_tag: row.codec_tag.clone(),
        language: row.language.clone(),
        title: row.title.clone(),
        comment: row.comment.clone(),
        time_base: row.time_base.clone(),
        codec_time_base: row.codec_time_base.clone(),
        profile: row.profile.clone(),
        aspect_ratio: row.aspect_ratio.clone(),
        path: row.path.clone(),
        channel_layout: row.channel_layout.clone(),
        pixel_format: row.pixel_format.clone(),
        color_space: row.color_space.clone(),
        color_transfer: row.color_transfer.clone(),
        color_primaries: row.color_primaries.clone(),
        is_interlaced: row.is_interlaced.unwrap_or(false),
        is_avc: row.is_avc,
        is_default: row.is_default,
        is_forced: row.is_forced,
        is_external: row.is_external,
        is_hearing_impaired: row.is_hearing_impaired.unwrap_or(false),
        is_original: row.is_original,
        is_anamorphic: row.is_anamorphic,
        bit_rate: row.bit_rate.and_then(|v| i32::try_from(v).ok()),
        bit_depth: row.bit_depth.and_then(|v| i32::try_from(v).ok()),
        channels: row.channels.and_then(|v| i32::try_from(v).ok()),
        sample_rate: row.sample_rate.and_then(|v| i32::try_from(v).ok()),
        ref_frames: row.ref_frames.and_then(|v| i32::try_from(v).ok()),
        height: row.height.and_then(|v| i32::try_from(v).ok()),
        width: row.width.and_then(|v| i32::try_from(v).ok()),
        level: row.level,
        // Frame rates are stored as f64 but the wire DTO is f32; the precision
        // loss on a frame-rate value (e.g. 23.976) is immaterial.
        #[allow(clippy::cast_possible_truncation)]
        average_frame_rate: row.average_frame_rate.map(|v| v as f32),
        #[allow(clippy::cast_possible_truncation)]
        real_frame_rate: row.real_frame_rate.map(|v| v as f32),
        ..Default::default()
    }
}

/// Maps a probed wire [`MediaStream`] back to a persistable
/// [`MediaStreamInfoEntity`] — the inverse of [`stream_to_dto`], used to store the
/// streams a scan probe returned. The essential codec/geometry fields playback
/// negotiation relies on are mapped; the optional HDR/Dolby-Vision metadata the
/// wire DTO carries in a different shape defaults off.
pub(crate) fn stream_dto_to_entity(item_id: &str, s: &MediaStream) -> MediaStreamInfoEntity {
    MediaStreamInfoEntity {
        item_id: item_id.to_owned(),
        stream_index: i64::from(s.index),
        stream_type: crate::db_error::media_stream_type_to_disc(s.stream_type),
        codec: s.codec.clone(),
        codec_tag: s.codec_tag.clone(),
        language: s.language.clone(),
        title: s.title.clone(),
        comment: s.comment.clone(),
        time_base: s.time_base.clone(),
        codec_time_base: s.codec_time_base.clone(),
        profile: s.profile.clone(),
        aspect_ratio: s.aspect_ratio.clone(),
        path: s.path.clone(),
        channel_layout: s.channel_layout.clone(),
        pixel_format: s.pixel_format.clone(),
        color_space: s.color_space.clone(),
        color_transfer: s.color_transfer.clone(),
        color_primaries: s.color_primaries.clone(),
        nal_length_size: s.nal_length_size.clone(),
        is_interlaced: Some(s.is_interlaced),
        is_avc: s.is_avc,
        is_default: s.is_default,
        is_forced: s.is_forced,
        is_external: s.is_external,
        is_hearing_impaired: Some(s.is_hearing_impaired),
        is_original: s.is_original,
        is_anamorphic: s.is_anamorphic,
        bit_rate: s.bit_rate.map(i64::from),
        bit_depth: s.bit_depth.map(i64::from),
        channels: s.channels.map(i64::from),
        sample_rate: s.sample_rate.map(i64::from),
        ref_frames: s.ref_frames.map(i64::from),
        height: s.height.map(i64::from),
        width: s.width.map(i64::from),
        level: s.level,
        rotation: s.rotation.map(i64::from),
        average_frame_rate: s.average_frame_rate.map(f64::from),
        real_frame_rate: s.real_frame_rate.map(f64::from),
        ..Default::default()
    }
}

/// Maps a persisted media-attachment row to the wire [`MediaAttachment`] DTO.
fn attachment_to_dto(row: &AttachmentStreamInfoEntity) -> MediaAttachment {
    MediaAttachment {
        index: i32::try_from(row.index).unwrap_or(0),
        codec: row.codec.clone(),
        codec_tag: row.codec_tag.clone(),
        comment: row.comment.clone(),
        file_name: row.filename.clone(),
        mime_type: row.mime_type.clone(),
        delivery_url: None,
    }
}

#[async_trait]
impl MediaSourceManager for HermitMediaSourceManager {
    async fn get_media_streams(&self, item_id: Uuid) -> Result<Vec<MediaStream>, ServiceError> {
        self.streams_dto(item_id).await
    }

    async fn get_media_attachments(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<MediaAttachment>, ServiceError> {
        let rows = self
            .attachments
            .get_media_attachments(&MediaAttachmentQuery {
                item_id,
                ..Default::default()
            })
            .await?;
        Ok(rows.iter().map(attachment_to_dto).collect())
    }

    async fn get_playback_media_sources(
        &self,
        item_id: Uuid,
        _user_id: Uuid,
        _allow_media_probe: bool,
        enable_path_substitution: bool,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        // Playback sources build on the static sources; the per-user bitrate/profile
        // negotiation (MediaStreamSelector) is deferred, so the static set is the
        // playback set for v1.
        self.get_static_media_sources(item_id, enable_path_substitution, Some(_user_id))
            .await
    }

    async fn get_static_media_sources(
        &self,
        item_id: Uuid,
        _enable_path_substitution: bool,
        _user_id: Option<Uuid>,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        let Some(item) = self.items.retrieve_item(item_id).await? else {
            return Ok(Vec::new());
        };
        let streams = self.streams_dto(item_id).await?;
        Ok(vec![Self::static_source(&item, streams)])
    }

    async fn open_live_stream(
        &self,
        request: &LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        // Probe the item to obtain a source, register it under a fresh id, and hand
        // it back. The C# open-token/tuner negotiation is out of scope; the probe
        // via the injected encoder is the real work.
        let request_info = hermit_traits::media_encoding::MediaInfoRequest {
            media_source: MediaSourceInfo {
                id: Some(request.item_id.to_string()),
                ..Default::default()
            },
            extract_chapters: false,
            media_is_audio: false,
        };
        let mut source = self.encoder.get_media_info(&request_info).await?;
        let live_id = Uuid::new_v4().to_string();
        source.live_stream_id = Some(live_id.clone());
        source.requires_closing = true;
        self.open_streams
            .lock()
            .expect("open streams not poisoned")
            .insert(live_id, source.clone());
        Ok(source)
    }

    async fn get_live_stream(&self, id: &str) -> Result<MediaSourceInfo, ServiceError> {
        self.open_streams
            .lock()
            .expect("open streams not poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| ServiceError::not_found(format!("live stream {id}")))
    }

    async fn close_live_stream(&self, id: &str) -> Result<(), ServiceError> {
        self.open_streams
            .lock()
            .expect("open streams not poisoned")
            .remove(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_repository::HermitItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::media_attachment_repository::HermitMediaAttachmentRepository;
    use crate::media_stream_repository::HermitMediaStreamRepository;
    use crate::test_support::{seed_item, test_db};
    use hermit_db::Database;
    use hermit_model::data::BaseItemKind;
    use hermit_traits::media_encoding::{MediaEncoder, MediaInfoRequest};

    /// A stub encoder whose probe returns a fixed source (no ffmpeg needed).
    struct StubEncoder;

    #[async_trait]
    impl MediaEncoder for StubEncoder {
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
            request: &MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            Ok(MediaSourceInfo {
                id: request.media_source.id.clone(),
                container: Some("mkv".to_owned()),
                ..Default::default()
            })
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
            _threed_format: Option<hermit_model::entities::Video3DFormat>,
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

    /// A no-op provider manager (unused by the tested paths).
    struct StubProvider;

    #[async_trait]
    impl ProviderManager for StubProvider {
        async fn queue_refresh(
            &self,
            _item_id: Uuid,
            _options: &hermit_traits::providers::MetadataRefreshOptions,
            _priority: hermit_traits::providers::RefreshPriority,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_full_item(
            &self,
            _item_id: Uuid,
            _options: &hermit_traits::providers::MetadataRefreshOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_single_item(
            &self,
            _item_id: Uuid,
            _options: &hermit_traits::providers::MetadataRefreshOptions,
        ) -> Result<hermit_traits::providers::ItemUpdateType, ServiceError> {
            Ok(hermit_traits::providers::ItemUpdateType::None)
        }
        async fn save_image_from_url(
            &self,
            _item_id: Uuid,
            _url: &str,
            _image_type: hermit_model::entities::ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn save_image(
            &self,
            _item_id: Uuid,
            _content: &[u8],
            _mime_type: &str,
            _image_type: hermit_model::entities::ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_available_remote_images(
            &self,
            _item_id: Uuid,
            _query: &hermit_model::providers::RemoteImageQuery,
        ) -> Result<Vec<hermit_model::providers::RemoteImageInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_remote_image_provider_info(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<hermit_model::providers::ImageProviderInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn save_metadata(
            &self,
            _item_id: Uuid,
            _update_type: hermit_traits::providers::ItemUpdateType,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_external_urls(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<hermit_model::providers::ExternalUrl>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_external_id_infos(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<hermit_model::providers::ExternalIdInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_all_metadata_plugins(
            &self,
        ) -> Result<Vec<hermit_model::configuration::MetadataPluginSummary>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_metadata_options(
            &self,
            _item_id: Uuid,
        ) -> Result<hermit_model::configuration::MetadataOptions, ServiceError> {
            Ok(hermit_model::configuration::MetadataOptions::default())
        }
        async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
            Ok(Vec::new())
        }
    }

    fn manager(db: &Database) -> HermitMediaSourceManager {
        let lookup: Arc<dyn hermit_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        HermitMediaSourceManager::new(
            Arc::new(HermitItemRepository::new(db.clone(), lookup)),
            Arc::new(HermitMediaStreamRepository::new(db.clone())),
            Arc::new(HermitMediaAttachmentRepository::new(db.clone())),
            Arc::new(StubEncoder),
            Arc::new(StubProvider),
        )
    }

    #[tokio::test]
    async fn static_source_carries_path_and_streams() {
        let db = test_db().await;
        // Avoid id 1 — it collides with the query translator's placeholder row.
        let id = Uuid::from_u128(0x101);
        seed_item(&db, id, BaseItemKind::Movie).await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "Path" = '/media/m.mkv', "RunTimeTicks" = 100 WHERE "Id" = ?1"#,
        )
        .bind(id.to_string())
        .execute(db.pool())
        .await
        .expect("set path");
        let mgr = manager(&db);

        let sources = mgr
            .get_static_media_sources(id, false, None)
            .await
            .expect("sources");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path.as_deref(), Some("/media/m.mkv"));
        assert_eq!(sources[0].container.as_deref(), Some("mkv"));
        assert_eq!(sources[0].run_time_ticks, Some(100));
    }

    #[tokio::test]
    async fn missing_item_yields_no_sources() {
        let db = test_db().await;
        let mgr = manager(&db);
        let sources = mgr
            .get_static_media_sources(Uuid::from_u128(99), false, None)
            .await
            .expect("sources");
        assert!(sources.is_empty());
    }

    #[tokio::test]
    async fn live_stream_open_get_close_round_trips() {
        let db = test_db().await;
        let mgr = manager(&db);
        let request = LiveStreamRequest {
            item_id: Uuid::from_u128(7),
            ..Default::default()
        };

        let opened = mgr.open_live_stream(&request).await.expect("open");
        let id = opened.live_stream_id.clone().expect("live id");
        assert!(opened.requires_closing);

        let fetched = mgr.get_live_stream(&id).await.expect("get");
        assert_eq!(fetched.live_stream_id, Some(id.clone()));

        mgr.close_live_stream(&id).await.expect("close");
        assert!(mgr.get_live_stream(&id).await.is_err());
    }
}
