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
