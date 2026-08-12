//! Live TV for Ferrofin — M3U tuner + XMLTV guide.
//!
//! [`FerrofinLiveTvManager`] stores tuner-host/listing-provider configuration and
//! caches the channel lineup and EPG in SQLite. A refresh fetches each source
//! (via a [`SourceFetcher`]), parses it ([`m3u`]/[`xmltv`]) and rewrites the
//! cache; channels and programmes are then surfaced to clients as `BaseItemDto`s.

pub mod error;
pub mod fetch;
pub mod m3u;
pub mod manager;
pub mod xmltv;

pub use error::LiveTvError;
pub use fetch::{ReqwestFetcher, SourceFetcher};
pub use manager::FerrofinLiveTvManager;
