//! HLS playlist generation for Ferrofin — port of `Jellyfin.MediaEncoding.Hls`.
//!
//! Builds `.m3u8` playlists on top of `ferrofin-mediaencoding` and
//! `ferrofin-keyframes`. Mirrors the C# namespace layout:
//! - [`create_main_playlist_request`] — `Playlist.CreateMainPlaylistRequest`.
//! - [`dynamic_hls_playlist_generator`] — `Playlist.DynamicHlsPlaylistGenerator`
//!   (+ `IDynamicHlsPlaylistGenerator`), including the parity-core timing
//!   helpers ([`compute_segments`], [`compute_equal_length_segments`],
//!   [`is_extraction_allowed_for_file`]).
//! - [`hls_codec_strings`] — `Jellyfin.Api.Helpers.HlsCodecStringHelpers`, the
//!   RFC 6381 `CODECS` strings the master playlist advertises.

pub mod create_main_playlist_request;
pub mod dynamic_hls_playlist_generator;
pub mod error;
pub mod hls_codec_strings;
pub mod hls_stream_manager;

pub use create_main_playlist_request::CreateMainPlaylistRequest;
pub use dynamic_hls_playlist_generator::{
    DynamicHlsPlaylistGenerator, EncodingOptionsProvider, KeyframeExtractor, TICKS_PER_MILLISECOND,
    TICKS_PER_SECOND, compute_equal_length_segments, compute_segments,
    is_extraction_allowed_for_file,
};
pub use error::HlsError;
pub use hls_stream_manager::{HlsStreamManagerImpl, StreamStatePlanner, TranscodePlan};
