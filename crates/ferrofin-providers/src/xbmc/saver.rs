//! XbmcMetadata NFO writing — port of `MediaBrowser.XbmcMetadata.Savers`.
//!
//! Serializes an [`NfoBaseItem`] (plus its [`PersonInfo`] cast, carried on the
//! owning [`MetadataResult`]) back into a Kodi/XBMC `.nfo` document, mirroring
//! `BaseNfoSaver` and the per-kind savers (`MovieNfoSaver`, `EpisodeNfoSaver`,
//! `SeriesNfoSaver`, `SeasonNfoSaver`, `AlbumNfoSaver`, `ArtistNfoSaver`).
//!
//! Only the pure serialization is ported here: the C# `SaveAsync` /
//! `SaveToFileAsync` filesystem pipeline (byte-for-byte compare, hidden-attribute
//! handling, atomic replace), the media-stream (`AddMediaInfo`) block — which
//! needs `IHasMediaSources`, absent from [`NfoBaseItem`] — the image-path block
//! (`SaveImagePathsInNfo`, off by default in First-Light), the user-data block
//! (needs a user store), and the `AddCustomTags` merge of an existing file are
//! deferred. What remains is the round-trip oracle: [`save_movie`] et al. produce
//! the same tags [`super::fetch_movie`] et al. read back.
//!
//! The album/artist savers write child tracks/albums; [`NfoBaseItem`] carries no
//! child collection, so those savers emit only the parent tags (the child loops
//! become no-ops), which is faithful for a childless item.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use ferrofin_model::data::PersonKind;
use ferrofin_model::entities_media::MetadataProvider;

use crate::container_types::{MetadataResult, PersonInfo};

use super::config::NfoConfiguration;
use super::item::{NfoBaseItem, NfoItemKind};

/// The `dateadded` timestamp format (`BaseNfoSaver.DateAddedFormat`).
const DATE_ADDED_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// The canonical YouTube watch URL prefix (`BaseNfoSaver.YouTubeWatchUrl`).
const YOUTUBE_WATCH_URL: &str = "https://www.youtube.com/watch?v=";

/// The Kodi trailer plugin prefix that replaces [`YOUTUBE_WATCH_URL`] on save
/// (`BaseNfoSaver.GetOutputTrailerUrl`).
const YOUTUBE_PLUGIN_URL: &str = "plugin://plugin.video.youtube/play/?video_id=";

/// .NET ticks per minute; used to convert `RunTimeTicks` back to `<runtime>`.
const TICKS_PER_MINUTE: i64 = 600_000_000;

/// A minimal, in-memory XML writer mirroring the subset of `System.Xml.XmlWriter`
/// the NFO savers use (indented start-document, element strings, nested
/// start/end elements, and a single attribute-carrying `<url>`).
///
/// Kept private: the savers drive it and hand back the finished string. Output is
/// UTF-8, two-space indented, with the same `<?xml version="1.0" ...?>` prolog
/// `XmlWriter.WriteStartDocument(true)` emits.
struct NfoWriter {
    buf: String,
    depth: usize,
}

impl NfoWriter {
    /// Starts a document with the standalone XML declaration `XmlWriter` writes.
    fn new() -> Self {
        Self {
            buf: String::from("<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n"),
            depth: 0,
        }
    }

    /// Writes the current indentation for a fresh line.
    fn indent(&mut self) {
        for _ in 0..self.depth {
            self.buf.push_str("  ");
        }
    }

    /// Opens an element `<name>` on its own indented line and descends a level.
    fn start_element(&mut self, name: &str) {
        self.indent();
        let _ = write!(self.buf, "<{name}>");
        self.buf.push('\n');
        self.depth += 1;
    }

    /// Closes the most recently opened element.
    fn end_element(&mut self, name: &str) {
        self.depth -= 1;
        self.indent();
        let _ = write!(self.buf, "</{name}>");
        self.buf.push('\n');
    }

    /// Writes a leaf element `<name>value</name>` (`WriteElementString`).
    ///
    /// An empty value is written as `<name />`, matching `XmlWriter`.
    fn element(&mut self, name: &str, value: &str) {
        self.indent();
        if value.is_empty() {
            let _ = write!(self.buf, "<{name} />");
        } else {
            let _ = write!(self.buf, "<{name}>{}</{name}>", escape_text(value));
        }
        self.buf.push('\n');
    }

    /// Writes a `<url cache="...">value</url>` element (series episode guide).
    fn url_element(&mut self, cache: &str, value: &str) {
        self.indent();
        let _ = write!(
            self.buf,
            "<url cache=\"{}\">{}</url>",
            escape_attr(cache),
            escape_text(value)
        );
        self.buf.push('\n');
    }

    /// Finishes the document and returns the accumulated XML.
    fn finish(self) -> String {
        self.buf
    }
}

/// Escapes text content for an XML element body (`&`, `<`, `>`).
fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escapes an attribute value (element escapes plus the double quote).
fn escape_attr(value: &str) -> String {
    escape_text(value).replace('"', "&quot;")
}

/// Strips HTML tags from the overview (`BaseExtensions.StripHtml`).
///
/// Port of the `StripHtmlRegex().Replace(html, "").Trim()` prep followed by the
/// saver's `.Replace("&quot;", "'")`. The regex is `<(.|\n)*?>` — a *non-greedy*
/// `<`…`>` span — so a lone `<` with no following `>` (e.g. the `<<` in
/// `>>text<<`) is left untouched, exactly as the regex leaves it.
fn strip_html(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<'
            && let Some(close) = chars[i + 1..].iter().position(|&c| c == '>')
        {
            // Skip the whole `<…>` span (i .. i+1+close inclusive).
            i += close + 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out.trim().replace("&quot;", "'")
}

/// Formats a `DateTime` with a chrono `strftime` string (invariant culture).
fn format_date(date: DateTime<Utc>, fmt: &str) -> String {
    date.format(fmt).to_string()
}

/// Converts the .NET release-date format to a chrono `strftime` string.
///
/// The NFO configuration only ever carries `"yyyy-MM-dd"` (see
/// [`super::config::NfoConfiguration::release_date_format`]), which the parser
/// reads back with `try_read_date_time_exact`'s `yyyy-MM-dd` path; the single
/// mapped format is therefore the only one the round trip needs.
fn release_date_strftime(_format: &str) -> &'static str {
    "%Y-%m-%d"
}

/// The Kodi output form of a trailer URL (`BaseNfoSaver.GetOutputTrailerUrl`).
///
/// Replaces the YouTube watch prefix with the plugin prefix Kodi expects.
fn output_trailer_url(url: &str) -> String {
    if url.len() >= YOUTUBE_WATCH_URL.len()
        && url[..YOUTUBE_WATCH_URL.len()].eq_ignore_ascii_case(YOUTUBE_WATCH_URL)
    {
        format!("{YOUTUBE_PLUGIN_URL}{}", &url[YOUTUBE_WATCH_URL.len()..])
    } else {
        url.to_owned()
    }
}

/// The lowercase `<key>id` tag `BaseNfoSaver.GetTagForProviderKey` derives.
fn tag_for_provider_key(key: &str) -> String {
    format!("{}id", key.to_ascii_lowercase())
}

/// Whether `item` derives from `Video` for the `is not Video` outline guard.
fn is_video(kind: NfoItemKind) -> bool {
    kind.is_video()
}

/// Writes the tags common to every item kind (`BaseNfoSaver.AddCommonNodes`).
///
/// `people` is the cast/crew the C# `libraryManager.GetPeople(item)` returns; in
/// this port it is the [`MetadataResult::people`] the parser populated.
#[allow(clippy::too_many_lines)]
fn add_common_nodes(
    writer: &mut NfoWriter,
    item: &NfoBaseItem,
    people: &[PersonInfo],
    config: &NfoConfiguration,
) {
    let mut written_provider_ids: Vec<String> = Vec::new();

    let overview = strip_html(item.overview.as_deref().unwrap_or_default());

    // C# `BaseNfoSaver`: a MusicArtist's overview is `<biography>`, a
    // MusicAlbum's is `<review>`, everything else uses `<plot>`.
    match item.kind {
        NfoItemKind::MusicArtist => writer.element("biography", &overview),
        NfoItemKind::MusicAlbum => writer.element("review", &overview),
        _ => writer.element("plot", &overview),
    }

    if !is_video(item.kind) {
        writer.element("outline", &overview);
    }

    if let Some(custom) = item
        .custom_rating
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        writer.element("customrating", custom);
    }

    writer.element("lockdata", if item.is_locked { "true" } else { "false" });

    if !item.locked_fields.is_empty() {
        let joined = item
            .locked_fields
            .iter()
            .map(|f| format!("{f:?}"))
            .collect::<Vec<_>>()
            .join("|");
        writer.element("lockedfields", &joined);
    }

    let date_added = item
        .date_created
        .map_or_else(default_date_added, |d| format_date(d, DATE_ADDED_FORMAT));
    writer.element("dateadded", &date_added);

    writer.element("title", item.name.as_deref().unwrap_or_default());

    if let Some(original) = item
        .original_title
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        writer.element("originaltitle", original);
    }

    // NfoBaseItem has no separate OriginalLanguage field; skipped (parser has no
    // target for it either).

    // Directors, then writers, then credits (writers again), each distinct
    // (case-insensitive) and sorted ascending.
    for name in distinct_sorted_people(people, PersonKind::Director) {
        writer.element("director", &name);
    }
    let writers = distinct_sorted_people(people, PersonKind::Writer);
    for name in &writers {
        writer.element("writer", name);
    }
    for name in &writers {
        writer.element("credits", name);
    }

    // Trailers, ordered by trimmed URL.
    let mut trailers: Vec<&String> = item.remote_trailers.iter().collect();
    trailers.sort_by(|a, b| a.trim().cmp(b.trim()));
    for trailer in trailers {
        writer.element("trailer", &output_trailer_url(trailer));
    }

    if let Some(rating) = item.community_rating {
        writer.element("rating", &invariant_f32(rating));
    }

    if let Some(year) = item.production_year {
        writer.element("year", &year.to_string());
    }

    if let Some(sort) = item.forced_sort_name.as_deref().filter(|s| !s.is_empty()) {
        writer.element("sorttitle", sort);
    }

    if let Some(mpaa) = item.official_rating.as_deref().filter(|s| !s.is_empty()) {
        writer.element("mpaa", mpaa);
    }

    if item.kind.has_aspect_ratio()
        && let Some(ar) = item.aspect_ratio.as_deref().filter(|s| !s.is_empty())
    {
        writer.element("aspectratio", ar);
    }

    // Provider ids emitted in the fixed C# order, each recording its enum name so
    // the trailing generic loop does not re-emit it.
    if let Some(v) = item
        .provider_ids
        .get(&provider_name(MetadataProvider::TmdbCollection))
    {
        writer.element("collectionnumber", v);
        written_provider_ids.push(provider_name(MetadataProvider::TmdbCollection));
    }

    if let Some(v) = item
        .provider_ids
        .get(&provider_name(MetadataProvider::Imdb))
    {
        if item.kind == NfoItemKind::Series {
            writer.element("imdb_id", v);
        } else {
            writer.element("imdbid", v);
        }
        written_provider_ids.push(provider_name(MetadataProvider::Imdb));
    }

    // Series xml saver writes tvdb itself (in WriteCustomElements' <id>).
    if item.kind != NfoItemKind::Series
        && let Some(v) = item
            .provider_ids
            .get(&provider_name(MetadataProvider::Tvdb))
    {
        writer.element("tvdbid", v);
        written_provider_ids.push(provider_name(MetadataProvider::Tvdb));
    }

    if let Some(v) = item
        .provider_ids
        .get(&provider_name(MetadataProvider::Tmdb))
    {
        writer.element("tmdbid", v);
        written_provider_ids.push(provider_name(MetadataProvider::Tmdb));
    }

    if let Some(lang) = item
        .preferred_metadata_language
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        writer.element("language", lang);
    }

    if let Some(cc) = item
        .preferred_metadata_country_code
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        writer.element("countrycode", cc);
    }

    let date_fmt = release_date_strftime(&config.release_date_format);
    if let Some(premiere) = item
        .premiere_date
        .filter(|_| item.kind != NfoItemKind::Episode)
    {
        let formatted = format_date(premiere, date_fmt);
        // A MusicArtist's premiere date is the date it `<formed>`.
        if item.kind == NfoItemKind::MusicArtist {
            writer.element("formed", &formatted);
        } else {
            writer.element("premiered", &formatted);
            writer.element("releasedate", &formatted);
        }
    }

    if let Some(end) = item.end_date.filter(|_| item.kind != NfoItemKind::Episode) {
        writer.element("enddate", &format_date(end, date_fmt));
    }

    if let Some(critic) = item.critic_rating {
        writer.element("criticrating", &invariant_f32(critic));
    }

    if item.kind.has_display_order()
        && let Some(order) = item.display_order.as_deref().filter(|s| !s.is_empty())
    {
        writer.element("displayorder", order);
    }

    if let Some(ticks) = item.run_time_ticks {
        let minutes = ticks / TICKS_PER_MINUTE;
        writer.element("runtime", &minutes.to_string());
    }

    if let Some(tagline) = item.tagline.as_deref().filter(|s| !s.trim().is_empty()) {
        writer.element("tagline", tagline);
    }

    for country in sorted_trimmed(&item.production_locations) {
        writer.element("country", &country);
    }
    for genre in sorted_trimmed(&item.genres) {
        writer.element("genre", &genre);
    }
    for studio in sorted_trimmed(&item.studios) {
        writer.element("studio", &studio);
    }
    // Both music kinds write their tags as `<style>`.
    let tag_element = if is_music(item.kind) { "style" } else { "tag" };
    for tag in sorted_trimmed(&item.tags) {
        writer.element(tag_element, &tag);
    }

    // The remaining fixed-order provider ids.
    write_fixed_provider(
        writer,
        item,
        &mut written_provider_ids,
        MetadataProvider::AudioDbArtist,
        "audiodbartistid",
    );
    write_fixed_provider(
        writer,
        item,
        &mut written_provider_ids,
        MetadataProvider::AudioDbAlbum,
        "audiodbalbumid",
    );
    write_fixed_provider(
        writer,
        item,
        &mut written_provider_ids,
        MetadataProvider::Zap2It,
        "zap2itid",
    );
    write_fixed_provider(
        writer,
        item,
        &mut written_provider_ids,
        MetadataProvider::MusicBrainzAlbum,
        "musicbrainzalbumid",
    );
    write_fixed_provider(
        writer,
        item,
        &mut written_provider_ids,
        MetadataProvider::MusicBrainzAlbumArtist,
        "musicbrainzalbumartistid",
    );
    write_fixed_provider(
        writer,
        item,
        &mut written_provider_ids,
        MetadataProvider::MusicBrainzArtist,
        "musicbrainzartistid",
    );
    write_fixed_provider(
        writer,
        item,
        &mut written_provider_ids,
        MetadataProvider::MusicBrainzReleaseGroup,
        "musicbrainzreleasegroupid",
    );
    write_fixed_provider(
        writer,
        item,
        &mut written_provider_ids,
        MetadataProvider::TvRage,
        "tvrageid",
    );

    // Any remaining provider ids, keyed <key>id, in ascending key order.
    let mut remaining: Vec<(&String, &String)> = item
        .provider_ids
        .iter()
        .filter(|(key, value)| {
            !value.is_empty()
                && !written_provider_ids
                    .iter()
                    .any(|w| w.eq_ignore_ascii_case(key))
        })
        .collect();
    remaining.sort_by(|a, b| a.0.cmp(b.0));
    for (key, value) in remaining {
        let tag = tag_for_provider_key(key);
        if is_valid_xml_name(&tag) {
            writer.element(&tag, value);
        }
    }

    // Image paths (SaveImagePathsInNfo) and user data are deferred (off in
    // First-Light); the parser has no target for either.

    // C# `if (item is not MusicAlbum && item is not MusicArtist)` — the music
    // kinds get their credits from their own savers, not `<actor>` blocks.
    if !is_music(item.kind) {
        add_actors(writer, people);
    }
}

/// Whether this kind is one of the two music kinds, which diverge from the
/// common node set in four places (`biography`/`review`, `formed`, `style`,
/// and no `<actor>` blocks).
fn is_music(kind: NfoItemKind) -> bool {
    matches!(kind, NfoItemKind::MusicAlbum | NfoItemKind::MusicArtist)
}

/// The `dateadded` value for an item with no `DateCreated`.
///
/// C# `DateTime.MinValue.ToString(DateAddedFormat)` renders as this constant; a
/// freshly-parsed item never has a date, so serializing it must reproduce it.
fn default_date_added() -> String {
    "0001-01-01 00:00:00".to_owned()
}

/// Writes a fixed-order provider-id element and records the enum name as written.
fn write_fixed_provider(
    writer: &mut NfoWriter,
    item: &NfoBaseItem,
    written: &mut Vec<String>,
    provider: MetadataProvider,
    tag: &str,
) {
    if let Some(v) = item.provider_ids.get(&provider_name(provider)) {
        writer.element(tag, v);
        written.push(provider_name(provider));
    }
}

/// Writes the `<actor>` blocks (`BaseNfoSaver.AddActors`).
///
/// Directors and writers are excluded (they went to `<director>`/`<writer>`).
/// Ordered by sort order then trimmed name.
fn add_actors(writer: &mut NfoWriter, people: &[PersonInfo]) {
    let mut sorted: Vec<&PersonInfo> = people.iter().collect();
    sorted.sort_by(|a, b| {
        a.sort_order
            .unwrap_or(0)
            .cmp(&b.sort_order.unwrap_or(0))
            .then_with(|| a.name.trim().cmp(b.name.trim()))
    });

    for person in sorted {
        if person.is_type(PersonKind::Director) || person.is_type(PersonKind::Writer) {
            continue;
        }

        writer.start_element("actor");

        if !person.name.trim().is_empty() {
            writer.element("name", &person.name);
        }
        if let Some(role) = person.role.as_deref().filter(|s| !s.trim().is_empty()) {
            writer.element("role", role);
        }
        if person.type_ != PersonKind::Unknown {
            writer.element("type", &format!("{:?}", person.type_));
        }
        if let Some(order) = person.sort_order {
            writer.element("sortorder", &order.to_string());
        }
        // Image path (saveImagePath) is deferred.

        writer.end_element("actor");
    }
}

/// Returns the distinct (case-insensitive), ascending-sorted names of the people
/// of `kind` (`BaseNfoSaver` director/writer projection).
fn distinct_sorted_people(people: &[PersonInfo], kind: PersonKind) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for person in people.iter().filter(|p| p.is_type(kind)) {
        let name = person.name.trim().to_owned();
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            names.push(name);
        }
    }
    names.sort();
    names
}

/// Trims each string, drops empties, and sorts ascending (`.Trimmed().OrderBy`).
fn sorted_trimmed(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = values
        .iter()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .collect();
    out.sort();
    out
}

/// The canonical string spelling of a [`MetadataProvider`] (`Enum.ToString`).
fn provider_name(provider: MetadataProvider) -> String {
    format!("{provider:?}")
}

/// Formats an `f32` the invariant-culture way .NET's `float.ToString()` does for
/// the values the NFO tags carry (no trailing `.0`, `.` decimal separator).
fn invariant_f32(value: f32) -> String {
    let mut s = format!("{value}");
    if s.ends_with(".0") {
        s.truncate(s.len() - 2);
    }
    s
}

/// Whether `name` is a valid XML element name (`XmlConvert.VerifyName` guard).
///
/// The saver skips a custom provider tag whose derived name is not a legal XML
/// name (upstream catches the thrown `ArgumentException`/`XmlException`).
fn is_valid_xml_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_alphabetic() || first == '_' || first == ':') {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// Serializes a movie (or music video) NFO document (`MovieNfoSaver`).
///
/// The root element is `musicvideo` for [`NfoItemKind::MusicVideo`], else
/// `movie`. `<id>` (IMDb), the music-video `<artist>`/`<album>` tags, and the
/// movie `<set>` block are written after the common nodes.
#[must_use]
pub fn save_movie(result: &MetadataResult<NfoBaseItem>, config: &NfoConfiguration) -> String {
    let item = &result.item;
    let root = if item.kind == NfoItemKind::MusicVideo {
        "musicvideo"
    } else {
        "movie"
    };
    let mut writer = NfoWriter::new();
    writer.start_element(root);
    add_common_nodes(&mut writer, item, people_slice(result), config);

    if let Some(imdb) = item
        .provider_ids
        .get(&provider_name(MetadataProvider::Imdb))
    {
        writer.element("id", imdb);
    }

    if item.kind == NfoItemKind::MusicVideo {
        for artist in sorted_trimmed(&item.artists) {
            writer.element("artist", &artist);
        }
        if let Some(album) = item.album.as_deref().filter(|s| !s.is_empty()) {
            writer.element("album", album);
        }
    }

    if item.kind == NfoItemKind::Movie
        && let Some(collection) = item.collection_name.as_deref().filter(|s| !s.is_empty())
    {
        writer.start_element("set");
        writer.element("name", collection);
        writer.end_element("set");
    }

    writer.end_element(root);
    writer.finish()
}

/// Serializes an episode NFO document (`EpisodeNfoSaver`; root `episodedetails`).
#[must_use]
pub fn save_episode(result: &MetadataResult<NfoBaseItem>, config: &NfoConfiguration) -> String {
    let item = &result.item;
    let mut writer = NfoWriter::new();
    writer.start_element("episodedetails");
    add_common_nodes(&mut writer, item, people_slice(result), config);

    writer.element("showtitle", item.series_name.as_deref().unwrap_or_default());

    if let Some(idx) = item.index_number {
        writer.element("episode", &idx.to_string());
    }
    if let Some(end) = item.index_number_end {
        writer.element("episodenumberend", &end.to_string());
    }
    if let Some(season) = item.parent_index_number {
        writer.element("season", &season.to_string());
    }
    if let Some(premiere) = item.premiere_date {
        let fmt = release_date_strftime(&config.release_date_format);
        writer.element("aired", &format_date(premiere, fmt));
    }

    if item.parent_index_number.is_none_or(|s| s == 0) {
        if let Some(v) = item.airs_after_season_number.filter(|&v| v != -1) {
            writer.element("airsafter_season", &v.to_string());
        }
        if let Some(v) = item.airs_before_episode_number.filter(|&v| v != -1) {
            writer.element("airsbefore_episode", &v.to_string());
        }
        if let Some(v) = item.airs_before_season_number.filter(|&v| v != -1) {
            writer.element("airsbefore_season", &v.to_string());
        }
        if let Some(v) = item.airs_before_episode_number.filter(|&v| v != -1) {
            writer.element("displayepisode", &v.to_string());
        }
        // AiredSeasonNumber = ParentIndexNumber ?? AirsBeforeSeasonNumber; here
        // ParentIndexNumber is 0/None in this branch, so it is the before-season.
        if let Some(v) = item.airs_before_season_number.filter(|&v| v != -1) {
            writer.element("displayseason", &v.to_string());
        }
    }

    writer.end_element("episodedetails");
    writer.finish()
}

/// Serializes a series NFO document (`SeriesNfoSaver`; root `tvshow`).
#[must_use]
pub fn save_series(result: &MetadataResult<NfoBaseItem>, config: &NfoConfiguration) -> String {
    let item = &result.item;
    let mut writer = NfoWriter::new();
    writer.start_element("tvshow");
    add_common_nodes(&mut writer, item, people_slice(result), config);

    if let Some(tvdb) = item
        .provider_ids
        .get(&provider_name(MetadataProvider::Tvdb))
    {
        writer.element("id", tvdb);
        writer.start_element("episodeguide");
        let language = item
            .preferred_metadata_language
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("en");
        writer.url_element(
            &format!("{tvdb}.xml"),
            &format!(
                "http://www.thetvdb.com/api/1D62F2F90030C444/series/{tvdb}/all/{language}.zip"
            ),
        );
        writer.end_element("episodeguide");
    }

    writer.element("season", "-1");
    writer.element("episode", "-1");

    if let Some(status) = item.status {
        writer.element("status", &format!("{status:?}"));
    }

    writer.end_element("tvshow");
    writer.finish()
}

/// Serializes a season NFO document (`SeasonNfoSaver`; root `season`).
#[must_use]
pub fn save_season(result: &MetadataResult<NfoBaseItem>, config: &NfoConfiguration) -> String {
    let item = &result.item;
    let mut writer = NfoWriter::new();
    writer.start_element("season");
    add_common_nodes(&mut writer, item, people_slice(result), config);

    if let Some(idx) = item.index_number {
        writer.element("seasonnumber", &idx.to_string());
    }

    writer.end_element("season");
    writer.finish()
}

/// One track of an album NFO's `<track>` list (`AlbumNfoSaver.AddTracks`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NfoTrack {
    /// `ParentIndexNumber` — the disc number; `0`/absent is not written.
    pub disc: Option<i32>,
    /// `IndexNumber` — the position within the disc; `0`/absent is not written.
    pub position: Option<i32>,
    /// The track title.
    pub title: Option<String>,
    /// `RunTimeTicks`, written as `mm:ss`.
    pub run_time_ticks: Option<i64>,
    /// The track's sort name, used only for the tie-break ordering.
    pub sort_name: Option<String>,
}

/// One album of an artist NFO's `<album>` list (`ArtistNfoSaver.AddAlbums`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NfoAlbum {
    /// The album title.
    pub title: Option<String>,
    /// The album's production year.
    pub year: Option<i32>,
    /// The album's sort name, used only for the tie-break ordering.
    pub sort_name: Option<String>,
}

/// Serializes an `album.nfo` document (`AlbumNfoSaver`; root `album`).
///
/// After the common nodes come the album's artists, its album-artists, and one
/// `<track>` block per track ordered by disc, then position, then sort name,
/// then name — the exact C# `OrderBy`/`ThenBy` chain.
#[must_use]
pub fn save_album(
    result: &MetadataResult<NfoBaseItem>,
    tracks: &[NfoTrack],
    config: &NfoConfiguration,
) -> String {
    let item = &result.item;
    let mut writer = NfoWriter::new();
    writer.start_element("album");
    add_common_nodes(&mut writer, item, people_slice(result), config);

    for artist in sorted_trimmed(&item.artists) {
        writer.element("artist", &artist);
    }
    for artist in sorted_trimmed(&item.album_artists) {
        writer.element("albumartist", &artist);
    }

    let mut ordered: Vec<&NfoTrack> = tracks.iter().collect();
    ordered.sort_by(|a, b| {
        a.disc
            .unwrap_or(0)
            .cmp(&b.disc.unwrap_or(0))
            .then(a.position.unwrap_or(0).cmp(&b.position.unwrap_or(0)))
            .then_with(|| {
                sort_name_or_name(a.sort_name.as_deref(), a.title.as_deref()).cmp(
                    &sort_name_or_name(b.sort_name.as_deref(), b.title.as_deref()),
                )
            })
            .then_with(|| trimmed_name(a.title.as_deref()).cmp(&trimmed_name(b.title.as_deref())))
    });

    for track in ordered {
        writer.start_element("track");
        if let Some(disc) = track.disc.filter(|d| *d != 0) {
            writer.element("disc", &disc.to_string());
        }
        if let Some(position) = track.position.filter(|p| *p != 0) {
            writer.element("position", &position.to_string());
        }
        if let Some(title) = track.title.as_deref().filter(|t| !t.is_empty()) {
            writer.element("title", title);
        }
        if let Some(ticks) = track.run_time_ticks {
            writer.element("duration", &mm_ss(ticks));
        }
        writer.end_element("track");
    }

    writer.end_element("album");
    writer.finish()
}

/// Serializes an `artist.nfo` document (`ArtistNfoSaver`; root `artist`).
///
/// Adds `<disbanded>` (the artist's end date, in the configured release-date
/// format) and one `<album>` block per album, ordered by year, then sort name,
/// then name.
#[must_use]
pub fn save_artist(
    result: &MetadataResult<NfoBaseItem>,
    albums: &[NfoAlbum],
    config: &NfoConfiguration,
) -> String {
    let item = &result.item;
    let mut writer = NfoWriter::new();
    writer.start_element("artist");
    add_common_nodes(&mut writer, item, people_slice(result), config);

    if let Some(end) = item.end_date {
        let fmt = release_date_strftime(&config.release_date_format);
        writer.element("disbanded", &format_date(end, fmt));
    }

    let mut ordered: Vec<&NfoAlbum> = albums.iter().collect();
    ordered.sort_by(|a, b| {
        a.year
            .unwrap_or(0)
            .cmp(&b.year.unwrap_or(0))
            .then_with(|| {
                sort_name_or_name(a.sort_name.as_deref(), a.title.as_deref()).cmp(
                    &sort_name_or_name(b.sort_name.as_deref(), b.title.as_deref()),
                )
            })
            .then_with(|| trimmed_name(a.title.as_deref()).cmp(&trimmed_name(b.title.as_deref())))
    });

    for album in ordered {
        writer.start_element("album");
        if let Some(title) = album.title.as_deref().filter(|t| !t.is_empty()) {
            writer.element("title", title);
        }
        if let Some(year) = album.year {
            writer.element("year", &year.to_string());
        }
        writer.end_element("album");
    }

    writer.end_element("artist");
    writer.finish()
}

/// C# `SortNameOrName`: the sort name when set, else the name.
fn sort_name_or_name(sort_name: Option<&str>, name: Option<&str>) -> String {
    sort_name
        .filter(|s| !s.is_empty())
        .or(name)
        .unwrap_or_default()
        .to_owned()
}

/// The trimmed name, for the final `ThenBy(i => i.Name?.Trim())` tie-break.
fn trimmed_name(name: Option<&str>) -> String {
    name.unwrap_or_default().trim().to_owned()
}

/// A run time as `mm:ss` — C# `TimeSpan.FromTicks(...).ToString(@"mm\:ss")`,
/// which formats the minutes *component*, so a track over an hour wraps.
fn mm_ss(ticks: i64) -> String {
    let total_seconds = ticks / 10_000_000;
    let minutes = (total_seconds / 60) % 60;
    let seconds = total_seconds % 60;
    format!("{minutes:02}:{seconds:02}")
}

/// Borrows the result's people as a slice, treating a never-populated list as
/// empty (`libraryManager.GetPeople` returns an empty list, never null).
fn people_slice(result: &MetadataResult<NfoBaseItem>) -> &[PersonInfo] {
    result.people.as_deref().unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_is_non_greedy_and_keeps_orphan_brackets() {
        // A `<b>` tag is removed; the `>>…<<` (no closing `>` after `<<`) is kept.
        assert_eq!(strip_html("<b>hi</b>"), "hi");
        assert_eq!(strip_html(">>text<<"), ">>text<<");
        // &quot; maps to a single quote; surrounding whitespace is trimmed.
        assert_eq!(strip_html("  a &quot;b&quot; c  "), "a 'b' c");
    }

    #[test]
    fn output_trailer_url_rewrites_youtube_watch_prefix() {
        assert_eq!(
            output_trailer_url("https://www.youtube.com/watch?v=abc"),
            "plugin://plugin.video.youtube/play/?video_id=abc"
        );
        // A non-YouTube URL is untouched.
        assert_eq!(
            output_trailer_url("https://example.com/x"),
            "https://example.com/x"
        );
    }

    #[test]
    fn invariant_f32_drops_trailing_dot_zero() {
        assert_eq!(invariant_f32(7.6), "7.6");
        assert_eq!(invariant_f32(77.0), "77");
        assert_eq!(invariant_f32(8.5), "8.5");
    }

    #[test]
    fn tag_for_provider_key_lowercases_and_suffixes() {
        assert_eq!(tag_for_provider_key("MyCustom"), "mycustomid");
        assert_eq!(tag_for_provider_key("Imdb"), "imdbid");
    }

    #[test]
    fn is_valid_xml_name_rejects_bad_leading_char() {
        assert!(is_valid_xml_name("imdbid"));
        assert!(is_valid_xml_name("_x"));
        assert!(!is_valid_xml_name("9id"));
        assert!(!is_valid_xml_name(""));
        assert!(!is_valid_xml_name("a b"));
    }

    #[test]
    fn escape_text_and_attr_escape_the_expected_characters() {
        assert_eq!(escape_text("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        assert_eq!(escape_attr("x\"y&z"), "x&quot;y&amp;z");
    }

    #[test]
    fn save_movie_emits_expected_prolog_and_root() {
        let config = NfoConfiguration::default();
        let mut result = MetadataResult::new(NfoBaseItem::new(NfoItemKind::Movie));
        result.item.name = Some("Hi".to_owned());
        let xml = save_movie(&result, &config);
        assert!(xml.starts_with(
            "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<movie>\n"
        ));
        assert!(xml.contains("<title>Hi</title>"));
        assert!(xml.trim_end().ends_with("</movie>"));
    }

    #[test]
    fn save_movie_uses_musicvideo_root_for_music_video() {
        let config = NfoConfiguration::default();
        let result = MetadataResult::new(NfoBaseItem::new(NfoItemKind::MusicVideo));
        let xml = save_movie(&result, &config);
        assert!(xml.contains("<musicvideo>"));
        assert!(xml.trim_end().ends_with("</musicvideo>"));
    }
}
