//! Localization abstraction consumed by the probe normalizer.
//!
//! Upstream `ProbeResultNormalizer` takes an `ILocalizationManager` to resolve
//! the localized "Default"/"External"/… labels and language display names it
//! stamps onto each [`ferrofin_model::entities_media::MediaStream`]. That lookup
//! is pure (no I/O), so it is modeled as a small trait here; tests inject a
//! passthrough implementation exactly as the C# tests inject a `Mock`, and the
//! server injects the real culture data through [`TraitLocalization`].

use std::sync::Arc;

/// Resolves localized display strings for probe output.
pub trait LocalizationManager {
    /// Returns the localized string for the given phrase key, or the key
    /// itself when no translation is available.
    fn get_localized_string(&self, phrase: &str) -> String;

    /// Returns the display name for a language code (e.g. `"eng"` ->
    /// `"English"`), or `None` when the code is unknown (C#
    /// `GetLanguageDisplayName` returns `null`).
    fn get_language_display_name(&self, language: &str) -> Option<String>;
}

impl<L: LocalizationManager + ?Sized> LocalizationManager for Box<L> {
    fn get_localized_string(&self, phrase: &str) -> String {
        (**self).get_localized_string(phrase)
    }

    fn get_language_display_name(&self, language: &str) -> Option<String> {
        (**self).get_language_display_name(language)
    }
}

/// A passthrough [`LocalizationManager`] for tests and unwired encoders.
///
/// Phrase keys echo back unchanged; language display names are `None`, matching
/// the default `Mock<ILocalizationManager>` the upstream
/// `ProbeResultNormalizerTests` inject (its `GetLanguageDisplayName` yields
/// `null`).
#[derive(Debug, Clone, Copy, Default)]
pub struct PassthroughLocalization;

impl LocalizationManager for PassthroughLocalization {
    fn get_localized_string(&self, phrase: &str) -> String {
        phrase.to_owned()
    }

    fn get_language_display_name(&self, _language: &str) -> Option<String> {
        None
    }
}

/// Adapts the server's [`ferrofin_traits::localization::LocalizationManager`]
/// to the probe seam, so un-persisted probe output (Live TV, opened live
/// streams) carries the same localized labels Jellyfin's production
/// normalizer stamps.
#[derive(Clone)]
pub struct TraitLocalization(pub Arc<dyn ferrofin_traits::localization::LocalizationManager>);

impl std::fmt::Debug for TraitLocalization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraitLocalization").finish_non_exhaustive()
    }
}

impl LocalizationManager for TraitLocalization {
    fn get_localized_string(&self, phrase: &str) -> String {
        self.0.get_localized_string(phrase)
    }

    fn get_language_display_name(&self, language: &str) -> Option<String> {
        self.0.get_language_display_name(language)
    }
}
