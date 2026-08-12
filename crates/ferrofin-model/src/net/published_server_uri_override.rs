//! Port of `MediaBrowser.Model.Net.PublishedServerUriOverride`.

use super::ip_data::IpData;

/// Holds information for a published server URI override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedServerUriOverride {
    /// The object's IP data.
    pub data: IpData,
    /// The override URI.
    pub override_uri: String,
    /// Whether the override applies to internal requests.
    pub is_internal_override: bool,
    /// Whether the override applies to external requests.
    pub is_external_override: bool,
}

impl PublishedServerUriOverride {
    /// Creates a new [`PublishedServerUriOverride`].
    #[must_use]
    pub fn new(
        data: IpData,
        override_uri: impl Into<String>,
        internal_override: bool,
        external_override: bool,
    ) -> Self {
        Self {
            data,
            override_uri: override_uri.into(),
            is_internal_override: internal_override,
            is_external_override: external_override,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    #[test]
    fn new_populates_fields() {
        let data = IpData::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), None, "eth0");
        let value =
            PublishedServerUriOverride::new(data.clone(), "https://media.example.com", true, false);
        assert_eq!(value.data, data);
        assert_eq!(value.override_uri, "https://media.example.com");
        assert!(value.is_internal_override);
        assert!(!value.is_external_override);
    }

    #[test]
    fn clone_equals_original() {
        let data = IpData::new(IpAddr::V4(Ipv4Addr::LOCALHOST), None, "lo");
        let value = PublishedServerUriOverride::new(data, "http://localhost:8096", false, true);
        assert_eq!(value.clone(), value);
    }
}
