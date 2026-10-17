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
//! [`NoopTranscoder`](super::media_encoder) fakes and stays counted.
#![cfg_attr(coverage_nightly, coverage(off))]

use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt as _;

use super::encoding_utils::split_args;
use super::transcoder::{ProcessOutput, Transcoder};

/// The production [`Transcoder`]: spawns ffmpeg/ffprobe with `tokio::process`.
///
/// Splits the caller-supplied `arguments` string into argv with [`split_args`],
/// which respects the shell quoting the argument builders emit (`file:"…"`) so
/// paths containing spaces survive as a single token. A string whose quoting
/// does not balance is refused, never run.
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
        env: &[(String, String)],
    ) -> Result<ProcessOutput, String> {
        let mut command = tokio::process::Command::new(path);
        command
            .args(split_args(arguments)?)
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Scheduled-task cancellation is `JoinHandle::abort` — the future is
            // dropped, and a tokio `Child` outlives its future unless told not
            // to. Upstream kills the process on the cancellation token; this is
            // the same contract (observed: a stopped trickplay task left its 4K
            // ffmpeg running to completion).
            .kill_on_drop(true);

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
        Ok(ProcessOutput {
            output: String::from_utf8_lossy(bytes).into_owned(),
            success: output.status.success(),
        })
    }

    async fn get_process_exit_code(&self, path: &str, arguments: &str) -> bool {
        let args = match split_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                tracing::debug!(%error, "not running a command whose quoting does not balance");
                return false;
            }
        };
        tokio::process::Command::new(path)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .is_ok_and(|status| status.success())
    }
}

#[cfg(test)]
mod tests {
    use crate::encoder::split_args;

    /// Whether some live (non-zombie) process has exactly this argv.
    #[cfg(target_os = "linux")]
    fn process_with_argv_exists(argv: &[&str]) -> bool {
        let want: Vec<u8> = argv.iter().flat_map(|a| a.bytes().chain([0])).collect();
        std::fs::read_dir("/proc").is_ok_and(|d| {
            d.flatten()
                .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == want))
        })
    }

    /// The environment has to reach the real child, not just the seam.
    ///
    /// The extractor-side test for this uses a fake transcoder, so it proves
    /// the hand-off and nothing about the spawn; delete the `.envs()` call and
    /// it still passes. This one spawns a process and reads back what it saw.
    /// It matters because the variable carried here is the libva driver
    /// selection: an i965 host that loses it silently runs on iHD.
    #[tokio::test]
    async fn the_child_process_receives_the_environment() {
        use super::TokioTranscoder;
        use crate::encoder::Transcoder as _;

        let run = TokioTranscoder::new()
            .get_process_output(
                "sh",
                r#"-c "printf %s \"$LIBVA_DRIVER_NAME\"""#,
                false,
                None,
                &[("LIBVA_DRIVER_NAME".to_owned(), "i965".to_owned())],
            )
            .await
            .expect("sh runs");
        assert_eq!(run.output, "i965");
        assert!(run.success);
    }

    /// A non-zero exit is reported as such, not folded into the output.
    ///
    /// The trickplay retry keys on this: ffmpeg that dies partway exits
    /// non-zero having already written frames, so "produced output" and
    /// "succeeded" have to stay separable.
    #[tokio::test]
    async fn a_non_zero_exit_is_reported_as_failure() {
        use super::TokioTranscoder;
        use crate::encoder::Transcoder as _;

        let run = TokioTranscoder::new()
            .get_process_output("sh", r#"-c "exit 3""#, false, None, &[])
            .await
            .expect("sh runs");
        assert!(!run.success, "exit 3 is a failure");
    }

    /// The scheduled-task cancel path is `JoinHandle::abort`, i.e. the future
    /// running the child is dropped. The child must die with it — otherwise a
    /// stopped trickplay task leaves its ffmpeg burning CPU to completion
    /// (observed live).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn aborting_the_caller_kills_the_child_process() {
        use super::TokioTranscoder;
        use crate::encoder::Transcoder as _;

        // A duration no other process on the box is sleeping for (integer:
        // fractional `sleep` is a GNU/busybox extension).
        let marker = format!("{}", 70_000 + std::process::id() % 10_000);
        let task = tokio::spawn({
            let marker = marker.clone();
            async move {
                TokioTranscoder
                    .get_process_output("sleep", &marker, false, None, &[])
                    .await
            }
        });
        for _ in 0..100 {
            if process_with_argv_exists(&["sleep", &marker]) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            process_with_argv_exists(&["sleep", &marker]),
            "child never started"
        );

        task.abort();
        let _ = task.await;

        for _ in 0..100 {
            if !process_with_argv_exists(&["sleep", &marker]) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("`sleep {marker}` outlived the aborted task");
    }

    #[test]
    fn quoted_path_with_spaces_stays_one_arg() {
        // The exact shape the probe/encoding argument builders emit.
        let args =
            r#"-i file:"/tmp/movies/Big Buck Bunny (2008).mp4" -threads 0 -print_format json"#;
        assert_eq!(
            split_args(args).unwrap(),
            vec![
                "-i",
                "file:/tmp/movies/Big Buck Bunny (2008).mp4",
                "-threads",
                "0",
                "-print_format",
                "json",
            ]
        );
    }

    #[test]
    fn unquoted_args_split_on_whitespace() {
        assert_eq!(
            split_args("-v warning -show_streams").unwrap(),
            vec!["-v", "warning", "-show_streams"]
        );
    }

    /// Unbalanced quoting is refused before anything is spawned, rather than
    /// guessed at with a whitespace split that could turn path text into
    /// options.
    #[tokio::test]
    async fn unbalanced_quoting_is_not_run() {
        use super::TokioTranscoder;
        use crate::encoder::Transcoder as _;

        let err = TokioTranscoder
            .get_process_output("sh", r#"-c "exit 0"#, false, None, &[])
            .await
            .expect_err("unbalanced quoting must not spawn");
        assert!(err.contains("unbalanced quoting"), "{err}");
        // The old whitespace fallback ran this as `sh -c true '"'`, exit 0.
        assert!(
            !TokioTranscoder
                .get_process_exit_code("sh", r#"-c true ""#)
                .await
        );
    }
}
