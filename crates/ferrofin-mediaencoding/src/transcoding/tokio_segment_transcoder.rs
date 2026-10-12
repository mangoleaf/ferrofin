//! The concrete [`SegmentTranscoder`] that spawns real ffmpeg via
//! `tokio::process` — the one un-mockable piece of I/O in the transcode runtime.
//!
//! This module is the only thing that actually launches a process, pumps its
//! stderr to a log file, and waits on / kills it. It is exercised solely by the
//! ffmpeg-gated integration tests in `tests/segment_transcode_ffmpeg.rs` (behind
//! `FERROFIN_FFMPEG_TESTS`), never the unit suite, so it is carved out of the
//! line-coverage gate below. Everything it feeds — the `start_ffmpeg`
//! orchestration, the wait-until-segment loops, kill/cleanup — is unit-tested
//! against [`FakeSegmentTranscoder`](super::FakeSegmentTranscoder) and stays
//! counted.
#![cfg_attr(coverage_nightly, coverage(off))]

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use super::progress::{FfmpegProgress, parse};
use super::segment_transcoder::{SegmentTranscoder, SpawnRequest, TranscodeChild};

/// The production [`SegmentTranscoder`]: spawns ffmpeg with `tokio::process`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioSegmentTranscoder;

impl TokioSegmentTranscoder {
    /// Creates the production segment transcoder.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SegmentTranscoder for TokioSegmentTranscoder {
    async fn start_transcode(&self, req: &SpawnRequest) -> Result<Box<dyn TranscodeChild>, String> {
        let mut command = tokio::process::Command::new(&req.program);
        command
            .args(&req.arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &req.working_dir {
            command.current_dir(dir);
        }
        for (key, value) in &req.env {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to spawn ffmpeg {}: {e}", req.program))?;

        // Open the stderr log, prefixed with the command line (mirrors the C#
        // JobLogger header) so a failed transcode is diagnosable from the log.
        let mut log = tokio::fs::File::create(&req.log_path)
            .await
            .map_err(|e| format!("failed to create log {}: {e}", req.log_path.display()))?;
        let header = format!("{} {}\n\n", req.program, req.arguments.join(" "));
        let _ = log.write_all(header.as_bytes()).await;

        let stderr = child.stderr.take();
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let progress = Arc::new(std::sync::Mutex::new(None));

        let exited = Arc::new(AtomicBool::new(false));
        let exit_code = Arc::new(Mutex::new(None::<i32>));
        let child = Arc::new(Mutex::new(child));

        // Pump stderr → log until EOF. Detached so it never blocks kill.
        if let Some(mut stderr) = stderr {
            let report = Arc::clone(&progress);
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let mut line = Vec::new();
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            // Preserve the original stderr bytes in the log;
                            // ffmpeg uses carriage returns for live statistics.
                            let _ = log.write_all(&buf[..n]).await;
                            for byte in &buf[..n] {
                                if *byte == b'\r' || *byte == b'\n' {
                                    if let Some(parsed) = parse(&String::from_utf8_lossy(&line)) {
                                        *report.lock().expect("progress lock poisoned") =
                                            Some(parsed);
                                    }
                                    line.clear();
                                } else if line.len() < 65_536 {
                                    line.push(*byte);
                                }
                            }
                        }
                    }
                }
                if let Some(parsed) = parse(&String::from_utf8_lossy(&line)) {
                    *report.lock().expect("progress lock poisoned") = Some(parsed);
                }
                let _ = log.flush().await;
            });
        }

        Ok(Box::new(TokioTranscodeChild {
            child,
            exited,
            exit_code,
            stdin,
            progress,
        }))
    }
}

/// A live handle over a spawned ffmpeg `tokio::process::Child`.
struct TokioTranscodeChild {
    child: Arc<Mutex<tokio::process::Child>>,
    exited: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    stdin: Arc<Mutex<Option<tokio::process::ChildStdin>>>,
    progress: Arc<std::sync::Mutex<Option<FfmpegProgress>>>,
}

#[async_trait]
impl TranscodeChild for TokioTranscodeChild {
    fn has_exited(&self) -> bool {
        // Non-blocking probe: try_lock avoids stalling the caller's poll loop
        // when `wait` holds the lock; try_wait reaps without blocking.
        if let Ok(mut child) = self.child.try_lock()
            && let Ok(Some(status)) = child.try_wait()
        {
            self.exited.store(true, Ordering::SeqCst);
            if let Ok(mut code) = self.exit_code.try_lock() {
                *code = Some(status.code().unwrap_or(-1));
            }
        }
        self.exited.load(Ordering::SeqCst)
    }

    fn exit_code(&self) -> Option<i32> {
        self.exit_code.try_lock().ok().and_then(|c| *c)
    }

    async fn wait(&self) -> i32 {
        // Poll/reap without holding the child mutex across a wait: teardown
        // must be able to resume and stop a process while another task awaits it.
        while !self.has_exited() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        self.exit_code().unwrap_or(-1)
    }

    fn progress(&self) -> Option<FfmpegProgress> {
        *self.progress.lock().expect("progress lock poisoned")
    }

    async fn write_stdin(&self, bytes: &[u8]) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        let stdin = stdin
            .as_mut()
            .ok_or_else(|| "ffmpeg stdin is closed".to_owned())?;
        stdin
            .write_all(bytes)
            .await
            .map_err(|error| format!("write ffmpeg stdin: {error}"))?;
        stdin
            .flush()
            .await
            .map_err(|error| format!("flush ffmpeg stdin: {error}"))
    }

    async fn kill(&self) -> Result<(), String> {
        if self.has_exited() {
            return Ok(());
        }
        // TranscodingJob.Stop resumes throttling first, then writes q and
        // allows five seconds for a graceful exit before terminating the child.
        let graceful = self.write_stdin(b"q\n").await.is_ok();
        let mut child = self.child.lock().await;
        let status = if graceful {
            tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                .await
                .ok()
                .and_then(Result::ok)
        } else {
            None
        };
        if let Some(status) = status {
            *self.exit_code.lock().await = Some(status.code().unwrap_or(-1));
        } else {
            child
                .kill()
                .await
                .map_err(|error| format!("failed to kill ffmpeg: {error}"))?;
        }
        self.exited.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcoding::SpawnRequest;

    /// The environment has to reach the **child**, not the server.
    ///
    /// No hardware path sets one yet — NVENC configures itself entirely by
    /// argument — so without this the seam would sit unexercised until the
    /// VAAPI driver selection needs it, which is exactly when a silent break
    /// would be hardest to attribute.
    #[tokio::test]
    async fn the_spawned_child_gets_the_requested_environment() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("t.log");
        let req = SpawnRequest {
            program: "sh".to_owned(),
            // stderr, because that is what the transcoder pumps into the log.
            arguments: vec![
                "-c".to_owned(),
                "printf %s \"$FERROFIN_TEST_VAR\" >&2".to_owned(),
            ],
            working_dir: None,
            output_dir: dir.path().to_path_buf(),
            log_path: log.clone(),
            env: vec![("FERROFIN_TEST_VAR".to_owned(), "libcuda".to_owned())],
        };
        let child = TokioSegmentTranscoder::new()
            .start_transcode(&req)
            .await
            .unwrap();
        assert_eq!(child.wait().await, 0);

        // The pump is detached, so give it a moment to flush.
        for _ in 0..50 {
            if std::fs::read_to_string(&log).is_ok_and(|s| s.contains("libcuda")) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!(
            "child never saw the variable; log: {:?}",
            std::fs::read_to_string(&log)
        );
    }
}
