//! Port of `Emby.Naming.Video.ExtraRule`.

use ferrofin_model::entities::ExtraType;

use crate::common::MediaType;
use crate::video::ExtraRuleType;

/// A rule used to match a file path with an [`ExtraType`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraRule {
    /// The token to use for matching against the file path.
    pub token: String,
    /// The type of the extra to return when matched.
    pub extra_type: ExtraType,
    /// The type of the rule.
    pub rule_type: ExtraRuleType,
    /// The type of the media to return when matched.
    pub media_type: MediaType,
}

impl ExtraRule {
    /// Creates a new [`ExtraRule`].
    #[must_use]
    pub fn new(
        extra_type: ExtraType,
        rule_type: ExtraRuleType,
        token: impl Into<String>,
        media_type: MediaType,
    ) -> Self {
        Self {
            token: token.into(),
            extra_type,
            rule_type,
            media_type,
        }
    }
}
