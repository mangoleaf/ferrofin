//! The [`Transcoder`] seam — the boundary between the pure argument/validation
//! logic and the un-mockable ffmpeg/ffprobe process invocation.
//!
//! All capability probes (`-version`, `-encoders`, `-decoders`, `-hwaccels`,
//! `-filters`, `-h filter=…`, hardware device init) shell out to ffmpeg. Those
//! calls live behind this trait so unit tests substitute a fake and the real
//! `tokio::process` spawn stays out of the coverage/parity numbers.

use async_trait::async_trait;

/// What one process run produced.
///
/// The exit status is carried, not discarded: ffmpeg that dies partway through
/// an extraction exits non-zero **and has already written frames**, so "did it
/// produce output" and "did it succeed" are different questions. Upstream
/// keys its trickplay retry on the exit code for exactly this reason, and
/// notes in its own comment that a failed run is not guaranteed to leave the
/// output directory empty.
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    /// stdout, or stderr when the caller asked to read stderr.
    pub output: String,
    /// Whether the process exited with status `0`.
    pub success: bool,
}

impl ProcessOutput {
    /// The captured text, discarding the status — for callers that only parse
    /// output and treat an empty parse as the failure.
    #[must_use]
    pub fn into_output(self) -> String {
        self.output
    }
}

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
    /// `env` is set **on the child**, not on the server. Upstream's VAAPI
    /// branch calls `Environment.SetEnvironmentVariable` and lets every child
    /// inherit; scoping it to the one process that needs it is the same effect
    /// without mutating the server's own environment from a filter builder.
    /// It carries the libva driver selection, so dropping it on a host with
    /// more than one driver installed silently loads the wrong one. Probe
    /// calls pass `&[]`.
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
        env: &[(String, String)],
    ) -> Result<ProcessOutput, String>;

    /// Runs `path arguments` and returns whether it exited with code `0`.
    ///
    /// Mirrors C# `EncoderValidator.GetProcessExitCode`.
    async fn get_process_exit_code(&self, path: &str, arguments: &str) -> bool;
}
