//! The per-item-kind NFO parsers — ports of `MovieNfoParser`,
//! `EpisodeNfoParser`, `SeriesNfoParser`, `SeasonNfoParser` and
//! `SeriesNfoSeasonParser`.
//!
//! Each is a thin [`NfoParser`] implementation over [`BaseNfoParser`], overriding
//! `SupportsUrlAfterClosingXmlTag` and/or `FetchDataFromXmlNode` exactly as its
//! C# counterpart does. `MusicVideo` reuses [`MovieNfoParser`] (as upstream does).

use ferrofin_model::entities::VideoType;

use crate::container_types::MetadataResult;

use super::base_parser::{BaseNfoParser, NfoParser};
use super::item::{NfoBaseItem, NfoItemKind};
use super::xml_ext::{get_air_days, read_normalized_string, try_parse_series_status, try_read_int};
use super::xml_reader::XmlCursor;

/// NFO parser for movies (and music videos) — port of `MovieNfoParser`.
///
/// Adds `<id>` attribute parsing, `<set>` (collection) handling, and the
/// music-video `<artist>`/`<album>` tags on top of the base switch, and enables
/// the URL-after-closing-tag path.
#[derive(Debug, Default, Clone, Copy)]
pub struct MovieNfoParser;

impl NfoParser for MovieNfoParser {
    fn supports_url_after_closing_xml_tag(&self) -> bool {
        true
    }

    fn fetch_data_from_xml_node(
        &self,
        base: &BaseNfoParser<'_>,
        reader: &mut XmlCursor,
        result: &mut MetadataResult<NfoBaseItem>,
    ) {
        match reader.name() {
            "id" => parse_id_node(reader, result),
            "set" => {
                let tmdbcol = reader.get_attribute("tmdbcolid").map(ToOwned::to_owned);
                if result.item.kind == NfoItemKind::Movie
                    && let Some(v) = tmdbcol.filter(|s| !s.is_empty())
                {
                    result.item.set_provider_id("TmdbCollection", &v);
                }

                let inner = reader.read_inner_xml();
                if !inner.trim().is_empty()
                    && result.item.kind == NfoItemKind::Movie
                    && let Some(name) = parse_set_xml(&inner)
                {
                    result.item.collection_name = Some(name);
                }
            }
            "artist" => {
                let artist = read_normalized_string(reader);
                if !artist.is_empty() && result.item.kind == NfoItemKind::MusicVideo {
                    result.item.artists.push(artist);
                }
            }
            "album" => {
                let album = read_normalized_string(reader);
                if !album.is_empty() && result.item.kind == NfoItemKind::MusicVideo {
                    result.item.album = Some(album);
                }
            }
            _ => base.fetch_data_from_xml_node_base(reader, result),
        }
    }
}

/// Reads an `<id>` node's TMDB/TVDB/IMDB attributes and content
/// (`MovieNfoParser`/`SeriesNfoParser` `<id>` handling; identical in both).
///
/// The content id is treated as an IMDb id only when no `IMDB` attribute was
/// given and it starts with `tt` (Kodi allows arbitrary content otherwise).
fn parse_id_node(reader: &mut XmlCursor, result: &mut MetadataResult<NfoBaseItem>) {
    let from_moviedb = reader.get_attribute("TMDB").map(ToOwned::to_owned);
    let from_thetvdb = reader.get_attribute("TVDB").map(ToOwned::to_owned);
    let mut from_imdb = reader.get_attribute("IMDB").map(ToOwned::to_owned);

    let content = reader.read_element_content_as_string();
    if from_imdb.as_deref().is_none_or(str::is_empty) && content.starts_with("tt") {
        from_imdb = Some(content);
    }

    if let Some(v) = from_moviedb.filter(|s| !s.is_empty()) {
        result.item.set_provider_id("Tmdb", &v);
    }
    if let Some(v) = from_thetvdb.filter(|s| !s.is_empty()) {
        result.item.set_provider_id("Tvdb", &v);
    }
    if let Some(v) = from_imdb.filter(|s| !s.is_empty()) {
        result.item.set_provider_id("Imdb", &v);
    }
}

/// Extracts the collection name from a movie `<set>` node's inner XML
/// (`MovieNfoParser.ParseSetXml`).
///
/// The set is either bare text (`<set>Name</set>`) or a nested `<name>` element.
fn parse_set_xml(inner: &str) -> Option<String> {
    // Reproduce the C# wrapping `"<set>" + xml + "</set>"`.
    let wrapped = format!("<set>{inner}</set>");
    let Ok(mut reader) = XmlCursor::new(&wrapped) else {
        return None;
    };
    reader.move_to_content();
    reader.read();

    while !reader.eof() {
        if reader.is_text() {
            // A *significant* text node directly under <set> is the collection
            // name. .NET reports whitespace-only runs between elements as a
            // `Whitespace` node (not `Text`), so the C# `NodeType == Text` check
            // skips them — mirror that by ignoring whitespace-only text here.
            match reader.text_value() {
                Some(text) if !text.trim().is_empty() => return Some(text.to_owned()),
                _ => reader.read(),
            }
        } else if reader.is_element() {
            match reader.name() {
                "name" => return Some(reader.read_element_content_as_string()),
                _ => reader.skip(),
            }
        } else {
            reader.read();
        }
    }
    None
}

/// NFO parser for TV episodes — port of `EpisodeNfoParser`.
///
/// Adds season/episode numbering, air-order, `showtitle`, and the multi-block
/// (`</episodedetails>`) merge that concatenates name/originaltitle/overview.
#[derive(Debug, Default, Clone, Copy)]
pub struct EpisodeNfoParser;

impl EpisodeNfoParser {
    /// Runs the multi-episode aware fetch (`EpisodeNfoParser.Fetch` override).
    ///
    /// Splits the document on `</episodedetails>`: the first block populates the
    /// item, and each subsequent block appends its name/overview/originaltitle
    /// (joined with `" / "`) and extends `IndexNumberEnd`.
    pub fn fetch(
        &self,
        base: &BaseNfoParser<'_>,
        result: &mut MetadataResult<NfoBaseItem>,
        xml: &str,
    ) {
        result.reset_people();

        let needle = "</episodedetails>";
        let mut remaining = xml;
        let (first, rest) =
            split_after_ci(remaining, needle).map_or((remaining, ""), |(head, tail)| (head, tail));
        remaining = rest;

        // First block populates the result item directly.
        self.read_details(base, result, first);

        let mut name = result.item.name.clone().unwrap_or_default();
        let mut original_title = result.item.original_title.clone().unwrap_or_default();
        let mut overview = result.item.overview.clone().unwrap_or_default();

        while let Some((block, tail)) = split_after_ci(remaining, needle) {
            remaining = tail;

            let mut additional = MetadataResult::new(NfoBaseItem::new(NfoItemKind::Episode));
            self.read_details(base, &mut additional, block);

            if let Some(n) = additional.item.name.as_deref().filter(|s| !s.is_empty()) {
                name.push_str(" / ");
                name.push_str(n);
            }
            if let Some(o) = additional
                .item
                .overview
                .as_deref()
                .filter(|s| !s.is_empty())
            {
                overview.push_str(" / ");
                overview.push_str(o);
            }
            if let Some(t) = additional
                .item
                .original_title
                .as_deref()
                .filter(|s| !s.is_empty())
            {
                original_title.push_str(" / ");
                original_title.push_str(t);
            }
            if let Some(idx) = additional.item.index_number {
                let current = result.item.index_number_end.unwrap_or(idx);
                result.item.index_number_end = Some(current.max(idx));
            }
        }

        result.item.name = Some(name);
        result.item.original_title = Some(original_title);
        result.item.overview = Some(overview);
    }

    /// Streams one `<episodedetails>` block through the element switch.
    fn read_details(
        self,
        base: &BaseNfoParser<'_>,
        result: &mut MetadataResult<NfoBaseItem>,
        xml: &str,
    ) {
        let Ok(mut reader) = XmlCursor::new(xml) else {
            return;
        };
        reader.move_to_content();
        reader.read();
        while !reader.eof() {
            if reader.is_element() {
                self.fetch_data_from_xml_node(base, &mut reader, result);
            } else {
                reader.read();
            }
        }
    }
}

impl NfoParser for EpisodeNfoParser {
    fn fetch_data_from_xml_node(
        &self,
        base: &BaseNfoParser<'_>,
        reader: &mut XmlCursor,
        result: &mut MetadataResult<NfoBaseItem>,
    ) {
        match reader.name() {
            "season" => {
                if let Some(v) = try_read_int(reader) {
                    result.item.parent_index_number = Some(v);
                }
            }
            "episode" => {
                if let Some(v) = try_read_int(reader) {
                    result.item.index_number = Some(v);
                }
            }
            "episodenumberend" => {
                if let Some(v) = try_read_int(reader) {
                    result.item.index_number_end = Some(v);
                }
            }
            "airsbefore_episode" | "displayepisode" => {
                if let Some(v) = try_read_int(reader) {
                    result.item.airs_before_episode_number = Some(v);
                }
            }
            "airsafter_season" | "displayafterseason" => {
                if let Some(v) = try_read_int(reader) {
                    result.item.airs_after_season_number = Some(v);
                }
            }
            "airsbefore_season" | "displayseason" => {
                if let Some(v) = try_read_int(reader) {
                    result.item.airs_before_season_number = Some(v);
                }
            }
            "showtitle" => result.item.series_name = Some(read_normalized_string(reader)),
            _ => base.fetch_data_from_xml_node_base(reader, result),
        }
    }
}

/// NFO parser for TV series — port of `SeriesNfoParser`.
///
/// Adds `<id>` attribute parsing, air day/time, status, and skips `namedseason`
/// (handled by [`SeriesNfoSeasonParser`]); enables the URL-after-closing path.
#[derive(Debug, Default, Clone, Copy)]
pub struct SeriesNfoParser;

impl NfoParser for SeriesNfoParser {
    fn supports_url_after_closing_xml_tag(&self) -> bool {
        true
    }

    fn fetch_data_from_xml_node(
        &self,
        base: &BaseNfoParser<'_>,
        reader: &mut XmlCursor,
        result: &mut MetadataResult<NfoBaseItem>,
    ) {
        match reader.name() {
            "id" => parse_id_node(reader, result),
            "airs_dayofweek" => {
                let val = reader.read_element_content_as_string();
                if let Some(days) = get_air_days(Some(val.as_str())) {
                    result.item.air_days = days;
                }
            }
            "airs_time" => result.item.air_time = Some(read_normalized_string(reader)),
            "status" => {
                let status = reader.read_element_content_as_string();
                if !status.trim().is_empty()
                    && let Some(s) = try_parse_series_status(status.trim())
                {
                    result.item.status = Some(s);
                }
            }
            "namedseason" => reader.skip(),
            _ => base.fetch_data_from_xml_node_base(reader, result),
        }
    }
}

/// NFO parser for TV seasons — port of `SeasonNfoParser`.
///
/// Adds `<seasonnumber>` / `<seasonname>`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SeasonNfoParser;

impl NfoParser for SeasonNfoParser {
    fn fetch_data_from_xml_node(
        &self,
        base: &BaseNfoParser<'_>,
        reader: &mut XmlCursor,
        result: &mut MetadataResult<NfoBaseItem>,
    ) {
        match reader.name() {
            "seasonnumber" => {
                if let Some(v) = try_read_int(reader) {
                    result.item.index_number = Some(v);
                }
            }
            "seasonname" => result.item.name = Some(read_normalized_string(reader)),
            _ => base.fetch_data_from_xml_node_base(reader, result),
        }
    }
}

/// NFO parser mapping a series `<namedseason>` onto a season — port of
/// `SeriesNfoSeasonParser`.
///
/// Sets the season name only when the `number` attribute matches the season's
/// index number; enables the URL-after-closing path.
#[derive(Debug, Default, Clone, Copy)]
pub struct SeriesNfoSeasonParser;

impl NfoParser for SeriesNfoSeasonParser {
    fn supports_url_after_closing_xml_tag(&self) -> bool {
        true
    }

    fn fetch_data_from_xml_node(
        &self,
        _base: &BaseNfoParser<'_>,
        reader: &mut XmlCursor,
        result: &mut MetadataResult<NfoBaseItem>,
    ) {
        if reader.name() == "namedseason" {
            let number = reader
                .get_attribute("number")
                .and_then(|n| n.trim().parse::<i32>().ok());
            let name = reader.read_element_content_as_string();

            if let (Some(number), Some(index)) = (number, result.item.index_number)
                && !name.trim().is_empty()
                && number == index
            {
                result.item.name = Some(name);
            }
        } else {
            reader.skip();
        }
    }
}

/// Splits `text` into `(head_including_needle, tail)` at the first
/// case-insensitive occurrence of `needle`; `None` if absent.
fn split_after_ci<'a>(text: &'a str, needle: &str) -> Option<(&'a str, &'a str)> {
    let lower = text.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    let idx = lower.find(&needle_lower)?;
    let end = idx + needle.len();
    Some((&text[..end], &text[end..]))
}

/// The default video type for a freshly-constructed movie/music-video item.
///
/// Retained for parity with the C# constructor default; not written by the
/// parsers but exercised by consumers building items.
#[must_use]
pub const fn default_video_type() -> VideoType {
    VideoType::VideoFile
}
