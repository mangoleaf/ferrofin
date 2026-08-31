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

/// A plugin's manifest, as `POST /Plugins/{pluginId}/Manifest` returns it.
///
/// Port of `MediaBrowser.Common/Plugins/PluginManifest.cs` (v10.11.8;
/// byte-identical on master).
///
/// **This DTO is camelCase on purpose — do not "fix" it to PascalCase for
/// consistency with the rest of the API.** Every property upstream carries an
/// explicit lowercase `[JsonPropertyName]`, which overrides the server's
/// PascalCase naming policy, and the id is spelled `guid`, not `Id`. Both were
/// verified against a live 10.11.8, which answers this route with
/// `{"category":…,"guid":"b8715ed16c4745289ad3f72deb539cd4",…}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    /// The plugin's category.
    pub category: String,

    /// The changelog text.
    pub changelog: String,

    /// The manifest's own description, which is NOT
    /// [`PluginInfo::description`]: that one comes from `IPlugin.Description`,
    /// this one from the on-disk manifest, and a bundled plugin leaves it empty.
    pub description: String,

    /// The plugin's unique id. Spelled `guid` on the wire
    /// (`[JsonPropertyName("guid")] public Guid Id`), in the dashless `N` form
    /// `JsonGuidConverter` writes.
    #[schema(value_type = String, format = "uuid")]
    #[serde(rename = "guid", with = "crate::json::guid")]
    pub id: Uuid,

    /// The plugin's name.
    pub name: String,

    /// An overview of the plugin.
    pub overview: String,

    /// The plugin's owner.
    pub owner: String,

    /// The compatibility version the plugin targets.
    pub target_abi: String,

    /// The manifest's timestamp — `DateTime.MinValue` when it carries none.
    #[schema(value_type = String, format = "date-time")]
    #[serde(with = "crate::json::datetime")]
    pub timestamp: chrono::DateTime<chrono::Utc>,

    /// The plugin's version.
    pub version: String,

    /// The plugin's operational status.
    pub status: PluginStatus,

    /// Whether the plugin should update itself automatically.
    pub auto_update: bool,

    /// The bundled image's path, relative to the plugin folder.
    ///
    /// `DefaultIgnoreCondition = WhenWritingNull` drops the key when there is
    /// no image, which is why it is absent from a bundled plugin's manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,

    /// The assemblies to load, relative to the plugin folder. Always empty
    /// here: Ferrofin loads no .NET assemblies.
    pub assemblies: Vec<String>,
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
/// Ferrofin ships no dynamic plugin host, so the list is always empty, but the
/// type is part of the wire contract.
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
