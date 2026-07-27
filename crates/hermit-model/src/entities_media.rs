//! Media-description entities — port of the struct/trait half of
//! `MediaBrowser.Model.Entities` (the enums live in [`crate::entities`]).
//!
//! [`MediaStream`] carries the derived display-title / resolution-text logic
//! that `StreamBuilder` reads. [`MetadataProvider`] plus the [`IHasProviderIds`]
//! trait and its extension helpers describe external ids. Serde casing matches
//! the Jellyfin JSON contract (PascalCase properties).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::configuration::LibraryOptions;
use crate::data::{VideoRange, VideoRangeType};
use crate::dlna::SubtitleDeliveryMethod;
use crate::entities::{CollectionTypeOptions, MediaStreamType};
use crate::extensions;
use crate::media_info::audio_codec;

/// An enum representing formats of spatial audio.
///
/// Upstream this lives in `Jellyfin.Data.Enums`, but it is pulled in here
/// because [`MediaStream`] derives it from the codec profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum AudioSpatialFormat {
    /// None audio spatial format.
    #[default]
    None,
    /// Dolby Atmos audio spatial format.
    DolbyAtmos,
    /// DTS:X audio spatial format.
    #[serde(rename = "DTSX")]
    Dtsx,
}

/// Enum `MetadataProvider` — well-known external metadata sources.
///
/// The discriminants mirror the upstream enum exactly (there are gaps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum MetadataProvider {
    /// This metadata provider is for users and/or plugins to override the
    /// default merging behaviour.
    Custom = 0,
    /// The IMDb provider.
    Imdb = 2,
    /// The TMDb provider.
    Tmdb = 3,
    /// The TVDb provider.
    Tvdb = 4,
    /// The tvcom provider.
    Tvcom = 5,
    /// TMDb collection provider.
    TmdbCollection = 7,
    /// The `MusicBrainz` album provider.
    MusicBrainzAlbum = 8,
    /// The `MusicBrainz` album artist provider.
    MusicBrainzAlbumArtist = 9,
    /// The `MusicBrainz` artist provider.
    MusicBrainzArtist = 10,
    /// The `MusicBrainz` release group provider.
    MusicBrainzReleaseGroup = 11,
    /// The Zap2It provider.
    Zap2It = 12,
    /// The `TvRage` provider.
    TvRage = 15,
    /// The `AudioDb` artist provider.
    AudioDbArtist = 16,
    /// The `AudioDb` collection provider.
    AudioDbAlbum = 17,
    /// The `MusicBrainz` track provider.
    MusicBrainzTrack = 18,
    /// The `TvMaze` provider.
    TvMaze = 19,
    /// The `MusicBrainz` recording provider.
    MusicBrainzRecording = 20,
}

impl MetadataProvider {
    /// Returns the C# `ToString()` name of this provider, used as the
    /// canonical key in a provider-id dictionary.
    #[must_use]
    pub fn as_name(self) -> &'static str {
        match self {
            Self::Custom => "Custom",
            Self::Imdb => "Imdb",
            Self::Tmdb => "Tmdb",
            Self::Tvdb => "Tvdb",
            Self::Tvcom => "Tvcom",
            Self::TmdbCollection => "TmdbCollection",
            Self::MusicBrainzAlbum => "MusicBrainzAlbum",
            Self::MusicBrainzAlbumArtist => "MusicBrainzAlbumArtist",
            Self::MusicBrainzArtist => "MusicBrainzArtist",
            Self::MusicBrainzReleaseGroup => "MusicBrainzReleaseGroup",
            Self::Zap2It => "Zap2It",
            Self::TvRage => "TvRage",
            Self::AudioDbArtist => "AudioDbArtist",
            Self::AudioDbAlbum => "AudioDbAlbum",
            Self::MusicBrainzTrack => "MusicBrainzTrack",
            Self::TvMaze => "TvMaze",
            Self::MusicBrainzRecording => "MusicBrainzRecording",
        }
    }

    /// All variants, in declaration order (used to build the case-insensitive
    /// canonicalization table).
    #[must_use]
    pub fn all() -> &'static [MetadataProvider] {
        &[
            Self::Custom,
            Self::Imdb,
            Self::Tmdb,
            Self::Tvdb,
            Self::Tvcom,
            Self::TmdbCollection,
            Self::MusicBrainzAlbum,
            Self::MusicBrainzAlbumArtist,
            Self::MusicBrainzArtist,
            Self::MusicBrainzReleaseGroup,
            Self::Zap2It,
            Self::TvRage,
            Self::AudioDbArtist,
            Self::AudioDbAlbum,
            Self::MusicBrainzTrack,
            Self::TvMaze,
            Self::MusicBrainzRecording,
        ]
    }
}

/// Class `ChapterInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ChapterInfo {
    /// The start position ticks.
    pub start_position_ticks: i64,
    /// The name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The image path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,
    /// The image date modified.
    #[schema(value_type = String, format = "date-time")]
    pub image_date_modified: chrono::DateTime<chrono::Utc>,
    /// The image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,
}

/// Class `MediaAttachment`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MediaAttachment {
    /// The codec.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// The codec tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_tag: Option<String>,
    /// The comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// The index.
    pub index: i32,
    /// The filename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The MIME type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// The delivery URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_url: Option<String>,
}

/// Class `MediaUrl`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MediaUrl {
    /// The URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A class representing a parental rating score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ParentalRatingScore {
    /// The score.
    #[serde(rename = "score")]
    pub score: i32,
    /// The sub score.
    #[serde(rename = "subScore", skip_serializing_if = "Option::is_none")]
    pub sub_score: Option<i32>,
}

impl ParentalRatingScore {
    /// Initializes a new instance of the [`ParentalRatingScore`] struct.
    #[must_use]
    pub fn new(score: i32, sub_score: Option<i32>) -> Self {
        Self { score, sub_score }
    }
}

/// A class representing a parental rating entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ParentalRatingEntry {
    /// The rating strings.
    #[serde(rename = "ratingStrings")]
    pub rating_strings: Vec<String>,
    /// The score.
    #[serde(rename = "ratingScore")]
    pub rating_score: ParentalRatingScore,
}

/// A class representing a parental rating system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ParentalRatingSystem {
    /// The country code.
    #[serde(rename = "countryCode")]
    pub country_code: String,
    /// A value indicating whether sub scores are supported.
    #[serde(rename = "supportsSubScores")]
    pub supports_sub_scores: bool,
    /// The ratings.
    #[serde(rename = "ratings", skip_serializing_if = "Option::is_none")]
    pub ratings: Option<Vec<ParentalRatingEntry>>,
}

/// Class `ParentalRating`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ParentalRating {
    /// The name.
    pub name: String,
    /// The value.
    ///
    /// Deprecated: mirrors the score for backwards compatibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<i32>,
    /// The rating score.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_score: Option<ParentalRatingScore>,
}

impl ParentalRating {
    /// Initializes a new instance of the [`ParentalRating`] struct.
    #[must_use]
    pub fn new(name: String, score: Option<ParentalRatingScore>) -> Self {
        Self {
            name,
            value: score.map(|s| s.score),
            rating_score: score,
        }
    }
}

/// Class to hold data on user permissions for playlists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlaylistUserPermissions {
    /// The user id.
    #[schema(value_type = String, format = "uuid")]
    pub user_id: Uuid,
    /// A value indicating whether the user has edit permissions.
    pub can_edit: bool,
}

impl PlaylistUserPermissions {
    /// Initializes a new instance of the [`PlaylistUserPermissions`] struct.
    #[must_use]
    pub fn new(user_id: Uuid, can_edit: bool) -> Self {
        Self { user_id, can_edit }
    }
}

/// Interface for access to shares.
pub trait IHasShares {
    /// Gets the shares.
    fn shares(&self) -> &[PlaylistUserPermissions];
    /// Sets the shares.
    fn set_shares(&mut self, shares: Vec<PlaylistUserPermissions>);
}

/// Class `LibraryUpdateInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LibraryUpdateInfo {
    /// The folders added to.
    pub folders_added_to: Vec<String>,
    /// The folders removed from.
    pub folders_removed_from: Vec<String>,
    /// The items added.
    pub items_added: Vec<String>,
    /// The items removed.
    pub items_removed: Vec<String>,
    /// The items updated.
    pub items_updated: Vec<String>,
    /// The collection folders.
    pub collection_folders: Vec<String>,
    /// A value indicating whether this update carries no changes.
    pub is_empty: bool,
}

impl LibraryUpdateInfo {
    /// Computes whether this update carries no changes across all buckets.
    #[must_use]
    pub fn compute_is_empty(&self) -> bool {
        self.folders_added_to.is_empty()
            && self.folders_removed_from.is_empty()
            && self.items_added.is_empty()
            && self.items_removed.is_empty()
            && self.items_updated.is_empty()
            && self.collection_folders.is_empty()
    }
}

/// Used to hold information about a user's list of configured virtual folders.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct VirtualFolderInfo {
    /// The name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The locations.
    pub locations: Vec<String>,
    /// The type of the collection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection_type: Option<CollectionTypeOptions>,
    /// The library options associated with the folder (per-library
    /// `library.xml`/`options.xml` payload).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_options: Option<LibraryOptions>,
    /// The item identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// The primary image item identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_image_item_id: Option<String>,
    /// The refresh progress.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_progress: Option<f64>,
    /// The refresh status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_status: Option<String>,
}

/// Since `BaseItem` and `DTOBaseItem` both have provider ids, this trait helps
/// avoid code repetition by using extension methods (the free functions on
/// this module).
pub trait IHasProviderIds {
    /// Gets the provider ids, if any.
    fn provider_ids(&self) -> Option<&HashMap<String, String>>;
    /// Gets a mutable reference to the provider ids, creating the map if it
    /// does not yet exist.
    fn provider_ids_mut(&mut self) -> &mut HashMap<String, String>;
    /// Gets the provider ids as an `Option`, without creating the map.
    fn provider_ids_opt_mut(&mut self) -> &mut Option<HashMap<String, String>>;
}

/// The special language codes that are never shown in a display title.
const SPECIAL_CODES: [&str; 4] = ["mis", "mul", "und", "zxx"];

/// Class `MediaStream`.
///
/// The derived `DisplayTitle` / `VideoRange` / resolution logic is preserved
/// verbatim from upstream; `StreamBuilder` depends on it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)] // Faithful DTO port of the upstream fields.
#[serde(default)]
pub struct MediaStream {
    /// The codec.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// The codec tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_tag: Option<String>,
    /// The language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The color range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_range: Option<String>,
    /// The color space.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
    /// The color transfer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_transfer: Option<String>,
    /// The color primaries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_primaries: Option<String>,
    /// The Dolby Vision version major.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dv_version_major: Option<i32>,
    /// The Dolby Vision version minor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dv_version_minor: Option<i32>,
    /// The Dolby Vision profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dv_profile: Option<i32>,
    /// The Dolby Vision level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dv_level: Option<i32>,
    /// The Dolby Vision rpu present flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpu_present_flag: Option<i32>,
    /// The Dolby Vision el present flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub el_present_flag: Option<i32>,
    /// The Dolby Vision bl present flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bl_present_flag: Option<i32>,
    /// The Dolby Vision bl signal compatibility id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dv_bl_signal_compatibility_id: Option<i32>,
    /// The rotation in degrees.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation: Option<i32>,
    /// The comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// The time base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_base: Option<String>,
    /// The codec time base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_time_base: Option<String>,
    /// The title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The composed display title (language + codec + channels/flags), computed
    /// by [`display_title`](Self::display_title). Populated when the wire DTO is
    /// built; clients show it verbatim (jellyfin-web renders "Undefined" without
    /// it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
    /// The HDR10+ present flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr10_plus_present_flag: Option<bool>,
    /// The localized "undefined" label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub localized_undefined: Option<String>,
    /// The localized "default" label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub localized_default: Option<String>,
    /// The localized "forced" label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub localized_forced: Option<String>,
    /// The localized "external" label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub localized_external: Option<String>,
    /// The localized "hearing impaired" label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub localized_hearing_impaired: Option<String>,
    /// The localized language name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub localized_language: Option<String>,
    /// The localized "original" label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub localized_original: Option<String>,
    /// The NAL length size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nal_length_size: Option<String>,
    /// A value indicating whether this instance is interlaced.
    pub is_interlaced: bool,
    /// A value indicating whether this instance is AVC.
    #[serde(rename = "IsAVC", skip_serializing_if = "Option::is_none")]
    pub is_avc: Option<bool>,
    /// The channel layout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_layout: Option<String>,
    /// The bit rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<i32>,
    /// The bit depth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<i32>,
    /// The reference frames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_frames: Option<i32>,
    /// The length of the packet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packet_length: Option<i32>,
    /// The channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<i32>,
    /// The sample rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<i32>,
    /// A value indicating whether this instance is default.
    pub is_default: bool,
    /// A value indicating whether this instance is forced.
    pub is_forced: bool,
    /// A value indicating whether this instance is for the hearing impaired.
    pub is_hearing_impaired: bool,
    /// A value indicating whether this instance is original.
    pub is_original: bool,
    /// The height.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
    /// The width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    /// The average frame rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_frame_rate: Option<f32>,
    /// The real frame rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub real_frame_rate: Option<f32>,
    /// The profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The type.
    #[serde(rename = "Type", deserialize_with = "deserialize_media_stream_type")]
    pub stream_type: MediaStreamType,
    /// The aspect ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    /// The index.
    pub index: i32,
    /// The score.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<i32>,
    /// A value indicating whether this instance is external.
    pub is_external: bool,
    /// The delivery method.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_method: Option<SubtitleDeliveryMethod>,
    /// The delivery URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_url: Option<String>,
    /// A value indicating whether this instance is an external URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_external_url: Option<bool>,
    /// A value indicating whether external streams are supported.
    pub supports_external_stream: bool,
    /// The filename / path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The pixel format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,
    /// The level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<f64>,
    /// A value indicating whether this instance is anamorphic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_anamorphic: Option<bool>,
}

impl MediaStream {
    /// Gets the video range derived from the color / Dolby Vision metadata.
    #[must_use]
    pub fn video_range(&self) -> VideoRange {
        self.get_video_color_range().0
    }

    /// Gets the video range type derived from the color / Dolby Vision
    /// metadata.
    #[must_use]
    pub fn video_range_type(&self) -> VideoRangeType {
        self.get_video_color_range().1
    }

    /// Gets the video Dolby Vision title, if applicable.
    #[must_use]
    #[allow(clippy::match_same_arms)] // Preserve the upstream compat-id arms verbatim.
    pub fn video_dovi_title(&self) -> Option<String> {
        let dv_profile = self.dv_profile;
        let rpu_present_flag = self.rpu_present_flag == Some(1);
        let bl_present_flag = self.bl_present_flag == Some(1);
        let dv_bl_compat_id = self.dv_bl_signal_compatibility_id;

        if rpu_present_flag && bl_present_flag && matches!(dv_profile, Some(4 | 5 | 7 | 8 | 9 | 10))
        {
            let mut title = format!("Dolby Vision Profile {}", dv_profile.unwrap_or_default());

            if dv_bl_compat_id.unwrap_or(0) > 0 {
                title.push('.');
                title.push_str(&dv_bl_compat_id.unwrap_or_default().to_string());
            }

            return Some(match dv_bl_compat_id {
                Some(1) => title + " (HDR10)",
                Some(2) => title + " (SDR)",
                Some(4) => title + " (HLG)",
                // Technically means Blu-ray, but practically always HDR10.
                Some(6) => title + " (HDR10)",
                _ => title,
            });
        }

        None
    }

    /// Gets the audio spatial format derived from the codec profile.
    #[must_use]
    pub fn audio_spatial_format(&self) -> AudioSpatialFormat {
        let Some(profile) = self.profile.as_deref() else {
            return AudioSpatialFormat::None;
        };

        if self.stream_type != MediaStreamType::Audio || profile.is_empty() {
            return AudioSpatialFormat::None;
        }

        if contains_ignore_ascii_case(profile, "Dolby Atmos") {
            AudioSpatialFormat::DolbyAtmos
        } else if contains_ignore_ascii_case(profile, "DTS:X") {
            AudioSpatialFormat::Dtsx
        } else {
            AudioSpatialFormat::None
        }
    }

    /// Gets the framerate used as reference. Prefer `average_frame_rate`, but
    /// if that is null or an unrealistic value (>= 1000) fall back to
    /// `real_frame_rate`.
    #[must_use]
    pub fn reference_frame_rate(&self) -> Option<f32> {
        match self.average_frame_rate {
            Some(avg) if avg < 1000.0 => Some(avg),
            _ => self.real_frame_rate,
        }
    }

    /// Gets the derived display title for this stream.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn display_title(&self) -> Option<String> {
        match self.stream_type {
            MediaStreamType::Audio => {
                let mut attributes: Vec<String> = Vec::new();

                // Do not display the language code if unset or set to a special
                // code. Show it in all other cases (possibly expanded).
                if let Some(language) = non_empty(self.language.as_deref())
                    && !SPECIAL_CODES
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(language))
                {
                    let localized = self.localized_language.as_deref().unwrap_or(language);
                    attributes.push(extensions::first_to_upper(localized));
                }

                if let Some(profile) = non_empty(self.profile.as_deref()) {
                    if !profile.eq_ignore_ascii_case("lc") {
                        attributes.push(profile.to_owned());
                    } else if let Some(codec) = non_empty(self.codec.as_deref()) {
                        attributes.push(audio_codec::friendly_name(codec));
                    }
                } else if let Some(codec) = non_empty(self.codec.as_deref()) {
                    attributes.push(audio_codec::friendly_name(codec));
                }

                if let Some(channel_layout) = non_empty(self.channel_layout.as_deref()) {
                    attributes.push(extensions::first_to_upper(channel_layout));
                } else if let Some(channels) = self.channels {
                    attributes.push(format!("{channels} ch"));
                }

                if self.is_default {
                    attributes.push(localized_or(self.localized_default.as_ref(), "Default"));
                }

                if self.is_external {
                    attributes.push(localized_or(self.localized_external.as_ref(), "External"));
                }

                if self.is_original {
                    attributes.push(localized_or(self.localized_original.as_ref(), "Original"));
                }

                Some(join_with_title(self.title.as_deref(), &attributes, " - "))
            }
            MediaStreamType::Video => {
                let mut attributes: Vec<String> = Vec::new();

                if let Some(resolution_text) = self.get_resolution_text() {
                    attributes.push(resolution_text);
                }

                if let Some(codec) = non_empty(self.codec.as_deref()) {
                    attributes.push(codec.to_uppercase());
                }

                if let Some(dovi_title) = self.video_dovi_title() {
                    attributes.push(dovi_title);
                } else {
                    let video_range = self.video_range();
                    if video_range != VideoRange::Unknown {
                        attributes.push(video_range_to_string(video_range).to_owned());
                    }
                }

                Some(join_with_title(self.title.as_deref(), &attributes, " "))
            }
            MediaStreamType::Subtitle => {
                let mut attributes: Vec<String> = Vec::new();

                if let Some(language) = non_empty(self.language.as_deref()) {
                    let localized = self.localized_language.as_deref().unwrap_or(language);
                    attributes.push(extensions::first_to_upper(localized));
                } else {
                    attributes.push(localized_or(self.localized_undefined.as_ref(), "Und"));
                }

                if self.is_hearing_impaired {
                    attributes.push(localized_or(
                        self.localized_hearing_impaired.as_ref(),
                        "Hearing Impaired",
                    ));
                }

                if self.is_default {
                    attributes.push(localized_or(self.localized_default.as_ref(), "Default"));
                }

                if self.is_forced {
                    attributes.push(localized_or(self.localized_forced.as_ref(), "Forced"));
                }

                if let Some(codec) = non_empty(self.codec.as_deref()) {
                    attributes.push(codec.to_uppercase());
                }

                if self.is_external {
                    attributes.push(localized_or(self.localized_external.as_ref(), "External"));
                }

                Some(join_with_title(self.title.as_deref(), &attributes, " - "))
            }
            _ => None,
        }
    }

    /// Gets the human-readable resolution text (e.g. `1080p`), if width and
    /// height are known.
    #[must_use]
    pub fn get_resolution_text(&self) -> Option<String> {
        let (Some(width), Some(height)) = (self.width, self.height) else {
            return None;
        };
        let i = self.is_interlaced;

        let label = match (width, height) {
            (w, h) if w <= 256 && h <= 144 => it(i, "144"),
            (w, h) if w <= 426 && h <= 240 => it(i, "240"),
            (w, h) if w <= 640 && h <= 360 => it(i, "360"),
            (w, h) if w <= 682 && h <= 384 => it(i, "384"),
            (w, h) if w <= 720 && h <= 404 => it(i, "404"),
            (w, h) if w <= 854 && h <= 480 => it(i, "480"),
            (w, h) if w <= 960 && h <= 544 => it(i, "540"),
            (w, h) if w <= 1024 && h <= 576 => it(i, "576"),
            (w, h) if w <= 1280 && h <= 962 => it(i, "720"),
            (w, h) if w <= 2560 && h <= 1440 => it(i, "1080"),
            (w, h) if w <= 4096 && h <= 3072 => "4K".to_owned(),
            (w, h) if w <= 8192 && h <= 6144 => "8K".to_owned(),
            _ => return None,
        };

        Some(label)
    }

    /// Whether this is a text-based subtitle stream.
    #[must_use]
    pub fn is_text_subtitle_stream(&self) -> bool {
        if self.stream_type != MediaStreamType::Subtitle {
            return false;
        }
        if self.codec.as_deref().unwrap_or("").is_empty() && !self.is_external {
            return false;
        }
        Self::is_text_format(self.codec.as_deref())
    }

    /// Whether this is a PGS subtitle stream.
    #[must_use]
    pub fn is_pgs_subtitle_stream(&self) -> bool {
        if self.stream_type != MediaStreamType::Subtitle {
            return false;
        }
        if self.codec.as_deref().unwrap_or("").is_empty() && !self.is_external {
            return false;
        }
        Self::is_pgs_format(self.codec.as_deref())
    }

    /// Whether this is a `VobSub` subtitle stream.
    #[must_use]
    pub fn is_vob_sub_subtitle_stream(&self) -> bool {
        if self.stream_type != MediaStreamType::Subtitle {
            return false;
        }
        if self.codec.as_deref().unwrap_or("").is_empty() && !self.is_external {
            return false;
        }
        Self::is_vob_sub_format(self.codec.as_deref())
    }

    /// Whether this subtitle stream is extractable by ffmpeg (all text-based,
    /// PGS, and `VobSub` subtitles can be extracted).
    #[must_use]
    pub fn is_extractable_subtitle_stream(&self) -> bool {
        self.is_text_subtitle_stream()
            || self.is_pgs_subtitle_stream()
            || self.is_vob_sub_subtitle_stream()
    }

    /// Whether the given codec `format` is a text-based subtitle format.
    #[must_use]
    pub fn is_text_format(format: Option<&str>) -> bool {
        let codec = format.unwrap_or("");

        // microdvd and dvdsub/vobsub share the ".sub" file extension, but it's
        // text-based.
        contains_ignore_ascii_case(codec, "microdvd")
            || (!contains_ignore_ascii_case(codec, "pgs")
                && !contains_ignore_ascii_case(codec, "dvdsub")
                && !contains_ignore_ascii_case(codec, "vobsub")
                && !contains_ignore_ascii_case(codec, "dvbsub")
                && !codec.eq_ignore_ascii_case("sup")
                && !codec.eq_ignore_ascii_case("sub"))
    }

    /// Whether the given codec `format` is a PGS subtitle format.
    #[must_use]
    pub fn is_pgs_format(format: Option<&str>) -> bool {
        let codec = format.unwrap_or("");
        contains_ignore_ascii_case(codec, "pgs") || codec.eq_ignore_ascii_case("sup")
    }

    /// Whether the given codec `format` is a `VobSub` subtitle format.
    #[must_use]
    pub fn is_vob_sub_format(format: Option<&str>) -> bool {
        let codec = format.unwrap_or("");
        contains_ignore_ascii_case(codec, "dvdsub") || contains_ignore_ascii_case(codec, "vobsub")
    }

    /// Whether this text subtitle stream can be converted to `to_codec`.
    #[must_use]
    pub fn supports_subtitle_conversion_to(&self, to_codec: &str) -> bool {
        if !self.is_text_subtitle_stream() {
            return false;
        }

        let from_codec = self.codec.as_deref().unwrap_or("");

        // Can't convert from this.
        if from_codec.eq_ignore_ascii_case("ass") || from_codec.eq_ignore_ascii_case("ssa") {
            return false;
        }

        // Can't convert to this.
        if to_codec.eq_ignore_ascii_case("ass") || to_codec.eq_ignore_ascii_case("ssa") {
            return false;
        }

        true
    }

    /// Computes the `(VideoRange, VideoRangeType)` pair from the color / Dolby
    /// Vision metadata.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn get_video_color_range(&self) -> (VideoRange, VideoRangeType) {
        if self.stream_type != MediaStreamType::Video {
            return (VideoRange::Unknown, VideoRangeType::Unknown);
        }

        let codec_tag = self.codec_tag.as_deref();
        let dv_profile = self.dv_profile;
        let rpu_present_flag = self.rpu_present_flag == Some(1);
        let bl_present_flag = self.bl_present_flag == Some(1);
        let dv_bl_compat_id = self.dv_bl_signal_compatibility_id;

        let is_dovi_profile = matches!(dv_profile, Some(5 | 7 | 8 | 10));
        let is_dovi_flag = rpu_present_flag
            && bl_present_flag
            && matches!(dv_bl_compat_id, Some(0 | 1 | 4 | 2 | 6));

        if (is_dovi_profile && is_dovi_flag)
            || codec_tag.is_some_and(|t| t.eq_ignore_ascii_case("dovi"))
            || codec_tag.is_some_and(|t| t.eq_ignore_ascii_case("dvh1"))
            || codec_tag.is_some_and(|t| t.eq_ignore_ascii_case("dvhe"))
            || codec_tag.is_some_and(|t| t.eq_ignore_ascii_case("dav1"))
        {
            let dv_range_set: (VideoRange, VideoRangeType) = match dv_profile {
                Some(5) => (VideoRange::Hdr, VideoRangeType::Dovi),
                Some(8) => match dv_bl_compat_id {
                    Some(1) => (VideoRange::Hdr, VideoRangeType::DoviWithHdr10),
                    Some(4) => (VideoRange::Hdr, VideoRangeType::DoviWithHlg),
                    Some(2) => (VideoRange::Sdr, VideoRangeType::DoviWithSdr),
                    // Out of Dolby Spec files should be marked as invalid.
                    _ => (VideoRange::Hdr, VideoRangeType::DoviInvalid),
                },
                Some(7) => (VideoRange::Hdr, VideoRangeType::DoviWithEl),
                Some(10) => match dv_bl_compat_id {
                    Some(0) => (VideoRange::Hdr, VideoRangeType::Dovi),
                    Some(1) => (VideoRange::Hdr, VideoRangeType::DoviWithHdr10),
                    Some(2) => (VideoRange::Sdr, VideoRangeType::DoviWithSdr),
                    Some(4) => (VideoRange::Hdr, VideoRangeType::DoviWithHlg),
                    // Out of Dolby Spec files should be marked as invalid.
                    _ => (VideoRange::Hdr, VideoRangeType::DoviInvalid),
                },
                _ => (VideoRange::Sdr, VideoRangeType::Sdr),
            };

            if self.hdr10_plus_present_flag == Some(true) {
                return match dv_range_set.1 {
                    VideoRangeType::DoviWithHdr10 => {
                        (VideoRange::Hdr, VideoRangeType::DoviWithHdr10Plus)
                    }
                    VideoRangeType::DoviWithEl => {
                        (VideoRange::Hdr, VideoRangeType::DoviWithElhdr10Plus)
                    }
                    _ => dv_range_set,
                };
            }

            return dv_range_set;
        }

        let color_transfer = self.color_transfer.as_deref();

        if color_transfer.is_some_and(|t| t.eq_ignore_ascii_case("smpte2084")) {
            return if self.hdr10_plus_present_flag == Some(true) {
                (VideoRange::Hdr, VideoRangeType::Hdr10Plus)
            } else {
                (VideoRange::Hdr, VideoRangeType::Hdr10)
            };
        } else if color_transfer.is_some_and(|t| t.eq_ignore_ascii_case("arib-std-b67")) {
            return (VideoRange::Hdr, VideoRangeType::Hlg);
        }

        (VideoRange::Sdr, VideoRangeType::Sdr)
    }
}

/// Case-insensitive substring test (mirrors C#
/// `string.Contains(_, StringComparison.OrdinalIgnoreCase)`).
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// Returns `Some(s)` only when `s` is present and non-empty.
fn non_empty(s: Option<&str>) -> Option<&str> {
    s.filter(|v| !v.is_empty())
}

/// Returns the localized label when non-empty, otherwise the fallback.
fn localized_or(localized: Option<&String>, fallback: &str) -> String {
    match localized.map(String::as_str) {
        Some(l) if !l.is_empty() => l.to_owned(),
        _ => fallback.to_owned(),
    }
}

/// The interlaced/progressive suffix for a resolution label.
fn it(interlaced: bool, base: &str) -> String {
    format!("{base}{}", if interlaced { "i" } else { "p" })
}

/// The `ToString()` of a [`VideoRange`], matching the C# enum member names.
fn video_range_to_string(range: VideoRange) -> &'static str {
    match range {
        VideoRange::Unknown => "Unknown",
        VideoRange::Sdr => "SDR",
        VideoRange::Hdr => "HDR",
    }
}

/// Joins `attributes` with `separator`; if a `title` is present, appends only
/// the attributes not already contained (case-insensitively) in the title.
fn join_with_title(title: Option<&str>, attributes: &[String], separator: &str) -> String {
    if let Some(title) = non_empty(title) {
        let mut result = String::from(title);
        for tag in attributes {
            if !contains_ignore_ascii_case(title, tag) {
                result.push_str(separator);
                result.push_str(tag);
            }
        }
        result
    } else {
        attributes.join(separator)
    }
}

/// Case-insensitive dictionary of [`MetadataProvider`] string representations,
/// keyed by lower-cased name, mapping to the canonical name.
fn canonical_provider_name(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    MetadataProvider::all()
        .iter()
        .find(|p| p.as_name().eq_ignore_ascii_case(&lower))
        .map(|p| p.as_name())
}

/// Checks if this instance has an id for the given provider `name`.
pub fn has_provider_id<T: IHasProviderIds + ?Sized>(instance: &T, name: &str) -> bool {
    try_get_provider_id(instance, name).is_some()
}

/// Checks if this instance has an id for the given [`MetadataProvider`].
pub fn has_provider_id_for<T: IHasProviderIds + ?Sized>(
    instance: &T,
    provider: MetadataProvider,
) -> bool {
    has_provider_id(instance, provider.as_name())
}

/// Gets a provider id by `name`, returning `None` if absent or empty.
pub fn try_get_provider_id<'a, T: IHasProviderIds + ?Sized>(
    instance: &'a T,
    name: &str,
) -> Option<&'a str> {
    let ids = instance.provider_ids()?;
    match ids.get(name) {
        Some(id) if !id.is_empty() => Some(id),
        _ => None,
    }
}

/// Gets a provider id for the given [`MetadataProvider`].
pub fn try_get_provider_id_for<T: IHasProviderIds + ?Sized>(
    instance: &T,
    provider: MetadataProvider,
) -> Option<&str> {
    try_get_provider_id(instance, provider.as_name())
}

/// Gets a provider id by `name` (owned convenience over [`try_get_provider_id`]).
pub fn get_provider_id<T: IHasProviderIds + ?Sized>(instance: &T, name: &str) -> Option<String> {
    try_get_provider_id(instance, name).map(ToOwned::to_owned)
}

/// Gets a provider id for the given [`MetadataProvider`].
pub fn get_provider_id_for<T: IHasProviderIds + ?Sized>(
    instance: &T,
    provider: MetadataProvider,
) -> Option<String> {
    get_provider_id(instance, provider.as_name())
}

/// Sets a provider id, returning `true` on success.
///
/// Returns `false` (a no-op) when `name` or `value` is blank, or when `name`
/// contains a `'='` (which cannot be deserialized from the database). Matches
/// on the internal [`MetadataProvider`] canonical casing before adding
/// arbitrary providers.
pub fn try_set_provider_id<T: IHasProviderIds + ?Sized>(
    instance: &mut T,
    name: Option<&str>,
    value: Option<&str>,
) -> bool {
    let (Some(name), Some(value)) = (name, value) else {
        return false;
    };

    if name.trim().is_empty() || value.trim().is_empty() || name.contains('=') {
        return false;
    }

    let key = canonical_provider_name(name).unwrap_or(name);
    instance
        .provider_ids_mut()
        .insert(key.to_owned(), value.to_owned());
    true
}

/// Sets a provider id for the given [`MetadataProvider`], returning `true` on
/// success.
pub fn try_set_provider_id_for<T: IHasProviderIds + ?Sized>(
    instance: &mut T,
    provider: MetadataProvider,
    value: Option<&str>,
) -> bool {
    try_set_provider_id(instance, Some(provider.as_name()), value)
}

/// The error returned by [`set_provider_id`] on invalid input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetProviderIdError {
    /// `name` or `value` was null or whitespace.
    #[error("name and value cannot be null or whitespace")]
    NullOrWhitespace,
    /// `name` contained a `'='` character.
    #[error("Provider id name cannot contain '='")]
    ContainsEquals,
}

/// Sets a provider id, throwing on blank input or a `'='` in `name` (mirrors
/// the C# `SetProviderId` overload that argument-checks).
///
/// # Errors
///
/// Returns [`SetProviderIdError`] when `name` or `value` is blank, or when
/// `name` contains a `'='`.
pub fn set_provider_id<T: IHasProviderIds + ?Sized>(
    instance: &mut T,
    name: &str,
    value: &str,
) -> Result<(), SetProviderIdError> {
    if name.trim().is_empty() || value.trim().is_empty() {
        return Err(SetProviderIdError::NullOrWhitespace);
    }

    if name.contains('=') {
        return Err(SetProviderIdError::ContainsEquals);
    }

    let key = canonical_provider_name(name).unwrap_or(name);
    instance
        .provider_ids_mut()
        .insert(key.to_owned(), value.to_owned());
    Ok(())
}

/// Sets a provider id for the given [`MetadataProvider`], throwing on blank
/// input.
///
/// # Errors
///
/// Returns [`SetProviderIdError`] when `value` is blank.
pub fn set_provider_id_for<T: IHasProviderIds + ?Sized>(
    instance: &mut T,
    provider: MetadataProvider,
    value: &str,
) -> Result<(), SetProviderIdError> {
    set_provider_id(instance, provider.as_name(), value)
}

/// Removes a provider id by `name`.
pub fn remove_provider_id<T: IHasProviderIds + ?Sized>(instance: &mut T, name: &str) {
    if let Some(ids) = instance.provider_ids_opt_mut() {
        ids.remove(name);
    }
}

/// Removes a provider id for the given [`MetadataProvider`].
pub fn remove_provider_id_for<T: IHasProviderIds + ?Sized>(
    instance: &mut T,
    provider: MetadataProvider,
) {
    remove_provider_id(instance, provider.as_name());
}

/// Deserializes a [`MediaStreamType`] from either its PascalCase string name or
/// its integer discriminant.
///
/// Jellyfin's `System.Text.Json` enum converter accepts both forms on read; the
/// checked-in test fixtures encode `MediaStream.Type` as an integer.
fn deserialize_media_stream_type<'de, D>(deserializer: D) -> Result<MediaStreamType, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IntOrString {
        Int(u8),
        Str(String),
    }

    match IntOrString::deserialize(deserializer)? {
        IntOrString::Int(n) => match n {
            0 => Ok(MediaStreamType::Audio),
            1 => Ok(MediaStreamType::Video),
            2 => Ok(MediaStreamType::Subtitle),
            3 => Ok(MediaStreamType::EmbeddedImage),
            4 => Ok(MediaStreamType::Data),
            5 => Ok(MediaStreamType::Lyric),
            other => Err(serde::de::Error::custom(format!(
                "invalid MediaStreamType discriminant: {other}"
            ))),
        },
        IntOrString::Str(s) => match s.as_str() {
            "Audio" => Ok(MediaStreamType::Audio),
            "Video" => Ok(MediaStreamType::Video),
            "Subtitle" => Ok(MediaStreamType::Subtitle),
            "EmbeddedImage" => Ok(MediaStreamType::EmbeddedImage),
            "Data" => Ok(MediaStreamType::Data),
            "Lyric" => Ok(MediaStreamType::Lyric),
            other => Err(serde::de::Error::custom(format!(
                "invalid MediaStreamType: {other}"
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_spatial_format_default_and_wire_names() {
        assert_eq!(AudioSpatialFormat::default(), AudioSpatialFormat::None);
        assert_eq!(
            serde_json::to_value(AudioSpatialFormat::None).unwrap(),
            "None"
        );
        assert_eq!(
            serde_json::to_value(AudioSpatialFormat::DolbyAtmos).unwrap(),
            "DolbyAtmos"
        );
        // DTS:X is renamed to "DTSX" on the wire.
        assert_eq!(
            serde_json::to_value(AudioSpatialFormat::Dtsx).unwrap(),
            "DTSX"
        );
        let back: AudioSpatialFormat = serde_json::from_str("\"DTSX\"").unwrap();
        assert_eq!(back, AudioSpatialFormat::Dtsx);
    }

    #[test]
    fn metadata_provider_as_name_and_all_are_consistent() {
        // `all()` and `as_name()` must agree, and every name must be distinct.
        let providers = MetadataProvider::all();
        assert_eq!(providers.len(), 17);
        let mut seen = std::collections::HashSet::new();
        for p in providers {
            assert!(seen.insert(p.as_name()), "duplicate name for {p:?}");
        }
        assert_eq!(MetadataProvider::Imdb.as_name(), "Imdb");
        assert_eq!(
            MetadataProvider::MusicBrainzRecording.as_name(),
            "MusicBrainzRecording"
        );
    }

    #[test]
    fn metadata_provider_discriminants_have_gaps() {
        // The discriminants mirror upstream verbatim, including gaps.
        assert_eq!(MetadataProvider::Custom as i32, 0);
        assert_eq!(MetadataProvider::Imdb as i32, 2);
        assert_eq!(MetadataProvider::MusicBrainzRecording as i32, 20);
    }

    #[test]
    fn chapter_info_round_trips_and_pascal_case() {
        let info = ChapterInfo {
            start_position_ticks: 42,
            name: Some("Intro".to_owned()),
            image_path: Some("/img".to_owned()),
            image_date_modified: chrono::DateTime::<chrono::Utc>::default(),
            image_tag: Some("tag".to_owned()),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["StartPositionTicks"], 42);
        assert_eq!(json["Name"], "Intro");
        assert_eq!(json["ImagePath"], "/img");
        assert_eq!(json["ImageTag"], "tag");
        let back: ChapterInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn chapter_info_omits_none() {
        let json = serde_json::to_value(ChapterInfo::default()).unwrap();
        assert!(json.get("Name").is_none());
        assert!(json.get("ImagePath").is_none());
        assert!(json.get("ImageTag").is_none());
    }

    #[test]
    fn media_attachment_round_trips() {
        let att = MediaAttachment {
            codec: Some("srt".to_owned()),
            index: 3,
            file_name: Some("sub.srt".to_owned()),
            mime_type: Some("text/plain".to_owned()),
            ..MediaAttachment::default()
        };
        let json = serde_json::to_value(&att).unwrap();
        assert_eq!(json["Codec"], "srt");
        assert_eq!(json["Index"], 3);
        assert_eq!(json["FileName"], "sub.srt");
        assert_eq!(json["MimeType"], "text/plain");
        let back: MediaAttachment = serde_json::from_value(json).unwrap();
        assert_eq!(att, back);
    }

    #[test]
    fn media_url_round_trips() {
        let url = MediaUrl {
            url: Some("http://x".to_owned()),
            name: Some("home".to_owned()),
        };
        let json = serde_json::to_value(&url).unwrap();
        assert_eq!(json["Url"], "http://x");
        assert_eq!(json["Name"], "home");
        let back: MediaUrl = serde_json::from_value(json).unwrap();
        assert_eq!(url, back);
    }

    #[test]
    fn parental_rating_score_new_and_camel_case() {
        let score = ParentalRatingScore::new(13, Some(4));
        assert_eq!(score.score, 13);
        assert_eq!(score.sub_score, Some(4));
        let json = serde_json::to_value(score).unwrap();
        // camelCase renames on this type.
        assert_eq!(json["score"], 13);
        assert_eq!(json["subScore"], 4);
        let back: ParentalRatingScore = serde_json::from_value(json).unwrap();
        assert_eq!(score, back);
    }

    #[test]
    fn parental_rating_score_omits_sub_score_when_none() {
        let json = serde_json::to_value(ParentalRatingScore::new(0, None)).unwrap();
        assert!(json.get("subScore").is_none());
    }

    #[test]
    fn parental_rating_new_mirrors_score_into_value() {
        let score = ParentalRatingScore::new(18, None);
        let rating = ParentalRating::new("R".to_owned(), Some(score));
        assert_eq!(rating.name, "R");
        assert_eq!(rating.value, Some(18));
        assert_eq!(rating.rating_score, Some(score));

        let none = ParentalRating::new("NR".to_owned(), None);
        assert_eq!(none.value, None);
        assert_eq!(none.rating_score, None);
        let json = serde_json::to_value(&none).unwrap();
        assert_eq!(json["Name"], "NR");
        assert!(json.get("Value").is_none());
    }

    #[test]
    fn parental_rating_entry_and_system_round_trip() {
        let entry = ParentalRatingEntry {
            rating_strings: vec!["PG-13".to_owned()],
            rating_score: ParentalRatingScore::new(13, None),
        };
        let system = ParentalRatingSystem {
            country_code: "US".to_owned(),
            supports_sub_scores: false,
            ratings: Some(vec![entry.clone()]),
        };
        let json = serde_json::to_value(&system).unwrap();
        assert_eq!(json["countryCode"], "US");
        assert_eq!(json["supportsSubScores"], false);
        assert_eq!(json["ratings"][0]["ratingStrings"][0], "PG-13");
        let back: ParentalRatingSystem = serde_json::from_value(json).unwrap();
        assert_eq!(system, back);

        let entry_back: ParentalRatingEntry =
            serde_json::from_str(&serde_json::to_string(&entry).unwrap()).unwrap();
        assert_eq!(entry, entry_back);
    }

    #[test]
    fn playlist_user_permissions_new_and_pascal_case() {
        let uid = Uuid::from_u128(9);
        let perm = PlaylistUserPermissions::new(uid, true);
        assert_eq!(perm.user_id, uid);
        assert!(perm.can_edit);
        let json = serde_json::to_value(perm).unwrap();
        assert!(json.get("UserId").is_some());
        assert_eq!(json["CanEdit"], true);
        let back: PlaylistUserPermissions = serde_json::from_value(json).unwrap();
        assert_eq!(perm, back);
    }

    #[test]
    fn library_update_info_compute_is_empty() {
        let empty = LibraryUpdateInfo::default();
        assert!(empty.compute_is_empty());

        let non_empty = LibraryUpdateInfo {
            items_added: vec!["x".to_owned()],
            ..LibraryUpdateInfo::default()
        };
        assert!(!non_empty.compute_is_empty());

        let json = serde_json::to_value(&non_empty).unwrap();
        assert_eq!(json["ItemsAdded"][0], "x");
        assert_eq!(json["IsEmpty"], false);
        let back: LibraryUpdateInfo = serde_json::from_value(json).unwrap();
        assert_eq!(non_empty, back);
    }

    #[test]
    fn virtual_folder_info_round_trips_and_omits_none() {
        let info = VirtualFolderInfo {
            name: Some("Movies".to_owned()),
            locations: vec!["/movies".to_owned()],
            refresh_progress: Some(0.5),
            ..VirtualFolderInfo::default()
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["Name"], "Movies");
        assert_eq!(json["Locations"][0], "/movies");
        assert_eq!(json["RefreshProgress"], 0.5);
        assert!(json.get("ItemId").is_none());
        let back: VirtualFolderInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info, back);
    }

    /// A minimal [`IHasProviderIds`] implementor to exercise the trait's
    /// mutable accessors (the free-function extension helpers are covered by
    /// the integration tests).
    #[derive(Default)]
    struct Bag {
        ids: Option<HashMap<String, String>>,
    }

    impl IHasProviderIds for Bag {
        fn provider_ids(&self) -> Option<&HashMap<String, String>> {
            self.ids.as_ref()
        }
        fn provider_ids_mut(&mut self) -> &mut HashMap<String, String> {
            self.ids.get_or_insert_with(HashMap::new)
        }
        fn provider_ids_opt_mut(&mut self) -> &mut Option<HashMap<String, String>> {
            &mut self.ids
        }
    }

    #[test]
    fn set_provider_id_error_variants() {
        let mut bag = Bag::default();
        assert_eq!(
            set_provider_id(&mut bag, "  ", "v"),
            Err(SetProviderIdError::NullOrWhitespace)
        );
        assert_eq!(
            set_provider_id(&mut bag, "Imdb", "  "),
            Err(SetProviderIdError::NullOrWhitespace)
        );
        assert_eq!(
            set_provider_id(&mut bag, "na=me", "v"),
            Err(SetProviderIdError::ContainsEquals)
        );
        assert!(set_provider_id(&mut bag, "imdb", "tt1").is_ok());
        // Name is canonicalized to the MetadataProvider casing.
        assert_eq!(get_provider_id(&bag, "Imdb").as_deref(), Some("tt1"));
    }

    #[test]
    fn provider_id_helpers_via_metadata_provider() {
        let mut bag = Bag::default();
        assert!(try_set_provider_id_for(
            &mut bag,
            MetadataProvider::Tmdb,
            Some("42")
        ));
        assert!(has_provider_id_for(&bag, MetadataProvider::Tmdb));
        assert_eq!(
            try_get_provider_id_for(&bag, MetadataProvider::Tmdb),
            Some("42")
        );
        assert_eq!(
            get_provider_id_for(&bag, MetadataProvider::Tmdb).as_deref(),
            Some("42")
        );
        remove_provider_id_for(&mut bag, MetadataProvider::Tmdb);
        assert!(!has_provider_id_for(&bag, MetadataProvider::Tmdb));
    }

    /// A [`MediaStream`] of the given type with all-neutral fields.
    fn stream(stream_type: MediaStreamType) -> MediaStream {
        MediaStream {
            stream_type,
            ..MediaStream::default()
        }
    }

    #[test]
    fn resolution_text_spans_every_label_and_interlacing() {
        let cases = [
            (256, 144, false, Some("144p")),
            (256, 144, true, Some("144i")),
            (426, 240, false, Some("240p")),
            (640, 360, false, Some("360p")),
            (682, 384, false, Some("384p")),
            (720, 404, false, Some("404p")),
            (854, 480, false, Some("480p")),
            (960, 544, false, Some("540p")),
            (1024, 576, false, Some("576p")),
            (1280, 720, false, Some("720p")),
            (1920, 1080, false, Some("1080p")),
            (1920, 1080, true, Some("1080i")),
            (3840, 2160, false, Some("4K")),
            (7680, 4320, false, Some("8K")),
            (99999, 99999, false, None),
        ];
        for (w, h, interlaced, expected) in cases {
            let mut s = stream(MediaStreamType::Video);
            s.width = Some(w);
            s.height = Some(h);
            s.is_interlaced = interlaced;
            assert_eq!(s.get_resolution_text().as_deref(), expected, "{w}x{h}");
        }
        // Missing width/height yields None.
        assert_eq!(stream(MediaStreamType::Video).get_resolution_text(), None);
    }

    #[test]
    fn audio_display_title_assembles_language_codec_channels_and_flags() {
        let mut s = stream(MediaStreamType::Audio);
        s.language = Some("eng".to_owned());
        s.codec = Some("aac".to_owned());
        s.channels = Some(6);
        s.is_default = true;
        s.is_external = true;
        s.is_original = true;
        let title = s.display_title().unwrap();
        assert!(title.contains("Default"));
        assert!(title.contains("External"));
        assert!(title.contains("Original"));
        assert!(title.contains("6 ch"));

        // A special language code (und) is not shown; LC profile expands the codec.
        let mut s2 = stream(MediaStreamType::Audio);
        s2.language = Some("und".to_owned());
        s2.profile = Some("LC".to_owned());
        s2.codec = Some("aac".to_owned());
        s2.channel_layout = Some("stereo".to_owned());
        let title2 = s2.display_title().unwrap();
        assert!(!title2.to_lowercase().contains("und"));
        assert!(title2.contains("Stereo"));
    }

    #[test]
    fn video_display_title_uses_resolution_codec_and_range() {
        let mut s = stream(MediaStreamType::Video);
        s.width = Some(1920);
        s.height = Some(1080);
        s.codec = Some("hevc".to_owned());
        s.color_transfer = Some("smpte2084".to_owned());
        let title = s.display_title().unwrap();
        assert!(title.contains("1080p"));
        assert!(title.contains("HEVC"));
        // smpte2084 → HDR range name is appended (no DoVi title present).
        assert!(title.contains("HDR"));
    }

    #[test]
    fn subtitle_display_title_covers_flags_and_undefined() {
        let mut s = stream(MediaStreamType::Subtitle);
        s.is_hearing_impaired = true;
        s.is_default = true;
        s.is_forced = true;
        s.is_external = true;
        s.codec = Some("srt".to_owned());
        let title = s.display_title().unwrap();
        assert!(title.contains("Und")); // no language → Und
        assert!(title.contains("Hearing Impaired"));
        assert!(title.contains("Default"));
        assert!(title.contains("Forced"));
        assert!(title.contains("External"));
        assert!(title.contains("SRT"));

        // A non-audio/video/subtitle stream has no display title.
        assert_eq!(stream(MediaStreamType::Data).display_title(), None);
    }

    #[test]
    fn subtitle_format_predicates_classify_codecs() {
        assert!(MediaStream::is_text_format(Some("srt")));
        assert!(MediaStream::is_text_format(Some("microdvd")));
        assert!(!MediaStream::is_text_format(Some("pgssub")));
        assert!(MediaStream::is_pgs_format(Some("pgssub")));
        assert!(MediaStream::is_pgs_format(Some("sup")));
        assert!(MediaStream::is_vob_sub_format(Some("dvdsub")));
        assert!(MediaStream::is_vob_sub_format(Some("vobsub")));

        let mut text = stream(MediaStreamType::Subtitle);
        text.codec = Some("srt".to_owned());
        text.is_external = true;
        assert!(text.is_text_subtitle_stream());
        assert!(text.is_extractable_subtitle_stream());
        assert!(text.supports_subtitle_conversion_to("vtt"));
        assert!(!text.supports_subtitle_conversion_to("ass"));

        let mut ass = stream(MediaStreamType::Subtitle);
        ass.codec = Some("ass".to_owned());
        ass.is_external = true;
        assert!(!ass.supports_subtitle_conversion_to("vtt"));

        let mut pgs = stream(MediaStreamType::Subtitle);
        pgs.codec = Some("pgssub".to_owned());
        pgs.is_external = true;
        assert!(pgs.is_pgs_subtitle_stream());

        let mut vob = stream(MediaStreamType::Subtitle);
        vob.codec = Some("dvdsub".to_owned());
        vob.is_external = true;
        assert!(vob.is_vob_sub_subtitle_stream());

        // Non-subtitle streams are never any subtitle format.
        assert!(!stream(MediaStreamType::Video).is_text_subtitle_stream());
        assert!(!stream(MediaStreamType::Video).is_pgs_subtitle_stream());
        assert!(!stream(MediaStreamType::Video).is_vob_sub_subtitle_stream());
    }

    #[test]
    fn video_color_range_maps_dovi_and_transfer_metadata() {
        // Dolby Vision profile 5 → HDR/Dovi.
        let mut dovi = stream(MediaStreamType::Video);
        dovi.dv_profile = Some(5);
        dovi.rpu_present_flag = Some(1);
        dovi.bl_present_flag = Some(1);
        dovi.dv_bl_signal_compatibility_id = Some(0);
        assert_eq!(dovi.video_range(), VideoRange::Hdr);
        assert_eq!(dovi.video_range_type(), VideoRangeType::Dovi);
        assert_eq!(
            dovi.video_dovi_title().as_deref(),
            Some("Dolby Vision Profile 5")
        );

        // Profile 8 compat 1 → DoviWithHdr10; with HDR10+ flag → DoviWithHdr10Plus.
        let mut p8 = stream(MediaStreamType::Video);
        p8.dv_profile = Some(8);
        p8.rpu_present_flag = Some(1);
        p8.bl_present_flag = Some(1);
        p8.dv_bl_signal_compatibility_id = Some(1);
        assert_eq!(p8.video_range_type(), VideoRangeType::DoviWithHdr10);
        p8.hdr10_plus_present_flag = Some(true);
        assert_eq!(p8.video_range_type(), VideoRangeType::DoviWithHdr10Plus);

        // Plain HDR10 via color transfer.
        let mut hdr10 = stream(MediaStreamType::Video);
        hdr10.color_transfer = Some("smpte2084".to_owned());
        assert_eq!(hdr10.video_range_type(), VideoRangeType::Hdr10);
        hdr10.hdr10_plus_present_flag = Some(true);
        assert_eq!(hdr10.video_range_type(), VideoRangeType::Hdr10Plus);

        // HLG.
        let mut hlg = stream(MediaStreamType::Video);
        hlg.color_transfer = Some("arib-std-b67".to_owned());
        assert_eq!(hlg.video_range_type(), VideoRangeType::Hlg);

        // SDR default + non-video short-circuit.
        assert_eq!(
            stream(MediaStreamType::Video).video_range(),
            VideoRange::Sdr
        );
        assert_eq!(
            stream(MediaStreamType::Audio).get_video_color_range(),
            (VideoRange::Unknown, VideoRangeType::Unknown)
        );
    }

    /// A DoVi video stream at `profile`/`compat`, with RPU+BL present.
    fn dovi(profile: i32, compat: i32) -> MediaStream {
        let mut s = stream(MediaStreamType::Video);
        s.dv_profile = Some(profile);
        s.rpu_present_flag = Some(1);
        s.bl_present_flag = Some(1);
        s.dv_bl_signal_compatibility_id = Some(compat);
        s
    }

    #[test]
    fn video_color_range_covers_all_dovi_profiles() {
        // Profile 8 SDR (compat 2) and HLG (compat 4).
        assert_eq!(dovi(8, 2).video_range_type(), VideoRangeType::DoviWithSdr);
        assert_eq!(dovi(8, 4).video_range_type(), VideoRangeType::DoviWithHlg);
        // Profile 8 with an out-of-spec compat → invalid.
        assert_eq!(dovi(8, 6).video_range_type(), VideoRangeType::DoviInvalid);

        // Profile 7 → DoviWithEl; with HDR10+ flag → DoviWithElhdr10Plus.
        assert_eq!(dovi(7, 0).video_range_type(), VideoRangeType::DoviWithEl);
        let mut p7 = dovi(7, 0);
        p7.hdr10_plus_present_flag = Some(true);
        assert_eq!(p7.video_range_type(), VideoRangeType::DoviWithElhdr10Plus);

        // Profile 10 across its compat table.
        assert_eq!(dovi(10, 0).video_range_type(), VideoRangeType::Dovi);
        assert_eq!(
            dovi(10, 1).video_range_type(),
            VideoRangeType::DoviWithHdr10
        );
        assert_eq!(dovi(10, 2).video_range_type(), VideoRangeType::DoviWithSdr);
        assert_eq!(dovi(10, 4).video_range_type(), VideoRangeType::DoviWithHlg);
        // Compat 6 passes the DoVi flag gate but is out-of-spec for the inner
        // profile-10 table → invalid.
        assert_eq!(dovi(10, 6).video_range_type(), VideoRangeType::DoviInvalid);

        // A codec-tag-driven DoVi (no profile) falls back to plain SDR/Sdr.
        let mut tagged = stream(MediaStreamType::Video);
        tagged.codec_tag = Some("dvh1".to_owned());
        assert_eq!(tagged.video_range_type(), VideoRangeType::Sdr);
    }

    #[test]
    fn dovi_title_covers_compat_suffixes() {
        assert_eq!(
            dovi(8, 2).video_dovi_title().as_deref(),
            Some("Dolby Vision Profile 8.2 (SDR)")
        );
        assert_eq!(
            dovi(8, 4).video_dovi_title().as_deref(),
            Some("Dolby Vision Profile 8.4 (HLG)")
        );
        assert_eq!(
            dovi(8, 6).video_dovi_title().as_deref(),
            Some("Dolby Vision Profile 8.6 (HDR10)")
        );
        assert_eq!(
            dovi(8, 1).video_dovi_title().as_deref(),
            Some("Dolby Vision Profile 8.1 (HDR10)")
        );
        // No RPU → no DoVi title.
        assert_eq!(stream(MediaStreamType::Video).video_dovi_title(), None);
    }

    #[test]
    fn display_title_dedupes_attributes_already_in_title() {
        // The channels-without-layout branch, plus title-dedup: the title already
        // contains "AAC" so it is not appended again.
        let mut s = stream(MediaStreamType::Audio);
        s.title = Some("My AAC Track".to_owned());
        s.codec = Some("aac".to_owned());
        s.channels = Some(2);
        let title = s.display_title().unwrap();
        assert!(title.starts_with("My AAC Track"));
        assert!(title.contains("2 ch"));
    }

    #[test]
    fn audio_spatial_format_and_reference_frame_rate() {
        let mut atmos = stream(MediaStreamType::Audio);
        atmos.profile = Some("Dolby Atmos".to_owned());
        assert_eq!(atmos.audio_spatial_format(), AudioSpatialFormat::DolbyAtmos);

        let mut dtsx = stream(MediaStreamType::Audio);
        dtsx.profile = Some("DTS:X".to_owned());
        assert_eq!(dtsx.audio_spatial_format(), AudioSpatialFormat::Dtsx);

        // No profile / non-audio → None.
        assert_eq!(
            stream(MediaStreamType::Audio).audio_spatial_format(),
            AudioSpatialFormat::None
        );

        // Reference frame rate prefers a realistic average, else falls back.
        let mut fr = stream(MediaStreamType::Video);
        fr.average_frame_rate = Some(23.976);
        assert_eq!(fr.reference_frame_rate(), Some(23.976));
        fr.average_frame_rate = Some(9999.0);
        fr.real_frame_rate = Some(25.0);
        assert_eq!(fr.reference_frame_rate(), Some(25.0));
    }

    #[test]
    fn media_stream_type_deserializes_from_int_and_string() {
        // A MediaStream's stream_type accepts both the numeric discriminant and
        // the PascalCase name on the wire.
        for (int_wire, str_wire, expected) in [
            (0, "Audio", MediaStreamType::Audio),
            (1, "Video", MediaStreamType::Video),
            (2, "Subtitle", MediaStreamType::Subtitle),
            (3, "EmbeddedImage", MediaStreamType::EmbeddedImage),
            (4, "Data", MediaStreamType::Data),
            (5, "Lyric", MediaStreamType::Lyric),
        ] {
            let from_int: MediaStream =
                serde_json::from_value(serde_json::json!({ "Type": int_wire })).unwrap();
            assert_eq!(from_int.stream_type, expected);
            let from_str: MediaStream =
                serde_json::from_value(serde_json::json!({ "Type": str_wire })).unwrap();
            assert_eq!(from_str.stream_type, expected);
        }
        // Out-of-range int and unknown string both error.
        assert!(serde_json::from_value::<MediaStream>(serde_json::json!({ "Type": 99 })).is_err());
        assert!(
            serde_json::from_value::<MediaStream>(serde_json::json!({ "Type": "Nope" })).is_err()
        );
    }

    #[test]
    fn virtual_folder_info_carries_library_options() {
        let info = VirtualFolderInfo {
            name: Some("Movies".to_owned()),
            library_options: Some(crate::configuration::LibraryOptions::default()),
            ..VirtualFolderInfo::default()
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("LibraryOptions").is_some());
        let back: VirtualFolderInfo = serde_json::from_value(json).unwrap();
        assert_eq!(back, info);
    }
}
