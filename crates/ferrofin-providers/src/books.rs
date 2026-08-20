//! Book and comic local metadata — port of `MediaBrowser.Providers/Books`.
//!
//! Four readers, tried in the order `ComicProvider` tries them, plus the two
//! cover extractors:
//!
//! | Source | C# | What it reads |
//! |---|---|---|
//! | `ComicInfo.xml` inside a `.cbz` | `InternalComicInfoProvider` | the ComicRack schema |
//! | `ComicInfo.xml` beside the file | `ExternalComicInfoProvider` | the same schema, as a sidecar |
//! | the `.cbz` archive comment | `ComicBookInfoProvider` | the ComicBookInfo JSON schema |
//! | `.opf` (standalone or inside a `.epub`) | `OpfProvider`/`EpubProvider` | Dublin Core + Calibre |
//!
//! Covers come from the first image in a comic archive
//! (`ComicImageProvider`) or the EPUB's declared cover (`EpubImageProvider`).

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

use chrono::{DateTime, NaiveDate, TimeZone as _, Utc};
use serde::Deserialize;

use crate::xbmc::xml_reader::XmlCursor;

/// The comic-archive extensions Jellyfin recognizes
/// (`ComicImageProvider._comicBookExtensions`).
pub const COMIC_EXTENSIONS: [&str; 4] = ["cb7", "cbr", "cbt", "cbz"];

/// The archive extensions this port can actually open. `.cbr` is RAR and
/// `.cb7` is 7z; neither has a maintained pure-Rust reader worth a dependency
/// for a cover image, so those two are recognized as comics but yield no
/// embedded metadata or cover.
const READABLE_ARCHIVE_EXTENSIONS: [&str; 2] = ["cbz", "cbt"];

/// The image extensions a comic page may have, for the cover extractor.
const PAGE_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];

/// A book's parsed metadata — the subset of the C# `Book` entity the readers
/// fill.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BookMetadata {
    /// `Name` — the title.
    pub name: Option<String>,
    /// `OriginalTitle` — a manga's original-language series name.
    pub original_title: Option<String>,
    /// `SeriesName`.
    pub series_name: Option<String>,
    /// `IndexNumber` — the issue or series index.
    pub index_number: Option<i32>,
    /// `Overview`.
    pub overview: Option<String>,
    /// `ProductionYear`.
    pub production_year: Option<i32>,
    /// `PremiereDate`.
    pub premiere_date: Option<DateTime<Utc>>,
    /// `Genres`.
    pub genres: Vec<String>,
    /// `Studios` — the publisher.
    pub studios: Vec<String>,
    /// `Tags`.
    pub tags: Vec<String>,
    /// `CommunityRating`.
    pub community_rating: Option<f64>,
    /// The item's language, when the source declares one.
    pub language: Option<String>,
    /// `ProviderIds` — an EPUB's ISBN/Amazon/Google identifiers.
    pub provider_ids: HashMap<String, String>,
    /// The credited people as `(name, person kind)`.
    pub people: Vec<(String, String)>,
}

impl BookMetadata {
    /// Whether any field was filled — C#'s `hasFoundMetadata`, which decides
    /// whether the reader reports a hit at all.
    #[must_use]
    pub fn has_metadata(&self) -> bool {
        self.name.is_some()
            || self.series_name.is_some()
            || self.index_number.is_some()
            || self.overview.is_some()
            || self.production_year.is_some()
            || !self.genres.is_empty()
            || !self.studios.is_empty()
            || !self.tags.is_empty()
            || !self.provider_ids.is_empty()
    }
}

/// Reads a book's embedded/sidecar metadata, trying each source in the order
/// `ComicProvider` does and returning the first hit.
///
/// `None` means no source had anything — the item keeps its filename-derived
/// name, exactly as upstream leaves it.
#[must_use]
pub fn read_book_metadata(path: &str) -> Option<BookMetadata> {
    let extension = extension_of(path);
    if extension == "epub" {
        return read_epub_metadata(path);
    }
    if extension == "opf" {
        return std::fs::read_to_string(path)
            .ok()
            .and_then(|xml| parse_opf(&xml));
    }
    if COMIC_EXTENSIONS.contains(&extension.as_str()) {
        // 1. ComicInfo.xml inside the archive.
        if let Some(xml) = read_archive_entry_text(path, "comicinfo.xml")
            && let Some(book) = parse_comic_info(&xml)
        {
            return Some(book);
        }
        // 2. A ComicInfo.xml sidecar next to the file.
        if let Some(sidecar) = Path::new(path).parent().map(|d| d.join("ComicInfo.xml"))
            && let Ok(xml) = std::fs::read_to_string(&sidecar)
            && let Some(book) = parse_comic_info(&xml)
        {
            return Some(book);
        }
        // 3. The ComicBookInfo JSON in the archive comment.
        if let Some(comment) = read_archive_comment(path)
            && let Some(book) = parse_comic_book_info(&comment)
        {
            return Some(book);
        }
        return None;
    }
    // A plain book file may still have an `.opf` sidecar (Calibre's layout).
    let sidecar = Path::new(path).with_extension("opf");
    std::fs::read_to_string(sidecar)
        .ok()
        .and_then(|xml| parse_opf(&xml))
}

/// An EPUB's metadata: the OPF the container points at.
fn read_epub_metadata(path: &str) -> Option<BookMetadata> {
    let opf_path = read_epub_content_path(path)?;
    let xml = read_archive_entry_text(path, &opf_path.to_lowercase())?;
    parse_opf(&xml)
}

/// The OPF path an EPUB's `META-INF/container.xml` declares — port of
/// `EpubUtils.ReadContentFilePath`.
fn read_epub_content_path(path: &str) -> Option<String> {
    let container = read_archive_entry_text(path, "meta-inf/container.xml")?;
    let mut cursor = XmlCursor::new(&container).ok()?;
    while !cursor.eof() {
        if cursor.is_element()
            && cursor.name().eq_ignore_ascii_case("rootfile")
            && let Some(full_path) = cursor.get_attribute("full-path")
        {
            let full_path = full_path.trim().to_owned();
            if !full_path.is_empty() {
                return Some(full_path);
            }
        }
        cursor.read();
    }
    None
}

/// Parses a ComicRack `ComicInfo.xml` document — port of
/// `ComicInfoReader.ReadComicBookMetadata` + `ReadPeopleMetadata`.
#[must_use]
pub fn parse_comic_info(xml: &str) -> Option<BookMetadata> {
    let mut book = BookMetadata::default();
    let mut is_manga = false;
    let mut alternate_series = None;
    let (mut year, mut month, mut day) = (None, None, None);
    let Ok(mut cursor) = XmlCursor::new(xml) else {
        return None;
    };
    // Step into the root element, as the NFO parsers do: reading the root's own
    // content would swallow the whole document in one go.
    cursor.move_to_content();
    cursor.read();
    while !cursor.eof() {
        if !cursor.is_element() {
            cursor.read();
            continue;
        }
        let name = cursor.name().to_ascii_lowercase();
        let value = cursor.read_element_content_as_string();
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match name.as_str() {
            "title" => book.name = Some(value.to_owned()),
            "manga" => is_manga = value.eq_ignore_ascii_case("Yes"),
            "series" => book.series_name = Some(value.to_owned()),
            "number" => book.index_number = value.parse().ok(),
            "summary" => book.overview = Some(value.to_owned()),
            "year" => {
                year = value.parse::<i32>().ok();
                book.production_year = year;
            }
            "month" => month = value.parse::<u32>().ok(),
            "day" => day = value.parse::<u32>().ok(),
            "genre" => book.genres = split_commas(value),
            "publisher" => book.studios = vec![value.to_owned()],
            "languageiso" => book.language = Some(value.to_owned()),
            "alternateseries" => alternate_series = Some(value.to_owned()),
            "writer" => add_people(&mut book, value, "Author"),
            "penciller" => add_people(&mut book, value, "Penciller"),
            "inker" => add_people(&mut book, value, "Inker"),
            "letterer" => add_people(&mut book, value, "Letterer"),
            "coverartist" => add_people(&mut book, value, "CoverArtist"),
            "colourist" | "colorist" => add_people(&mut book, value, "Colorist"),
            _ => {}
        }
    }
    // ComicTagger stores a manga's original-language series in AlternateSeries.
    if is_manga {
        book.original_title = alternate_series;
    }
    book.premiere_date = three_part_date(year, month, day);
    book.has_metadata().then_some(book)
}

/// Parses a ComicBookInfo JSON document (a `.cbz`'s archive comment) — port of
/// `ComicBookInfoProvider.ReadComicBookMetadata`.
#[must_use]
pub fn parse_comic_book_info(json: &str) -> Option<BookMetadata> {
    let format: ComicBookInfoFormat = serde_json::from_str(json).ok()?;
    let comic = format.metadata?;
    let mut book = BookMetadata {
        name: non_empty(comic.title),
        series_name: non_empty(comic.series),
        overview: non_empty(comic.comments),
        production_year: comic.publication_year,
        index_number: comic.issue,
        tags: comic.tags,
        language: non_empty(comic.language),
        ..BookMetadata::default()
    };
    if let Some(genre) = non_empty(comic.genre) {
        book.genres = vec![genre];
    }
    if let Some(publisher) = non_empty(comic.publisher) {
        book.studios = vec![publisher];
    }
    book.premiere_date = match (comic.publication_year, comic.publication_month) {
        (Some(year), Some(month)) => three_part_date(Some(year), u32::try_from(month).ok(), None),
        _ => None,
    };
    for credit in comic.credits {
        let (Some(person), Some(role)) = (non_empty(credit.person), non_empty(credit.role)) else {
            continue;
        };
        // "Last, First" is stored by some taggers; C# flips it.
        let name = match person.split_once(',') {
            Some((last, first)) => format!("{} {}", first.trim(), last.trim()),
            None => person,
        };
        // C# parses the role as a PersonKind, mapping the "Colorer" spelling.
        let kind = if role.eq_ignore_ascii_case("Colorer") {
            "Colorist".to_owned()
        } else {
            role
        };
        book.people.push((name, kind));
    }
    book.has_metadata().then_some(book)
}

/// Parses an Open Packaging Format document — port of `OpfReader.ReadOpfData`.
///
/// Reads the Dublin Core elements plus the two Calibre `<meta>` extensions
/// Jellyfin looks for (`calibre:series`/`series_index`/`rating`).
#[must_use]
pub fn parse_opf(xml: &str) -> Option<BookMetadata> {
    let mut book = BookMetadata::default();
    let Ok(mut cursor) = XmlCursor::new(xml) else {
        return None;
    };
    cursor.move_to_content();
    cursor.read();
    while !cursor.eof() {
        if !cursor.is_element() {
            cursor.read();
            continue;
        }
        let name = cursor.name().to_ascii_lowercase();
        // Descend through the container elements: reading their content would
        // swallow every child in one string.
        if matches!(
            name.rsplit(':').next().unwrap_or(&name),
            "package" | "metadata" | "manifest" | "spine" | "guide"
        ) {
            cursor.read();
            continue;
        }
        // Calibre's series/rating live on `<meta name= content= />` attributes,
        // which carry no element text.
        if name == "meta" || name.ends_with(":meta") {
            let meta_name = cursor.get_attribute("name").unwrap_or_default().to_owned();
            let content = cursor
                .get_attribute("content")
                .unwrap_or_default()
                .to_owned();
            match meta_name.to_ascii_lowercase().as_str() {
                "calibre:series" => book.series_name = non_empty(Some(content)),
                "calibre:series_index" => book.index_number = content.trim().parse().ok(),
                "calibre:rating" => book.community_rating = content.trim().parse().ok(),
                _ => {}
            }
            cursor.read();
            continue;
        }
        let scheme = cursor
            .get_attribute("opf:scheme")
            .or_else(|| cursor.get_attribute("scheme"))
            .map(str::to_owned);
        let role = cursor
            .get_attribute("opf:role")
            .or_else(|| cursor.get_attribute("role"))
            .map(str::to_owned);
        let local = name.rsplit(':').next().unwrap_or(&name).to_owned();
        let value = cursor.read_element_content_as_string();
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match local.as_str() {
            "title" if book.name.is_none() => book.name = Some(value.to_owned()),
            "description" => book.overview = Some(value.to_owned()),
            "publisher" => book.studios.push(value.to_owned()),
            "language" => book.language = Some(value.to_owned()),
            "subject" => book.genres.extend(split_commas(value)),
            "date" => {
                if let Some(date) = parse_opf_date(value) {
                    book.production_year = Some(date.year());
                    book.premiere_date = Some(date.date);
                }
            }
            "identifier" => {
                if let Some(key) = scheme.as_deref().and_then(opf_identifier_key) {
                    book.provider_ids.insert(key.to_owned(), value.to_owned());
                }
            }
            "creator" => {
                book.people
                    .push((value.to_owned(), opf_role(role.as_deref())));
            }
            _ => {}
        }
    }
    book.has_metadata().then_some(book)
}

/// The `ProviderIds` key an OPF `opf:scheme` maps to, or `None` for a scheme
/// Jellyfin does not record.
fn opf_identifier_key(scheme: &str) -> Option<&'static str> {
    match scheme.trim().to_ascii_uppercase().as_str() {
        "AMAZON" => Some("Amazon"),
        "GOOGLE" => Some("GoogleBooks"),
        "ISBN" => Some("ISBN"),
        _ => None,
    }
}

/// The person kind an OPF `opf:role` marks. Jellyfin maps the MARC relator
/// `aut` to an author and treats everything else as an unknown credit.
fn opf_role(role: Option<&str>) -> String {
    match role.map(str::trim) {
        Some("aut") | None => "Author".to_owned(),
        Some(other) => other.to_owned(),
    }
}

/// A parsed OPF `<dc:date>`.
struct OpfDate {
    /// The instant, at midnight UTC.
    date: DateTime<Utc>,
}

impl OpfDate {
    /// The date's year.
    fn year(&self) -> i32 {
        self.date.format("%Y").to_string().parse().unwrap_or(0)
    }
}

/// Parses an OPF date, which may be a full timestamp or a bare `YYYY`.
fn parse_opf_date(value: &str) -> Option<OpfDate> {
    let value = value.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Some(OpfDate {
            date: parsed.with_timezone(&Utc),
        });
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(OpfDate {
            date: Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?),
        });
    }
    let year: i32 = value.get(..4)?.parse().ok()?;
    Some(OpfDate {
        date: Utc.from_utc_datetime(&NaiveDate::from_ymd_opt(year, 1, 1)?.and_hms_opt(0, 0, 0)?),
    })
}

/// The cover image bytes for a book, or `None` when none can be extracted.
///
/// A comic's cover is its first page (`ComicImageProvider`); an EPUB's is the
/// image its OPF declares as the cover (`EpubImageProvider`).
#[must_use]
pub fn read_book_cover(path: &str) -> Option<(String, Vec<u8>)> {
    let extension = extension_of(path);
    if extension == "epub" {
        return read_epub_cover(path);
    }
    if !READABLE_ARCHIVE_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    // The first page in name order, as a comic reader would open it.
    let mut pages: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|e| e.name().to_owned()))
        .filter(|name| PAGE_EXTENSIONS.contains(&extension_of(name).as_str()))
        .collect();
    pages.sort();
    let first = pages.into_iter().next()?;
    let mut entry = archive.by_name(&first).ok()?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).ok()?;
    Some((first, bytes))
}

/// The EPUB's declared cover image.
fn read_epub_cover(path: &str) -> Option<(String, Vec<u8>)> {
    let opf_path = read_epub_content_path(path)?;
    let opf = read_archive_entry_text(path, &opf_path.to_lowercase())?;
    let cover_id = epub_cover_id(&opf)?;
    let href = epub_manifest_href(&opf, &cover_id)?;
    // Manifest hrefs are relative to the OPF's own directory.
    let root = Path::new(&opf_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let entry = if root.is_empty() {
        href.clone()
    } else {
        format!("{root}/{href}")
    };
    let bytes = read_archive_entry_bytes(path, &entry.to_lowercase())?;
    Some((href, bytes))
}

/// The manifest id an EPUB's `<meta name="cover" content="…"/>` points at.
fn epub_cover_id(opf: &str) -> Option<String> {
    let Ok(mut cursor) = XmlCursor::new(opf) else {
        return None;
    };
    while !cursor.eof() {
        if cursor.is_element()
            && cursor.name().eq_ignore_ascii_case("meta")
            && cursor
                .get_attribute("name")
                .is_some_and(|n| n.eq_ignore_ascii_case("cover"))
            && let Some(content) = cursor.get_attribute("content")
        {
            let content = content.trim().to_owned();
            if !content.is_empty() {
                return Some(content);
            }
        }
        cursor.read();
    }
    None
}

/// The `href` of the manifest item with `id`.
fn epub_manifest_href(opf: &str, id: &str) -> Option<String> {
    let Ok(mut cursor) = XmlCursor::new(opf) else {
        return None;
    };
    while !cursor.eof() {
        if cursor.is_element()
            && cursor.name().eq_ignore_ascii_case("item")
            && cursor.get_attribute("id").is_some_and(|v| v == id)
            && let Some(href) = cursor.get_attribute("href")
        {
            let href = href.trim().to_owned();
            if !href.is_empty() {
                return Some(href);
            }
        }
        cursor.read();
    }
    None
}

/// One archive entry's bytes, matched case-insensitively by name.
fn read_archive_entry_bytes(path: &str, wanted_lower: &str) -> Option<Vec<u8>> {
    if !READABLE_ARCHIVE_EXTENSIONS.contains(&extension_of(path).as_str())
        && extension_of(path) != "epub"
    {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let index = (0..archive.len()).find(|i| {
        archive
            .by_index(*i)
            .ok()
            .is_some_and(|e| e.name().to_lowercase() == wanted_lower)
    })?;
    let mut entry = archive.by_index(index).ok()?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// One archive entry as UTF-8 text.
fn read_archive_entry_text(path: &str, wanted_lower: &str) -> Option<String> {
    let bytes = read_archive_entry_bytes(path, wanted_lower)?;
    String::from_utf8(bytes).ok()
}

/// A zip archive's comment, where ComicBookInfo stores its JSON.
fn read_archive_comment(path: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let archive = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let comment = String::from_utf8(archive.comment().to_vec()).ok()?;
    let comment = comment.trim();
    (!comment.is_empty()).then(|| comment.to_owned())
}

/// The lowercase extension (no dot) of a path, or `""`.
fn extension_of(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Splits a comma-separated list, trimming and dropping empties.
fn split_commas(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Adds one credit per comma-separated name.
fn add_people(book: &mut BookMetadata, value: &str, kind: &str) {
    for name in split_commas(value) {
        book.people.push((name, kind.to_owned()));
    }
}

/// A `None` for an absent or blank string.
fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// A `(year, month, day)` triple as a UTC instant; a missing month or day
/// defaults to 1, as C# `ReadThreePartDateInto` does.
fn three_part_date(
    year: Option<i32>,
    month: Option<u32>,
    day: Option<u32>,
) -> Option<DateTime<Utc>> {
    let date = NaiveDate::from_ymd_opt(year?, month.unwrap_or(1), day.unwrap_or(1))?;
    Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?))
}

/// The ComicBookInfo JSON envelope (`{"ComicBookInfo/1.0": {…}}`).
#[derive(Debug, Deserialize)]
struct ComicBookInfoFormat {
    #[serde(rename = "ComicBookInfo/1.0")]
    metadata: Option<ComicBookInfoMetadata>,
}

/// The ComicBookInfo metadata object.
#[derive(Debug, Deserialize)]
struct ComicBookInfoMetadata {
    #[serde(default)]
    series: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    publisher: Option<String>,
    #[serde(rename = "publicationMonth", default)]
    publication_month: Option<i32>,
    #[serde(rename = "publicationYear", default)]
    publication_year: Option<i32>,
    #[serde(default)]
    issue: Option<i32>,
    #[serde(default)]
    genre: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    credits: Vec<ComicBookInfoCredit>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    comments: Option<String>,
}

/// One ComicBookInfo credit.
#[derive(Debug, Deserialize)]
struct ComicBookInfoCredit {
    #[serde(default)]
    person: Option<String>,
    #[serde(default)]
    role: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comic_info_maps_the_comicrack_schema() {
        let book = parse_comic_info(
            r"<ComicInfo>
                <Title>The Killing Joke</Title>
                <Series>Batman</Series>
                <Number>1</Number>
                <Summary>One bad day.</Summary>
                <Year>1988</Year><Month>3</Month><Day>29</Day>
                <Genre>Superhero, Crime</Genre>
                <Publisher>DC Comics</Publisher>
                <Writer>Alan Moore</Writer>
                <Penciller>Brian Bolland</Penciller>
                <Colourist>John Higgins</Colourist>
              </ComicInfo>",
        )
        .expect("metadata");
        assert_eq!(book.name.as_deref(), Some("The Killing Joke"));
        assert_eq!(book.series_name.as_deref(), Some("Batman"));
        assert_eq!(book.index_number, Some(1));
        assert_eq!(book.overview.as_deref(), Some("One bad day."));
        assert_eq!(book.production_year, Some(1988));
        assert_eq!(book.genres, ["Superhero", "Crime"]);
        assert_eq!(book.studios, ["DC Comics"]);
        assert_eq!(
            book.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
            Some("1988-03-29".to_owned())
        );
        assert_eq!(
            book.people,
            [
                ("Alan Moore".to_owned(), "Author".to_owned()),
                ("Brian Bolland".to_owned(), "Penciller".to_owned()),
                ("John Higgins".to_owned(), "Colorist".to_owned()),
            ]
        );
    }

    #[test]
    fn a_manga_keeps_its_alternate_series_as_the_original_title() {
        // ComicTagger writes the original-language series into AlternateSeries.
        let manga = parse_comic_info(
            r"<ComicInfo><Series>Attack on Titan</Series><Manga>Yes</Manga>
              <AlternateSeries>Shingeki no Kyojin</AlternateSeries></ComicInfo>",
        )
        .expect("metadata");
        assert_eq!(manga.original_title.as_deref(), Some("Shingeki no Kyojin"));
        // A non-manga's AlternateSeries is a cross-over arc, not a title.
        let comic = parse_comic_info(
            r"<ComicInfo><Series>Batman</Series>
              <AlternateSeries>Knightfall</AlternateSeries></ComicInfo>",
        )
        .expect("metadata");
        assert_eq!(comic.original_title, None);
    }

    #[test]
    fn an_empty_comic_info_is_not_a_hit() {
        assert!(parse_comic_info("<ComicInfo></ComicInfo>").is_none());
        assert!(parse_comic_info("not xml at all").is_none());
    }

    #[test]
    fn comic_book_info_maps_its_json_schema() {
        let book = parse_comic_book_info(
            r#"{"appID":"x","ComicBookInfo/1.0":{
                "series":"Batman","title":"The Killing Joke","publisher":"DC Comics",
                "publicationMonth":3,"publicationYear":1988,"issue":1,
                "genre":"Superhero","comments":"One bad day.","tags":["classic"],
                "credits":[{"person":"Moore, Alan","role":"Writer"},
                           {"person":"John Higgins","role":"Colorer"}]}}"#,
        )
        .expect("metadata");
        assert_eq!(book.name.as_deref(), Some("The Killing Joke"));
        assert_eq!(book.tags, ["classic"]);
        assert_eq!(book.index_number, Some(1));
        assert_eq!(
            book.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
            Some("1988-03-01".to_owned())
        );
        // "Last, First" is flipped, and the Colorer spelling is normalized.
        assert_eq!(
            book.people,
            [
                ("Alan Moore".to_owned(), "Writer".to_owned()),
                ("John Higgins".to_owned(), "Colorist".to_owned()),
            ]
        );
    }

    #[test]
    fn a_comic_book_info_without_metadata_is_not_a_hit() {
        assert!(parse_comic_book_info(r#"{"appID":"x"}"#).is_none());
        assert!(parse_comic_book_info("{}").is_none());
    }

    #[test]
    fn opf_maps_dublin_core_and_calibre_fields() {
        let book = parse_opf(
            r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
                <metadata>
                  <dc:title>Dune</dc:title>
                  <dc:creator opf:role="aut">Frank Herbert</dc:creator>
                  <dc:description>Arrakis.</dc:description>
                  <dc:publisher>Chilton Books</dc:publisher>
                  <dc:language>en</dc:language>
                  <dc:subject>Science Fiction</dc:subject>
                  <dc:date>1965-08-01</dc:date>
                  <dc:identifier opf:scheme="ISBN">9780441013593</dc:identifier>
                  <meta name="calibre:series" content="Dune Chronicles"/>
                  <meta name="calibre:series_index" content="1"/>
                  <meta name="calibre:rating" content="8"/>
                </metadata>
              </package>"#,
        )
        .expect("metadata");
        assert_eq!(book.name.as_deref(), Some("Dune"));
        assert_eq!(book.overview.as_deref(), Some("Arrakis."));
        assert_eq!(book.studios, ["Chilton Books"]);
        assert_eq!(book.genres, ["Science Fiction"]);
        assert_eq!(book.production_year, Some(1965));
        assert_eq!(book.provider_ids["ISBN"], "9780441013593");
        assert_eq!(book.series_name.as_deref(), Some("Dune Chronicles"));
        assert_eq!(book.index_number, Some(1));
        assert_eq!(book.community_rating, Some(8.0));
        assert_eq!(
            book.people,
            [("Frank Herbert".to_owned(), "Author".to_owned())]
        );
    }

    #[test]
    fn a_bare_year_date_still_yields_a_production_year() {
        let book = parse_opf(
            r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/">
                <dc:title>T</dc:title><dc:date>1965</dc:date></package>"#,
        )
        .expect("metadata");
        assert_eq!(book.production_year, Some(1965));
    }

    #[test]
    fn an_unrecognized_identifier_scheme_is_not_recorded() {
        assert_eq!(opf_identifier_key("uuid"), None);
        assert_eq!(opf_identifier_key("isbn"), Some("ISBN"));
        assert_eq!(opf_identifier_key(" AMAZON "), Some("Amazon"));
    }

    /// Writes a zip archive of `(name, bytes)` entries, optionally with an
    /// archive comment, and returns its path.
    fn write_archive(
        dir: &tempfile::TempDir,
        name: &str,
        entries: &[(&str, &[u8])],
        comment: Option<&str>,
    ) -> String {
        use std::io::Write as _;
        let path = dir.path().join(name);
        let file = std::fs::File::create(&path).expect("create archive");
        let mut writer = zip::ZipWriter::new(file);
        if let Some(comment) = comment {
            writer.set_comment(comment).expect("set comment");
        }
        for (entry, bytes) in entries {
            writer
                .start_file(*entry, zip::write::SimpleFileOptions::default())
                .expect("start entry");
            writer.write_all(bytes).expect("write entry");
        }
        writer.finish().expect("finish archive");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_cbz_is_read_from_its_internal_comic_info() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_archive(
            &dir,
            "batman.cbz",
            &[
                (
                    "ComicInfo.xml",
                    b"<ComicInfo><Title>The Killing Joke</Title><Number>1</Number></ComicInfo>",
                ),
                ("002.jpg", b"second page"),
                ("001.jpg", b"first page"),
            ],
            None,
        );
        let book = read_book_metadata(&path).expect("metadata");
        assert_eq!(book.name.as_deref(), Some("The Killing Joke"));
        assert_eq!(book.index_number, Some(1));

        // The cover is the first page in name order, not archive order.
        let (name, bytes) = read_book_cover(&path).expect("cover");
        assert_eq!(name, "001.jpg");
        assert_eq!(bytes, b"first page");
    }

    #[test]
    fn a_cbz_falls_back_to_a_sidecar_then_to_the_archive_comment() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No internal ComicInfo, but a sidecar next to it.
        let path = write_archive(&dir, "a.cbz", &[("001.jpg", b"page")], None);
        std::fs::write(
            dir.path().join("ComicInfo.xml"),
            "<ComicInfo><Title>From the sidecar</Title></ComicInfo>",
        )
        .expect("write sidecar");
        assert_eq!(
            read_book_metadata(&path).and_then(|b| b.name).as_deref(),
            Some("From the sidecar")
        );

        // A different directory, with only the archive comment.
        let other = tempfile::tempdir().expect("tempdir");
        let commented = write_archive(
            &other,
            "b.cbz",
            &[("001.jpg", b"page")],
            Some(r#"{"ComicBookInfo/1.0":{"title":"From the comment"}}"#),
        );
        assert_eq!(
            read_book_metadata(&commented)
                .and_then(|b| b.name)
                .as_deref(),
            Some("From the comment")
        );
    }

    #[test]
    fn a_comic_with_no_metadata_anywhere_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_archive(&dir, "bare.cbz", &[("001.jpg", b"page")], None);
        assert!(read_book_metadata(&path).is_none());
    }

    #[test]
    fn an_epub_is_read_through_its_container_and_opf() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_archive(
            &dir,
            "dune.epub",
            &[
                (
                    "META-INF/container.xml",
                    br#"<container><rootfiles>
                          <rootfile full-path="OEBPS/content.opf"/>
                        </rootfiles></container>"#,
                ),
                (
                    "OEBPS/content.opf",
                    br#"<package xmlns:dc="http://purl.org/dc/elements/1.1/">
                          <metadata>
                            <dc:title>Dune</dc:title>
                            <meta name="cover" content="cover-image"/>
                          </metadata>
                          <manifest><item id="cover-image" href="cover.jpg"/></manifest>
                        </package>"#,
                ),
                ("OEBPS/cover.jpg", b"cover bytes"),
            ],
            None,
        );
        assert_eq!(
            read_book_metadata(&path).and_then(|b| b.name).as_deref(),
            Some("Dune")
        );
        let (name, bytes) = read_book_cover(&path).expect("cover");
        assert_eq!(name, "cover.jpg");
        assert_eq!(bytes, b"cover bytes");
    }

    #[test]
    fn an_unreadable_archive_format_yields_nothing() {
        // .cbr is RAR: recognized as a comic, but nothing is extracted.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("x.cbr");
        std::fs::write(&path, b"Rar!\x1a\x07\x00").expect("write");
        let path = path.to_string_lossy().into_owned();
        assert!(read_book_metadata(&path).is_none());
        assert!(read_book_cover(&path).is_none());
    }

    #[test]
    fn a_book_with_an_opf_sidecar_is_read_from_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("dune.mobi"), b"book").expect("write");
        std::fs::write(
            dir.path().join("dune.opf"),
            r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/">
                 <metadata><dc:title>Dune</dc:title></metadata></package>"#,
        )
        .expect("write opf");
        let path = dir.path().join("dune.mobi").to_string_lossy().into_owned();
        assert_eq!(
            read_book_metadata(&path).and_then(|b| b.name).as_deref(),
            Some("Dune")
        );
    }

    #[test]
    fn a_missing_file_is_never_an_error() {
        assert!(read_book_metadata("/nope/missing.cbz").is_none());
        assert!(read_book_cover("/nope/missing.epub").is_none());
    }

    #[test]
    fn extensions_are_read_case_insensitively() {
        assert_eq!(extension_of("/b/Book.EPUB"), "epub");
        assert_eq!(extension_of("/b/comic.cbz"), "cbz");
        assert_eq!(extension_of("/b/no-extension"), "");
    }
}
