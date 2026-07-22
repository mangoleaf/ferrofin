//! Port of `MediaBrowser.Model.Dlna.SubtitleProfile`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::enums::SubtitleDeliveryMethod;
use crate::extensions::contains_container;

/// Declares how a subtitle format is delivered to a device.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[serde(default)]
pub struct SubtitleProfile {
    /// The subtitle format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// The delivery method.
    pub method: SubtitleDeliveryMethod,
    /// The DIDL mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub didl_mode: Option<String>,
    /// The language(s), comma-delimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The container(s), comma-delimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

impl SubtitleProfile {
    /// Returns whether `sub_language` is supported by this profile.
    ///
    /// An empty/`None` [`Self::language`] supports every language; a missing
    /// `sub_language` is treated as `und`.
    #[must_use]
    pub fn supports_language(&self, sub_language: Option<&str>) -> bool {
        let Some(language) = self.language.as_deref().filter(|l| !l.is_empty()) else {
            return true;
        };

        let sub_language = sub_language.filter(|s| !s.is_empty()).unwrap_or("und");
        contains_container(Some(language), Some(sub_language))
    }
}
