//! Keyframe extraction for Hermit — port of Jellyfin's `Jellyfin.MediaEncoding.Keyframes`.
//!
//! Mirrors the C# namespace layout:
//! - [`keyframe_data`] — `KeyframeData` (root namespace).
//! - [`ff_probe`] — `FfProbe.FfProbeKeyframeExtractor`.
//! - [`ff_tool`] — `FfTool.FfToolKeyframeExtractor` (unimplemented upstream stub).
//!
//! The Matroska extractor is deferred.

pub mod error;
pub mod ff_probe;
pub mod ff_tool;
pub mod keyframe_data;
