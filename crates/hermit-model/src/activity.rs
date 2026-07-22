//! Port of the portable DTOs in `MediaBrowser.Model.Activity`.
//!
//! The `IActivityManager` service interface is a server-side manager and is not
//! part of the wire contract, so it is dropped from this port.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// The log severity of an activity log entry (mirrors
/// `Microsoft.Extensions.Logging.LogLevel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
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
    pub date: DateTime<Utc>,

    /// Gets or sets the user identifier.
    #[schema(value_type = String, format = "uuid")]
    pub user_id: Uuid,

    /// Gets or sets the user primary image tag.
    #[deprecated(note = "UserPrimaryImageTag is not used.")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_primary_image_tag: Option<String>,

    /// Gets or sets the log severity.
    pub severity: LogLevel,
}
