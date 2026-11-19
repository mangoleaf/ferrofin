//! A book's runtime from its pages: the `Book` arm of upstream's
//! `ProbeProvider.FetchAsync` (`MediaBrowser.Providers/MediaInfo/
//! ProbeProvider.cs:246-294`, Jellyfin PR 17311).
//!
//! A comic archive's pages are its non-directory entries, read from the
//! archive's directory alone: the ZIP central directory, the 7z header, the
//! tar headers. A PDF's are its page tree's count, reached with seeks through
//! the trailer and the cross-reference sections, as PDFium reads it for
//! upstream: decrypted with the empty user password, and from a table rebuilt
//! by scanning the file when the sections cannot be followed. Neither reads a
//! page.

mod crypt;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

use super::{ArchiveFormat, MAX_ARCHIVE_ENTRY_BYTES, extension_of, open_comic};
use crypt::{Cipher, Crypt, Standard};

/// `TimeSpan.TicksPerSecond`: the runtime of a book whose pages are not
/// counted. Upstream gives it to an EPUB, whose readers report progress as a
/// fraction of one second; Ferrofin gives it to a few more formats
/// ([`book_run_time_ticks`]).
pub const NOMINAL_BOOK_RUN_TIME_TICKS: i64 = ferrofin_util::ticks::TICKS_PER_SECOND;

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
/// PDF needs about ten, and one or two more for each page-tree node a walk
/// reads and each object stream a rebuild reads; a file of endless
/// cross-reference sections or subsections stops here, and is not counted.
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
/// recursion, so a hostile file cannot exhaust the stack. TODO(parity, open
/// work item; gl-1811): a trailer's direct `/Encrypt` dictionary puts its
/// crypt filter one level deeper, so PDFium opens a file this refuses;
/// un-defer path: parse the trailer's `/Encrypt` value with its own lexer.
const MAX_NESTING: usize = 3;

/// The bytes of a PDF header (`%PDF-1.x` and its line end): a `startxref`
/// offset inside it is no offset, and a shorter file is no PDF (PDFium's
/// `kPDFHeaderSize`).
const PDF_HEADER_BYTES: u64 = 9;

/// The furthest into the file `%PDF` may start; every offset the file gives
/// counts from there (PDFium's `GetHeaderOffset`).
const MAX_HEADER_OFFSET: u64 = 1024;

/// The objects a rebuilt table may hold: as many as fit in
/// [`MAX_ARCHIVE_ENTRY_BYTES`] at twice their size, the room a hash table
/// takes while it grows. A file with more is not counted.
const MAX_REBUILT_OBJECTS: u64 =
    MAX_ARCHIVE_ENTRY_BYTES / (2 * std::mem::size_of::<(u64, Rebuilt)>() as u64);

/// The trailer entries this reader uses, the only ones a merge of trailers
/// keeps.
const TRAILER_KEYS: [&[u8]; 3] = [b"Root", b"Encrypt", b"ID"];

/// The highest object number a rebuilt table records (PDFium's
/// `CPDF_Parser::kMaxObjectNumber`).
const MAX_OBJECT_NUMBER: u64 = 24 * 1024 * 1024;

/// A page-tree node's `/Count` is trusted below this; at it or past it, or at
/// zero or less, the node's kids are counted, and a count reaching it is an
/// error (PDFium's `CPDF_Document::kPageMaxNum`).
const PAGE_MAX_NUM: i64 = 0xF_FFFF;

/// A PDF's page count as PDFium's `FPDF_GetPageCount` returns it for
/// upstream's `Conversion.GetPageCount` (PDFtoImage, which passes no
/// password): the count of the page tree under `/Root` → `/Pages`. `None`
/// where PDFium fails to open the file, so the book keeps its runtime. `budget`
/// caps the bytes all the PDF's streams may decompress to, together.
///
/// It follows `CPDF_Parser::StartParseInternal`: the cross-reference sections
/// from `startxref`, or a table rebuilt by scanning the file when they cannot
/// be read; then the Standard security handler with the empty user password;
/// then, when there is no catalog or no page, the table rebuilt once more
/// (`CPDF_Document::TryInit`).
///
/// A count that ran out of the reads or the bytes this reader allows itself is
/// not trusted: `None`.
fn pdf_page_count(mut file: File, budget: u64) -> Option<u64> {
    // `InitSyntaxParser`: `%PDF` within the first bytes, enough of a file
    // after it to hold a header.
    let mut head = Vec::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    (&mut file)
        .take(MAX_HEADER_OFFSET + 4)
        .read_to_end(&mut head)
        .ok()?;
    let header = head.windows(4).position(|w| w == b"%PDF")? as u64;
    let len = file.metadata().ok()?.len().checked_sub(header)?;
    if len < PDF_HEADER_BYTES {
        return None;
    }
    let mut pdf = Pdf {
        file,
        header,
        len,
        sections: Vec::new(),
        trailer: None,
        crypt: None,
        budget: Cell::new(budget),
        reads: Cell::new(MAX_READS),
        exhausted: Cell::new(false),
    };
    let rebuilt = pdf.load_cross_references().is_none();
    if rebuilt {
        pdf.rebuild()?;
    }
    pdf.set_encrypt_handler()?;
    let pages = match pdf.page_count() {
        Some(pages) if pages > 0 => Some(pages),
        _ if rebuilt => None,
        _ => {
            pdf.crypt = None;
            pdf.rebuild()?;
            pdf.set_encrypt_handler()?;
            pdf.page_count()
        }
    };
    pages.filter(|_| !pdf.exhausted.get())
}

/// A PDF being read for its page count.
struct Pdf {
    file: File,
    /// Where `%PDF` starts: the file's offsets count from here.
    header: u64,
    /// The bytes from the header to the end of the file.
    len: u64,
    /// The cross-reference sections, newest first: a rebuilt table, then each
    /// revision's table, then the stream a hybrid file's table names
    /// (`/XRefStm`).
    sections: Vec<Section>,
    /// The newest trailer, with a rebuilt table's trailers over it.
    trailer: Option<Obj>,
    /// The document's decryption, once the security handler is open.
    crypt: Option<Crypt>,
    /// The bytes the PDF's streams may still decompress to.
    budget: Cell<u64>,
    /// The reads of the file left ([`MAX_READS`]).
    reads: Cell<u64>,
    /// Whether a read or a stream was refused for want of reads or budget.
    exhausted: Cell<bool>,
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
    /// The objects a scan of the file found ([`Pdf::rebuild`]).
    Rebuilt(HashMap<u64, Rebuilt>),
}

/// An object a scan of the file found (PDFium's
/// `CPDF_CrossRefTable::ObjectInfo`).
#[derive(Default)]
struct Rebuilt {
    generation: u64,
    location: Option<Location>,
    /// It holds other objects, and so is never itself held in one.
    object_stream: bool,
}

/// Where an object is.
#[derive(Clone, Copy)]
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
        let Some(reads) = self.reads.get().checked_sub(1) else {
            self.exhausted.set(true);
            return None;
        };
        self.reads.set(reads);
        let mut file = &self.file;
        file.seek(SeekFrom::Start(self.header.checked_add(offset)?))
            .ok()?;
        Some(Lexer::new(BufReader::with_capacity(
            READ_BUFFER_BYTES,
            file.take(MAX_ARCHIVE_ENTRY_BYTES),
        )))
    }

    /// Reads every cross-reference section, newest first, following `/Prev`,
    /// with each older trailer's entries under the newer ones
    /// (`LoadAllCrossRefTablesAndStreams`). `None`, and the table is rebuilt,
    /// when a section cannot be read, the `/Prev` chain loops, or the oldest
    /// section is a classic table that fails its check
    /// ([`Pdf::verify_table`]).
    fn load_cross_references(&mut self) -> Option<()> {
        let mut offset = self.startxref()?;
        let mut seen = HashSet::new();
        while seen.insert(offset) {
            let (trailer, sections) = self.section_at(offset)?;
            let oldest = match sections.first() {
                Some(Section::Table(subsections)) => Some(subsections.clone()),
                _ => None,
            };
            self.sections.extend(sections);
            let prev = trailer.get(b"Prev").and_then(Obj::integer);
            self.trailer = Some(match self.trailer.take() {
                Some(newer) => merge_trailer(Some(&trailer), &newer),
                None => trailer,
            });
            match prev {
                Some(prev) if prev > 0 => offset = u64::try_from(prev).ok()?,
                _ => return oldest.map_or(Some(()), |table| self.verify_table(&table)),
            }
        }
        None
    }

    /// PDFium's check of the oldest section when it is a classic table
    /// (`VerifyCrossRefTable`): the lowest-numbered object it locates must
    /// start with its own number. A table whose numbers are off by one, as
    /// when its first subsection starts at 1 instead of 0, fails it.
    fn verify_table(&self, subsections: &[Subsection]) -> Option<()> {
        let mut sorted = subsections.to_vec();
        sorted.sort_unstable();
        let mut lowest = None;
        'subsections: for (first, count, start) in sorted {
            let mut lexer = self.lexer_at(start)?;
            for id in first..first.saturating_add(count) {
                let (Token::Int(offset), Token::Int(_), Token::Keyword(kind)) =
                    (lexer.token()?, lexer.token()?, lexer.token()?)
                else {
                    return None;
                };
                if kind != b"f" && offset > 0 {
                    lowest = Some((id, offset));
                    break 'subsections;
                }
            }
        }
        let Some((id, offset)) = lowest else {
            return Some(());
        };
        match self.lexer_at(u64::try_from(offset).ok()?)?.token()? {
            Token::Int(number) if u64::try_from(number).ok() == Some(id) => Some(()),
            _ => None,
        }
    }

    /// The offset `startxref` names, from the file's tail: none past the
    /// file's end or inside its header (`ParseStartXRef`).
    fn startxref(&self) -> Option<u64> {
        let mut file = &self.file;
        let tail_at = self.header + self.len.saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(tail_at)).ok()?;
        let mut tail = Vec::new();
        file.take(TAIL_BYTES).read_to_end(&mut tail).ok()?;
        let at = tail
            .windows(STARTXREF.len())
            .rposition(|w| w == STARTXREF)?;
        match Lexer::new(&tail[at + STARTXREF.len()..]).token()? {
            Token::Int(offset) => u64::try_from(offset)
                .ok()
                .filter(|offset| (PDF_HEADER_BYTES..self.len).contains(offset)),
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
                // lists, so the stream is read after the table. PDFium skips
                // the oldest section's (gl-1811).
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
            Section::Rebuilt(objects) => objects.get(&id)?.location,
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
    /// use in practice. Once the security handler is open, an object stream
    /// is decrypted before it is inflated (`DecryptObjectTree`); a
    /// cross-reference stream is read before it opens, so never is.
    fn stream_at(&self, offset: u64, id: Option<u64>) -> Option<(Obj, Vec<u8>)> {
        let mut lexer = self.lexer_at(offset)?;
        let (generation, dict) = lexer.indirect_generation(id)?;
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
        let crypt = self.crypt.as_ref().zip(id);
        let over = || {
            self.exhausted.set(true);
            None
        };
        let mut data = Vec::new();
        if flate && crypt.is_none() {
            // A zlib stream ends itself: `/Length` is not needed.
            flate2::bufread::ZlibDecoder::new(lexer.input)
                .take(limit)
                .read_to_end(&mut data)
                .ok()?;
        } else {
            drop(lexer);
            // Resolving an indirect `/Length` moves the file cursor first.
            // TODO(parity, open work item; gl-1811): PDFium searches for
            // `endstream` when `/Length` is missing or wrong; this reads by
            // `/Length` alone. Un-defer path: `ReadStream`'s fallback.
            let length = u64::try_from(self.direct_integer(dict.get(b"Length")?)?).ok()?;
            self.lexer_at(data_at)?
                .input
                .take(length.min(limit))
                .read_to_end(&mut data)
                .ok()?;
            if let Some((crypt, id)) = crypt {
                if data.len() as u64 > budget {
                    return over();
                }
                data = crypt.decrypt(id, generation, data);
                if flate {
                    let encrypted = std::mem::take(&mut data);
                    flate2::bufread::ZlibDecoder::new(encrypted.as_slice())
                        .take(limit)
                        .read_to_end(&mut data)
                        .ok()?;
                }
            }
        }
        let used = data.len() as u64;
        if used > budget {
            return over();
        }
        self.budget.set(budget - used);
        let parms = match dict.get(b"DecodeParms") {
            Some(Obj::Array(parms)) => parms.first(),
            parms => parms,
        };
        let data = unpredict(data, parms)?;
        Some((dict, data))
    }

    /// A value, or the object it refers to.
    fn resolve(&self, value: &Obj) -> Option<Obj> {
        match value {
            Obj::Ref(id) => self.object(*id),
            other => Some(other.clone()),
        }
    }

    /// An integer as PDFium's `GetIntegerFor(key, default)` reads it:
    /// `default` when the key is missing, 0 when it is not an integer.
    fn integer_or(&self, value: Option<&Obj>, default: i64) -> i64 {
        value.map_or(default, |value| self.integer(value).unwrap_or(0))
    }

    /// A string's or a name's bytes, direct or referred to; empty for any
    /// other value (`GetByteStringFor`).
    fn bytes(&self, value: Option<&Obj>) -> Vec<u8> {
        match value.and_then(|value| self.resolve(value)) {
            Some(Obj::Str(bytes) | Obj::Name(bytes)) => bytes,
            _ => Vec::new(),
        }
    }

    /// A dictionary, direct or referred to, with its object number when it
    /// has one.
    fn dict(&self, value: &Obj) -> Option<(Option<u64>, Obj)> {
        let dict = self.resolve(value)?;
        matches!(dict, Obj::Dict(_)).then(|| (value.reference(), dict))
    }

    /// `CPDF_Parser::SetEncryptHandler` with the empty password: opens the
    /// trailer's `/Encrypt` with the Standard security handler. `None` where
    /// PDFium refuses the file: no trailer, another security handler, an
    /// encryption dictionary it cannot load, or a user password that is not
    /// empty. An `/Encrypt` that is not a dictionary leaves the file read as
    /// it is, as in PDFium.
    fn set_encrypt_handler(&mut self) -> Option<()> {
        self.crypt = None;
        let trailer = self.trailer.as_ref()?;
        let Some((_, dict)) = trailer.get(b"Encrypt").and_then(|value| self.dict(value)) else {
            return Some(());
        };
        if !matches!(dict.get(b"Filter"), Some(Obj::Name(name)) if name == b"Standard") {
            return None;
        }
        let file_id = match trailer.get(b"ID").and_then(|ids| self.resolve(ids)) {
            Some(Obj::Array(ids)) => self.bytes(ids.first()),
            _ => Vec::new(),
        };
        // `LoadDict` and `LoadCryptInfo`: the cipher and the key's length.
        let version = self.integer_or(dict.get(b"V"), 0);
        let mut cipher = Cipher::Rc4;
        let key_len = if version < 4 {
            if version > 1 {
                self.integer_or(dict.get(b"Length"), 40) / 8
            } else {
                5
            }
        } else {
            let filter = self.bytes(dict.get(b"StrF"));
            if self.bytes(dict.get(b"StmF")) != filter {
                return None;
            }
            let (_, filters) = self.dict(dict.get(b"CF")?)?;
            if filter == b"Identity" {
                return Some(());
            }
            let (_, filter) = self.dict(filters.get(&filter)?)?;
            let mut bits = if version == 4 {
                match self.integer_or(filter.get(b"Length"), 0) {
                    0 => self.integer_or(dict.get(b"Length"), 128),
                    bits => bits,
                }
            } else {
                self.integer_or(dict.get(b"Length"), 256)
            };
            if bits < 0 {
                return None;
            }
            // Some writers give the length in bytes.
            if bits < 40 {
                bits *= 8;
            }
            if matches!(
                self.bytes(filter.get(b"CFM")).as_slice(),
                b"AESV2" | b"AESV3"
            ) {
                cipher = Cipher::Aes;
            }
            bits / 8
        };
        let key_len = usize::try_from(key_len).ok().filter(|len| match cipher {
            Cipher::Aes => matches!(len, 16 | 24 | 32),
            Cipher::Rc4 => (5..=16).contains(len),
        })?;
        let keys: [&[u8]; 4] = [b"O", b"U", b"UE", b"Perms"];
        let [owner, user, user_key, perms] = keys.map(|key| self.bytes(dict.get(key)));
        let crypt = Crypt::open(
            cipher,
            &Standard {
                revision: self.integer_or(dict.get(b"R"), 0),
                key_len,
                owner: &owner,
                user: &user,
                user_key: &user_key,
                perms: &perms,
                p_key: self.integer_or(dict.get(b"P"), 0),
                p_permissions: self.integer_or(dict.get(b"P"), -1),
                file_id: &file_id,
                encrypt_metadata: !matches!(dict.get(b"EncryptMetadata"), Some(Obj::Bool(false))),
            },
        )?;
        self.crypt = Some(crypt);
        Some(())
    }

    /// The pages of the catalog the trailer's `/Root` names
    /// (`CPDF_Document::TryInit`), or `None` when there is no catalog
    /// (`CPDF_Parser::GetRoot`).
    fn page_count(&self) -> Option<u64> {
        let catalog = self.object(self.trailer.as_ref()?.get(b"Root")?.reference()?)?;
        if !matches!(catalog, Obj::Dict(_)) {
            return None;
        }
        // `RetrievePageCount`: no page tree, no page; a root without kids, one.
        let Some((id, tree)) = catalog.get(b"Pages").and_then(|pages| self.dict(pages)) else {
            return Some(0);
        };
        if tree.get(b"Kids").is_none() {
            return Some(1);
        }
        Some(self.count_pages(id, tree).unwrap_or(0))
    }

    /// PDFium's `CountPages`, walked without recursion: a branch's `/Count`
    /// when it is plausible, else its kids counted, a branch by the same rule
    /// and a leaf as one page. A kid naming a branch it sits under is not
    /// counted again. `None` when the count reaches [`PAGE_MAX_NUM`].
    fn count_pages(&self, id: Option<u64>, tree: Obj) -> Option<u64> {
        // The branches whose kids are being counted, outermost first, each
        // with its object number.
        let mut branches: Vec<(std::vec::IntoIter<Obj>, Option<u64>)> = Vec::new();
        let mut count = 0_u64;
        let mut branch = Some((id, tree));
        loop {
            if let Some((id, node)) = branch.take() {
                let own = self.integer_or(node.get(b"Count"), 0);
                if (1..PAGE_MAX_NUM).contains(&own) {
                    count += own.cast_unsigned();
                } else if let Some(Obj::Array(kids)) =
                    node.get(b"Kids").and_then(|kids| self.resolve(kids))
                {
                    branches.push((kids.into_iter(), id));
                }
            }
            // The total only grows, so it reaches the cap exactly when one of
            // PDFium's per-branch counts does.
            if count >= PAGE_MAX_NUM.cast_unsigned() {
                return None;
            }
            let Some((kids, _)) = branches.last_mut() else {
                break;
            };
            let Some(kid) = kids.next() else {
                branches.pop();
                continue;
            };
            let Some((id, kid)) = self.dict(&kid) else {
                continue;
            };
            if id.is_some_and(|id| branches.iter().any(|(_, above)| *above == Some(id))) {
                continue;
            }
            // A node whose `/Type` is neither `/Pages` nor `/Page` is a branch
            // when it has kids.
            let is_branch = match kid.get(b"Type") {
                Some(Obj::Name(node_type)) if node_type == b"Pages" => true,
                Some(Obj::Name(node_type)) if node_type == b"Page" => false,
                _ => kid.get(b"Kids").is_some(),
            };
            if is_branch {
                branch = Some((id, kid));
            } else {
                count += 1;
            }
        }
        Some(count)
    }

    /// PDFium's `RebuildCrossRef`: a table of the objects a scan of the file
    /// finds, put before the sections already read, and the trailers it finds
    /// (each `trailer` dictionary and each cross-reference stream's) merged
    /// in file order over the newest one read. An object stream the scan
    /// finds is read, undecrypted, for the objects it holds. `None` with no
    /// trailer or no object.
    ///
    /// TODO(parity, open work item; gl-1811): the scan reads the first
    /// [`MAX_ARCHIVE_ENTRY_BYTES`] of the file, where PDFium reads it all, so a
    /// larger PDF whose offsets are wrong is not counted; and it skips a
    /// stream's data to the first `endstream`, where
    /// PDFium's `ReadStream` first trusts a direct `/Length`. Un-defer path:
    /// scan the whole file once the owner weighs its cost on every scan of an
    /// uncountable file, and skip by `/Length` when `endstream` follows it.
    fn rebuild(&mut self) -> Option<()> {
        let mut objects = HashMap::new();
        let mut trailer = None;
        let mut lexer = self.lexer_at(0)?;
        // The last two integers and where each starts: an object's number and
        // generation when `obj` follows.
        let mut numbers: Vec<(u64, u64)> = Vec::new();
        // The object whose dictionary was just read, should `stream` follow.
        let mut last = None;
        loop {
            lexer.skip_space();
            let at = lexer.pos;
            let Some(token) = lexer.token() else {
                break;
            };
            let object = last.take();
            match token {
                Token::Int(n) if n >= 0 => {
                    numbers.push((n.cast_unsigned(), at));
                    if numbers.len() > 2 {
                        numbers.remove(0);
                    }
                    continue;
                }
                Token::Keyword(word) if word == b"obj" && numbers.len() == 2 => {
                    let ((id, at), (generation, _)) = (numbers[0], numbers[1]);
                    lexer.nodes = MAX_NODES;
                    let value = lexer.object(0);
                    if id <= MAX_OBJECT_NUMBER {
                        rebuilt_normal(&mut objects, id, generation, at)?;
                    }
                    last = value
                        .filter(|value| matches!(value, Obj::Dict(_)))
                        .map(|dict| (id, at, dict));
                }
                Token::Keyword(word) if word == b"stream" => {
                    // The data is skipped, not read as tokens.
                    if lexer.skip_past(b"endstream").is_none() {
                        break;
                    }
                    let kind = object.as_ref().and_then(|(_, _, dict)| dict.get(b"Type"));
                    match (kind, object.as_ref()) {
                        (Some(Obj::Name(kind)), Some((_, _, dict))) if kind == b"XRef" => {
                            trailer = Some(merge_trailer(trailer.as_ref(), dict));
                        }
                        (Some(Obj::Name(kind)), Some(&(id, at, ref dict)))
                            if kind == b"ObjStm" && id <= MAX_OBJECT_NUMBER =>
                        {
                            // Its own reads move the file cursor: put it back
                            // where the scan's buffer ends.
                            let resume = lexer.pos + lexer.input.buffer().len() as u64;
                            let held = self.object_stream_numbers(at, id, dict);
                            (&self.file)
                                .seek(SeekFrom::Start(self.header + resume))
                                .ok()?;
                            for number in held.into_iter().filter(|n| *n <= MAX_OBJECT_NUMBER) {
                                rebuilt_compressed(&mut objects, number, id)?;
                            }
                        }
                        _ => {}
                    }
                }
                Token::Keyword(word) if word == b"trailer" => {
                    lexer.nodes = MAX_NODES;
                    if let Some(dict @ Obj::Dict(_)) = lexer.object(0) {
                        trailer = Some(merge_trailer(trailer.as_ref(), &dict));
                    }
                }
                _ => {}
            }
            numbers.clear();
        }
        // A table rebuilt from part of the file is not trusted.
        if self.len > MAX_ARCHIVE_ENTRY_BYTES {
            self.exhausted.set(true);
        }
        if let Some(rebuilt) = trailer {
            self.trailer = Some(merge_trailer(self.trailer.as_ref(), &rebuilt));
        }
        let found = !objects.is_empty() || !self.sections.is_empty();
        self.sections.insert(0, Section::Rebuilt(objects));
        (self.trailer.is_some() && found).then_some(())
    }

    /// The object numbers an object stream holds, as PDFium's rebuild reads
    /// them (`CPDF_ObjectStream::Init`): its first `/N` pairs of numbers, the
    /// second of each its offset; a pair whose number is 0 or not an integer
    /// is skipped. Empty for a stream that is not an object stream or cannot
    /// be read.
    fn object_stream_numbers(&self, offset: u64, id: u64, dict: &Obj) -> Vec<u64> {
        let (Some(n), Some(first)) = (
            dict.get(b"N").and_then(Obj::integer),
            dict.get(b"First").and_then(Obj::integer),
        ) else {
            return Vec::new();
        };
        let in_range = u64::try_from(n).is_ok_and(|n| n <= MAX_OBJECT_NUMBER);
        if !in_range || first < 0 {
            return Vec::new();
        }
        let Some((_, data)) = self.stream_at(offset, Some(id)) else {
            return Vec::new();
        };
        let mut head = Lexer::new(data.as_slice());
        let mut numbers = Vec::new();
        for _ in 0..n {
            // Past the table's cap the rebuild fails anyway.
            if numbers.len() as u64 > MAX_REBUILT_OBJECTS {
                break;
            }
            let (Some(number), Some(_)) = (head.token(), head.token()) else {
                break;
            };
            if let Token::Int(number @ 1..) = number {
                numbers.push(number.cast_unsigned());
            }
        }
        numbers
    }
}

/// `CPDF_CrossRefTable::AddNormal`: object `id` at `offset`, unless an object
/// of a later generation was found. `None` past [`MAX_REBUILT_OBJECTS`]
/// objects.
fn rebuilt_normal(
    objects: &mut HashMap<u64, Rebuilt>,
    id: u64,
    generation: u64,
    offset: u64,
) -> Option<()> {
    let object = objects.entry(id).or_default();
    if object.generation <= generation {
        object.generation = generation;
        object.location = Some(Location::Offset(offset));
    }
    (objects.len() as u64 <= MAX_REBUILT_OBJECTS).then_some(())
}

/// `CPDF_CrossRefTable::AddCompressed`: object `id` in object stream
/// `stream`, unless an object of a later generation was found or `id` is
/// itself an object stream. `None` past [`MAX_REBUILT_OBJECTS`] objects.
fn rebuilt_compressed(objects: &mut HashMap<u64, Rebuilt>, id: u64, stream: u64) -> Option<()> {
    let object = objects.entry(id).or_default();
    if object.generation == 0 && !object.object_stream {
        object.location = Some(Location::InStream(stream));
        objects.entry(stream).or_default().object_stream = true;
    }
    (objects.len() as u64 <= MAX_REBUILT_OBJECTS).then_some(())
}

/// `CPDF_CrossRefTable::UpdateTrailer`: `newer`'s entries over `older`'s,
/// of the [`TRAILER_KEYS`] this reader uses.
fn merge_trailer(older: Option<&Obj>, newer: &Obj) -> Obj {
    Obj::Dict(
        TRAILER_KEYS
            .iter()
            .filter_map(|key| {
                let value = newer.get(key).or_else(|| older?.get(key))?;
                Some((key.to_vec(), value.clone()))
            })
            .collect(),
    )
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
    /// A string's bytes, its escapes and hex digits decoded.
    Str(Vec<u8>),
    Bool(bool),
    Array(Vec<Obj>),
    Dict(Vec<(Vec<u8>, Obj)>),
    /// A real, null, or a container nested too deep.
    Other,
}

impl Obj {
    /// A dictionary's value for `key`: the last one, as PDFium keeps a
    /// repeated key's.
    fn get(&self, key: &[u8]) -> Option<&Obj> {
        match self {
            Obj::Dict(entries) => entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
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
    /// A literal or hex string's bytes.
    Str(Vec<u8>),
    /// A bare word: `obj`, `R`, `stream`, `xref`, `trailer`, `n`, `true`…
    Keyword(Vec<u8>),
    ArrayOpen,
    ArrayClose,
    DictOpen,
    DictClose,
    /// A real, or a stray delimiter.
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
            b'<' => Token::Str(self.hex_string()?),
            b'(' => Token::Str(self.literal_string()?),
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

    /// A hex string's bytes after its `<`, as PDFium's `ReadHexString`
    /// decodes them: characters other than hex digits are skipped, and an odd
    /// last digit is its byte's high half.
    fn hex_string(&mut self) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut high = None;
        loop {
            let byte = self.bump()?;
            if byte == b'>' {
                bytes.extend(high);
                return Some(bytes);
            }
            let Some(digit) = char::from(byte).to_digit(16) else {
                continue;
            };
            // A hex digit is at most 15.
            #[allow(clippy::cast_possible_truncation)]
            let digit = digit as u8;
            match high.take() {
                Some(high) => bytes.push(high | digit),
                None => high = Some(digit << 4),
            }
        }
    }

    /// A literal string's bytes after its `(`, as PDFium's `ReadString`
    /// decodes them: balanced parentheses kept, the escapes `\n`, `\r`, `\t`,
    /// `\b`, `\f` and up to three octal digits, a backslash before a line end
    /// dropped with it, and any other escaped byte kept as itself.
    fn literal_string(&mut self) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut depth = 0_u64;
        loop {
            match self.bump()? {
                b')' if depth == 0 => return Some(bytes),
                b'\\' => match self.bump()? {
                    b'n' => bytes.push(b'\n'),
                    b'r' => bytes.push(b'\r'),
                    b't' => bytes.push(b'\t'),
                    b'b' => bytes.push(0x08),
                    b'f' => bytes.push(0x0C),
                    b'\r' => {
                        if self.peek() == Some(b'\n') {
                            self.bump();
                        }
                    }
                    b'\n' => {}
                    digit @ b'0'..=b'7' => {
                        let mut code = digit - b'0';
                        for _ in 0..2 {
                            match self.peek() {
                                Some(next @ b'0'..=b'7') => {
                                    self.bump();
                                    code = code.wrapping_mul(8).wrapping_add(next - b'0');
                                }
                                _ => break,
                            }
                        }
                        bytes.push(code);
                    }
                    other => bytes.push(other),
                },
                byte => {
                    match byte {
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                    bytes.push(byte);
                }
            }
        }
    }

    /// Consumes the input up to and through the next `word`, as a stream's
    /// data is skipped to its `endstream`; `None` at the end of the input.
    fn skip_past(&mut self, word: &[u8; 9]) -> Option<()> {
        let mut window = [0_u8; 9];
        loop {
            let buffer = self.input.fill_buf().ok()?;
            if buffer.is_empty() {
                return None;
            }
            let found = buffer.iter().position(|&byte| {
                window.rotate_left(1);
                window[8] = byte;
                window == *word
            });
            let used = found.map_or(buffer.len(), |at| at + 1);
            self.input.consume(used);
            self.pos += used as u64;
            if found.is_some() {
                return Some(());
            }
        }
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
            Token::Str(bytes) => Some(Obj::Str(bytes)),
            Token::Keyword(word) if word == b"true" || word == b"false" => {
                Some(Obj::Bool(word == b"true"))
            }
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
        self.indirect_generation(id).map(|(_, value)| value)
    }

    /// [`Lexer::indirect`], with the object's generation.
    fn indirect_generation(&mut self, id: Option<u64>) -> Option<(u64, Obj)> {
        let (Token::Int(number), Token::Int(generation), Token::Keyword(word)) =
            (self.token()?, self.token()?, self.token()?)
        else {
            return None;
        };
        if word != b"obj" || id.is_some_and(|id| u64::try_from(number).ok() != Some(id)) {
            return None;
        }
        Some((generation.cast_unsigned(), self.object(0)?))
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
        assert_eq!(count(&incremental_update()), Some(3));
    }

    /// Two pages, then a revision that adds a third: its own table, and its
    /// own definitions of the page tree (object 2) and the new page (5).
    fn incremental_update() -> Vec<u8> {
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
        pdf
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
        // A `startxref` pointing past the end, and a `/Prev` that loops: the
        // sections cannot be followed, and the table a scan rebuilds counts.
        let mut dangling = whole;
        let at = String::from_utf8_lossy(&dangling)
            .rfind("startxref")
            .unwrap();
        dangling.truncate(at);
        dangling.extend_from_slice(b"startxref\n999999\n%%EOF\n");
        assert_eq!(count(&dangling), Some(3));
        let objects = page_tree(1);
        let mut cyclic = classic_pdf(&refs(&objects), 1);
        let text = String::from_utf8_lossy(&cyclic).into_owned();
        let xref = text.rfind("xref\n0").unwrap();
        let replaced = text.replacen(" >>\nstartxref", &format!(" /Prev {xref} >>\nstartxref"), 1);
        cyclic = replaced.into_bytes();
        assert_eq!(count(&cyclic), Some(1));
    }

    fn fixture(name: &str) -> Vec<u8> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read(dir.join("src/books/page_count/testdata").join(name)).unwrap()
    }

    /// qpdf's encryptions with the empty user password (`--encrypt "" owner
    /// 40|128|256`, testdata/README.md), the page tree written as it is and
    /// packed in an encrypted object stream: PDFium opens each without a
    /// password, and `pdfinfo` counts seven pages in each.
    #[rstest::rstest]
    #[case("rc4-40.pdf")]
    #[case("rc4-40-objstm.pdf")]
    #[case("rc4-128.pdf")]
    #[case("rc4-128-objstm.pdf")]
    #[case("aes-128.pdf")]
    #[case("aes-128-objstm.pdf")]
    #[case("aes-256.pdf")]
    #[case("aes-256-objstm.pdf")]
    #[case("aes-256-r5.pdf")]
    #[case("aes-256-r5-objstm.pdf")]
    #[case("aes-128-cleartext-metadata-objstm.pdf")]
    #[case("aes-256-cleartext-metadata-objstm.pdf")]
    #[case("objstm.pdf")]
    fn an_encrypted_pdf_opens_with_the_empty_password(#[case] name: &str) {
        assert_eq!(count(&fixture(name)), Some(7));
    }

    /// A user password that is not empty: PDFium refuses the file, so it is
    /// not counted, even where its page tree is not encrypted.
    #[rstest::rstest]
    #[case("user-password-rc4.pdf")]
    #[case("user-password-objstm.pdf")]
    fn a_user_password_is_refused(#[case] name: &str) {
        assert_eq!(count(&fixture(name)), None);
    }

    /// A broken writer's word after the header: every offset the file gives,
    /// `startxref`'s included, now misses by its five bytes.
    fn shifted(pdf: &[u8]) -> Vec<u8> {
        let at = pdf.iter().position(|b| *b == b'\n').unwrap() + 1;
        [&pdf[..at], b"junk\n", &pdf[at..]].concat()
    }

    /// Wrong offsets are rebuilt from a scan of the file, as PDFium does: its
    /// objects, the object streams it finds, and its trailers.
    #[test]
    fn wrong_offsets_are_rebuilt_from_a_scan() {
        let classic = classic_pdf(&refs(&page_tree(7)), 1);
        assert_eq!(count(&shifted(&classic)), Some(7));
        // The later of two definitions of an object wins.
        assert_eq!(count(&shifted(&incremental_update())), Some(3));
        // An object stream behind a cross-reference stream, whose dictionary
        // is the trailer.
        assert_eq!(count(&shifted(&fixture("objstm.pdf"))), Some(7));
        assert_eq!(count(&shifted(&objstm_pdf(&Packing::Flate))), Some(41));
        // An encrypted file opens on the rebuilt table, but the scan reads an
        // object stream before the security handler opens, undecrypted, as
        // PDFium's does: a page tree packed in one is not found.
        assert_eq!(count(&shifted(&fixture("aes-256.pdf"))), Some(7));
        assert_eq!(count(&shifted(&fixture("aes-256-objstm.pdf"))), None);
        // `startxref` right, the table's offsets wrong: its lowest object
        // does not start with its number.
        let text = String::from_utf8_lossy(&shifted(&classic)).into_owned();
        let at = text.rfind("startxref\n").unwrap() + 10;
        let xref: usize = text[at..].lines().next().unwrap().parse().unwrap();
        let fixed = format!("{}startxref\n{}\n%%EOF\n", &text[..at - 10], xref + 5);
        assert_eq!(count(fixed.as_bytes()), Some(7));
    }

    /// The newest trailer leaves `/Root` out: an older one's is used, as
    /// PDFium merges the trailers of the `/Prev` chain.
    #[test]
    fn an_older_trailer_fills_in_the_newer_one() {
        // The trailer follows every offset the file gives, so no offset moves.
        let text = String::from_utf8(incremental_update()).unwrap();
        let without_root = text.replacen("/Size 6 /Root 1 0 R /Prev", "/Size 6 /Prev", 1);
        assert_ne!(without_root, text);
        assert_eq!(count(without_root.as_bytes()), Some(3));
    }

    /// `%PDF` up to 1024 bytes into the file: every offset counts from it.
    /// Further in, or not there at all, the file is no PDF, as for PDFium.
    #[test]
    fn offsets_count_from_the_header() {
        let classic = classic_pdf(&refs(&page_tree(7)), 1);
        let prefixed = |junk: usize| [vec![b'x'; junk], classic.clone()].concat();
        assert_eq!(count(&prefixed(1024)), Some(7));
        assert_eq!(count(&prefixed(1025)), None);
        assert_eq!(count(b"%PDF-1"), None);
    }

    /// A `startxref` inside the header names no section: the table is
    /// rebuilt.
    #[test]
    fn a_startxref_inside_the_header_is_rebuilt() {
        let text = String::from_utf8(classic_pdf(&refs(&page_tree(2)), 1)).unwrap();
        let at = text.rfind("startxref\n").unwrap() + 10;
        assert_eq!(
            count(format!("{}8\n%%EOF\n", &text[..at]).as_bytes()),
            Some(2)
        );
    }

    /// The table locates the page tree at the catalog's offset: the first
    /// pass finds a catalog and no page, and the table rebuilt from a scan
    /// finds the tree.
    #[test]
    fn a_page_tree_the_table_misses_is_found_by_a_rebuild() {
        let classic = String::from_utf8(classic_pdf(&refs(&page_tree(7)), 1)).unwrap();
        let catalog = classic.find("1 0 obj").unwrap();
        let tree = classic.find("2 0 obj").unwrap();
        let wrong = classic.replacen(
            &format!("{tree:010} 00000 n"),
            &format!("{catalog:010} 00000 n"),
            1,
        );
        assert_eq!(count(wrong.as_bytes()), Some(7));
    }

    /// A walk of a page tree that needs more reads than a count allows is not
    /// trusted, rather than counting the kids it reached.
    #[test]
    fn a_walk_past_the_read_cap_is_not_counted() {
        let flat = |pages: u64| {
            let mut objects = page_tree(pages);
            objects[1].1 = objects[1].1.replace(&format!("/Count {pages}"), "/Count 0");
            classic_pdf(&refs(&objects), 1)
        };
        // Two reads for each kid: its table entry and the object.
        assert_eq!(count(&flat(8_000)), Some(8_000));
        assert_eq!(count(&flat(MAX_READS / 2 + 1)), None);
    }

    /// A table that fails PDFium's check is rebuilt at once, and a rebuilt
    /// table that shows no page is no PDF: an empty page tree behind a
    /// 1-based table is not counted, where behind a sound one it has none.
    #[test]
    fn a_table_that_fails_its_check_is_rebuilt_before_any_lookup() {
        let empty = classic_pdf(
            &[
                (1, "<< /Type /Catalog /Pages 2 0 R >>"),
                (2, "<< /Type /Pages /Kids [] /Count 0 >>"),
            ],
            1,
        );
        assert_eq!(count(&empty), Some(0));
        let one_based = String::from_utf8_lossy(&empty).replacen("xref\n0 ", "xref\n1 ", 1);
        assert_eq!(count(one_based.as_bytes()), None);
    }

    /// A key repeated in a dictionary has its last value, as in PDFium.
    #[test]
    fn a_repeated_key_has_its_last_value() {
        let dict = Lexer::new(b"<< /Count 1 /Count 2 >>".as_slice())
            .object(0)
            .unwrap();
        assert_eq!(dict.get(b"Count"), Some(&Obj::Int(2)));
    }

    /// A rebuild reads the file's first [`MAX_ARCHIVE_ENTRY_BYTES`] only, so a
    /// longer file is not counted from it, even with its objects and trailer
    /// in that window.
    #[test]
    fn a_rebuild_of_a_file_past_the_scan_window_is_not_counted() {
        // A stream with no end: the scan skips to the end of its window.
        let pdf = [
            classic_pdf(&refs(&page_tree(7)), 1),
            b"9 0 obj\n<< >>\nstream\n".to_vec(),
        ]
        .concat();
        let rebuilt = |len: u64| {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(&pdf).unwrap();
            // Sparse zeros: `startxref` is no longer in the file's tail.
            file.set_len(len).unwrap();
            pdf_page_count(file, MAX_ARCHIVE_ENTRY_BYTES)
        };
        assert_eq!(rebuilt(pdf.len() as u64 + 8192), Some(7));
        assert_eq!(rebuilt(MAX_ARCHIVE_ENTRY_BYTES + 1), None);
    }

    /// A rebuilt table records no object numbered past PDFium's
    /// `kMaxObjectNumber`.
    #[test]
    fn a_rebuilt_table_skips_numbers_past_the_limit() {
        let objects = [
            (1, "<< /Type /Catalog /Pages 25165825 0 R >>"),
            (3, "<< /Type /Page >>"),
            (25_165_825, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        ];
        let mut pdf = b"%PDF-1.5\n".to_vec();
        for (id, body) in objects {
            pdf.extend_from_slice(format!("{id} 0 obj\n{body}\nendobj\n").as_bytes());
        }
        pdf.extend_from_slice(b"trailer\n<< /Root 1 0 R >>\n%%EOF\n");
        assert_eq!(count(&pdf), None);
        let within = String::from_utf8(pdf)
            .unwrap()
            .replace("25165825", "25165824");
        assert_eq!(count(within.as_bytes()), Some(1));
    }

    /// A broken writer's table whose first subsection starts at 1 instead of
    /// 0 still lists object 0 first, so every number is off by one: PDFium's
    /// check of the table fails, and the scan finds the objects.
    #[test]
    fn a_table_whose_first_subsection_starts_at_one_is_rebuilt() {
        let classic = classic_pdf(&refs(&page_tree(7)), 1);
        let text = String::from_utf8_lossy(&classic).replacen("xref\n0 ", "xref\n1 ", 1);
        assert_eq!(count(text.as_bytes()), Some(7));
    }

    /// A file with no cross-reference section, its objects in `order`: an
    /// object stream (4) holding a page tree of 5 pages, and the same object
    /// (2) written directly with 9.
    fn scanned(order: &[&str]) -> Vec<u8> {
        let packed = "2 0 << /Type /Pages /Kids [3 0 R] /Count 5 >>";
        let parts = |part: &str| match part {
            "packed" => format!(
                "4 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n{packed}\nendstream\nendobj\n",
                packed.len()
            ),
            "direct" => "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 9 >>\nendobj\n".to_owned(),
            "later" => "2 1 obj\n<< /Type /Pages /Kids [3 0 R] /Count 9 >>\nendobj\n".to_owned(),
            other => other.to_owned(),
        };
        let body: String = order.iter().copied().map(parts).collect();
        format!(
            "%PDF-1.5\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n3 0 obj\n<< /Type /Page >>\nendobj\n{body}trailer\n<< /Root 1 0 R >>\n%%EOF\n"
        )
        .into_bytes()
    }

    /// PDFium's rebuilt table: a later object of the same generation or a
    /// later one wins, and an object stream wins over an object of
    /// generation 0 found before it, not over a later generation.
    #[rstest::rstest]
    #[case(&["direct", "packed"], Some(5))]
    #[case(&["packed", "direct"], Some(9))]
    #[case(&["later", "packed"], Some(9))]
    #[case(&["packed"], Some(5))]
    fn a_rebuilt_table_keeps_pdfiums_order(#[case] order: &[&str], #[case] pages: Option<u64>) {
        assert_eq!(count(&scanned(order)), pages);
    }

    /// PDFium distrusts a page-tree root's `/Count` at zero or below, at its
    /// page limit and past it, or not an integer, and counts its kids.
    #[rstest::rstest]
    #[case("0")]
    #[case("-3")]
    #[case("1048575")]
    #[case("(seven)")]
    fn an_implausible_count_is_walked(#[case] value: &str) {
        let mut objects = page_tree(7);
        objects[1].1 = objects[1].1.replace("/Count 7", &format!("/Count {value}"));
        assert_eq!(count(&classic_pdf(&refs(&objects), 1)), Some(7));
    }

    /// PDFium's walk of the page tree (`CountPages`): a branch with a
    /// plausible `/Count` counts it, a branch without one counts its kids, a
    /// leaf counts one; a `/Type` decides which a kid is, its kids when it
    /// has none; a kid naming a branch above it, or not a dictionary, counts
    /// nothing.
    #[test]
    fn the_page_tree_is_walked_as_pdfium_walks_it() {
        let leaf = "<< /Type /Page /Parent 2 0 R >>";
        let objects = [
            (1, "<< /Type /Catalog /Pages 2 0 R >>"),
            (
                2,
                "<< /Type /Pages /Count 0 /Kids [3 0 R 4 0 R 5 0 R 2 0 R 6 0 R 7 0 R << /Type /Page >> 8 0 R 9 0 R] >>",
            ),
            // Trusted: 3.
            (3, "<< /Type /Pages /Kids [10 0 R] /Count 3 >>"),
            (4, leaf),
            // No `/Type`, kids: a branch of two, its own number skipped.
            (5, "<< /Kids [10 0 R 11 0 R 5 0 R] >>"),
            (6, "42"),
            // A leaf, whatever kids it names.
            (7, "<< /Type /Page /Kids [10 0 R 11 0 R] >>"),
            // Object 8 is missing; 9 is a branch with no count and no kids.
            (9, "<< /Type /Pages /Count -1 >>"),
            (10, leaf),
            (11, leaf),
        ];
        assert_eq!(count(&classic_pdf(&objects, 1)), Some(3 + 1 + 2 + 1 + 1));
    }

    /// `RetrievePageCount`'s edges: a page-tree root without kids is one page
    /// whatever its `/Count`; a catalog without a page tree has none, as
    /// does a count reaching PDFium's page limit; one below it is kept.
    #[test]
    fn page_tree_edges_count_as_pdfium_counts_them() {
        let root = |tree: &str| {
            classic_pdf(
                &[
                    (1, "<< /Type /Catalog /Pages 2 0 R >>"),
                    (2, tree),
                    (3, "<< /Type /Pages /Count 1048574 >>"),
                    (4, "<< /Type /Page >>"),
                ],
                1,
            )
        };
        assert_eq!(count(&root("<< /Type /Pages /Count 5 >>")), Some(1));
        assert_eq!(
            count(&root("<< /Type /Pages /Count 0 /Kids [3 0 R] >>")),
            Some(1_048_574)
        );
        assert_eq!(
            count(&root("<< /Type /Pages /Count 0 /Kids [3 0 R 4 0 R] >>")),
            Some(0)
        );
        let no_tree = classic_pdf(&[(1, "<< /Type /Catalog >>")], 1);
        assert_eq!(count(&no_tree), Some(0));
    }

    /// Strings decode as PDFium's `ReadString` and `ReadHexString` read them,
    /// and booleans are kept.
    #[rstest::rstest]
    #[case(b"(a\\(b\\)c)", Obj::Str(b"a(b)c".to_vec()))]
    #[case(b"(nested (parens) kept)", Obj::Str(b"nested (parens) kept".to_vec()))]
    #[case(b"(\\n\\r\\t\\b\\f\\q)", Obj::Str(b"\n\r\t\x08\x0cq".to_vec()))]
    #[case(b"(\\101\\60\\0659\\501)", Obj::Str(b"A059A".to_vec()))]
    #[case(b"(joined\\\r\nhere, raw\r\nkept)", Obj::Str(b"joinedhere, raw\r\nkept".to_vec()))]
    #[case(b"<41 4 2 4x1 4>", Obj::Str(b"ABA@".to_vec()))]
    #[case(b"true", Obj::Bool(true))]
    #[case(b"false", Obj::Bool(false))]
    fn strings_and_booleans_decode_as_pdfium_reads_them(#[case] input: &[u8], #[case] value: Obj) {
        assert_eq!(Lexer::new(input).object(0), Some(value));
    }

    /// Stream data is skipped through `endstream`, across buffer boundaries
    /// and after a false start that overlaps it.
    #[test]
    fn stream_data_is_skipped_through_endstream() {
        let data = b"\x00(<endstrendstream tail".as_slice();
        let mut lexer = Lexer::new(BufReader::with_capacity(4, data));
        assert_eq!(lexer.skip_past(b"endstream"), Some(()));
        assert_eq!(lexer.token(), Some(Token::Keyword(b"tail".to_vec())));
        assert_eq!(lexer.pos, data.len() as u64);
        assert_eq!(
            Lexer::new(b"no end".as_slice()).skip_past(b"endstream"),
            None
        );
    }
}
