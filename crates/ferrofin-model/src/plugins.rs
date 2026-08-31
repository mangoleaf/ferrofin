//! Port of `MediaBrowser.Model.Plugins`.
//!
//! `IHasWebPages` is a server-side plugin extension interface (not a wire type)
//! and is dropped from this port. `BasePluginConfiguration` is an empty marker
//! base class in C#; it is modeled as an empty struct.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Base plugin configuration (an empty marker base class upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct BasePluginConfiguration {}

/// Plugin load status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum PluginStatus {
    /// This plugin is currently running.
    #[default]
    Active,
    /// This plugin requires a restart in order for it to load.
    Restart,
    /// An attempt to remove this plugin from disk will happen at every restart.
    Deleted,
    /// This plugin has been superseded by another version.
    Superseded,
    /// [DEPRECATED] See [`PluginStatus::Superseded`].
    Superceded,
    /// This plugin caused an error when instantiated.
    Malfunctioned,
    /// This plugin does not meet the `TargetAbi` requirements.
    NotSupported,
    /// This plugin has been marked as disabled.
    Disabled,
}

/// A serializable stub used by the API to provide information about installed
/// plugins.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PluginInfo {
    /// Gets or sets the name.
    pub name: String,

    /// Gets or sets the version.
    pub version: String,

    /// Gets or sets the name of the configuration file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration_file_name: Option<String>,

    /// Gets or sets the description.
    pub description: String,

    /// Gets or sets the unique id.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,

    /// Gets or sets a value indicating whether the plugin can be uninstalled.
    pub can_uninstall: bool,

    /// Gets or sets a value indicating whether this plugin has a valid image.
    pub has_image: bool,

    /// Gets or sets a value indicating the status of the plugin.
    pub status: PluginStatus,
}

/// Defines a plugin's web page.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PluginPageInfo {
    /// Gets or sets the name of the plugin.
    pub name: String,

    /// Gets or sets the display name of the plugin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,

    /// Gets or sets the resource path.
    pub embedded_resource_path: String,

    /// Gets or sets a value indicating whether this plugin should appear in the
    /// main menu.
    pub enable_in_main_menu: bool,

    /// Gets or sets the menu section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub menu_section: Option<String>,

    /// Gets or sets the menu icon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub menu_icon: Option<String>,
}

/// The configuration page info returned by the dashboard controller.
///
/// Port of `MediaBrowser.Model.Plugins.ConfigurationPageInfo`. In Jellyfin this
/// is projected from a plugin's [`PluginPageInfo`] plus the owning plugin id;
/// Ferrofin projects it from the compiled-in registry the same way — Jellyfin's
/// five in-tree provider plugins, the curated extensions, and any loaded WASM
/// plugin each contribute their pages.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ConfigurationPageInfo {
    /// Gets or sets the name.
    pub name: String,

    /// Gets or sets a value indicating whether the configurations page is
    /// enabled in the main menu.
    pub enable_in_main_menu: bool,

    /// Gets or sets the menu section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub menu_section: Option<String>,

    /// Gets or sets the menu icon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub menu_icon: Option<String>,

    /// Gets or sets the display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,

    /// Gets or sets the plugin id.
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, with = "crate::json::guid::option")]
    pub plugin_id: Option<Uuid>,
}

/// A plugin's manifest, as `POST /Plugins/{pluginId}/Manifest` returns it.
///
/// Port of `MediaBrowser.Common.Plugins.PluginManifest`. Every property there
/// carries an explicit `[JsonPropertyName]` in **camelCase**, so this one type
/// does not follow the API's PascalCase default — `guid`, `targetAbi`,
/// `autoUpdate` and the rest are the names on the wire, and `Id` is spelled
/// `guid`. Ferrofin used to answer this route with a five-field PascalCase
/// projection of its own, which no client written against Jellyfin could read.
///
/// The values are the ones upstream fills in for a plugin that has **no
/// `meta.json` on disk**: `PluginManager.CreatePluginInstance` builds a "dummy
/// record" (v10.11.8 `PluginManager.cs:560-575`) setting only `Id`, `Name`,
/// `Version` and `Status`, so the string fields stay at their constructor
/// defaults (empty), `Assemblies` is empty, `Timestamp` is `DateTime.MinValue`
/// and `AutoUpdate` keeps its `true` property initializer. That is exactly what
/// a stock Jellyfin returns for its five in-tree provider plugins, and every
/// Ferrofin plugin — compiled-in or staged WASM — is that same
/// manifest-less shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PluginManifest {
    /// The plugin's category.
    pub category: String,

    /// The changelog information.
    pub changelog: String,

    /// The plugin's description.
    pub description: String,

    /// The plugin's globally unique identifier (C# `Id`, spelled `guid`).
    #[schema(value_type = String, format = "uuid")]
    #[serde(rename = "guid", with = "crate::json::guid")]
    pub id: Uuid,

    /// The plugin's name.
    pub name: String,

    /// An overview of the plugin.
    pub overview: String,

    /// The plugin's owner.
    pub owner: String,

    /// The compatibility version for the plugin.
    #[serde(rename = "targetAbi")]
    pub target_abi: String,

    /// The manifest's timestamp (`DateTime.MinValue` for a manifest-less plugin).
    pub timestamp: String,

    /// The plugin's version number.
    pub version: String,

    /// The plugin's operational status.
    pub status: PluginStatus,

    /// Whether this plugin should automatically update.
    #[serde(rename = "autoUpdate")]
    pub auto_update: bool,

    /// The plugin's image path, relative to its folder. Omitted when absent —
    /// upstream serializes with `DefaultIgnoreCondition = WhenWritingNull`.
    #[serde(rename = "imagePath", skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,

    /// The assemblies that should be loaded (always empty here: Ferrofin has no
    /// .NET assembly loading, by design — see `docs/EXTENSIONS.md`).
    pub assemblies: Vec<String>,
}

impl PluginManifest {
    /// The manifest of a plugin with no `meta.json`, matching the C# dummy
    /// record `PluginManager.CreatePluginInstance` builds.
    ///
    /// `DateTime.MinValue` serializes through Jellyfin's `JsonDateTimeConverter`
    /// as `0001-01-01T00:00:00.0000000Z`; it is a constant here because a
    /// manifest-less plugin has no timestamp to report.
    #[must_use]
    pub fn manifestless(id: Uuid, name: String, version: String, status: PluginStatus) -> Self {
        Self {
            category: String::new(),
            changelog: String::new(),
            description: String::new(),
            id,
            name,
            overview: String::new(),
            owner: String::new(),
            target_abi: String::new(),
            timestamp: "0001-01-01T00:00:00.0000000Z".to_owned(),
            version,
            status,
            auto_update: true,
            image_path: None,
            assemblies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod manifest_tests {
    use super::{PluginManifest, PluginStatus};

    #[test]
    fn manifestless_serializes_with_the_csharp_camel_case_names() {
        let id = uuid::Uuid::parse_str("b8715ed1-6c47-4528-9ad3-f72deb539cd4").expect("uuid");
        let m = PluginManifest::manifestless(
            id,
            "TMDb".to_owned(),
            "10.11.8.0".to_owned(),
            PluginStatus::Active,
        );
        let v: serde_json::Value = serde_json::to_value(&m).expect("serialize");
        // Byte-for-byte the key set a live Jellyfin 10.11.8 returns for TMDb.
        let mut keys: Vec<&str> = v
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "assemblies",
                "autoUpdate",
                "category",
                "changelog",
                "description",
                "guid",
                "name",
                "overview",
                "owner",
                "status",
                "targetAbi",
                "timestamp",
                "version",
            ],
            "imagePath must be omitted when absent (WhenWritingNull)"
        );
        assert_eq!(v["guid"], "b8715ed16c4745289ad3f72deb539cd4");
        assert_eq!(v["status"], "Active");
        assert_eq!(v["autoUpdate"], true);
        assert_eq!(v["timestamp"], "0001-01-01T00:00:00.0000000Z");
        assert_eq!(v["description"], "");
    }
}
