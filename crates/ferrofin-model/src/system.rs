//! Port of `MediaBrowser.Model.System`.
//!
//! [`SystemInfo`] inherits `PublicSystemInfo` in C#; the base fields are
//! flattened here (there is no struct inheritance in Rust). Several `[Obsolete]`
//! members are retained because they remain in the wire contract.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::updates::InstallationInfo;

/// The cast receiver application model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct CastReceiverApplication {
    /// Gets or sets the cast receiver application id.
    pub id: String,

    /// Gets or sets the cast receiver application name.
    pub name: String,
}

/// A server log file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LogFile {
    /// Gets or sets the date created.
    #[schema(value_type = String, format = "date-time")]
    #[serde(with = "crate::json::datetime")]
    pub date_created: DateTime<Utc>,

    /// Gets or sets the date modified.
    #[schema(value_type = String, format = "date-time")]
    #[serde(with = "crate::json::datetime")]
    pub date_modified: DateTime<Utc>,

    /// Gets or sets the size.
    pub size: i64,

    /// Gets or sets the name.
    pub name: String,
}

/// The public system information (unauthenticated view).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PublicSystemInfo {
    /// Gets or sets the local address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_address: Option<String>,

    /// Gets or sets the name of the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,

    /// Gets or sets the server version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Gets or sets the product name. This is the `AssemblyProduct` name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_name: Option<String>,

    /// Gets or sets the operating system.
    #[deprecated(note = "This is no longer set")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operating_system: Option<String>,

    /// Gets or sets the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets a value indicating whether the startup wizard is completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_wizard_completed: Option<bool>,
}

/// The full system information (authenticated view). Flattens
/// [`PublicSystemInfo`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(deprecated)]
#[allow(clippy::struct_excessive_bools)]
pub struct SystemInfo {
    // --- Flattened PublicSystemInfo fields ---
    /// Gets or sets the local address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_address: Option<String>,

    /// Gets or sets the name of the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,

    /// Gets or sets the server version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Gets or sets the product name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_name: Option<String>,

    /// Gets or sets the operating system.
    #[deprecated(note = "This is no longer set")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operating_system: Option<String>,

    /// Gets or sets the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets a value indicating whether the startup wizard is completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_wizard_completed: Option<bool>,

    // --- SystemInfo-specific fields ---
    /// Gets or sets the display name of the operating system.
    #[deprecated(note = "This is no longer set")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operating_system_display_name: Option<String>,

    /// Gets or sets the package name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_name: Option<String>,

    /// Gets or sets a value indicating whether this instance has pending
    /// restart.
    pub has_pending_restart: bool,

    /// Gets or sets a value indicating whether the server is shutting down.
    pub is_shutting_down: bool,

    /// Gets or sets a value indicating whether library monitoring is supported.
    pub supports_library_monitor: bool,

    /// Gets or sets the web socket port number.
    pub web_socket_port_number: i32,

    /// Gets or sets the completed installations.
    pub completed_installations: Vec<InstallationInfo>,

    /// Gets or sets a value indicating whether this instance can self restart.
    #[deprecated(note = "This is always true")]
    pub can_self_restart: bool,

    /// Gets or sets a value indicating whether a web browser can be launched.
    #[deprecated(note = "This is always false")]
    pub can_launch_web_browser: bool,

    /// Gets or sets the program data path.
    #[deprecated(note = "Use the newer SystemStorageInfo instead")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_data_path: Option<String>,

    /// Gets or sets the web UI resources path.
    #[deprecated(note = "Use the newer SystemStorageInfo instead")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_path: Option<String>,

    /// Gets or sets the items by name path.
    #[deprecated(note = "Use the newer SystemStorageInfo instead")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items_by_name_path: Option<String>,

    /// Gets or sets the cache path.
    #[deprecated(note = "Use the newer SystemStorageInfo instead")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_path: Option<String>,

    /// Gets or sets the log path.
    #[deprecated(note = "Use the newer SystemStorageInfo instead")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,

    /// Gets or sets the internal metadata path.
    #[deprecated(note = "Use the newer SystemStorageInfo instead")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub internal_metadata_path: Option<String>,

    /// Gets or sets the transcode path.
    #[deprecated(note = "Use the newer SystemStorageInfo instead")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcoding_temp_path: Option<String>,

    /// Gets or sets the list of cast receiver applications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cast_receiver_applications: Option<Vec<CastReceiverApplication>>,

    /// Gets or sets a value indicating whether this instance has an update
    /// available.
    #[deprecated(note = "This should be handled by the package manager")]
    pub has_update_available: bool,

    /// Gets or sets the encoder location.
    #[deprecated(note = "This isn't set correctly anymore")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_location: Option<String>,

    /// Gets or sets the system architecture.
    #[deprecated(note = "This is no longer set")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_architecture: Option<String>,
}

/// Contains information about a specific folder.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct FolderStorageInfo {
    /// Gets the path of the folder in question.
    pub path: String,

    /// Gets the fully resolved path of the folder in question.
    pub resolved_path: String,

    /// Gets the free space of the underlying storage device.
    pub free_space: i64,

    /// Gets the used space of the underlying storage device.
    pub used_space: i64,

    /// Gets the kind of storage device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_type: Option<String>,

    /// Gets the device identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

/// Contains information about a library's storage.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LibraryStorageInfo {
    /// Gets or sets the library id.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,

    /// Gets or sets the name of the library.
    pub name: String,

    /// Gets or sets the storage information about the folders used in a
    /// library.
    pub folders: Vec<FolderStorageInfo>,
}

/// Contains information about the system's storage.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SystemStorageInfo {
    /// Gets or sets the program data folder.
    pub program_data_folder: FolderStorageInfo,

    /// Gets or sets the web UI resources folder.
    pub web_folder: FolderStorageInfo,

    /// Gets or sets the image cache folder.
    pub image_cache_folder: FolderStorageInfo,

    /// Gets or sets the cache folder.
    pub cache_folder: FolderStorageInfo,

    /// Gets or sets the log folder.
    pub log_folder: FolderStorageInfo,

    /// Gets or sets the internal metadata folder.
    pub internal_metadata_folder: FolderStorageInfo,

    /// Gets or sets the transcoding temp folder.
    pub transcoding_temp_folder: FolderStorageInfo,

    /// Gets or sets the storage information of all libraries.
    pub libraries: Vec<LibraryStorageInfo>,
}
