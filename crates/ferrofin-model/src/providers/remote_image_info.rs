//! `RemoteImageInfo` — port of `MediaBrowser.Model.Providers.RemoteImageInfo`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::dto::RatingType;
use crate::entities::ImageType;

/// Class `RemoteImageInfo` — a remote image candidate for an item.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct RemoteImageInfo {
    /// Gets or sets the name of the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,

    /// Gets or sets the URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Gets or sets a url used for previewing a smaller version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,

    /// Gets or sets the height.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,

    /// Gets or sets the width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,

    /// Gets or sets the community rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_rating: Option<f64>,

    /// Gets or sets the vote count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vote_count: Option<i32>,

    /// Gets or sets the language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,

    /// Gets or sets the image type.
    #[serde(rename = "Type")]
    pub type_: ImageType,

    /// Gets or sets the type of the rating.
    pub rating_type: RatingType,
}
