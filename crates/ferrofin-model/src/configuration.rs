//! Port of `MediaBrowser.Model.Configuration`.
//!
//! [`ServerConfiguration`] flattens its C# base `BaseApplicationConfiguration`
//! (Rust has no struct inheritance). Non-wire helper methods (`TypeOptions`
//! image-option lookups, `LibraryOptions::GetTypeOptions`) and the large static
//! `DefaultImageOptions` table are intentionally not ported — they are server
//! logic, not part of the JSON contract.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::drawing::ImageResolution;
use crate::dto::NameValuePair;
use crate::entities::{
    DeinterlaceMethod, DownMixStereoAlgorithms, EncoderPreset, HardwareAccelerationType, ImageType,
    TonemappingAlgorithm, TonemappingMode, TonemappingRange,
};
use crate::system::CastReceiverApplication;
use crate::updates::RepositoryInfo;

/// The convention used for naming saved images.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ImageSavingConvention {
    /// The legacy naming convention.
    #[default]
    Legacy,
    /// A convention compatible with other media servers and metadata managers.
    Compatible,
}

/// Options for seeking the input audio stream when transcoding HLS segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum HlsAudioSeekStrategy {
    /// Trim copied audio packets before the seek point.
    #[default]
    TrimCopiedAudio = 0,
    /// Prevent audio streams from being copied if the video stream is
    /// transcoded.
    TranscodeAudio = 1,
}

/// The behavior used by the trickplay provider on library scan/update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum TrickplayScanBehavior {
    /// Start generation, only return once complete.
    Blocking,
    /// Start generation, return immediately.
    #[default]
    NonBlocking,
}

/// The process priority for a spawned ffmpeg process (mirrors
/// `System.Diagnostics.ProcessPriorityClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ProcessPriorityClass {
    /// Normal priority.
    Normal,
    /// Idle priority.
    Idle,
    /// High priority.
    High,
    /// Real-time priority.
    RealTime,
    /// Below-normal priority.
    #[default]
    BelowNormal,
    /// Above-normal priority.
    AboveNormal,
}

/// The type of a metadata plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum MetadataPluginType {
    /// A local image provider.
    #[default]
    LocalImageProvider,
    /// An image fetcher.
    ImageFetcher,
    /// An image saver.
    ImageSaver,
    /// A local metadata provider.
    LocalMetadataProvider,
    /// A metadata fetcher.
    MetadataFetcher,
    /// A metadata saver.
    MetadataSaver,
    /// A subtitle fetcher.
    SubtitleFetcher,
    /// A lyric fetcher.
    LyricFetcher,
    /// A media segment provider.
    MediaSegmentProvider,
    /// A local similarity provider.
    LocalSimilarityProvider,
    /// A similarity provider.
    SimilarityProvider,
    /// A search provider.
    SearchProvider,
}

/// Options for disabling embedded subtitles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum EmbeddedSubtitleOptions {
    /// Allow all embedded subs.
    #[default]
    AllowAll = 0,
    /// Allow only text-based embedded subs.
    AllowText = 1,
    /// Allow only image-based embedded subs.
    AllowImage = 2,
    /// Disable all embedded subs.
    AllowNone = 3,
}

/// The subtitle playback mode (mirrors
/// `Jellyfin.Database.Implementations.Enums.SubtitlePlaybackMode`).
///
/// Forward reference: referenced by [`UserConfiguration`] and defined here as it
/// has no dedicated port unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum SubtitlePlaybackMode {
    /// The default mode.
    #[default]
    Default,
    /// Always show subtitles.
    Always,
    /// Only show forced subtitles.
    OnlyForced,
    /// Never show subtitles.
    None,
    /// Smart subtitle display.
    Smart,
}

/// A single image download option for a metadata type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ImageOption {
    /// Gets or sets the type.
    #[serde(rename = "Type")]
    pub type_: ImageType,

    /// Gets or sets the limit.
    pub limit: i32,

    /// Gets or sets the minimum width.
    pub min_width: i32,
}

impl Default for ImageOption {
    fn default() -> Self {
        Self {
            type_: ImageType::default(),
            limit: 1,
            min_width: 0,
        }
    }
}

/// Info about a media path in a library.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MediaPathInfo {
    /// Gets or sets the path.
    pub path: String,
}

/// A single available library option (a metadata/image/subtitle provider) and
/// whether it is enabled by default.
///
/// Port of `Jellyfin.Api.Models.LibraryDtos.LibraryOptionInfoDto`, one entry in
/// the `GET /Libraries/AvailableOptions` result.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LibraryOptionInfoDto {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets a value indicating whether this option is enabled by
    /// default.
    pub default_enabled: bool,
}

/// The per-item-type options (fetchers and default image options) offered for a
/// library of a given representative item type.
///
/// Port of `Jellyfin.Api.Models.LibraryDtos.LibraryTypeOptionsDto`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LibraryTypeOptionsDto {
    /// Gets or sets the item type this block applies to.
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,

    /// Gets or sets the available metadata fetchers.
    pub metadata_fetchers: Vec<LibraryOptionInfoDto>,

    /// Gets or sets the available image fetchers.
    pub image_fetchers: Vec<LibraryOptionInfoDto>,

    /// Gets or sets the supported image types.
    pub supported_image_types: Vec<ImageType>,

    /// Gets or sets the default image options.
    pub default_image_options: Vec<ImageOption>,
}

/// The result of `GET /Libraries/AvailableOptions`: the metadata/subtitle/lyric
/// providers plus the per-item-type options assembled from the metadata-plugin
/// registry.
///
/// Port of `Jellyfin.Api.Models.LibraryDtos.LibraryOptionsResultDto`. At this
/// seam no metadata plugins are registered, so every collection is empty — a
/// faithful projection (Jellyfin returns empty arrays when no plugin matches).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LibraryOptionsResultDto {
    /// Gets or sets the available metadata savers.
    pub metadata_savers: Vec<LibraryOptionInfoDto>,

    /// Gets or sets the available metadata readers.
    pub metadata_readers: Vec<LibraryOptionInfoDto>,

    /// Gets or sets the available subtitle fetchers.
    pub subtitle_fetchers: Vec<LibraryOptionInfoDto>,

    /// Gets or sets the available lyric fetchers.
    pub lyric_fetchers: Vec<LibraryOptionInfoDto>,

    /// Gets or sets the available media-segment providers.
    pub media_segment_providers: Vec<LibraryOptionInfoDto>,

    /// Gets or sets the per-item-type options.
    pub type_options: Vec<LibraryTypeOptionsDto>,
}

/// Metadata configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MetadataConfiguration {
    /// Gets or sets a value indicating whether to use the file creation time
    /// for the date added.
    pub use_file_creation_time_for_date_added: bool,
}

impl Default for MetadataConfiguration {
    fn default() -> Self {
        Self {
            use_file_creation_time_for_date_added: true,
        }
    }
}

/// A path substitution rule.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PathSubstitution {
    /// Gets or sets the value to substitute.
    pub from: String,

    /// Gets or sets the value to substitute with.
    pub to: String,
}

/// A metadata plugin (name plus type).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MetadataPlugin {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the type.
    #[serde(rename = "Type")]
    pub type_: MetadataPluginType,
}

/// A summary of the metadata plugins available for an item type.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MetadataPluginSummary {
    /// Gets or sets the type of the item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_type: Option<String>,

    /// Gets or sets the plugins.
    pub plugins: Vec<MetadataPlugin>,

    /// Gets or sets the supported image types.
    pub supported_image_types: Vec<ImageType>,
}

/// XBMC (Kodi) NFO metadata options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct XbmcMetadataOptions {
    /// Gets or sets the user id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,

    /// Gets or sets the release date format.
    pub release_date_format: String,

    /// Gets or sets a value indicating whether to save image paths in the NFO.
    #[serde(rename = "SaveImagePathsInNfo")]
    pub save_image_paths_in_nfo: bool,

    /// Gets or sets a value indicating whether path substitution is enabled.
    pub enable_path_substitution: bool,

    /// Gets or sets a value indicating whether extra thumbs are duplicated.
    pub enable_extra_thumbs_duplication: bool,
}

impl Default for XbmcMetadataOptions {
    fn default() -> Self {
        Self {
            user_id: None,
            release_date_format: "yyyy-MM-dd".to_owned(),
            save_image_paths_in_nfo: true,
            enable_path_substitution: true,
            enable_extra_thumbs_duplication: false,
        }
    }
}

/// A user's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)]
pub struct UserConfiguration {
    /// Gets or sets the audio language preference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_language_preference: Option<String>,

    /// Gets or sets a value indicating whether to play the default audio track.
    pub play_default_audio_track: bool,

    /// Gets or sets the subtitle language preference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_language_preference: Option<String>,

    /// Gets or sets a value indicating whether to display missing episodes.
    pub display_missing_episodes: bool,

    /// Gets or sets the grouped folders.
    #[schema(value_type = Vec<String>, format = "uuid")]
    pub grouped_folders: Vec<Uuid>,

    /// Gets or sets the subtitle mode.
    pub subtitle_mode: SubtitlePlaybackMode,

    /// Gets or sets a value indicating whether to display the collections view.
    pub display_collections_view: bool,

    /// Gets or sets a value indicating whether a local password is enabled.
    pub enable_local_password: bool,

    /// Gets or sets the ordered views.
    #[schema(value_type = Vec<String>, format = "uuid")]
    pub ordered_views: Vec<Uuid>,

    /// Gets or sets the latest items excludes.
    #[schema(value_type = Vec<String>, format = "uuid")]
    pub latest_items_excludes: Vec<Uuid>,

    /// Gets or sets the my media excludes.
    #[schema(value_type = Vec<String>, format = "uuid")]
    pub my_media_excludes: Vec<Uuid>,

    /// Gets or sets a value indicating whether to hide played items in latest.
    pub hide_played_in_latest: bool,

    /// Gets or sets a value indicating whether to remember audio selections.
    pub remember_audio_selections: bool,

    /// Gets or sets a value indicating whether to remember subtitle selections.
    pub remember_subtitle_selections: bool,

    /// Gets or sets a value indicating whether to auto-play the next episode.
    pub enable_next_episode_auto_play: bool,

    /// Gets or sets the id of the selected cast receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cast_receiver_id: Option<String>,
}

impl Default for UserConfiguration {
    fn default() -> Self {
        Self {
            audio_language_preference: None,
            play_default_audio_track: true,
            subtitle_language_preference: None,
            display_missing_episodes: false,
            grouped_folders: Vec::new(),
            subtitle_mode: SubtitlePlaybackMode::default(),
            display_collections_view: false,
            enable_local_password: false,
            ordered_views: Vec::new(),
            latest_items_excludes: Vec::new(),
            my_media_excludes: Vec::new(),
            hide_played_in_latest: true,
            remember_audio_selections: true,
            remember_subtitle_selections: true,
            enable_next_episode_auto_play: true,
            cast_receiver_id: None,
        }
    }
}

/// Metadata options for an item type.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MetadataOptions {
    /// Gets or sets the item type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_type: Option<String>,

    /// Gets or sets the disabled metadata savers.
    pub disabled_metadata_savers: Vec<String>,

    /// Gets or sets the local metadata reader order.
    pub local_metadata_reader_order: Vec<String>,

    /// Gets or sets the disabled metadata fetchers.
    pub disabled_metadata_fetchers: Vec<String>,

    /// Gets or sets the metadata fetcher order.
    pub metadata_fetcher_order: Vec<String>,

    /// Gets or sets the disabled image fetchers.
    pub disabled_image_fetchers: Vec<String>,

    /// Gets or sets the image fetcher order.
    pub image_fetcher_order: Vec<String>,
}

/// Per-type metadata/image provider options.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
// `default`: jellyfin-web's POST /Library/VirtualFolders body nests TypeOptions entries that omit
// `ImageOptions` (and may omit other arrays). Without container `default`, serde rejects the whole
// body with a 422 at the Json extractor before the handler runs. Mirrors LibraryOptions. Jellyfin's
// System.Text.Json fills missing members from default, so this is the faithful behavior.
#[serde(rename_all = "PascalCase", default)]
pub struct TypeOptions {
    /// Gets or sets the type.
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,

    /// Gets or sets the metadata fetchers.
    pub metadata_fetchers: Vec<String>,

    /// Gets or sets the metadata fetcher order.
    pub metadata_fetcher_order: Vec<String>,

    /// Gets or sets the image fetchers.
    pub image_fetchers: Vec<String>,

    /// Gets or sets the image fetcher order.
    pub image_fetcher_order: Vec<String>,

    /// Gets or sets the image options.
    pub image_options: Vec<ImageOption>,
}

/// Trickplay generation options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct TrickplayOptions {
    /// Gets or sets a value indicating whether to use HW acceleration.
    pub enable_hw_acceleration: bool,

    /// Gets or sets a value indicating whether to use HW accelerated MJPEG
    /// encoding.
    pub enable_hw_encoding: bool,

    /// Gets or sets a value indicating whether to only extract key frames.
    pub enable_key_frame_only_extraction: bool,

    /// Gets or sets the behavior used by the trickplay provider on library
    /// scan/update.
    pub scan_behavior: TrickplayScanBehavior,

    /// Gets or sets the process priority for the ffmpeg process.
    pub process_priority: ProcessPriorityClass,

    /// Gets or sets the interval, in ms, between each new trickplay image.
    pub interval: i32,

    /// Gets or sets the target width resolutions, in px.
    pub width_resolutions: Vec<i32>,

    /// Gets or sets the number of tile images to allow in the X dimension.
    pub tile_width: i32,

    /// Gets or sets the number of tile images to allow in the Y dimension.
    pub tile_height: i32,

    /// Gets or sets the ffmpeg output quality level.
    pub qscale: i32,

    /// Gets or sets the jpeg quality to use for image tiles.
    pub jpeg_quality: i32,

    /// Gets or sets the number of threads to be used by ffmpeg.
    pub process_threads: i32,
}

impl Default for TrickplayOptions {
    fn default() -> Self {
        Self {
            enable_hw_acceleration: false,
            enable_hw_encoding: false,
            enable_key_frame_only_extraction: false,
            scan_behavior: TrickplayScanBehavior::NonBlocking,
            process_priority: ProcessPriorityClass::BelowNormal,
            interval: 10_000,
            width_resolutions: vec![320],
            tile_width: 10,
            tile_height: 10,
            qscale: 4,
            jpeg_quality: 90,
            process_threads: 1,
        }
    }
}

/// Library options.
///
/// Deserialized with `#[serde(default)]` so a partial payload (e.g. the
/// `AddVirtualFolderDto`/`UpdateLibraryOptionsDto` bodies, which typically carry
/// only a handful of fields) fills the rest from [`Default`], matching Jellyfin's
/// C# model binding (which never requires the full option set on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)]
pub struct LibraryOptions {
    /// Gets or sets a value indicating whether the library is enabled.
    pub enabled: bool,

    /// Gets or sets a value indicating whether photos are enabled.
    pub enable_photos: bool,

    /// Gets or sets a value indicating whether the realtime monitor is enabled.
    pub enable_realtime_monitor: bool,

    /// Gets or sets a value indicating whether the LUFS scan is enabled.
    #[serde(rename = "EnableLUFSScan")]
    pub enable_lufs_scan: bool,

    /// Gets or sets a value indicating whether chapter image extraction is
    /// enabled.
    pub enable_chapter_image_extraction: bool,

    /// Gets or sets a value indicating whether to extract chapter images during
    /// library scan.
    pub extract_chapter_images_during_library_scan: bool,

    /// Gets or sets a value indicating whether trickplay image extraction is
    /// enabled.
    pub enable_trickplay_image_extraction: bool,

    /// Gets or sets a value indicating whether to extract trickplay images
    /// during library scan.
    pub extract_trickplay_images_during_library_scan: bool,

    /// Gets or sets the path infos.
    pub path_infos: Vec<MediaPathInfo>,

    /// Gets or sets a value indicating whether to save local metadata.
    pub save_local_metadata: bool,

    /// Gets or sets a value indicating whether internet providers are enabled.
    #[deprecated(note = "Disable remote providers in TypeOptions instead")]
    pub enable_internet_providers: bool,

    /// Gets or sets a value indicating whether automatic series grouping is
    /// enabled.
    pub enable_automatic_series_grouping: bool,

    /// Gets or sets a value indicating whether embedded titles are enabled.
    pub enable_embedded_titles: bool,

    /// Gets or sets a value indicating whether embedded extras titles are
    /// enabled.
    pub enable_embedded_extras_titles: bool,

    /// Gets or sets a value indicating whether embedded episode infos are
    /// enabled.
    pub enable_embedded_episode_infos: bool,

    /// Gets or sets the automatic refresh interval in days.
    pub automatic_refresh_interval_days: i32,

    /// Gets or sets the preferred metadata language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_metadata_language: Option<String>,

    /// Gets or sets the metadata country code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_country_code: Option<String>,

    /// Gets or sets the season zero display name.
    pub season_zero_display_name: String,

    /// Gets or sets the metadata savers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_savers: Option<Vec<String>>,

    /// Gets or sets the disabled local metadata readers.
    pub disabled_local_metadata_readers: Vec<String>,

    /// Gets or sets the local metadata reader order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_metadata_reader_order: Option<Vec<String>>,

    /// Gets or sets the disabled subtitle fetchers.
    pub disabled_subtitle_fetchers: Vec<String>,

    /// Gets or sets the subtitle fetcher order.
    pub subtitle_fetcher_order: Vec<String>,

    /// Gets or sets the disabled media segment providers.
    pub disabled_media_segment_providers: Vec<String>,

    /// Gets or sets the media segment provider order.
    pub media_segment_provider_order: Vec<String>,

    /// Gets or sets a value indicating whether to skip subtitles if embedded
    /// subtitles are present.
    pub skip_subtitles_if_embedded_subtitles_present: bool,

    /// Gets or sets a value indicating whether to skip subtitles if the audio
    /// track matches.
    pub skip_subtitles_if_audio_track_matches: bool,

    /// Gets or sets the subtitle download languages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_download_languages: Option<Vec<String>>,

    /// Gets or sets a value indicating whether a perfect subtitle match is
    /// required.
    pub require_perfect_subtitle_match: bool,

    /// Gets or sets a value indicating whether to save subtitles with media.
    pub save_subtitles_with_media: bool,

    /// Gets or sets a value indicating whether to save lyrics with media.
    pub save_lyrics_with_media: bool,

    /// Gets or sets a value indicating whether to save trickplay with media.
    pub save_trickplay_with_media: bool,

    /// Gets or sets the disabled lyric fetchers.
    pub disabled_lyric_fetchers: Vec<String>,

    /// Gets or sets the lyric fetcher order.
    pub lyric_fetcher_order: Vec<String>,

    /// Gets or sets a value indicating whether to prefer the nonstandard
    /// artists tag.
    pub prefer_nonstandard_artists_tag: bool,

    /// Gets or sets a value indicating whether to use custom tag delimiters.
    pub use_custom_tag_delimiters: bool,

    /// Gets or sets the custom tag delimiters.
    pub custom_tag_delimiters: Vec<String>,

    /// Gets or sets the delimiter whitelist.
    pub delimiter_whitelist: Vec<String>,

    /// Gets or sets a value indicating whether to automatically add to a
    /// collection.
    pub automatically_add_to_collection: bool,

    /// Gets or sets a value indicating whether embedded subtitles are allowed.
    pub allow_embedded_subtitles: EmbeddedSubtitleOptions,

    /// Gets or sets the type options.
    pub type_options: Vec<TypeOptions>,
}

impl Default for LibraryOptions {
    fn default() -> Self {
        #[allow(deprecated)]
        Self {
            enabled: true,
            enable_photos: true,
            // Jellyfin's LibraryOptions ctor default: realtime monitoring on.
            // (It was false while Ferrofin's watcher was a no-op.)
            enable_realtime_monitor: true,
            enable_lufs_scan: false,
            enable_chapter_image_extraction: false,
            extract_chapter_images_during_library_scan: false,
            enable_trickplay_image_extraction: false,
            extract_trickplay_images_during_library_scan: false,
            path_infos: Vec::new(),
            save_local_metadata: false,
            enable_internet_providers: false,
            enable_automatic_series_grouping: true,
            enable_embedded_titles: false,
            enable_embedded_extras_titles: false,
            enable_embedded_episode_infos: false,
            automatic_refresh_interval_days: 0,
            preferred_metadata_language: None,
            metadata_country_code: None,
            season_zero_display_name: "Specials".to_owned(),
            metadata_savers: None,
            disabled_local_metadata_readers: Vec::new(),
            local_metadata_reader_order: None,
            disabled_subtitle_fetchers: Vec::new(),
            subtitle_fetcher_order: Vec::new(),
            disabled_media_segment_providers: Vec::new(),
            media_segment_provider_order: Vec::new(),
            skip_subtitles_if_embedded_subtitles_present: false,
            skip_subtitles_if_audio_track_matches: true,
            subtitle_download_languages: None,
            require_perfect_subtitle_match: true,
            save_subtitles_with_media: true,
            save_lyrics_with_media: false,
            save_trickplay_with_media: false,
            disabled_lyric_fetchers: Vec::new(),
            lyric_fetcher_order: Vec::new(),
            prefer_nonstandard_artists_tag: false,
            use_custom_tag_delimiters: false,
            custom_tag_delimiters: vec!["/".into(), "|".into(), ";".into(), "\\".into()],
            delimiter_whitelist: Vec::new(),
            automatically_add_to_collection: false,
            allow_embedded_subtitles: EmbeddedSubtitleOptions::AllowAll,
            type_options: Vec::new(),
        }
    }
}

/// FFmpeg encoding options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct EncodingOptions {
    /// Gets or sets the thread count used for encoding.
    pub encoding_thread_count: i32,

    /// Gets or sets the temporary transcoding path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcoding_temp_path: Option<String>,

    /// Gets or sets the path to the fallback font.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_font_path: Option<String>,

    /// Gets or sets a value indicating whether to use the fallback font.
    pub enable_fallback_font: bool,

    /// Gets or sets a value indicating whether audio VBR is enabled.
    pub enable_audio_vbr: bool,

    /// Gets or sets the audio boost applied when downmixing audio.
    pub down_mix_audio_boost: f64,

    /// Gets or sets the algorithm used for downmixing audio to stereo.
    pub down_mix_stereo_algorithm: DownMixStereoAlgorithms,

    /// Gets or sets the maximum size of the muxing queue.
    pub max_muxing_queue_size: i32,

    /// Gets or sets a value indicating whether throttling is enabled.
    pub enable_throttling: bool,

    /// Gets or sets the delay after which throttling happens.
    pub throttle_delay_seconds: i32,

    /// Gets or sets a value indicating whether segment deletion is enabled.
    pub enable_segment_deletion: bool,

    /// Gets or sets seconds for which segments should be kept before deletion.
    pub segment_keep_seconds: i32,

    /// Gets or sets the hardware acceleration type.
    pub hardware_acceleration_type: HardwareAccelerationType,

    /// Gets or sets the FFmpeg path as set by the user via the UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_app_path: Option<String>,

    /// Gets or sets the current FFmpeg path being used by the system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_app_path_display: Option<String>,

    /// Gets or sets the VA-API device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vaapi_device: Option<String>,

    /// Gets or sets the QSV device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qsv_device: Option<String>,

    /// Gets or sets a value indicating whether tonemapping is enabled.
    pub enable_tonemapping: bool,

    /// Gets or sets a value indicating whether VPP tonemapping is enabled.
    pub enable_vpp_tonemapping: bool,

    /// Gets or sets a value indicating whether videotoolbox tonemapping is
    /// enabled.
    pub enable_video_toolbox_tonemapping: bool,

    /// Gets or sets the tone-mapping algorithm.
    pub tonemapping_algorithm: TonemappingAlgorithm,

    /// Gets or sets the tone-mapping mode.
    pub tonemapping_mode: TonemappingMode,

    /// Gets or sets the tone-mapping range.
    pub tonemapping_range: TonemappingRange,

    /// Gets or sets the tone-mapping desaturation.
    pub tonemapping_desat: f64,

    /// Gets or sets the tone-mapping peak.
    pub tonemapping_peak: f64,

    /// Gets or sets the tone-mapping parameters.
    pub tonemapping_param: f64,

    /// Gets or sets the VPP tone-mapping brightness.
    pub vpp_tonemapping_brightness: f64,

    /// Gets or sets the VPP tone-mapping contrast.
    pub vpp_tonemapping_contrast: f64,

    /// Gets or sets the H264 CRF.
    #[serde(rename = "H264Crf")]
    pub h264_crf: i32,

    /// Gets or sets the H265 CRF.
    #[serde(rename = "H265Crf")]
    pub h265_crf: i32,

    /// Gets or sets the encoder preset.
    pub encoder_preset: EncoderPreset,

    /// Gets or sets a value indicating whether the framerate is doubled when
    /// deinterlacing.
    pub deinterlace_double_rate: bool,

    /// Gets or sets the deinterlace method.
    pub deinterlace_method: DeinterlaceMethod,

    /// Gets or sets a value indicating whether 10bit HEVC decoding is enabled.
    #[serde(rename = "EnableDecodingColorDepth10Hevc")]
    pub enable_decoding_color_depth10_hevc: bool,

    /// Gets or sets a value indicating whether 10bit VP9 decoding is enabled.
    #[serde(rename = "EnableDecodingColorDepth10Vp9")]
    pub enable_decoding_color_depth10_vp9: bool,

    /// Gets or sets a value indicating whether 8/10bit HEVC RExt decoding is
    /// enabled.
    #[serde(rename = "EnableDecodingColorDepth10HevcRext")]
    pub enable_decoding_color_depth10_hevc_rext: bool,

    /// Gets or sets a value indicating whether 12bit HEVC RExt decoding is
    /// enabled.
    #[serde(rename = "EnableDecodingColorDepth12HevcRext")]
    pub enable_decoding_color_depth12_hevc_rext: bool,

    /// Gets or sets a value indicating whether the enhanced NVDEC is enabled.
    pub enable_enhanced_nvdec_decoder: bool,

    /// Gets or sets a value indicating whether the system native hardware
    /// decoder should be used.
    pub prefer_system_native_hw_decoder: bool,

    /// Gets or sets a value indicating whether the Intel H264 low-power hardware
    /// encoder should be used.
    #[serde(rename = "EnableIntelLowPowerH264HwEncoder")]
    pub enable_intel_low_power_h264_hw_encoder: bool,

    /// Gets or sets a value indicating whether the Intel HEVC low-power hardware
    /// encoder should be used.
    pub enable_intel_low_power_hevc_hw_encoder: bool,

    /// Gets or sets a value indicating whether hardware encoding is enabled.
    pub enable_hardware_encoding: bool,

    /// Gets or sets a value indicating whether HEVC encoding is enabled.
    pub allow_hevc_encoding: bool,

    /// Gets or sets a value indicating whether AV1 encoding is enabled.
    #[serde(rename = "AllowAv1Encoding")]
    pub allow_av1_encoding: bool,

    /// Gets or sets a value indicating whether subtitle extraction is enabled.
    pub enable_subtitle_extraction: bool,

    /// Gets or sets the timeout for subtitle extraction in minutes.
    pub subtitle_extraction_timeout_minutes: i32,

    /// Gets or sets the codecs hardware decoding is used for.
    pub hardware_decoding_codecs: Vec<String>,

    /// Gets or sets the file extensions on-demand metadata based keyframe
    /// extraction is enabled for.
    pub allow_on_demand_metadata_based_keyframe_extraction_for_extensions: Vec<String>,

    /// Gets or sets the method used for audio seeking in HLS.
    pub hls_audio_seek_strategy: HlsAudioSeekStrategy,
}

impl Default for EncodingOptions {
    fn default() -> Self {
        Self {
            encoding_thread_count: -1,
            transcoding_temp_path: None,
            fallback_font_path: None,
            enable_fallback_font: false,
            enable_audio_vbr: false,
            down_mix_audio_boost: 2.0,
            down_mix_stereo_algorithm: DownMixStereoAlgorithms::None,
            max_muxing_queue_size: 2048,
            enable_throttling: false,
            throttle_delay_seconds: 180,
            enable_segment_deletion: false,
            segment_keep_seconds: 720,
            hardware_acceleration_type: HardwareAccelerationType::none,
            encoder_app_path: None,
            encoder_app_path_display: None,
            vaapi_device: Some("/dev/dri/renderD128".to_owned()),
            qsv_device: Some(String::new()),
            enable_tonemapping: false,
            enable_vpp_tonemapping: false,
            enable_video_toolbox_tonemapping: false,
            tonemapping_algorithm: TonemappingAlgorithm::bt2390,
            tonemapping_mode: TonemappingMode::auto,
            tonemapping_range: TonemappingRange::auto,
            tonemapping_desat: 0.0,
            tonemapping_peak: 100.0,
            tonemapping_param: 0.0,
            vpp_tonemapping_brightness: 16.0,
            vpp_tonemapping_contrast: 1.0,
            h264_crf: 23,
            h265_crf: 28,
            encoder_preset: EncoderPreset::auto,
            deinterlace_double_rate: false,
            deinterlace_method: DeinterlaceMethod::yadif,
            enable_decoding_color_depth10_hevc: true,
            enable_decoding_color_depth10_vp9: true,
            enable_decoding_color_depth10_hevc_rext: false,
            enable_decoding_color_depth12_hevc_rext: false,
            enable_enhanced_nvdec_decoder: true,
            prefer_system_native_hw_decoder: true,
            enable_intel_low_power_h264_hw_encoder: false,
            enable_intel_low_power_hevc_hw_encoder: false,
            enable_hardware_encoding: true,
            allow_hevc_encoding: false,
            allow_av1_encoding: false,
            enable_subtitle_extraction: true,
            subtitle_extraction_timeout_minutes: 30,
            hardware_decoding_codecs: vec!["h264".to_owned(), "vc1".to_owned()],
            allow_on_demand_metadata_based_keyframe_extraction_for_extensions: vec![
                "mkv".to_owned(),
            ],
            hls_audio_seek_strategy: HlsAudioSeekStrategy::TrimCopiedAudio,
        }
    }
}

/// The server configuration. Flattens the C# base
/// `BaseApplicationConfiguration`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ServerConfiguration {
    // --- Flattened BaseApplicationConfiguration fields ---
    /// Gets or sets the number of days log files should be retained.
    pub log_file_retention_days: i32,

    /// Gets or sets a value indicating whether the startup wizard is completed.
    pub is_startup_wizard_completed: bool,

    /// Gets or sets the cache path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_path: Option<String>,

    /// Gets or sets the previous version (structured; `null` when unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,

    /// Gets or sets the stringified previous version stored/loaded from config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_version_str: Option<String>,

    // --- ServerConfiguration-specific fields ---
    /// Gets or sets a value indicating whether prometheus metrics exporting is
    /// enabled.
    pub enable_metrics: bool,

    /// Gets or sets a value indicating whether normalized item-by-name ids are
    /// enabled.
    pub enable_normalized_item_by_name_ids: bool,

    /// Gets or sets a value indicating whether this instance is port
    /// authorized.
    pub is_port_authorized: bool,

    /// Gets or sets a value indicating whether quick connect is available.
    pub quick_connect_available: bool,

    /// Gets or sets a value indicating whether case-sensitive item ids are
    /// enabled.
    pub enable_case_sensitive_item_ids: bool,

    /// Gets or sets a value indicating whether to disable the live TV channel
    /// user data name.
    pub disable_live_tv_channel_user_data_name: bool,

    /// Gets or sets the metadata path.
    pub metadata_path: String,

    /// Gets or sets the preferred metadata language.
    pub preferred_metadata_language: String,

    /// Gets or sets the metadata country code.
    pub metadata_country_code: String,

    /// Gets or sets the sort replace characters.
    pub sort_replace_characters: Vec<String>,

    /// Gets or sets the sort remove characters.
    pub sort_remove_characters: Vec<String>,

    /// Gets or sets the sort remove words.
    pub sort_remove_words: Vec<String>,

    /// Gets or sets the minimum resume percentage.
    pub min_resume_pct: i32,

    /// Gets or sets the maximum resume percentage.
    pub max_resume_pct: i32,

    /// Gets or sets the minimum resume duration in seconds.
    pub min_resume_duration_seconds: i32,

    /// Gets or sets the minimum audiobook resume in minutes.
    pub min_audiobook_resume: i32,

    /// Gets or sets the maximum audiobook resume in minutes.
    pub max_audiobook_resume: i32,

    /// Gets or sets the inactive session threshold in minutes.
    pub inactive_session_threshold: i32,

    /// Gets or sets the library monitor delay in seconds.
    pub library_monitor_delay: i32,

    /// Gets or sets the library update duration in seconds.
    pub library_update_duration: i32,

    /// Gets or sets the maximum number of items to cache.
    pub cache_size: i32,

    /// Gets or sets the image saving convention.
    pub image_saving_convention: ImageSavingConvention,

    /// Gets or sets the metadata options.
    pub metadata_options: Vec<MetadataOptions>,

    /// Gets or sets a value indicating whether to skip deserialization for
    /// basic types.
    pub skip_deserialization_for_basic_types: bool,

    /// Gets or sets the server name.
    pub server_name: String,

    /// Gets or sets the UI culture.
    #[serde(rename = "UICulture")]
    pub ui_culture: String,

    /// Gets or sets a value indicating whether to save metadata hidden.
    pub save_metadata_hidden: bool,

    /// Gets or sets the content types.
    pub content_types: Vec<NameValuePair>,

    /// Gets or sets the remote client bitrate limit.
    pub remote_client_bitrate_limit: i32,

    /// Gets or sets a value indicating whether the folder view is enabled.
    pub enable_folder_view: bool,

    /// Gets or sets a value indicating whether to group movies into
    /// collections.
    pub enable_grouping_movies_into_collections: bool,

    /// Gets or sets a value indicating whether to group shows into collections.
    pub enable_grouping_shows_into_collections: bool,

    /// Gets or sets a value indicating whether to display specials within
    /// seasons.
    pub display_specials_within_seasons: bool,

    /// Gets or sets the codecs used.
    pub codecs_used: Vec<String>,

    /// Gets or sets the plugin repositories.
    pub plugin_repositories: Vec<RepositoryInfo>,

    /// Gets or sets a value indicating whether external content is enabled in
    /// suggestions.
    pub enable_external_content_in_suggestions: bool,

    /// Gets or sets the image extraction timeout in ms.
    pub image_extraction_timeout_ms: i32,

    /// Gets or sets the path substitutions.
    pub path_substitutions: Vec<PathSubstitution>,

    /// Gets or sets a value indicating whether slow response warnings are
    /// enabled.
    pub enable_slow_response_warning: bool,

    /// Gets or sets the slow response threshold in ms.
    pub slow_response_threshold_ms: i64,

    /// Gets or sets the cors hosts.
    pub cors_hosts: Vec<String>,

    /// Gets or sets the number of days activity logs should be retained.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_log_retention_days: Option<i32>,

    /// Gets or sets the library scan fanout concurrency.
    pub library_scan_fanout_concurrency: i32,

    /// Gets or sets the library metadata refresh concurrency.
    pub library_metadata_refresh_concurrency: i32,

    /// Gets or sets a value indicating whether clients may upload logs.
    pub allow_client_log_upload: bool,

    /// Gets or sets the dummy chapter duration in seconds.
    pub dummy_chapter_duration: i32,

    /// Gets or sets the chapter image resolution.
    pub chapter_image_resolution: ImageResolution,

    /// Gets or sets the limit for parallel image encoding.
    pub parallel_image_encoding_limit: i32,

    /// Gets or sets the list of cast receiver applications.
    pub cast_receiver_applications: Vec<CastReceiverApplication>,

    /// Gets or sets the trickplay options.
    pub trickplay_options: TrickplayOptions,

    /// Gets or sets a value indicating whether legacy authorization is enabled.
    pub enable_legacy_authorization: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_defaults_match_csharp() {
        assert_eq!(
            ImageSavingConvention::default(),
            ImageSavingConvention::Legacy
        );
        assert_eq!(
            HlsAudioSeekStrategy::default(),
            HlsAudioSeekStrategy::TrimCopiedAudio
        );
        assert_eq!(
            TrickplayScanBehavior::default(),
            TrickplayScanBehavior::NonBlocking
        );
        assert_eq!(
            ProcessPriorityClass::default(),
            ProcessPriorityClass::BelowNormal
        );
        assert_eq!(
            MetadataPluginType::default(),
            MetadataPluginType::LocalImageProvider
        );
        assert_eq!(
            EmbeddedSubtitleOptions::default(),
            EmbeddedSubtitleOptions::AllowAll
        );
        assert_eq!(
            SubtitlePlaybackMode::default(),
            SubtitlePlaybackMode::Default
        );
    }

    #[test]
    fn enum_wire_names_are_pascal_case() {
        assert_eq!(
            serde_json::to_value(ProcessPriorityClass::RealTime).unwrap(),
            "RealTime"
        );
        assert_eq!(
            serde_json::to_value(EmbeddedSubtitleOptions::AllowNone).unwrap(),
            "AllowNone"
        );
        assert_eq!(
            serde_json::to_value(SubtitlePlaybackMode::OnlyForced).unwrap(),
            "OnlyForced"
        );
        assert_eq!(
            serde_json::to_value(MetadataPluginType::MediaSegmentProvider).unwrap(),
            "MediaSegmentProvider"
        );
    }

    #[test]
    fn image_option_default_and_type_rename() {
        let opt = ImageOption::default();
        assert_eq!(opt.limit, 1);
        assert_eq!(opt.min_width, 0);
        let json = serde_json::to_value(opt).unwrap();
        // `type_` is renamed to `Type` on the wire.
        assert!(json.get("Type").is_some());
        assert_eq!(json["Limit"], 1);
        assert_eq!(json["MinWidth"], 0);
    }

    #[test]
    fn metadata_configuration_default_is_true() {
        let cfg = MetadataConfiguration::default();
        assert!(cfg.use_file_creation_time_for_date_added);
        let json = serde_json::to_value(cfg).unwrap();
        assert_eq!(json["UseFileCreationTimeForDateAdded"], true);
    }

    #[test]
    fn xbmc_metadata_options_default() {
        let opts = XbmcMetadataOptions::default();
        assert_eq!(opts.release_date_format, "yyyy-MM-dd");
        assert!(opts.save_image_paths_in_nfo);
        assert!(opts.enable_path_substitution);
        assert!(!opts.enable_extra_thumbs_duplication);
        let json = serde_json::to_value(&opts).unwrap();
        // Explicit rename check.
        assert_eq!(json["SaveImagePathsInNfo"], true);
        assert!(json.get("UserId").is_none());
    }

    #[test]
    fn user_configuration_default_matches_csharp() {
        let cfg = UserConfiguration::default();
        assert!(cfg.play_default_audio_track);
        assert!(!cfg.display_missing_episodes);
        assert!(cfg.hide_played_in_latest);
        assert!(cfg.remember_audio_selections);
        assert!(cfg.remember_subtitle_selections);
        assert!(cfg.enable_next_episode_auto_play);
        assert_eq!(cfg.subtitle_mode, SubtitlePlaybackMode::Default);
        assert!(cfg.grouped_folders.is_empty());
    }

    #[test]
    fn user_configuration_round_trips() {
        let cfg = UserConfiguration {
            audio_language_preference: Some("eng".to_owned()),
            grouped_folders: vec![Uuid::from_u128(3)],
            cast_receiver_id: Some("cast".to_owned()),
            ..UserConfiguration::default()
        };
        let back: UserConfiguration =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(cfg, back);
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["AudioLanguagePreference"], "eng");
        assert_eq!(json["PlayDefaultAudioTrack"], true);
    }

    #[test]
    fn trickplay_options_default() {
        let opts = TrickplayOptions::default();
        assert_eq!(opts.interval, 10_000);
        assert_eq!(opts.width_resolutions, vec![320]);
        assert_eq!(opts.tile_width, 10);
        assert_eq!(opts.tile_height, 10);
        assert_eq!(opts.qscale, 4);
        assert_eq!(opts.jpeg_quality, 90);
        assert_eq!(opts.process_threads, 1);
        assert_eq!(opts.scan_behavior, TrickplayScanBehavior::NonBlocking);
        assert_eq!(opts.process_priority, ProcessPriorityClass::BelowNormal);
    }

    #[test]
    fn library_options_default_matches_csharp() {
        #[allow(deprecated)]
        let opts = LibraryOptions::default();
        assert!(opts.enabled);
        assert!(opts.enable_photos);
        assert_eq!(opts.season_zero_display_name, "Specials");
        assert!(opts.skip_subtitles_if_audio_track_matches);
        assert!(opts.require_perfect_subtitle_match);
        assert!(opts.save_subtitles_with_media);
        assert_eq!(
            opts.custom_tag_delimiters,
            vec![
                "/".to_owned(),
                "|".to_owned(),
                ";".to_owned(),
                "\\".to_owned()
            ]
        );
        assert_eq!(
            opts.allow_embedded_subtitles,
            EmbeddedSubtitleOptions::AllowAll
        );
    }

    #[test]
    fn library_options_round_trips() {
        let opts = LibraryOptions::default();
        let back: LibraryOptions =
            serde_json::from_str(&serde_json::to_string(&opts).unwrap()).unwrap();
        assert_eq!(opts, back);
    }

    #[test]
    fn library_options_wire_renames() {
        let opts = LibraryOptions::default();
        let json = serde_json::to_value(&opts).unwrap();
        assert_eq!(json["EnableLUFSScan"], false);
        assert_eq!(json["Enabled"], true);
    }

    #[test]
    fn encoding_options_default_matches_csharp() {
        let opts = EncodingOptions::default();
        assert_eq!(opts.encoding_thread_count, -1);
        assert!((opts.down_mix_audio_boost - 2.0).abs() < f64::EPSILON);
        assert_eq!(opts.max_muxing_queue_size, 2048);
        assert_eq!(opts.throttle_delay_seconds, 180);
        assert_eq!(opts.segment_keep_seconds, 720);
        assert_eq!(opts.h264_crf, 23);
        assert_eq!(opts.h265_crf, 28);
        assert_eq!(opts.vaapi_device.as_deref(), Some("/dev/dri/renderD128"));
        assert_eq!(opts.hardware_decoding_codecs, vec!["h264", "vc1"]);
        assert_eq!(
            opts.allow_on_demand_metadata_based_keyframe_extraction_for_extensions,
            vec!["mkv"]
        );
    }

    #[test]
    fn encoding_options_round_trips_and_renames() {
        let opts = EncodingOptions::default();
        let json = serde_json::to_value(&opts).unwrap();
        assert_eq!(json["H264Crf"], 23);
        assert_eq!(json["H265Crf"], 28);
        assert_eq!(json["EnableDecodingColorDepth10Hevc"], true);
        assert_eq!(json["AllowAv1Encoding"], false);
        let back: EncodingOptions = serde_json::from_value(json).unwrap();
        assert_eq!(opts, back);
    }

    #[test]
    fn server_configuration_default_and_round_trips() {
        let cfg = ServerConfiguration::default();
        let json = serde_json::to_value(cfg.clone()).unwrap();
        // A renamed field is exercised.
        assert!(json.get("UICulture").is_some());
        let back: ServerConfiguration = serde_json::from_value(json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn small_structs_round_trip() {
        let mpi = MediaPathInfo {
            path: "/x".to_owned(),
        };
        assert_eq!(
            serde_json::from_str::<MediaPathInfo>(&serde_json::to_string(&mpi).unwrap()).unwrap(),
            mpi
        );
        assert_eq!(serde_json::to_value(&mpi).unwrap()["Path"], "/x");

        let ps = PathSubstitution {
            from: "/a".to_owned(),
            to: "/b".to_owned(),
        };
        let json = serde_json::to_value(&ps).unwrap();
        assert_eq!(json["From"], "/a");
        assert_eq!(json["To"], "/b");

        let plugin = MetadataPlugin {
            name: Some("nfo".to_owned()),
            type_: MetadataPluginType::MetadataSaver,
        };
        let json = serde_json::to_value(&plugin).unwrap();
        assert_eq!(json["Name"], "nfo");
        assert_eq!(json["Type"], "MetadataSaver");
    }

    #[test]
    fn config_default_impls_are_constructible_and_round_trip() {
        // Exercise each hand-written Default body and confirm it survives a JSON
        // round-trip (these Defaults carry the non-trivial C# initializers).
        let xbmc = XbmcMetadataOptions::default();
        assert_eq!(
            serde_json::from_str::<XbmcMetadataOptions>(&serde_json::to_string(&xbmc).unwrap())
                .unwrap(),
            xbmc
        );

        let user = UserConfiguration::default();
        assert_eq!(
            serde_json::from_str::<UserConfiguration>(&serde_json::to_string(&user).unwrap())
                .unwrap(),
            user
        );

        let trickplay = TrickplayOptions::default();
        assert_eq!(
            serde_json::from_str::<TrickplayOptions>(&serde_json::to_string(&trickplay).unwrap())
                .unwrap(),
            trickplay
        );

        // Populate every `Option` field so the serde `skip_serializing_if`
        // Some-branch (and the field serialization) is exercised, then round-trip.
        let library = LibraryOptions {
            local_metadata_reader_order: Some(vec!["nfo".to_owned()]),
            subtitle_download_languages: Some(vec!["eng".to_owned()]),
            metadata_country_code: Some("US".to_owned()),
            preferred_metadata_language: Some("en".to_owned()),
            season_zero_display_name: "Specials".to_owned(),
            path_infos: vec![MediaPathInfo {
                path: "/media".to_owned(),
            }],
            ..LibraryOptions::default()
        };
        assert_eq!(
            serde_json::from_str::<LibraryOptions>(&serde_json::to_string(&library).unwrap())
                .unwrap(),
            library
        );

        let encoding = EncodingOptions::default();
        assert_eq!(encoding.encoding_thread_count, -1);
        assert_eq!(
            serde_json::from_str::<EncodingOptions>(&serde_json::to_string(&encoding).unwrap())
                .unwrap(),
            encoding
        );

        let metadata = MetadataConfiguration::default();
        assert!(metadata.use_file_creation_time_for_date_added);

        // TypeOptions with a populated `Type` covers its serde field code.
        let type_options = TypeOptions {
            type_: Some("Movie".to_owned()),
            metadata_fetchers: vec!["Tmdb".to_owned()],
            metadata_fetcher_order: vec!["Tmdb".to_owned()],
            image_fetchers: vec!["Tmdb".to_owned()],
            image_fetcher_order: vec!["Tmdb".to_owned()],
            ..TypeOptions::default()
        };
        let json = serde_json::to_string(&type_options).unwrap();
        assert_eq!(
            serde_json::from_str::<TypeOptions>(&json).unwrap(),
            type_options
        );
    }

    #[test]
    fn encoding_options_round_trips_from_json_map() {
        // Deserialize an EncodingOptions from an explicit JSON object so every
        // renamed field's visitor arm (H265Crf, EnableDecodingColorDepth10Hevc,
        // …) is exercised, then re-serialize.
        let value = serde_json::to_value(EncodingOptions::default()).unwrap();
        assert!(value.is_object());
        // The renamed keys are present in the serialized form.
        assert!(value.get("H265Crf").is_some());
        assert!(value.get("EnableDecodingColorDepth10Hevc").is_some());
        let back: EncodingOptions = serde_json::from_value(value).unwrap();
        assert_eq!(back, EncodingOptions::default());
    }

    #[test]
    fn server_configuration_round_trips() {
        // The top-level ServerConfiguration nests many of the above structs; a
        // full round-trip exercises their serde field code together.
        let cfg = ServerConfiguration::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ServerConfiguration = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn library_options_result_dto_uses_pascal_case_and_round_trips() {
        let dto = LibraryOptionsResultDto {
            type_options: vec![LibraryTypeOptionsDto {
                type_: Some("Movie".to_owned()),
                default_image_options: vec![ImageOption::default()],
                supported_image_types: vec![ImageType::Primary],
                metadata_fetchers: vec![LibraryOptionInfoDto {
                    name: Some("Tmdb".to_owned()),
                    default_enabled: true,
                }],
                ..LibraryTypeOptionsDto::default()
            }],
            ..LibraryOptionsResultDto::default()
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["TypeOptions"][0]["Type"], "Movie");
        assert_eq!(
            json["TypeOptions"][0]["MetadataFetchers"][0]["Name"],
            "Tmdb"
        );
        assert_eq!(
            json["TypeOptions"][0]["MetadataFetchers"][0]["DefaultEnabled"],
            true
        );
        assert!(json["MetadataSavers"].as_array().unwrap().is_empty());
        let back: LibraryOptionsResultDto = serde_json::from_value(json).unwrap();
        assert_eq!(dto, back);
    }

    #[test]
    fn type_options_deserializes_without_image_options() {
        // jellyfin-web's POST /Library/VirtualFolders nests TypeOptions omitting ImageOptions.
        // Regression: without container `default` this failed serde with "missing field ImageOptions",
        // which axum's Json extractor surfaced as a 422 before the handler ran.
        let body = serde_json::json!({
            "Type": "Series",
            "MetadataFetchers": ["TheMovieDb"],
            "MetadataFetcherOrder": ["TheMovieDb"],
            "ImageFetchers": ["TheMovieDb"],
            "ImageFetcherOrder": ["TheMovieDb"]
        });
        let opts: TypeOptions = serde_json::from_value(body).unwrap();
        assert_eq!(opts.type_.as_deref(), Some("Series"));
        assert_eq!(opts.metadata_fetchers, ["TheMovieDb"]);
        assert!(opts.image_options.is_empty());

        // And the full LibraryOptions wrapper with such a TypeOptions array must deserialize too.
        let lib = serde_json::json!({
            "TypeOptions": [{
                "Type": "Movie",
                "MetadataFetchers": [], "MetadataFetcherOrder": [],
                "ImageFetchers": [], "ImageFetcherOrder": []
            }]
        });
        let parsed: LibraryOptions = serde_json::from_value(lib).unwrap();
        assert_eq!(parsed.type_options.len(), 1);
        assert!(parsed.type_options[0].image_options.is_empty());
    }
}
