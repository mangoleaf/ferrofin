//! M3U tuner-playlist parser.
//!
//! Ported from Jellyfin's `Emby.Server.Implementations/LiveTv/TunerHosts/M3uParser.cs`.
//! An M3U tuner exposes its channel lineup as an extended-M3U playlist: each
//! channel is one `#EXTINF:` header (carrying `tvg-*`/`group-title` attributes
//! and a display name) followed by the stream URL on the next non-comment line.
//!
//! ```text
//! #EXTM3U
//! #EXTINF:-1 tvg-id="BBCOne.uk" tvg-chno="1" tvg-logo="http://x/bbc1.png" group-title="UK",BBC One
//! http://tuner/stream/bbc1.ts
//! ```

use std::collections::HashMap;
use std::sync::LazyLock;

use ferrofin_util::stream_extensions::LineReader;
use regex::Regex;

/// A single channel parsed from an M3U tuner playlist.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct M3uChannel {
    /// Stable channel id — the `tvg-id`/`channel-id`/`CUID` attribute when
    /// present, otherwise the `#EXTINF` display text after the comma (as
    /// written, before the SatIP prefix is stripped from [`name`](Self::name)). Also the key the XMLTV guide
    /// matches its `channel id` against.
    pub id: String,
    /// The display name (`M3uParser.GetChannelName`): the text after the comma
    /// on the `#EXTINF` line without a SatIP `NN.` prefix, else `tvg-name`,
    /// else `tvg-id`.
    pub name: String,
    /// The channel number (`M3uParser.GetChannelNumber`): a numeric
    /// `tvg-chno`, a numeric `tvg-id` (or `channel-id` next to a `tvg-id`), a
    /// SatIP `NN.` name prefix, or a numeric stream file name.
    pub number: Option<String>,
    /// Logo/artwork URL (`tvg-logo`, else `logo`), when present.
    pub logo: Option<String>,
    /// The `group-title` category, when present.
    pub group: Option<String>,
    /// `true` when the entry is flagged `radio="true"` (audio-only).
    pub is_radio: bool,
    /// The stream URL that plays the channel.
    pub url: String,
}

/// Parses the body of an M3U tuner playlist into its channel list: the
/// playlist's bytes through [`M3uParser`].
///
/// The text is read as a .NET `StreamReader` reads the bytes, so a line ends
/// at `\n`, `\r` or `\r\n` and a leading byte-order mark is dropped.
#[must_use]
pub fn parse_m3u(content: &str) -> Vec<M3uChannel> {
    let mut parser = M3uParser::new();
    parser.push(content.as_bytes());
    parser.finish()
}

/// Reads an M3U tuner playlist into its channel list as its bytes arrive.
///
/// Port of `M3uParser.Parse`/`GetChannelsAsync` (v12.2 M3uParser.cs:39-46,72-114),
/// which reads the playlist with `new StreamReader(stream)` and
/// `ReadAllLinesAsync`: only the current line and the channels so far are
/// held, never the whole playlist. The text is decoded as that reader decodes
/// it (see [`LineReader`]).
///
/// Lines that are not `#EXTINF`/URL pairs (including `#EXTM3U`, blank lines and
/// unknown `#`-directives) are skipped, matching Jellyfin's lenient parsing. An
/// `#EXTINF` with no following URL line is dropped, and so is an entry whose
/// URL line fails [`is_valid_channel_url`].
#[derive(Debug, Default)]
pub struct M3uParser {
    /// Splits the bytes into lines.
    lines: LineReader,
    /// What has been read so far.
    state: ParseState,
}

/// The lines an [`M3uParser`] has read, folded.
#[derive(Debug, Default)]
struct ParseState {
    /// The channels found so far.
    channels: Vec<M3uChannel>,
    /// The `#EXTINF` text awaiting its URL line: the number can come from the
    /// URL, so the entry is parsed once both are known.
    pending: Option<String>,
    /// How many entries were dropped for an invalid stream URL.
    skipped: usize,
}

impl M3uParser {
    /// A parser at the start of a playlist.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads `chunk`, the playlist's next bytes.
    pub fn push(&mut self, chunk: &[u8]) {
        let state = &mut self.state;
        self.lines.push(chunk, |line| state.push_line(line));
    }

    /// Ends the playlist and answers its channels.
    #[must_use]
    pub fn finish(self) -> Vec<M3uChannel> {
        let mut state = self.state;
        self.lines.finish(|line| state.push_line(line));
        if state.skipped > 0 {
            tracing::warn!(
                skipped = state.skipped,
                "live tv: skipped M3U channel entries with invalid stream URLs"
            );
        }
        state.channels
    }
}

impl ParseState {
    /// Reads one line of the playlist (`GetChannelsAsync`'s loop body).
    fn push_line(&mut self, raw: &str) {
        let line = raw.trim();
        if line.is_empty() {
            return;
        }
        if let Some(rest) = line
            .get(..EXTINF.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(EXTINF))
            .map(|_| line[EXTINF.len()..].trim())
        {
            // `string.IsNullOrWhiteSpace(extInf)`: an empty header waits for none.
            self.pending = Some(rest.to_owned()).filter(|rest| !rest.is_empty());
        } else if line.starts_with('#') {
            // #EXTM3U / #EXTVLCOPT / #EXTGRP / other directives — ignored.
        } else if let Some(extinf) = self.pending.take() {
            // `M3uParser.GetChannelsAsync` drops the entry and clears the
            // pending `#EXTINF` (`take` already did). Upstream warns per entry;
            // that volume scales with the playlist, so one count is warned in
            // `finish`.
            if !is_valid_channel_url(line) {
                tracing::debug!(
                    path = line,
                    "live tv: skipping M3U channel entry with an invalid stream URL"
                );
                self.skipped += 1;
                return;
            }
            self.channels.push(parse_extinf(&extinf, line));
        }
        // A bare URL with no preceding #EXTINF is ignored (no metadata).
    }
}

/// `M3uParser.ExtInfPrefix`, matched case-insensitively.
const EXTINF: &str = "#EXTINF:";

/// Whether an M3U entry's URL line is one a channel may play from.
///
/// Port of `M3uParser.IsValidChannelUrl` (upstream fix for
/// GHSA-8fw7-f233-ffr8): an absolute `http`, `https`, `rtsp`, `rtp` or `udp`
/// URL, scheme compared case-insensitively. Anything else (a local path,
/// `file://`, a relative path) would be read as a file and served by the
/// anonymous stream routes.
///
/// Two checks are stricter than .NET's `Uri.TryCreate`, and neither drops a
/// real stream URL. The line must start with `scheme://`, so
/// [`path_protocol`](crate::stream::path_protocol) never reads an accepted line
/// as a file path. It must also hold no `"`, `\` or control character (never
/// valid unescaped in a URL), because the raw line reaches ffmpeg inside a
/// quoted argument.
#[must_use]
pub fn is_valid_channel_url(url: &str) -> bool {
    url.split_once("://").is_some_and(|(scheme, _)| {
        ["http", "https", "rtsp", "rtp", "udp"]
            .iter()
            .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
    }) && !url.contains(['"', '\\'])
        && !url.contains(|c: char| c.is_control())
        && reqwest::Url::parse(url).is_ok()
}

/// `M3uParser.KeyValueRegex`: one `key="value"` attribute, matched
/// case-insensitively.
static KEY_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)([a-z0-9\-_]+)="([^"]+)""#).expect("KeyValueRegex pattern is valid")
});

/// Port of `M3uParser.GetChannelInfo` over the text after `#EXTINF:` and the
/// entry's stream URL.
///
/// The display text is what follows the first comma once the attributes are
/// removed, read only when the header has more than one non-empty comma part
/// (`nameParts.Length > 1`), with surrounding commas trimmed. Upstream keeps
/// only the last comma-separated part, which cuts a name such as
/// "Movies, Series HD" down to "Series HD"; that is not ported.
/// The id is not upstream's either: it stays the `tvg-id`, `channel-id` or
/// `CUID`, else the display text, so the guide keys and mappings existing
/// tuners carry keep matching.
fn parse_extinf(extinf: &str, url: &str) -> M3uChannel {
    let (attrs, remaining) = parse_attributes(extinf);
    let after_comma = remaining.split_once(',').map(|(_, text)| text.trim());
    let display = after_comma
        .filter(|_| remaining.split(',').filter(|part| !part.is_empty()).count() > 1)
        .map(|text| text.trim_matches(|c: char| c == ',' || c.is_whitespace()))
        .filter(|text| !text.is_empty());
    let attr = |key: &str| attrs.get(key).filter(|v| !v.trim().is_empty()).cloned();
    M3uChannel {
        id: attr("tvg-id")
            .or_else(|| attr("channel-id"))
            .or_else(|| attr("cuid"))
            .or_else(|| after_comma.filter(|t| !t.is_empty()).map(str::to_owned))
            .or_else(|| attr("tvg-name"))
            .unwrap_or_default(),
        name: display
            .map(|text| satip_split(text).map_or(text, |(_, name)| name))
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| attr("tvg-name"))
            .or_else(|| attr("tvg-id"))
            .unwrap_or_default(),
        number: channel_number(display, &attrs, url),
        logo: attr("tvg-logo").or_else(|| attr("logo")),
        group: attr("group-title"),
        is_radio: attrs
            .get("radio")
            .is_some_and(|v| v.eq_ignore_ascii_case("true")),
        url: url.to_owned(),
    }
}

/// Port of `M3uParser.ParseExtInf`: the attributes by lower-cased key (C#'s
/// `OrdinalIgnoreCase` dictionary, a later key winning) and the line with
/// every matched `key="value"` removed.
fn parse_attributes(line: &str) -> (HashMap<String, String>, String) {
    let mut attrs = HashMap::new();
    let mut remaining = line.to_owned();
    for found in KEY_VALUE.captures_iter(line) {
        attrs.insert(found[1].to_ascii_lowercase(), found[2].to_owned());
        remaining = remaining.replace(&found[0], "");
    }
    (attrs, remaining)
}

/// Port of `M3uParser.GetChannelNumber`. The nesting is upstream's: a
/// `channel-id` is read only next to a non-numeric `tvg-id`, and the SatIP
/// prefix only when nothing has been found yet.
fn channel_number(
    display: Option<&str>,
    attrs: &HashMap<String, String>,
    url: &str,
) -> Option<String> {
    let numeric = |key: &str| attrs.get(key).filter(|v| is_double(v)).cloned();
    let mut number = numeric("tvg-chno");
    if !number.as_deref().is_some_and(is_valid_channel_number) {
        if attrs.contains_key("tvg-id")
            && let Some(id) = numeric("tvg-id").or_else(|| numeric("channel-id"))
        {
            number = Some(id);
        }
        if number.as_deref().is_none_or(|n| n.trim().is_empty()) {
            number = display.and_then(satip_split).map(|(n, _)| n.to_owned());
        }
    }
    if let Some(number) = number.filter(|n| is_valid_channel_number(n)) {
        return Some(number.trim().to_owned());
    }
    // `Path.GetFileNameWithoutExtension(mediaUrl.RightPart('/'))`.
    let file = url.rsplit('/').next().unwrap_or(url);
    let stem = file.rfind('.').map_or(file, |dot| &file[..dot]);
    is_valid_channel_number(stem).then(|| stem.to_owned())
}

/// The SatIP form `GetChannelName` and `GetChannelNumber` both read
/// (`#EXTINF:0,84. VOX Schweiz`, `#EXTINF:0,84.0 - VOX Schweiz`): text before
/// the first space that, with `.` trimmed, is a number, then the name with
/// ` ` and `-` trimmed.
fn satip_split(text: &str) -> Option<(&str, &str)> {
    let space = text.find(' ').filter(|&at| at > 0)?;
    let number = text[..space].trim_matches([' ', '.']);
    is_double(number).then(|| (number, text[space + 1..].trim_matches([' ', '-'])))
}

/// `double.TryParse(value, CultureInfo.InvariantCulture, out _)`, through
/// Rust's float parser. They agree on channel numbers; at the edges Rust also
/// reads `inf`/`infinity`/`nan` in any case where .NET reads `Infinity` and
/// `NaN`, and Rust rejects .NET's thousands separators.
fn is_double(value: &str) -> bool {
    value.trim().parse::<f64>().is_ok()
}

/// Port of `M3uParser.IsValidChannelNumber`: a number that is not `-1` or `0`.
fn is_valid_channel_number(value: &str) -> bool {
    !value.trim().is_empty() && value != "-1" && value != "0" && is_double(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_attributes_and_name_and_url() {
        let m3u = "#EXTM3U\n\
            #EXTINF:-1 tvg-id=\"BBCOne.uk\" tvg-chno=\"1\" tvg-logo=\"http://x/bbc1.png\" group-title=\"UK\",BBC One\n\
            http://tuner/stream/bbc1.ts\n";
        let channels = parse_m3u(m3u);
        assert_eq!(channels.len(), 1);
        let c = &channels[0];
        assert_eq!(c.id, "BBCOne.uk");
        assert_eq!(c.name, "BBC One");
        assert_eq!(c.number.as_deref(), Some("1"));
        assert_eq!(c.logo.as_deref(), Some("http://x/bbc1.png"));
        assert_eq!(c.group.as_deref(), Some("UK"));
        assert!(!c.is_radio);
        assert_eq!(c.url, "http://tuner/stream/bbc1.ts");
    }

    #[test]
    fn falls_back_to_name_for_id_and_parses_leading_number() {
        let m3u = "#EXTINF:-1,2.1 - KCTS HD\nhttp://tuner/kcts\n";
        let channels = parse_m3u(m3u);
        assert_eq!(channels.len(), 1);
        // The id stays the playlist's display text, so guide keys and
        // mappings made before the SatIP rule was ported keep matching.
        assert_eq!(channels[0].id, "2.1 - KCTS HD");
        assert_eq!(channels[0].name, "KCTS HD");
        assert_eq!(channels[0].number.as_deref(), Some("2.1"));
    }

    /// `M3uParser.GetChannelInfo` / `GetChannelName` / `GetChannelNumber` /
    /// `ParseExtInf`: the attributes are matched and removed before the name
    /// is read, `#EXTINF` is case-insensitive, `logo` backs up `tvg-logo`, a
    /// SatIP `NN. Name` gives both the number and the name, and the number
    /// falls back from a numeric `tvg-chno` to a numeric `tvg-id` (or, with a
    /// `tvg-id`, `channel-id`), the name's prefix, then the URL's file name.
    /// Columns: the `#EXTINF` line, the URL, then name, number, group, logo.
    #[rstest::rstest]
    #[case::comma_in_an_attribute(
        r#"#EXTINF:-1 tvg-id="cnn.us" group-title="News, Politics",CNN"#,
        "http://tuner/cnn",
        "CNN",
        None,
        Some("News, Politics"),
        None
    )]
    #[case::lower_case_extinf("#extinf:-1,Arte", "http://tuner/arte", "Arte", None, None, None)]
    #[case::logo_without_tvg_logo(
        r#"#EXTINF:-1 logo="http://x/l.png",Arte"#,
        "http://tuner/arte",
        "Arte",
        None,
        None,
        Some("http://x/l.png")
    )]
    #[case::satip_name(
        "#EXTINF:0,84. VOX Schweiz",
        "http://tuner/vox",
        "VOX Schweiz",
        Some("84"),
        None,
        None
    )]
    #[case::satip_dash_name(
        "#EXTINF:0,84.0 - VOX Schweiz",
        "http://tuner/vox",
        "VOX Schweiz",
        Some("84.0"),
        None,
        None
    )]
    #[case::numeric_tvg_id(
        r#"#EXTINF:-1 tvg-id="105",Arte"#,
        "http://tuner/arte",
        "Arte",
        Some("105"),
        None,
        None
    )]
    #[case::numeric_channel_id_behind_a_tvg_id(
        r#"#EXTINF:-1 tvg-id="arte.fr" channel-id="7",Arte"#,
        "http://tuner/arte",
        "Arte",
        Some("7"),
        None,
        None
    )]
    #[case::non_numeric_tvg_chno(
        r#"#EXTINF:-1 tvg-chno="abc",Arte"#,
        "http://tuner/arte",
        "Arte",
        None,
        None,
        None
    )]
    #[case::zero_tvg_chno_falls_to_the_url(
        r#"#EXTINF:-1 tvg-chno="0",Arte"#,
        "http://tuner/live/42.ts",
        "Arte",
        Some("42"),
        None,
        None
    )]
    #[case::number_from_the_url(
        "#EXTINF:-1,Arte",
        "http://tuner/live/42.ts",
        "Arte",
        Some("42"),
        None,
        None
    )]
    #[case::no_channel_number_key(
        r#"#EXTINF:-1 channel-number="9",Arte"#,
        "http://tuner/arte",
        "Arte",
        None,
        None,
        None
    )]
    #[case::name_from_tvg_id(
        r#"#EXTINF:-1 tvg-id="bbc1.uk","#,
        "http://tuner/bbc",
        "bbc1.uk",
        None,
        None,
        None
    )]
    #[case::no_duration_before_the_comma(
        r#"#EXTINF:tvg-name="Arte HD",Arte"#,
        "http://tuner/arte",
        "Arte HD",
        None,
        None,
        None
    )]
    #[case::trailing_comma("#EXTINF:-1,Arte,", "http://tuner/arte", "Arte", None, None, None)]
    #[case::minus_one_tvg_chno(
        r#"#EXTINF:-1 tvg-chno="-1",Arte"#,
        "http://tuner/arte",
        "Arte",
        None,
        None,
        None
    )]
    #[case::number_from_a_url_with_a_query(
        "#EXTINF:-1,Arte",
        "http://tuner/live/42.ts?token=abc",
        "Arte",
        Some("42"),
        None,
        None
    )]
    #[case::url_ending_in_a_slash(
        "#EXTINF:-1,Arte",
        "http://tuner/live/42/",
        "Arte",
        None,
        None,
        None
    )]
    // Not ported: upstream keeps only the last comma-separated part, cutting
    // "Movies, Series HD" down to "Series HD".
    #[case::comma_in_the_name(
        "#EXTINF:-1,Movies, Series HD",
        "http://tuner/movies",
        "Movies, Series HD",
        None,
        None,
        None
    )]
    fn extinf_fields_match_the_upstream_parser(
        #[case] extinf: &str,
        #[case] url: &str,
        #[case] name: &str,
        #[case] number: Option<&str>,
        #[case] group: Option<&str>,
        #[case] logo: Option<&str>,
    ) {
        let channels = parse_m3u(&format!("#EXTM3U\n{extinf}\n{url}\n"));
        assert_eq!(channels.len(), 1, "{extinf}");
        let c = &channels[0];
        assert_eq!(c.name, name, "{extinf}");
        assert_eq!(c.number.as_deref(), number, "{extinf}");
        assert_eq!(c.group.as_deref(), group, "{extinf}");
        assert_eq!(c.logo.as_deref(), logo, "{extinf}");
    }

    #[test]
    fn detects_radio_and_tvg_name_fallback() {
        let m3u = "#EXTINF:-1 tvg-name=\"Jazz FM\" radio=\"true\",\nhttp://tuner/jazz\n";
        let channels = parse_m3u(m3u);
        assert_eq!(channels[0].name, "Jazz FM");
        assert!(channels[0].is_radio);
    }

    #[test]
    fn skips_extinf_without_url_and_bare_urls() {
        let m3u = "#EXTINF:-1,Dangling\n#EXTINF:-1,Real\nhttp://tuner/real\nhttp://tuner/orphan\n";
        let channels = parse_m3u(m3u);
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].name, "Real");
        assert_eq!(channels[0].url, "http://tuner/real");
    }

    #[rstest::rstest]
    #[case("http://tuner/bbc1.ts", true)]
    #[case("HTTPS://tuner/bbc1.ts", true)]
    #[case("rtsp://10.0.0.5:554/ch1", true)]
    #[case("RTP://@239.0.0.1:5000", true)]
    #[case("udp://@239.0.0.1:1234", true)]
    #[case("/var/lib/ferrofin/jellyfin.db", false)]
    #[case("file:///etc/passwd", false)]
    #[case("data/jellyfin.db", false)]
    #[case(r"C:\ProgramData\jellyfin.db", false)]
    #[case("concat:/etc/passwd|/etc/hosts", false)]
    #[case("http:/etc/passwd", false)]
    #[case("rtmp://server/live", false)]
    #[case("ftp://server/ch1.ts", false)]
    #[case("http://", false)]
    #[case("http://tuner/a\" -i \"/etc/passwd", false)]
    #[case(r"http://tuner/a\b", false)]
    #[case("http://tuner/a\tb", false)]
    fn only_absolute_stream_urls_are_channel_urls(#[case] url: &str, #[case] valid: bool) {
        assert_eq!(is_valid_channel_url(url), valid, "{url}");
    }

    #[test]
    fn drops_entries_whose_url_is_not_a_stream_url() {
        // The rejected line also clears the pending #EXTINF, so the next bare
        // line does not inherit its metadata.
        let m3u = "#EXTINF:-1,Leak\n/var/lib/ferrofin/jellyfin.db\nhttp://tuner/orphan\n\
            #EXTINF:-1,Real\nhttp://tuner/real\n";
        let channels = parse_m3u(m3u);
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].name, "Real");
        assert_eq!(channels[0].url, "http://tuner/real");
    }

    #[test]
    fn empty_input_yields_no_channels() {
        let found = parse_m3u("");
        assert!(found.is_empty(), "{found:?}");
        let found = parse_m3u("#EXTM3U\n");
        assert!(found.is_empty(), "{found:?}");
    }

    /// `ReadLineAsync` ends a line at a lone `\r` too, and `StreamReader`
    /// drops a leading byte-order mark (v12.2 M3uParser.cs:42,77): neither
    /// loses an entry.
    #[rstest::rstest]
    #[case::cr_only("#EXTM3U\r#EXTINF:-1,One\rhttp://tuner/1\r")]
    #[case::bom("\u{FEFF}#EXTINF:-1,One\nhttp://tuner/1\n")]
    #[case::no_final_terminator("#EXTINF:-1,One\r\nhttp://tuner/1")]
    fn line_endings_and_a_bom_keep_every_entry(#[case] m3u: &str) {
        let channels = parse_m3u(m3u);
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].name, "One");
        assert_eq!(channels[0].url, "http://tuner/1");
    }

    /// Fed in pieces cut anywhere (inside a line, a character, or between
    /// `\r` and `\n`), the parser reads what it reads whole.
    #[test]
    fn a_playlist_read_in_pieces_parses_as_one_read_whole() {
        let m3u = "#EXTM3U\r\n#EXTINF:-1 tvg-id=\"a\" tvg-chno=\"1\",Café One\r\n\
            http://tuner/1\r\n#EXTINF:-1,Two\rhttp://tuner/2";
        let whole = parse_m3u(m3u);
        assert_eq!(whole.len(), 2);
        assert_eq!(whole[0].name, "Café One");
        for size in 1..=7 {
            let mut parser = M3uParser::new();
            for piece in m3u.as_bytes().chunks(size) {
                parser.push(piece);
            }
            assert_eq!(parser.finish(), whole, "pieces of {size}");
        }
    }
}
