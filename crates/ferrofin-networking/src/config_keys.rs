//! The startup configuration key the network manager reads.
//!
//! This is the only entry the networking port consumes from the
//! `Microsoft.Extensions.Configuration` startup bag, inlined here (per the port
//! charter) rather than pulling in the whole
//! `MediaBrowser.Controller.Extensions.ConfigurationExtensions` helper.

/// Startup key for a hard-coded published-server URL override
/// (`ConfigurationExtensions.AddressOverrideKey`).
pub const ADDRESS_OVERRIDE_KEY: &str = "PublishedServerUrl";
