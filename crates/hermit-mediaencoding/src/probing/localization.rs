//! Localization abstraction consumed by the probe normalizer.
//!
//! Upstream `ProbeResultNormalizer` takes an `ILocalizationManager` to resolve
//! the localized "Default"/"External"/… labels and language display names it
//! stamps onto each [`hermit_model::entities_media::MediaStream`]. That lookup
//! is pure (no I/O), so it is modeled as a small trait here; tests inject a
//! passthrough implementation exactly as the C# tests inject a `Mock`.

/// Resolves localized display strings for probe output.
pub trait LocalizationManager {
    /// Returns the localized string for the given phrase key, or the key
    /// itself when no translation is available.
    fn get_localized_string(&self, phrase: &str) -> String;

    /// Returns the display name for a language code (e.g. `"eng"` ->
    /// `"English"`), or the code itself when unknown.
    fn get_language_display_name(&self, language: &str) -> String;
}

/// A passthrough [`LocalizationManager`] that echoes its inputs.
///
/// Matches the behaviour of the mocked localization manager used throughout the
/// upstream `ProbeResultNormalizerTests` (return the input string unchanged).
#[derive(Debug, Clone, Copy, Default)]
pub struct PassthroughLocalization;

impl LocalizationManager for PassthroughLocalization {
    fn get_localized_string(&self, phrase: &str) -> String {
        phrase.to_owned()
    }

    fn get_language_display_name(&self, language: &str) -> String {
        language.to_owned()
    }
}
