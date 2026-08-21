//! Book and comic local metadata — port of `MediaBrowser.Providers/Books`.
//!
//! Four readers, tried in `ComicProvider`'s DI registration order — the first
//! with metadata wins — plus the two cover extractors:
//!
//! | Order | Source | C# | What it reads |
//! |---|---|---|---|
//! | 1 | the `.cbz` archive comment | `ComicBookInfoProvider` | the ComicBookInfo JSON schema |
//! | 2 | `<stem>.xml`, else `ComicInfo.xml`, beside the file | `ExternalComicInfoProvider` | the ComicRack schema, as a sidecar |
//! | 3 | `ComicInfo.xml` inside a `.cbz` | `InternalComicInfoProvider` | the same schema, bundled |
//! | 4 | `<stem>.opf`, `content.opf`, `metadata.opf`, or the one inside a `.epub` | `OpfProvider`/`EpubProvider` | Dublin Core + Calibre |
//!
//! `OpfProvider` is unfiltered, so step 4 runs for a comic too — a `.cbz` in a
//! Calibre directory resolves through it.
//!
//! Covers come from an explicitly named `cover.<ext>` in a comic archive, else
//! its first page in name order (`ComicImageProvider`), or the EPUB's declared
//! cover (`EpubImageProvider`).

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

use chrono::{DateTime, NaiveDate, TimeZone as _, Utc};
use serde::Deserialize;

use crate::xbmc::xml_reader::XmlCursor;

/// The comic-archive extensions Jellyfin recognizes
/// (`ComicImageProvider._comicBookExtensions`).
pub const COMIC_EXTENSIONS: [&str; 4] = ["cb7", "cbr", "cbt", "cbz"];

/// The archive extensions this port can actually open — ZIP, and only ZIP.
///
/// `.cbr` is RAR, `.cb7` is 7z and `.cbt` is tar; none has a maintained
/// pure-Rust reader worth a dependency for a cover image, so all three are
/// recognized as comics (so the file is still a Book) but yield no embedded
/// metadata or cover.
const READABLE_ARCHIVE_EXTENSIONS: [&str; 1] = ["cbz"];

/// The largest single archive entry this module will decompress into memory.
///
/// A cover page or an OPF document is kilobytes; the cap is what stops a
/// malicious `.cbz`/`.epub` from turning a scan into an OOM abort.
const MAX_ARCHIVE_ENTRY_BYTES: u64 = 128 * 1024 * 1024;

/// The image extensions a comic page may have, for the cover extractor.
const PAGE_EXTENSIONS: [&str; 6] = ["png", "jpeg", "jpg", "webp", "bmp", "gif"];

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
    /// `ForcedSortName` — the OPF's `file-as` refinement of the main title, or
    /// Calibre's `calibre:title_sort`.
    pub sort_name: Option<String>,
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
        // `ComicProvider` returns the first source with metadata, over the DI
        // registration order: ComicBookInfoProvider, then External (sidecar),
        // then Internal (in-archive). Reading the archive first would let a
        // stale bundled ComicInfo.xml beat the comment the user rewrote.
        // 1. The ComicBookInfo JSON in the archive comment.
        if let Some(comment) = read_archive_comment(path)
            && let Some(book) = parse_comic_book_info(&comment)
        {
            return Some(book);
        }
        // 2. A sidecar next to the file. C# `GetXmlFilePath` prefers
        //    `<stem>.xml` and only then the fixed `ComicInfo.xml`.
        if let Some(xml) = read_sidecar(path, &["xml"], &["ComicInfo.xml"])
            && let Some(book) = parse_comic_info(&xml)
        {
            return Some(book);
        }
        // 3. ComicInfo.xml inside the archive.
        if let Some(xml) = read_archive_entry_text(path, "comicinfo.xml")
            && let Some(book) = parse_comic_info(&xml)
        {
            return Some(book);
        }
        // `OpfProvider` is an unfiltered `ILocalMetadataProvider<Book>`, so it
        // runs for a comic too: a `.cbz` in a Calibre directory still resolves
        // through the sidecar chain below.
    }
    // A plain book file may still have an `.opf` sidecar. C# `GetXmlFile`
    // probes `<stem>.opf` (most specific), then `content.opf`, then Calibre's
    // `metadata.opf` — a Calibre library is the whole reason the last one is
    // there, and without it such a library reads nothing.
    read_sidecar(path, &["opf"], &["content.opf", "metadata.opf"]).and_then(|xml| parse_opf(&xml))
}

/// Reads the first sidecar that exists beside `path`: one named after the
/// file's own stem with each of `stem_extensions`, then each of `fixed_names`
/// in the file's directory.
fn read_sidecar(path: &str, stem_extensions: &[&str], fixed_names: &[&str]) -> Option<String> {
    let file = Path::new(path);
    let dir = file.parent()?;
    for extension in stem_extensions {
        if let Ok(xml) = std::fs::read_to_string(file.with_extension(extension)) {
            return Some(xml);
        }
    }
    fixed_names
        .iter()
        .find_map(|name| std::fs::read_to_string(dir.join(name)).ok())
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
        (Some(year), Some(month)) => u32::try_from(month)
            .ok()
            .and_then(|m| two_part_date(year, m)),
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
        book.people.push((name, person_kind(&role)));
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
    // EPUB 3 refines the title through sibling `<meta property=…>` elements
    // that point back at a `<dc:title id=…>`. They can appear either side of
    // the title they refine, so both are collected and resolved at the end.
    let mut titles: Vec<(Option<String>, String)> = Vec::new();
    let mut refinements: Vec<(String, String, String)> = Vec::new();
    let mut calibre_title_sort: Option<String> = None;
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
        // which carry no element text; EPUB 3 refinements carry theirs as
        // element text keyed by `property`.
        if name == "meta" || name.ends_with(":meta") {
            if read_opf_meta(
                &mut cursor,
                &mut book,
                &mut refinements,
                &mut calibre_title_sort,
            ) {
                continue;
            }
            cursor.read();
            continue;
        }
        let element_id = cursor.get_attribute("id").map(str::to_owned);
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
            "title" => titles.push((element_id, value.to_owned())),
            "description" => book.overview = Some(value.to_owned()),
            "publisher" => book.studios.push(value.to_owned()),
            "language" => book.language = Some(value.to_owned()),
            // "specification has no rules about content and some books combine
            // every genre into a single element" — C# splits on all five.
            "subject" => book.genres.extend(split_genres(value)),
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
                let kind = opf_role(role.as_deref());
                for name in split_creators(value) {
                    book.people.push((name, kind.clone()));
                }
            }
            _ => {}
        }
    }
    // C# `FindMainTitle`: the title a `title-type` of `main` refines wins (the
    // loop keeps the LAST such match), else the first `<dc:title>`. Without
    // this an EPUB 3 that also declares a subtitle can name the book after it.
    book.name = refinements
        .iter()
        .filter(|(property, _, value)| {
            property == "title-type" && value.eq_ignore_ascii_case("main")
        })
        .filter_map(|(_, refines, _)| title_with_id(&titles, refines))
        .next_back()
        .or_else(|| titles.first().map(|(_, text)| text.clone()));
    // C# `FindSortTitle`: the first `file-as` refining a real title, then
    // OPF 2.0's `calibre:title_sort`.
    book.sort_name = refinements
        .iter()
        .filter(|(property, _, _)| property == "file-as")
        .find(|(_, refines, _)| title_with_id(&titles, refines).is_some())
        .map(|(_, _, value)| value.clone())
        .or(calibre_title_sort);
    book.has_metadata().then_some(book)
}

/// Applies one `<meta>` element to the book being parsed.
///
/// Returns `true` when the cursor was already advanced past the element (the
/// refinement branch reads its text), `false` when the caller must advance it.
fn read_opf_meta(
    cursor: &mut XmlCursor,
    book: &mut BookMetadata,
    refinements: &mut Vec<(String, String, String)>,
    calibre_title_sort: &mut Option<String>,
) -> bool {
    let meta_name = cursor.get_attribute("name").unwrap_or_default().to_owned();
    let content = cursor
        .get_attribute("content")
        .unwrap_or_default()
        .to_owned();
    match meta_name.to_ascii_lowercase().as_str() {
        "calibre:series" => book.series_name = non_empty(Some(content)),
        // Calibre writes this as a float (`1.0`); C# rounds via
        // `Convert.ToInt32(Convert.ToDouble(value))`.
        "calibre:series_index" => {
            book.index_number = content
                .trim()
                .parse::<f64>()
                .ok()
                .map(f64::round)
                .filter(|v| v.is_finite() && v.abs() <= f64::from(i32::MAX))
                .map(|v| {
                    #[allow(clippy::cast_possible_truncation)]
                    let rounded = v as i32;
                    rounded
                });
        }
        "calibre:rating" => book.community_rating = content.trim().parse().ok(),
        // OPF 2.0's sort title.
        "calibre:title_sort" => *calibre_title_sort = non_empty(Some(content)),
        _ => {}
    }
    // EPUB 3 refinements carry their value as element TEXT, keyed by
    // `property` and pointing at `refines="#id"`.
    let property = cursor
        .get_attribute("property")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(property.as_str(), "title-type" | "file-as") {
        return false;
    }
    let refines = cursor
        .get_attribute("refines")
        .unwrap_or_default()
        .trim_start_matches('#')
        .to_owned();
    let value = cursor.read_element_content_as_string();
    let value = value.trim();
    if !refines.is_empty() && !value.is_empty() {
        refinements.push((property, refines, value.to_owned()));
    }
    true
}

/// The text of the collected `<dc:title>` carrying `id`.
fn title_with_id(titles: &[(Option<String>, String)], id: &str) -> Option<String> {
    titles
        .iter()
        .find(|(title_id, _)| title_id.as_deref() == Some(id))
        .map(|(_, text)| text.clone())
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

/// A ComicBookInfo credit role as a `PersonKind` name — port of C#'s
/// `Enum.TryParse<PersonKind>(role)` with its `Unknown` fallback and the
/// explicit `"Colorer"` → `Colorist` special case.
fn person_kind(role: &str) -> String {
    if role.eq_ignore_ascii_case("Colorer") {
        return "Colorist".to_owned();
    }
    PERSON_KINDS
        .iter()
        .find(|kind| kind.eq_ignore_ascii_case(role))
        .map_or_else(|| "Unknown".to_owned(), |kind| (*kind).to_owned())
}

/// The `PersonKind` names `Enum.TryParse` accepts, from
/// `Jellyfin.Data/Enums/PersonKind.cs`.
const PERSON_KINDS: [&str; 25] = [
    "Unknown",
    "Actor",
    "Director",
    "Composer",
    "Writer",
    "GuestStar",
    "Producer",
    "Conductor",
    "Lyricist",
    "Arranger",
    "Engineer",
    "Mixer",
    "Remixer",
    "Creator",
    "Artist",
    "AlbumArtist",
    "Author",
    "Illustrator",
    "Penciller",
    "Inker",
    "Colorist",
    "Letterer",
    "CoverArtist",
    "Editor",
    "Translator",
];

/// The person kind an OPF `opf:role` MARC relator marks — port of
/// `OpfReader.GetRole`, including its default-to-`Author` fallthrough.
fn opf_role(role: Option<&str>) -> String {
    match role.map(str::trim) {
        Some("arr") => "Arranger",
        Some("art") => "Artist",
        Some("edt") => "Editor",
        Some("ill") => "Illustrator",
        Some("lyr") => "Lyricist",
        Some("mus") => "AlbumArtist",
        Some("nrt") => "Narrator",
        Some("oth") => "Unknown",
        Some("trl") => "Translator",
        // `aut`/`aqt`/`aft`/`aui`, an unknown relator, and no role at all.
        _ => "Author",
    }
    .to_owned()
}

/// Splits one `<dc:creator>` into names — port of `FindAuthors`: entries are
/// separated by `;`, a `"Last, First"` pair is flipped, and an initial that is
/// not already followed by a space gets one (`"J.R.R. Tolkien"`).
fn split_creators(value: &str) -> Vec<String> {
    value
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let flipped = match entry.split_once(',') {
                Some((last, first)) if !last.trim().is_empty() && !first.trim().is_empty() => {
                    format!("{} {}", first.trim(), last.trim())
                }
                _ => entry.to_owned(),
            };
            expand_initials(&flipped)
        })
        .collect()
}

/// C# `InitialsRegex` — `(?<=\p{L})\.(?!\s|$)` replaced by `". "`: a period
/// that follows a letter and is not already followed by whitespace or the end
/// of the string gains a space.
fn expand_initials(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len());
    for (index, ch) in chars.iter().enumerate() {
        out.push(*ch);
        if *ch != '.' {
            continue;
        }
        let follows_letter = index
            .checked_sub(1)
            .and_then(|i| chars.get(i))
            .is_some_and(|prev| prev.is_alphabetic());
        let next_is_space_or_end = chars.get(index + 1).is_none_or(|next| next.is_whitespace());
        if follows_letter && !next_is_space_or_end {
            out.push(' ');
        }
    }
    out
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
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|e| e.name().to_owned()))
        .collect();
    // C# `FindCoverEntryInArchiveAsync` looks for an entry literally named
    // `cover.<ext>` first, in ITS extension order, and only then falls back to
    // the first image in name order. Skipping that pass hands back page 001 for
    // every comic that names its cover explicitly.
    let explicit = PAGE_EXTENSIONS.iter().find_map(|ext| {
        names
            .iter()
            .find(|n| *n == &format!("cover.{ext}"))
            .cloned()
    });
    let first = if let Some(cover) = explicit {
        cover
    } else {
        let mut pages: Vec<&String> = names
            .iter()
            .filter(|name| PAGE_EXTENSIONS.contains(&extension_of(name).as_str()))
            .collect();
        pages.sort();
        pages.into_iter().next()?.clone()
    };
    let mut entry = archive.by_name(&first).ok()?;
    let bytes = read_capped(&mut entry)?;
    Some((first, bytes))
}

/// The EPUB's declared cover image.
fn read_epub_cover(path: &str) -> Option<(String, Vec<u8>)> {
    let opf_path = read_epub_content_path(path)?;
    let opf = read_archive_entry_text(path, &opf_path.to_lowercase())?;
    // C# `ReadCoverPath` probes in order: a manifest item with
    // `properties="cover-image"` (the EPUB 3 way), then id `cover-image`,
    // then id `cover`, and only then `<meta name="cover" content="…"/>`.
    let href = epub_manifest_href_by_property(&opf, "cover-image")
        .or_else(|| epub_manifest_href(&opf, "cover"))
        .or_else(|| epub_cover_id(&opf).and_then(|id| epub_manifest_href(&opf, &id)))?;
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

/// The `href` of the manifest item carrying `properties="…"` — the EPUB 3
/// declaration, which most modern books use instead of the `<meta>` pointer.
fn epub_manifest_href_by_property(opf: &str, property: &str) -> Option<String> {
    let Ok(mut cursor) = XmlCursor::new(opf) else {
        return None;
    };
    while !cursor.eof() {
        if cursor.is_element()
            && cursor.name().eq_ignore_ascii_case("item")
            && cursor
                .get_attribute("properties")
                .is_some_and(|v| v.split_whitespace().any(|p| p == property))
            && cursor
                .get_attribute("media-type")
                .is_some_and(is_image_media_type)
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

/// Whether a manifest `media-type` names an image — C# `IsValidImage`.
fn is_image_media_type(media_type: &str) -> bool {
    media_type.trim().to_ascii_lowercase().starts_with("image/")
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
            // C# probes `//opf:item[@id='cover' and @media-type='image/*']`:
            // the common EPUB 2 `id="cover"` points at an XHTML wrapper page,
            // and attaching that as the cover would store markup as an image.
            && cursor.get_attribute("media-type").is_some_and(is_image_media_type)
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
    read_capped(&mut entry)
}

/// Reads one archive entry, refusing anything that inflates past
/// [`MAX_ARCHIVE_ENTRY_BYTES`].
///
/// A comic page or an OPF is kilobytes. The cap is enforced on the BYTES READ,
/// not on `ZipFile.size()`: that header field is attacker-controlled, so a
/// zip bomb declaring a small size would sail past a header check and then
/// inflate unbounded into memory. C# catches the resulting exception; an
/// unbounded read here would abort the process on OOM instead.
fn read_capped(entry: &mut zip::read::ZipFile<'_, impl std::io::Read>) -> Option<Vec<u8>> {
    read_capped_to(entry, MAX_ARCHIVE_ENTRY_BYTES)
}

/// [`read_capped`] with an explicit cap, so the read-side enforcement can be
/// exercised without a 128 MiB fixture.
fn read_capped_to(
    entry: &mut zip::read::ZipFile<'_, impl std::io::Read>,
    cap: u64,
) -> Option<Vec<u8>> {
    let name = entry.name().to_owned();
    // The declared size is still worth honouring as a capacity hint and as a
    // cheap early-out, but only ever as a hint.
    if entry.size() > cap {
        tracing::warn!(
            entry = name,
            size = entry.size(),
            "archive entry is implausibly large; skipping"
        );
        return None;
    }
    // The capacity hint is bounded by the cap as well as by the declared size,
    // so a lying header cannot make this allocate more than one capped read
    // could ever fill.
    let hint = usize::try_from(entry.size().min(cap)).unwrap_or_default();
    let mut bytes = Vec::with_capacity(hint);
    // One byte past the cap is enough to tell "at the limit" from "over it".
    entry
        .take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > cap {
        tracing::warn!(
            entry = name,
            "archive entry inflated past the cap; skipping"
        );
        return None;
    }
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

/// Splits an OPF `<dc:subject>` on the separators C# uses (`/`, `&`, `,`, `;`
/// and `" - "`), trimming and dropping empties.
fn split_genres(value: &str) -> Vec<String> {
    value
        .replace(" - ", ",")
        .replace(['/', '&', ';'], ",")
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect()
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

/// A `(year, month, day)` triple as a UTC instant.
///
/// C# `ReadThreePartDateInto` initialises all three parts to `0` and lets
/// `new DateTime(year, month, day)` throw for a missing part, catching the
/// exception and recording no date — so a `ComicInfo.xml` carrying only
/// `<Year>` yields **no** `PremiereDate`, not January 1st.
fn three_part_date(
    year: Option<i32>,
    month: Option<u32>,
    day: Option<u32>,
) -> Option<DateTime<Utc>> {
    let date = NaiveDate::from_ymd_opt(year?, month?, day?)?;
    Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?))
}

/// A `(year, month)` pair as a UTC instant — C# `ReadTwoPartDateInto`, which
/// passes `1` for the day, so a year+month pair *does* yield a date.
fn two_part_date(year: i32, month: u32) -> Option<DateTime<Utc>> {
    let date = NaiveDate::from_ymd_opt(year, month, 1)?;
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
    fn a_year_only_comic_info_has_no_premiere_date() {
        // C# builds the date from three parts initialised to 0 and lets the
        // DateTime constructor throw, so a Year on its own yields no date —
        // NOT January 1st.
        let book = parse_comic_info(r"<ComicInfo><Title>T</Title><Year>1988</Year></ComicInfo>")
            .expect("metadata");
        assert_eq!(book.production_year, Some(1988));
        assert_eq!(book.premiere_date, None);

        // All three parts present still yields a date.
        let full = parse_comic_info(
            r"<ComicInfo><Year>1988</Year><Month>3</Month><Day>29</Day></ComicInfo>",
        )
        .expect("metadata");
        assert_eq!(
            full.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
            Some("1988-03-29".to_owned())
        );
    }

    #[test]
    fn comic_book_info_credits_map_to_real_person_kinds() {
        let book = parse_comic_book_info(
            r#"{"ComicBookInfo/1.0":{"title":"T","credits":[
                {"person":"A","role":"Penciller"},
                {"person":"B","role":"Colorer"},
                {"person":"C","role":"Chief Vibes Officer"}]}}"#,
        )
        .expect("metadata");
        let kinds: Vec<&str> = book.people.iter().map(|(_, k)| k.as_str()).collect();
        // A real PersonKind survives, "Colorer" is normalized, and anything
        // Enum.TryParse would reject becomes Unknown rather than a raw string.
        assert_eq!(kinds, ["Penciller", "Colorist", "Unknown"]);
    }

    #[test]
    fn opf_roles_map_marc_relators_and_default_to_author() {
        assert_eq!(opf_role(Some("edt")), "Editor");
        assert_eq!(opf_role(Some("ill")), "Illustrator");
        assert_eq!(opf_role(Some("trl")), "Translator");
        assert_eq!(opf_role(Some("oth")), "Unknown");
        // `aut` and every unknown relator fall through to Author, as C# does.
        assert_eq!(opf_role(Some("aut")), "Author");
        assert_eq!(opf_role(Some("zzz")), "Author");
        assert_eq!(opf_role(None), "Author");
    }

    #[test]
    fn opf_creators_are_split_flipped_and_initial_spaced() {
        let book = parse_opf(
            r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/">
                 <metadata>
                   <dc:title>T</dc:title>
                   <dc:creator>Herbert, Frank; Tolkien, J.R.R.</dc:creator>
                 </metadata></package>"#,
        )
        .expect("metadata");
        let names: Vec<&str> = book.people.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Frank Herbert", "J. R. R. Tolkien"]);
    }

    #[test]
    fn a_calibre_float_series_index_is_rounded() {
        // Calibre writes `1.0`, which an integer parse would reject outright.
        let book = parse_opf(
            r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/">
                 <metadata><dc:title>T</dc:title>
                 <meta name="calibre:series_index" content="1.0"/></metadata></package>"#,
        )
        .expect("metadata");
        assert_eq!(book.index_number, Some(1));
    }

    #[test]
    fn a_combined_subject_element_is_split_on_every_upstream_separator() {
        let book = parse_opf(
            r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/">
                 <metadata><dc:title>T</dc:title>
                 <dc:subject>Fantasy / Adventure &amp; Myth; Epic - Classic</dc:subject>
                 </metadata></package>"#,
        )
        .expect("metadata");
        assert_eq!(
            book.genres,
            ["Fantasy", "Adventure", "Myth", "Epic", "Classic"]
        );
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
    fn the_archive_comment_outranks_a_bundled_comic_info() {
        // `ComicProvider` returns the first source with metadata over the DI
        // order ComicBookInfo → external sidecar → in-archive. A bundled
        // ComicInfo.xml must not beat the comment.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_archive(
            &dir,
            "issue.cbz",
            &[(
                "ComicInfo.xml",
                br"<ComicInfo><Title>From the archive</Title></ComicInfo>",
            )],
            Some(r#"{"appID":"t","ComicBookInfo/1.0":{"title":"From the comment"}}"#),
        );
        let book = read_book_metadata(&path).expect("metadata");
        assert_eq!(book.name.as_deref(), Some("From the comment"));

        // …and a sidecar outranks the bundled file, but not the comment.
        let plain = write_archive(
            &dir,
            "other.cbz",
            &[(
                "ComicInfo.xml",
                br"<ComicInfo><Title>From the archive</Title></ComicInfo>",
            )],
            None,
        );
        std::fs::write(
            dir.path().join("ComicInfo.xml"),
            r"<ComicInfo><Title>From the sidecar</Title></ComicInfo>",
        )
        .expect("write sidecar");
        let book = read_book_metadata(&plain).expect("metadata");
        assert_eq!(book.name.as_deref(), Some("From the sidecar"));
    }

    #[test]
    fn the_main_title_and_its_sort_form_win_over_a_subtitle() {
        // C# `FindMainTitle` prefers the `<dc:title>` a `title-type` of `main`
        // refines; `FindSortTitle` takes the matching `file-as`. Without the
        // refinement pass an EPUB 3 gets named after whichever title came
        // first, which is often the subtitle.
        let parsed = parse_opf(
            r##"<package xmlns:dc="http://purl.org/dc/elements/1.1/"
                        xmlns:opf="http://www.idpf.org/2007/opf">
                 <metadata>
                   <dc:title id="sub">A Tale of Two Halves</dc:title>
                   <dc:title id="main">The Hobbit</dc:title>
                   <meta refines="#sub" property="title-type">subtitle</meta>
                   <meta refines="#main" property="title-type">main</meta>
                   <meta refines="#main" property="file-as">Hobbit, The</meta>
                 </metadata>
               </package>"##,
        )
        .expect("parse");
        assert_eq!(parsed.name.as_deref(), Some("The Hobbit"));
        assert_eq!(parsed.sort_name.as_deref(), Some("Hobbit, The"));
    }

    #[test]
    fn an_opf_2_falls_back_to_the_first_title_and_calibre_sort() {
        let parsed = parse_opf(
            r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/"
                        xmlns:opf="http://www.idpf.org/2007/opf">
                 <metadata>
                   <dc:title>The Silmarillion</dc:title>
                   <meta name="calibre:title_sort" content="Silmarillion, The"/>
                 </metadata>
               </package>"#,
        )
        .expect("parse");
        assert_eq!(parsed.name.as_deref(), Some("The Silmarillion"));
        assert_eq!(parsed.sort_name.as_deref(), Some("Silmarillion, The"));
    }

    #[test]
    fn a_calibre_metadata_opf_is_found_beside_the_book() {
        // C# `GetXmlFile` probes `<stem>.opf`, then `content.opf`, then
        // Calibre's `metadata.opf`. Without the last one an entire Calibre
        // library reads nothing.
        let dir = tempfile::tempdir().expect("tempdir");
        let book = dir.path().join("book.azw3");
        std::fs::write(&book, b"").expect("write book");
        std::fs::write(
            dir.path().join("metadata.opf"),
            r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/"><metadata>
                 <dc:title>Calibre Title</dc:title></metadata></package>"#,
        )
        .expect("write opf");
        let parsed = read_book_metadata(&book.to_string_lossy()).expect("metadata");
        assert_eq!(parsed.name.as_deref(), Some("Calibre Title"));
    }

    #[test]
    fn an_explicitly_named_cover_beats_the_first_page() {
        // C# probes `cover.<ext>` over its own extension order before falling
        // back to the first image in name order.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_archive(
            &dir,
            "named.cbz",
            &[
                ("001.jpg", b"page-one"),
                ("cover.png", b"the-cover"),
                ("002.jpg", b"page-two"),
            ],
            None,
        );
        let (name, bytes) = read_book_cover(&path).expect("cover");
        assert_eq!(name, "cover.png");
        assert_eq!(bytes, b"the-cover");

        // With no explicit cover, the first image in name order wins.
        let path = write_archive(
            &dir,
            "unnamed.cbz",
            &[("002.jpg", b"page-two"), ("001.jpg", b"page-one")],
            None,
        );
        let (name, _) = read_book_cover(&path).expect("cover");
        assert_eq!(name, "001.jpg");
    }

    #[test]
    fn an_entry_that_inflates_past_the_cap_is_refused() {
        // The cap must bind on the BYTES READ, not on `ZipFile::size()` — that
        // header field is attacker-controlled, so a zip bomb declaring a tiny
        // size would sail past a header-only check and inflate unbounded.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_archive(&dir, "bomb.cbz", &[("page.jpg", &[b'A'; 4096])], None);
        let read = |cap: u64| {
            let file = std::fs::File::open(&path).expect("open");
            let mut archive =
                zip::ZipArchive::new(std::io::BufReader::new(file)).expect("read archive");
            let mut entry = archive.by_name("page.jpg").expect("entry");
            super::read_capped_to(&mut entry, cap).map(|b| b.len())
        };
        // An honest header well over the cap: refused before any read.
        assert_eq!(read(16), None);
        assert_eq!(read(8192), Some(4096));
        assert_eq!(read(4096), Some(4096), "content exactly at the cap is fine");

        // The case that actually exercises the READ-side cap: a header that
        // lies. `size()` claims 8 bytes so the early-out lets it through, but
        // the entry inflates to 4096. Patch both the local-file-header and the
        // central-directory uncompressed-size fields, which is exactly what a
        // zip bomb does.
        let mut raw = std::fs::read(&path).expect("read archive bytes");
        let lie = 8u32.to_le_bytes();
        // Local file header: PK\x03\x04, uncompressed size at +22.
        let local = raw
            .windows(4)
            .position(|w| w == b"PK\x03\x04")
            .expect("local header");
        raw[local + 22..local + 26].copy_from_slice(&lie);
        // Central directory: PK\x01\x02, uncompressed size at +24.
        let central = raw
            .windows(4)
            .position(|w| w == b"PK\x01\x02")
            .expect("central header");
        raw[central + 24..central + 28].copy_from_slice(&lie);
        let bomb = dir.path().join("lying.cbz");
        std::fs::write(&bomb, &raw).expect("write patched archive");

        let file = std::fs::File::open(&bomb).expect("open");
        let mut archive =
            zip::ZipArchive::new(std::io::BufReader::new(file)).expect("read archive");
        let mut entry = archive.by_name("page.jpg").expect("entry");
        assert_eq!(entry.size(), 8, "the header must be the one that lies");
        assert_eq!(
            super::read_capped_to(&mut entry, 64),
            None,
            "a lying header must not let 4096 bytes past a 64-byte cap"
        );
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
                          <manifest>
                            <item id="cover-page" href="cover.xhtml"
                                  media-type="application/xhtml+xml"/>
                            <item id="cover-image" href="cover.jpg"
                                  media-type="image/jpeg"/>
                          </manifest>
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
