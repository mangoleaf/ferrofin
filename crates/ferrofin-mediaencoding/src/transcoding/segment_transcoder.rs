//! The [`SegmentTranscoder`] seam — the boundary between the pure
//! transcode-orchestration logic and the un-mockable long-running ffmpeg spawn.
//!
//! Unlike the probe-only [`Transcoder`](crate::encoder::Transcoder) (which runs
//! a short-lived process and returns its *captured* output), a segment transcode
//! spawns ffmpeg as a **long-running** process that writes a growing set of
//! segment files to disk while the caller polls for them. That difference in
//! shape — a live [`TranscodeChild`] handle rather than captured bytes — is why
//! this is a separate, sibling trait rather than a method on `Transcoder`; it
//! keeps each trait honest and object-safe.
//!
//! All orchestration (`start_ffmpeg`, the wait-until-segment loops, kill and
//! cleanup) depends only on this seam and is unit-tested with
//! [`FakeSegmentTranscoder`], which materialises fake segment files on demand.
//! The single un-mockable real spawn lives in
//! [`TokioSegmentTranscoder`](crate::transcoding::TokioSegmentTranscoder).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;

/// A parked long-running-transcode request handed to a [`SegmentTranscoder`].
///
/// Port of the `ProcessStartInfo` fields `StartFfMpeg` fills in
/// (`FileName`/`Arguments`/`WorkingDirectory`) plus the segment cache directory
/// and stderr log path the orchestration allocates before spawning. Arguments
/// are pre-tokenised into a `Vec<String>` (not one command line) so the fake and
/// the real spawn agree on tokenisation and neither re-parses shell quoting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    /// The resolved ffmpeg binary path (`MediaEncoder::encoder_path`).
    pub program: String,
    /// The fully-built ffmpeg arguments (from `encoding_helper`).
    pub arguments: Vec<String>,
    /// The working directory for the process, if any (`WorkingDirectory`).
    pub working_dir: Option<PathBuf>,
    /// The per-session segment cache directory; created by the orchestration
    /// **before** the seam is called.
    pub output_dir: PathBuf,
    /// The `FFmpeg.Transcode-*.log` target that ffmpeg's stderr is streamed to.
    pub log_path: PathBuf,
    /// Environment variables to set **on the ffmpeg child**, from
    /// [`InputHwaccelArgs::env`](crate::encoding_helper::hw::input_args::InputHwaccelArgs::env).
    ///
    /// Some hardware paths are configured by environment rather than by
    /// argument — the VAAPI driver selection among them — and upstream sets
    /// these on the server process. Carrying them per-spawn instead keeps one
    /// job's driver choice from leaking into another's, and keeps the server's
    /// own environment untouched.
    pub env: Vec<(String, String)>,
}

/// A live handle to a spawned transcode process.
///
/// Port of the identity/state subset of `TranscodingJob` that `StartFfMpeg` and
/// `GetSegmentResult` poll: `HasExited`, `ExitCode`, and the wait/kill controls.
#[async_trait]
pub trait TranscodeChild: Send + Sync {
    /// Whether the process has exited. Port of `TranscodingJob.HasExited`.
    fn has_exited(&self) -> bool;

    /// The process exit code, once it has exited. Port of
    /// `TranscodingJob.ExitCode`.
    fn exit_code(&self) -> Option<i32>;

    /// Awaits process exit, returning the exit code. Drives the
    /// `OnFfMpegProcessExited` equivalent (flips `has_exited`, records the code).
    async fn wait(&self) -> i32;

    /// Kills the process (the `Process.Kill`/`Stop` equivalent used by
    /// `KillTranscodingJob`).
    ///
    /// # Errors
    ///
    /// Returns an error string if the process cannot be signalled.
    async fn kill(&self) -> Result<(), String>;
}

/// Compile-time assertion that [`TranscodeChild`] is object-safe.
fn _assert_object_safe_transcode_child(_: &dyn TranscodeChild) {}

/// Spawns a long-running, segment-producing ffmpeg process behind a seam.
///
/// The one method every transcode-orchestration path depends on; the real
/// `tokio::process` spawn lives only in the concrete
/// [`TokioSegmentTranscoder`](crate::transcoding::TokioSegmentTranscoder).
#[async_trait]
pub trait SegmentTranscoder: Send + Sync {
    /// Spawns the transcode described by `req`, returning a live handle.
    ///
    /// # Errors
    ///
    /// Returns an error string if the process cannot be spawned.
    async fn start_transcode(&self, req: &SpawnRequest) -> Result<Box<dyn TranscodeChild>, String>;
}

/// Compile-time assertion that [`SegmentTranscoder`] is object-safe.
fn _assert_object_safe_segment_transcoder(_: &dyn SegmentTranscoder) {}

/// How a [`FakeSegmentTranscoder`] should materialise its output.
///
/// Lets unit tests drive every orchestration branch (segment appears
/// immediately / after N polls / never; process exits cleanly / non-zero /
/// keeps running) with zero ffmpeg and no wall-clock dependence.
#[derive(Debug, Clone, Default)]
pub struct FakeScript {
    /// Segment filenames (relative to `output_dir`) to write, in order.
    pub segment_files: Vec<String>,
    /// Extra bookkeeping files (e.g. the `.m3u8`) to write alongside segments.
    pub extra_files: Vec<String>,
    /// Number of `start_transcode` calls to wait before writing files. `0`
    /// writes them synchronously inside `start_transcode`; higher values leave
    /// the directory empty so the caller's wait-loop must poll.
    pub write_after_polls: usize,
    /// The exit code the child reports once it has "exited".
    pub exit_code: i32,
    /// Whether the child has already exited when handed back. When `false`, the
    /// child stays running until [`FakeTranscodeChild::finish`] is called.
    pub exits_immediately: bool,
}

/// A [`SegmentTranscoder`] that writes fake segment files instead of spawning
/// ffmpeg, so every orchestration path is unit-testable deterministically.
///
/// Port-test analogue of a running ffmpeg: `start_transcode` writes the scripted
/// segment files into `output_dir` (per [`FakeScript`]) and returns a
/// script-controlled [`FakeTranscodeChild`].
#[derive(Debug, Clone)]
pub struct FakeSegmentTranscoder {
    script: FakeScript,
    /// The requests this fake was asked to spawn (test inspection).
    pub requests: Arc<std::sync::Mutex<Vec<SpawnRequest>>>,
}

impl FakeSegmentTranscoder {
    /// Creates a fake driven by `script`.
    #[must_use]
    pub fn new(script: FakeScript) -> Self {
        Self {
            script,
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Writes the scripted segment + extra files into `output_dir`.
    fn materialize(&self, output_dir: &std::path::Path) -> Result<(), String> {
        for name in self
            .script
            .segment_files
            .iter()
            .chain(self.script.extra_files.iter())
        {
            std::fs::write(output_dir.join(name), b"fake\n")
                .map_err(|e| format!("fake write {name}: {e}"))?;
        }
        Ok(())
    }
}

#[async_trait]
impl SegmentTranscoder for FakeSegmentTranscoder {
    async fn start_transcode(&self, req: &SpawnRequest) -> Result<Box<dyn TranscodeChild>, String> {
        self.requests
            .lock()
            .expect("fake requests lock poisoned")
            .push(req.clone());

        if self.script.write_after_polls == 0 {
            self.materialize(&req.output_dir)?;
        }

        let child = FakeTranscodeChild::new(
            self.clone(),
            req.output_dir.clone(),
            self.script.exit_code,
            self.script.exits_immediately,
            self.script.write_after_polls,
        );
        Ok(Box::new(child))
    }
}

/// A script-controlled [`TranscodeChild`] paired with [`FakeSegmentTranscoder`].
///
/// `has_exited`/`exit_code` are flipped either immediately (per the script) or
/// by an explicit [`finish`](Self::finish); `has_exited` also materialises the
/// scripted files once its poll budget is spent, so a caller's wait loop can
/// observe segments appearing "over time" without any real clock.
pub struct FakeTranscodeChild {
    fake: FakeSegmentTranscoder,
    output_dir: PathBuf,
    exit_code: i32,
    exited: Arc<AtomicBool>,
    killed: Arc<AtomicBool>,
    polls_remaining: Arc<AtomicUsize>,
}

impl FakeTranscodeChild {
    fn new(
        fake: FakeSegmentTranscoder,
        output_dir: PathBuf,
        exit_code: i32,
        exits_immediately: bool,
        write_after_polls: usize,
    ) -> Self {
        Self {
            fake,
            output_dir,
            exit_code,
            exited: Arc::new(AtomicBool::new(exits_immediately)),
            killed: Arc::new(AtomicBool::new(false)),
            polls_remaining: Arc::new(AtomicUsize::new(write_after_polls)),
        }
    }

    /// Marks the fake process as finished (its scripted files are written).
    ///
    /// # Panics
    ///
    /// Panics if writing a scripted file fails.
    pub fn finish(&self) {
        self.fake
            .materialize(&self.output_dir)
            .expect("fake materialize on finish");
        self.exited.store(true, Ordering::SeqCst);
    }

    /// Whether [`kill`](TranscodeChild::kill) was called (test inspection).
    #[must_use]
    pub fn was_killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TranscodeChild for FakeTranscodeChild {
    fn has_exited(&self) -> bool {
        // Each poll of `has_exited` spends one unit of the write budget; when it
        // reaches zero the scripted files appear, letting a wait-loop observe
        // segments materialising after N polls without any wall clock.
        let remaining = self.polls_remaining.load(Ordering::SeqCst);
        if remaining == 0 {
            let _ = self.fake.materialize(&self.output_dir);
        } else {
            self.polls_remaining.store(remaining - 1, Ordering::SeqCst);
        }
        self.exited.load(Ordering::SeqCst)
    }

    fn exit_code(&self) -> Option<i32> {
        self.exited.load(Ordering::SeqCst).then_some(self.exit_code)
    }

    async fn wait(&self) -> i32 {
        self.finish();
        self.exit_code
    }

    async fn kill(&self) -> Result<(), String> {
        self.killed.store(true, Ordering::SeqCst);
        self.exited.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{FakeScript, FakeSegmentTranscoder, SegmentTranscoder, SpawnRequest};

    fn req(dir: &std::path::Path) -> SpawnRequest {
        SpawnRequest {
            env: Vec::new(),
            program: "ffmpeg".to_owned(),
            arguments: vec!["-i".to_owned(), "in.mkv".to_owned()],
            working_dir: None,
            output_dir: dir.to_path_buf(),
            log_path: dir.join("ffmpeg.log"),
        }
    }

    #[tokio::test]
    async fn fake_writes_segments_immediately_and_records_request() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeSegmentTranscoder::new(FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            ..FakeScript::default()
        });
        let child = fake.start_transcode(&req(dir.path())).await.unwrap();
        assert!(dir.path().join("out0.ts").exists());
        assert!(dir.path().join("out.m3u8").exists());
        assert!(!child.has_exited());
        assert_eq!(fake.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fake_child_exits_on_wait_with_code() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeSegmentTranscoder::new(FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            exit_code: 3,
            ..FakeScript::default()
        });
        let child = fake.start_transcode(&req(dir.path())).await.unwrap();
        assert_eq!(child.wait().await, 3);
        assert!(child.has_exited());
        assert_eq!(child.exit_code(), Some(3));
    }

    #[tokio::test]
    async fn fake_kill_sets_flags() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeSegmentTranscoder::new(FakeScript::default());
        let child = fake.start_transcode(&req(dir.path())).await.unwrap();
        child.kill().await.unwrap();
        assert!(child.has_exited());
    }

    #[tokio::test]
    async fn immediate_exit_reports_code_without_wait() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeSegmentTranscoder::new(FakeScript {
            exit_code: 1,
            exits_immediately: true,
            ..FakeScript::default()
        });
        let child = fake.start_transcode(&req(dir.path())).await.unwrap();
        assert!(child.has_exited());
        assert_eq!(child.exit_code(), Some(1));
    }
}
