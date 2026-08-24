//! `RemoteSubtitleInfo` — port of
//! `MediaBrowser.Model.Providers.RemoteSubtitleInfo`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A remote subtitle candidate for an item.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct RemoteSubtitleInfo {
    /// Gets or sets the three-letter ISO language name.
    #[serde(
        rename = "ThreeLetterISOLanguageName",
        skip_serializing_if = "Option::is_none"
    )]
    pub three_letter_iso_language_name: Option<String>,

    /// Gets or sets the identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the provider name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,

    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,

    /// Gets or sets the author.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,

    /// Gets or sets the comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,

    /// Gets or sets the date created.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub date_created: Option<DateTime<Utc>>,

    /// Gets or sets the community rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_rating: Option<f32>,

    /// Gets or sets the frame rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f32>,

    /// Gets or sets the download count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_count: Option<i32>,

    /// Gets or sets a value indicating whether this is a hash match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_hash_match: Option<bool>,

    /// Gets or sets a value indicating whether the subtitle is AI-translated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_translated: Option<bool>,

    /// Gets or sets a value indicating whether the subtitle is machine-translated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_translated: Option<bool>,

    /// Gets or sets a value indicating whether the subtitle is forced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forced: Option<bool>,

    /// Gets or sets a value indicating whether the subtitle is hearing-impaired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hearing_impaired: Option<bool>,
}
