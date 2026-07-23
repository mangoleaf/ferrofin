//! The production [`Transcoder`] that shells out to real ffmpeg/ffprobe via
//! `tokio::process` — the un-mockable process seam behind the pure encoder and
//! probing logic.
//!
//! Mirrors [`TokioSegmentTranscoder`](crate::transcoding::TokioSegmentTranscoder):
//! the only piece that launches a process, captures its output, and inspects
//! its exit status. It is exercised solely by the ffmpeg-gated integration
//! tests, never the unit suite, so it is carved out of the line-coverage gate.
//! Everything that feeds it — argument building, output normalisation,
//! version validation — is unit-tested against
//! [`NoopTranscoder`](super::media_encoder) fakes and stays counted. See
//! `brain/DEFERRED.md` for the carve-out rationale.
#![cfg_attr(coverage_nightly, coverage(off))]

use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt as _;

use super::transcoder::Transcoder;

/// The production [`Transcoder`]: spawns ffmpeg/ffprobe with `tokio::process`.
///
/// Splits the caller-supplied `arguments` string on ASCII whitespace, matching
/// the C# `EncoderValidator` invocation shape where the arguments are a single
/// space-joined command line with no embedded-space tokens.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioTranscoder;

impl TokioTranscoder {
    /// Creates the production transcoder.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Transcoder for TokioTranscoder {
    async fn get_process_output(
        &self,
        path: &str,
        arguments: &str,
        read_stderr: bool,
        test_key: Option<&str>,
    ) -> Result<String, String> {
        let mut command = tokio::process::Command::new(path);
        command
            .args(arguments.split_whitespace())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to spawn `{path} {arguments}`: {e}"))?;

        if let Some(key) = test_key
            && let Some(mut stdin) = child.stdin.take()
        {
            stdin
                .write_all(key.as_bytes())
                .await
                .map_err(|e| format!("failed to write test key to `{path}`: {e}"))?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| format!("failed to run `{path} {arguments}`: {e}"))?;

        let bytes = if read_stderr {
            &output.stderr
        } else {
            &output.stdout
        };
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    async fn get_process_exit_code(&self, path: &str, arguments: &str) -> bool {
        tokio::process::Command::new(path)
            .args(arguments.split_whitespace())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success())
    }
}
