//! Executes the configured recording post-processor after a retained capture.
//!
//! Port of Jellyfin v12.4 `RecordingsManager.PostProcessRecording`: no shell,
//! wait for exit, and leave recording success independent of process failure.

use std::io;
use std::path::Path;
use std::process::ExitStatus;

use ferrofin_model::live_tv::LiveTvOptions;

/// Runs the configured executable, or returns `None` when it is disabled.
///
/// Parse the administrator's argument template before replacing `{path}` so
/// quotes, spaces and metacharacters in recording names remain one argument.
/// Expansion is case-insensitive, matching upstream. Arguments are never logged:
/// an administrator may have put credentials in them.
pub(crate) async fn run(options: &LiveTvOptions, path: &Path) -> io::Result<Option<ExitStatus>> {
    let Some(executable) = options
        .recording_post_processor
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let arguments = expand_arguments(
        options
            .recording_post_processor_arguments
            .as_deref()
            .unwrap_or_default(),
        path,
    )?;
    tracing::info!(executable, path = %path.display(), "live tv: running recording post-processor");
    tokio::process::Command::new(executable)
        .args(arguments)
        .status()
        .await
        .map(Some)
}

fn expand_arguments(template: &str, path: &Path) -> io::Result<Vec<String>> {
    let arguments = shlex::split(template).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid recording post-processor argument quoting",
        )
    })?;
    let placeholder = regex::Regex::new(r"(?i)\{path\}").expect("constant placeholder regex");
    let path = path.to_string_lossy();
    Ok(arguments
        .into_iter()
        .map(|argument| {
            placeholder
                .replace_all(&argument, |_: &regex::Captures<'_>| path.as_ref())
                .into_owned()
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_expansion_preserves_one_argument_and_never_expands_shell_syntax() {
        let path = Path::new("/recordings/A \"quote\"; $(touch unwanted) $1 \\ name.ts");
        assert_eq!(
            expand_arguments(r#"--input "{PaTh}" --again={PATH} """#, path).expect("arguments"),
            vec![
                "--input".to_owned(),
                path.to_string_lossy().into_owned(),
                format!("--again={}", path.display()),
                String::new()
            ]
        );
    }

    #[test]
    fn malformed_quoting_is_rejected_before_launch() {
        assert!(expand_arguments("\"unterminated", Path::new("recording.ts")).is_err());
    }

    #[tokio::test]
    async fn unset_or_blank_processor_is_disabled() {
        for processor in [None, Some(String::new()), Some(" \t".to_owned())] {
            let options = LiveTvOptions {
                recording_post_processor: processor,
                ..LiveTvOptions::default()
            };
            assert!(
                run(&options, Path::new("missing.ts"))
                    .await
                    .expect("disabled")
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn missing_processor_reports_spawn_failure() {
        let dir = tempfile::tempdir().expect("directory");
        let options = LiveTvOptions {
            recording_post_processor: Some(dir.path().join("missing").display().to_string()),
            ..LiveTvOptions::default()
        };
        assert_eq!(
            run(&options, Path::new("recording.ts"))
                .await
                .expect_err("missing executable")
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_receives_literal_path_and_its_exit_status_is_observed() {
        let dir = tempfile::tempdir().expect("directory");
        let script = dir.path().join("process.sh");
        let output = dir.path().join("arguments.txt");
        std::fs::write(&script, "printf '%s' \"$1\" > \"$2\"\nexit 7\n").expect("script");
        let options = LiveTvOptions {
            recording_post_processor: Some("/bin/sh".to_owned()),
            recording_post_processor_arguments: Some(format!(
                "{} \"{{path}}\" {}",
                script.display(),
                output.display()
            )),
            ..LiveTvOptions::default()
        };
        let path = Path::new("/recordings/space 'quote\" $value; $(no-command).ts");
        let status = run(&options, path).await.expect("spawn").expect("enabled");
        assert_eq!(status.code(), Some(7));
        assert_eq!(
            std::fs::read_to_string(output).expect("arguments"),
            path.to_string_lossy()
        );
    }
}
