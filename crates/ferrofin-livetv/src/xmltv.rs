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

use std::borrow::Cow;

use std::io::BufRead;

use chrono::{DateTime, FixedOffset, TimeZone};
use encoding_rs::{DecoderResult, Encoding};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

/// A channel definition from the guide.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct XmltvChannel {
    /// The `id` attribute — the join key against tuner channels' `tvg-id`.
    pub id: String,
    /// The first `<display-name>`. Upstream `ProcessNode` prefers the one
    /// whose `lang` matches the provider's or server's preferred language;
    /// that pick is not ported yet and is tracked with the programme fields'
    /// language pick in gl-684.
    pub display_name: String,
    /// The channel number: the last `<display-name>` that reads as a number
    /// once `-` and `_` become `.` (Jellyfin.XmlTv `SetChannelNumber`), in
    /// that replaced form — `4-1` gives `4.1`.
    pub number: Option<String>,
    /// The `src` of the channel's first own `<icon>` that has a `src`, `width`
    /// or `height` (`Icons.FirstOrDefault()?.Source`), when it has one.
    pub icon: Option<String>,
}

impl XmltvChannel {
    /// The number a listings provider reports for this channel:
    /// `string.IsNullOrWhiteSpace(c.Number) ? c.Id : c.Number`
    /// (`XmlTvListingsProvider.GetChannels`).
    #[must_use]
    pub fn listings_number(&self) -> &str {
        self.number
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(&self.id)
    }
}

/// A single programme (an airing on a channel over a time range).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)] // one field per XMLTV flag element
pub struct XmltvProgramme {
    /// The `channel` attribute — matches an [`XmltvChannel::id`].
    pub channel_id: String,
    /// Airing start (parsed from the `start` attribute), keeping the offset the
    /// file declared.
    ///
    /// Upstream's `XmlTvProgram.StartDate` is a `DateTimeOffset`, and
    /// `XmlTvListingsProvider.GetProgramInfo` renders it with `{1:O}` into the
    /// programme's external id (v10.11.8 XmlTvListingsProvider.cs:216) — which
    /// `LiveTvDtoService.GetInternalProgramId` then hashes into the item GUID.
    /// Normalising to UTC here would silently change every programme id for any
    /// guide not published at `+0000`, so the source offset is load-bearing and
    /// is kept.
    pub start: Option<DateTime<FixedOffset>>,
    /// Airing end (parsed from the `stop` attribute), keeping the offset the
    /// file declared.
    pub stop: Option<DateTime<FixedOffset>>,
    /// `<title>`.
    pub title: String,
    /// `<sub-title>` — the episode title, when present.
    pub sub_title: Option<String>,
    /// `<desc>` — the synopsis, when present.
    pub desc: Option<String>,
    /// All `<category>` values.
    pub categories: Vec<String>,
    /// The `src` of the programme's first own `<icon>` that has a `src`,
    /// `width` or `height` (`Icons.FirstOrDefault()?.Source`), when it has one.
    /// Icons inside `<rating>` and `<star-rating>` belong to those.
    pub icon: Option<String>,
    /// The text of the programme's first own `<image>` of type `still`
    /// (`Images.FirstOrDefault(m => m.Type == ImageType.Still)?.Path`), which
    /// `XmlTvListingsProvider.GetProgramInfo` makes its `ThumbImageUrl`.
    pub still_image: Option<String>,
    /// The text of the programme's first own `<image>` of type `backdrop`,
    /// its `BackdropImageUrl`.
    pub backdrop_image: Option<String>,
    /// Production year from `<date>` (first four digits), when present.
    pub year: Option<i32>,
    /// The text of the last `<episode-num>` whose `system` the reader understands
    /// (`xmltv_ns`, e.g. `0.5.` → S1E6, or `SxxExx`).
    pub episode_num: Option<String>,
    /// `Episode.Series` — the 1-based season number from that `<episode-num>`.
    pub season_number: Option<i32>,
    /// `Episode.Episode` — the 1-based episode number from that `<episode-num>`.
    pub episode_number: Option<i32>,
    /// `true` when a `<new>` element is present.
    pub is_new: bool,
    /// `true` when a `<premiere>` element is present, with or without text.
    pub is_premiere: bool,
    /// `true` when a `<previously-shown>` element is present.
    pub is_previously_shown: bool,
    /// `true` when a `<live>` element is present (not in the XMLTV DTD, but
    /// Jellyfin 12 reads it).
    pub is_live: bool,
    /// The content rating (`TV-PG`) of the programme's first `<rating>`
    /// (`Ratings.FirstOrDefault()?.Value`), when it has a value. A
    /// `<star-rating>` never supplies it.
    pub rating: Option<String>,
}

/// The parsed guide: channel definitions and their programmes.
#[derive(Debug, Clone, Default)]
pub struct Xmltv {
    /// Channel definitions.
    pub channels: Vec<XmltvChannel>,
    /// Programme airings.
    pub programmes: Vec<XmltvProgramme>,
    /// Why the document is not a readable XMLTV guide, when it is not: empty,
    /// not well-formed, cut off inside an element, or rooted at something other
    /// than `<tv>`. A guide refresh treats such a document the way Jellyfin 12.1
    /// treats a throwing `GetProgramsAsync` (`hasErrors`): it keeps the stored
    /// guide rather than replacing it with what was read.
    pub error: Option<String>,
}

/// Parses an XMLTV document. Malformed XML yields whatever was read before the
/// error, with [`Xmltv::error`] saying what went wrong, so a caller can tell a
/// partial read from a complete guide.
#[must_use]
pub fn parse_xmltv(xml: &str) -> Xmltv {
    parse_xmltv_where(xml, |_| true)
}

/// [`parse_xmltv`], keeping only the programmes `keep` accepts. Each one is
/// judged as its element closes and dropped there, so a reader that wants a
/// few channels' airings over a window never holds the rest of a large
/// guide. Channels and [`Xmltv::error`] are reported in full, so
/// `parse_xmltv_where(xml, |_| false)` reads just the channel list and whether
/// the document is readable.
///
/// The text is already decoded, so an encoding the document declares is not
/// applied again.
#[must_use]
pub fn parse_xmltv_where(xml: &str, keep: impl FnMut(&XmltvProgramme) -> bool) -> Xmltv {
    read_events(Reader::from_str(xml), keep)
}

/// [`parse_xmltv_where`] over a document read as it arrives, from a file or
/// any other byte source, so the document is never held whole.
///
/// The bytes are decoded the way `XmlTvReader`'s `XmlReader` decodes the
/// cached file (Jellyfin.XmlTv `XmlTvReader.CreateXmlTextReader`): by the
/// byte-order mark, else by the `<?xml … encoding="…"?>` declaration, else as
/// UTF-8. A byte sequence that is not valid in that encoding makes the
/// document unreadable ([`Xmltv::error`]), as it makes `XmlReader` throw. An
/// encoding name is looked up by the WHATWG rules, so `ISO-8859-1` reads as
/// `windows-1252`, which differs from .NET's Latin-1 only in the C1 control
/// range.
#[must_use]
pub fn read_xmltv_where<R: BufRead>(
    mut source: R,
    keep: impl FnMut(&XmltvProgramme) -> bool,
) -> Xmltv {
    // The head, up to the end of its first markup (`?>` for a declaration),
    // read whatever the source's buffer size, then read again by the parser.
    let mut head = Vec::new();
    while head.len() < HEAD_BYTES && !head.contains(&b'>') {
        let available = match source.fill_buf() {
            Ok(available) => available,
            Err(e) => {
                return Xmltv {
                    error: Some(format!("the XMLTV document could not be read: {e}")),
                    ..Xmltv::default()
                };
            }
        };
        if available.is_empty() {
            break;
        }
        let take = available.len().min(HEAD_BYTES - head.len());
        head.extend_from_slice(&available[..take]);
        source.consume(take);
    }
    let encoding = document_encoding(&head);
    let source = std::io::Read::chain(std::io::Cursor::new(head), source);
    match encoding {
        Ok(encoding) if encoding == encoding_rs::UTF_8 => {
            read_events(Reader::from_reader(source), keep)
        }
        Ok(encoding) => read_events(Reader::from_reader(Decoded::new(source, encoding)), keep),
        Err(error) => Xmltv {
            error: Some(error),
            ..Xmltv::default()
        },
    }
}

/// How much of a document's start [`read_xmltv_where`] looks at for its
/// encoding: more than any XML declaration a guide writes.
const HEAD_BYTES: usize = 1024;

/// The text encoding of a document whose first bytes are `head`, by
/// `XmlReader`'s rules: a byte-order mark, the UTF-16 shape of `<?`, then the
/// XML declaration's `encoding`, else UTF-8.
///
/// # Errors
///
/// The declaration names an encoding that does not exist, or a UTF-16 one in
/// a document that is not UTF-16 (`XmlReader`'s "There is no Unicode byte
/// order mark. Cannot switch to Unicode.").
fn document_encoding(head: &[u8]) -> Result<&'static Encoding, String> {
    if let Some((encoding, _)) = Encoding::for_bom(head) {
        return Ok(encoding);
    }
    if head.starts_with(&[b'<', 0, b'?', 0]) {
        return Ok(encoding_rs::UTF_16LE);
    }
    if head.starts_with(&[0, b'<', 0, b'?']) {
        return Ok(encoding_rs::UTF_16BE);
    }
    let Some(label) = declared_encoding(head) else {
        return Ok(encoding_rs::UTF_8);
    };
    match Encoding::for_label(label) {
        Some(encoding)
            if encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE =>
        {
            Err(format!(
                "the XMLTV document declares {} but is not UTF-16",
                String::from_utf8_lossy(label)
            ))
        }
        Some(encoding) if encoding != encoding_rs::REPLACEMENT => Ok(encoding),
        _ => Err(format!(
            "the XMLTV document declares an unsupported encoding: {}",
            String::from_utf8_lossy(label)
        )),
    }
}

/// The `encoding` pseudo-attribute of the XML declaration `head` starts with,
/// when it starts with one that has it.
fn declared_encoding(head: &[u8]) -> Option<&[u8]> {
    const NAME: &[u8] = b"encoding";
    let rest = head.strip_prefix(b"<?xml")?;
    if !rest.first().is_some_and(u8::is_ascii_whitespace) {
        return None;
    }
    let declaration = &rest[..rest.windows(2).position(|w| w == b"?>")?];
    let at = declaration.windows(NAME.len()).position(|w| w == NAME)?;
    let value = declaration[at + NAME.len()..].trim_ascii_start();
    let value = value.strip_prefix(b"=")?.trim_ascii_start();
    let quote = *value.first().filter(|q| matches!(q, b'"' | b'\''))?;
    let value = &value[1..];
    Some(&value[..value.iter().position(|&b| b == quote)?])
}

/// How much decoded text [`Decoded`] holds at once.
const DECODED_BUFFER: usize = 16 * 1024;

/// A byte source decoded to UTF-8 as it is read, for a document in another
/// encoding. A sequence that is not valid in it is an `InvalidData` error.
struct Decoded<R> {
    /// The encoded bytes.
    source: R,
    /// The decoder; it removes a byte-order mark.
    decoder: encoding_rs::Decoder,
    /// Decoded text; `out[at..len]` is not read yet.
    out: Box<[u8]>,
    /// Where the unread text starts.
    at: usize,
    /// Where the decoded text ends.
    len: usize,
    /// Whether the source ended and the decoder was flushed.
    finished: bool,
}

impl<R> Decoded<R> {
    fn new(source: R, encoding: &'static Encoding) -> Self {
        Self {
            source,
            decoder: encoding.new_decoder_with_bom_removal(),
            out: vec![0; DECODED_BUFFER].into_boxed_slice(),
            at: 0,
            len: 0,
            finished: false,
        }
    }
}

impl<R: BufRead> std::io::Read for Decoded<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let available = self.fill_buf()?;
        let read = available.len().min(buf.len());
        buf[..read].copy_from_slice(&available[..read]);
        self.consume(read);
        Ok(read)
    }
}

impl<R: BufRead> BufRead for Decoded<R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        while self.at == self.len && !self.finished {
            let input = self.source.fill_buf()?;
            let last = input.is_empty();
            let (result, read, written) =
                self.decoder
                    .decode_to_utf8_without_replacement(input, &mut self.out, last);
            self.source.consume(read);
            self.at = 0;
            self.len = written;
            match result {
                DecoderResult::Malformed(..) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "a byte sequence is not valid {}",
                            self.decoder.encoding().name()
                        ),
                    ));
                }
                DecoderResult::InputEmpty if last => self.finished = true,
                DecoderResult::InputEmpty | DecoderResult::OutputFull => {}
            }
        }
        Ok(&self.out[self.at..self.len])
    }

    fn consume(&mut self, amount: usize) {
        self.at = (self.at + amount).min(self.len);
    }
}

/// Reads the XMLTV document `reader` yields, keeping the programmes `keep`
/// accepts — see [`parse_xmltv_where`].
#[allow(clippy::too_many_lines)] // one pull-parser state machine
fn read_events<R: BufRead>(
    mut reader: Reader<R>,
    mut keep: impl FnMut(&XmltvProgramme) -> bool,
) -> Xmltv {
    let config = reader.config_mut();
    config.check_end_names = false;
    config.trim_text(false);

    // The bytes of the event being read.
    let mut buf = Vec::new();
    // Whether anything but white space has been read: a document with none is
    // empty rather than rootless.
    let mut saw_content = false;
    let mut out = Xmltv::default();
    // Scratch buffer for the text content of the element currently being read.
    let mut text = String::new();
    // The `system` of the `<episode-num>` being read, if any.
    let mut episode_system: Option<String> = None;
    // The number of elements open around the current one. An unbalanced tag
    // (`check_end_names` is off) skews it until the programme or channel it
    // sits in ends, losing that one's later children. A tag still open at EOF
    // sets `error`, so the refresh keeps the stored guide, as upstream's
    // `XmlReader` rejecting the document does. Best-effort only: a stray end
    // tag can cancel an unclosed one.
    let mut depth = 0_usize;
    let mut channel: Option<OpenChannel> = None;
    let mut programme: Option<OpenProgramme> = None;
    let mut root_seen = false;

    loop {
        buf.clear();
        let event = reader.read_event_into(&mut buf);
        if !root_seen && let Ok(Event::Start(e) | Event::Empty(e)) = &event {
            root_seen = true;
            // Stricter than .NET's `XmlReader`, which rejects an HTML error
            // page only when it is not well-formed: with `check_end_names` off
            // this reader cannot tell, so any root but `<tv>` keeps the stored
            // guide.
            if e.local_name().as_ref() != "tv" {
                out.error = Some(format!(
                    "the document's root element is <{}>, not <tv>",
                    e.local_name().as_ref()
                ));
                break;
            }
        }
        match event {
            Ok(Event::Start(e)) => {
                match e.local_name().as_ref().as_bytes() {
                    b"channel" => {
                        channel = Some(OpenChannel::new(
                            attr(&e, b"id").unwrap_or_default(),
                            depth + 1,
                        ));
                    }
                    b"programme" => {
                        programme = Some(OpenProgramme::new(read_programme(&e), depth + 1));
                    }
                    name => apply_start(
                        name,
                        &e,
                        depth,
                        &mut episode_system,
                        channel.as_mut(),
                        programme.as_mut(),
                    ),
                }
                depth += 1;
                text.clear();
            }
            Ok(Event::Empty(e)) => {
                // A self-closing element is its own start and end, with no text.
                let name = e.local_name();
                let name = name.as_ref().as_bytes();
                apply_start(
                    name,
                    &e,
                    depth,
                    &mut episode_system,
                    channel.as_mut(),
                    programme.as_mut(),
                );
                apply_end(
                    name,
                    "",
                    depth,
                    episode_system.take().as_deref(),
                    channel.as_mut(),
                    programme.as_mut(),
                );
            }
            Ok(Event::Text(e)) => {
                saw_content |= !e.trim().is_empty();
                text.push_str(e.as_ref());
            }
            Ok(Event::GeneralRef(e)) => {
                if let Ok(value) = quick_xml::escape::unescape(&format!("&{};", e.as_ref())) {
                    text.push_str(&value);
                }
            }
            Ok(Event::CData(e)) => {
                text.push_str(e.as_ref());
            }
            Ok(Event::End(e)) => {
                depth = depth.saturating_sub(1);
                let name = e.local_name();
                let name = name.as_ref().as_bytes();
                apply_end(
                    name,
                    &text,
                    depth,
                    episode_system.take().as_deref(),
                    channel.as_mut(),
                    programme.as_mut(),
                );
                match name {
                    b"channel" => {
                        if let Some(c) = channel.take() {
                            out.channels.push(c.channel);
                        }
                    }
                    b"programme" => {
                        if let Some(p) = programme.take()
                            && keep(&p.programme)
                        {
                            out.programmes.push(p.programme);
                        }
                    }
                    _ => {}
                }
                text.clear();
            }
            Ok(Event::Eof) => {
                if depth > 0 {
                    out.error = Some("the XMLTV document ends inside an open element".to_owned());
                } else if !root_seen && !saw_content {
                    // `XmlTvListingsProvider.UnzipIfNeededAndCopy`: "Downloaded
                    // XMLTV file is empty".
                    out.error = Some("the XMLTV document is empty".to_owned());
                } else if !root_seen {
                    out.error = Some("the XMLTV document has no root element".to_owned());
                }
                break;
            }
            Err(e) => {
                out.error = Some(format!("the XMLTV document is not well-formed: {e}"));
                break;
            }
            Ok(_) => saw_content = true,
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
    a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .map_or_else(
            |_| a.value.as_ref().to_owned(),
            std::borrow::Cow::into_owned,
        )
}

/// Reads a `<programme>` start tag's `channel`/`start`/`stop` attributes in a
/// single pass over the attribute list.
fn read_programme(e: &BytesStart<'_>) -> XmltvProgramme {
    let mut p = XmltvProgramme::default();
    for a in e.attributes().flatten() {
        let value = unescaped_or_raw(&a);
        match a.key.local_name().as_ref().as_bytes() {
            b"channel" => p.channel_id = value,
            b"start" => p.start = parse_xmltv_time(&value),
            b"stop" => p.stop = parse_xmltv_time(&value),
            _ => {}
        }
    }
    p
}

/// A `<channel>` being read.
struct OpenChannel {
    channel: XmltvChannel,
    /// The depth of the channel's own children.
    child_depth: usize,
    /// Whether its first populated `<icon>` has been read.
    icon_read: bool,
}

impl OpenChannel {
    fn new(id: String, child_depth: usize) -> Self {
        Self {
            channel: XmltvChannel {
                id,
                ..XmltvChannel::default()
            },
            child_depth,
            icon_read: false,
        }
    }
}

/// A `<programme>` being read.
#[allow(clippy::struct_excessive_bools)] // one first-seen flag per kind of child
struct OpenProgramme {
    programme: XmltvProgramme,
    /// The depth of the programme's own children.
    child_depth: usize,
    /// Whether its first populated `<icon>` has been read.
    icon_read: bool,
    /// Whether its first kept `<rating>` has been read.
    rating_read: bool,
    /// The `<rating>` being read, if one is open.
    rating: Option<OpenRating>,
    /// The type of the `<image>` being read, if one is open.
    image_type: Option<i64>,
    /// Whether its first `still` `<image>` has been read.
    still_read: bool,
    /// Whether its first `backdrop` `<image>` has been read.
    backdrop_read: bool,
}

impl OpenProgramme {
    fn new(programme: XmltvProgramme, child_depth: usize) -> Self {
        Self {
            programme,
            child_depth,
            icon_read: false,
            rating_read: false,
            rating: None,
            image_type: None,
            still_read: false,
            backdrop_read: false,
        }
    }
}

/// A `<rating>` being read, as `XmlTvReader.ProcessRating` builds it.
#[derive(Default)]
struct OpenRating {
    /// Its last `<value>`.
    value: Option<String>,
    /// Set by an empty `<value>`: `ProcessRating` then returns without adding
    /// the rating, so it is never the programme's first.
    dropped: bool,
}

/// An `<icon>` that `XmlTvReader.ProcessIconNode` keeps: one with a `src`, a
/// `width` or a `height`.
struct PopulatedIcon {
    /// Its `src`, when not empty.
    src: Option<String>,
}

/// The `<icon>` `e` as `ProcessIconNode` reads it, or `None` when it is no icon
/// at all.
fn populated_icon(e: &BytesStart<'_>) -> Option<PopulatedIcon> {
    let src = attr(e, b"src").filter(|s| !s.is_empty());
    let sized = |name: &[u8]| attr(e, name).is_some_and(|v| v.trim().parse::<i32>().is_ok());
    (src.is_some() || sized(b"width") || sized(b"height")).then_some(PopulatedIcon { src })
}

/// `Jellyfin.XmlTv` `ImageType.Backdrop`.
const IMAGE_TYPE_BACKDROP: i64 = 1;
/// `Jellyfin.XmlTv` `ImageType.Still`.
const IMAGE_TYPE_STILL: i64 = 2;

/// The `type` of an `<image>` as `XmlTvReader.ProcessImageNode` reads it:
/// `Enum.TryParse<ImageType>(type, ignoreCase: true)`, which takes a member
/// name in any case or an integer, around white space. Anything else leaves
/// the default, `Poster` (`0`). A comma-separated list of names, which .NET
/// would OR together, is not a real guide's spelling and reads as `Poster`.
fn image_type(value: Option<&str>) -> i64 {
    const NAMES: [&str; 5] = ["poster", "backdrop", "still", "person", "character"];
    let Some(value) = value.map(str::trim) else {
        return 0;
    };
    NAMES
        .iter()
        .zip(0..)
        .find(|(name, _)| value.eq_ignore_ascii_case(name))
        .map(|(_, n)| n)
        .or_else(|| value.parse().ok())
        .unwrap_or(0)
}

/// A programme child's name as `GetProgramme` matches it
/// (`xmlProg.Name.ToLower(...)`), borrowed when it is already lower-case.
fn programme_child_name(name: &[u8]) -> Cow<'_, [u8]> {
    if name.iter().any(u8::is_ascii_uppercase) {
        Cow::Owned(name.to_ascii_lowercase())
    } else {
        Cow::Borrowed(name)
    }
}

/// Applies the start of an element, or a self-closing one, to the channel or
/// programme being read. `depth` is the number of elements open around it.
///
/// Only the channel's or programme's own children count, as in
/// `XmlTvReader.GetChannel`/`GetProgramme`, which hand any other element to its
/// parent's handler or skip it.
fn apply_start(
    name: &[u8],
    e: &BytesStart<'_>,
    depth: usize,
    episode_system: &mut Option<String>,
    channel: Option<&mut OpenChannel>,
    programme: Option<&mut OpenProgramme>,
) {
    if let Some(p) = programme {
        if depth != p.child_depth {
            return;
        }
        // `GetProgramme` matches on `xmlProg.Name.ToLower(...)`.
        match programme_child_name(name).as_ref() {
            b"episode-num" => *episode_system = attr(e, b"system"),
            // `ProcessLive`/`ProcessNew`/`ProcessPremiereNode`/
            // `ProcessPreviouslyShown`: the element is the flag, whatever its
            // text. TODO(gl-683): `ProcessPreviouslyShown` also reads a `start`
            // attribute and leaves an airing whose original-air date equals its
            // own start a first showing; here every `<previously-shown>` is a
            // repeat.
            b"live" => p.programme.is_live = true,
            b"new" => p.programme.is_new = true,
            b"premiere" => p.programme.is_premiere = true,
            b"previously-shown" => p.programme.is_previously_shown = true,
            b"icon" => {
                if !p.icon_read
                    && let Some(icon) = populated_icon(e)
                {
                    p.icon_read = true;
                    p.programme.icon = icon.src;
                }
            }
            b"rating" => p.rating = Some(OpenRating::default()),
            b"image" => p.image_type = Some(image_type(attr(e, b"type").as_deref())),
            _ => {}
        }
    } else if let Some(c) = channel
        // `GetChannel` matches its children case-sensitively.
        && depth == c.child_depth
        && name == b"icon"
        && !c.icon_read
        && let Some(icon) = populated_icon(e)
    {
        c.icon_read = true;
        c.channel.icon = icon.src;
    }
}

/// Jellyfin.XmlTv `SetChannelNumber`: a display-name with `-` and `_` turned
/// into `.` is the channel number when it parses as a `double` under
/// `NumberStyles.Any` and the invariant culture.
fn channel_number(display_name: &str) -> Option<String> {
    let value = display_name.replace(['-', '_'], ".");
    parses_as_invariant_double(&value).then_some(value)
}

/// Whether .NET's `double.TryParse(s, NumberStyles.Any,
/// CultureInfo.InvariantCulture, out _)` accepts `s`. That style allows
/// leading and trailing white space, a leading or trailing sign, a pair of
/// parentheses, the currency symbol `¤`, `,` group separators, a decimal
/// point and an exponent. The invariant `NaN` and infinity symbols parse too,
/// ignoring case.
///
/// This is an approximation for channel display-names. It is stricter than
/// .NET about white space inside the affixes (`( 5)`, `¤ 5`), and looser about
/// `NaN`/`Infinity` wrapped in them (`(NaN)`). No real guide writes either.
fn parses_as_invariant_double(s: &str) -> bool {
    let white = |c: char| matches!(c, '\u{9}'..='\u{d}' | ' ');
    let mut s = s.trim_matches(white);
    // Parentheses are the sign, so a pair rules out `+`/`-`. A sign and the
    // currency symbol may each appear once, at either end, in either order.
    let mut signed = false;
    if let Some(inner) = s.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        s = inner;
        signed = true;
    }
    let mut currency = false;
    loop {
        let sign = (!signed)
            .then(|| {
                s.strip_prefix(['+', '-'])
                    .or_else(|| s.strip_suffix(['+', '-']))
            })
            .flatten();
        if let Some(rest) = sign {
            s = rest;
            signed = true;
            continue;
        }
        let symbol = (!currency)
            .then(|| s.strip_prefix('¤').or_else(|| s.strip_suffix('¤')))
            .flatten();
        if let Some(rest) = symbol {
            s = rest;
            currency = true;
            continue;
        }
        break;
    }
    if s.eq_ignore_ascii_case("nan") || s.eq_ignore_ascii_case("infinity") {
        return true;
    }
    let (mantissa, exponent) = match s.find(['e', 'E']) {
        Some(at) => (&s[..at], Some(&s[at + 1..])),
        None => (s, None),
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    let integer_ok =
        !integer.starts_with(',') && integer.chars().all(|c| c.is_ascii_digit() || c == ',');
    let has_digit = integer
        .chars()
        .chain(fraction.chars())
        .any(|c| c.is_ascii_digit());
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    integer_ok && digits(fraction) && has_digit && exponent_ok
}

/// Applies the end of an element, or a self-closing one, to the channel or
/// programme being read, using the accumulated `text`. `depth` is the number
/// of elements open around it.
fn apply_end(
    name: &[u8],
    text: &str,
    depth: usize,
    episode_system: Option<&str>,
    channel: Option<&mut OpenChannel>,
    programme: Option<&mut OpenProgramme>,
) {
    let text = text.trim();
    if let Some(c) = channel {
        let c = &mut c.channel;
        if name == b"display-name" {
            if c.display_name.is_empty() && !text.is_empty() {
                text.clone_into(&mut c.display_name);
            }
            if let Some(number) = channel_number(text) {
                c.number = Some(number);
            }
        }
        return;
    }
    let Some(open) = programme else { return };
    if depth == open.child_depth + 1 {
        // `ProcessRating` reads a `<value>` (matched ignoring case) of the open
        // rating: an empty one drops the rating, a later one replaces an
        // earlier one. Two upstream quirks are not mirrored: a whitespace-only
        // value counts as empty here (text is trimmed), and upstream's
        // `Skip()` after reading a value swallows a second `<value>` written
        // with no whitespace before it (the DTD allows one value).
        if let Some(rating) = open.rating.as_mut()
            && !rating.dropped
            && name.eq_ignore_ascii_case(b"value")
        {
            if text.is_empty() {
                rating.dropped = true;
            } else {
                rating.value = Some(text.to_owned());
            }
        }
        return;
    }
    if depth != open.child_depth {
        return;
    }
    let p = &mut open.programme;
    match programme_child_name(name).as_ref() {
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
        // `XmlTvListingsProvider` takes `Ratings.FirstOrDefault()?.Value`: the
        // first rating `ProcessRating` kept decides, with or without a value.
        // `<rating/>` reads as `<rating></rating>`.
        b"rating" => {
            if let Some(rating) = open.rating.take()
                && !rating.dropped
                && !open.rating_read
            {
                open.rating_read = true;
                p.rating = rating.value;
            }
        }
        // `ProcessImageNode` keeps an image with a type or a path (its size,
        // orientation and system are not read here), and `GetProgramInfo`
        // takes the first of each type, so a `still` with no path still hides
        // a later one.
        b"image" => {
            let path = (!text.is_empty()).then(|| text.to_owned());
            match open.image_type.take() {
                Some(IMAGE_TYPE_STILL) if !open.still_read => {
                    open.still_read = true;
                    p.still_image = path;
                }
                Some(IMAGE_TYPE_BACKDROP) if !open.backdrop_read => {
                    open.backdrop_read = true;
                    p.backdrop_image = path;
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Parses an XMLTV timestamp: `YYYYMMDDHHMMSS` optionally followed by a
/// ` ±HHMM` offset. A missing offset is treated as UTC.
///
/// The XMLTV DTD lets a feed give "some initial substring" of the time
/// (`200007281733 +0100`). A whole-unit prefix (year, then month, day, hour,
/// minute) is completed from `20000101000000`, so each missing unit takes its
/// first value; a prefix that cuts a unit in half is no time. Only numeric
/// offsets are read: a named zone (`BST`) makes the whole time unreadable.
///
/// The declared offset is preserved rather than normalised away — see
/// [`XmltvProgramme::start`] for why the programme's item GUID depends on it.
#[must_use]
pub fn parse_xmltv_time(raw: &str) -> Option<DateTime<FixedOffset>> {
    const COMPLETION: &str = "20000101000000";
    let raw = raw.trim();
    let (datetime, offset) = match raw.split_once(' ') {
        Some((dt, off)) => (dt, Some(off.trim())),
        None => (raw, None),
    };
    let digits = datetime.bytes().take_while(u8::is_ascii_digit).count();
    let given = if digits >= 14 {
        14
    } else if digits >= 4 && digits % 2 == 0 && digits == datetime.len() {
        digits
    } else {
        return None;
    };
    let full = format!("{}{}", &datetime[..given], &COMPLETION[given..]);
    let naive = chrono::NaiveDateTime::parse_from_str(&full, "%Y%m%d%H%M%S").ok()?;
    let fixed = match offset.filter(|o| !o.is_empty()) {
        Some(o) => parse_offset(o)?,
        None => FixedOffset::east_opt(0)?,
    };
    fixed.from_local_datetime(&naive).single()
}

/// Parses a `±HHMM` numeric timezone offset into a [`FixedOffset`].
fn parse_offset(o: &str) -> Option<FixedOffset> {
    let bytes = o.as_bytes();
    // Every byte after the sign must be an ASCII digit before the string is
    // sliced: a multi-byte character there would split at a non-boundary.
    if bytes.len() != 5
        || (bytes[0] != b'+' && bytes[0] != b'-')
        || !bytes[1..].iter().all(u8::is_ascii_digit)
    {
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
        if a.key.local_name().as_ref().as_bytes() == name {
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

    /// A guide whose `<channel>` elements come after its programmes (XMLTV
    /// does not fix the order), with airings on two channels.
    const CHANNELS_LAST: &str = r#"<tv>
  <programme start="20260725060000 +0000" channel="a"><title>A1</title></programme>
  <programme start="20260725060000 +0000" channel="b"><title>B1</title></programme>
  <programme start="20260725070000 +0000" channel="a"><title>A2</title><category>News</category></programme>
  <channel id="a"><display-name>Channel A</display-name></channel>
  <channel id="b"><display-name>Channel B</display-name></channel>
</tv>"#;

    /// `keep` sees every programme once, whole, in document order, and only the
    /// ones it accepts are stored; the channels are read whatever it decides.
    #[test]
    fn parse_where_keeps_only_the_programmes_it_accepts() {
        let mut seen = Vec::new();
        let g = parse_xmltv_where(CHANNELS_LAST, |p| {
            seen.push((p.title.clone(), p.categories.clone()));
            p.channel_id == "a"
        });
        assert_eq!(
            seen,
            vec![
                ("A1".to_owned(), Vec::<String>::new()),
                ("B1".to_owned(), Vec::new()),
                ("A2".to_owned(), vec!["News".to_owned()]),
            ]
        );
        let titles: Vec<&str> = g.programmes.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, ["A1", "A2"]);
        let ids: Vec<&str> = g.channels.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(g.error, None);
    }

    /// Rejecting every programme reads the channel list and the document's
    /// readability exactly as the full parse does.
    #[test]
    fn parse_where_rejecting_everything_still_reports_channels_and_errors() {
        let all = parse_xmltv(SAMPLE);
        let none = parse_xmltv_where(SAMPLE, |_| false);
        assert!(none.programmes.is_empty());
        assert_eq!(none.channels, all.channels);
        assert_eq!(none.error, None);

        let truncated = &SAMPLE[..SAMPLE.len() - "</tv>".len()];
        assert_eq!(
            parse_xmltv_where(truncated, |_| false).error,
            parse_xmltv(truncated).error
        );
        assert!(parse_xmltv(truncated).error.is_some());
    }

    #[test]
    fn parses_utc_and_offset_times() {
        let g = parse_xmltv(SAMPLE);
        let p = &g.programmes[0];
        // 06:00 +0000 == 06:00 UTC.
        assert_eq!(p.start.unwrap().to_rfc3339(), "2026-07-25T06:00:00+00:00");
        // 07:00 +0100 is the same instant, and the declared offset is KEPT
        // rather than normalised away: upstream's `XmlTvProgram.StartDate` is a
        // `DateTimeOffset` and its `{0:O}` rendering — offset included — is
        // hashed into the programme's item GUID.
        assert_eq!(p.stop.unwrap().to_rfc3339(), "2026-07-25T07:00:00+01:00");
        assert_eq!(
            p.stop.unwrap().with_timezone(&chrono::Utc).to_rfc3339(),
            "2026-07-25T06:00:00+00:00"
        );
        assert_eq!(p.stop.unwrap().offset().local_minus_utc(), 3600);
    }

    /// The `{0:O}` rendering the programme id is built from keeps the source
    /// offset, so an id derived from a `+0100` guide differs from one derived
    /// from the same instant written as `+0000`. That is upstream's behaviour,
    /// and normalising here would silently rewrite every such id.
    #[test]
    fn offset_survives_into_the_dotnet_round_trip_form() {
        let plus_one = parse_xmltv_time("20260725070000 +0100").expect("parse");
        let utc = parse_xmltv_time("20260725060000 +0000").expect("parse");
        assert_eq!(plus_one, utc);
        let render = |d: DateTime<FixedOffset>| {
            format!(
                "{}.{:07}{}",
                d.format("%Y-%m-%dT%H:%M:%S"),
                d.timestamp_subsec_nanos() / 100,
                d.offset()
            )
        };
        assert_eq!(render(plus_one), "2026-07-25T07:00:00.0000000+01:00");
        assert_eq!(render(utc), "2026-07-25T06:00:00.0000000+00:00");
    }

    #[test]
    fn time_parser_handles_missing_offset_as_utc() {
        let t = parse_xmltv_time("20260101120000").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-01-01T12:00:00+00:00");
        assert!(parse_xmltv_time("garbage").is_none());
    }

    /// The XMLTV DTD: a time is `YYYYMMDDhhmmss`, of which "some initial
    /// substring" may be given, the missing units taking their first value. A
    /// unit is never cut in half, and an offset is `+hhmm`/`-hhmm`; anything
    /// else is no time, never a panic.
    #[rstest::rstest]
    #[case("200007281733 +0100", Some("2000-07-28T17:33:00+01:00"))]
    #[case("2000072817", Some("2000-07-28T17:00:00+00:00"))]
    #[case("20000728", Some("2000-07-28T00:00:00+00:00"))]
    #[case("200007", Some("2000-07-01T00:00:00+00:00"))]
    #[case("2026", Some("2026-01-01T00:00:00+00:00"))]
    #[case("20000728173", None)]
    #[case("200", None)]
    #[case("20260725060000 -0530", Some("2026-07-25T06:00:00-05:30"))]
    #[case("20260725060000 +0é0", None)]
    #[case("20260725060000 +01é", None)]
    #[case("20260725060000 +01x0", None)]
    // Unchanged: 14 digits with an attached tail read the 14, as UTC.
    #[case("20260725060000+0100", Some("2026-07-25T06:00:00+00:00"))]
    #[case("2026072506000", None)]
    #[case("202607281733xyz", None)]
    #[case("200007281733 BST", None)]
    fn time_parser_follows_the_dtd(#[case] raw: &str, #[case] expected: Option<&str>) {
        assert_eq!(
            parse_xmltv_time(raw).map(|t| t.to_rfc3339()).as_deref(),
            expected,
            "{raw}"
        );
    }

    /// Jellyfin.XmlTv `SetChannelNumber`: `-` and `_` become `.`, and the
    /// result is the number when `double.TryParse(NumberStyles.Any,
    /// InvariantCulture)` accepts it.
    #[rstest::rstest]
    #[case("4.1", Some("4.1"))]
    #[case("4-1", Some("4.1"))]
    #[case("4_1", Some("4.1"))]
    #[case("704", Some("704"))]
    #[case("1,234", Some("1,234"))]
    #[case("(5)", Some("(5)"))]
    #[case("+5", Some("+5"))]
    #[case("5e3", Some("5e3"))]
    #[case("¤5", Some("¤5"))]
    #[case("4.1 WNBC", None)]
    #[case("WNBC", None)]
    #[case("12-1-1", None)]
    #[case(".", None)]
    #[case("5e", None)]
    #[case(".5", Some(".5"))]
    #[case("5.", Some("5."))]
    #[case("5+", Some("5+"))]
    #[case("¤+5", Some("¤+5"))]
    #[case("NaN", Some("NaN"))]
    #[case("infinity", Some("infinity"))]
    #[case("1.2,3", None)]
    #[case("(+5)", None)]
    #[case("+ 5", None)]
    #[case("", None)]
    fn display_name_channel_numbers(#[case] display_name: &str, #[case] number: Option<&str>) {
        assert_eq!(super::channel_number(display_name).as_deref(), number);
    }

    #[test]
    fn the_last_numeric_display_name_numbers_the_channel() {
        let g = parse_xmltv(
            r#"<tv>
  <channel id="I4.1.12345.zap2it.com">
    <display-name>4.1 WNBC</display-name><display-name>4-1</display-name><display-name>WNBC</display-name>
  </channel>
  <channel id="news.example"><display-name>News</display-name></channel>
</tv>"#,
        );
        assert_eq!(g.channels[0].display_name, "4.1 WNBC");
        assert_eq!(g.channels[0].number.as_deref(), Some("4.1"));
        assert_eq!(g.channels[0].listings_number(), "4.1");
        // No numeric display-name: the provider reports the id as the number.
        assert_eq!(g.channels[1].number, None);
        assert_eq!(g.channels[1].listings_number(), "news.example");
    }

    #[test]
    fn empty_document_is_empty_guide() {
        let g = parse_xmltv("");
        assert!(g.channels.is_empty());
        assert!(g.programmes.is_empty());
        assert!(
            g.error.is_some(),
            "an empty body is an error, not an empty guide"
        );
    }

    /// Which documents a guide refresh refuses (`error` set) and which it
    /// takes as authoritative, even when they carry no programmes.
    #[rstest::rstest]
    #[case::whitespace(" \n\t", true)]
    #[case::text_only("Service Unavailable", true)]
    #[case::bare_ampersand_in_text(
        "<tv><programme channel=\"a\"><title>A & B</title></programme></tv>",
        true
    )]
    #[case::unclosed_at_eof("<tv><channel id=\"a\">", true)]
    #[case::html_root("<!DOCTYPE html><html><body>503</body></html>", true)]
    #[case::empty_tv("<tv></tv>", false)]
    #[case::self_closing_tv("<tv/>", false)]
    #[case::prolog(
        "<?xml version=\"1.0\"?><!DOCTYPE tv SYSTEM \"xmltv.dtd\"><!-- grabber --><tv></tv>",
        false
    )]
    #[case::prefixed_tv("<x:tv xmlns:x=\"urn:x\"></x:tv>", false)]
    fn unreadable_documents_report_an_error(#[case] xml: &str, #[case] unreadable: bool) {
        assert_eq!(parse_xmltv(xml).error.is_some(), unreadable, "{xml}");
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

    #[test]
    fn text_entities_keep_word_boundaries_and_cdata() {
        let guide = parse_xmltv(
            r#"<tv><channel id="c"><display-name>News &amp; Sport &#33;</display-name></channel>
             <programme channel="c"><title>Fish &amp; Chips <![CDATA[<live>]]></title></programme></tv>"#,
        );
        assert_eq!(guide.channels[0].display_name, "News & Sport !");
        assert_eq!(guide.programmes[0].title, "Fish & Chips <live>");
    }
}

/// Programme and channel children as `Jellyfin.XmlTv` 10.12.0-pre1 reads them,
/// and as v12.2 `XmlTvListingsProvider` uses the result
/// (`Ratings.FirstOrDefault()?.Value`, `Icons.FirstOrDefault()?.Source`).
#[cfg(test)]
mod programme_child_tests {
    use super::{XmltvProgramme, parse_xmltv};

    /// The single programme of a guide whose `<programme>` holds `children`.
    fn programme(children: &str) -> XmltvProgramme {
        let guide = parse_xmltv(&format!(
            "<tv><programme start=\"20260725060000 +0000\" channel=\"x\"><title>T</title>{children}</programme></tv>"
        ));
        guide.programmes.into_iter().next().expect("one programme")
    }

    #[test]
    fn flags_count_in_either_form() {
        // `ProcessLive`/`ProcessNew`: the element's presence is the signal.
        let p = programme("<live/><new/><premiere/><previously-shown/>");
        assert!(p.is_live && p.is_new && p.is_premiere && p.is_previously_shown);
        // `ProcessPremiereNode` reads the text but keeps a premiere either way.
        let p = programme(
            "<live>yes</live><new></new><premiere lang=\"en\">First showing</premiere>\
             <previously-shown></previously-shown>",
        );
        assert!(p.is_live && p.is_new && p.is_premiere && p.is_previously_shown);
        assert_eq!(p.title, "T", "flag text never leaks into a field");
        let p = programme("");
        assert!(!(p.is_live || p.is_new || p.is_premiere || p.is_previously_shown));
        // A flag outside a programme flags nothing.
        let guide = parse_xmltv(
            "<tv><channel id=\"c\"><display-name>C</display-name><live/><new></new></channel>\
             <programme start=\"20260725060000 +0000\" channel=\"c\"><title>T</title></programme></tv>",
        );
        assert!(!(guide.programmes[0].is_live || guide.programmes[0].is_new));
    }

    #[test]
    fn programme_children_match_case_insensitively() {
        // `GetProgramme` switches on `xmlProg.Name.ToLower(...)`.
        let guide = parse_xmltv(
            "<tv><programme start=\"20260725060000 +0000\" channel=\"x\"><Title>Upper</Title>\
             <DESC>Synopsis</DESC><Category>News</Category><LIVE/><Icon src=\"http://x/p.png\"/>\
             <RATING><VALUE>TV-G</VALUE></RATING></programme></tv>",
        );
        let p = &guide.programmes[0];
        assert_eq!(p.title, "Upper");
        assert_eq!(p.desc.as_deref(), Some("Synopsis"));
        assert_eq!(p.categories, vec!["News"]);
        assert!(p.is_live);
        assert_eq!(p.icon.as_deref(), Some("http://x/p.png"));
        assert_eq!(p.rating.as_deref(), Some("TV-G"));
    }

    #[test]
    fn only_the_programmes_own_children_are_read() {
        // `GetProgramme` hands each child element to its handler or skips it
        // whole, so a grandchild with a programme child's name is never read.
        let p = programme(
            "<credits><desc>nested</desc><sub-title>nested</sub-title><category>Movie</category>\
             <date>1999</date><episode-num system=\"xmltv_ns\">0.5.</episode-num><live/></credits>\
             <review><title>nested</title><icon src=\"http://x/r.png\"/></review>",
        );
        assert_eq!((p.desc, p.sub_title), (None, None));
        assert!(p.categories.is_empty());
        assert_eq!((p.year, p.episode_num), (None, None));
        assert!(!p.is_live);
        assert_eq!(p.icon, None);
        // A nested title ahead of the programme's own is not its title.
        let guide = parse_xmltv(
            "<tv><programme start=\"20260725060000 +0000\" channel=\"x\">\
             <review><title>nested</title></review><title>Own</title></programme></tv>",
        );
        assert_eq!(guide.programmes[0].title, "Own");
    }

    #[test]
    fn the_rating_comes_from_rating_alone() {
        // A star rating is a community rating, never the OfficialRating.
        let p = programme("<star-rating system=\"IMDb\"><value>7.2/10</value></star-rating>");
        assert_eq!(p.rating, None);
        let p = programme(
            "<star-rating><value>4/5</value></star-rating><rating system=\"MPAA\"><value>PG</value></rating>",
        );
        assert_eq!(p.rating.as_deref(), Some("PG"));
        // A `<value>` outside any rating is nobody's rating.
        assert_eq!(
            programme("<review><value>bad</value></review>").rating,
            None
        );
    }

    #[test]
    fn the_first_rating_kept_decides() {
        // `ProcessRating` drops a rating whose `<value>` is empty, so the next
        // one is first.
        let p = programme("<rating><value></value></rating><rating><value>PG</value></rating>");
        assert_eq!(p.rating.as_deref(), Some("PG"));
        let p = programme("<rating><value/></rating><rating><value>PG</value></rating>");
        assert_eq!(p.rating.as_deref(), Some("PG"));
        // A rating with no value is kept, so it is first and has no value.
        let p = programme("<rating system=\"x\"></rating><rating><value>PG</value></rating>");
        assert_eq!(p.rating, None);
        // `<rating/>` reads as `<rating></rating>`.
        let p = programme("<rating system=\"x\"/><rating><value>PG</value></rating>");
        assert_eq!(p.rating, None);
        // Two kept ratings: the first wins.
        let p =
            programme("<rating><value>TV-14</value></rating><rating><value>12</value></rating>");
        assert_eq!(p.rating.as_deref(), Some("TV-14"));
        // Within one (pretty-printed) rating a later value replaces an earlier
        // one.
        let p = programme("<rating>\n  <value>A</value>\n  <value>B</value>\n</rating>");
        assert_eq!(p.rating.as_deref(), Some("B"));
    }

    #[test]
    fn a_programmes_still_and_backdrop_are_its_first_own_images_of_each_type() {
        let p = programme(
            "<image type=\"poster\">http://x/poster.jpg</image>\
             <image type=\"Still\">http://x/still.jpg</image>\
             <image type=\" BACKDROP \">http://x/back.jpg</image>\
             <image type=\"still\">http://x/later.jpg</image>\
             <credits><actor><image type=\"backdrop\">http://x/actor.jpg</image></actor></credits>",
        );
        assert_eq!(p.still_image.as_deref(), Some("http://x/still.jpg"));
        assert_eq!(p.backdrop_image.as_deref(), Some("http://x/back.jpg"));
        // An untyped or unknown type is a poster; an integer is its member.
        let p = programme(
            "<image>http://x/a.jpg</image><image type=\"wide\">http://x/b.jpg</image><image type=\"2\">http://x/c.jpg</image>",
        );
        assert_eq!(p.still_image.as_deref(), Some("http://x/c.jpg"));
        assert_eq!(p.backdrop_image, None);
        // The first still decides even with no path.
        let p = programme("<image type=\"still\"/><image type=\"still\">http://x/s.jpg</image>");
        assert_eq!(p.still_image, None);
    }

    #[test]
    fn the_programme_icon_is_its_first_own_populated_icon() {
        // A rating's badge belongs to the rating, not the programme.
        let p = programme(
            "<icon src=\"http://x/art.png\"/>\
             <rating system=\"MPAA\"><value>PG</value><icon src=\"http://x/pg.png\"/></rating>\
             <star-rating><value>3/4</value><icon src=\"http://x/stars.png\"/></star-rating>",
        );
        assert_eq!(p.icon.as_deref(), Some("http://x/art.png"));
        let p = programme("<rating><value>PG</value><icon src=\"http://x/pg.png\"/></rating>");
        assert_eq!(p.icon, None);
        // The first icon wins, in either form; one with no src, width or height
        // is not an icon at all.
        let p =
            programme("<icon/><icon src=\"http://x/a.png\"></icon><icon src=\"http://x/b.png\"/>");
        assert_eq!(p.icon.as_deref(), Some("http://x/a.png"));
        // A sized icon without a source is still the first icon: no image.
        let p = programme("<icon width=\"100\"/><icon src=\"http://x/b.png\"/>");
        assert_eq!(p.icon, None);
        let p = programme("<icon width=\"wide\"/><icon src=\"\"/><icon src=\"http://x/b.png\"/>");
        assert_eq!(p.icon.as_deref(), Some("http://x/b.png"));
    }

    #[test]
    fn the_channel_icon_is_its_first_own_populated_icon() {
        let guide = parse_xmltv(
            "<tv><channel id=\"c\"><display-name>C</display-name><icon/>\
             <icon src=\"http://x/first.png\"></icon><icon src=\"http://x/second.png\"/>\
             <extra><icon src=\"http://x/nested.png\"/></extra></channel></tv>",
        );
        assert_eq!(
            guide.channels[0].icon.as_deref(),
            Some("http://x/first.png")
        );
        let guide = parse_xmltv(
            "<tv><channel id=\"c\"><display-name>C</display-name>\
             <extra><icon src=\"http://x/nested.png\"/></extra></channel></tv>",
        );
        assert_eq!(guide.channels[0].icon, None);
        // Unlike a programme's, a channel's children match case-sensitively
        // (`GetChannel` switches on `xmlChannel.Name`).
        let guide = parse_xmltv(
            "<tv><channel id=\"c\"><display-name>C</display-name>\
             <Icon src=\"http://x/upper.png\"/></channel></tv>",
        );
        assert_eq!(guide.channels[0].icon, None);
    }
}

/// [`read_xmltv_where`]: a guide read as it arrives, decoded the way
/// `XmlReader` decodes the cached file.
#[cfg(test)]
mod read_tests {
    use super::{parse_xmltv, read_xmltv_where};

    const GUIDE: &str = "<?xml version=\"1.0\"?>\n<tv>\
        <channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
        <programme start=\"20260725060000 +0000\" stop=\"20260725070000 +0000\" channel=\"one.tv\">\
        <title>Morning &amp; News</title><desc><![CDATA[Headlines.]]></desc>\
        <category>News</category><episode-num system=\"xmltv_ns\">0.5.</episode-num>\
        <previously-shown/></programme></tv>";

    /// A reader that hands out at most `step` bytes at a time, so every event
    /// straddles a buffer boundary somewhere.
    fn trickle(bytes: &[u8], step: usize) -> std::io::BufReader<&[u8]> {
        std::io::BufReader::with_capacity(step, bytes)
    }

    /// The same document reads the same from a byte source in any chunking as
    /// from a string.
    #[rstest::rstest]
    #[case(1)]
    #[case(7)]
    #[case(8192)]
    fn a_guide_read_as_it_arrives_reads_as_the_string_does(#[case] step: usize) {
        let expected = parse_xmltv(GUIDE);
        let read = read_xmltv_where(trickle(GUIDE.as_bytes(), step), |_| true);
        assert_eq!(read.error, None);
        assert_eq!(read.channels, expected.channels);
        assert_eq!(read.programmes, expected.programmes);
        assert_eq!(read.programmes[0].title, "Morning & News");
    }

    /// The `encoding` the declaration names decodes the document, as
    /// `XmlReader` honours it (Jellyfin.XmlTv's own `MultilanguageData.xml`
    /// is `ISO-8859-1`).
    #[test]
    fn a_guide_in_its_declared_encoding_is_decoded() {
        const BODY: &[u8] =
            b"<tv><channel id=\"a\"><display-name>T\xe9l\xe9</display-name></channel></tv>";
        let latin1 = [
            &b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n"[..],
            BODY,
        ]
        .concat();
        let guide = read_xmltv_where(trickle(&latin1, 5), |_| true);
        assert_eq!(guide.error, None);
        assert_eq!(guide.channels[0].display_name, "Télé");

        // Single quotes and spacing around `=` are the declaration's too.
        let latin1 = [&b"<?xml version='1.0' encoding = 'latin1' ?>"[..], BODY].concat();
        let guide = read_xmltv_where(&latin1[..], |_| true);
        assert_eq!(guide.error, None);
        assert_eq!(guide.channels[0].display_name, "Télé");
    }

    /// A byte-order mark decides, UTF-16 included.
    #[test]
    fn a_guide_with_a_byte_order_mark_is_decoded_by_it() {
        let text = "<tv><channel id=\"a\"><display-name>Ünï</display-name></channel></tv>";
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let guide = read_xmltv_where(trickle(&utf16, 3), |_| true);
        assert_eq!(guide.error, None);
        assert_eq!(guide.channels[0].display_name, "Ünï");

        let mut utf8 = b"\xEF\xBB\xBF".to_vec();
        utf8.extend_from_slice(text.as_bytes());
        let guide = read_xmltv_where(&utf8[..], |_| true);
        assert_eq!(guide.error, None);
        assert_eq!(guide.channels[0].display_name, "Ünï");
    }

    /// Bytes that are not valid in the document's encoding, an encoding that
    /// does not exist and a UTF-16 declaration on a single-byte document make
    /// it unreadable, as they make `XmlReader` throw; the refresh then keeps
    /// the stored guide.
    #[rstest::rstest]
    #[case::invalid_utf8(&b"<tv><channel id=\"a\"><display-name>T\xe9l\xe9</display-name></channel></tv>"[..])]
    #[case::unknown_encoding(&b"<?xml version=\"1.0\" encoding=\"klingon\"?><tv></tv>"[..])]
    #[case::utf16_declared_on_bytes(&b"<?xml version=\"1.0\" encoding=\"utf-16\"?><tv></tv>"[..])]
    fn a_guide_its_encoding_cannot_read_is_unreadable(#[case] bytes: &[u8]) {
        assert!(read_xmltv_where(bytes, |_| true).error.is_some());
    }

    /// A lone surrogate is not UTF-16, even in a well-formed document.
    #[test]
    fn a_utf16_guide_with_a_lone_surrogate_is_unreadable() {
        let utf16 =
            |text: &str| -> Vec<u8> { text.encode_utf16().flat_map(u16::to_le_bytes).collect() };
        let bytes = [
            vec![0xFF, 0xFE],
            utf16("<tv><channel id=\"a\"><display-name>"),
            vec![0x00, 0xD8],
            utf16("</display-name></channel></tv>"),
        ]
        .concat();
        assert!(read_xmltv_where(&bytes[..], |_| true).error.is_some());
    }

    /// An empty or white-space source is an empty document, not a rootless one.
    #[rstest::rstest]
    #[case(&b""[..])]
    #[case(&b" \n\t"[..])]
    fn an_empty_source_is_an_empty_document(#[case] bytes: &[u8]) {
        assert_eq!(
            read_xmltv_where(bytes, |_| true).error.as_deref(),
            Some("the XMLTV document is empty")
        );
    }
}
