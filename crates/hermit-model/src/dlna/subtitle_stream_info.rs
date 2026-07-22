//! Port of `MediaBrowser.Model.Dlna.SubtitleStreamInfo`.

use super::enums::SubtitleDeliveryMethod;

/// Information on a single subtitle stream in the output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubtitleStreamInfo {
    /// Gets or sets the URL.
    pub url: Option<String>,

    /// Gets or sets the language.
    pub language: Option<String>,

    /// Gets or sets the name.
    pub name: Option<String>,

    /// Gets or sets a value indicating whether the subtitle is forced.
    pub is_forced: bool,

    /// Gets or sets the format.
    pub format: Option<String>,

    /// Gets or sets the display title.
    pub display_title: Option<String>,

    /// Gets or sets the index.
    pub index: i32,

    /// Gets or sets the delivery method.
    pub delivery_method: SubtitleDeliveryMethod,

    /// Gets or sets a value indicating whether the URL is external.
    pub is_external_url: bool,
}
