//! What Ferrofin writes down about an ffmpeg job whose input is a remote URL.
//!
//! ffmpeg must have the source's real URL, which can carry an account in its
//! userinfo, its query or its path. A job's `arguments` keep it; everything
//! written about the job says only the scheme and host (`LOGGING.md`, rule 5).
//! Upstream writes the command line and stderr as they are, into a log the
//! dashboard offers for download. No API response carries them, so this is log
//! text only.
//!
//! The arguments are the test: a job that reads no URL and sends no source
//! header is an ordinary file transcode, and everything about it is written
//! byte for byte.

use std::borrow::Cow;

use ferrofin_util::source_url::{is_remote_url, redact_url, redact_urls_in};

use super::progress::{FfmpegProgress, parse};

/// The longest stderr line kept, for the progress parser and a filtered log.
/// ffmpeg's own logger cuts a line well short of this.
const MAX_LINE_BYTES: usize = 65_536;

/// The options whose value is a `RequiredHttpHeaders` entry: a cookie or a
/// token can sit in any of them, and no rule tells one from a plain value.
const HEADER_FLAGS: [&str; 3] = ["-headers", "-referer", "-user_agent"];

/// What a log shows in place of a [`HEADER_FLAGS`] value.
const HIDDEN_VALUE: &str = "<redacted>";

/// Whether anything written about a job with these `arguments` needs filtering.
fn names_a_remote_source(arguments: &[String]) -> bool {
    arguments
        .iter()
        .any(|argument| is_remote_url(argument) || HEADER_FLAGS.contains(&argument.as_str()))
}

/// `arguments` as the job's log header and the `ffmpeg_args` debug line show
/// them: unchanged for an ordinary file transcode.
pub(super) fn shown_arguments(arguments: &[String]) -> Cow<'_, [String]> {
    if !names_a_remote_source(arguments) {
        return Cow::Borrowed(arguments);
    }
    let mut after_header_flag = false;
    let shown = arguments.iter().map(|argument| {
        let is_header = std::mem::replace(
            &mut after_header_flag,
            HEADER_FLAGS.contains(&argument.as_str()),
        );
        if is_header {
            HIDDEN_VALUE.to_owned()
        } else if is_remote_url(argument) {
            redact_url(argument)
        } else {
            redact_urls_in(argument).into_owned()
        }
    });
    Cow::Owned(shown.collect())
}

/// An argument line, as a process runner's error shows it: [`shown_arguments`]
/// once it splits, nothing after the first header flag when its quoting does
/// not balance, and unchanged for an ordinary file probe.
pub(crate) fn shown_argument_line(arguments: &str) -> Cow<'_, str> {
    let Some(tokens) = shlex::split(arguments) else {
        // Unbalanced quoting leaves no telling where a header value ends, so
        // nothing after the first header flag is shown.
        let header_end = HEADER_FLAGS
            .iter()
            .filter_map(|flag| arguments.find(flag).map(|at| at + flag.len()))
            .min();
        return match header_end {
            Some(end) => Cow::Owned(format!(
                "{} {HIDDEN_VALUE}",
                redact_urls_in(&arguments[..end])
            )),
            None => redact_urls_in(arguments),
        };
    };
    if tokens
        .iter()
        .any(|token| HEADER_FLAGS.contains(&token.as_str()))
    {
        Cow::Owned(shown_arguments(&tokens).join(" "))
    } else {
        redact_urls_in(arguments)
    }
}

/// Scrubs ffmpeg's stderr lines for a job reading a remote source.
#[derive(Debug)]
struct LineFilter {
    /// Each URL in the job's arguments and how it is shown. ffmpeg prints an
    /// input unquoted in places, where a space in it would end the URL
    /// [`redact_urls_in`] finds.
    inputs: Vec<(String, String)>,
}

impl LineFilter {
    /// The filter for a job's `arguments`, when it needs one.
    fn new(arguments: &[String]) -> Option<Self> {
        names_a_remote_source(arguments).then(|| Self {
            inputs: arguments
                .iter()
                .filter(|argument| is_remote_url(argument))
                .map(|url| (url.clone(), redact_url(url)))
                .collect(),
        })
    }

    /// `line` without a URL's credentials. Borrowed when nothing changed.
    fn scrub<'a>(&self, line: &'a str) -> Cow<'a, str> {
        let mut scrubbed = Cow::Borrowed(line);
        for (url, shown) in &self.inputs {
            if scrubbed.contains(url.as_str()) {
                scrubbed = Cow::Owned(scrubbed.replace(url.as_str(), shown));
            }
        }
        match scrubbed {
            Cow::Borrowed(line) => redact_urls_in(line),
            Cow::Owned(line) => Cow::Owned(redact_urls_in(&line).into_owned()),
        }
    }
}

/// Turns ffmpeg's stderr, a chunk at a time, into what the job's log gets, and
/// reads the progress statistics out of it.
///
/// ffmpeg ends its statistics with `\r` and everything else with `\n`, and
/// both stay in the log. A job with no [`LineFilter`] passes every byte through
/// as it arrives. With one, text waits for the end of its line, because a URL
/// can straddle two reads, so a line past [`MAX_LINE_BYTES`] loses its tail.
#[derive(Debug)]
pub(super) struct StderrPump {
    filter: Option<LineFilter>,
    line: Vec<u8>,
    progress: Option<FfmpegProgress>,
}

impl StderrPump {
    /// A pump for the job started with `arguments`.
    pub(super) fn new(arguments: &[String]) -> Self {
        Self {
            filter: LineFilter::new(arguments),
            line: Vec::new(),
            progress: None,
        }
    }

    /// Takes in one read of stderr and returns what the log gets for it.
    pub(super) fn feed<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        let mut shown = self
            .filter
            .is_some()
            .then(|| Vec::with_capacity(bytes.len()));
        for &byte in bytes {
            if byte == b'\r' || byte == b'\n' {
                self.end_line(shown.as_mut());
                if let Some(shown) = shown.as_mut() {
                    shown.push(byte);
                }
            } else if self.line.len() < MAX_LINE_BYTES {
                self.line.push(byte);
            }
        }
        shown.map_or(Cow::Borrowed(bytes), Cow::Owned)
    }

    /// Ends the stream and returns what the log gets for its unfinished line.
    pub(super) fn finish(&mut self) -> Vec<u8> {
        let mut shown = self.filter.is_some().then(Vec::new);
        self.end_line(shown.as_mut());
        shown.unwrap_or_default()
    }

    /// The statistics read since the last call, if any.
    pub(super) fn take_progress(&mut self) -> Option<FfmpegProgress> {
        self.progress.take()
    }

    /// Parses the finished line and, for a filtered job, appends it to `shown`.
    fn end_line(&mut self, shown: Option<&mut Vec<u8>>) {
        let text = String::from_utf8_lossy(&self.line);
        if let Some(parsed) = parse(&text) {
            self.progress = Some(parsed);
        }
        if let (Some(filter), Some(shown)) = (&self.filter, shown) {
            match filter.scrub(&text) {
                // Unchanged: the bytes ffmpeg wrote, even if they are not UTF-8.
                Cow::Borrowed(_) => shown.extend_from_slice(&self.line),
                Cow::Owned(line) => shown.extend_from_slice(line.as_bytes()),
            }
        }
        self.line.clear();
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::{StderrPump, shown_argument_line, shown_arguments};

    const SOURCE: &str = "http://USER:SECRET@127.0.0.1:18275/live/USER/SECRET/1.m3u8?token=SECRET";
    const SHOWN: &str = "http://127.0.0.1:18275";

    fn strings(arguments: &[&str]) -> Vec<String> {
        arguments.iter().map(|&a| a.to_owned()).collect()
    }

    /// Everything the log gets when stderr arrives in `chunks`.
    fn log_of(arguments: &[String], chunks: &[&[u8]]) -> Vec<u8> {
        let mut pump = StderrPump::new(arguments);
        let mut log = Vec::new();
        for chunk in chunks {
            log.extend_from_slice(&pump.feed(chunk));
        }
        log.extend_from_slice(&pump.finish());
        log
    }

    /// An ordinary file transcode's text is not rebuilt at all, even where a
    /// tag or a file name holds something that looks like a URL.
    #[rstest]
    #[case::a_plain_file(&["-i", "file:/media/movie.mkv", "-c:v", "libx264", "/t/out.m3u8"])]
    #[case::a_url_in_a_file_name(&["-i", "file:/media/see http://x.example/a.mkv"])]
    #[case::a_url_in_metadata(&["-metadata", "comment=https://u:p@x.example/", "-i", "/m.mkv"])]
    #[case::a_file_url(&["-i", "file:///media/movie.mkv"])]
    fn an_ordinary_job_is_written_byte_for_byte(#[case] arguments: &[&str]) {
        let arguments = strings(arguments);
        assert!(
            matches!(shown_arguments(&arguments), std::borrow::Cow::Borrowed(_)),
            "{arguments:?}"
        );

        let stderr: &[u8] = b"  comment: https://user:pw@x.example/a\nbad \xff byte\r\rframe=1\r\n";
        let mut pump = StderrPump::new(&arguments);
        assert!(matches!(pump.feed(stderr), std::borrow::Cow::Borrowed(_)));
        assert_eq!(pump.finish(), Vec::<u8>::new());
    }

    /// `-i` keeps a job's real URL in `arguments`; only the shown copy changes.
    #[test]
    fn the_input_url_is_shown_by_its_scheme_and_host() {
        let arguments = strings(&["-analyzeduration", "3000000", "-i", SOURCE, "-c:v", "copy"]);
        assert_eq!(
            *shown_arguments(&arguments),
            strings(&["-analyzeduration", "3000000", "-i", SHOWN, "-c:v", "copy"])
        );
        assert_eq!(arguments[3], SOURCE);
    }

    /// A required header can carry a cookie or a token, with or without a URL
    /// for the job to read: a buffered channel reads a local file.
    #[rstest]
    #[case::user_agent("-user_agent", "Box/1.0 token=SECRET")]
    #[case::referer("-referer", "http://portal.example/SECRET")]
    #[case::headers("-headers", "Cookie: SECRET\r\n")]
    fn a_source_header_is_not_shown(#[case] flag: &str, #[case] value: &str) {
        for input in [SOURCE, "/transcodes/buffer.ts"] {
            let arguments = strings(&[flag, value, "-i", input]);
            let shown = shown_arguments(&arguments).join(" ");
            assert!(!shown.contains("SECRET"), "{shown}");
            assert!(
                shown.starts_with(&format!("{flag} <redacted> -i ")),
                "{shown}"
            );
        }
    }

    /// A process runner's error line keeps an ordinary probe as it is, and a
    /// remote one without header flags as `redact_urls_in` shows it (gl-1407).
    #[rstest]
    #[case::file(
        r#"-i file:"/media/a b.mkv" -f null -"#,
        r#"-i file:"/media/a b.mkv" -f null -"#
    )]
    #[case::url(&format!(r#"-i "{SOURCE}" -f null -"#), &format!(r#"-i "{SHOWN}" -f null -"#))]
    fn a_line_without_header_flags_keeps_its_text(#[case] line: &str, #[case] shown: &str) {
        assert_eq!(shown_argument_line(line), shown);
    }

    /// A URL inside a longer argument goes too (`movie=` in a filter graph).
    #[test]
    fn a_url_inside_an_argument_is_redacted() {
        let arguments = strings(&["-i", SOURCE, "-vf", "movie=http://u:SECRET@h.example/a.png"]);
        assert_eq!(shown_arguments(&arguments)[3], "movie=http://h.example");
    }

    /// The lines the issue's reproduction and an HLS source print, with the
    /// log's line ends kept and the input not the only URL filtered.
    #[test]
    fn stderr_lines_lose_every_url_but_keep_their_shape() {
        let stderr = format!(
            "[http @ 0x1] HTTP error 500 Internal Server Error\n\
             Error opening input file {SOURCE}.\n\
             Input #0, hls, from '{SOURCE}':\n\
             [hls @ 0x1] Opening 'http://USER:SECRET@cdn.example/live/SECRET/2.ts' for reading\n\
             [tcp @ 0x1] Connection to tcp://127.0.0.1:18275 failed: Connection refused\n\
             frame=  10 fps=0.0 time=00:00:01.00 speed=1.0x\rframe=  20\r\n"
        );
        let log = log_of(&strings(&["-i", SOURCE]), &[stderr.as_bytes()]);
        assert_eq!(
            String::from_utf8(log).unwrap(),
            "[http @ 0x1] HTTP error 500 Internal Server Error\n\
             Error opening input file http://127.0.0.1:18275.\n\
             Input #0, hls, from 'http://127.0.0.1:18275':\n\
             [hls @ 0x1] Opening 'http://cdn.example' for reading\n\
             [tcp @ 0x1] Connection to tcp://127.0.0.1:18275 failed: Connection refused\n\
             frame=  10 fps=0.0 time=00:00:01.00 speed=1.0x\rframe=  20\r\n"
        );
    }

    /// A read can end in the middle of a URL, in any place.
    #[test]
    fn a_url_straddling_two_reads_is_still_found() {
        let arguments = strings(&["-i", SOURCE]);
        let line = format!("Error opening input file {SOURCE}.\n");
        let bytes = line.as_bytes();
        for split in 1..bytes.len() {
            let log = log_of(&arguments, &[&bytes[..split], &bytes[split..]]);
            assert_eq!(
                log, b"Error opening input file http://127.0.0.1:18275.\n",
                "split at {split}"
            );
        }
    }

    /// ffmpeg prints the input bare here, so only the exact string finds all
    /// of a URL with a space in it.
    #[test]
    fn an_input_with_a_space_is_replaced_whole() {
        let source = "http://h.example/live/my SECRET/1.m3u8";
        let log = log_of(
            &strings(&["-i", source]),
            &[format!("Error opening input file {source}.\n").as_bytes()],
        );
        assert_eq!(log, b"Error opening input file http://h.example.\n");
    }

    /// The last line has no end when ffmpeg dies mid-write.
    #[test]
    fn an_unfinished_last_line_is_filtered_too() {
        let log = log_of(
            &strings(&["-i", SOURCE]),
            &[format!("{SOURCE} bye").as_bytes()],
        );
        assert_eq!(log, b"http://127.0.0.1:18275 bye");
    }

    /// A line with nothing to redact keeps ffmpeg's bytes, `UTF-8` or not.
    #[test]
    fn a_filtered_job_keeps_a_clean_lines_bytes() {
        let stderr: &[u8] = b"title: caf\xe9 \r\nStream #0:0: Video: h264\n";
        assert_eq!(log_of(&strings(&["-i", SOURCE]), &[stderr]), stderr);
    }

    /// A line longer than anything ffmpeg writes is cut, never written raw.
    #[test]
    fn an_oversized_line_is_cut_and_still_filtered() {
        let stderr = format!("{SOURCE} {}\n", "x".repeat(super::MAX_LINE_BYTES));
        let log = log_of(&strings(&["-i", SOURCE]), &[stderr.as_bytes()]);
        let log = String::from_utf8(log).unwrap();
        assert!(!log.contains("SECRET"));
        assert!(log.len() <= super::MAX_LINE_BYTES + 1, "{}", log.len());
        assert!(log.ends_with('\n'));
    }

    /// The statistics parser reads the same lines with or without a filter, a
    /// line ending in `\r` or left unfinished at the end of the stream.
    #[rstest]
    #[case::ordinary(&["-i", "/m.mkv"])]
    #[case::remote(&["-i", SOURCE])]
    fn progress_is_read_either_way(#[case] arguments: &[&str]) {
        let mut pump = StderrPump::new(&strings(arguments));
        let _ = pump.feed(b"frame=   10 fps=25.0 time=00:0");
        assert!(pump.take_progress().is_none());
        let _ = pump.feed(b"0:01.00 speed=1.0x\r");
        assert!(pump.take_progress().is_some());
        assert!(pump.take_progress().is_none());
        let _ = pump.feed(b"frame=   20 fps=25.0 time=00:00:02.00 speed=1.0x");
        assert!(pump.take_progress().is_none());
        let _ = pump.finish();
        assert!(pump.take_progress().is_some());
    }
}
