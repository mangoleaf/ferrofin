//! `BaseItemDto` — port of `MediaBrowser.Model.Dto.BaseItemDto`.
//!
//! The primary content DTO returned by the API. All members are optional on the
//! wire (Jellyfin omits nulls), so nearly every field is an [`Option`] or a
//! collection. Serde casing matches the OpenAPI contract (PascalCase).

// The `ToSchema`/`Default` derives on this 150+-field DTO expand to large
// stack-materialized arrays; that is inherent to the generated code.
#![allow(clippy::large_stack_arrays)]

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    BaseItemPerson, ChannelType, DayOfWeek, MediaSourceInfo, NameGuidPair, PlayAccess,
    ProgramAudio, TrickplayInfoDto, UserItemDataDto,
};
use crate::data::{BaseItemKind, CollectionType, MediaType};
use crate::drawing::ImageOrientation;
use crate::entities::{
    ExtraType, ImageType, IsoType, LocationType, MetadataField, Video3DFormat, VideoType,
};
use crate::entities_media::{ChapterInfo, IHasProviderIds, MediaStream, MediaUrl};
use crate::providers::ExternalUrl;

/// This is strictly used as a data transfer object from the API layer.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct BaseItemDto {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Gets or sets the original title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_title: Option<String>,
    /// Gets or sets the server identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    /// Gets or sets the id.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,
    /// Gets or sets the etag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Gets or sets the source type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    /// Gets or sets the playlist item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_item_id: Option<String>,
    /// Gets or sets the date created.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub date_created: Option<DateTime<Utc>>,
    /// Gets or sets the date last media added.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub date_last_media_added: Option<DateTime<Utc>>,
    /// Gets or sets the extra type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_type: Option<ExtraType>,
    /// Gets or sets the number of the season an episode airs before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub airs_before_season_number: Option<i32>,
    /// Gets or sets the number of the season an episode airs after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub airs_after_season_number: Option<i32>,
    /// Gets or sets the number of the episode an episode airs before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub airs_before_episode_number: Option<i32>,
    /// Gets or sets a value indicating whether the item can be deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_delete: Option<bool>,
    /// Gets or sets a value indicating whether the item can be downloaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_download: Option<bool>,
    /// Gets or sets a value indicating whether the item has lyrics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_lyrics: Option<bool>,
    /// Gets or sets a value indicating whether the item has subtitles.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_subtitles: Option<bool>,
    /// Gets or sets the preferred metadata language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_metadata_language: Option<String>,
    /// Gets or sets the preferred metadata country code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_metadata_country_code: Option<String>,
    /// Gets or sets the container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Gets or sets the sort name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_name: Option<String>,
    /// Gets or sets the forced sort name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forced_sort_name: Option<String>,
    /// Gets or sets the 3D format.
    #[serde(rename = "Video3DFormat", skip_serializing_if = "Option::is_none")]
    pub video3d_format: Option<Video3DFormat>,
    /// Gets or sets the premiere date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub premiere_date: Option<DateTime<Utc>>,
    /// Gets or sets the external urls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_urls: Option<Vec<ExternalUrl>>,
    /// Gets or sets the media sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_sources: Option<Vec<MediaSourceInfo>>,
    /// Gets or sets the critic rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critic_rating: Option<f32>,
    /// Gets or sets the production locations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_locations: Option<Vec<String>>,
    /// Gets or sets the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Gets or sets a value indicating whether media source display is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_media_source_display: Option<bool>,
    /// Gets or sets the official rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub official_rating: Option<String>,
    /// Gets or sets the custom rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_rating: Option<String>,
    /// Gets or sets the channel identifier. Serialized even when null — Jellyfin always
    /// emits `ChannelId` (null for non-LiveTV items), so it is not skipped.
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub channel_id: Option<Uuid>,
    /// Gets or sets the channel name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    /// Gets or sets the overview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    /// Gets or sets the taglines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taglines: Option<Vec<String>>,
    /// Gets or sets the genres.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genres: Option<Vec<String>>,
    /// Gets or sets the community rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_rating: Option<f32>,
    /// Gets or sets the cumulative run time ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_run_time_ticks: Option<i64>,
    /// Gets or sets the run time ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_time_ticks: Option<i64>,
    /// Gets or sets the play access.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_access: Option<PlayAccess>,
    /// Gets or sets the aspect ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    /// Gets or sets the production year.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_year: Option<i32>,
    /// Gets or sets a value indicating whether the item is a placeholder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_place_holder: Option<bool>,
    /// Gets or sets the number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<String>,
    /// Gets or sets the channel number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_number: Option<String>,
    /// Gets or sets the index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_number: Option<i32>,
    /// Gets or sets the end index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_number_end: Option<i32>,
    /// Gets or sets the parent index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_index_number: Option<i32>,
    /// Gets or sets the remote trailers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_trailers: Option<Vec<MediaUrl>>,
    /// Gets or sets the provider ids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_ids: Option<HashMap<String, String>>,
    /// Gets or sets a value indicating whether the item is HD.
    #[serde(rename = "IsHD", skip_serializing_if = "Option::is_none")]
    pub is_hd: Option<bool>,
    /// Gets or sets a value indicating whether the item is a folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_folder: Option<bool>,
    /// Gets or sets the parent id.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub parent_id: Option<Uuid>,
    /// Gets or sets the type.
    #[serde(rename = "Type")]
    pub type_: BaseItemKind,
    /// Gets or sets the people.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub people: Option<Vec<BaseItemPerson>>,
    /// Gets or sets the studios.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub studios: Option<Vec<NameGuidPair>>,
    /// Gets or sets the genre items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre_items: Option<Vec<NameGuidPair>>,
    /// Gets or sets the parent logo item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub parent_logo_item_id: Option<Uuid>,
    /// Gets or sets the parent backdrop item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub parent_backdrop_item_id: Option<Uuid>,
    /// Gets or sets the parent backdrop image tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_backdrop_image_tags: Option<Vec<String>>,
    /// Gets or sets the local trailer count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_trailer_count: Option<i32>,
    /// Gets or sets the user data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<UserItemDataDto>,
    /// Gets or sets the recursive item count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recursive_item_count: Option<i32>,
    /// Gets or sets the child count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_count: Option<i32>,
    /// Gets or sets the series name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_name: Option<String>,
    /// Gets or sets the series id.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub series_id: Option<Uuid>,
    /// Gets or sets the season id.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub season_id: Option<Uuid>,
    /// Gets or sets the special feature count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub special_feature_count: Option<i32>,
    /// Gets or sets the display preferences id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_preferences_id: Option<String>,
    /// Gets or sets the status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Gets or sets the air time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub air_time: Option<String>,
    /// Gets or sets the air days.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub air_days: Option<Vec<DayOfWeek>>,
    /// Gets or sets the tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Gets or sets the primary image aspect ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_image_aspect_ratio: Option<f64>,
    /// Gets or sets the artists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artists: Option<Vec<String>>,
    /// Gets or sets the artist items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_items: Option<Vec<NameGuidPair>>,
    /// Gets or sets the album.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Gets or sets the collection type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection_type: Option<CollectionType>,
    /// Gets or sets the display order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_order: Option<String>,
    /// Gets or sets the album id.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub album_id: Option<Uuid>,
    /// Gets or sets the album primary image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_primary_image_tag: Option<String>,
    /// Gets or sets the series primary image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_primary_image_tag: Option<String>,
    /// Gets or sets the album artist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    /// Gets or sets the album artists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artists: Option<Vec<NameGuidPair>>,
    /// Gets or sets the season name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season_name: Option<String>,
    /// Gets or sets the media streams.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_streams: Option<Vec<MediaStream>>,
    /// Gets or sets the video type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_type: Option<VideoType>,
    /// Gets or sets the part count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_count: Option<i32>,
    /// Gets or sets the media source count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_source_count: Option<i32>,
    /// Gets or sets the image tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tags: Option<HashMap<ImageType, String>>,
    /// Gets or sets the backdrop image tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backdrop_image_tags: Option<Vec<String>>,
    /// Gets or sets the screenshot image tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot_image_tags: Option<Vec<String>>,
    /// Gets or sets the parent logo image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_logo_image_tag: Option<String>,
    /// Gets or sets the parent art item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub parent_art_item_id: Option<Uuid>,
    /// Gets or sets the parent art image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_art_image_tag: Option<String>,
    /// Gets or sets the series thumb image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_thumb_image_tag: Option<String>,
    /// Gets or sets the image blur hashes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_blur_hashes: Option<HashMap<ImageType, HashMap<String, String>>>,
    /// Gets or sets the series studio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_studio: Option<String>,
    /// Gets or sets the parent thumb item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub parent_thumb_item_id: Option<Uuid>,
    /// Gets or sets the parent thumb image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thumb_image_tag: Option<String>,
    /// Gets or sets the parent primary image item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_primary_image_item_id: Option<String>,
    /// Gets or sets the parent primary image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_primary_image_tag: Option<String>,
    /// Gets or sets the chapters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chapters: Option<Vec<ChapterInfo>>,
    /// Gets or sets the trickplay manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trickplay: Option<HashMap<String, HashMap<i32, TrickplayInfoDto>>>,
    /// Gets or sets the location type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_type: Option<LocationType>,
    /// Gets or sets the ISO type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iso_type: Option<IsoType>,
    /// Gets or sets the media type.
    pub media_type: MediaType,
    /// Gets or sets the end date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub end_date: Option<DateTime<Utc>>,
    /// Gets or sets the locked fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked_fields: Option<Vec<MetadataField>>,
    /// Gets or sets the trailer count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailer_count: Option<i32>,
    /// Gets or sets the movie count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub movie_count: Option<i32>,
    /// Gets or sets the series count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_count: Option<i32>,
    /// Gets or sets the program count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_count: Option<i32>,
    /// Gets or sets the episode count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode_count: Option<i32>,
    /// Gets or sets the song count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub song_count: Option<i32>,
    /// Gets or sets the album count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_count: Option<i32>,
    /// Gets or sets the artist count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_count: Option<i32>,
    /// Gets or sets the music video count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub music_video_count: Option<i32>,
    /// Gets or sets a value indicating whether the metadata is locked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_data: Option<bool>,
    /// Gets or sets the width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    /// Gets or sets the height.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
    /// Gets or sets the camera make.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_make: Option<String>,
    /// Gets or sets the camera model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_model: Option<String>,
    /// Gets or sets the software.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software: Option<String>,
    /// Gets or sets the exposure time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure_time: Option<f64>,
    /// Gets or sets the focal length.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focal_length: Option<f64>,
    /// Gets or sets the image orientation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_orientation: Option<ImageOrientation>,
    /// Gets or sets the aperture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aperture: Option<f64>,
    /// Gets or sets the shutter speed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutter_speed: Option<f64>,
    /// Gets or sets the latitude.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    /// Gets or sets the longitude.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    /// Gets or sets the altitude.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub altitude: Option<f64>,
    /// Gets or sets the ISO speed rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iso_speed_rating: Option<i32>,
    /// Gets or sets the series timer id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_timer_id: Option<String>,
    /// Gets or sets the program id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_id: Option<String>,
    /// Gets or sets the channel primary image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_primary_image_tag: Option<String>,
    /// Gets or sets the start date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub start_date: Option<DateTime<Utc>>,
    /// Gets or sets the completion percentage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_percentage: Option<f64>,
    /// Gets or sets a value indicating whether the program is a repeat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_repeat: Option<bool>,
    /// Gets or sets the episode title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode_title: Option<String>,
    /// Gets or sets the channel type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_type: Option<ChannelType>,
    /// Gets or sets the program audio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<ProgramAudio>,
    /// Gets or sets a value indicating whether the item is a movie.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_movie: Option<bool>,
    /// Gets or sets a value indicating whether the item is sports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_sports: Option<bool>,
    /// Gets or sets a value indicating whether the item is a series.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_series: Option<bool>,
    /// Gets or sets a value indicating whether the item is live.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_live: Option<bool>,
    /// Gets or sets a value indicating whether the item is news.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_news: Option<bool>,
    /// Gets or sets a value indicating whether the item is kids content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_kids: Option<bool>,
    /// Gets or sets a value indicating whether the item is a premiere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_premiere: Option<bool>,
    /// Gets or sets the timer id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timer_id: Option<String>,
    /// Gets or sets the gain required for audio normalization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalization_gain: Option<f32>,
    /// Gets or sets the gain required for album audio normalization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_normalization_gain: Option<f32>,
    /// Gets or sets the current program.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_program: Option<Box<BaseItemDto>>,
}

impl IHasProviderIds for BaseItemDto {
    fn provider_ids(&self) -> Option<&HashMap<String, String>> {
        self.provider_ids.as_ref()
    }

    fn provider_ids_mut(&mut self) -> &mut HashMap<String, String> {
        self.provider_ids.get_or_insert_with(HashMap::new)
    }

    fn provider_ids_opt_mut(&mut self) -> &mut Option<HashMap<String, String>> {
        &mut self.provider_ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_through_json() {
        let value = BaseItemDto::default();
        let json = serde_json::to_string(&value).unwrap();
        let back: BaseItemDto = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn populated_round_trips_and_omits_none() {
        let value = BaseItemDto {
            name: Some("Inception".to_owned()),
            id: Uuid::from_u128(9),
            production_year: Some(2010),
            ..BaseItemDto::default()
        };
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["Name"], "Inception");
        assert_eq!(json["Id"], Uuid::from_u128(9).simple().to_string());
        assert_eq!(json["ProductionYear"], 2010);
        // None fields are omitted from the wire form.
        assert!(json.get("OriginalTitle").is_none());
        let back: BaseItemDto = serde_json::from_value(json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn provider_ids_trait_accessors() {
        let mut value = BaseItemDto::default();
        assert!(value.provider_ids().is_none());
        value
            .provider_ids_mut()
            .insert("Imdb".to_owned(), "tt1375666".to_owned());
        assert_eq!(value.provider_ids().unwrap()["Imdb"], "tt1375666");
        *value.provider_ids_opt_mut() = None;
        assert!(value.provider_ids().is_none());
    }
}
