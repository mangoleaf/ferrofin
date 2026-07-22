//! The generic NFO parser core — port of
//! `MediaBrowser.XbmcMetadata.Parsers.BaseNfoParser<T>`.
//!
//! Drives the element switch that reads a Kodi/XBMC `.nfo` file into an
//! [`NfoBaseItem`] wrapped in a [`MetadataResult`]. The per-item-kind subclasses
//! (`MovieNfoParser`, `EpisodeNfoParser`, …) live in [`super::parsers`] and hook
//! this via the [`NfoParser`] trait's `fetch_data_from_xml_node` override point.
//!
//! Two collaborators are abstracted behind small traits so no real I/O leaks
//! into the parse: [`ExternalIdSource`] supplies the provider-id key set the C#
//! `IProviderManager.GetExternalIdInfos` returns, and [`DirectoryService`]
//! resolves local artwork paths (C# `IDirectoryService.GetFile`). Tests pass
//! fakes for both.

use chrono::Datelike;
use hermit_model::data::PersonKind;
use hermit_model::entities::MetadataField;

use crate::container_types::{FileSystemMetadata, LocalImageInfo, MetadataResult, PersonInfo};

use super::config::NfoConfiguration;
use super::item::{NfoBaseItem, NfoItemKind};
use super::xml_ext::{
    get_image_type, get_person_array, get_person_from_xml_node, left_part, provider_id_parsers,
    read_element_content_as_bool, read_normalized_string, try_read_date_time,
    try_read_date_time_exact, try_read_int,
};
use super::xml_reader::XmlCursor;

/// The deprecated Kodi YouTube trailer plugin prefix (old format).
const YT_PLUGIN_OLD: &str = "plugin://plugin.video.youtube/?action=play_video&videoid=";
/// The current Kodi YouTube trailer plugin prefix.
const YT_PLUGIN_NEW: &str = "plugin://plugin.video.youtube/play/?video_id=";
/// The canonical YouTube watch URL prefix (`BaseNfoSaver.YouTubeWatchUrl`).
const YOUTUBE_WATCH_URL: &str = "https://www.youtube.com/watch?v=";

/// Minimum production year accepted from a `<year>` tag (upstream `> 1850`).
const MIN_PRODUCTION_YEAR: i32 = 1850;
/// .NET ticks per second (100-ns units); used for run-time conversions.
const TICKS_PER_SECOND: i64 = 10_000_000;
/// Seconds per minute; used for the `<runtime>` (minutes) conversion.
const SECONDS_PER_MINUTE: i64 = 60;

/// Supplies the external provider-id key set for building `_validProviderIds`.
///
/// Port of the single `IProviderManager.GetExternalIdInfos(item)` call the base
/// parser makes; only the id `Key`s matter here, so the trait yields them
/// directly. Kept synchronous (the parse is synchronous) and separate from the
/// async `hermit_traits::providers::ProviderManager`.
pub trait ExternalIdSource: Send + Sync {
    /// Returns the external-id keys applicable to the item being parsed
    /// (e.g. `"Imdb"`, `"Tmdb"`); each becomes a `"<Key>Id"` → `Key` mapping.
    fn external_id_keys(&self) -> Vec<String>;
}

/// Resolves local artwork file paths (`IDirectoryService.GetFile`).
///
/// Behind a trait so the un-mockable filesystem access stays out of the parser's
/// coverage/parity numbers; tests supply a fake.
pub trait DirectoryService: Send + Sync {
    /// Returns metadata for the file at `path`, or `None` if it does not exist.
    fn get_file(&self, path: &str) -> Option<FileSystemMetadata>;
}

/// A [`DirectoryService`] that resolves nothing (every path is absent).
///
/// Used by parsers/tests that carry no local artwork.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoDirectoryService;

impl DirectoryService for NoDirectoryService {
    fn get_file(&self, _path: &str) -> Option<FileSystemMetadata> {
        None
    }
}

/// The result of a fetch — either an argument error or the populated item.
///
/// Port of the `ArgumentException` the C# `Fetch` throws for a null item or an
/// empty metadata path; a `Result` replaces the exception.
pub type FetchResult = Result<(), FetchError>;

/// Why a fetch could not proceed (`ArgumentException` cases).
#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The metadata-file path was empty (`ArgumentException.ThrowIfNullOrEmpty`).
    EmptyMetadataFile,
}

/// The extension point the per-kind subclasses implement.
///
/// Mirrors the C# `virtual` hooks: `SupportsUrlAfterClosingXmlTag`, the top-level
/// `Fetch` override (episodes), and `FetchDataFromXmlNode`. Default methods
/// reproduce the base behavior; subclasses override and may delegate back to
/// [`BaseNfoParser::fetch_data_from_xml_node_base`].
pub trait NfoParser {
    /// Whether a raw provider URL may follow the closing XML tag
    /// (`SupportsUrlAfterClosingXmlTag`; true for movie/series parsers).
    fn supports_url_after_closing_xml_tag(&self) -> bool {
        false
    }

    /// Handles one element, dispatching to the base switch by default.
    fn fetch_data_from_xml_node(
        &self,
        base: &BaseNfoParser<'_>,
        reader: &mut XmlCursor,
        result: &mut MetadataResult<NfoBaseItem>,
    ) {
        base.fetch_data_from_xml_node_base(reader, result);
    }
}

/// The generic NFO parser core.
///
/// Holds the collaborators and the `_validProviderIds` map assembled from
/// [`ExternalIdSource`]; the actual element handling lives in
/// [`Self::fetch_data_from_xml_node_base`].
pub struct BaseNfoParser<'a> {
    config: &'a NfoConfiguration,
    directory_service: &'a dyn DirectoryService,
    /// Map of NFO id-tag name → canonical provider key (`_validProviderIds`),
    /// keyed case-insensitively (lower-cased here).
    valid_provider_ids: Vec<(String, String)>,
}

impl<'a> BaseNfoParser<'a> {
    /// Builds a parser core, assembling `_validProviderIds` from the external-id
    /// source plus the fixed additional mappings.
    #[must_use]
    pub fn new(
        config: &'a NfoConfiguration,
        external_ids: &'a dyn ExternalIdSource,
        directory_service: &'a dyn DirectoryService,
    ) -> Self {
        let mut valid_provider_ids: Vec<(String, String)> = Vec::new();
        let mut add = |key: &str, value: &str| {
            let key = key.to_ascii_lowercase();
            if !valid_provider_ids.iter().any(|(k, _)| *k == key) {
                valid_provider_ids.push((key, value.to_owned()));
            }
        };

        for key in external_ids.external_id_keys() {
            let tag = format!("{key}Id");
            add(&tag, &key);
        }

        // Additional Mappings (fixed).
        add("collectionnumber", "TmdbCollection");
        add("tmdbcolid", "TmdbCollection");
        add("tmdbcol", "TmdbCollection");
        add("imdb_id", "Imdb");

        Self {
            config,
            directory_service,
            valid_provider_ids,
        }
    }

    /// Looks up an NFO id-tag name in `_validProviderIds` (case-insensitively).
    fn lookup_provider(&self, tag: &str) -> Option<&str> {
        let tag = tag.to_ascii_lowercase();
        self.valid_provider_ids
            .iter()
            .find(|(k, _)| *k == tag)
            .map(|(_, v)| v.as_str())
    }

    /// Fetches metadata from an NFO document string into `result`.
    ///
    /// Port of the two `Fetch` bodies: the `SupportsUrlAfterClosingXmlTag` path
    /// (handle a raw provider URL after the last closing tag) and the plain
    /// path. The caller supplies the file contents (I/O stays outside).
    pub fn fetch<P: NfoParser>(
        &self,
        parser: &P,
        result: &mut MetadataResult<NfoBaseItem>,
        xml: &str,
    ) {
        result.reset_people();

        if !parser.supports_url_after_closing_xml_tag() {
            self.fetch_document(parser, result, xml);
            return;
        }

        // Handle a url after the xml data (http://kodi.wiki/view/NFO_files/movies).
        // Find last "</" then the '>' that closes that tag.
        let index = xml
            .rfind("</")
            .and_then(|i| xml[i..].find('>').map(|g| i + g));

        match index {
            Some(idx) => {
                let ending = &xml[idx..];
                Self::parse_provider_links(result, ending);

                // If the file is just an IMDb url (closing tag at 0), stop.
                if idx == 0 {
                    return;
                }

                let doc = &xml[..=idx];
                self.fetch_document(parser, result, doc);
            }
            None => {
                // The file is just provider urls.
                Self::parse_provider_links(result, xml);
            }
        }
    }

    /// Streams a well-formed(ish) XML document through the element switch.
    fn fetch_document<P: NfoParser>(
        &self,
        parser: &P,
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
                parser.fetch_data_from_xml_node(self, &mut reader, result);
            } else {
                reader.read();
            }
        }
    }

    /// Scans a trailing/leading raw URL for IMDb/TMDb/TVDb ids
    /// (`ParseProviderLinks`).
    fn parse_provider_links(result: &mut MetadataResult<NfoBaseItem>, xml: &str) {
        let item = &mut result.item;

        if let Some(imdb) = provider_id_parsers::try_find_imdb_id(xml) {
            item.set_provider_id("Imdb", &imdb);
        }

        if item.kind == NfoItemKind::Movie
            && let Some(tmdb) = provider_id_parsers::try_find_tmdb_movie_id(xml)
        {
            item.set_provider_id("Tmdb", &tmdb);
        }

        if item.kind == NfoItemKind::Series {
            if let Some(tmdb) = provider_id_parsers::try_find_tmdb_series_id(xml) {
                item.set_provider_id("Tmdb", &tmdb);
            }
            if let Some(tvdb) = provider_id_parsers::try_find_tvdb_id(xml) {
                item.set_provider_id("Tvdb", &tvdb);
            }
        }
    }

    /// The base element switch (`BaseNfoParser.FetchDataFromXmlNode`).
    ///
    /// Handles every tag common to all item kinds; unrecognized tags that match
    /// a `_validProviderIds` entry set a provider id, otherwise the element is
    /// skipped.
    #[allow(clippy::too_many_lines)]
    pub fn fetch_data_from_xml_node_base(
        &self,
        reader: &mut XmlCursor,
        result: &mut MetadataResult<NfoBaseItem>,
    ) {
        let name = reader.name().to_owned();
        match name.as_str() {
            "dateadded" => {
                if let Some(dt) = try_read_date_time(reader) {
                    result.item.date_created = Some(dt);
                }
            }
            "originaltitle" => result.item.original_title = Some(read_normalized_string(reader)),
            "name" | "title" | "localtitle" => {
                result.item.name = Some(read_normalized_string(reader));
            }
            "sortname" => result.item.sort_name = Some(read_normalized_string(reader)),
            "criticrating" => {
                let text = reader.read_element_content_as_string();
                if let Ok(value) = text.trim().parse::<f32>() {
                    result.item.critic_rating = Some(value);
                }
            }
            "sorttitle" => result.item.forced_sort_name = Some(read_normalized_string(reader)),
            "biography" | "plot" | "review" => {
                result.item.overview = Some(read_normalized_string(reader));
            }
            "language" => {
                result.item.preferred_metadata_language = Some(read_normalized_string(reader));
            }
            "watched" => {
                // User-data import is out of scope for the local parse; consume.
                let _ = read_element_content_as_bool(reader);
            }
            "playcount" | "lastplayed" => {
                // User-data import is out of scope for the local parse; consume.
                let _ = reader.read_element_content_as_string();
            }
            "countrycode" => {
                result.item.preferred_metadata_country_code = Some(read_normalized_string(reader));
            }
            "lockedfields" => {
                let val = reader.read_element_content_as_string();
                if !val.trim().is_empty() {
                    result.item.locked_fields =
                        val.split('|').filter_map(parse_metadata_field).collect();
                }
            }
            "tagline" => result.item.tagline = Some(read_normalized_string(reader)),
            "country" => {
                let val = reader.read_element_content_as_string();
                if !val.trim().is_empty() {
                    result.item.production_locations = val
                        .split('/')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned)
                        .collect();
                }
            }
            "mpaa" => result.item.official_rating = Some(read_normalized_string(reader)),
            "customrating" => result.item.custom_rating = Some(read_normalized_string(reader)),
            "runtime" => {
                let text = reader.read_element_content_as_string();
                if let Ok(runtime) = left_part(text.trim(), ' ').parse::<i64>() {
                    result.item.run_time_ticks =
                        Some(runtime * SECONDS_PER_MINUTE * TICKS_PER_SECOND);
                }
            }
            "aspectratio" => {
                let ar = read_normalized_string(reader);
                if !ar.is_empty() && result.item.kind.has_aspect_ratio() {
                    result.item.aspect_ratio = Some(ar);
                }
            }
            "lockdata" => {
                result.item.is_locked = reader
                    .read_element_content_as_string()
                    .eq_ignore_ascii_case("true");
            }
            "studio" => {
                let studio = read_normalized_string(reader);
                if !studio.is_empty() {
                    result.item.add_studio(studio);
                }
            }
            "director" => {
                for director in get_person_array(reader, PersonKind::Director) {
                    result.add_person(director);
                }
            }
            "credits" => {
                let val = reader.read_element_content_as_string();
                if !val.trim().is_empty() {
                    for part in val.split('/').map(str::trim).filter(|s| !s.is_empty()) {
                        result.add_person(PersonInfo {
                            name: part.to_owned(),
                            type_: PersonKind::Writer,
                            ..PersonInfo::default()
                        });
                    }
                }
            }
            "writer" => {
                for writer in get_person_array(reader, PersonKind::Writer) {
                    result.add_person(writer);
                }
            }
            "actor" => {
                if let Some(person) = get_person_from_xml_node(reader) {
                    result.add_person(person);
                }
            }
            "trailer" => {
                let trailer = read_normalized_string(reader);
                if !trailer.is_empty() {
                    if let Some(rest) = strip_prefix_ci(&trailer, YT_PLUGIN_OLD) {
                        result
                            .item
                            .add_trailer_url(format!("{YOUTUBE_WATCH_URL}{rest}"));
                    } else if let Some(rest) = strip_prefix_ci(&trailer, YT_PLUGIN_NEW) {
                        result
                            .item
                            .add_trailer_url(format!("{YOUTUBE_WATCH_URL}{rest}"));
                    }
                }
            }
            "displayorder" => {
                let display_order = read_normalized_string(reader);
                if !display_order.is_empty() && result.item.kind.has_display_order() {
                    result.item.display_order = Some(display_order);
                }
            }
            "year" => {
                if let Some(year) = try_read_int(reader)
                    && year > MIN_PRODUCTION_YEAR
                {
                    result.item.production_year = Some(year);
                }
            }
            "rating" => {
                let rating = reader.read_element_content_as_string().replace(',', ".");
                if let Ok(value) = rating.trim().parse::<f32>() {
                    result.item.community_rating = Some(value);
                }
            }
            "ratings" => Self::fetch_from_ratings_node(reader, &mut result.item),
            "communityrating" => {
                let text = reader.read_element_content_as_string().replace(',', ".");
                if let Ok(value) = text.trim().parse::<f32>()
                    && (0.0..=10.0).contains(&value)
                {
                    result.item.community_rating = Some(value);
                }
            }
            "aired" | "formed" | "premiered" | "releasedate" => {
                if let Some(date) =
                    try_read_date_time_exact(reader, &self.config.release_date_format)
                {
                    result.item.premiere_date = Some(date);
                    // Production year can already be set by the <year> tag.
                    if result.item.production_year.is_none() {
                        result.item.production_year = Some(date.year());
                    }
                }
            }
            "enddate" => {
                if let Some(date) =
                    try_read_date_time_exact(reader, &self.config.release_date_format)
                {
                    result.item.end_date = Some(date);
                }
            }
            "genre" => {
                let val = reader.read_element_content_as_string();
                if !val.trim().is_empty() {
                    for part in val.split('/').map(str::trim).filter(|s| !s.is_empty()) {
                        result.item.add_genre(part);
                    }
                }
            }
            "style" | "tag" => {
                let tag = read_normalized_string(reader);
                if !tag.is_empty() {
                    result.item.add_tag(tag);
                }
            }
            "fileinfo" => Self::fetch_from_file_info_node(reader, &mut result.item),
            "uniqueid" => {
                if reader.is_empty_element() {
                    reader.read();
                    return;
                }
                let provider = reader.get_attribute("type").map(ToOwned::to_owned);
                let provider_id = reader.read_element_content_as_string();
                if let Some(provider) = provider.filter(|p| !p.is_empty()) {
                    let normalized = self
                        .lookup_provider(&provider)
                        .map_or(provider.clone(), ToOwned::to_owned);
                    result.item.set_provider_id(&normalized, &provider_id);
                }
            }
            "thumb" => self.fetch_thumb_node(reader, result, "thumb"),
            "fanart" => {
                if reader.is_empty_element() {
                    reader.read();
                    return;
                }
                let mut subtree = reader.read_subtree();
                if read_to_descendant(&mut subtree, "thumb") {
                    self.fetch_thumb_node(&mut subtree, result, "fanart");
                }
            }
            other => {
                if let Some(provider) = self.lookup_provider(other).map(ToOwned::to_owned) {
                    let id = reader.read_element_content_as_string();
                    result.item.set_provider_id(&provider, &id);
                } else {
                    reader.skip();
                }
            }
        }
    }

    /// Reads a `<thumb>`/`<fanart><thumb>` node into the images/remote-images
    /// lists (`FetchThumbNode`).
    fn fetch_thumb_node(
        &self,
        reader: &mut XmlCursor,
        result: &mut MetadataResult<NfoBaseItem>,
        parent_node: &str,
    ) {
        let mut art_type = reader.get_attribute("aspect").map(ToOwned::to_owned);
        let val = reader.read_element_content_as_string();

        let art_type = match art_type.take() {
            Some(a) if !a.trim().is_empty() => a,
            // No aspect under <fanart> → fanart; otherwise Sonarr posters → poster.
            _ if parent_node == "fanart" => "fanart".to_owned(),
            _ => "poster".to_owned(),
        };

        // Skip empty uri, or a tag with '.' (season/episode/set images).
        if val.is_empty() || art_type.contains('.') {
            return;
        }

        let image_type = get_image_type(&art_type);

        // A non-URL value (`Uri.TryCreate(Absolute)` failing) is dropped upstream;
        // here that means anything that is neither an http(s) URL nor a local
        // file path. File paths route to the directory service; URLs to remote.
        if !is_url(&val) {
            return;
        }

        if is_file_path(&val) {
            if result.images.iter().any(|i| i.type_ == image_type) {
                return;
            }
            // C# passes the original value to `IDirectoryService.GetFile`.
            if let Some(meta) = self.directory_service.get_file(&val)
                && meta.exists
            {
                result.images.push(LocalImageInfo {
                    file_info: meta,
                    type_: image_type,
                });
            }
        } else {
            if result.remote_images.iter().any(|(_, t)| *t == image_type) {
                return;
            }
            result.remote_images.push((val, image_type));
        }
    }

    /// Reads `<fileinfo>` → `<streamdetails>` (`FetchFromFileInfoNode`).
    fn fetch_from_file_info_node(reader: &mut XmlCursor, item: &mut NfoBaseItem) {
        if reader.is_empty_element() {
            reader.read();
            return;
        }
        let mut sub = reader.read_subtree();
        sub.move_to_content();
        sub.read();
        while !sub.eof() {
            if !sub.is_element() {
                sub.read();
                continue;
            }
            match sub.name() {
                "streamdetails" => Self::fetch_from_stream_details_node(&mut sub, item),
                _ => sub.skip(),
            }
        }
    }

    /// Reads `<streamdetails>` → `<video>`/`<subtitle>` (`FetchFromStreamDetailsNode`).
    fn fetch_from_stream_details_node(reader: &mut XmlCursor, item: &mut NfoBaseItem) {
        if reader.is_empty_element() {
            reader.read();
            return;
        }
        let mut sub = reader.read_subtree();
        sub.move_to_content();
        sub.read();
        while !sub.eof() {
            if !sub.is_element() {
                sub.read();
                continue;
            }
            match sub.name() {
                "video" => Self::fetch_from_video_node(&mut sub, item),
                "subtitle" => Self::fetch_from_subtitle_node(&mut sub, item),
                _ => sub.skip(),
            }
        }
    }

    /// Reads a `<video>` stream node (`FetchFromVideoNode`).
    fn fetch_from_video_node(reader: &mut XmlCursor, item: &mut NfoBaseItem) {
        if reader.is_empty_element() {
            reader.read();
            return;
        }
        let mut sub = reader.read_subtree();
        sub.move_to_content();
        sub.read();
        while !sub.eof() {
            if !sub.is_element() || !item.kind.is_video() {
                sub.read();
                continue;
            }
            match sub.name() {
                "format3d" => {
                    item.video_3d_format = parse_3d_format(&sub.read_element_content_as_string());
                }
                "aspect" => item.aspect_ratio = Some(read_normalized_string(&mut sub)),
                "width" => item.width = try_read_int(&mut sub),
                "height" => item.height = try_read_int(&mut sub),
                "durationinseconds" => {
                    if let Some(seconds) = try_read_int(&mut sub) {
                        item.run_time_ticks = Some(i64::from(seconds) * TICKS_PER_SECOND);
                    }
                }
                _ => sub.skip(),
            }
        }
    }

    /// Reads a `<subtitle>` stream node (`FetchFromSubtitleNode`).
    fn fetch_from_subtitle_node(reader: &mut XmlCursor, item: &mut NfoBaseItem) {
        if reader.is_empty_element() {
            reader.read();
            return;
        }
        let mut sub = reader.read_subtree();
        sub.move_to_content();
        sub.read();
        while !sub.eof() {
            if !sub.is_element() {
                sub.read();
                continue;
            }
            match sub.name() {
                "language" => {
                    let _ = sub.read_element_content_as_string();
                    if item.kind.is_video() {
                        item.has_subtitles = true;
                    }
                }
                _ => sub.skip(),
            }
        }
    }

    /// Reads a `<ratings>` node (`FetchFromRatingsNode`).
    fn fetch_from_ratings_node(reader: &mut XmlCursor, item: &mut NfoBaseItem) {
        if reader.is_empty_element() {
            reader.read();
            return;
        }
        let mut sub = reader.read_subtree();
        sub.move_to_content();
        sub.read();
        while !sub.eof() {
            if sub.is_element() {
                match sub.name() {
                    "rating" => {
                        if sub.is_empty_element() {
                            sub.read();
                            continue;
                        }
                        let rating_name = sub.get_attribute("name").map(ToOwned::to_owned);
                        let mut inner = sub.read_subtree();
                        Self::fetch_from_rating_node(&mut inner, item, rating_name.as_deref());
                    }
                    _ => sub.skip(),
                }
            } else {
                sub.read();
            }
        }
    }

    /// Reads a single `<rating>` node's `<value>` (`FetchFromRatingNode`).
    fn fetch_from_rating_node(
        reader: &mut XmlCursor,
        item: &mut NfoBaseItem,
        rating_name: Option<&str>,
    ) {
        reader.move_to_content();
        reader.read();
        while !reader.eof() {
            if reader.is_element() {
                match reader.name() {
                    "value" => {
                        let val = reader.read_element_content_as_string();
                        if let Ok(rating) = val.trim().parse::<f32>() {
                            let is_tomato = rating_name.is_some_and(|n| {
                                n.to_ascii_lowercase().contains("tomato")
                                    && !n.to_ascii_lowercase().contains("audience")
                            });
                            if is_tomato {
                                if !rating_name
                                    .is_some_and(|n| n.to_ascii_lowercase().contains("avg"))
                                {
                                    item.critic_rating = Some(rating);
                                }
                            } else {
                                item.community_rating = Some(rating);
                            }
                        }
                    }
                    _ => reader.skip(),
                }
            } else {
                reader.read();
            }
        }
    }
}

/// Parses a `<lockedfields>` pipe part into a [`MetadataField`], if recognized.
fn parse_metadata_field(part: &str) -> Option<MetadataField> {
    const FIELDS: [MetadataField; 9] = [
        MetadataField::Cast,
        MetadataField::Genres,
        MetadataField::ProductionLocations,
        MetadataField::Studios,
        MetadataField::Tags,
        MetadataField::Name,
        MetadataField::Overview,
        MetadataField::Runtime,
        MetadataField::OfficialRating,
    ];
    let part = part.trim();
    FIELDS
        .into_iter()
        .find(|f| format!("{f:?}").eq_ignore_ascii_case(part))
}

/// Parses a Kodi `<format3d>` value into a [`Video3DFormat`].
fn parse_3d_format(format: &str) -> Option<hermit_model::entities::Video3DFormat> {
    use hermit_model::entities::Video3DFormat;
    let format = format.trim();
    if format.eq_ignore_ascii_case("HSBS") {
        Some(Video3DFormat::HalfSideBySide)
    } else if format.eq_ignore_ascii_case("HTAB") {
        Some(Video3DFormat::HalfTopAndBottom)
    } else if format.eq_ignore_ascii_case("FTAB") {
        Some(Video3DFormat::FullTopAndBottom)
    } else if format.eq_ignore_ascii_case("FSBS") {
        Some(Video3DFormat::FullSideBySide)
    } else if format.eq_ignore_ascii_case("MVC") {
        Some(Video3DFormat::Mvc)
    } else {
        None
    }
}

/// Case-insensitively strips `prefix` from `text`, returning the remainder.
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

/// Whether `val` parses as an absolute URI (`Uri.TryCreate(.., Absolute)`).
///
/// Accepts an http(s)/file scheme, a Unix rooted path (`/…`, which .NET treats
/// as a `file` URI), or a Windows drive path (`X:\…`).
fn is_url(val: &str) -> bool {
    is_http_url(val) || is_file_path(val) || val.to_ascii_lowercase().starts_with("file:")
}

/// Whether `val` is an http(s) URL.
fn is_http_url(val: &str) -> bool {
    let lower = val.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Whether `val` is a local file path (`Uri.IsFile`): a Unix rooted path, a
/// Windows drive path, or an explicit `file:` URI.
fn is_file_path(val: &str) -> bool {
    if val.to_ascii_lowercase().starts_with("file:") {
        return true;
    }
    if val.starts_with('/') {
        return true;
    }
    // Windows drive path, e.g. `C:\…`.
    let bytes = val.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\'
}

/// Advances a subtree cursor to the first descendant element named `name`
/// (`XmlReader.ReadToDescendant`).
fn read_to_descendant(reader: &mut XmlCursor, name: &str) -> bool {
    // Move off the subtree root, then scan for the named element.
    reader.move_to_content();
    reader.read();
    while !reader.eof() {
        if reader.is_element() && reader.name() == name {
            return true;
        }
        reader.read();
    }
    false
}
