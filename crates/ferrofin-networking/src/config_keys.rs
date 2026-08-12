//! Startup configuration keys used by the network manager.
//!
//! These are the only two entries the networking port consumes from the
//! `Microsoft.Extensions.Configuration` startup bag, inlined here (per the port
//! charter) rather than pulling in the whole
//! `MediaBrowser.Controller.Extensions.ConfigurationExtensions` helper.

/// Startup key for a hard-coded published-server URL override
/// (`ConfigurationExtensions.AddressOverrideKey`).
pub const ADDRESS_OVERRIDE_KEY: &str = "PublishedServerUrl";

/// Startup key that toggles OS network-change detection
/// (`ConfigurationExtensions.DetectNetworkChangeKey`).
///
/// The change-detection machinery itself is deferred (OS eventing), but the key
/// is preserved so a startup bag carrying it parses without surprises.
pub const DETECT_NETWORK_CHANGE_KEY: &str = "DetectNetworkChange";
