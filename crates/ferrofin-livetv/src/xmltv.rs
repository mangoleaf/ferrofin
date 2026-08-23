//! XMLTV electronic-program-guide parser.
//!
//! Ported from Jellyfin's `Jellyfin.XmlTv/XmlTvReader.cs`. An XMLTV document is a
//! `<tv>` root containing `<channel>` definitions and `<programme>` airings:
//!
//! ```text
//! <tv>
//!   <channel id="BBCOne.uk"><display-name>BBC One</display-name><icon src="..."/></channel>
//!   <programme start="20260725060000 +0000" stop="20260725070000 +0000" channel="BBCOne.uk">
//!     <title>Breakfast</title><desc>Morning news.</desc><category>News</category>
//!   </programme>
//! </tv>
//! ```
//!
//! A programme's `channel` attribute matches a channel's `id`, which is the same
//! key an M3U tuner exposes as `tvg-id` — that is how guide data binds to tuner
//! channels.

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

/// A channel definition from the guide.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct XmltvChannel {
    /// The `id` attribute — the join key against tuner channels' `tvg-id`.
    pub id: String,
    /// The first `<display-name>`.
    pub display_name: String,
    /// The `<icon src>` URL, when present.
    pub icon: Option<String>,
}

/// A single programme (an airing on a channel over a time range).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct XmltvProgramme {
    /// The `channel` attribute — matches an [`XmltvChannel::id`].
    pub channel_id: String,
    /// Airing start (parsed from the `start` attribute), UTC.
    pub start: Option<DateTime<Utc>>,
    /// Airing end (parsed from the `stop` attribute), UTC.
    pub stop: Option<DateTime<Utc>>,
    /// `<title>`.
    pub title: String,
    /// `<sub-title>` — the episode title, when present.
    pub sub_title: Option<String>,
    /// `<desc>` — the synopsis, when present.
    pub desc: Option<String>,
    /// All `<category>` values.
    pub categories: Vec<String>,
    /// `<icon src>` artwork URL, when present.
    pub icon: Option<String>,
    /// Production year from `<date>` (first four digits), when present.
    pub year: Option<i32>,
    /// The text of the last `<episode-num>` whose `system` the reader understands
    /// (`xmltv_ns`, e.g. `0.5.` → S1E6, or `SxxExx`).
    pub episode_num: Option<String>,
    /// `Episode.Series` — the 1-based season number from that `<episode-num>`.
    pub season_number: Option<i32>,
    /// `Episode.Episode` — the 1-based episode number from that `<episode-num>`.
    pub episode_number: Option<i32>,
    /// `true` when a `<new/>` element is present.
    pub is_new: bool,
    /// `true` when a `<premiere>` element is present.
    pub is_premiere: bool,
    /// `true` when a `<previously-shown>` element is present.
    pub is_previously_shown: bool,
    /// `<rating>/<value>` (content rating like `TV-PG`), when present.
    pub rating: Option<String>,
}

/// The parsed guide: channel definitions and their programmes.
#[derive(Debug, Clone, Default)]
pub struct Xmltv {
    /// Channel definitions.
    pub channels: Vec<XmltvChannel>,
    /// Programme airings.
    pub programmes: Vec<XmltvProgramme>,
}

/// Parses an XMLTV document. Malformed XML yields whatever was read before the
/// error (lenient, matching how tuner guides are consumed in practice); a
/// completely unparseable document yields an empty guide.
#[must_use]
pub fn parse_xmltv(xml: &str) -> Xmltv {
    let mut reader = Reader::from_str(xml);
    let config = reader.config_mut();
    config.check_end_names = false;
    config.trim_text(true);

    let mut out = Xmltv::default();
    // Scratch buffer for the text content of the element currently being read.
    let mut text = String::new();
    // The `system` of the `<episode-num>` being read, if any.
    let mut episode_system: Option<String> = None;
    let mut channel: Option<XmltvChannel> = None;
    let mut programme: Option<XmltvProgramme> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                match e.local_name().as_ref() {
                    b"channel" => {
                        channel = Some(XmltvChannel {
                            id: attr(&e, b"id").unwrap_or_default(),
                            ..XmltvChannel::default()
                        });
                    }
                    b"programme" => {
                        programme = Some(read_programme(&e));
                    }
                    b"episode-num" => episode_system = attr(&e, b"system"),
                    _ => {}
                }
                text.clear();
            }
            Ok(Event::Empty(e)) => {
                let name = e.local_name();
                apply_empty(name.as_ref(), &e, channel.as_mut(), programme.as_mut());
            }
            Ok(Event::Text(e)) => {
                if let Ok(t) = e.unescape() {
                    text.push_str(&t);
                }
            }
            Ok(Event::CData(e)) => {
                text.push_str(&String::from_utf8_lossy(e.as_ref()));
            }
            Ok(Event::End(e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                apply_end(
                    name,
                    &text,
                    episode_system.take().as_deref(),
                    channel.as_mut(),
                    programme.as_mut(),
                );
                match name {
                    b"channel" => {
                        if let Some(c) = channel.take() {
                            out.channels.push(c);
                        }
                    }
                    b"programme" => {
                        if let Some(p) = programme.take() {
                            out.programmes.push(p);
                        }
                    }
                    _ => {}
                }
                text.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => {}
        }
    }

    out
}

/// An attribute's value with XML entity references resolved, falling back to the
/// raw bytes when the value will not unescape.
///
/// Resolution matters for parity: `.NET`'s `XmlReader.GetAttribute` — what
/// Jellyfin's `XmlTvReader` calls — unescapes attribute values, so a
/// `channel="A&amp;E.us"` has to join against the tuner's `tvg-id="A&E.us"`.
///
/// The fallback is what keeps a sloppy guide working. `unescape_value` fails on
/// a bare `&` (`http://x?a=1&b=2`) and on a reference to an entity XMLTV never
/// defines, both of which real generators emit. Dropping the attribute there
/// would empty `channel_id` and silently discard every programme on that
/// channel — the exact failure this function exists to prevent, just moved to a
/// different input. Returning the raw text instead keeps the join key that
/// already matched before entity resolution was added.
///
/// This is deliberately more lenient than upstream: `.NET`'s `XmlReader` throws
/// `XmlException` on a bare `&`, so Jellyfin fails the whole guide refresh. A
/// guide that mostly parses is more useful than no guide at all.
fn unescaped_or_raw(a: &quick_xml::events::attributes::Attribute<'_>) -> String {
    a.unescape_value().map_or_else(
        |_| String::from_utf8_lossy(a.value.as_ref()).into_owned(),
        std::borrow::Cow::into_owned,
    )
}

/// Reads a `<programme>` start tag's `channel`/`start`/`stop` attributes in a
/// single pass over the attribute list.
fn read_programme(e: &BytesStart<'_>) -> XmltvProgramme {
    let mut p = XmltvProgramme::default();
    for a in e.attributes().flatten() {
        let value = unescaped_or_raw(&a);
        match a.key.local_name().as_ref() {
            b"channel" => p.channel_id = value,
            b"start" => p.start = parse_xmltv_time(&value),
            b"stop" => p.stop = parse_xmltv_time(&value),
            _ => {}
        }
    }
    p
}

/// Applies a self-closing element (`<icon .../>`, `<new/>`, …) to the channel or
/// programme currently being built.
fn apply_empty(
    name: &[u8],
    e: &BytesStart<'_>,
    channel: Option<&mut XmltvChannel>,
    programme: Option<&mut XmltvProgramme>,
) {
    match name {
        b"icon" => {
            let src = attr(e, b"src");
            if let Some(p) = programme {
                p.icon = src;
            } else if let Some(c) = channel {
                c.icon = src;
            }
        }
        b"new" => {
            if let Some(p) = programme {
                p.is_new = true;
            }
        }
        b"premiere" => {
            if let Some(p) = programme {
                p.is_premiere = true;
            }
        }
        b"previously-shown" => {
            if let Some(p) = programme {
                p.is_previously_shown = true;
            }
        }
        _ => {}
    }
}

/// Applies the closing of a text-bearing element to the channel or programme
/// currently being built, using the accumulated `text`.
fn apply_end(
    name: &[u8],
    text: &str,
    episode_system: Option<&str>,
    channel: Option<&mut XmltvChannel>,
    programme: Option<&mut XmltvProgramme>,
) {
    let text = text.trim();
    if let Some(c) = channel {
        if name == b"display-name" && c.display_name.is_empty() && !text.is_empty() {
            text.clone_into(&mut c.display_name);
        }
        return;
    }
    let Some(p) = programme else { return };
    match name {
        b"title" if p.title.is_empty() => text.clone_into(&mut p.title),
        b"sub-title" if !text.is_empty() => p.sub_title = Some(text.to_owned()),
        b"desc" if !text.is_empty() => p.desc = Some(text.to_owned()),
        b"category" if !text.is_empty() => p.categories.push(text.to_owned()),
        b"date" => p.year = text.get(0..4).and_then(|y| y.parse().ok()),
        // `XmlTvReader.ProcessEpisodeNum` dispatches on `system`; a later
        // element assigns only the parts it carries (a part that does not parse
        // leaves the earlier value), and unknown systems (`onscreen`,
        // `dd_progid`, …) are skipped.
        b"episode-num" if !text.is_empty() => {
            if let Some((season, episode)) = parse_episode_num(episode_system, text) {
                p.episode_num = Some(text.to_owned());
                if season.is_some() {
                    p.season_number = season;
                }
                if episode.is_some() {
                    p.episode_number = episode;
                }
            }
        }
        // <rating><value>TV-PG</value></rating> — the value carries the text.
        b"value" if p.rating.is_none() && !text.is_empty() => p.rating = Some(text.to_owned()),
        _ => {}
    }
}

/// Parses an XMLTV timestamp: `YYYYMMDDHHMMSS` optionally followed by a
/// ` ±HHMM` offset. A missing offset is treated as UTC.
#[must_use]
pub fn parse_xmltv_time(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    let (datetime, offset) = match raw.split_once(' ') {
        Some((dt, off)) => (dt, Some(off.trim())),
        None => (raw, None),
    };
    if datetime.len() < 14 || !datetime.as_bytes()[..14].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let naive = chrono::NaiveDateTime::parse_from_str(&datetime[..14], "%Y%m%d%H%M%S").ok()?;
    let fixed = match offset.filter(|o| !o.is_empty()) {
        Some(o) => parse_offset(o)?,
        None => FixedOffset::east_opt(0)?,
    };
    Some(
        fixed
            .from_local_datetime(&naive)
            .single()?
            .with_timezone(&Utc),
    )
}

/// Parses a `±HHMM` numeric timezone offset into a [`FixedOffset`].
fn parse_offset(o: &str) -> Option<FixedOffset> {
    let bytes = o.as_bytes();
    if bytes.len() != 5 || (bytes[0] != b'+' && bytes[0] != b'-') {
        return None;
    }
    let hours: i32 = o[1..3].parse().ok()?;
    let mins: i32 = o[3..5].parse().ok()?;
    let secs = (hours * 3600 + mins * 60) * if bytes[0] == b'-' { -1 } else { 1 };
    FixedOffset::east_opt(secs)
}

/// Reads an attribute by (local) name, with XML entity references resolved.
///
/// The named attribute's value, entity-resolved (see [`unescaped_or_raw`]).
fn attr(e: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if a.key.local_name().as_ref() == name {
            Some(unescaped_or_raw(&a))
        } else {
            None
        }
    })
}

/// Season and episode numbers from an `<episode-num>` of the given `system`,
/// as `XmlTvReader.ProcessEpisodeNum` reads them; `None` when the system is one
/// the reader skips (`onscreen`, `dd_progid`, an unknown one).
///
/// `xmltv_ns` is `S.E.P`, each part 0-based and optionally `n/total`, any part
/// empty → 1-based numbers. `SxxExx` is the `s(\d+)e(\d+)` pattern anywhere in
/// the text, case-insensitive, taken as-is.
#[must_use]
pub fn parse_episode_num(system: Option<&str>, value: &str) -> Option<(Option<i32>, Option<i32>)> {
    let value = value.trim();
    // The upstream `switch` is an exact, case-sensitive match on the system name.
    match system {
        Some("xmltv_ns") => {
            // Spaces are stripped from the whole value first (`Replace(" ", "")`).
            let value: String = value.chars().filter(|c| *c != ' ').collect();
            let part = |part: Option<&str>| -> Option<i32> {
                let n = part?.split('/').next()?;
                n.parse::<i32>().ok().map(|n| n + 1)
            };
            let mut parts = value.split('.');
            Some((part(parts.next()), part(parts.next())))
        }
        Some("SxxExx") => {
            let lower = value.to_ascii_lowercase();
            let bytes = lower.as_bytes();
            let digits = |from: usize| -> Option<(i32, usize)> {
                let end = from
                    + bytes
                        .get(from..)?
                        .iter()
                        .take_while(|b| b.is_ascii_digit())
                        .count();
                if end == from {
                    return None;
                }
                lower[from..end].parse().ok().map(|n| (n, end))
            };
            let mut at = 0;
            while let Some(s_pos) = lower[at..].find('s') {
                let s_pos = at + s_pos;
                if let Some((season, e_pos)) = digits(s_pos + 1)
                    && bytes.get(e_pos) == Some(&b'e')
                    && let Some((episode, _)) = digits(e_pos + 1)
                {
                    return Some((Some(season), Some(episode)));
                }
                at = s_pos + 1;
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="BBCOne.uk">
    <display-name>BBC One</display-name>
    <icon src="http://x/bbc1.png"/>
  </channel>
  <programme start="20260725060000 +0000" stop="20260725070000 +0100" channel="BBCOne.uk">
    <title>Breakfast</title>
    <sub-title>Episode 42</sub-title>
    <desc>Morning news and sport.</desc>
    <category>News</category>
    <category>Magazine</category>
    <date>2026</date>
    <episode-num system="xmltv_ns">0.41.</episode-num>
    <icon src="http://x/prog.png"/>
    <new/>
    <rating><value>TV-PG</value></rating>
  </programme>
</tv>"#;

    #[test]
    fn parses_channels_and_programmes() {
        let g = parse_xmltv(SAMPLE);
        assert_eq!(g.channels.len(), 1);
        assert_eq!(g.channels[0].id, "BBCOne.uk");
        assert_eq!(g.channels[0].display_name, "BBC One");
        assert_eq!(g.channels[0].icon.as_deref(), Some("http://x/bbc1.png"));

        assert_eq!(g.programmes.len(), 1);
        let p = &g.programmes[0];
        assert_eq!(p.channel_id, "BBCOne.uk");
        assert_eq!(p.title, "Breakfast");
        assert_eq!(p.sub_title.as_deref(), Some("Episode 42"));
        assert_eq!(p.desc.as_deref(), Some("Morning news and sport."));
        assert_eq!(p.categories, vec!["News", "Magazine"]);
        assert_eq!(p.year, Some(2026));
        assert_eq!(p.episode_num.as_deref(), Some("0.41."));
        assert_eq!(p.icon.as_deref(), Some("http://x/prog.png"));
        assert!(p.is_new);
        assert_eq!(p.rating.as_deref(), Some("TV-PG"));
    }

    #[test]
    fn parses_utc_and_offset_times() {
        let g = parse_xmltv(SAMPLE);
        let p = &g.programmes[0];
        // 06:00 +0000 == 06:00 UTC.
        assert_eq!(p.start.unwrap().to_rfc3339(), "2026-07-25T06:00:00+00:00");
        // 07:00 +0100 == 06:00 UTC.
        assert_eq!(p.stop.unwrap().to_rfc3339(), "2026-07-25T06:00:00+00:00");
    }

    #[test]
    fn time_parser_handles_missing_offset_as_utc() {
        let t = parse_xmltv_time("20260101120000").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-01-01T12:00:00+00:00");
        assert!(parse_xmltv_time("garbage").is_none());
        assert!(parse_xmltv_time("2026").is_none());
    }

    #[test]
    fn empty_document_is_empty_guide() {
        let g = parse_xmltv("");
        assert!(g.channels.is_empty());
        assert!(g.programmes.is_empty());
    }

    /// Attribute values carry XML entity references just like element text does,
    /// and `.NET`'s `XmlReader.GetAttribute` (what Jellyfin's reader calls)
    /// resolves them. Leaving them raw silently breaks the guide↔tuner join: the
    /// M3U's `tvg-id` is `A&E.us`, so a channel id kept as `A&amp;E.us` matches
    /// nothing and every programme on that channel is dropped.
    #[test]
    fn attribute_entities_are_resolved() {
        const ESCAPED: &str = r#"<tv>
  <channel id="A&amp;E.us">
    <display-name>A&amp;E</display-name>
    <icon src="http://x/logo.png?a=1&amp;b=2"/>
  </channel>
  <programme start="20260725060000 +0000" stop="20260725070000 +0000" channel="A&amp;E.us">
    <title>Storage &amp; Wars</title>
    <icon src="http://x/art.png?a=1&#38;b=2"/>
  </programme>
</tv>"#;
        let g = parse_xmltv(ESCAPED);
        assert_eq!(g.channels[0].id, "A&E.us");
        assert_eq!(g.channels[0].display_name, "A&E");
        assert_eq!(
            g.channels[0].icon.as_deref(),
            Some("http://x/logo.png?a=1&b=2")
        );
        let p = &g.programmes[0];
        // The join key must equal the channel's id, or the programme is orphaned.
        assert_eq!(p.channel_id, g.channels[0].id);
        assert_eq!(p.title, "Storage & Wars");
        // Numeric character references resolve too.
        assert_eq!(p.icon.as_deref(), Some("http://x/art.png?a=1&b=2"));
    }

    /// An attribute that will not unescape keeps its raw text — it is never
    /// dropped.
    ///
    /// Dropping it is the same silent-programme-loss bug that entity resolution
    /// was added to fix: an empty `channel_id` joins no tuner channel, so every
    /// programme on it disappears. A bare `&` in a query string is the common
    /// real-world case (`.NET`'s `XmlReader` throws outright, so Jellyfin loses
    /// the whole refresh); an undefined entity reference is the rarer one. Both
    /// fall back rather than vanish.
    #[test]
    fn an_attribute_that_will_not_unescape_keeps_its_raw_text() {
        // A bare `&` — the input a sloppy generator emits most often.
        let g = parse_xmltv(
            r#"<tv><programme start="20260725060000" channel="A&E.us"><title>T</title><icon src="http://x/a.png?a=1&b=2"/></programme></tv>"#,
        );
        assert_eq!(g.programmes.len(), 1);
        assert_eq!(
            g.programmes[0].channel_id, "A&E.us",
            "the raw join key still matches a tuner's tvg-id=\"A&E.us\""
        );
        assert_eq!(
            g.programmes[0].icon.as_deref(),
            Some("http://x/a.png?a=1&b=2")
        );
        assert!(g.programmes[0].start.is_some());

        // A reference to an entity no DTD-less document defines.
        let g = parse_xmltv(
            r#"<tv><programme start="20260725060000" channel="a&nope;b"><title>T</title></programme></tv>"#,
        );
        assert_eq!(g.programmes.len(), 1);
        assert_eq!(g.programmes[0].channel_id, "a&nope;b");
        assert_eq!(g.programmes[0].title, "T");
    }
}

#[cfg(test)]
mod episode_num_tests {
    use super::{parse_episode_num, parse_xmltv};
    use rstest::rstest;

    #[rstest]
    #[case(Some("xmltv_ns"), "0.5.", Some((Some(1), Some(6))))]
    #[case(Some("xmltv_ns"), "0.41.", Some((Some(1), Some(42))))]
    #[case(Some("xmltv_ns"), "1/3.0/10.", Some((Some(2), Some(1))))]
    #[case(Some("xmltv_ns"), ".5.", Some((None, Some(6))))]
    #[case(Some("xmltv_ns"), "2..", Some((Some(3), None)))]
    #[case(Some("SxxExx"), "S01E06", Some((Some(1), Some(6))))]
    #[case(Some("SxxExx"), "Episode s2e10 (repeat)", Some((Some(2), Some(10))))]
    #[case(Some("sxxexx"), "S02E10", None)]
    #[case(Some("xmltv_ns"), "1 0.5.", Some((Some(11), Some(6))))]
    #[case(Some("onscreen"), "S01E06", None)]
    #[case(Some("dd_progid"), "EP012345.0001", None)]
    #[case(None, "0.5.", None)]
    fn episode_numbers(
        #[case] system: Option<&str>,
        #[case] text: &str,
        #[case] expected: Option<(Option<i32>, Option<i32>)>,
    ) {
        assert_eq!(parse_episode_num(system, text), expected);
    }

    #[test]
    fn the_last_understood_episode_num_wins_and_skipped_systems_are_ignored() {
        // Schedules Direct / zap2xml order: dd_progid, xmltv_ns, onscreen.
        let guide = parse_xmltv(
            "<tv><programme start=\"20260725060000 +0000\" channel=\"x\"><title>T</title>\
             <episode-num system=\"dd_progid\">EP012345.0001</episode-num>\
             <episode-num system=\"xmltv_ns\">0.5.</episode-num>\
             <episode-num system=\"onscreen\">S09E09</episode-num></programme></tv>",
        );
        let p = &guide.programmes[0];
        assert_eq!((p.season_number, p.episode_number), (Some(1), Some(6)));
        assert_eq!(p.episode_num.as_deref(), Some("0.5."));

        // Per-field assignment: a later element only overwrites the parts it
        // carries, and an unparsable part leaves the earlier value alone.
        let guide = parse_xmltv(
            "<tv><programme start=\"20260725060000 +0000\" channel=\"x\"><title>T</title>\
             <episode-num system=\"SxxExx\">S02E03</episode-num>\
             <episode-num system=\"xmltv_ns\">.5.</episode-num>\
             <episode-num system=\"xmltv_ns\">a.b.</episode-num></programme></tv>",
        );
        let p = &guide.programmes[0];
        assert_eq!((p.season_number, p.episode_number), (Some(2), Some(6)));
    }
}
