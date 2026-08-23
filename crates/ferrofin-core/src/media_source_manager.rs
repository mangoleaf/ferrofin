//! [`FerrofinMediaSourceManager`] — the concrete [`MediaSourceManager`].
//!
//! Port of `Emby.Server.Implementations.Library.MediaSourceManager` (the
//! API-facing subset). Responsibilities that carry over to this seam:
//! - **streams/attachments as DTOs:** read the persisted
//!   [`MediaStreamInfoEntity`](ferrofin_db::entities::base_items::MediaStreamInfoEntity)
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
use ferrofin_db::entities::base_items::{
    AttachmentStreamInfoEntity, BaseItemEntity, MediaStreamInfoEntity,
};
use ferrofin_model::dto::{MediaSourceInfo, MediaSourceType};
use ferrofin_model::entities::{MediaStreamType, VideoType};
use ferrofin_model::entities_media::{MediaAttachment, MediaStream};
use ferrofin_model::media_info::{LiveStreamRequest, MediaProtocol};
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::MediaSourceManager;
use ferrofin_traits::media_encoding::MediaEncoder;
use ferrofin_traits::persistence::{
    ItemRepository, MediaAttachmentQuery, MediaAttachmentRepository, MediaStreamQuery,
    MediaStreamRepository,
};
use ferrofin_traits::providers::ProviderManager;

use crate::db_error::media_stream_type_from_disc;

/// The concrete media-source manager.
///
/// Holds injected persistence repositories plus the sibling `MediaEncoder` /
/// `ProviderManager` (both by `Arc<dyn _>`), and the in-memory open-live-stream
/// table keyed by the live-stream id.
#[derive(Clone)]
pub struct FerrofinMediaSourceManager {
    items: Arc<dyn ItemRepository>,
    streams: Arc<dyn MediaStreamRepository>,
    attachments: Arc<dyn MediaAttachmentRepository>,
    encoder: Arc<dyn MediaEncoder>,
    #[allow(dead_code)]
    provider: Arc<dyn ProviderManager>,
    /// Live TV manager, when configured — lets playback resolve a Live TV channel
    /// id (which is not a `BaseItems` row) to its tuner stream.
    live_tv: Option<Arc<dyn ferrofin_traits::stubs::LiveTvManager>>,
    /// Localization for the `MediaStream.Localized*` labels re-stamped on
    /// every read (the C# `MediaStreamRepository.Map` does the same — they are
    /// not persisted). `None` (unit tests) leaves them unset.
    localization: Option<Arc<dyn ferrofin_traits::localization::LocalizationManager>>,
    /// Open live streams keyed by their live-stream id. Guarded by a
    /// `std::sync::Mutex` because the guard never spans an `.await`.
    open_streams: Arc<Mutex<HashMap<String, MediaSourceInfo>>>,
}

impl std::fmt::Debug for FerrofinMediaSourceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinMediaSourceManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinMediaSourceManager {
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
            live_tv: None,
            localization: None,
            open_streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Wires the Live TV manager so playback can resolve channel ids to their
    /// tuner streams. Without it, an unknown id yields no media sources as before.
    #[must_use]
    pub fn with_live_tv(mut self, live_tv: Arc<dyn ferrofin_traits::stubs::LiveTvManager>) -> Self {
        self.live_tv = Some(live_tv);
        self
    }

    /// Sets the localization the read-back `Localized*` stream labels come from.
    #[must_use]
    pub fn with_localization(
        mut self,
        localization: Arc<dyn ferrofin_traits::localization::LocalizationManager>,
    ) -> Self {
        self.localization = Some(localization);
        self
    }

    /// Maps a persisted stream row to its DTO and stamps the localized labels.
    fn row_to_dto(&self, row: MediaStreamInfoEntity) -> MediaStream {
        let mut stream = stream_to_dto(row);
        if let Some(localization) = &self.localization {
            localize_stream(&mut stream, localization.as_ref());
        }
        stream
    }

    /// Builds the media source for a Live TV channel id: probe its tuner stream so
    /// transcode negotiation knows the codecs, then mark it as an infinite stream
    /// that must be transcoded (the raw tuner container/codec is rarely
    /// browser-playable). Returns empty when Live TV is unconfigured or the id is
    /// not a known channel.
    async fn channel_media_source(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        let Some(live_tv) = &self.live_tv else {
            return Ok(Vec::new());
        };
        let Some(url) = live_tv.get_channel_stream_url(item_id).await? else {
            return Ok(Vec::new());
        };
        // Probe the tuner stream for its real streams; if the probe fails (tuner
        // unreachable), fall back to a bare source so the id still resolves.
        let request = ferrofin_traits::media_encoding::MediaInfoRequest {
            media_source: MediaSourceInfo {
                path: Some(url.clone()),
                ..Default::default()
            },
            extract_chapters: false,
            media_is_audio: false,
        };
        let probed = self.encoder.get_media_info(&request).await.ok();
        let mut source = probed.unwrap_or_default();
        source.id = Some(item_id.to_string());
        source.path = Some(url.clone());
        source.protocol = MediaProtocol::Http;
        source.container = Some(live_stream_container(&url));
        source.is_infinite_stream = true;
        source.run_time_ticks = None;
        source.supports_direct_play = false;
        source.supports_direct_stream = false;
        source.supports_transcoding = true;
        Ok(vec![source])
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
        Ok(rows.into_iter().map(|row| self.row_to_dto(row)).collect())
    }

    /// Builds the static [`MediaSourceInfo`] for a resolved item row and its
    /// streams (C# `GetStaticMediaSources` inner assembly).
    ///
    /// `pub(crate)` so the DTO service can assemble a list item's media source
    /// from the row it already holds plus prefetched streams, skipping the
    /// per-item `retrieve_item` + `streams_dto` round-trips.
    pub(crate) fn static_source(
        item: &BaseItemEntity,
        streams: Vec<MediaStream>,
    ) -> MediaSourceInfo {
        // A video item's source reports `VideoType.VideoFile` (Jellyfin's
        // `Video.VideoType` default); audio/other sources leave it unset.
        let video_type =
            (item.media_type.as_deref() == Some("Video")).then_some(VideoType::VideoFile);

        // Default audio stream index: the audio stream marked default, else the
        // first audio stream (mirrors `MediaSourceInfo.DefaultAudioStreamIndex`
        // resolution in `MediaSourceManager.SetDefaultAudioAndSubtitleStreamIndexes`).
        let default_audio_stream_index = streams
            .iter()
            .filter(|s| s.stream_type == MediaStreamType::Audio)
            .find(|s| s.is_default)
            .or_else(|| {
                streams
                    .iter()
                    .find(|s| s.stream_type == MediaStreamType::Audio)
            })
            .map(|s| s.index);

        let mut source = MediaSourceInfo {
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
            video_type,
            default_audio_stream_index,
            e_tag: Some(source_etag(item)),
            ..Default::default()
        };
        // Sum the internal streams' bit rates into the source total
        // (`MediaSourceInfo.InferTotalBitrate`).
        source.infer_total_bitrate(false);
        source
    }
}

/// The container reported for a Live TV tuner stream, from its URL extension: an
/// HLS playlist (`.m3u8`) or MPEG-TS (`.ts`); otherwise `ts` (the common IPTV
/// default). Used only for negotiation labelling — the transcode reads the URL
/// directly regardless.
fn live_stream_container(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    if path.to_ascii_lowercase().ends_with(".m3u8") {
        "hls".to_owned()
    } else {
        "ts".to_owned()
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

/// A stable `ETag` for a static media source.
///
/// Mirrors Jellyfin's `BaseItem.GetEtag`, which MD5-hashes a pipe-joined value
/// list (there, the item's `DateLastSaved.Ticks`) and renders it as a 32-char
/// dashless hex string (`Guid.ToString("N")`). Ferrofin hashes the item id plus
/// its last-modified time, so the tag changes whenever the item's media does.
/// The MD5-over-UTF-16LE helper is the same one Jellyfin uses (`GetMD5`).
fn source_etag(item: &BaseItemEntity) -> String {
    let modified = item.date_modified.map_or(0, |d| d.timestamp_millis());
    let input = format!("{}|{modified}", item.id);
    ferrofin_common::extensions::get_md5(&input)
        .simple()
        .to_string()
}

/// Maps a persisted media-stream row to the wire [`MediaStream`] DTO. Fields the
/// entity does not carry are left at their [`Default`].
fn stream_to_dto(row: MediaStreamInfoEntity) -> MediaStream {
    let mut stream = MediaStream {
        index: i32::try_from(row.stream_index).unwrap_or(0),
        stream_type: media_stream_type_from_disc(row.stream_type),
        codec: row.codec,
        codec_tag: row.codec_tag,
        language: row.language,
        title: row.title,
        comment: row.comment,
        time_base: row.time_base,
        codec_time_base: row.codec_time_base,
        nal_length_size: row.nal_length_size,
        profile: row.profile,
        aspect_ratio: row.aspect_ratio,
        path: row.path,
        channel_layout: row.channel_layout,
        pixel_format: row.pixel_format,
        color_space: row.color_space,
        color_transfer: row.color_transfer,
        color_primaries: row.color_primaries,
        is_interlaced: row.is_interlaced.unwrap_or(false),
        is_avc: row.is_avc,
        is_default: row.is_default,
        is_forced: row.is_forced,
        is_external: row.is_external,
        is_hearing_impaired: row.is_hearing_impaired.unwrap_or(false),
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
        // Dolby Vision / HDR10+ metadata — the wire DTO carries the present flags
        // as 0/1 ints; the entity stores booleans. Required so playback's
        // `video_range_type` sees DOVI/HDR10+ and negotiates copy-vs-transcode
        // correctly.
        dv_version_major: row.dv_version_major.and_then(|v| i32::try_from(v).ok()),
        dv_version_minor: row.dv_version_minor.and_then(|v| i32::try_from(v).ok()),
        dv_profile: row.dv_profile.and_then(|v| i32::try_from(v).ok()),
        dv_level: row.dv_level.and_then(|v| i32::try_from(v).ok()),
        dv_bl_signal_compatibility_id: row
            .dv_bl_signal_compatibility_id
            .and_then(|v| i32::try_from(v).ok()),
        rpu_present_flag: row.rpu_present_flag.map(i32::from),
        bl_present_flag: row.bl_present_flag.map(i32::from),
        el_present_flag: row.el_present_flag.map(i32::from),
        hdr10_plus_present_flag: row.hdr10_plus_present_flag,
        ..Default::default()
    };
    // Compose the display title from the now-populated codec/language/channel
    // fields so clients don't fall back to "Undefined".
    stream.display_title = stream.display_title();
    // Materialize the computed-property fields (VideoRange/VideoRangeType/
    // AudioSpatialFormat/IsTextSubtitleStream/ReferenceFrameRate) Jellyfin serializes
    // as getters — derived on every load, not persisted.
    stream.populate_computed_fields();
    // Port of `MediaSourceManager.StreamSupportsExternalStream`: stamp whether a
    // subtitle can be delivered as a separate stream (external file, extractable
    // text, or PGS/VobSub). Not persisted — derived on every load, like C#.
    // Without it the StreamBuilder's External+conversion match always fails and
    // every embedded text subtitle silently falls back to Encode with no URL.
    if stream.stream_type == MediaStreamType::Subtitle {
        stream.supports_external_stream = stream.is_external
            || stream.is_text_subtitle_stream()
            || stream.is_pgs_subtitle_stream()
            || stream.is_vob_sub_subtitle_stream();
    }
    stream
}

/// Port of the tail of `MediaStreamRepository.Map(MediaStreamInfo)`: the
/// `Localized*` labels are derived on every read, never persisted. Audio and
/// subtitle streams get `Default`/`External` (+ the language display name);
/// subtitles add `Undefined`/`Forced`/`HearingImpaired`.
/// Runs after `display_title` so the title is recomputed with the labels, as
/// the C# getter evaluates them lazily.
fn localize_stream(
    stream: &mut MediaStream,
    localization: &dyn ferrofin_traits::localization::LocalizationManager,
) {
    if !matches!(
        stream.stream_type,
        MediaStreamType::Audio | MediaStreamType::Subtitle
    ) {
        return;
    }
    stream.localized_default = Some(localization.get_localized_string("Default"));
    stream.localized_external = Some(localization.get_localized_string("External"));
    if let Some(language) = stream.language.as_deref().filter(|l| !l.is_empty()) {
        stream.localized_language = localization.get_language_display_name(language);
    }
    // (`LocalizedOriginal` is post-10.11.8 and absent from the contract.)
    if stream.stream_type == MediaStreamType::Subtitle {
        stream.localized_undefined = Some(localization.get_localized_string("Undefined"));
        stream.localized_forced = Some(localization.get_localized_string("Forced"));
        stream.localized_hearing_impaired =
            Some(localization.get_localized_string("HearingImpaired"));
    }
    stream.display_title = stream.display_title();
}

/// Maps a probed wire [`MediaStream`] back to a persistable
/// [`MediaStreamInfoEntity`] — the inverse of [`stream_to_dto`], used to store the
/// streams a scan probe returned. Codec/geometry plus the HDR/Dolby-Vision
/// metadata (present flags stored as booleans) are mapped so the persisted row
/// round-trips the video-range information playback negotiation relies on.
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
        // Dolby Vision / HDR10+ metadata — load-bearing for the HDR video-range
        // derivation (`MediaStream::video_range_type`) that drives the transcode
        // copy-vs-encode decision. The DTO carries the present flags as 0/1 ints;
        // the entity stores them as booleans.
        dv_version_major: s.dv_version_major.map(i64::from),
        dv_version_minor: s.dv_version_minor.map(i64::from),
        dv_profile: s.dv_profile.map(i64::from),
        dv_level: s.dv_level.map(i64::from),
        dv_bl_signal_compatibility_id: s.dv_bl_signal_compatibility_id.map(i64::from),
        rpu_present_flag: s.rpu_present_flag.map(|v| v != 0),
        bl_present_flag: s.bl_present_flag.map(|v| v != 0),
        el_present_flag: s.el_present_flag.map(|v| v != 0),
        hdr10_plus_present_flag: s.hdr10_plus_present_flag,
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
impl MediaSourceManager for FerrofinMediaSourceManager {
    async fn get_media_streams(&self, item_id: Uuid) -> Result<Vec<MediaStream>, ServiceError> {
        self.streams_dto(item_id).await
    }

    async fn get_media_streams_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<MediaStream>>, ServiceError> {
        // One `ItemId IN (…)` query for the whole page, then map each item's rows
        // to DTOs — the batch form used by list projection to avoid an N+1.
        let rows = self.streams.get_media_streams_batch(item_ids).await?;
        Ok(rows
            .into_iter()
            .map(|(id, streams)| {
                (
                    id,
                    streams
                        .into_iter()
                        .map(|row| self.row_to_dto(row))
                        .collect(),
                )
            })
            .collect())
    }

    async fn get_item_ids_with_subtitles(
        &self,
        item_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, ServiceError> {
        // Delegate to the repository's ids-only query (no stream rows load).
        self.streams.get_item_ids_with_subtitles(item_ids).await
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

    async fn get_alternate_versions_batch(
        &self,
        primary_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<ferrofin_db::entities::base_items::BaseItemEntity>>, ServiceError>
    {
        self.items
            .get_items_by_primary_version_batch(primary_ids)
            .await
    }

    async fn get_static_media_sources(
        &self,
        item_id: Uuid,
        _enable_path_substitution: bool,
        _user_id: Option<Uuid>,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        let Some(mut item) = self.items.retrieve_item(item_id).await? else {
            // Not a library item — it may be a Live TV channel.
            return self.channel_media_source(item_id).await;
        };
        // An ALTERNATE version (PrimaryVersionId set) resolves through its
        // primary, so playing any version yields the full merged source list
        // (upstream navigates the same way; the alternate alone would hide its
        // siblings from the version picker).
        if let Some(primary) = item
            .primary_version_id
            .as_deref()
            .and_then(|p| Uuid::parse_str(p).ok())
            && let Some(primary_item) = self.items.retrieve_item(primary).await?
        {
            item = primary_item;
        }
        let item_id = Uuid::parse_str(&item.id).unwrap_or(item_id);
        let streams = self.streams_dto(item_id).await?;
        let mut sources = vec![Self::static_source(&item, streams)];
        // Append merged alternate versions' sources (C# GetStaticMediaSources includes the item's
        // LinkedAlternateVersions). After MergeVersions the alternates point at this item via
        // PrimaryVersionId, so a merged item reports all its versions as selectable sources.
        for alt in self.items.get_items_by_primary_version(item_id).await? {
            if let Ok(alt_id) = Uuid::parse_str(&alt.id) {
                let alt_streams = self.streams_dto(alt_id).await?;
                sources.push(Self::static_source(&alt, alt_streams));
            }
        }
        Ok(sources)
    }

    async fn open_live_stream(
        &self,
        request: &LiveStreamRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        // Probe the item to obtain a source, register it under a fresh id, and hand
        // it back. The C# open-token/tuner negotiation is out of scope; the probe
        // via the injected encoder is the real work.
        let request_info = ferrofin_traits::media_encoding::MediaInfoRequest {
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

    async fn refresh_media_streams(&self, item_id: Uuid) -> Result<(), ServiceError> {
        let Some(item) = self.items.retrieve_item(item_id).await? else {
            return Ok(());
        };
        let is_audio = item.media_type.as_deref() == Some("Audio");
        let is_media = is_audio || item.media_type.as_deref() == Some("Video");
        if item.is_folder || !is_media || item.path.is_none() {
            return Ok(());
        }
        let request = ferrofin_traits::media_encoding::MediaInfoRequest {
            media_source: MediaSourceInfo {
                path: item.path.clone(),
                ..Default::default()
            },
            extract_chapters: false,
            media_is_audio: is_audio,
        };
        // Re-probe and rewrite the item's stream rows (which carry the codec/HDR/
        // Dolby-Vision fields). Duration/size persistence lives with the item
        // repository's scan path, not this manager, so they are left as scanned.
        let probed = self.encoder.get_media_info(&request).await?;
        let streams: Vec<_> = probed
            .media_streams
            .iter()
            .map(|s| stream_dto_to_entity(&item.id, s))
            .collect();
        self.streams.save_media_streams(item_id, &streams).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_repository::FerrofinItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::media_attachment_repository::FerrofinMediaAttachmentRepository;
    use crate::media_stream_repository::FerrofinMediaStreamRepository;
    use crate::test_support::{seed_item, test_db};
    use ferrofin_db::Database;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};

    #[test]
    fn stream_text_fields_round_trip_through_entity() {
        // `stream_to_dto` moves ~16 owned text fields out of the row rather than
        // cloning them (it is the per-stream hot path for every list page that
        // asks for MediaStreams/MediaSources). Nothing else asserts those fields
        // survive the entity hop, so dropping one — codec, language, the
        // subtitle title a client shows in its track picker — would otherwise be
        // silent.
        let dto = MediaStream {
            index: 2,
            stream_type: ferrofin_model::entities::MediaStreamType::Subtitle,
            codec: Some("subrip".to_owned()),
            codec_tag: Some("srt ".to_owned()),
            language: Some("eng".to_owned()),
            title: Some("English (SDH)".to_owned()),
            comment: Some("forced narrative".to_owned()),
            time_base: Some("1/1000".to_owned()),
            codec_time_base: Some("1/48000".to_owned()),
            nal_length_size: Some("4".to_owned()),
            profile: Some("High".to_owned()),
            aspect_ratio: Some("16:9".to_owned()),
            path: Some("/media/movie.en.srt".to_owned()),
            channel_layout: Some("5.1".to_owned()),
            pixel_format: Some("yuv420p".to_owned()),
            // Distinct values on purpose: the three colour fields are adjacent
            // and same-shaped, so a shared sentinel would let a cross-wired
            // pair (the likeliest regex slip) round-trip undetected.
            color_space: Some("bt2020nc".to_owned()),
            color_transfer: Some("smpte2084".to_owned()),
            color_primaries: Some("bt2020".to_owned()),
            ..MediaStream::default()
        };

        let back = stream_to_dto(stream_dto_to_entity("item-1", &dto));

        assert_eq!(back.codec.as_deref(), Some("subrip"));
        assert_eq!(back.codec_tag.as_deref(), Some("srt "));
        assert_eq!(back.language.as_deref(), Some("eng"));
        assert_eq!(back.title.as_deref(), Some("English (SDH)"));
        assert_eq!(back.comment.as_deref(), Some("forced narrative"));
        assert_eq!(back.time_base.as_deref(), Some("1/1000"));
        assert_eq!(back.codec_time_base.as_deref(), Some("1/48000"));
        assert_eq!(back.nal_length_size.as_deref(), Some("4"));
        assert_eq!(back.profile.as_deref(), Some("High"));
        assert_eq!(back.aspect_ratio.as_deref(), Some("16:9"));
        assert_eq!(back.path.as_deref(), Some("/media/movie.en.srt"));
        assert_eq!(back.channel_layout.as_deref(), Some("5.1"));
        assert_eq!(back.pixel_format.as_deref(), Some("yuv420p"));
        assert_eq!(back.color_space.as_deref(), Some("bt2020nc"));
        assert_eq!(back.color_transfer.as_deref(), Some("smpte2084"));
        assert_eq!(back.color_primaries.as_deref(), Some("bt2020"));
    }

    #[test]
    fn dv_metadata_round_trips_through_entity() {
        use ferrofin_model::data::VideoRangeType;
        // A Dolby Vision Profile 8.1 (HDR10-compatible) video stream, as ffprobe
        // reports it: present flags as 0/1 ints, HDR10 base transfer.
        let dto = MediaStream {
            index: 0,
            stream_type: ferrofin_model::entities::MediaStreamType::Video,
            codec: Some("av1".to_owned()),
            color_transfer: Some("smpte2084".to_owned()),
            dv_profile: Some(8),
            dv_level: Some(10),
            dv_version_major: Some(1),
            dv_bl_signal_compatibility_id: Some(1),
            rpu_present_flag: Some(1),
            bl_present_flag: Some(1),
            el_present_flag: Some(0),
            ..MediaStream::default()
        };
        let entity = stream_dto_to_entity("item-1", &dto);
        assert_eq!(entity.dv_profile, Some(8));
        assert_eq!(entity.dv_bl_signal_compatibility_id, Some(1));
        assert_eq!(entity.rpu_present_flag, Some(true));
        assert_eq!(entity.bl_present_flag, Some(true));
        assert_eq!(entity.el_present_flag, Some(false));

        let back = stream_to_dto(entity);
        assert_eq!(back.dv_profile, Some(8));
        assert_eq!(back.dv_bl_signal_compatibility_id, Some(1));
        assert_eq!(back.rpu_present_flag, Some(1));
        assert_eq!(back.bl_present_flag, Some(1));
        assert_eq!(back.el_present_flag, Some(0));
        // The point of persisting DV: the derived range is DOVI, not plain HDR10.
        assert_eq!(back.video_range_type(), VideoRangeType::DoviWithHdr10);
    }

    /// `MediaStreamRepository.Map` re-stamps the `Localized*` labels on every
    /// read (they are not persisted) for audio/subtitle streams only, and the
    /// display title picks them up.
    #[test]
    fn read_back_stamps_localized_labels_for_audio_and_subtitles() {
        let de = crate::localization_manager::LocalizationManager::default().with_ui_culture("de");
        let mut subtitle = MediaStream {
            stream_type: MediaStreamType::Subtitle,
            codec: Some("subrip".to_owned()),
            language: Some("eng".to_owned()),
            is_forced: true,
            is_default: true,
            ..Default::default()
        };
        localize_stream(&mut subtitle, &de);
        assert_eq!(subtitle.localized_default.as_deref(), Some("Standard"));
        assert_eq!(subtitle.localized_forced.as_deref(), Some("Erzwungen"));
        assert_eq!(subtitle.localized_undefined.as_deref(), Some("Undefiniert"));
        assert_eq!(
            subtitle.localized_hearing_impaired.as_deref(),
            Some("Hörgeschädigt")
        );
        assert_eq!(subtitle.localized_external.as_deref(), Some("Extern"));
        assert_eq!(subtitle.localized_language.as_deref(), Some("English"));
        let title = subtitle.display_title.clone().unwrap_or_default();
        assert!(
            title.contains("Standard") && title.contains("Erzwungen"),
            "{title}"
        );

        let mut audio = MediaStream {
            stream_type: MediaStreamType::Audio,
            language: Some("fra".to_owned()),
            ..Default::default()
        };
        localize_stream(&mut audio, &de);
        assert_eq!(audio.localized_default.as_deref(), Some("Standard"));
        assert_eq!(audio.localized_external.as_deref(), Some("Extern"));
        assert!(audio.localized_forced.is_none() && audio.localized_undefined.is_none());
        assert_eq!(audio.localized_language.as_deref(), Some("French"));

        let mut video = MediaStream {
            stream_type: MediaStreamType::Video,
            ..Default::default()
        };
        localize_stream(&mut video, &de);
        assert!(video.localized_default.is_none());
    }

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

    /// A no-op provider manager (unused by the tested paths).
    struct StubProvider;

    #[async_trait]
    impl ProviderManager for StubProvider {
        async fn queue_refresh(
            &self,
            _item_id: Uuid,
            _options: &ferrofin_traits::providers::MetadataRefreshOptions,
            _priority: ferrofin_traits::providers::RefreshPriority,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_full_item(
            &self,
            _item_id: Uuid,
            _options: &ferrofin_traits::providers::MetadataRefreshOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_single_item(
            &self,
            _item_id: Uuid,
            _options: &ferrofin_traits::providers::MetadataRefreshOptions,
        ) -> Result<ferrofin_traits::providers::ItemUpdateType, ServiceError> {
            Ok(ferrofin_traits::providers::ItemUpdateType::None)
        }
        async fn save_image_from_url(
            &self,
            _item_id: Uuid,
            _url: &str,
            _image_type: ferrofin_model::entities::ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn save_image(
            &self,
            _item_id: Uuid,
            _content: &[u8],
            _mime_type: &str,
            _image_type: ferrofin_model::entities::ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_available_remote_images(
            &self,
            _item_id: Uuid,
            _query: &ferrofin_model::providers::RemoteImageQuery,
        ) -> Result<Vec<ferrofin_model::providers::RemoteImageInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_remote_image_provider_info(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_model::providers::ImageProviderInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn save_metadata(
            &self,
            _item_id: Uuid,
            _update_type: ferrofin_traits::providers::ItemUpdateType,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_external_id_infos(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_model::providers::ExternalIdInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_all_metadata_plugins(
            &self,
        ) -> Result<Vec<ferrofin_model::configuration::MetadataPluginSummary>, ServiceError>
        {
            Ok(Vec::new())
        }
        async fn get_metadata_options(
            &self,
            _item_id: Uuid,
        ) -> Result<ferrofin_model::configuration::MetadataOptions, ServiceError> {
            Ok(ferrofin_model::configuration::MetadataOptions::default())
        }
        async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
            Ok(Vec::new())
        }
    }

    fn manager(db: &Database) -> FerrofinMediaSourceManager {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        FerrofinMediaSourceManager::new(
            Arc::new(FerrofinItemRepository::new(db.clone(), lookup)),
            Arc::new(FerrofinMediaStreamRepository::new(db.clone())),
            Arc::new(FerrofinMediaAttachmentRepository::new(db.clone())),
            Arc::new(StubEncoder),
            Arc::new(StubProvider),
        )
    }

    /// A Live TV manager that resolves any channel id to a fixed tuner URL.
    struct FakeLiveTv;

    #[async_trait]
    impl ferrofin_traits::stubs::LiveTvManager for FakeLiveTv {
        async fn get_schedules_direct_countries(&self) -> Result<Vec<u8>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_live_tv_info(
            &self,
        ) -> Result<ferrofin_model::live_tv::LiveTvInfo, ServiceError> {
            Ok(ferrofin_model::live_tv::LiveTvInfo::default())
        }
        async fn get_tuner_hosts(
            &self,
        ) -> Result<Vec<ferrofin_model::live_tv::TunerHostInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn save_tuner_host(
            &self,
            info: ferrofin_model::live_tv::TunerHostInfo,
        ) -> Result<ferrofin_model::live_tv::TunerHostInfo, ServiceError> {
            Ok(info)
        }
        async fn delete_tuner_host(&self, _id: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_listing_providers(
            &self,
        ) -> Result<Vec<ferrofin_model::live_tv::ListingsProviderInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn save_listing_provider(
            &self,
            info: ferrofin_model::live_tv::ListingsProviderInfo,
        ) -> Result<ferrofin_model::live_tv::ListingsProviderInfo, ServiceError> {
            Ok(info)
        }
        async fn delete_listing_provider(&self, _id: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_channels(
            &self,
            _options: &ferrofin_traits::options::DtoOptions,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_model::dto::BaseItemDto>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_channel(
            &self,
            _id: Uuid,
            _options: &ferrofin_traits::options::DtoOptions,
        ) -> Result<Option<ferrofin_model::dto::BaseItemDto>, ServiceError> {
            Ok(None)
        }
        async fn get_programs(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
            _options: &ferrofin_traits::options::DtoOptions,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_model::dto::BaseItemDto>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_program(
            &self,
            _id: Uuid,
            _options: &ferrofin_traits::options::DtoOptions,
        ) -> Result<Option<ferrofin_model::dto::BaseItemDto>, ServiceError> {
            Ok(None)
        }
        async fn reset_tuner(&self, _id: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_guide(&self) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_channel_stream_url(&self, _id: Uuid) -> Result<Option<String>, ServiceError> {
            Ok(Some("http://tuner/live.ts".to_owned()))
        }
        async fn get_timers(
            &self,
        ) -> Result<Vec<ferrofin_model::live_tv::TimerInfoDto>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_timer(
            &self,
            _id: &str,
        ) -> Result<Option<ferrofin_model::live_tv::TimerInfoDto>, ServiceError> {
            Ok(None)
        }
        async fn create_timer(
            &self,
            _timer: ferrofin_model::live_tv::TimerInfoDto,
        ) -> Result<String, ServiceError> {
            Ok(String::new())
        }
        async fn update_timer(
            &self,
            _id: &str,
            _timer: ferrofin_model::live_tv::TimerInfoDto,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn cancel_timer(&self, _id: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_series_timers(
            &self,
        ) -> Result<Vec<ferrofin_model::live_tv::SeriesTimerInfoDto>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_series_timer(
            &self,
            _id: &str,
        ) -> Result<Option<ferrofin_model::live_tv::SeriesTimerInfoDto>, ServiceError> {
            Ok(None)
        }
        async fn create_series_timer(
            &self,
            _timer: ferrofin_model::live_tv::SeriesTimerInfoDto,
        ) -> Result<String, ServiceError> {
            Ok(String::new())
        }
        async fn update_series_timer(
            &self,
            _id: &str,
            _timer: ferrofin_model::live_tv::SeriesTimerInfoDto,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn cancel_series_timer(&self, _id: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_recordings(
            &self,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_model::dto::BaseItemDto>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_recording(
            &self,
            _id: Uuid,
        ) -> Result<Option<ferrofin_model::dto::BaseItemDto>, ServiceError> {
            Ok(None)
        }
        async fn get_recording_path(&self, _id: Uuid) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
        async fn delete_recording(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn channel_id_resolves_to_infinite_transcodable_stream() {
        let db = test_db().await;
        let mgr = manager(&db).with_live_tv(Arc::new(FakeLiveTv));
        // An id absent from BaseItems falls through to the Live TV channel path.
        let sources = mgr
            .get_static_media_sources(Uuid::from_u128(0x777), false, None)
            .await
            .expect("sources");
        assert_eq!(sources.len(), 1);
        let s = &sources[0];
        assert_eq!(s.path.as_deref(), Some("http://tuner/live.ts"));
        assert!(s.is_infinite_stream);
        assert!(s.supports_transcoding);
        assert!(!s.supports_direct_play);
        assert_eq!(s.container.as_deref(), Some("ts"));
    }

    #[tokio::test]
    async fn static_source_carries_path_and_streams() {
        use ferrofin_traits::persistence::ItemPersistenceService as _;
        let db = test_db().await;
        // Avoid id 1 — it collides with the query translator's placeholder row.
        let id = Uuid::from_u128(0x101);
        seed_item(&db, id, BaseItemKind::Movie).await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "Path" = '/media/m.mkv', "RunTimeTicks" = 100 WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .execute(db.writer())
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

        // Link a merged alternate version through the repository: the batch
        // alternates lookup groups it under this primary.
        let alt = BaseItemEntity {
            id: guid_to_db(Uuid::from_u128(0x102)),
            type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
            name: Some("Alt".to_owned()),
            path: Some("/media/alt.mkv".to_owned()),
            media_type: Some("Video".to_owned()),
            primary_version_id: Some(guid_to_db(id)),
            date_modified: Some(chrono::Utc::now()),
            ..Default::default()
        };
        crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone())
            .save_items(std::slice::from_ref(&alt))
            .await
            .expect("save alternate");
        let batch = mgr
            .get_alternate_versions_batch(&[id])
            .await
            .expect("alternates");
        assert_eq!(batch[&id].len(), 1);
        assert_eq!(batch[&id][0].path.as_deref(), Some("/media/alt.mkv"));
    }

    #[test]
    fn static_source_fills_video_source_fields() {
        let item = BaseItemEntity {
            id: "item-1".to_owned(),
            name: Some("Movie".to_owned()),
            path: Some("/media/m.mkv".to_owned()),
            media_type: Some("Video".to_owned()),
            run_time_ticks: Some(100),
            date_modified: Some(chrono::Utc::now()),
            ..Default::default()
        };
        // Stream 1 is the default audio track (index 0 is video).
        let streams = vec![
            MediaStream {
                index: 0,
                stream_type: MediaStreamType::Video,
                bit_rate: Some(4_000_000),
                ..MediaStream::default()
            },
            MediaStream {
                index: 1,
                stream_type: MediaStreamType::Audio,
                is_default: true,
                bit_rate: Some(128_000),
                ..MediaStream::default()
            },
        ];

        let source = FerrofinMediaSourceManager::static_source(&item, streams);
        assert_eq!(source.video_type, Some(VideoType::VideoFile));
        assert_eq!(source.default_audio_stream_index, Some(1));
        // Total bitrate = sum of the internal streams.
        assert_eq!(source.bitrate, Some(4_128_000));
        // ETag is a 32-char dashless MD5 hex, stable for the same id+modified.
        let etag = source.e_tag.expect("etag");
        assert_eq!(etag.len(), 32);
        assert!(etag.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(source_etag(&item), etag);
    }

    #[test]
    fn static_source_first_audio_when_none_default() {
        let item = BaseItemEntity {
            id: "item-2".to_owned(),
            media_type: Some("Video".to_owned()),
            ..Default::default()
        };
        let streams = vec![
            MediaStream {
                index: 2,
                stream_type: MediaStreamType::Audio,
                ..MediaStream::default()
            },
            MediaStream {
                index: 3,
                stream_type: MediaStreamType::Audio,
                ..MediaStream::default()
            },
        ];
        let source = FerrofinMediaSourceManager::static_source(&item, streams);
        assert_eq!(source.default_audio_stream_index, Some(2));
    }

    #[test]
    fn static_source_audio_item_has_no_video_type() {
        let item = BaseItemEntity {
            id: "item-3".to_owned(),
            media_type: Some("Audio".to_owned()),
            ..Default::default()
        };
        let source = FerrofinMediaSourceManager::static_source(&item, Vec::new());
        assert_eq!(source.video_type, None);
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
