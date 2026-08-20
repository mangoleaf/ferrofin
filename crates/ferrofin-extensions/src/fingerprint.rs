//! Audio fingerprinting seam + backends.
//!
//! Turns an audio window of a media file into Chromaprint points (`Vec<u32>`) so
//! [`ferrofin_chromaprint`] can compare two episodes.
//!
//! Preferred backend: **`ffmpeg -f chromaprint`** (upstream Intro Skipper's own
//! path) — one pass that seeks, decodes and fingerprints the window, against
//! whatever libchromaprint the ffmpeg build links. The release image's
//! jellyfin-ffmpeg is built `--enable-chromaprint`, so that is a current
//! Chromaprint rather than the 1.5.1 Debian bookworm packages.
//!
//! Fallback for an ffmpeg without the muxer: **`fpcalc`** (Chromaprint's CLI),
//! which fingerprints from the start of a file (with `-length`) — so an intro
//! window (`start == 0`) is a single `fpcalc` call, while a credits window
//! (`start > 0`) is decoded to a temp WAV by ffmpeg first, then fingerprinted.
//! Both backends emit the same points for the same window.

use std::path::PathBuf;
use std::process::{Output, Stdio};

use async_trait::async_trait;

/// Produces Chromaprint points for an audio window of a media file.
#[async_trait]
pub trait Fingerprinter: Send + Sync {
    /// Fingerprints `[start, end]` seconds of `path` into Chromaprint points, or
    /// an error string on any spawn/parse failure.
    async fn fingerprint(&self, path: &str, start: f64, end: f64) -> Result<Vec<u32>, String>;
}

/// A Chromaprint fingerprinter over `ffmpeg -f chromaprint`, or `fpcalc` when
/// the ffmpeg build lacks that muxer.
#[derive(Debug, Clone)]
pub struct ChromaprintFingerprinter {
    /// `fpcalc` on `$PATH`, or `None` when only the ffmpeg muxer is available.
    fpcalc: Option<String>,
    ffmpeg: String,
    /// Whether `ffmpeg` has the `chromaprint` muxer (the preferred backend).
    ffmpeg_chromaprint: bool,
    /// Where the `fpcalc` fallback's intermediate WAV is written. Defaults to
    /// the system temp dir; the composition root points it at the extension
    /// cache so the decode does not depend on the size of a container's `/tmp`
    /// (a full or read-only `/tmp` made every credits fingerprint fail, and
    /// with it every "Skip Credits" button).
    scratch_dir: Option<PathBuf>,
}

impl ChromaprintFingerprinter {
    /// Picks the best backend available for `ffmpeg`, or `None` when neither the
    /// `chromaprint` muxer nor `fpcalc` is present — the intro skipper then
    /// reports unavailable.
    #[must_use]
    pub fn discover(ffmpeg: &str) -> Option<Self> {
        Self::with_ffmpeg_chromaprint(ffmpeg, ffmpeg_has_chromaprint(ffmpeg))
    }

    /// [`Self::discover`] for a caller that ALREADY knows whether this ffmpeg
    /// has the `chromaprint` muxer.
    ///
    /// The composition root probes `ffmpeg -muxers` during encoder discovery
    /// anyway; re-deriving it here meant a second blocking `ffmpeg` spawn on
    /// the startup critical path (34 ms of an 88 ms cold start, measured).
    /// Backend selection is otherwise identical to [`Self::discover`].
    #[must_use]
    pub fn with_ffmpeg_chromaprint(ffmpeg: &str, ffmpeg_chromaprint: bool) -> Option<Self> {
        // `fpcalc` is only consulted when the (preferred) ffmpeg muxer is
        // absent, so the common case spawns nothing at all.
        let fpcalc = if ffmpeg_chromaprint {
            None
        } else {
            discover_fpcalc()
        };
        (ffmpeg_chromaprint || fpcalc.is_some()).then(|| Self {
            fpcalc,
            ffmpeg: ffmpeg.to_owned(),
            ffmpeg_chromaprint,
            scratch_dir: None,
        })
    }

    /// Writes the `fpcalc` fallback's intermediate WAV under `dir` instead of
    /// the system temp dir.
    #[must_use]
    pub fn with_scratch_dir(mut self, dir: PathBuf) -> Self {
        self.scratch_dir = Some(dir);
        self
    }

    /// The backend that will be used, for logs and the support bundle.
    #[must_use]
    pub fn backend(&self) -> &'static str {
        if self.ffmpeg_chromaprint {
            "ffmpeg -f chromaprint"
        } else {
            "fpcalc"
        }
    }
}

#[async_trait]
impl Fingerprinter for ChromaprintFingerprinter {
    async fn fingerprint(&self, path: &str, start: f64, end: f64) -> Result<Vec<u32>, String> {
        let length = (end - start).max(1.0).ceil();
        if self.ffmpeg_chromaprint {
            // One pass: ffmpeg seeks, decodes and fingerprints the window, and
            // writes the raw points (little-endian u32) to stdout. No temp WAV,
            // and no dependence on a separately packaged Chromaprint.
            let out = output(
                &self.ffmpeg,
                &[
                    "-v",
                    "error",
                    "-ss",
                    &format!("{start}"),
                    "-t",
                    &format!("{length:.0}"),
                    "-i",
                    path,
                    "-ac",
                    "1",
                    "-vn",
                    "-sn",
                    "-dn",
                    "-f",
                    "chromaprint",
                    "-fp_format",
                    "raw",
                    "-",
                ],
            )
            .await?;
            check_status(&self.ffmpeg, &out)?;
            return parse_raw_points(&out.stdout);
        }
        let Some(fpcalc) = &self.fpcalc else {
            return Err("no Chromaprint backend".to_owned());
        };
        if start <= 0.0 {
            // Intro window: fpcalc reads the file directly, limited by -length.
            fpcalc_points(fpcalc, &["-raw", "-length", &format!("{length:.0}"), path]).await
        } else {
            // Credits window: decode [start, end] to a temp WAV, then fingerprint.
            let mut builder = tempfile::Builder::new();
            builder.prefix("ferrofin-fp-").suffix(".wav");
            let tmp = match &self.scratch_dir {
                Some(dir) => {
                    std::fs::create_dir_all(dir)
                        .map_err(|e| format!("scratch dir {}: {e}", dir.display()))?;
                    builder.tempfile_in(dir)
                }
                None => builder.tempfile(),
            }
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
                    // Chromaprint resamples to 11025 Hz mono internally, so
                    // decoding at that rate loses nothing and cuts the
                    // intermediate WAV ~4× (a 480 s window: ~42 MB → ~10 MB).
                    "-ar",
                    "11025",
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
            fpcalc_points(
                fpcalc,
                &["-raw", "-length", &format!("{length:.0}"), &tmp_path],
            )
            .await
        }
    }
}

/// Runs `program args…`, returning its captured output, or an error when it
/// could not be spawned at all.
async fn output(program: &str, args: &[&str]) -> Result<Output, String> {
    tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("spawn {program}: {e}"))
}

/// Fails on a non-zero exit, carrying the status + stderr.
fn check_status(program: &str, out: &Output) -> Result<(), String> {
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "{program} exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// Runs `program args…`, returning stdout as a string, or an error carrying the
/// exit status + stderr on failure.
async fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = output(program, args).await?;
    check_status(program, &out)?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs `fpcalc args…` and parses its points.
///
/// A fingerprint on stdout wins over a non-zero exit: Chromaprint 1.5.1 prints
/// the complete fingerprint and *then* dies with
/// "Error decoding audio frame (End of file)" (exit 3) whenever `-length`
/// reaches the end of the stream — which a windowed credits WAV always does.
async fn fpcalc_points(fpcalc: &str, args: &[&str]) -> Result<Vec<u32>, String> {
    let out = output(fpcalc, args).await?;
    match parse_fpcalc(&String::from_utf8_lossy(&out.stdout)) {
        Ok(points) => Ok(points),
        Err(parse_err) => {
            check_status(fpcalc, &out)?;
            Err(parse_err)
        }
    }
}

/// Decodes `ffmpeg -f chromaprint -fp_format raw` output: little-endian `u32`
/// points.
fn parse_raw_points(bytes: &[u8]) -> Result<Vec<u32>, String> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return Err(format!("chromaprint muxer wrote {} bytes", bytes.len()));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Whether `ffmpeg` was built with the `chromaprint` muxer.
fn ffmpeg_has_chromaprint(ffmpeg: &str) -> bool {
    std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-muxers"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("chromaprint")
        })
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

    #[test]
    fn decodes_the_chromaprint_muxers_raw_points() {
        let mut bytes = 1u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_raw_points(&bytes).unwrap(), vec![1, u32::MAX]);
        // Nothing, or a truncated point, is a failure — never a short fingerprint.
        assert!(parse_raw_points(&[]).is_err());
        assert!(parse_raw_points(&bytes[..5]).is_err());
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

    #[test]
    fn pre_probed_muxer_picks_ffmpeg_without_re_probing() {
        // The composition root passes the `-muxers` answer it already has, so
        // this path must NOT depend on the named ffmpeg existing — that second
        // spawn is exactly what it removes from the startup critical path.
        let fp = ChromaprintFingerprinter::with_ffmpeg_chromaprint("ferrofin-no-such-ffmpeg", true)
            .expect("a reported chromaprint muxer is a usable backend");
        assert_eq!(fp.backend(), "ffmpeg -f chromaprint");
    }

    #[test]
    fn pre_probed_absent_muxer_matches_discovery() {
        // Told the muxer is absent, selection must land wherever `discover`
        // lands on this host — fpcalc if present, no fingerprinter otherwise.
        let fp =
            ChromaprintFingerprinter::with_ffmpeg_chromaprint("ferrofin-no-such-ffmpeg", false);
        assert_eq!(fp.is_some(), discover_fpcalc().is_some());
        assert!(fp.is_none_or(|f| f.backend() == "fpcalc"));
    }

    #[test]
    fn discovery_needs_one_backend_or_none() {
        // A missing ffmpeg leaves only fpcalc — and when that is absent too,
        // the intro skipper gets no fingerprinter rather than a broken one.
        let fp = ChromaprintFingerprinter::discover("ferrofin-no-such-ffmpeg");
        assert_eq!(fp.is_some(), discover_fpcalc().is_some());
        assert!(fp.is_none_or(|f| f.backend() == "fpcalc"));
    }

    /// The real `Command` path, over stub `fpcalc`/`ffmpeg` scripts.
    ///
    /// One test, run in sequence: every stub is written before the first spawn.
    /// Writing an executable in one thread while another forks makes the child
    /// inherit the still-open write fd, and the exec then fails `ETXTBSY` — so
    /// these cases must not interleave with each other's script writes.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one sequential test, see above
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
        // Chromaprint 1.5.1 prints the whole fingerprint and *then* fails at
        // EOF; the points must still be used.
        let fpcalc_eof = stub(
            dir.path(),
            "fpcalc-eof",
            "echo 'FINGERPRINT=1,2,3'\necho 'ERROR: Error decoding audio frame (End of file)' >&2\nexit 3",
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
        // The `fpcalc` fallback: an ffmpeg without the `chromaprint` muxer.
        let fallback = |fpcalc: &str, ffmpeg: &str| ChromaprintFingerprinter {
            fpcalc: Some(fpcalc.to_owned()),
            ffmpeg: ffmpeg.to_owned(),
            ffmpeg_chromaprint: false,
            scratch_dir: None,
        };

        // A non-zero exit carries the status and stderr.
        let err = run(&boom, &[]).await.expect_err("non-zero exit");
        assert!(err.contains("exit"), "{err}");
        assert!(err.ends_with("it broke"), "{err}");

        // Preferred backend: one ffmpeg pass emitting raw little-endian points,
        // with no fpcalc call and no temp WAV at all.
        let ffmpeg_cp = stub(
            dir.path(),
            "ffmpeg-chromaprint",
            r"printf '\001\000\000\000\002\000\000\000'",
        );
        let fp = ChromaprintFingerprinter {
            fpcalc: None,
            ffmpeg: ffmpeg_cp,
            ffmpeg_chromaprint: true,
            scratch_dir: None,
        };
        assert_eq!(fp.backend(), "ffmpeg -f chromaprint");
        assert_eq!(
            fp.fingerprint("/media/a.mkv", 1500.0, 1800.0)
                .await
                .expect("fp"),
            vec![1, 2]
        );

        // An intro window (start == 0) goes straight to fpcalc.
        let fp = fallback(&fpcalc, &ffmpeg);
        assert_eq!(fp.backend(), "fpcalc");
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

        // …and an fpcalc that dies at EOF after printing the fingerprint still
        // yields it (chromaprint 1.5.1 on any window that reaches end-of-stream).
        assert_eq!(
            fallback(&fpcalc_eof, &ffmpeg)
                .fingerprint("/media/a.mkv", 1500.0, 1800.0)
                .await
                .expect("fp"),
            vec![1, 2, 3]
        );

        // A failing decode fails the fingerprint rather than fingerprinting junk.
        assert!(
            fallback(&fpcalc, &broken_ffmpeg)
                .fingerprint("/media/a.mkv", 1500.0, 1800.0)
                .await
                .is_err()
        );

        // The credits decode writes its intermediate WAV under the configured
        // scratch dir (created on demand) instead of the system temp dir — a
        // container's /tmp is routinely small or read-only, and a failed decode
        // there costs every "Skip Credits" segment. Nothing is left behind.
        let scratch = dir.path().join("scratch/nested");
        let ffmpeg_probe = stub(
            dir.path(),
            "ffmpeg-probe",
            &format!(
                r#"for a in "$@"; do case "$a" in *.wav) echo "$a" > {root}/wav-path ;; esac; done"#
            ),
        );
        let fp = fallback(&fpcalc, &ffmpeg_probe).with_scratch_dir(scratch.clone());
        fp.fingerprint("/media/a.mkv", 1500.0, 1800.0)
            .await
            .expect("fp");
        let wav = std::fs::read_to_string(dir.path().join("wav-path")).expect("wav path");
        assert!(
            std::path::Path::new(wav.trim()).starts_with(&scratch),
            "decoded under the scratch dir: {wav}"
        );
        assert!(
            std::fs::read_dir(&scratch)
                .expect("scratch dir")
                .next()
                .is_none(),
            "the intermediate WAV is cleaned up"
        );
    }
}
