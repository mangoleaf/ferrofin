//! The HDHomeRun tuner host.
//!
//! Port of `src/Jellyfin.LiveTv/TunerHosts/HdHomerun/` (v10.11.8, 8 files):
//! the [device JSON](types), the [tuner host](host) itself, the
//! [TCP control protocol](manager) a legacy device is tuned over and the
//! [UDP stream](udp_stream) that device sends back.
//!
//! **Verification status.** No physical HDHomeRun was available when this was
//! ported. The HTTP surface (`discover.json`, `lineup.json`, the per-channel
//! MPEG-TS URL) is verified against upstream's own JSON fixtures and, in the
//! parity lab, against a faithful fake device (`suite/perf/hdhomerun-source.py`)
//! that both Ferrofin and Jellyfin consume. The legacy UDP control path is
//! verified at the byte/CRC level — which is the same and only level upstream
//! verifies it at (`tests/Jellyfin.LiveTv.Tests/HdHomerunManagerTests.cs` is 11
//! pure framing facts and no socket). A real device is still needed to confirm
//! `HdHomerunUdpStream` end to end.

#[cfg(test)]
pub mod fake;
pub mod host;
pub mod manager;
pub mod types;
pub mod udp_stream;

pub use host::{HDHR_CHANNEL_ID_PREFIX, HdHomerunHost, LegacyUdpPlan, is_legacy_tuner};
pub use manager::{HdHomerunSession, channel_commands, legacy_channel_commands};
pub use types::{DiscoverResponse, LineupChannel};
