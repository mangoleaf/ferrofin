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
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use ferrofin_model::configuration::ProcessPriorityClass;
use ferrofin_util::process_priority::{ProcessPriority, set_process_priority};
use tokio::io::AsyncWriteExt as _;

use super::encoding_utils::split_args;
use super::transcoder::{ProcessOutput, Transcoder};

/// The production [`Transcoder`]: spawns ffmpeg/ffprobe with `tokio::process`.
///
/// Splits the caller-supplied `arguments` string into argv with [`split_args`],
/// which respects the shell quoting the argument builders emit (`file:"…"`) so
/// paths containing spaces survive as a single token. A string whose quoting
/// does not balance is refused, never run.
///
/// Everything started through [`Transcoder::get_process_output`] (probes, image
/// extraction) or [`Transcoder::get_process_output_at_priority`] (trickplay)
/// runs at `BelowNormal`, as upstream's `MediaEncoder.StartProcess` does;
/// [`Transcoder::get_process_exit_code`] does not lower its process.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioTranscoder;

impl TokioTranscoder {
    /// Creates the production transcoder.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Upstream's `MediaEncoder._canSetProcessPriority` (jellyfin#16872): cleared
/// by the first failure to lower a process's priority, so a server that may
/// not (some containers) warns once rather than once per probed file. A
/// process that exits before it is lowered does not clear it (jellyfin#17762).
static CAN_SET_PROCESS_PRIORITY: AtomicBool = AtomicBool::new(true);

/// `MediaEncoder.StartProcess`: every probe and extraction runs at
/// `BelowNormal`, so background media work yields to live transcodes.
fn lower_priority(child: &tokio::process::Child, path: &str) {
    if let Some(error) = lower_while_able(&CAN_SET_PROCESS_PRIORITY, || {
        set_process_priority(child, ProcessPriority::BelowNormal)
    }) {
        tracing::warn!(
            path,
            %error,
            "unable to set process priority to BelowNormal; further attempts will be skipped"
        );
    }
}

/// The capability step of `StartProcess`: runs `set` while `can_set` holds.
/// The first failure clears it and is returned, to be logged once; later
/// calls do not try. A process that exited first is no failure:
/// [`set_process_priority`] answers `Ok` for it.
fn lower_while_able(
    can_set: &AtomicBool,
    set: impl FnOnce() -> std::io::Result<()>,
) -> Option<std::io::Error> {
    if !can_set.load(Ordering::Relaxed) {
        return None;
    }
    match set() {
        Ok(()) => None,
        Err(error) => can_set.swap(false, Ordering::Relaxed).then_some(error),
    }
}

/// `ExtractVideoImagesOnIntervalInternal`: the trickplay run then takes
/// `TrickplayOptions.ProcessPriority`. Raising a priority needs privileges, so
/// a failure is logged at debug and the process keeps running as it is.
fn set_configured_priority(
    child: &tokio::process::Child,
    priority: ProcessPriorityClass,
    path: &str,
) {
    let Some(class) = priority.os_priority() else {
        tracing::debug!(
            path,
            ?priority,
            "unable to set an unnamed process priority class"
        );
        return;
    };
    if let Err(error) = set_process_priority(child, class) {
        tracing::debug!(path, ?priority, %error, "unable to set process priority");
    }
}

impl TokioTranscoder {
    /// Runs `path arguments`, lowered to `BelowNormal` and then, when given,
    /// set to `priority`.
    async fn run(
        path: &str,
        arguments: &str,
        read_stderr: bool,
        test_key: Option<&str>,
        env: &[(String, String)],
        priority: Option<ProcessPriorityClass>,
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
        lower_priority(&child, path);
        if let Some(priority) = priority {
            set_configured_priority(&child, priority, path);
        }

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
        Self::run(path, arguments, read_stderr, test_key, env, None).await
    }

    async fn get_process_output_at_priority(
        &self,
        path: &str,
        arguments: &str,
        read_stderr: bool,
        env: &[(String, String)],
        priority: ProcessPriorityClass,
    ) -> Result<ProcessOutput, String> {
        Self::run(path, arguments, read_stderr, None, env, Some(priority)).await
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

    /// The niceness of this test process, which a child starts with.
    #[cfg(target_os = "linux")]
    fn own_nice() -> i32 {
        let out = std::process::Command::new("nice")
            .output()
            .expect("run nice");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("a number")
    }

    /// The niceness a shell started through the transcoder reports once it
    /// has run a moment, by which time the transcoder has set its priority.
    #[cfg(target_os = "linux")]
    async fn child_nice(
        priority: Option<ferrofin_model::configuration::ProcessPriorityClass>,
    ) -> i32 {
        use crate::encoder::Transcoder as _;
        let transcoder = super::TokioTranscoder::new();
        let script = r#"-c "sleep 0.3; nice""#;
        let out = match priority {
            None => {
                transcoder
                    .get_process_output("sh", script, false, None, &[])
                    .await
            }
            Some(p) => {
                transcoder
                    .get_process_output_at_priority("sh", script, false, &[], p)
                    .await
            }
        }
        .expect("sh runs");
        out.output.trim().parse().expect("a number")
    }

    /// The niceness a child set to `priority` ends up at on this host: the
    /// class's, unless the OS refuses it (raising a priority past
    /// `RLIMIT_NICE`), when the child keeps the one it started with.
    #[cfg(target_os = "linux")]
    fn allowed_nice(priority: ferrofin_util::process_priority::ProcessPriority) -> i32 {
        let probe = tokio::process::Command::new("sleep")
            .arg("5")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");
        match ferrofin_util::process_priority::set_process_priority(&probe, priority) {
            Ok(()) => priority.nice(),
            Err(_) => own_nice(),
        }
    }

    /// The first failure to lower a priority clears the capability and is
    /// reported once; afterwards nothing is tried (jellyfin#16872). Success
    /// keeps it.
    #[test]
    fn the_first_failure_switches_lowering_off_once() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let can_set = AtomicBool::new(true);
        assert!(super::lower_while_able(&can_set, || Ok(())).is_none());
        assert!(can_set.load(Ordering::Relaxed));
        let refused = || Err(std::io::Error::from_raw_os_error(1));
        assert!(super::lower_while_able(&can_set, refused).is_some());
        assert!(!can_set.load(Ordering::Relaxed));
        let untried = || -> std::io::Result<()> { panic!("not tried once off") };
        assert!(super::lower_while_able(&can_set, untried).is_none());
    }

    /// Every probe and extraction runs below normal priority, as upstream's
    /// `MediaEncoder.StartProcess` lowers it (nice 10 on Unix).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn processes_run_below_normal_priority() {
        let expected = allowed_nice(ferrofin_util::process_priority::ProcessPriority::BelowNormal);
        assert_eq!(child_nice(None).await, expected);
    }

    /// The trickplay run takes its configured priority after the default
    /// (`ExtractVideoImagesOnIntervalInternal`). An unnamed class cannot be
    /// set, and leaves the process at the default.
    #[cfg(target_os = "linux")]
    #[rstest::rstest]
    #[case(
        ferrofin_model::configuration::ProcessPriorityClass::Idle,
        ferrofin_util::process_priority::ProcessPriority::Idle
    )]
    #[case(
        ferrofin_model::configuration::ProcessPriorityClass::BelowNormal,
        ferrofin_util::process_priority::ProcessPriority::BelowNormal
    )]
    #[case(
        ferrofin_model::configuration::ProcessPriorityClass::Unrecognized(7),
        ferrofin_util::process_priority::ProcessPriority::BelowNormal
    )]
    #[tokio::test]
    async fn the_trickplay_priority_is_set_after_the_default(
        #[case] priority: ferrofin_model::configuration::ProcessPriorityClass,
        #[case] ends_at: ferrofin_util::process_priority::ProcessPriority,
    ) {
        assert_eq!(child_nice(Some(priority)).await, allowed_nice(ends_at));
    }

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
