//! Port of `MediaBrowser.Model.Subtitles`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A subtitle font file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct FontFile {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the size.
    pub size: i64,

    /// Gets or sets the date created.
    #[schema(value_type = String, format = "date-time")]
    #[serde(with = "crate::json::datetime")]
    pub date_created: DateTime<Utc>,

    /// Gets or sets the date modified.
    #[schema(value_type = String, format = "date-time")]
    #[serde(with = "crate::json::datetime")]
    pub date_modified: DateTime<Utc>,
}
