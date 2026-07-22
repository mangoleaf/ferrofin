//! Port of `Emby.Naming.Video.ExtraResult`.

use hermit_model::entities::ExtraType;

use crate::video::ExtraRule;

/// Holder object for passing results from the extra resolver.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtraResult {
    /// The type of the extra.
    pub extra_type: Option<ExtraType>,
    /// The rule that matched.
    pub rule: Option<ExtraRule>,
}
