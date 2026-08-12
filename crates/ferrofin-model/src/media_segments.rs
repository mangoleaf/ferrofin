//! Port of `MediaBrowser.Model.MediaSegments`.
//!
//! [`MediaSegmentType`] lives in the out-of-tree
//! `Jellyfin.Database.Implementations.Enums` upstream; it is defined here as the
//! DTO references it and it has no dedicated port unit.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// The type of content a media segment defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum MediaSegmentType {
    /// An unknown segment.
    #[default]
    Unknown,
    /// A commercial segment.
    Commercial,
    /// A preview segment.
    Preview,
    /// A recap segment.
    Recap,
    /// An outro segment.
    Outro,
    /// An intro segment.
    Intro,
}

/// API model for a media segment.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSegmentDto {
    /// Gets or sets the id of the media segment.
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,

    /// Gets or sets the id of the associated item.
    #[schema(value_type = String, format = "uuid")]
    pub item_id: Uuid,

    /// Gets or sets the type of content this segment defines.
    #[serde(rename = "Type")]
    pub type_: MediaSegmentType,

    /// Gets or sets the start of the segment.
    pub start_ticks: i64,

    /// Gets or sets the end of the segment.
    pub end_ticks: i64,
}

/// Model containing the arguments for enumerating the requested media item.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSegmentGenerationRequest {
    /// Gets the id of the `BaseItem` the segments should be extracted from.
    #[schema(value_type = String, format = "uuid")]
    pub item_id: Uuid,

    /// Gets existing media segments generated on an earlier scan by this
    /// provider.
    pub existing_segments: Vec<MediaSegmentDto>,
}
