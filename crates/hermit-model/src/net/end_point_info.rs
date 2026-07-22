//! Port of `MediaBrowser.Model.Net.EndPointInfo`.

/// Information about a request endpoint's network position.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EndPointInfo {
    /// Whether the endpoint is on the local machine.
    pub is_local: bool,
    /// Whether the endpoint is within the configured network.
    pub is_in_network: bool,
}
