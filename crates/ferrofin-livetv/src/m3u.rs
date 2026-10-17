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

/// A single channel parsed from an M3U tuner playlist.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct M3uChannel {
    /// Stable channel id — the `tvg-id`/`channel-id`/`CUID` attribute when
    /// present, otherwise derived from the name. Also the key the XMLTV guide
    /// matches its `channel id` against.
    pub id: String,
    /// The display name (the text after the comma on the `#EXTINF` line, or
    /// `tvg-name` when that is empty).
    pub name: String,
    /// The channel number (`tvg-chno`/`channel-number`), or a leading `N.N`
    /// parsed from the name, when present.
    pub number: Option<String>,
    /// Logo/artwork URL (`tvg-logo`), when present.
    pub logo: Option<String>,
    /// The `group-title` category, when present.
    pub group: Option<String>,
    /// `true` when the entry is flagged `radio="true"` (audio-only).
    pub is_radio: bool,
    /// The stream URL that plays the channel.
    pub url: String,
}

/// Parses the body of an M3U tuner playlist into its channel list.
///
/// Lines that are not `#EXTINF`/URL pairs (including `#EXTM3U`, blank lines and
/// unknown `#`-directives) are skipped, matching Jellyfin's lenient parsing. An
/// `#EXTINF` with no following URL line is dropped, and so is an entry whose
/// URL line fails [`is_valid_channel_url`].
#[must_use]
pub fn parse_m3u(content: &str) -> Vec<M3uChannel> {
    let mut channels = Vec::new();
    let mut pending: Option<M3uChannel> = None;
    let mut skipped = 0usize;

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending = Some(parse_extinf(rest));
        } else if line.starts_with('#') {
            // #EXTM3U / #EXTVLCOPT / #EXTGRP / other directives — ignored.
        } else if let Some(mut channel) = pending.take() {
            // `M3uParser.GetChannelsAsync` drops the entry and clears the
            // pending `#EXTINF` (`take` already did). Upstream warns per entry;
            // that volume scales with the playlist, so one count is warned below.
            if !is_valid_channel_url(line) {
                tracing::debug!(
                    path = line,
                    "live tv: skipping M3U channel entry with an invalid stream URL"
                );
                skipped += 1;
                continue;
            }
            line.clone_into(&mut channel.url);
            channels.push(channel);
        }
        // A bare URL with no preceding #EXTINF is ignored (no metadata).
    }

    if skipped > 0 {
        tracing::warn!(
            skipped,
            "live tv: skipped M3U channel entries with invalid stream URLs"
        );
    }
    channels
}

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

/// Parses the portion of an `#EXTINF:` line after the `#EXTINF:` prefix: the
/// duration + attributes, a comma, then the display name.
fn parse_extinf(rest: &str) -> M3uChannel {
    // Split into "<duration> <attrs>" and "<name>" on the first comma.
    let (head, name) = match rest.split_once(',') {
        Some((head, name)) => (head, name.trim().to_owned()),
        None => (rest, String::new()),
    };
    let attrs = parse_attributes(head);

    let name = if name.is_empty() {
        attrs.get("tvg-name").cloned().unwrap_or_default()
    } else {
        name
    };

    let number = attrs
        .get("tvg-chno")
        .or_else(|| attrs.get("channel-number"))
        .cloned()
        .or_else(|| leading_channel_number(&name));

    let id = attrs
        .get("tvg-id")
        .or_else(|| attrs.get("channel-id"))
        .or_else(|| attrs.get("cuid"))
        .filter(|v| !v.is_empty())
        .cloned()
        .unwrap_or_else(|| name.clone());

    M3uChannel {
        id,
        name,
        number,
        logo: attrs.get("tvg-logo").filter(|v| !v.is_empty()).cloned(),
        group: attrs.get("group-title").filter(|v| !v.is_empty()).cloned(),
        is_radio: attrs
            .get("radio")
            .is_some_and(|v| v.eq_ignore_ascii_case("true")),
        url: String::new(),
    }
}

/// Extracts `key="value"` attribute pairs from an `#EXTINF` head. Keys are
/// lower-cased so lookups are case-insensitive (playlists vary on casing).
fn parse_attributes(head: &str) -> HashMap<String, String> {
    let mut attrs = HashMap::new();
    let bytes = head.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find the next `key=`.
        if bytes[i] == b'=' {
            // Walk back to the start of the key (letters, digits, '-').
            let key_end = i;
            let mut key_start = i;
            while key_start > 0 {
                let c = bytes[key_start - 1];
                if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' {
                    key_start -= 1;
                } else {
                    break;
                }
            }
            let key = head[key_start..key_end].to_ascii_lowercase();
            // Expect a quoted value.
            if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                let val_start = i + 2;
                if let Some(rel_end) = head[val_start..].find('"') {
                    let value = head[val_start..val_start + rel_end].to_owned();
                    if !key.is_empty() {
                        attrs.insert(key, value);
                    }
                    i = val_start + rel_end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    attrs
}

/// Pulls a leading `N` or `N.N` channel number off the front of a display name
/// (e.g. `"2.1 - KCTS HD"` → `"2.1"`), matching Jellyfin's fallback numbering.
fn leading_channel_number(name: &str) -> Option<String> {
    let token = name.split_whitespace().next()?;
    let trimmed = token.trim_end_matches(['.', '-', ':']);
    let ok = !trimmed.is_empty()
        && trimmed.chars().all(|c| c.is_ascii_digit() || c == '.')
        && trimmed.chars().any(|c| c.is_ascii_digit());
    ok.then(|| trimmed.to_owned())
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
        assert_eq!(channels[0].id, "2.1 - KCTS HD");
        assert_eq!(channels[0].number.as_deref(), Some("2.1"));
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
        assert!(parse_m3u("").is_empty());
        assert!(parse_m3u("#EXTM3U\n").is_empty());
    }
}
