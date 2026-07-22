//! Port of `Emby.Naming.Video.StubTypeRule`.

/// Data class holding information about a stub type rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StubTypeRule {
    /// The token to match against.
    pub token: String,
    /// The resolved stub type when the token matches.
    pub stub_type: String,
}

impl StubTypeRule {
    /// Creates a new [`StubTypeRule`].
    #[must_use]
    pub fn new(token: impl Into<String>, stub_type: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            stub_type: stub_type.into(),
        }
    }
}
