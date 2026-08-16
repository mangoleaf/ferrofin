//! [`FfmpegMediaExtractor`] — the ffmpeg-backed [`MediaExtractor`], feeding
//! the WASM plugin analysis capabilities (`extract-audio`/`extract-frames`).
//!
//! The whole ffmpeg invocation is owned here: fixed argument shapes, only
//! (path, window, clamped spec) vary. Callers (the wasm capability layer)
//! enforce the plugin-facing caps; this type enforces process hygiene —
//! stdout capped reads, stderr surfaced on failure, no shell.

use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncReadExt as _;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_analysis::{AudioSpec, ExtractedFrame, MediaExtractor};

/// Hard byte ceiling on any single extraction read — a backstop above the
/// caller's own caps (60 s of 48 kHz stereo s16 ≈ 11.5 MiB; frames are KBs).
const EXTRACT_OUTPUT_MAX: usize = 64 * 1024 * 1024;

/// Wall-clock bound on one extraction: a ≤60 s window decodes in seconds
/// on anything healthy, so a minute means a wedged decoder or a stalled
/// mount (see the NFS restart-storm history) — kill it and fail the call
/// rather than holding the plugin thread and the global analysis permit.
const EXTRACT_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);

/// The ffmpeg-backed extractor.
#[derive(Debug, Clone)]
pub struct FfmpegMediaExtractor {
    ffmpeg: String,
    timeout: std::time::Duration,
}

impl FfmpegMediaExtractor {
    /// Builds the extractor over the discovered `ffmpeg` binary path.
    #[must_use]
    pub fn new(ffmpeg: impl Into<String>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            timeout: EXTRACT_TIMEOUT,
        }
    }

    /// Overrides the per-invocation timeout — a test seam.
    #[doc(hidden)]
    #[must_use]
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs one fixed-shape ffmpeg invocation, returning capped stdout.
    /// stdout and stderr are drained CONCURRENTLY (a full stderr pipe would
    /// otherwise deadlock the child against our stdout read), and the whole
    /// invocation sits under a wall-clock timeout with a kill.
    async fn run(&self, args: &[String]) -> Result<Vec<u8>, ServiceError> {
        let mut child = tokio::process::Command::new(&self.ffmpeg)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ServiceError::backend(format!("spawning ffmpeg: {e}")))?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");

        let drained = async {
            let stdout_read = async {
                let mut out = Vec::new();
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = stdout
                        .read(&mut buf)
                        .await
                        .map_err(|e| format!("reading ffmpeg output: {e}"))?;
                    if n == 0 {
                        return Ok::<Vec<u8>, String>(out);
                    }
                    if out.len() + n > EXTRACT_OUTPUT_MAX {
                        return Err("extraction output exceeds the hard ceiling".to_owned());
                    }
                    out.extend_from_slice(&buf[..n]);
                }
            };
            let stderr_read = async {
                let mut err = String::new();
                let _ = stderr.read_to_string(&mut err).await;
                err
            };
            let (out, err_text) = tokio::join!(stdout_read, stderr_read);
            let status = child
                .wait()
                .await
                .map_err(|e| format!("waiting for ffmpeg: {e}"))?;
            let out = out?;
            if !status.success() {
                let tail: String = err_text
                    .lines()
                    .rev()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" | ");
                return Err(format!("ffmpeg extraction failed ({status}): {tail}"));
            }
            Ok(out)
        };
        match tokio::time::timeout(self.timeout, drained).await {
            Ok(result) => result.map_err(ServiceError::backend),
            // kill_on_drop reaps the child when the timed-out future drops.
            Err(_) => Err(ServiceError::backend(format!(
                "ffmpeg extraction timed out after {:?} (stalled decoder or mount)",
                self.timeout
            ))),
        }
    }
}

#[async_trait]
impl MediaExtractor for FfmpegMediaExtractor {
    async fn extract_audio(
        &self,
        path: &str,
        start_seconds: f64,
        duration_seconds: f64,
        spec: AudioSpec,
    ) -> Result<Vec<i16>, ServiceError> {
        let args: Vec<String> = vec![
            "-v".into(),
            "error".into(),
            "-ss".into(),
            format!("{start_seconds:.3}"),
            "-t".into(),
            format!("{duration_seconds:.3}"),
            "-i".into(),
            path.into(),
            "-vn".into(),
            "-map".into(),
            "0:a:0".into(),
            "-ac".into(),
            spec.channels.to_string(),
            "-ar".into(),
            spec.sample_rate.to_string(),
            "-f".into(),
            "s16le".into(),
            "-".into(),
        ];
        let bytes = self.run(&args).await?;
        // Little-endian s16 pairs → samples (a trailing odd byte is decoder
        // noise; drop it rather than failing the window).
        Ok(bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect())
    }

    async fn extract_frames(
        &self,
        path: &str,
        timestamps_seconds: &[f64],
        max_dimension: u32,
        jpeg: bool,
    ) -> Result<Vec<ExtractedFrame>, ServiceError> {
        let mut frames = Vec::with_capacity(timestamps_seconds.len());
        for &ts in timestamps_seconds {
            let (vf, format_args): (String, Vec<String>) = if jpeg {
                (
                    // Fit inside max_dimension, aspect preserved, even dims.
                    format!(
                        "scale='min({max_dimension},iw)':'min({max_dimension},ih)':force_original_aspect_ratio=decrease:force_divisible_by=2"
                    ),
                    vec!["-f".into(), "mjpeg".into()],
                )
            } else {
                (
                    // Analysis grayscale: exactly N×N (aspect NOT preserved
                    // — fixed dims are what make the raw bytes addressable).
                    format!("scale={max_dimension}:{max_dimension},format=gray"),
                    vec![
                        "-f".into(),
                        "rawvideo".into(),
                        "-pix_fmt".into(),
                        "gray".into(),
                    ],
                )
            };
            let mut args: Vec<String> = vec![
                "-v".into(),
                "error".into(),
                "-ss".into(),
                format!("{ts:.3}"),
                "-i".into(),
                path.into(),
                "-frames:v".into(),
                "1".into(),
                "-vf".into(),
                vf,
            ];
            args.extend(format_args);
            args.push("-".into());
            let data = self.run(&args).await?;
            if data.is_empty() {
                // Past end-of-stream: skip rather than fail the batch.
                continue;
            }
            let (width, height) = if jpeg {
                (0, 0) // dimensions live in the JPEG itself
            } else {
                (max_dimension, max_dimension)
            };
            frames.push(ExtractedFrame {
                seconds: ts,
                width,
                height,
                jpeg,
                data,
            });
        }
        Ok(frames)
    }
}
