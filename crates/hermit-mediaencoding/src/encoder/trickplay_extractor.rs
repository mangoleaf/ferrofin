//! [`TrickplayFrameExtractorImpl`] — the concrete
//! [`TrickplayFrameExtractor`] over the [`Transcoder`] process seam.
//!
//! Port of the **software** path of
//! `MediaEncoder.ExtractVideoImagesOnIntervalAccelerated` /
//! `ExtractVideoImagesOnIntervalInternal`: a single ffmpeg run with an
//! `fps=1/interval` sampling filter plus the width-bounded, DAR-preserving
//! `scale` expression, `-c:v mjpeg -qscale:v …`, writing `%08d.jpg` frames into
//! a caller-supplied directory.
//!
//! Departures from the C# (documented per the port rules):
//! - The hardware-acceleration / keyframe-only branches ride the deferred
//!   hw-accel matrix (see the crate docs) and are not ported; the software
//!   encoder is always used.
//! - The `setpts=N/frame_rate/TB` PTS normalisation the C# splices in front of
//!   the `fps` filter guards against containers with broken timestamps and
//!   needs the probed input frame rate, which this seam does not carry; ffmpeg's
//!   `fps` filter handles well-formed inputs without it.
//! - The C# creates the temp output directory itself; here the caller owns the
//!   output directory (the trickplay manager passes a temp dir and cleans it
//!   up), so the extractor only creates and fills it.

use std::path::Path;
use std::sync::Arc;

use crate::error::MediaEncodingError;
use async_trait::async_trait;
use hermit_traits::error::ServiceError;
use hermit_traits::media_encoding::TrickplayFrameExtractor;

use super::Transcoder;

/// The concrete trickplay frame extractor: builds the ffmpeg argument line and
/// runs it through the [`Transcoder`] seam (a real spawn in production, a fake
/// in unit tests).
pub struct TrickplayFrameExtractorImpl<T: Transcoder> {
    transcoder: Arc<T>,
    ffmpeg_path: String,
}

impl<T: Transcoder> std::fmt::Debug for TrickplayFrameExtractorImpl<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrickplayFrameExtractorImpl")
            .field("ffmpeg_path", &self.ffmpeg_path)
            .finish_non_exhaustive()
    }
}

impl<T: Transcoder> TrickplayFrameExtractorImpl<T> {
    /// Creates an extractor spawning `ffmpeg_path` through `transcoder`.
    pub fn new(transcoder: Arc<T>, ffmpeg_path: impl Into<String>) -> Self {
        Self {
            transcoder,
            ffmpeg_path: ffmpeg_path.into(),
        }
    }
}

/// Builds the ffmpeg argument line for one trickplay frame-extraction run.
///
/// Mirrors the software branch of the C# `ExtractVideoImagesOnIntervalInternal`
/// format string: `-loglevel error {input} -an -sn {filter} -threads {t}
/// -c:v mjpeg -qscale:v {q} -vsync 0 -f image2 "{out}"`. The `fps` rate is the
/// exact rational `1000/interval_ms`; the `scale` expression is Jellyfin's
/// width-bounded software scaler (`trunc(min(max(iw,ih*dar),W)/2)*2` by
/// `trunc(ow/dar/2)*2`), which keeps both dimensions even and honours
/// anamorphic sources.
#[must_use]
pub fn build_trickplay_args(
    input_path: &str,
    interval_ms: i32,
    max_width: i32,
    qscale: i32,
    threads: i32,
    output_pattern: &str,
) -> String {
    // ffmpeg qscale is 1 (best) – 31 (worst); C# clamps the configured value.
    let qscale = qscale.clamp(1, 31);
    format!(
        "-loglevel error -threads {threads} -i file:\"{input_path}\" -an -sn \
         -vf \"fps=1000/{interval_ms},scale=trunc(min(max(iw\\,ih*dar)\\,{max_width})/2)*2:trunc(ow/dar/2)*2\" \
         -threads {threads} -c:v mjpeg -qscale:v {qscale} -vsync 0 -f image2 \"{output_pattern}\""
    )
}

/// Lists the `.jpg` files directly inside `dir`, sorted by file name.
fn jpg_files_sorted(dir: &Path) -> Result<Vec<String>, ServiceError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        MediaEncodingError::io(format!("cannot read frame directory {}", dir.display()), e)
    })?;
    let mut frames: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg"))
        })
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    frames.sort();
    Ok(frames)
}

#[async_trait]
impl<T: Transcoder> TrickplayFrameExtractor for TrickplayFrameExtractorImpl<T> {
    async fn extract_trickplay_frames(
        &self,
        input_path: &str,
        interval_ms: i32,
        max_width: i32,
        qscale: i32,
        threads: i32,
        output_dir: &str,
    ) -> Result<Vec<String>, ServiceError> {
        if interval_ms <= 0 {
            return Err(ServiceError::invalid_input(
                "trickplay interval must be positive",
            ));
        }
        if max_width <= 0 {
            return Err(ServiceError::invalid_input(
                "trickplay width must be positive",
            ));
        }

        let dir = Path::new(output_dir);
        std::fs::create_dir_all(dir).map_err(|e| {
            MediaEncodingError::io(format!("cannot create frame directory {output_dir}"), e)
        })?;

        let output_pattern = dir.join("%08d.jpg");
        let args = build_trickplay_args(
            input_path,
            interval_ms,
            max_width,
            qscale,
            threads,
            &output_pattern.to_string_lossy(),
        );

        // The Transcoder seam captures output regardless of exit status, so
        // failure is detected by an empty frame directory; stderr (read here)
        // carries ffmpeg's error text for the diagnostic.
        let stderr = self
            .transcoder
            .get_process_output(&self.ffmpeg_path, &args, true, None)
            .await
            .map_err(MediaEncodingError::process)?;

        let frames = jpg_files_sorted(dir)?;
        if frames.is_empty() {
            return Err(ServiceError::backend(format!(
                "ffmpeg produced no trickplay frames for {input_path}: {}",
                stderr.trim()
            )));
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use hermit_traits::media_encoding::TrickplayFrameExtractor as _;

    use super::{TrickplayFrameExtractorImpl, build_trickplay_args};
    use crate::encoder::Transcoder;

    /// A [`Transcoder`] fake that records the argument line and "produces" the
    /// given frame files by touching them in the parsed output directory.
    struct RecordingTranscoder {
        args: Mutex<Vec<String>>,
        frames_to_write: usize,
        stderr: String,
    }

    impl RecordingTranscoder {
        fn new(frames_to_write: usize, stderr: &str) -> Self {
            Self {
                args: Mutex::new(Vec::new()),
                frames_to_write,
                stderr: stderr.to_owned(),
            }
        }

        fn recorded(&self) -> Vec<String> {
            self.args.lock().expect("args lock").clone()
        }
    }

    #[async_trait]
    impl Transcoder for RecordingTranscoder {
        async fn get_process_output(
            &self,
            _path: &str,
            arguments: &str,
            _read_stderr: bool,
            _test_key: Option<&str>,
        ) -> Result<String, String> {
            self.args
                .lock()
                .expect("args lock")
                .push(arguments.to_owned());
            // Recover the output dir from the trailing `"{dir}/%08d.jpg"`.
            let pattern = arguments
                .rsplit('"')
                .nth(1)
                .expect("quoted output pattern present");
            let dir = std::path::Path::new(pattern)
                .parent()
                .expect("pattern has a parent dir");
            for i in 1..=self.frames_to_write {
                std::fs::write(dir.join(format!("{i:08}.jpg")), b"jpg").expect("write frame");
            }
            Ok(self.stderr.clone())
        }

        async fn get_process_exit_code(&self, _path: &str, _arguments: &str) -> bool {
            true
        }
    }

    #[test]
    fn args_mirror_the_upstream_format_string() {
        let args = build_trickplay_args("/m/v.mkv", 10_000, 320, 4, 1, "/tmp/out/%08d.jpg");
        assert_eq!(
            args,
            "-loglevel error -threads 1 -i file:\"/m/v.mkv\" -an -sn \
             -vf \"fps=1000/10000,scale=trunc(min(max(iw\\,ih*dar)\\,320)/2)*2:trunc(ow/dar/2)*2\" \
             -threads 1 -c:v mjpeg -qscale:v 4 -vsync 0 -f image2 \"/tmp/out/%08d.jpg\""
        );
    }

    #[test]
    fn qscale_is_clamped_to_ffmpeg_range() {
        let args = build_trickplay_args("/m/v.mkv", 10_000, 320, 0, 0, "o");
        assert!(args.contains("-qscale:v 1 "), "low clamp in {args}");
        let args = build_trickplay_args("/m/v.mkv", 10_000, 320, 99, 0, "o");
        assert!(args.contains("-qscale:v 31 "), "high clamp in {args}");
    }

    #[tokio::test]
    async fn extraction_returns_sorted_frames() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let transcoder = Arc::new(RecordingTranscoder::new(3, ""));
        let extractor = TrickplayFrameExtractorImpl::new(Arc::clone(&transcoder), "ffmpeg");

        let frames = extractor
            .extract_trickplay_frames("/m/v.mkv", 10_000, 320, 4, 1, &out.to_string_lossy())
            .await
            .expect("frames");

        assert_eq!(frames.len(), 3);
        assert!(frames[0].ends_with("00000001.jpg"));
        assert!(frames[2].ends_with("00000003.jpg"));
        assert!(frames.windows(2).all(|w| w[0] < w[1]), "sorted order");

        let recorded = transcoder.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].contains("fps=1000/10000"));
        assert!(recorded[0].contains("-i file:\"/m/v.mkv\""));
    }

    #[tokio::test]
    async fn no_frames_is_an_error_carrying_stderr() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("frames");
        let extractor = TrickplayFrameExtractorImpl::new(
            Arc::new(RecordingTranscoder::new(0, "boom: no such codec")),
            "ffmpeg",
        );

        let err = extractor
            .extract_trickplay_frames("/m/v.mkv", 10_000, 320, 4, 1, &out.to_string_lossy())
            .await
            .expect_err("no frames should error");
        assert!(err.to_string().contains("boom: no such codec"), "{err}");
    }

    #[tokio::test]
    async fn non_positive_inputs_are_invalid() {
        let extractor =
            TrickplayFrameExtractorImpl::new(Arc::new(RecordingTranscoder::new(0, "")), "ffmpeg");
        assert!(
            extractor
                .extract_trickplay_frames("/m/v.mkv", 0, 320, 4, 1, "/tmp/x")
                .await
                .is_err()
        );
        assert!(
            extractor
                .extract_trickplay_frames("/m/v.mkv", 10_000, 0, 4, 1, "/tmp/x")
                .await
                .is_err()
        );
    }
}
