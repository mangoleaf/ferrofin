//! Port of the portable DTOs in `MediaBrowser.Model.Activity`.
//!
//! The `IActivityManager` service interface is a server-side manager and is not
//! part of the wire contract, so it is dropped from this port.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// The log severity of an activity log entry (mirrors
/// `Microsoft.Extensions.Logging.LogLevel`). A wire enum: an undefined stored or
/// requested value is kept, as C# keeps it (`?severity=99` filters on 99).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
pub enum LogLevel {
    /// Trace-level logs.
    Trace,
    /// Debug-level logs.
    Debug,
    /// Informational logs.
    #[default]
    Information,
    /// Warning logs.
    Warning,
    /// Error logs.
    Error,
    /// Critical logs.
    Critical,
    /// Logging disabled.
    None,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

// `Microsoft.Extensions.Logging.LogLevel` (not in Jellyfin's reflection
// inventory): Trace = 0 … None = 6.
crate::json::enums::wire_enum! {
    LogLevel, None, {
        Trace => ("Trace", 0),
        Debug => ("Debug", 1),
        Information => ("Information", 2),
        Warning => ("Warning", 3),
        Error => ("Error", 4),
        Critical => ("Critical", 5),
        None => ("None", 6),
    }
}

/// An activity log entry.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ActivityLogEntry {
    /// Gets or sets the identifier.
    pub id: i64,

    /// Gets or sets the name.
    pub name: String,

    /// Gets or sets the overview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,

    /// Gets or sets the short overview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_overview: Option<String>,

    /// Gets or sets the type.
    #[serde(rename = "Type")]
    pub type_: String,

    /// Gets or sets the item identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,

    /// Gets or sets the date.
    #[schema(value_type = String, format = "date-time")]
    #[serde(with = "crate::json::datetime")]
    pub date: DateTime<Utc>,

    /// Gets or sets the user identifier.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub user_id: Uuid,

    /// Gets or sets the user primary image tag.
    #[deprecated(note = "UserPrimaryImageTag is not used.")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_primary_image_tag: Option<String>,

    /// Gets or sets the log severity.
    pub severity: LogLevel,
}
