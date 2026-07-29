//! Audio fingerprinting seam + backends.
//!
//! Turns an audio window of a media file into Chromaprint points (`Vec<u32>`) so
//! [`hermit_chromaprint`] can compare two episodes. The default backend shells
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
                .prefix("hermit-fp-")
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
}
