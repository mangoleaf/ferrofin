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
/// Jellyfin defaults it to `True` (`ConfigurationOptions.DefaultConfiguration`)
/// and `--nonetchange` sets it to `False`. This is the upstream key's name
/// only: Ferrofin's bootstrap setting `detect_network_change`
/// (`FERROFIN_DETECT_NETWORK_CHANGE`, also `true` by default) is what starts
/// [`crate::network_change::monitor`].
pub const DETECT_NETWORK_CHANGE_KEY: &str = "DetectNetworkChange";
