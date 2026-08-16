//! The media-analysis extraction seam — bounded, host-mediated access to
//! decoded media data, backing the WASM plugin capabilities
//! `extract-audio` / `extract-frames`.
//!
//! Design rule (docs/EXTENSIONS.md): **the host decodes, the guest
//! analyzes**. Callers hand this seam a *resolved media path* plus a
//! bounded window; the implementation owns the decoder invocation
//! entirely — no decoder arguments ever originate from a plugin.

use async_trait::async_trait;

use crate::error::ServiceError;

/// Decoded-audio parameters (already clamped by the caller).
#[derive(Debug, Clone, Copy)]
pub struct AudioSpec {
    /// Target sample rate in Hz.
    pub sample_rate: u32,
    /// 1 (mono) or 2 (interleaved stereo).
    pub channels: u8,
}

/// One extracted still frame.
#[derive(Debug, Clone)]
pub struct ExtractedFrame {
    /// The sampled instant, in seconds.
    pub seconds: f64,
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Whether `data` is JPEG (else raw 8-bit grayscale, width×height).
    pub jpeg: bool,
    /// The encoded bytes.
    pub data: Vec<u8>,
}

/// Bounded decoding of media files for analysis.
#[async_trait]
pub trait MediaExtractor: Send + Sync {
    /// Decodes `[start, start+duration)` seconds of `path` into interleaved
    /// signed-16 PCM at `spec`.
    ///
    /// # Errors
    /// Spawn/decode failures, or an output exceeding the caller's byte cap.
    async fn extract_audio(
        &self,
        path: &str,
        start_seconds: f64,
        duration_seconds: f64,
        spec: AudioSpec,
    ) -> Result<Vec<i16>, ServiceError>;

    /// Samples one still per timestamp from `path`. `jpeg` selects JPEG
    /// (aspect-preserving fit into `max_dimension`) over raw grayscale
    /// (exactly `max_dimension`² — analysis frames, not thumbnails).
    ///
    /// # Errors
    /// Spawn/decode failures.
    async fn extract_frames(
        &self,
        path: &str,
        timestamps_seconds: &[f64],
        max_dimension: u32,
        jpeg: bool,
    ) -> Result<Vec<ExtractedFrame>, ServiceError>;
}

fn _assert_object_safe_media_extractor(_: &dyn MediaExtractor) {}
