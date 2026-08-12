//! Audio fingerprinting seam + backends.
//!
//! Turns an audio window of a media file into Chromaprint points (`Vec<u32>`) so
//! [`ferrofin_chromaprint`] can compare two episodes. The default backend shells
//! out to **`fpcalc`** (Chromaprint's standalone CLI) — the system ffmpeg on this
//! target is not built with the `chromaprint` muxer, so the newer Intro Skipper
//! `ffmpeg -f chromaprint` path is unavailable; `fpcalc` is what's installed.
//!
//! `fpcalc` fingerprints from the start of a file (with `-length`), so an intro
//! window (`start == 0`) is a single `fpcalc` call, while a credits window
//! (`start > 0`) is decoded to a temp WAV by ffmpeg first, then fingerprinted.

use std::process::Stdio;

use async_trait::async_trait;

/// Produces Chromaprint points for an audio window of a media file.
#[async_trait]
pub trait Fingerprinter: Send + Sync {
    /// Fingerprints `[start, end]` seconds of `path` into Chromaprint points, or
    /// an error string on any spawn/parse failure.
    async fn fingerprint(&self, path: &str, start: f64, end: f64) -> Result<Vec<u32>, String>;
}

/// A `fpcalc`-backed fingerprinter (with ffmpeg for windowed decodes).
#[derive(Debug, Clone)]
pub struct FpcalcFingerprinter {
    fpcalc: String,
    ffmpeg: String,
}

impl FpcalcFingerprinter {
    /// Builds the fingerprinter over the resolved `fpcalc` and `ffmpeg` paths.
    #[must_use]
    pub fn new(fpcalc: String, ffmpeg: String) -> Self {
        Self { fpcalc, ffmpeg }
    }
}

#[async_trait]
impl Fingerprinter for FpcalcFingerprinter {
    async fn fingerprint(&self, path: &str, start: f64, end: f64) -> Result<Vec<u32>, String> {
        let length = (end - start).max(1.0).ceil();
        if start <= 0.0 {
            // Intro window: fpcalc reads the file directly, limited by -length.
            let out = run(
                &self.fpcalc,
                &["-raw", "-length", &format!("{length:.0}"), path],
            )
            .await?;
            parse_fpcalc(&out)
        } else {
            // Credits window: decode [start, end] to a temp WAV, then fingerprint.
            let tmp = tempfile::Builder::new()
                .prefix("ferrofin-fp-")
                .suffix(".wav")
                .tempfile()
                .map_err(|e| format!("temp wav: {e}"))?;
            let tmp_path = tmp.path().to_string_lossy().into_owned();
            run(
                &self.ffmpeg,
                &[
                    "-y",
                    "-ss",
                    &format!("{start}"),
                    "-t",
                    &format!("{}", end - start),
                    "-i",
                    path,
                    "-ac",
                    "1",
                    "-vn",
                    "-sn",
                    "-dn",
                    "-f",
                    "wav",
                    &tmp_path,
                ],
            )
            .await?;
            // `-length` is required here too: fpcalc's default analysis length
            // is 120 s, which silently truncated the credits window to its
            // first two minutes — usually the final scene, not the credits —
            // and gutted the credits detection rate.
            let out = run(
                &self.fpcalc,
                &["-raw", "-length", &format!("{length:.0}"), &tmp_path],
            )
            .await?;
            parse_fpcalc(&out)
        }
    }
}

/// Runs `program args…`, returning stdout as a string, or an error carrying the
/// exit status + stderr on failure.
async fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("spawn {program}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parses `fpcalc -raw` output — the `FINGERPRINT=1,2,3,…` line — into points.
fn parse_fpcalc(output: &str) -> Result<Vec<u32>, String> {
    let line = output
        .lines()
        .find_map(|l| l.strip_prefix("FINGERPRINT="))
        .ok_or_else(|| "fpcalc output had no FINGERPRINT line".to_owned())?;
    let points: Vec<u32> = line
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().parse::<u32>())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("bad fpcalc point: {e}"))?;
    if points.is_empty() {
        return Err("fpcalc returned an empty fingerprint".to_owned());
    }
    Ok(points)
}

/// Locates `fpcalc` on `$PATH` (by running `fpcalc -version`), returning the
/// program name to invoke, or `None` when Chromaprint is not installed — in which
/// case the intro skipper loads but reports unavailable.
#[must_use]
pub fn discover_fpcalc() -> Option<String> {
    let ok = std::process::Command::new("fpcalc")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    ok.then(|| "fpcalc".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fpcalc_raw_output() {
        let out = "DURATION=25\nFINGERPRINT=1,2,3,4294967295\n";
        assert_eq!(parse_fpcalc(out).unwrap(), vec![1, 2, 3, u32::MAX]);
    }

    #[test]
    fn rejects_output_without_fingerprint() {
        assert!(parse_fpcalc("DURATION=25\n").is_err());
        assert!(parse_fpcalc("FINGERPRINT=\n").is_err());
    }

    #[test]
    fn rejects_non_numeric_points() {
        assert!(parse_fpcalc("FINGERPRINT=1,two,3\n").is_err());
    }

    #[tokio::test]
    async fn run_reports_a_missing_program() {
        let err = run("ferrofin-no-such-program", &[])
            .await
            .expect_err("spawn");
        assert!(err.starts_with("spawn ferrofin-no-such-program:"), "{err}");
    }

    #[test]
    fn discover_fpcalc_reports_presence_not_a_path() {
        // Either outcome is valid (fpcalc is optional); it must never invent one.
        assert!(discover_fpcalc().is_none_or(|p| p == "fpcalc"));
    }

    /// The real `Command` path, over stub `fpcalc`/`ffmpeg` scripts.
    ///
    /// One test, run in sequence: every stub is written before the first spawn.
    /// Writing an executable in one thread while another forks makes the child
    /// inherit the still-open write fd, and the exec then fails `ETXTBSY` — so
    /// these cases must not interleave with each other's script writes.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawned_fingerprinting_over_stub_programs() {
        use std::os::unix::fs::PermissionsExt;

        /// Writes an executable `#!/bin/sh` script into `dir`, returning its path.
        fn stub(dir: &std::path::Path, name: &str, body: &str) -> String {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
            path.to_string_lossy().into_owned()
        }

        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path().to_string_lossy().into_owned();
        // fpcalc records the `-length` it was given, so the assertions below can
        // prove the analysis window reaches it.
        let fpcalc = stub(
            dir.path(),
            "fpcalc",
            &format!(
                r#"while [ $# -gt 0 ]; do
  case "$1" in -length) echo "$2" > {root}/length ;; esac
  shift
done
echo "FINGERPRINT=1,2,3""#
            ),
        );
        let ffmpeg = stub(dir.path(), "ffmpeg", &format!("touch {root}/ffmpeg-ran"));
        let broken_ffmpeg = stub(dir.path(), "ffmpeg-broken", "exit 1");
        let boom = stub(dir.path(), "boom", "echo 'it broke' >&2; exit 3");
        let length = || {
            std::fs::read_to_string(dir.path().join("length"))
                .expect("length")
                .trim()
                .to_owned()
        };

        // A non-zero exit carries the status and stderr.
        let err = run(&boom, &[]).await.expect_err("non-zero exit");
        assert!(err.contains("exit"), "{err}");
        assert!(err.ends_with("it broke"), "{err}");

        // An intro window (start == 0) goes straight to fpcalc.
        let fp = FpcalcFingerprinter::new(fpcalc.clone(), ffmpeg);
        assert_eq!(
            fp.fingerprint("/media/a.mkv", 0.0, 90.0).await.expect("fp"),
            vec![1, 2, 3]
        );
        assert_eq!(length(), "90");
        assert!(!dir.path().join("ffmpeg-ran").exists());

        // A credits window is decoded to a temp WAV first, and fingerprinted
        // whole — not truncated to fpcalc's 120 s default, which used to gut
        // credits detection.
        assert_eq!(
            fp.fingerprint("/media/a.mkv", 1500.0, 1800.0)
                .await
                .expect("fp"),
            vec![1, 2, 3]
        );
        assert!(dir.path().join("ffmpeg-ran").exists());
        assert_eq!(length(), "300");

        // A failing decode fails the fingerprint rather than fingerprinting junk.
        let fp = FpcalcFingerprinter::new(fpcalc, broken_ffmpeg);
        assert!(
            fp.fingerprint("/media/a.mkv", 1500.0, 1800.0)
                .await
                .is_err()
        );
    }
}
