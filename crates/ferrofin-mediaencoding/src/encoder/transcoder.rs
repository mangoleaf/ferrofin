//! The [`Transcoder`] seam — the boundary between the pure argument/validation
//! logic and the un-mockable ffmpeg/ffprobe process invocation.
//!
//! All capability probes (`-version`, `-encoders`, `-decoders`, `-hwaccels`,
//! `-filters`, `-h filter=…`, hardware device init) shell out to ffmpeg. Those
//! calls live behind this trait so unit tests substitute a fake and the real
//! `tokio::process` spawn stays out of the coverage/parity numbers.

use async_trait::async_trait;

/// Runs an ffmpeg/ffprobe process and returns its captured output.
///
/// Implementors own the real process spawn (`tokio::process::Command`); the
/// pure `EncoderValidator` / encoding logic depends only on this trait so tests
/// can inject a deterministic fake.
#[async_trait]
pub trait Transcoder: Send + Sync {
    /// Runs `path arguments` and returns captured stdout (or stderr when
    /// `read_stderr` is set), feeding `test_key` to stdin when provided.
    ///
    /// Mirrors C# `EncoderValidator.GetProcessOutput`.
    ///
    /// # Errors
    ///
    /// Returns an error string if the process cannot be spawned or fails.
    async fn get_process_output(
        &self,
        path: &str,
        arguments: &str,
        read_stderr: bool,
        test_key: Option<&str>,
    ) -> Result<String, String>;

    /// Runs `path arguments` and returns whether it exited with code `0`.
    ///
    /// Mirrors C# `EncoderValidator.GetProcessExitCode`.
    async fn get_process_exit_code(&self, path: &str, arguments: &str) -> bool;
}
