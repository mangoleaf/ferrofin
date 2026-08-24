//! Port of `MediaBrowser.Model.Updates`.
//!
//! Casing note: [`PackageInfo`] and [`VersionInfo`] use camelCase property names
//! on the wire (they come from the plugin repository manifest), whereas
//! [`InstallationInfo`] and [`RepositoryInfo`] use PascalCase. Both are matched
//! verbatim against the OpenAPI contract. `System.Version` is a string on the
//! wire, so [`VersionInfo::version`]/[`InstallationInfo::version`] are `String`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Information about an installation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct InstallationInfo {
    /// Gets or sets the id.
    #[serde(rename = "Guid")]
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,

    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Gets or sets the changelog for this version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changelog: Option<String>,

    /// Gets or sets the source URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,

    /// Gets or sets a checksum for the binary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,

    /// Gets or sets package information for the installation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_info: Option<PackageInfo>,
}

/// Defines the version-info manifest entry for a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct VersionInfo {
    /// Gets or sets the version.
    #[serde(rename = "version")]
    pub version: String,

    /// Gets the version as a structured value (mirrors the read-only
    /// `VersionNumber` property).
    #[serde(rename = "VersionNumber", skip_serializing_if = "Option::is_none")]
    pub version_number: Option<String>,

    /// Gets or sets the changelog for this version.
    #[serde(rename = "changelog", skip_serializing_if = "Option::is_none")]
    pub changelog: Option<String>,

    /// Gets or sets the ABI that this version was built against.
    #[serde(rename = "targetAbi", skip_serializing_if = "Option::is_none")]
    pub target_abi: Option<String>,

    /// Gets or sets the source URL.
    #[serde(rename = "sourceUrl", skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,

    /// Gets or sets a checksum for the binary.
    #[serde(rename = "checksum", skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,

    /// Ferrofin extension: a SHA-256 checksum, preferred over the MD5
    /// `checksum` when both are present. Absent from Jellyfin manifests and
    /// skipped when unset, so the wire shape stays Jellyfin-identical unless
    /// a repository opts in.
    #[serde(rename = "sha256", skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,

    /// Gets or sets a timestamp of when the binary was built.
    #[serde(rename = "timestamp", skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,

    /// Gets or sets the repository name.
    #[serde(rename = "repositoryName")]
    pub repository_name: String,

    /// Gets or sets the repository url.
    #[serde(rename = "repositoryUrl")]
    pub repository_url: String,
}

/// Information about a plugin package available from a repository.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct PackageInfo {
    /// Gets or sets the name.
    #[serde(rename = "name")]
    pub name: String,

    /// Gets or sets a long description of the plugin.
    #[serde(rename = "description")]
    pub description: String,

    /// Gets or sets a short overview of what the plugin does.
    #[serde(rename = "overview")]
    pub overview: String,

    /// Gets or sets the owner.
    #[serde(rename = "owner")]
    pub owner: String,

    /// Gets or sets the category.
    #[serde(rename = "category")]
    pub category: String,

    /// Gets or sets the guid of the assembly associated with this plugin.
    #[serde(rename = "guid")]
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,

    /// Gets or sets the versions.
    #[serde(rename = "versions")]
    pub versions: Vec<VersionInfo>,

    /// Gets or sets the image url for the package.
    #[serde(rename = "imageUrl", skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
}

/// Information about a plugin repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct RepositoryInfo {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Gets or sets a value indicating whether the repository is enabled.
    pub enabled: bool,
}

impl Default for RepositoryInfo {
    fn default() -> Self {
        Self {
            name: None,
            url: None,
            enabled: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installation_info_uses_pascal_case_and_guid_alias() {
        let value = InstallationInfo {
            id: Uuid::from_u128(1),
            name: Some("MyPlugin".to_owned()),
            version: Some("1.2.3".to_owned()),
            ..InstallationInfo::default()
        };
        let json = serde_json::to_value(&value).unwrap();
        // The id field serializes under the "Guid" key upstream.
        assert_eq!(json["Guid"], Uuid::from_u128(1).simple().to_string());
        assert_eq!(json["Name"], "MyPlugin");
        assert_eq!(json["Version"], "1.2.3");
        let back: InstallationInfo = serde_json::from_value(json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn version_info_uses_camel_case_field_names() {
        let value = VersionInfo {
            version: "1.0.0".to_owned(),
            version_number: Some("1.0.0".to_owned()),
            changelog: Some("Initial".to_owned()),
            target_abi: Some("10.9.0.0".to_owned()),
            source_url: Some("https://example.com/x.zip".to_owned()),
            checksum: Some("deadbeef".to_owned()),
            sha256: None,
            timestamp: Some("2024-01-01".to_owned()),
            repository_name: "MyRepo".to_owned(),
            repository_url: "https://repo".to_owned(),
        };
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["version"], "1.0.0");
        assert_eq!(json["VersionNumber"], "1.0.0");
        assert_eq!(json["targetAbi"], "10.9.0.0");
        assert_eq!(json["sourceUrl"], "https://example.com/x.zip");
        assert_eq!(json["repositoryName"], "MyRepo");
        assert_eq!(json["repositoryUrl"], "https://repo");
        let back: VersionInfo = serde_json::from_value(json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn package_info_uses_camel_case_and_guid_key() {
        let value = PackageInfo {
            name: "Plugin".to_owned(),
            description: "desc".to_owned(),
            overview: "ov".to_owned(),
            owner: "owner".to_owned(),
            category: "cat".to_owned(),
            id: Uuid::from_u128(42),
            versions: vec![VersionInfo {
                version: "1.0.0".to_owned(),
                repository_name: "r".to_owned(),
                repository_url: "u".to_owned(),
                ..VersionInfo::default()
            }],
            image_url: Some("https://img".to_owned()),
        };
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["name"], "Plugin");
        assert_eq!(json["guid"], Uuid::from_u128(42).simple().to_string());
        assert_eq!(json["imageUrl"], "https://img");
        let back: PackageInfo = serde_json::from_value(json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn repository_info_defaults_enabled_true() {
        let value = RepositoryInfo::default();
        assert!(value.enabled);
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["Enabled"], true);
        let back: RepositoryInfo = serde_json::from_value(json).unwrap();
        assert_eq!(value, back);
    }
}
