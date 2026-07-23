//! HLS playlist generation for Hermit — port of `Jellyfin.MediaEncoding.Hls`.
//!
//! Builds `.m3u8` playlists on top of `hermit-mediaencoding` and
//! `hermit-keyframes`. Mirrors the C# namespace layout:
//! - [`create_main_playlist_request`] — `Playlist.CreateMainPlaylistRequest`.
//! - [`dynamic_hls_playlist_generator`] — `Playlist.DynamicHlsPlaylistGenerator`
//!   (+ `IDynamicHlsPlaylistGenerator`), including the parity-core timing
//!   helpers ([`compute_segments`], [`compute_equal_length_segments`],
//!   [`is_extraction_allowed_for_file`]).
//!
//! See `brain/PLAN_HERMIT_PORT.md`.

pub mod create_main_playlist_request;
pub mod dynamic_hls_playlist_generator;
pub mod error;
pub mod hls_stream_manager;

pub use create_main_playlist_request::CreateMainPlaylistRequest;
pub use dynamic_hls_playlist_generator::{
    DynamicHlsPlaylistGenerator, EncodingOptionsProvider, KeyframeExtractor, TICKS_PER_MILLISECOND,
    TICKS_PER_SECOND, compute_equal_length_segments, compute_segments,
    is_extraction_allowed_for_file,
};
pub use error::HlsError;
pub use hls_stream_manager::{HlsStreamManagerImpl, StreamStatePlanner, TranscodePlan};
