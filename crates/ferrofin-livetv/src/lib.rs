//! Live TV for Ferrofin — M3U tuner + XMLTV guide.
//!
//! [`FerrofinLiveTvManager`] stores tuner-host/listing-provider configuration and
//! caches the channel lineup and EPG in SQLite. A refresh fetches each source
//! (via a [`SourceFetcher`]), parses it ([`m3u`]/[`xmltv`]) and rewrites the
//! cache; channels and programmes are then surfaced to clients as `BaseItemDto`s.
//!
//! Playing a channel goes through the [`stream`] engine: the tuner's HTTP stream
//! is opened once, copied into a temp `.ts` file, and served to every consumer
//! from `/LiveTv/LiveStreamFiles/{uniqueId}/stream.ts` — so one tuner connection
//! feeds several viewers plus the transcoder.
//!
//! [`SchedulesDirect`] serves the account-less Schedules Direct country list
//! behind Jellyfin's memory + on-disk cache.

pub mod dvr;
pub mod dvr_repository;
pub mod error;
pub mod fetch;
pub mod guide_repository;
pub mod m3u;
pub mod manager;
pub mod projection;
pub mod schedules_direct;
pub mod stream;
pub mod xmltv;

pub use dvr::{ActiveRecording, RecorderKind, RecordingInput, TimerRecordingInfo};
pub use error::LiveTvError;
pub use fetch::{ReqwestFetcher, SourceFetcher};
pub use manager::{FerrofinLiveTvManager, LiveTvPaths};
pub use schedules_direct::SchedulesDirect;
pub use stream::{ReqwestTunerSource, TunerStreamBody, TunerStreamSource};
