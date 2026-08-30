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

    /// The version as a structured value — the C# read-only computed property
    /// `public SysVersion VersionNumber => _version ?? new SysVersion(0, 0, 0);`.
    ///
    /// It is a *getter*, so it is on the wire on every response and the vendored
    /// 10.11.8 contract declares it non-nullable `readOnly` — hence a plain
    /// `String` that always serializes, never a skipped `Option`. A repository
    /// manifest never carries the key (it is server-computed), so it is
    /// `#[serde(default)]` on the way in and
    /// [`fill_version_number`](Self::fill_version_number) derives it from
    /// [`version`](Self::version) on the way out.
    #[serde(rename = "VersionNumber", default)]
    pub version_number: String,

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
    ///
    /// `#[serde(default)]` mirrors the C# `= string.Empty` initialiser, and it is
    /// load-bearing rather than cosmetic: this field and `repositoryUrl` are
    /// stamped by the SERVER after the fetch
    /// (`InstallationManager.GetPackages`: `ver.RepositoryName = manifestName;`),
    /// so **no real manifest contains either key** — 0 of the 278 version entries
    /// in repo.jellyfin.org's live manifest do. Without the default, serde
    /// rejected the whole document and every repository catalogue came back empty.
    #[serde(rename = "repositoryName", default)]
    pub repository_name: String,

    /// Gets or sets the repository url. Server-stamped; see
    /// [`repository_name`](Self::repository_name).
    #[serde(rename = "repositoryUrl", default)]
    pub repository_url: String,
}

impl VersionInfo {
    /// Derives [`version_number`](Self::version_number) from
    /// [`version`](Self::version), as the C# computed property does.
    ///
    /// `VersionInfo.Version`'s setter is `_version = SysVersion.Parse(value)` and
    /// its getter is `_version.ToString()`, so the two are the same normalized
    /// string; an entry with no version at all reports `SysVersion(0, 0, 0)`.
    pub fn fill_version_number(&mut self) {
        if self.version.is_empty() {
            "0.0.0".clone_into(&mut self.version_number);
        } else {
            self.version_number.clone_from(&self.version);
        }
    }
}

/// Information about a plugin package available from a repository.
///
/// Every field is `#[serde(default)]`, mirroring the C# parameterless
/// constructor that initialises each property (`Name = string.Empty`, …): a
/// third-party manifest that omits one key must not take the whole catalogue
/// down with it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct PackageInfo {
    /// Gets or sets the name.
    #[serde(rename = "name", default)]
    pub name: String,

    /// Gets or sets a long description of the plugin.
    #[serde(rename = "description", default)]
    pub description: String,

    /// Gets or sets a short overview of what the plugin does.
    #[serde(rename = "overview", default)]
    pub overview: String,

    /// Gets or sets the owner.
    #[serde(rename = "owner", default)]
    pub owner: String,

    /// Gets or sets the category.
    #[serde(rename = "category", default)]
    pub category: String,

    /// Gets or sets the guid of the assembly associated with this plugin.
    #[serde(rename = "guid", default)]
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,

    /// Gets or sets the versions.
    #[serde(rename = "versions", default)]
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

    /// A REAL repository manifest entry — the exact key set repo.jellyfin.org
    /// publishes — must deserialize.
    ///
    /// `repositoryName`/`repositoryUrl` are stamped by the server after the
    /// fetch (`InstallationManager.GetPackages`), so no manifest carries them;
    /// before they were `#[serde(default)]` serde failed the whole document with
    /// "missing field `repositoryName`" and Ferrofin's plugin catalogue was
    /// permanently empty against every real repository. Every fixture in the
    /// plugin-manager tests hand-wrote the two keys, which is why a green test
    /// suite never caught it.
    #[test]
    fn real_repository_manifest_entry_deserializes_without_server_stamped_fields() {
        let manifest = serde_json::json!([{
            "category": "Metadata",
            "guid": "9c4e63f1-031b-4f25-988b-4f7d78a8b53e",
            "name": "Bookshelf",
            "description": "Book metadata",
            "overview": "Book metadata",
            "owner": "jellyfin",
            "versions": [{
                "checksum": "d41d8cd98f00b204e9800998ecf8427e",
                "changelog": "-",
                "targetAbi": "10.11.0.0",
                "sourceUrl": "https://repo.jellyfin.org/files/plugin/bookshelf/x.zip",
                "timestamp": "2025-01-01T00:00:00Z",
                "version": "13.0.0.0"
            }]
        }]);
        let packages: Vec<PackageInfo> = serde_json::from_value(manifest).expect("manifest parses");
        assert_eq!(packages.len(), 1);
        let version = &packages[0].versions[0];
        assert_eq!(version.repository_name, "");
        assert_eq!(version.repository_url, "");
        assert_eq!(version.version, "13.0.0.0");
    }

    /// A manifest entry missing optional descriptive keys still parses (the C#
    /// parameterless constructor initialises every property).
    #[test]
    fn manifest_entry_missing_optional_keys_parses_to_defaults() {
        let packages: Vec<PackageInfo> =
            serde_json::from_value(serde_json::json!([{ "name": "Sparse" }]))
                .expect("sparse manifest parses");
        assert_eq!(packages[0].name, "Sparse");
        assert_eq!(packages[0].owner, "");
        assert!(packages[0].versions.is_empty());
        assert!(packages[0].id.is_nil());
    }

    /// `VersionNumber` is a C# getter, so it is always on the wire; the vendored
    /// contract declares it non-nullable `readOnly`.
    #[test]
    fn version_number_is_derived_from_version_and_always_serialized() {
        let mut version = VersionInfo {
            version: "13.0.0.0".to_owned(),
            ..VersionInfo::default()
        };
        version.fill_version_number();
        assert_eq!(version.version_number, "13.0.0.0");

        let mut empty = VersionInfo::default();
        empty.fill_version_number();
        assert_eq!(empty.version_number, "0.0.0", "SysVersion(0, 0, 0)");

        let json = serde_json::to_value(&empty).unwrap();
        assert_eq!(json["VersionNumber"], "0.0.0");
        assert!(json.get("VersionNumber").is_some_and(|v| !v.is_null()));
    }

    #[test]
    fn version_info_uses_camel_case_field_names() {
        let value = VersionInfo {
            version: "1.0.0".to_owned(),
            version_number: "1.0.0".to_owned(),
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
