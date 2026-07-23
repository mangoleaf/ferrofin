//! Port of `Jellyfin.Api.Models.EnvironmentDtos`.
//!
//! The request/response DTOs the `EnvironmentController` filesystem-browse
//! endpoints exchange. The directory-listing responses reuse
//! [`FileSystemEntryInfo`](crate::io::FileSystemEntryInfo).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Validate path object (request body of `POST /Environment/ValidatePath`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ValidatePathDto {
    /// Gets or sets a value indicating whether to validate if the path is
    /// writable.
    pub validate_writable: bool,

    /// Gets or sets the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Gets or sets a value indicating whether the path is a file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_file: Option<bool>,
}

/// Default directory browser info (response of
/// `GET /Environment/DefaultDirectoryBrowser`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct DefaultDirectoryBrowserInfoDto {
    /// Gets or sets the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}
