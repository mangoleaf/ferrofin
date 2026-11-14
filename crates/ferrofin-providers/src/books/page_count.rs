//! A book's runtime from its pages: the `Book` arm of upstream's
//! `ProbeProvider.FetchAsync` (`MediaBrowser.Providers/MediaInfo/
//! ProbeProvider.cs:246-294`, Jellyfin PR 17311).
//!
//! A comic archive's pages are its non-directory entries, read from the
//! archive's directory alone: the ZIP central directory, the 7z header, the
//! tar headers. A PDF's are the `/Count` of its page-tree root, reached with
//! seeks through the trailer and the cross-reference sections, as PDFium reads
//! it for upstream. Neither reads a page.

use std::cell::Cell;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

use super::{ArchiveFormat, MAX_ARCHIVE_ENTRY_BYTES, extension_of, open_comic};

/// `TimeSpan.TicksPerSecond`: the runtime of a book whose pages are not
/// counted. Upstream gives it to an EPUB, whose readers report progress as a
/// fraction of one second; Ferrofin gives it to a few more formats
/// ([`book_run_time_ticks`]).
pub const NOMINAL_BOOK_RUN_TIME_TICKS: i64 = 10_000_000;

/// One page's runtime ticks: jellyfin-web's comic and PDF readers report the
/// page as `currentTime()`, and its playback manager sends
/// `Math.floor(10000 * currentTime())` as the position.
const TICKS_PER_PAGE: i64 = 10_000;

/// The most entries a comic archive may hold for its pages to be counted
/// when the server sets no other (`book_archive_max_entries`).
pub const DEFAULT_ARCHIVE_MAX_ENTRIES: u32 = 100_000;

/// The runtime upstream's `ProbeProvider.FetchAsync(Book)` gives the book at
/// `path`, or `None` where it leaves the item's runtime as it is:
///
/// - `.cb7`, `.cbr`, `.cbt`, `.cbz`: the archive's non-directory entries ×
///   10000. The archive is read as what its content is, as SharpCompress's
///   `ArchiveFactory` reads it, whatever its extension says.
/// - `.pdf`: its pages × 10000.
/// - `.epub`: one second.
/// - `.mobi`, `.azw`, `.azw3`: one second, an accepted divergence (owner
///   decision on gl-495, 2026-10-31). Upstream gives them no runtime, and
///   with none `UserDataManager.UpdatePlayState` marks the book played and
///   clears its position on every stop, so a client's reading position for
///   them would be lost.
///
/// `None` for an archive or a PDF that cannot be read, or an archive with more
/// than `max_entries` entries: upstream's provider throws there, and the item
/// keeps its runtime. `None` too for a RAR archive, which nothing here reads
/// yet (gl-747).
#[must_use]
pub fn book_run_time_ticks(path: &str, max_entries: u32) -> Option<i64> {
    let pages = match extension_of(path).as_str() {
        "cb7" | "cbr" | "cbt" | "cbz" => match open_comic(path)? {
            // TODO(parity, open work item — NOT an accepted divergence;
            // gl-747): SharpCompress counts a RAR archive's entries too. Until
            // gl-747 picks a RAR reader, a RAR comic is not counted: a new one
            // keeps the one-second placeholder (owner decision on gl-495), and
            // a stored one the runtime it has, such as the count an adopted
            // Jellyfin database holds. Un-defer path: count the chosen
            // reader's non-directory entries here, with a RAR fixture test.
            (_, ArchiveFormat::Rar) => return None,
            (file, format) => archive_page_count(file, format, max_entries, path)?,
        },
        "pdf" => pdf_page_count(File::open(path).ok()?, MAX_ARCHIVE_ENTRY_BYTES)?,
        "epub" | "mobi" | "azw" | "azw3" => return Some(NOMINAL_BOOK_RUN_TIME_TICKS),
        _ => return None,
    };
    i64::try_from(pages).ok()?.checked_mul(TICKS_PER_PAGE)
}

/// Whether `path` is a book whose pages [`book_run_time_ticks`] counts: a
/// `.cbz`, `.cb7` or `.cbt` comic, or a PDF.
///
/// TODO(parity, open work item; gl-747): `.cbr` joins once a RAR reader is
/// chosen. Until then most `.cbr` files are RAR, which nothing here counts,
/// so a caller that reopens these books on every scan leaves `.cbr` out; one
/// holding a ZIP, 7z or tar is still counted on its probe's own triggers.
#[must_use]
pub fn has_countable_pages(path: &str) -> bool {
    matches!(extension_of(path).as_str(), "cb7" | "cbt" | "cbz" | "pdf")
}

/// A comic archive's non-directory entries (`archive.Entries.Count(e =>
/// !e.IsDirectory)`), or `None` when it cannot be read or holds more than
/// `max_entries` entries. ZIP's central directory and 7z's header are read
/// whole, which their readers bound by the directory's size; a tar's headers
/// are read one at a time, seeking past each entry's body, and no further
/// than the cap.
fn archive_page_count(
    file: File,
    format: ArchiveFormat,
    max_entries: u32,
    path: &str,
) -> Option<u64> {
    let max = u64::from(max_entries);
    let (entries, pages) = match format {
        ArchiveFormat::Zip => {
            let archive = zip::ZipArchive::new(BufReader::new(file)).ok()?;
            // `ZipEntry.IsDirectory`: a name ending in a slash. The zip crate
            // keys entries by name, so a directory naming one entry twice
            // counts it once, where SharpCompress counts both.
            let pages = archive
                .file_names_raw()
                .filter(|name| !matches!(name.last(), Some(b'/' | b'\\')))
                .count();
            (archive.len() as u64, pages as u64)
        }
        ArchiveFormat::SevenZip => {
            let mut file = file;
            let archive =
                sevenz_rust2::Archive::read(&mut file, &sevenz_rust2::Password::empty()).ok()?;
            let pages = archive.files.iter().filter(|e| !e.is_directory()).count();
            (archive.files.len() as u64, pages as u64)
        }
        ArchiveFormat::Tar => {
            let mut archive = tar::Archive::new(file);
            let (mut entries, mut pages) = (0_u64, 0_u64);
            for entry in archive.entries_with_seek().ok()? {
                entries += 1;
                if entries > max {
                    break;
                }
                if !entry.ok()?.header().entry_type().is_dir() {
                    pages += 1;
                }
            }
            (entries, pages)
        }
        ArchiveFormat::Rar => return None,
    };
    if entries > max {
        // `debug`: a book left at one second is opened again on every scan.
        tracing::debug!(
            path,
            max_entries,
            "comic archive holds more entries than book_archive_max_entries; its pages are not \
             counted"
        );
        return None;
    }
    Some(pages)
}

/// How far from the end of a PDF `startxref` is looked for: as far back as
/// PDFium, upstream's reader, searches for it (`CPDF_Parser::ParseStartXRef`).
const TAIL_BYTES: u64 = 4096;

/// The keyword whose integer is the newest cross-reference section's offset.
const STARTXREF: &[u8] = b"startxref";

/// The buffer each read of the file fills: `std`'s default `BufReader` size.
const READ_BUFFER_BYTES: usize = 8 * 1024;

/// The most lexers one count makes, each a seek and a buffered read: as many
/// [`READ_BUFFER_BYTES`] buffers as fit in [`MAX_ARCHIVE_ENTRY_BYTES`]. A real
/// PDF needs about ten; a file of endless cross-reference sections or
/// subsections stops here.
const MAX_READS: u64 = MAX_ARCHIVE_ENTRY_BYTES / READ_BUFFER_BYTES as u64;

/// The values one parse may build: as many as fit in
/// [`MAX_ARCHIVE_ENTRY_BYTES`] at their inline size, so a hostile array of
/// small numbers stays within a small multiple of it (a name's bytes and a
/// growing vector's spare room come on top, and a few parses are alive at
/// once).
const MAX_NODES: u64 = MAX_ARCHIVE_ENTRY_BYTES / std::mem::size_of::<Obj>() as u64;

/// The bytes of one classic cross-reference entry, which the PDF format fixes.
const XREF_ENTRY_BYTES: u64 = 20;

/// The deepest container this reader parses: a stream dictionary's
/// `/DecodeParms` array of dictionaries. Deeper ones are skipped without
/// recursion, so a hostile file cannot exhaust the stack.
const MAX_NESTING: usize = 3;

/// A PDF's page count: the `/Count` of its page-tree root (`/Root` →
/// `/Pages` → `/Count`), which PDFium's `FPDF_GetPageCount` returns for
/// upstream's `Conversion.GetPageCount`. `None` when the file is not a PDF
/// this reader can follow. `budget` caps the bytes all the PDF's streams may
/// decompress to, together.
///
/// TODO(parity, open work item; gl-1705): a PDF whose cross-reference offsets
/// are wrong (a table whose first subsection starts at 1 instead of 0
/// included), or whose page tree sits in an encrypted object stream, is not
/// counted, and such a book keeps its runtime; PDFium rebuilds the offsets by
/// scanning the file and decrypts with the empty user password. PDFium also
/// walks the page tree's leaves when `/Count` is implausible, where this takes
/// `/Count` as it is. Port path: the Standard security handler's
/// empty-password key for object streams, a bounded streaming scan for
/// `N G obj` headers when an offset misses, and the leaf walk.
fn pdf_page_count(file: File, budget: u64) -> Option<u64> {
    let len = file.metadata().ok()?.len();
    let mut pdf = Pdf {
        file,
        len,
        sections: Vec::new(),
        budget: Cell::new(budget),
        reads: Cell::new(MAX_READS),
    };
    let trailer = pdf.load_cross_references()?;
    let catalog = pdf.object(trailer.get(b"Root")?.reference()?)?;
    let tree = pdf.object(catalog.get(b"Pages")?.reference()?)?;
    let count = pdf.integer(tree.get(b"Count")?)?;
    u64::try_from(count).ok()
}

/// A PDF being read for its page count.
struct Pdf {
    file: File,
    len: u64,
    /// The cross-reference sections, newest first: each revision's table,
    /// then the stream a hybrid file's table names (`/XRefStm`).
    sections: Vec<Section>,
    /// The bytes the PDF's streams may still decompress to.
    budget: Cell<u64>,
    /// The reads of the file left ([`MAX_READS`]).
    reads: Cell<u64>,
}

/// A classic table's subsection: `(first object, count, offset of the first
/// entry)`.
type Subsection = (u64, u64, u64);

/// One cross-reference section.
enum Section {
    /// A classic table: its subsections as `(first object, count, offset of
    /// the first entry)`.
    Table(Vec<Subsection>),
    /// A cross-reference stream: its `/W` field widths, its `/Index`
    /// subsections as `(first object, count)`, and its decoded rows.
    Stream {
        widths: [usize; 3],
        index: Vec<(u64, u64)>,
        rows: Vec<u8>,
    },
}

/// Where an object is.
enum Location {
    /// At this file offset.
    Offset(u64),
    /// Inside the object stream with this object number.
    InStream(u64),
}

impl Pdf {
    /// A lexer over the file from `offset`, reading at most
    /// [`MAX_ARCHIVE_ENTRY_BYTES`], or `None` once the count's reads are spent
    /// ([`MAX_READS`]). Every lexer moves the one file cursor, so a lexer is
    /// done with before the next one is made.
    fn lexer_at(&self, offset: u64) -> Option<Lexer<BufReader<std::io::Take<&File>>>> {
        self.reads.set(self.reads.get().checked_sub(1)?);
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset)).ok()?;
        Some(Lexer::new(BufReader::with_capacity(
            READ_BUFFER_BYTES,
            file.take(MAX_ARCHIVE_ENTRY_BYTES),
        )))
    }

    /// Reads every cross-reference section, newest first, following `/Prev`,
    /// and returns the newest trailer.
    fn load_cross_references(&mut self) -> Option<Obj> {
        let mut offset = self.startxref()?;
        let mut seen = HashSet::new();
        let mut newest = None;
        // Each offset once: a `/Prev` chain that loops ends there.
        while seen.insert(offset) {
            let (trailer, sections) = self.section_at(offset)?;
            self.sections.extend(sections);
            let prev = trailer.get(b"Prev").and_then(Obj::integer);
            newest.get_or_insert(trailer);
            match prev {
                Some(prev) => offset = u64::try_from(prev).ok()?,
                None => break,
            }
        }
        newest
    }

    /// The offset `startxref` names, from the file's tail.
    fn startxref(&self) -> Option<u64> {
        let mut file = &self.file;
        file.seek(SeekFrom::Start(self.len.saturating_sub(TAIL_BYTES)))
            .ok()?;
        let mut tail = Vec::new();
        file.take(TAIL_BYTES).read_to_end(&mut tail).ok()?;
        let at = tail
            .windows(STARTXREF.len())
            .rposition(|w| w == STARTXREF)?;
        match Lexer::new(&tail[at + STARTXREF.len()..]).token()? {
            Token::Int(offset) => u64::try_from(offset).ok(),
            _ => None,
        }
    }

    /// The cross-reference section at `offset` and its trailer: a classic
    /// table (with the stream a hybrid file's trailer names), or a stream,
    /// whose dictionary is its trailer.
    fn section_at(&self, offset: u64) -> Option<(Obj, Vec<Section>)> {
        let mut lexer = self.lexer_at(offset)?;
        match lexer.token()? {
            Token::Keyword(word) if word == b"xref" => {
                let (trailer, subsections) = self.table_at(offset + lexer.pos)?;
                let mut sections = vec![Section::Table(subsections)];
                // A hybrid file's table leaves out the objects its stream
                // lists, so the stream is read after the table.
                if let Some(at) = trailer.get(b"XRefStm").and_then(Obj::integer) {
                    sections.push(self.stream_section(u64::try_from(at).ok()?)?.1);
                }
                Some((trailer, sections))
            }
            Token::Int(_) => {
                let (dict, section) = self.stream_section(offset)?;
                Some((dict, vec![section]))
            }
            _ => None,
        }
    }

    /// A classic table's subsections from `pos` (just past `xref`), and the
    /// trailer after them. Each subsection is a header line and fixed-size
    /// entries, which are skipped, not read.
    fn table_at(&self, mut pos: u64) -> Option<(Obj, Vec<Subsection>)> {
        let mut subsections = Vec::new();
        loop {
            let mut lexer = self.lexer_at(pos)?;
            match lexer.token()? {
                Token::Keyword(word) if word == b"trailer" => {
                    return Some((lexer.object(0)?, subsections));
                }
                Token::Int(first) => {
                    let Token::Int(count) = lexer.token()? else {
                        return None;
                    };
                    lexer.skip_space();
                    let start = pos + lexer.pos;
                    let count = u64::try_from(count).ok()?;
                    subsections.push((u64::try_from(first).ok()?, count, start));
                    pos = start.checked_add(count.checked_mul(XREF_ENTRY_BYTES)?)?;
                }
                _ => return None,
            }
        }
    }

    /// A cross-reference stream at `offset`: its dictionary and its section.
    fn stream_section(&self, offset: u64) -> Option<(Obj, Section)> {
        let (dict, rows) = self.stream_at(offset, None)?;
        let mut widths = [0; 3];
        let w = dict.get(b"W")?.array()?;
        for (width, value) in widths.iter_mut().zip(w) {
            // A field wider than 8 bytes holds no file offset.
            *width = usize::try_from(value.integer()?).ok().filter(|w| *w <= 8)?;
        }
        let index = match dict.get(b"Index") {
            Some(index) => index
                .array()?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[first, count]| {
                    Some((
                        u64::try_from(first.integer()?).ok()?,
                        u64::try_from(count.integer()?).ok()?,
                    ))
                })
                .collect::<Option<Vec<_>>>()?,
            None => vec![(0, u64::try_from(dict.get(b"Size")?.integer()?).ok()?)],
        };
        Some((
            dict,
            Section::Stream {
                widths,
                index,
                rows,
            },
        ))
    }

    /// Where object `id` is, from the newest section that lists it in use.
    ///
    /// A free or unknown entry means "not here", and the search goes on to the
    /// older sections. A hybrid file needs that: its table lists the objects
    /// only its stream holds as free. It also means an object a later revision
    /// freed still resolves to its older offset, which no `/Root` or `/Pages`
    /// reference reaches in practice.
    fn locate(&self, id: u64) -> Option<Location> {
        self.sections.iter().find_map(|section| match section {
            Section::Table(subsections) => {
                let (first, _, start) = subsections.iter().find(|(first, count, _)| {
                    (*first..first.saturating_add(*count)).contains(&id)
                })?;
                let mut lexer = self.lexer_at(start + (id - first) * XREF_ENTRY_BYTES)?;
                let (Token::Int(offset), Token::Int(_), Token::Keyword(kind)) =
                    (lexer.token()?, lexer.token()?, lexer.token()?)
                else {
                    return None;
                };
                (kind == b"n")
                    .then(|| u64::try_from(offset).ok().map(Location::Offset))
                    .flatten()
            }
            Section::Stream {
                widths,
                index,
                rows,
            } => {
                let row_len: usize = widths.iter().sum();
                let mut before = 0_u64;
                let row = index.iter().find_map(|(first, count)| {
                    let row = (*first..first.saturating_add(*count))
                        .contains(&id)
                        .then(|| before.checked_add(id - first))
                        .flatten();
                    before = before.saturating_add(*count);
                    row
                })?;
                let at = usize::try_from(row).ok()?.checked_mul(row_len)?;
                let mut fields = [0_u64; 3];
                let mut cursor = rows.get(at..at.checked_add(row_len)?)?;
                for (field, width) in fields.iter_mut().zip(widths) {
                    let (bytes, rest) = cursor.split_at(*width);
                    *field = bytes.iter().fold(0, |n, b| n << 8 | u64::from(*b));
                    cursor = rest;
                }
                // A missing type field means type 1.
                match if widths[0] == 0 { 1 } else { fields[0] } {
                    1 => Some(Location::Offset(fields[1])),
                    2 => Some(Location::InStream(fields[1])),
                    _ => None,
                }
            }
        })
    }

    /// Object `id`'s value.
    fn object(&self, id: u64) -> Option<Obj> {
        match self.locate(id)? {
            Location::Offset(offset) => self.lexer_at(offset)?.indirect(Some(id)),
            Location::InStream(stream) => {
                // An object stream is never inside another one.
                let Location::Offset(at) = self.locate(stream)? else {
                    return None;
                };
                let (dict, data) = self.stream_at(at, Some(stream))?;
                let first = usize::try_from(dict.get(b"First")?.integer()?).ok()?;
                // Its head: `/N` pairs of object number and offset.
                let mut head = Lexer::new(data.get(..first)?);
                while let (Some(Token::Int(number)), Some(Token::Int(offset))) =
                    (head.token(), head.token())
                {
                    if u64::try_from(number).ok() == Some(id) {
                        let at = first.checked_add(usize::try_from(offset).ok()?)?;
                        return Lexer::new(data.get(at..)?).object(0);
                    }
                }
                None
            }
        }
    }

    /// An integer, direct or the value of an object outside any object
    /// stream, as a stream's `/Length` is (ISO 32000-1 7.5.7: an object stream's
    /// `/Length` is never inside one). Resolving it through an object stream
    /// could reach the very stream being read.
    fn direct_integer(&self, value: &Obj) -> Option<i64> {
        match value {
            Obj::Ref(id) => match self.locate(*id)? {
                Location::Offset(offset) => self.lexer_at(offset)?.indirect(Some(*id))?.integer(),
                Location::InStream(_) => None,
            },
            other => other.integer(),
        }
    }

    /// An integer, direct or the value of the object it refers to.
    fn integer(&self, value: &Obj) -> Option<i64> {
        match value {
            Obj::Ref(id) => self.object(*id)?.integer(),
            other => other.integer(),
        }
    }

    /// The stream object at `offset` (object `id`, when known): its
    /// dictionary and its decoded data. Only unfiltered and `FlateDecode`
    /// streams are read, the only filters cross-reference and object streams
    /// use in practice.
    fn stream_at(&self, offset: u64, id: Option<u64>) -> Option<(Obj, Vec<u8>)> {
        let mut lexer = self.lexer_at(offset)?;
        let dict = lexer.indirect(id)?;
        match lexer.token()? {
            Token::Keyword(word) if word == b"stream" => {}
            _ => return None,
        }
        // `stream` ends with CRLF or LF.
        if lexer.peek() == Some(b'\r') {
            lexer.bump();
        }
        if lexer.peek() == Some(b'\n') {
            lexer.bump();
        }
        let data_at = offset + lexer.pos;
        let flate = match dict.get(b"Filter") {
            None => false,
            Some(Obj::Name(name)) if name == b"FlateDecode" => true,
            Some(Obj::Array(filters)) if matches!(filters.as_slice(), [Obj::Name(name)] if name == b"FlateDecode") => {
                true
            }
            Some(_) => return None,
        };
        let budget = self.budget.get();
        // One byte past the budget tells "at it" from "over it".
        let limit = budget.saturating_add(1);
        let mut data = Vec::new();
        if flate {
            // A zlib stream ends itself: `/Length` is not needed.
            flate2::bufread::ZlibDecoder::new(lexer.input)
                .take(limit)
                .read_to_end(&mut data)
                .ok()?;
        } else {
            drop(lexer);
            // Resolving an indirect `/Length` moves the file cursor first.
            let length = u64::try_from(self.direct_integer(dict.get(b"Length")?)?).ok()?;
            self.lexer_at(data_at)?
                .input
                .take(length.min(limit))
                .read_to_end(&mut data)
                .ok()?;
        }
        let used = data.len() as u64;
        if used > budget {
            return None;
        }
        self.budget.set(budget - used);
        let parms = match dict.get(b"DecodeParms") {
            Some(Obj::Array(parms)) => parms.first(),
            parms => parms,
        };
        let data = unpredict(data, parms)?;
        Some((dict, data))
    }
}

/// Undoes a stream's PNG predictor (`/Predictor` 10 to 15), which
/// cross-reference streams use: each row starts with its filter type. `None`
/// for the TIFF predictor, which they do not.
fn unpredict(data: Vec<u8>, parms: Option<&Obj>) -> Option<Vec<u8>> {
    let parm = |key: &[u8], default: i64| {
        parms
            .and_then(|parms| parms.get(key))
            .map_or(Some(default), Obj::integer)
    };
    match parm(b"Predictor", 1)? {
        1 => return Some(data),
        10..=15 => {}
        _ => return None,
    }
    let bits = parm(b"Colors", 1)?.checked_mul(parm(b"BitsPerComponent", 8)?)?;
    let pixel = usize::try_from((bits / 8).max(1)).ok()?;
    let width =
        usize::try_from(bits.checked_mul(parm(b"Columns", 1)?)?.checked_add(7)? / 8).ok()?;
    // Checked before anything is allocated: `/Columns` is the file's word,
    // and no row can be wider than the data.
    if width == 0 || width > data.len() {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    let mut previous = vec![0_u8; width];
    for row in data.chunks(width + 1) {
        let (&kind, bytes) = row.split_first()?;
        let mut current = vec![0_u8; width];
        for (i, &byte) in bytes.iter().enumerate() {
            let left = if i >= pixel { current[i - pixel] } else { 0 };
            let up = previous[i];
            let up_left = if i >= pixel { previous[i - pixel] } else { 0 };
            current[i] = byte.wrapping_add(match kind {
                0 => 0,
                1 => left,
                2 => up,
                // `floor((left + up) / 2)`.
                3 => left.midpoint(up),
                4 => paeth(left, up, up_left),
                _ => return None,
            });
        }
        out.extend_from_slice(&current[..bytes.len()]);
        previous = current;
    }
    Some(out)
}

/// The PNG Paeth predictor.
fn paeth(left: u8, up: u8, up_left: u8) -> u8 {
    let estimate = i16::from(left) + i16::from(up) - i16::from(up_left);
    let (a, b, c) = (
        (estimate - i16::from(left)).abs(),
        (estimate - i16::from(up)).abs(),
        (estimate - i16::from(up_left)).abs(),
    );
    if a <= b && a <= c {
        left
    } else if b <= c {
        up
    } else {
        up_left
    }
}

/// The values this reader keeps.
#[derive(Debug, Clone, PartialEq)]
enum Obj {
    Int(i64),
    /// An indirect reference, `id gen R`.
    Ref(u64),
    Name(Vec<u8>),
    Array(Vec<Obj>),
    Dict(Vec<(Vec<u8>, Obj)>),
    /// A string, a real, a boolean, null, or a container nested too deep.
    Other,
}

impl Obj {
    /// A dictionary's value for `key`.
    fn get(&self, key: &[u8]) -> Option<&Obj> {
        match self {
            Obj::Dict(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn integer(&self) -> Option<i64> {
        match self {
            Obj::Int(n) => Some(*n),
            _ => None,
        }
    }

    fn reference(&self) -> Option<u64> {
        match self {
            Obj::Ref(id) => Some(*id),
            _ => None,
        }
    }

    fn array(&self) -> Option<&[Obj]> {
        match self {
            Obj::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// A PDF token.
#[derive(Debug, PartialEq)]
enum Token {
    Int(i64),
    Name(Vec<u8>),
    /// A bare word: `obj`, `R`, `stream`, `xref`, `trailer`, `n`, `true`…
    Keyword(Vec<u8>),
    ArrayOpen,
    ArrayClose,
    DictOpen,
    DictClose,
    /// A string, a real, or a stray delimiter.
    Other,
}

/// PDF whitespace.
fn is_space(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | b'\x0C' | b'\r' | b' ')
}

/// PDF delimiters.
fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// Reads PDF tokens and objects from `input`, counting the bytes it consumed.
struct Lexer<R> {
    input: R,
    pos: u64,
    /// The values this lexer may still build ([`MAX_NODES`]).
    nodes: u64,
}

impl<R: BufRead> Lexer<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            pos: 0,
            nodes: MAX_NODES,
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.input.fill_buf().ok()?.first().copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.input.consume(1);
        self.pos += 1;
        Some(byte)
    }

    /// Skips whitespace and comments.
    fn skip_space(&mut self) {
        while let Some(byte) = self.peek() {
            if is_space(byte) {
                self.bump();
            } else if byte == b'%' {
                while !matches!(self.bump(), None | Some(b'\n' | b'\r')) {}
            } else {
                return;
            }
        }
    }

    fn token(&mut self) -> Option<Token> {
        self.skip_space();
        let byte = self.bump()?;
        Some(match byte {
            b'[' => Token::ArrayOpen,
            b']' => Token::ArrayClose,
            b'<' if self.peek() == Some(b'<') => {
                self.bump();
                Token::DictOpen
            }
            b'>' if self.peek() == Some(b'>') => {
                self.bump();
                Token::DictClose
            }
            // A hex string.
            b'<' => {
                while self.bump()? != b'>' {}
                Token::Other
            }
            b'(' => {
                let mut depth = 1_u64;
                while depth > 0 {
                    match self.bump()? {
                        b'\\' => {
                            self.bump()?;
                        }
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                }
                Token::Other
            }
            b'/' => Token::Name(self.name()),
            _ if is_delimiter(byte) => Token::Other,
            _ => {
                let mut word = vec![byte];
                while let Some(next) = self.peek().filter(|b| !is_space(*b) && !is_delimiter(*b)) {
                    word.push(next);
                    self.bump();
                }
                let digits = word.strip_prefix(b"+").or_else(|| word.strip_prefix(b"-"));
                if digits.unwrap_or(&word).iter().all(u8::is_ascii_digit)
                    && digits.is_none_or(|d| !d.is_empty())
                {
                    std::str::from_utf8(&word)
                        .ok()
                        .and_then(|w| w.parse().ok())
                        .map_or(Token::Other, Token::Int)
                } else if word.iter().any(|b| matches!(b, b'.' | b'+' | b'-')) {
                    Token::Other
                } else {
                    Token::Keyword(word)
                }
            }
        })
    }

    /// A name's bytes after its `/`, with `#xx` escapes decoded.
    fn name(&mut self) -> Vec<u8> {
        let mut name = Vec::new();
        while let Some(byte) = self.peek().filter(|b| !is_space(*b) && !is_delimiter(*b)) {
            self.bump();
            let escaped = if byte == b'#' { self.hex_byte() } else { None };
            name.push(escaped.unwrap_or(byte));
        }
        name
    }

    /// The two hex digits of a name's `#xx` escape, as a byte.
    fn hex_byte(&mut self) -> Option<u8> {
        let high = char::from(self.peek()?).to_digit(16)?;
        self.bump();
        let low = char::from(self.peek()?).to_digit(16)?;
        self.bump();
        u8::try_from(high * 16 + low).ok()
    }

    /// The next object; containers at `depth` and deeper than
    /// [`MAX_NESTING`] are skipped.
    fn object(&mut self, depth: usize) -> Option<Obj> {
        let token = self.token()?;
        self.value(token, depth)
    }

    fn value(&mut self, token: Token, depth: usize) -> Option<Obj> {
        self.nodes = self.nodes.checked_sub(1)?;
        match token {
            Token::Int(n) => Some(Obj::Int(n)),
            Token::Name(name) => Some(Obj::Name(name)),
            Token::ArrayOpen | Token::DictOpen if depth >= MAX_NESTING => {
                let mut open = 1_u64;
                while open > 0 {
                    match self.token()? {
                        Token::ArrayOpen | Token::DictOpen => open += 1,
                        Token::ArrayClose | Token::DictClose => open -= 1,
                        _ => {}
                    }
                }
                Some(Obj::Other)
            }
            Token::ArrayOpen => Some(Obj::Array(self.items(&Token::ArrayClose, depth + 1)?)),
            Token::DictOpen => {
                let mut items = self.items(&Token::DictClose, depth + 1)?.into_iter();
                let mut entries = Vec::new();
                while let Some(key) = items.next() {
                    let Obj::Name(key) = key else {
                        return None;
                    };
                    entries.push((key, items.next()?));
                }
                Some(Obj::Dict(entries))
            }
            Token::Keyword(_) | Token::Other => Some(Obj::Other),
            Token::ArrayClose | Token::DictClose => None,
        }
    }

    /// A container's items up to `close`, with each `id gen R` folded into a
    /// reference.
    fn items(&mut self, close: &Token, depth: usize) -> Option<Vec<Obj>> {
        let mut items = Vec::new();
        loop {
            let token = self.token()?;
            if token == *close {
                return Some(items);
            }
            if matches!(&token, Token::Keyword(word) if word == b"R") {
                let (Some(Obj::Int(_)), Some(Obj::Int(id))) = (items.pop(), items.pop()) else {
                    return None;
                };
                items.push(Obj::Ref(u64::try_from(id).ok()?));
                continue;
            }
            items.push(self.value(token, depth)?);
        }
    }

    /// An indirect object, `id gen obj value`, checked against `id` when one
    /// is expected.
    fn indirect(&mut self, id: Option<u64>) -> Option<Obj> {
        let (Token::Int(number), Token::Int(_), Token::Keyword(word)) =
            (self.token()?, self.token()?, self.token()?)
        else {
            return None;
        };
        if word != b"obj" || id.is_some_and(|id| u64::try_from(number).ok() != Some(id)) {
            return None;
        }
        self.object(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::io::Write as _;

    /// Writes `objects` (each `(id, body)`) as a classic PDF with one
    /// cross-reference table, and returns its bytes.
    fn classic_pdf(objects: &[(u64, &str)], root: u64) -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (id, body) in objects {
            offsets.push((*id, pdf.len()));
            pdf.extend_from_slice(format!("{id} 0 obj\n{body}\nendobj\n").as_bytes());
        }
        let size = objects.iter().map(|(id, _)| id).max().unwrap() + 1;
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f\r\n").as_bytes());
        for id in 1..size {
            let entry = offsets.iter().find(|(o, _)| *o == id).map_or_else(
                || "0000000000 65535 f\r\n".to_owned(),
                |(_, at)| format!("{at:010} 00000 n\r\n"),
            );
            pdf.extend_from_slice(entry.as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {size} /Root {root} 0 R /ID [<00ff> <00ff>] >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        pdf
    }

    /// A page tree of `n` pages (objects 3 onwards) under object 2.
    fn page_tree(n: u64) -> Vec<(u64, String)> {
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", i + 3)).collect();
        let mut objects = vec![
            (1, "<< /Type /Catalog /Pages 2 0 R >>".to_owned()),
            (
                2,
                format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
            ),
        ];
        for i in 0..n {
            objects.push((
                i + 3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_owned(),
            ));
        }
        objects
    }

    fn count(bytes: &[u8]) -> Option<u64> {
        count_with_budget(bytes, MAX_ARCHIVE_ENTRY_BYTES)
    }

    fn count_with_budget(bytes: &[u8], budget: u64) -> Option<u64> {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        pdf_page_count(file, budget)
    }

    fn refs(objects: &[(u64, String)]) -> Vec<(u64, &str)> {
        objects
            .iter()
            .map(|(id, body)| (*id, body.as_str()))
            .collect()
    }

    fn deflate(bytes: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[rstest::rstest]
    #[case(1)]
    #[case(7)]
    #[case(250)]
    fn a_classic_pdf_counts_its_page_tree(#[case] pages: u64) {
        let objects = page_tree(pages);
        assert_eq!(count(&classic_pdf(&refs(&objects), 1)), Some(pages));
    }

    /// `/Count` may be an indirect integer, and a name may be escaped.
    #[test]
    fn an_indirect_count_and_an_escaped_name_are_followed() {
        let objects = [
            (1, "<< /Type /Catalog /P#61ges 2 0 R % a comment\n>>"),
            (
                2,
                "<< /Type /Pages /Kids [3 0 R] /Title (a (nested) \\) string) /Count 4 0 R >>",
            ),
            (3, "<< /Type /Page /Parent 2 0 R >>"),
            (4, "12"),
        ];
        assert_eq!(count(&classic_pdf(&objects, 1)), Some(12));
    }

    /// An incremental update appends a revision whose table lists only what
    /// changed, and whose trailer points back with `/Prev`: the newest
    /// `/Pages` wins, the rest is found in the older table.
    #[test]
    fn an_incremental_update_wins_over_the_older_revision() {
        let objects = page_tree(2);
        let mut pdf = classic_pdf(&refs(&objects), 1);
        let prev = {
            let tail = String::from_utf8_lossy(&pdf).into_owned();
            let at = tail.rfind("startxref\n").unwrap() + 10;
            tail[at..].lines().next().unwrap().parse::<usize>().unwrap()
        };
        let tree_at = pdf.len();
        pdf.extend_from_slice(
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>\nendobj\n",
        );
        let added_page_at = pdf.len();
        pdf.extend_from_slice(b"5 0 obj\n<< /Type /Page /Parent 2 0 R >>\nendobj\n");
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!(
                "xref\n2 1\n{tree_at:010} 00000 n\r\n5 1\n{added_page_at:010} 00000 n\r\ntrailer\n<< /Size 6 /Root 1 0 R /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        assert_eq!(count(&pdf), Some(3));
    }

    /// How object stream 4 of [`objstm_pdf`] stores its data.
    enum Packing {
        /// `FlateDecode`, with a direct `/Length`.
        Flate,
        /// Unfiltered, its `/Length` the object 6 written after it.
        RawLength,
        /// Unfiltered, its `/Length` the object 6 packed inside it.
        RawLengthInside,
    }

    /// A PDF 1.5 file: page 3 written directly; the catalog (1) and the
    /// page-tree root (2, `/Count 41`) packed in object stream 4; and the
    /// cross-reference stream 5, compressed behind a PNG `Up` predictor,
    /// locating all of them.
    fn objstm_pdf(packing: &Packing) -> Vec<u8> {
        let mut pdf = b"%PDF-1.5\n".to_vec();
        let page_at = pdf.len();
        pdf.extend_from_slice(b"3 0 obj\n<< /Type /Page /Parent 2 0 R >>\nendobj\n");
        let mut packed = vec![
            (1, "<< /Type /Catalog /Pages 2 0 R >>"),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 41 >>"),
        ];
        if matches!(packing, Packing::RawLengthInside) {
            packed.push((6, "999"));
        }
        let (mut head, mut body) = (String::new(), String::new());
        for (id, object) in &packed {
            write!(head, "{id} {} ", body.len()).unwrap();
            body += object;
            body.push('\n');
        }
        let data = format!("{head}{body}").into_bytes();
        let (filter, bytes, length) = match packing {
            Packing::Flate => {
                let bytes = deflate(&data);
                let length = bytes.len().to_string();
                ("/Filter /FlateDecode ", bytes, length)
            }
            _ => ("", data, "6 0 R".to_owned()),
        };
        let objstm_at = pdf.len();
        pdf.extend_from_slice(
            format!(
                "4 0 obj\n<< /Type /ObjStm /N {} /First {} {filter}/Length {length} >>\nstream\r\n",
                packed.len(),
                head.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&bytes);
        pdf.extend_from_slice(b"\r\nendstream\nendobj\n");
        let length_at = pdf.len();
        if matches!(packing, Packing::RawLength) {
            pdf.extend_from_slice(format!("6 0 obj\n{}\nendobj\n", bytes.len()).as_bytes());
        }
        let xref_at = pdf.len();
        // Rows of `/W [1 2 1]` for objects 0..=6, each behind a PNG `Up` byte.
        let at = |offset: usize| {
            let [high, low] = u16::try_from(offset).unwrap().to_be_bytes();
            [1, high, low, 0]
        };
        let rows: [[u8; 4]; 7] = [
            [0, 0, 0, 0],
            [2, 0, 4, 0],
            [2, 0, 4, 1],
            at(page_at),
            at(objstm_at),
            at(xref_at),
            match packing {
                Packing::Flate => [0, 0, 0, 0],
                Packing::RawLength => at(length_at),
                Packing::RawLengthInside => [2, 0, 4, 2],
            },
        ];
        let mut encoded = Vec::new();
        let mut previous = [0_u8; 4];
        for row in rows {
            encoded.push(2);
            encoded.extend(row.iter().zip(previous).map(|(b, p)| b.wrapping_sub(p)));
            previous = row;
        }
        let stream = deflate(&encoded);
        pdf.extend_from_slice(
            format!(
                "5 0 obj\n<< /Type /XRef /Size 7 /W [1 2 1] /Root 1 0 R /Filter [/FlateDecode] /DecodeParms << /Predictor 12 /Columns 4 >> /Length {} >>\nstream\n",
                stream.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&stream);
        pdf.extend_from_slice(
            format!("\nendstream\nendobj\nstartxref\n{xref_at}\n%%EOF").as_bytes(),
        );
        pdf
    }

    /// The PDF 1.5 shape: the catalog and the page-tree root sit in an object
    /// stream, which a compressed cross-reference stream with a PNG
    /// predictor locates.
    #[test]
    fn an_object_stream_behind_a_cross_reference_stream_is_read() {
        let pdf = objstm_pdf(&Packing::Flate);
        assert_eq!(count(&pdf), Some(41));
        // The same file, its streams allowed fewer bytes than they inflate to.
        assert_eq!(count_with_budget(&pdf, 8), None);
        // Unfiltered, with its `/Length` an object of its own.
        assert_eq!(count(&objstm_pdf(&Packing::RawLength)), Some(41));
        // A `/Length` packed in the very stream it measures cannot be read
        // without reading that stream first: not counted, and no endless
        // recursion.
        assert_eq!(count(&objstm_pdf(&Packing::RawLengthInside)), None);
    }

    /// A hybrid file: its table lists the packed objects as free, and the
    /// cross-reference stream its trailer names (`/XRefStm`) locates them, in
    /// two `/Index` subsections and with no predictor.
    #[test]
    fn a_hybrid_file_reads_its_stream_after_its_table() {
        let mut pdf = b"%PDF-1.5\n".to_vec();
        let page_at = pdf.len();
        pdf.extend_from_slice(b"3 0 obj\n<< /Type /Page /Parent 2 0 R >>\nendobj\n");
        let (one, two) = (
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 5 >>",
        );
        let head = format!("1 0 2 {} ", one.len() + 1);
        let body = deflate(format!("{head}{one}\n{two}").as_bytes());
        let objstm_at = pdf.len();
        pdf.extend_from_slice(
            format!(
                "4 0 obj\n<< /Type /ObjStm /N 2 /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
                head.len(),
                body.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&body);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        let [high, low] = u16::try_from(objstm_at).unwrap().to_be_bytes();
        let rows = deflate(&[2, 0, 4, 0, 2, 0, 4, 1, 1, high, low, 0]);
        let stream_at = pdf.len();
        pdf.extend_from_slice(
            format!(
                "5 0 obj\n<< /Type /XRef /Size 6 /W [1 2 1] /Index [1 2 4 1] /Filter /FlateDecode /Length {} >>\nstream\n",
                rows.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&rows);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        let table_at = pdf.len();
        let entry = |at: Option<usize>| {
            at.map_or_else(
                || "0000000000 65535 f\r\n".to_owned(),
                |at| format!("{at:010} 00000 n\r\n"),
            )
        };
        let mut table = "xref\n0 6\n".to_owned();
        for at in [None, None, None, Some(page_at), None, Some(stream_at)] {
            table += &entry(at);
        }
        pdf.extend_from_slice(table.as_bytes());
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size 6 /Root 1 0 R /XRefStm {stream_at} >>\nstartxref\n{table_at}\n%%EOF\n"
            )
            .as_bytes(),
        );
        assert_eq!(count(&pdf), Some(5));
    }

    /// Each PNG filter type decodes; the TIFF predictor is refused; and a
    /// `/Columns` wider than the data is refused before anything is allocated
    /// for it.
    #[test]
    fn png_predictors_decode_and_implausible_ones_are_refused() {
        let rows: [[u8; 6]; 3] = [
            [10, 200, 30, 255, 0, 7],
            [20, 100, 250, 1, 2, 3],
            [5, 5, 5, 200, 201, 202],
        ];
        let parms = |predictor: i64, columns: i64| {
            Obj::Dict(vec![
                (b"Predictor".to_vec(), Obj::Int(predictor)),
                (b"Columns".to_vec(), Obj::Int(columns)),
            ])
        };
        let plain = rows.concat();
        for kind in 0..=4_u8 {
            let mut encoded = Vec::new();
            let mut previous = [0_u8; 6];
            for row in rows {
                encoded.push(kind);
                for (i, &byte) in row.iter().enumerate() {
                    let left = if i > 0 { row[i - 1] } else { 0 };
                    let up_left = if i > 0 { previous[i - 1] } else { 0 };
                    let up = previous[i];
                    let predicted = match kind {
                        0 => 0,
                        1 => left,
                        2 => up,
                        3 => left.midpoint(up),
                        _ => paeth(left, up, up_left),
                    };
                    encoded.push(byte.wrapping_sub(predicted));
                }
                previous = row;
            }
            assert_eq!(
                unpredict(encoded, Some(&parms(12, 6))),
                Some(plain.clone()),
                "filter {kind}"
            );
        }
        assert_eq!(unpredict(plain.clone(), None), Some(plain.clone()));
        assert_eq!(unpredict(plain.clone(), Some(&parms(2, 6))), None, "TIFF");
        assert_eq!(
            unpredict(plain, Some(&parms(12, 9_000_000_000_000))),
            None,
            "rows wider than the data"
        );
    }

    /// The formats a scan opens again while they hold the one-second
    /// placeholder: not `.cbr`, mostly RAR, until a RAR reader is chosen.
    #[rstest::rstest]
    #[case("/b/a.cbz", true)]
    #[case("/b/a.CB7", true)]
    #[case("/b/a.cbt", true)]
    #[case("/b/a.pdf", true)]
    #[case("/b/a.cbr", false)]
    #[case("/b/a.epub", false)]
    #[case("/b/a.mobi", false)]
    #[case("/b/a.txt", false)]
    fn countable_pages_leave_out_rar_comics(#[case] path: &str, #[case] countable: bool) {
        assert_eq!(has_countable_pages(path), countable);
    }

    /// A parse builds at most its node budget of values.
    #[test]
    fn a_parse_stops_at_its_node_budget() {
        let parse = |nodes| {
            Lexer {
                nodes,
                ..Lexer::new(b"[1 2 [3] /Four]".as_slice())
            }
            .object(0)
        };
        // The array, its four items and the inner array's one: six values.
        assert!(parse(6).is_some());
        assert_eq!(parse(5), None);
    }

    /// A chain of cross-reference sections is followed to its end, unless it
    /// is longer than the count's reads allow.
    #[test]
    fn a_chain_of_sections_stops_at_the_read_cap() {
        let chain = |links: u64| {
            let objects = page_tree(1);
            let mut pdf = classic_pdf(&refs(&objects), 1);
            let text = String::from_utf8_lossy(&pdf).into_owned();
            let at = text.rfind("startxref\n").unwrap() + 10;
            let mut prev = text[at..].lines().next().unwrap().to_owned();
            for _ in 0..links {
                let here = pdf.len();
                pdf.extend_from_slice(
                    format!("xref\n0 0\ntrailer\n<< /Size 4 /Root 1 0 R /Prev {prev} >>\n")
                        .as_bytes(),
                );
                prev = here.to_string();
            }
            pdf.extend_from_slice(format!("startxref\n{prev}\n%%EOF\n").as_bytes());
            pdf
        };
        // A link costs three reads (its section, its subsection header, its
        // trailer), and the lookups after them four.
        let links = MAX_READS / 3;
        assert_eq!(count(&chain(links - 10)), Some(1));
        assert_eq!(count(&chain(links + 1)), None);
    }

    /// Hostile or broken input ends in `None`, never a panic or a stack
    /// overflow: nesting far past what is parsed is skipped.
    #[test]
    fn broken_and_hostile_pdfs_are_not_counted() {
        assert_eq!(count(b"not a pdf at all"), None);
        assert_eq!(count(b""), None);
        let objects = page_tree(3);
        let whole = classic_pdf(&refs(&objects), 1);
        assert_eq!(count(&whole[..whole.len() / 2]), None);
        let deep = format!(
            "<< /Type /Catalog /X {}{} /Pages 2 0 R >>",
            "[".repeat(100_000),
            "]".repeat(100_000)
        );
        let mut objects = page_tree(2);
        objects[0].1 = deep;
        assert_eq!(count(&classic_pdf(&refs(&objects), 1)), Some(2));
        // A `startxref` pointing nowhere, and a `/Prev` that loops.
        let mut dangling = whole;
        let at = String::from_utf8_lossy(&dangling)
            .rfind("startxref")
            .unwrap();
        dangling.truncate(at);
        dangling.extend_from_slice(b"startxref\n999999\n%%EOF\n");
        assert_eq!(count(&dangling), None);
        let objects = page_tree(1);
        let mut cyclic = classic_pdf(&refs(&objects), 1);
        let text = String::from_utf8_lossy(&cyclic).into_owned();
        let xref = text.rfind("xref\n0").unwrap();
        let replaced = text.replacen(" >>\nstartxref", &format!(" /Prev {xref} >>\nstartxref"), 1);
        cyclic = replaced.into_bytes();
        assert_eq!(count(&cyclic), Some(1));
    }
}
