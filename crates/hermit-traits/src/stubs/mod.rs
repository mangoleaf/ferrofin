//! Minimal manager traits for **deferred** subsystems.
//!
//! Live TV, channels, SyncPlay, plugins and lyrics are not part of the Hermit
//! v1 feature set. Rather than port their large C# interface surfaces (and the
//! per-strategy sub-interfaces behind them), each gets a single, minimal
//! manager trait here so the DI seam and `AppState` can name a
//! `Arc<dyn _Manager>` and be satisfied later by a disabled/stub implementation
//! in `hermit-core`.
//!
//! Each trait ports only a representative slice of its C# interface — enough to
//! establish the seam and exercise object safety — and omits the sub-strategy
//! interfaces (`ILiveTvService`, `IChannel`, `IGroupPlaybackRequest`,
//! `ILyricProvider`, …) entirely, per the port plan's SKIP list.
//!
//! Every trait is object-safe and carries a `_assert_object_safe_*` assertion.

pub mod channels;
pub mod live_tv;
pub mod lyrics;
pub mod plugins;
pub mod sync_play;

pub use channels::ChannelManager;
pub use live_tv::LiveTvManager;
pub use lyrics::LyricManager;
pub use plugins::PluginManager;
pub use sync_play::SyncPlayManager;
