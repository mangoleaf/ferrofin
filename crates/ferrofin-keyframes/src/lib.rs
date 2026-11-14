//! Keyframe extraction for Ferrofin — port of Jellyfin's `Jellyfin.MediaEncoding.Keyframes`.
//!
//! Mirrors the C# namespace layout:
//! - [`keyframe_data`] — `KeyframeData` (root namespace).
//! - [`ff_probe`] — `FfProbe.FfProbeKeyframeExtractor`.
//! - [`matroska`] — `Matroska.MatroskaKeyframeExtractor`, which reads a Matroska
//!   file's cue index.
//!
//! `FfTool.FfToolKeyframeExtractor` is not ported: upstream's version only throws
//! `NotImplementedException`, and nothing calls it.

pub mod error;
pub mod ff_probe;
pub mod keyframe_data;
pub mod matroska;
