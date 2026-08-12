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
    /// `<episode-num>` in `xmltv_ns` form, when present (e.g. `0.5.` → S1E6).
    pub episode_num: Option<String>,
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
    let mut channel: Option<XmltvChannel> = None;
    let mut programme: Option<XmltvProgramme> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(&e);
                match name.as_str() {
                    "channel" => {
                        channel = Some(XmltvChannel {
                            id: attr(&e, "id").unwrap_or_default(),
                            ..XmltvChannel::default()
                        });
                    }
                    "programme" => {
                        programme = Some(XmltvProgramme {
                            channel_id: attr(&e, "channel").unwrap_or_default(),
                            start: attr(&e, "start").as_deref().and_then(parse_xmltv_time),
                            stop: attr(&e, "stop").as_deref().and_then(parse_xmltv_time),
                            ..XmltvProgramme::default()
                        });
                    }
                    _ => {}
                }
                text.clear();
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(&e);
                apply_empty(&name, &e, channel.as_mut(), programme.as_mut());
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
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                apply_end(&name, &text, channel.as_mut(), programme.as_mut());
                match name.as_str() {
                    "channel" => {
                        if let Some(c) = channel.take() {
                            out.channels.push(c);
                        }
                    }
                    "programme" => {
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

/// Applies a self-closing element (`<icon .../>`, `<new/>`, …) to the channel or
/// programme currently being built.
fn apply_empty(
    name: &str,
    e: &BytesStart<'_>,
    channel: Option<&mut XmltvChannel>,
    programme: Option<&mut XmltvProgramme>,
) {
    match name {
        "icon" => {
            let src = attr(e, "src");
            if let Some(p) = programme {
                p.icon = src;
            } else if let Some(c) = channel {
                c.icon = src;
            }
        }
        "new" => {
            if let Some(p) = programme {
                p.is_new = true;
            }
        }
        "premiere" => {
            if let Some(p) = programme {
                p.is_premiere = true;
            }
        }
        "previously-shown" => {
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
    name: &str,
    text: &str,
    channel: Option<&mut XmltvChannel>,
    programme: Option<&mut XmltvProgramme>,
) {
    let text = text.trim();
    if let Some(c) = channel {
        if name == "display-name" && c.display_name.is_empty() && !text.is_empty() {
            text.clone_into(&mut c.display_name);
        }
        return;
    }
    let Some(p) = programme else { return };
    match name {
        "title" if p.title.is_empty() => text.clone_into(&mut p.title),
        "sub-title" if !text.is_empty() => p.sub_title = Some(text.to_owned()),
        "desc" if !text.is_empty() => p.desc = Some(text.to_owned()),
        "category" if !text.is_empty() => p.categories.push(text.to_owned()),
        "date" => p.year = text.get(0..4).and_then(|y| y.parse().ok()),
        "episode-num" if !text.is_empty() => {
            p.episode_num.get_or_insert_with(|| text.to_owned());
        }
        // <rating><value>TV-PG</value></rating> — the value carries the text.
        "value" if p.rating.is_none() && !text.is_empty() => p.rating = Some(text.to_owned()),
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

/// Reads an attribute by (local) name, unescaped.
fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (a.key.local_name().as_ref() == name.as_bytes())
            .then(|| String::from_utf8_lossy(&a.value).into_owned())
    })
}

/// Returns the local (namespace-stripped) name of a start/empty element.
fn local_name(e: &BytesStart<'_>) -> String {
    String::from_utf8_lossy(e.local_name().as_ref()).into_owned()
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
}
