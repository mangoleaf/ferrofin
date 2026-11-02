//! An EBML reader over a seekable stream, with the Matroska reads built on it.
//!
//! Upstream reads Matroska with NEbml's `EbmlReader` and adds the reads it needs
//! in `Matroska.Extensions.EbmlReaderExtensions`. This is the slice of NEbml
//! those use (`ReadNext`, `EnterContainer`, `LeaveContainer`, `ReadAt`,
//! `ReadUInt`, `ReadFloat`, `ReadBinary`), with the extensions as methods.
//!
//! Like NEbml, the reader walks one container at a time: `read_next` steps to
//! the next child of the current container, and `read_at` is relative to the
//! current container's data, which is how Matroska's `SeekPosition` is defined
//! (from the first byte of the segment's data).

use std::io::{BufReader, Read, Seek, SeekFrom};

use super::constants as mc;
use crate::error::KeyframesError;

type Result<T> = std::result::Result<T, KeyframesError>;

fn invalid(message: impl Into<String>) -> KeyframesError {
    KeyframesError::InvalidMatroska(message.into())
}

/// The longest element id EBML allows by default (`EBMLMaxIDLength`).
const MAX_ID_LENGTH: u32 = 4;

/// The longest element size EBML allows (`EBMLMaxSizeLength`).
const MAX_SIZE_LENGTH: u32 = 8;

/// Port of `Matroska.Models.SeekHead` (`InfoPosition`, `TracksPosition`,
/// `CuesPosition`): where the segment's `Info`, `Tracks` and `Cues` start,
/// relative to the segment's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SeekHead {
    pub(super) info: u64,
    pub(super) tracks: u64,
    pub(super) cues: u64,
}

/// Port of `Matroska.Models.Info`: the segment's timestamp scale (nanoseconds
/// per timestamp unit) and its duration in timestamp units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Info {
    pub(super) timestamp_scale: i64,
    pub(super) duration: Option<f64>,
}

/// The element the reader is on: its id (with the length marker), where its
/// data starts and how long the data is.
#[derive(Debug, Clone, Copy)]
struct Element {
    id: u64,
    data_start: u64,
    size: u64,
}

/// A container the reader has entered: where its children start and end, and
/// where the element after it starts.
#[derive(Debug, Clone, Copy)]
struct Container {
    data_start: u64,
    end: u64,
    next_sibling: u64,
}

/// A forward EBML reader that can also jump to a position in its container.
pub(super) struct EbmlReader<R> {
    source: BufReader<R>,
    /// Where `source` is, so a read that follows on needs no seek.
    position: u64,
    /// The stream's length: the end of the top-level "container".
    stream_end: u64,
    containers: Vec<Container>,
    element: Option<Element>,
    /// Where the next element of the current container starts.
    next: u64,
}

impl<R: Read + Seek> EbmlReader<R> {
    /// A reader at the start of `source`.
    pub(super) fn new(mut source: R) -> Result<Self> {
        let stream_end = source
            .seek(SeekFrom::End(0))
            .map_err(KeyframesError::MatroskaIo)?;
        source
            .seek(SeekFrom::Start(0))
            .map_err(KeyframesError::MatroskaIo)?;
        Ok(Self {
            source: BufReader::new(source),
            position: 0,
            stream_end,
            containers: Vec::new(),
            element: None,
            next: 0,
        })
    }

    /// The current container's data start and end.
    fn bounds(&self) -> (u64, u64) {
        self.containers
            .last()
            .map_or((0, self.stream_end), |c| (c.data_start, c.end))
    }

    fn element(&self) -> Result<Element> {
        self.element
            .ok_or_else(|| invalid("an element the Matroska layout requires is missing"))
    }

    fn seek_to(&mut self, position: u64) -> Result<()> {
        if position != self.position {
            let delta = i64::try_from(i128::from(position) - i128::from(self.position))
                .map_err(|_| invalid("an element position is out of range"))?;
            self.source
                .seek_relative(delta)
                .map_err(KeyframesError::MatroskaIo)?;
            self.position = position;
        }
        Ok(())
    }

    fn read_exact(&mut self, buffer: &mut [u8]) -> Result<()> {
        self.source
            .read_exact(buffer)
            .map_err(KeyframesError::MatroskaIo)?;
        self.position += buffer.len() as u64;
        Ok(())
    }

    /// Reads a variable-length integer of at most `max_length` bytes and
    /// returns its bytes as read (the marker kept, as element ids are written),
    /// its value (the marker dropped, as sizes are read), its length, and
    /// whether every value bit is set (an unknown size).
    fn read_vint(&mut self, max_length: u32) -> Result<(u64, u64, u32, bool)> {
        let mut first = [0u8; 1];
        self.read_exact(&mut first)?;
        let length = first[0].leading_zeros() + 1;
        if length > max_length {
            return Err(invalid("an element header is not a valid EBML number"));
        }
        let mut raw = u64::from(first[0]);
        for _ in 1..length {
            let mut byte = [0u8; 1];
            self.read_exact(&mut byte)?;
            raw = (raw << 8) | u64::from(byte[0]);
        }
        let marker = 1u64 << (7 * length);
        let value = raw & (marker - 1);
        Ok((raw, value, length, value == marker - 1))
    }

    /// NEbml `ReadNext`: steps to the next element of the current container.
    /// `false` at the container's end.
    pub(super) fn read_next(&mut self) -> Result<bool> {
        let (_, end) = self.bounds();
        if self.next >= end {
            self.element = None;
            return Ok(false);
        }
        let start = self.next;
        self.seek_to(start)?;
        let (id, _, id_length, _) = self.read_vint(MAX_ID_LENGTH)?;
        let (_, size, size_length, unknown) = self.read_vint(MAX_SIZE_LENGTH)?;
        let data_start = start + u64::from(id_length + size_length);
        // An unknown size (a live-written segment) runs to the end of its
        // container, and so does a size that claims more than the container
        // holds (a file cut short).
        let room = end.saturating_sub(data_start);
        let size = if unknown { room } else { size.min(room) };
        self.element = Some(Element {
            id,
            data_start,
            size,
        });
        self.next = data_start + size;
        Ok(true)
    }

    /// NEbml `EnterContainer`: reads the current element's children next.
    pub(super) fn enter_container(&mut self) -> Result<()> {
        let element = self.element()?;
        self.containers.push(Container {
            data_start: element.data_start,
            end: element.data_start + element.size,
            next_sibling: self.next,
        });
        self.element = None;
        self.next = element.data_start;
        Ok(())
    }

    /// NEbml `LeaveContainer`: goes back to the parent, after the container.
    pub(super) fn leave_container(&mut self) -> Result<()> {
        let container = self
            .containers
            .pop()
            .ok_or_else(|| invalid("left a container that was never entered"))?;
        self.element = None;
        self.next = container.next_sibling;
        Ok(())
    }

    /// NEbml `ReadAt`: reads the element at `position` in the current
    /// container's data.
    pub(super) fn read_at(&mut self, position: u64) -> Result<bool> {
        let (data_start, _) = self.bounds();
        self.next = data_start
            .checked_add(position)
            .ok_or_else(|| invalid("an element position is out of range"))?;
        self.read_next()
    }

    /// NEbml `ReadUInt`: the current element as a big-endian unsigned integer.
    pub(super) fn read_uint(&mut self) -> Result<u64> {
        let element = self.element()?;
        let length = usize::try_from(element.size)
            .ok()
            .filter(|&n| n <= 8)
            .ok_or_else(|| invalid("an unsigned integer is longer than 8 bytes"))?;
        let mut buffer = [0u8; 8];
        self.seek_to(element.data_start)?;
        self.read_exact(&mut buffer[..length])?;
        Ok(buffer[..length]
            .iter()
            .fold(0, |value, &byte| (value << 8) | u64::from(byte)))
    }

    /// NEbml `ReadFloat`: the current element as a 4- or 8-byte float (an
    /// empty one reads as 0).
    pub(super) fn read_float(&mut self) -> Result<f64> {
        let element = self.element()?;
        self.seek_to(element.data_start)?;
        match element.size {
            0 => Ok(0.0),
            4 => {
                let mut buffer = [0u8; 4];
                self.read_exact(&mut buffer)?;
                Ok(f64::from(f32::from_be_bytes(buffer)))
            }
            8 => {
                let mut buffer = [0u8; 8];
                self.read_exact(&mut buffer)?;
                Ok(f64::from_be_bytes(buffer))
            }
            _ => Err(invalid("a float is neither 4 nor 8 bytes")),
        }
    }

    /// `ReadUIntFromBinary`: the first 4 bytes of the current element, big
    /// endian (a `SeekID`, which names a top-level element by its 4-byte id).
    fn read_uint_from_binary(&mut self) -> Result<u32> {
        let element = self.element()?;
        if element.size < 4 {
            return Err(invalid("a SeekID is shorter than 4 bytes"));
        }
        let mut buffer = [0u8; 4];
        self.seek_to(element.data_start)?;
        self.read_exact(&mut buffer)?;
        Ok(u32::from_be_bytes(buffer))
    }

    /// `FindElement`: steps through the current container until an element
    /// with `identifier`; `false` when the container ends first.
    pub(super) fn find_element(&mut self, identifier: u64) -> Result<bool> {
        while self.read_next()? {
            if self.element.is_some_and(|e| e.id == identifier) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `ReadSeekHead`: enters the segment and reads where its `Info`, `Tracks`
    /// and `Cues` are from its first `SeekHead`. The reader stays in the segment.
    pub(super) fn read_seek_head(&mut self) -> Result<SeekHead> {
        // Skip the header.
        if !self.find_element(mc::SEGMENT_CONTAINER)? {
            return Err(invalid("Expected a segment container"));
        }
        self.enter_container()?;

        let mut tracks_position = None;
        let mut cues_position = None;
        let mut info_position = None;
        // The first element should be a SeekHead otherwise we'll have to search manually.
        if !self.find_element(mc::SEEK_HEAD)? {
            return Err(invalid("Expected a SeekHead"));
        }
        self.enter_container()?;
        while self.find_element(mc::SEEK)? {
            self.enter_container()?;
            self.read_next()?;
            match u64::from(self.read_uint_from_binary()?) {
                mc::TRACKS => {
                    self.read_next()?;
                    tracks_position = Some(self.read_uint()?);
                }
                mc::CUES => {
                    self.read_next()?;
                    cues_position = Some(self.read_uint()?);
                }
                mc::INFO => {
                    self.read_next()?;
                    info_position = Some(self.read_uint()?);
                }
                _ => {}
            }
            self.leave_container()?;

            if tracks_position.is_some() && cues_position.is_some() && info_position.is_some() {
                break;
            }
        }
        self.leave_container()?;

        match (info_position, tracks_position, cues_position) {
            (Some(info), Some(tracks), Some(cues)) => Ok(SeekHead { info, tracks, cues }),
            _ => Err(invalid(
                "SeekHead is missing or does not contain Info, Tracks and Cues positions. \
                 SeekHead referencing another SeekHead is not supported",
            )),
        }
    }

    /// `ReadInfo`: the timestamp scale and duration of the `Info` at `position`.
    pub(super) fn read_info(&mut self, position: u64) -> Result<Info> {
        self.read_at(position)?;

        let mut duration = None;
        self.enter_container()?;
        // Mandatory element.
        self.find_element(mc::TIMESTAMP_SCALE)?;
        let timestamp_scale = self.read_uint()?;

        if self.find_element(mc::DURATION)? {
            duration = Some(self.read_float()?);
        }

        self.leave_container()?;

        Ok(Info {
            // Upstream's `(long)` cast.
            timestamp_scale: timestamp_scale.cast_signed(),
            duration,
        })
    }

    /// `FindFirstTrackNumberByType`: the number of the first track of
    /// `track_type` in the `Tracks` at `tracks_position`.
    pub(super) fn find_first_track_number_by_type(
        &mut self,
        tracks_position: u64,
        track_type: u64,
    ) -> Result<u64> {
        self.read_at(tracks_position)?;

        self.enter_container()?;
        while self.find_element(mc::TRACK_ENTRY)? {
            self.enter_container()?;
            // Mandatory element.
            self.find_element(mc::TRACK_NUMBER)?;
            let track_number = self.read_uint()?;

            // Mandatory element.
            self.find_element(mc::TRACK_TYPE)?;
            let entry_type = self.read_uint()?;

            self.leave_container()?;
            if entry_type == track_type {
                self.leave_container()?;
                return Ok(track_number);
            }
        }

        self.leave_container()?;

        Err(invalid(format!("No stream with type {track_type} found")))
    }
}
