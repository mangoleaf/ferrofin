//! Port of `Emby.Naming.Video.Format3DRule`.

/// Data holder class for a 3D format rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format3DRule {
    /// The token identifying the 3D format (e.g. `hsbs`).
    pub token: String,
    /// An optional token that must precede [`Self::token`] (e.g. `3d`).
    pub preceding_token: Option<String>,
}

impl Format3DRule {
    /// Creates a new [`Format3DRule`].
    #[must_use]
    pub fn new(token: impl Into<String>, preceding_token: Option<String>) -> Self {
        Self {
            token: token.into(),
            preceding_token,
        }
    }
}
