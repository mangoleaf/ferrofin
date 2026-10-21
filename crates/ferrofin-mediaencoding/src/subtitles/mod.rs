//! Subtitle parsing, conversion, and extraction.
//!
//! Hand-port of `MediaBrowser.MediaEncoding.Subtitles`. The libse 4.0.12
//! (`Nikse.SubtitleEdit`) library it depends on has no Rust equivalent, so the
//! SubRip/SSA/ASS loaders and the writers Jellyfin 12's `SubtitleEncoder.TryGetWriter`
//! returns (libse's SubRip, WebVTT, SSA, ASS and TTML `ToText`, plus Jellyfin's own
//! JSON writer) are ported directly onto a small in-memory [`model`]
//! (`Subtitle`/`Paragraph`/`TimeCode`).
//!
//! - [`parser`] — the extension-keyed [`SubtitleEditParser`] ([`SubtitleParser`]).
//! - [`srt`] / [`ssa`] / [`vtt`] / [`ttml`] / [`json_writer`] — the format load/save code, with
//!   [`ssa_tags`] (SSA/ASS override tags to and from HTML) and [`ssa_style`] (styles
//!   read from a script header).
//! - [`dotnet`] / [`libse_text`] — the .NET runtime and libse helper behaviour the
//!   writers' bytes depend on (colours, `decimal`, number formatting, line handling).
//! - [`encoder`] — [`SubtitleEncoder`], with charset-detect → UTF-8 conversion
//!   (via `encoding_rs` + `chardetng`, replacing C# `UtfUnknown`), deterministic
//!   `ConvertSubtitles` (keyed `tokio::Mutex`, replacing `AsyncKeyedLock`), and
//!   the ffmpeg-backed extraction behind the [`SubtitleIo`] seam.
//! - [`service`] — [`SubtitleEncoderImpl`], the object-safe `ferrofin-traits`
//!   [`SubtitleEncoder`](ferrofin_traits::media_encoding::SubtitleEncoder) adapter
//!   that resolves an item's media source and delegates to [`encoder`].

pub mod dotnet;
pub mod encoder;
pub mod json_writer;
pub mod libse_text;
pub mod model;
pub mod parser;
pub mod service;
pub mod srt;
pub mod ssa;
pub mod ssa_style;
pub mod ssa_tags;
pub mod ttml;
pub mod vtt;

pub use encoder::{
    NoopSubtitleIo, SubtitleEncoder, SubtitleInfo, SubtitleIo, SubtitleStream,
    subtitle_extraction_timeout,
};
pub use model::{Paragraph, Subtitle, TimeCode};
pub use parser::{SubtitleEditParser, SubtitleParser};
pub use service::SubtitleEncoderImpl;
