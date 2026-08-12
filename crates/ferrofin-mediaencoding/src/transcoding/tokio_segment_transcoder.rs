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

        let exited = Arc::new(AtomicBool::new(false));
        let exit_code = Arc::new(Mutex::new(None::<i32>));
        let child = Arc::new(Mutex::new(child));

        // Pump stderr → log until EOF. Detached so it never blocks kill.
        if let Some(mut stderr) = stderr {
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let _ = log.write_all(&buf[..n]).await;
                        }
                    }
                }
                let _ = log.flush().await;
            });
        }

        Ok(Box::new(TokioTranscodeChild {
            child,
            exited,
            exit_code,
        }))
    }
}

/// A live handle over a spawned ffmpeg `tokio::process::Child`.
struct TokioTranscodeChild {
    child: Arc<Mutex<tokio::process::Child>>,
    exited: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
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
        let status = {
            let mut child = self.child.lock().await;
            child.wait().await
        };
        let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
        self.exited.store(true, Ordering::SeqCst);
        *self.exit_code.lock().await = Some(code);
        code
    }

    async fn kill(&self) -> Result<(), String> {
        let mut child = self.child.lock().await;
        child
            .kill()
            .await
            .map_err(|e| format!("failed to kill ffmpeg: {e}"))?;
        self.exited.store(true, Ordering::SeqCst);
        Ok(())
    }
}
