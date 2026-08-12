//! Port of `MediaBrowser.Common.Net.RemoteAccessPolicyResult`.

/// Result of [`crate::manager::NetworkManager::should_allow_server_access`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteAccessPolicyResult {
    /// The connection should be allowed.
    Allow,

    /// The connection should be rejected since it is not from a local IP and
    /// remote access is disabled.
    RejectDueToRemoteAccessDisabled,

    /// The connection should be rejected since it is from a blocklisted IP.
    RejectDueToIpBlocklist,

    /// The connection should be rejected since it is from a remote IP that is
    /// not in the allowlist.
    RejectDueToNotAllowlistedRemoteIp,
}
