//! Manager traits for Live TV, SyncPlay, lyrics and channels, and the disabled
//! or no-op implementations `AppState` starts with.
//!
//! The module name is historical: these seams were first ported as minimal
//! stubs. The traits now back real subsystems:
//! - [`LiveTvManager`] — the tuners, guide and DVR (`ferrofin-livetv`);
//! - [`SyncPlayManager`] — synchronized group playback (`ferrofin-core`);
//! - [`LyricManager`] and the [`LyricProvider`] strategy — local and remote
//!   lyrics (`ferrofin-core`);
//! - [`ChannelManager`] — channels. No channel backend (`IChannel`) can be
//!   registered, so every channel query is empty. TODO(parity, gl-298): open
//!   work item: give channels an extension seam.
//!
//! The rest are defaults for an `AppState` built without the real subsystem
//! (test constructors, a host without ffmpeg): [`NoopLibraryMonitor`],
//! [`DisabledVirtualFolderManager`], and the disabled HLS stream manager,
//! subtitle encoder and attachment extractor in [`hls_stream`]. The composition
//! root replaces each with the real one. The chapter-less [`chapters`] manager
//! is for extension contexts built without the database (extension tests).
//!
//! Every trait is object-safe and carries a `_assert_object_safe_*` assertion.

pub mod channels;
pub mod chapters;
pub mod hls_stream;
pub mod library_monitor;
pub mod live_tv;
pub mod lyrics;
pub mod sync_play;
pub mod virtual_folders;

pub use channels::ChannelManager;
pub use hls_stream::{
    DisabledAttachmentExtractor, DisabledHlsStreamManager, DisabledSubtitleEncoder,
};
pub use library_monitor::NoopLibraryMonitor;
pub use live_tv::{
    LIVE_TV_SERVICE_NAME, LiveStreamFile, LiveTvChannelQuery, LiveTvManager, RecordingVisibility,
    TAIL_SEEK_AFTER_SECONDS, TAIL_SEEK_BYTES, filter_timers, internal_series_timer_id,
    internal_timer_id, new_timer_defaults,
};
pub use lyrics::{LyricManager, LyricProvider, LyricResponse, RemoteLyricInfo};
pub use sync_play::{PlaybackRequest, SyncPlayManager, SyncPlayNowPlaying, SyncPlaySession};
pub use virtual_folders::DisabledVirtualFolderManager;
