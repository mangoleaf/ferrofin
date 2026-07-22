//! Port of `MediaBrowser.Model.QuickConnect`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Stores the state of a quick connect request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct QuickConnectResult {
    /// Gets or sets a value indicating whether this request is authorized.
    pub authenticated: bool,

    /// Gets the secret value used to uniquely identify this request. Can be
    /// used to retrieve authentication information.
    pub secret: String,

    /// Gets the user facing code used so the user can quickly differentiate
    /// this request from others.
    pub code: String,

    /// Gets the requesting device id.
    pub device_id: String,

    /// Gets the requesting device name.
    pub device_name: String,

    /// Gets the requesting app name.
    pub app_name: String,

    /// Gets the requesting app version.
    pub app_version: String,

    /// Gets or sets the `DateTime` that this request was created.
    #[schema(value_type = String, format = "date-time")]
    pub date_added: DateTime<Utc>,
}
