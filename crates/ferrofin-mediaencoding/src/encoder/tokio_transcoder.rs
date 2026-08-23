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

use super::transcoder::Transcoder;

/// The production [`Transcoder`]: spawns ffmpeg/ffprobe with `tokio::process`.
///
/// Splits the caller-supplied `arguments` string into argv with [`split_args`],
/// which respects the shell quoting the argument builders emit (`file:"…"`) so
/// paths containing spaces survive as a single token.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioTranscoder;

impl TokioTranscoder {
    /// Creates the production transcoder.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Splits a command-line argument string into argv, honoring the shell quoting
/// the encoder's argument builders emit — `-i file:"/path/with spaces/f.mkv"`
/// must reach ffmpeg as one `-i` value, not four. A plain `split_whitespace`
/// shatters any spaced path (most real media) and hands ffmpeg/ffprobe a broken
/// input, so it is used only as the fallback when the quoting is malformed.
fn split_args(arguments: &str) -> Vec<String> {
    shlex::split(arguments)
        .unwrap_or_else(|| arguments.split_whitespace().map(str::to_owned).collect())
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
            .args(split_args(arguments))
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
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    async fn get_process_exit_code(&self, path: &str, arguments: &str) -> bool {
        tokio::process::Command::new(path)
            .args(split_args(arguments))
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
    use super::split_args;

    /// Whether some live (non-zombie) process has exactly this argv.
    #[cfg(target_os = "linux")]
    fn process_with_argv_exists(argv: &[&str]) -> bool {
        let want: Vec<u8> = argv.iter().flat_map(|a| a.bytes().chain([0])).collect();
        std::fs::read_dir("/proc").is_ok_and(|d| {
            d.flatten()
                .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == want))
        })
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
                    .get_process_output("sleep", &marker, false, None)
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
            split_args(args),
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
            split_args("-v warning -show_streams"),
            vec!["-v", "warning", "-show_streams"]
        );
    }
}
