//! Local port of the `MediaBrowser.Model.Globalization.ILocalizationManager`
//! seam consumed by [`super::ExternalPathParser`].
//!
//! Only [`LocalizationManager::find_language_info`] is used by the naming code;
//! the full server-side manager lives elsewhere. Consumers provide an impl.

use hermit_model::globalization::CultureDto;

/// The subset of `ILocalizationManager` the external path parser needs.
pub trait LocalizationManager {
    /// Finds the [`CultureDto`] matching a language token, if any.
    fn find_language_info(&self, language: &str) -> Option<CultureDto>;
}
