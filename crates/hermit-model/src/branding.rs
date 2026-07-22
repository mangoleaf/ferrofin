//! Port of `MediaBrowser.Model.Branding`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The branding options.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct BrandingOptions {
    /// Gets or sets the login disclaimer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_disclaimer: Option<String>,

    /// Gets or sets the custom CSS.
    #[serde(rename = "CustomCss", skip_serializing_if = "Option::is_none")]
    pub custom_css: Option<String>,

    /// Gets or sets a value indicating whether to enable the splashscreen.
    pub splashscreen_enabled: bool,

    /// Gets or sets the splashscreen location on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub splashscreen_location: Option<String>,
}

/// The branding options DTO for API use.
///
/// This DTO excludes `SplashscreenLocation` to prevent it from being updated
/// via the API.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct BrandingOptionsDto {
    /// Gets or sets the login disclaimer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_disclaimer: Option<String>,

    /// Gets or sets the custom CSS.
    #[serde(rename = "CustomCss", skip_serializing_if = "Option::is_none")]
    pub custom_css: Option<String>,

    /// Gets or sets a value indicating whether to enable the splashscreen.
    pub splashscreen_enabled: bool,
}
