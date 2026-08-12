//! Port of `MediaBrowser.Model.Dto` — the core content DTOs.
//!
//! The pure enums (`MediaSourceType`, `RatingType`, `RecommendationType`) live
//! here alongside the struct DTOs split into submodules. Serde casing matches
//! the Jellyfin JSON contract (PascalCase properties), verified against the
//! vendored OpenAPI spec.
//!
//! A handful of small enums referenced by [`BaseItemDto`] belong to
//! `MediaBrowser.Model.Library`/`MediaBrowser.Model.LiveTv` (later port units).
//! They are stubbed here (see [`PlayAccess`], [`ChannelType`], [`ProgramAudio`])
//! so the contract-complete DTO can be expressed now; when those namespaces are
//! ported the definitions should move and these re-exports be removed.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

mod base_item;
mod base_item_person;
mod client_capabilities_dto;
mod config_image_types;
mod device_info_dto;
mod display_preferences_dto;
mod image_info;
mod item_counts;
mod media_source_info;
mod metadata_editor_info;
mod name_pairs;
mod playlist_dto;
mod recommendation_dto;
mod session_info_dto;
mod special_view_option_dto;
mod trickplay_info_dto;
mod user_dto;
mod user_item_data_dto;

pub use base_item::BaseItemDto;
pub use base_item_person::BaseItemPerson;
pub use client_capabilities_dto::ClientCapabilitiesDto;
pub use config_image_types::ConfigImageTypes;
pub use device_info_dto::{DeviceInfoDto, DeviceOptionsDto};
pub use display_preferences_dto::{DisplayPreferencesDto, ScrollDirection, SortOrder};
pub use image_info::ImageInfo;
pub use item_counts::ItemCounts;
pub use media_source_info::MediaSourceInfo;
pub use metadata_editor_info::MetadataEditorInfo;
pub use name_pairs::{NameGuidPair, NameIdPair, NameValuePair};
pub use playlist_dto::PlaylistDto;
pub use recommendation_dto::RecommendationDto;
pub use session_info_dto::SessionInfoDto;
pub use special_view_option_dto::SpecialViewOptionDto;
pub use trickplay_info_dto::TrickplayInfoDto;
pub use user_dto::UserDto;
pub use user_item_data_dto::{UpdateUserItemDataDto, UserItemDataDto};

/// The type of a media source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum MediaSourceType {
    /// A default media source.
    Default = 0,
    /// A grouping of media sources.
    Grouping = 1,
    /// A placeholder media source, for example a disc that has to be inserted.
    Placeholder = 2,
}

/// The type of a community rating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum RatingType {
    /// The rating is a numeric score.
    #[default]
    Score,
    /// The rating is based on likes.
    Likes,
}

/// Enum `RecommendationType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum RecommendationType {
    /// Similar to a recently played item.
    SimilarToRecentlyPlayed = 0,
    /// Similar to a liked item.
    SimilarToLikedItem = 1,
    /// Has a director from a recently played item.
    HasDirectorFromRecentlyPlayed = 2,
    /// Has an actor from a recently played item.
    HasActorFromRecentlyPlayed = 3,
    /// Has a liked director.
    HasLikedDirector = 4,
    /// Has a liked actor.
    HasLikedActor = 5,
}

// `PlayAccess` now lives in `crate::library`; `ChannelType` and `ProgramAudio`
// now live in `crate::live_tv`. They are re-exported here so [`BaseItemDto`] can
// continue to reference them through the `dto` module unchanged.
pub use crate::library::PlayAccess;
pub use crate::live_tv::{ChannelType, ProgramAudio};

/// The day of the week (mirrors `System.DayOfWeek`).
///
/// Referenced by [`BaseItemDto::air_days`]; defined here as it is a .NET
/// built-in with no dedicated port unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum DayOfWeek {
    /// Sunday.
    Sunday,
    /// Monday.
    Monday,
    /// Tuesday.
    Tuesday,
    /// Wednesday.
    Wednesday,
    /// Thursday.
    Thursday,
    /// Friday.
    Friday,
    /// Saturday.
    Saturday,
}
