//! Minimal manager traits for **deferred** subsystems.
//!
//! Live TV, channels, SyncPlay, plugins and lyrics are not part of the Ferrofin
//! v1 feature set. Rather than port their large C# interface surfaces (and the
//! per-strategy sub-interfaces behind them), each gets a single, minimal
//! manager trait here so the DI seam and `AppState` can name a
//! `Arc<dyn _Manager>` and be satisfied later by a disabled/stub implementation
//! in `ferrofin-core`.
//!
//! Each trait ports only a representative slice of its C# interface — enough to
//! establish the seam and exercise object safety — and omits the sub-strategy
//! interfaces (`ILiveTvService`, `IChannel`, `IGroupPlaybackRequest`, …)
//! entirely, per the port plan's SKIP list. Lyrics have since grown real: the
//! [`lyrics`] module now also carries the ported `ILyricProvider` strategy
//! seam ([`LyricProvider`]) backing remote lyric search/download.
//!
//! Every trait is object-safe and carries a `_assert_object_safe_*` assertion.

pub mod channels;
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
pub use live_tv::LiveTvManager;
pub use lyrics::{LyricManager, LyricProvider, LyricResponse, RemoteLyricInfo};
pub use sync_play::{PlaybackRequest, SyncPlayManager, SyncPlaySession};
pub use virtual_folders::DisabledVirtualFolderManager;
