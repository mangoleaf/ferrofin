//! Subtitle parsing, conversion, and extraction.
//!
//! Hand-port of `MediaBrowser.MediaEncoding.Subtitles`. The `libse`
//! (`Nikse.SubtitleEdit`) library it depends on has no Rust equivalent, so the
//! SubRip/SSA/ASS parsers and the WebVTT/JSON writers are ported directly onto a
//! small in-memory [`model`] (`Subtitle`/`Paragraph`/`TimeCode`).
//!
//! - [`parser`] — the extension-keyed [`SubtitleEditParser`] ([`SubtitleParser`]).
//! - [`srt`] / [`ssa`] / [`vtt`] / [`json_writer`] — the format load/save code.
//! - [`encoder`] — [`SubtitleEncoder`], with charset-detect → UTF-8 conversion
//!   (via `encoding_rs` + `chardetng`, replacing C# `UtfUnknown`), deterministic
//!   `ConvertSubtitles` (keyed `tokio::Mutex`, replacing `AsyncKeyedLock`), and
//!   the ffmpeg-backed extraction behind the [`SubtitleIo`] seam.
//! - [`service`] — [`SubtitleEncoderImpl`], the object-safe `ferrofin-traits`
//!   [`SubtitleEncoder`](ferrofin_traits::media_encoding::SubtitleEncoder) adapter
//!   that resolves an item's media source and delegates to [`encoder`].

pub mod encoder;
pub mod json_writer;
pub mod model;
pub mod parser;
pub mod service;
pub mod srt;
pub mod ssa;
pub mod vtt;

pub use encoder::{NoopSubtitleIo, SubtitleEncoder, SubtitleInfo, SubtitleIo, SubtitleStream};
pub use model::{Paragraph, Subtitle, TimeCode};
pub use parser::{SubtitleEditParser, SubtitleParser};
pub use service::SubtitleEncoderImpl;
