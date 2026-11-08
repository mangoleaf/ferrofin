//! Matroska based keyframe extractor.
//!
//! Port of `Jellyfin.MediaEncoding.Keyframes.Matroska.MatroskaKeyframeExtractor`.
//! It reads the keyframes from the file's cue index instead of decoding the
//! file: the first `SeekHead` names the `Info` (timestamp scale and duration),
//! `Tracks` (the first video track) and `Cues` (one `CueTime` per indexed
//! keyframe), and only those three are read.

mod constants;
mod ebml_reader;

use std::fs::File;
use std::io::{Read, Seek};

use crate::error::KeyframesError;
use crate::keyframe_data::KeyframeData;
use constants as mc;
use ebml_reader::EbmlReader;

/// Extracts the keyframes of the first video track from a Matroska file's cues.
///
/// # Arguments
///
/// * `file_path` - The file path.
///
/// # Errors
///
/// Returns [`KeyframesError::MatroskaIo`] when the file cannot be read, and
/// [`KeyframesError::InvalidMatroska`] when it has no segment, no `SeekHead`
/// naming `Info`, `Tracks` and `Cues`, no video track, or an element that
/// cannot be read.
pub fn get_keyframe_data(file_path: &str) -> Result<KeyframeData, KeyframesError> {
    let file = File::open(file_path).map_err(KeyframesError::MatroskaIo)?;
    get_keyframe_data_from(file)
}

/// [`get_keyframe_data`] over any seekable source.
///
/// # Errors
///
/// As [`get_keyframe_data`].
pub fn get_keyframe_data_from<R: Read + Seek>(source: R) -> Result<KeyframeData, KeyframesError> {
    let mut reader = EbmlReader::new(source)?;

    let seek_head = reader.read_seek_head()?;
    // Upstream reads Info and Tracks in file order because NEbml cannot seek
    // backwards. This reader can, so the order does not matter.
    let info = reader.read_info(seek_head.info)?;
    let video_track_number =
        reader.find_first_track_number_by_type(seek_head.tracks, mc::TRACK_TYPE_VIDEO)?;

    let mut keyframes = Vec::new();
    reader.read_at(seek_head.cues)?;
    reader.enter_container()?;

    while reader.find_element(mc::CUE_POINT)? {
        reader.enter_container()?;
        let mut track_number = None;
        // Mandatory element.
        reader.find_element(mc::CUE_TIME)?;
        let cue_time = reader.read_uint()?;

        // Mandatory element. Only the cue point's first track position is
        // read, as upstream does.
        reader.find_element(mc::CUE_TRACK_POSITIONS)?;
        reader.enter_container()?;
        if reader.find_element(mc::CUE_POINT_TRACK_NUMBER)? {
            track_number = Some(reader.read_uint()?);
        }

        reader.leave_container()?;

        if track_number == Some(video_track_number) {
            keyframes.push(scale_to_ticks(cue_time, info.timestamp_scale));
        }

        reader.leave_container()?;
    }

    reader.leave_container()?;

    Ok(KeyframeData::new(
        scale_duration_to_ticks(info.duration.unwrap_or(0.0), info.timestamp_scale)?,
        keyframes,
    ))
}

/// `ScaleToTicks(ulong, long)`: the timestamp scale is in nanoseconds, so this
/// gives the value in ticks (1 tick = 100 ns). It wraps on overflow, as the
/// unchecked C# arithmetic does.
fn scale_to_ticks(unscaled_value: u64, timestamp_scale: i64) -> i64 {
    unscaled_value.cast_signed().wrapping_mul(timestamp_scale) / 100
}

/// `ScaleToTicks(double, long)`: as [`scale_to_ticks`] for the float duration,
/// rounded as `Convert.ToInt64` rounds (to nearest, ties to even). A value out
/// of the `long` range is an error, as `Convert.ToInt64` throws.
fn scale_duration_to_ticks(
    unscaled_value: f64,
    timestamp_scale: i64,
) -> Result<i64, KeyframesError> {
    // C# converts the long scale to double the same way.
    #[allow(clippy::cast_precision_loss)]
    let ticks = (unscaled_value * timestamp_scale as f64 / 100.0).round_ties_even();
    // `i64::MIN` is exactly representable; `i64::MAX` rounds up to 2^63.
    #[allow(clippy::cast_precision_loss)]
    let in_range = ticks >= i64::MIN as f64 && ticks < i64::MAX as f64;
    if !in_range {
        return Err(KeyframesError::InvalidMatroska(
            "the segment duration is out of range".to_owned(),
        ));
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(ticks as i64)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use rstest::rstest;

    use super::*;

    // -- A minimal Matroska writer ------------------------------------------

    thread_local! {
        /// Whether [`element`] writes every size in its 8-byte form rather than
        /// the shortest one muxers use.
        static EIGHT_BYTE_SIZES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// An id's bytes as written (the length marker included).
    fn id_bytes(id: u64) -> Vec<u8> {
        let bytes = id.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        bytes[skip..].to_vec()
    }

    /// `size` as an EBML number: the shortest length that does not read as
    /// "unknown", or 8 bytes.
    fn size_bytes(size: u64, eight_bytes: bool) -> Vec<u8> {
        let length = if eight_bytes {
            8
        } else {
            (1..=8u32)
                .find(|&n| size < (1u64 << (7 * n)) - 1)
                .expect("the size fits")
        };
        let marked = size | (1u64 << (7 * length));
        marked.to_be_bytes()[8 - length as usize..].to_vec()
    }

    /// An element: its id, its size and the data.
    fn element(id: u64, data: &[u8]) -> Vec<u8> {
        let eight_bytes = EIGHT_BYTE_SIZES.with(std::cell::Cell::get);
        [
            id_bytes(id),
            size_bytes(data.len() as u64, eight_bytes),
            data.to_vec(),
        ]
        .concat()
    }

    fn uint(id: u64, value: u64) -> Vec<u8> {
        let bytes = value.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count().min(7);
        element(id, &bytes[skip..])
    }

    fn container(id: u64, children: &[Vec<u8>]) -> Vec<u8> {
        element(id, &children.concat())
    }

    fn ebml_header() -> Vec<u8> {
        container(0x1A45_DFA3, &[element(0x4282, b"matroska")])
    }

    fn info(timestamp_scale: u64, duration: Option<Vec<u8>>) -> Vec<u8> {
        let mut children = vec![uint(mc::TIMESTAMP_SCALE, timestamp_scale)];
        children.extend(duration);
        container(mc::INFO, &children)
    }

    fn duration_f64(value: f64) -> Vec<u8> {
        element(mc::DURATION, &value.to_be_bytes())
    }

    fn tracks(entries: &[(u64, u64)]) -> Vec<u8> {
        let entries: Vec<Vec<u8>> = entries
            .iter()
            .map(|&(number, kind)| {
                container(
                    mc::TRACK_ENTRY,
                    &[uint(mc::TRACK_NUMBER, number), uint(mc::TRACK_TYPE, kind)],
                )
            })
            .collect();
        container(mc::TRACKS, &entries)
    }

    /// One cue point per `(time, tracks)`, a track position per track.
    fn cues(points: &[(u64, &[u64])]) -> Vec<u8> {
        let points: Vec<Vec<u8>> = points
            .iter()
            .map(|&(time, tracks)| {
                let mut children = vec![uint(mc::CUE_TIME, time)];
                children.extend(tracks.iter().map(|&track| {
                    container(
                        mc::CUE_TRACK_POSITIONS,
                        &[uint(mc::CUE_POINT_TRACK_NUMBER, track), uint(0xF1, 0)],
                    )
                }));
                container(mc::CUE_POINT, &children)
            })
            .collect();
        container(mc::CUES, &points)
    }

    /// A seek entry; the position is always 8 bytes, so the head's length
    /// does not depend on the positions.
    fn seek(id: u64, position: u64) -> Vec<u8> {
        container(
            mc::SEEK,
            &[
                element(0x53AB, &u32::try_from(id).expect("4-byte id").to_be_bytes()),
                element(0x53AC, &position.to_be_bytes()),
            ],
        )
    }

    /// How the segment's size is written.
    #[derive(Clone, Copy)]
    enum SegmentSize {
        Known,
        /// The 1-byte unknown size, `0xFF`, as a live muxer writes it.
        Unknown,
        /// The 8-byte unknown size, `0x01FFFFFFFFFFFFFF`.
        UnknownEightBytes,
    }

    /// A file: the EBML header, then a segment of a `SeekHead` naming each
    /// `(id, element)` in `indexed`, followed by those elements in order.
    fn file_with(indexed: &[(u64, Vec<u8>)], segment_size: SegmentSize) -> Vec<u8> {
        let head_of = |positions: &[u64]| {
            let entries: Vec<Vec<u8>> = indexed
                .iter()
                .zip(positions)
                .map(|((id, _), &position)| seek(*id, position))
                .collect();
            container(mc::SEEK_HEAD, &entries)
        };
        let head_length = head_of(&vec![0; indexed.len()]).len() as u64;
        let mut positions = Vec::new();
        let mut at = head_length;
        for (_, body) in indexed {
            positions.push(at);
            at += body.len() as u64;
        }
        let mut data = head_of(&positions);
        for (_, body) in indexed {
            data.extend_from_slice(body);
        }
        let segment = match segment_size {
            SegmentSize::Known => element(mc::SEGMENT_CONTAINER, &data),
            SegmentSize::Unknown => [id_bytes(mc::SEGMENT_CONTAINER), vec![0xFF], data].concat(),
            SegmentSize::UnknownEightBytes => [
                id_bytes(mc::SEGMENT_CONTAINER),
                vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
                data,
            ]
            .concat(),
        };
        [ebml_header(), segment].concat()
    }

    fn extract(bytes: Vec<u8>) -> Result<KeyframeData, KeyframesError> {
        get_keyframe_data_from(Cursor::new(bytes))
    }

    // -- Tests ----------------------------------------------------------------

    #[test]
    fn reads_the_first_video_tracks_cue_times_as_ticks() {
        let bytes = file_with(
            &[
                (mc::INFO, info(1_000_000, Some(duration_f64(10_000.0)))),
                (mc::TRACKS, tracks(&[(1, 2), (2, 1), (3, 1)])),
                (
                    mc::CUES,
                    cues(&[
                        (0, &[2]),
                        (0, &[1]),
                        (4_170, &[2]),
                        (6_000, &[3]),
                        (8_342, &[2]),
                    ]),
                ),
            ],
            SegmentSize::Known,
        );

        let data = extract(bytes).expect("extract");

        // Milliseconds (a 1 ms timestamp scale) are 10 000 ticks each. Track 1
        // is audio and track 3 is a second video track: neither counts.
        assert_eq!(data.keyframe_ticks, vec![0, 41_700_000, 83_420_000]);
        assert_eq!(data.total_duration, 100_000_000);
    }

    #[test]
    fn info_may_follow_tracks_and_cues() {
        let bytes = file_with(
            &[
                (mc::CUES, cues(&[(0, &[1]), (2_002, &[1])])),
                (mc::TRACKS, tracks(&[(1, 1)])),
                (mc::INFO, info(1_000_000, Some(duration_f64(3_000.0)))),
            ],
            SegmentSize::Known,
        );

        let data = extract(bytes).expect("extract");

        assert_eq!(data.keyframe_ticks, vec![0, 20_020_000]);
        assert_eq!(data.total_duration, 30_000_000);
    }

    #[rstest]
    #[case::one_byte(SegmentSize::Unknown)]
    #[case::eight_bytes(SegmentSize::UnknownEightBytes)]
    fn a_segment_of_unknown_size_runs_to_the_end_of_the_file(#[case] size: SegmentSize) {
        let bytes = file_with(
            &[
                (mc::INFO, info(1_000_000, Some(duration_f64(1_000.0)))),
                (mc::TRACKS, tracks(&[(1, 1)])),
                (mc::CUES, cues(&[(0, &[1]), (500, &[1])])),
            ],
            size,
        );

        let data = extract(bytes).expect("extract");

        assert_eq!(data.keyframe_ticks, vec![0, 5_000_000]);
    }

    /// Muxers write the shortest size that fits, which the other tests use;
    /// the 8-byte form must read the same.
    #[test]
    fn sizes_written_in_eight_bytes_read_the_same() {
        EIGHT_BYTE_SIZES.with(|eight| eight.set(true));
        let bytes = file_with(
            &[
                (mc::INFO, info(1_000_000, Some(duration_f64(3_000.0)))),
                (mc::TRACKS, tracks(&[(1, 1)])),
                (mc::CUES, cues(&[(0, &[1]), (2_002, &[1])])),
            ],
            SegmentSize::Known,
        );
        EIGHT_BYTE_SIZES.with(|eight| eight.set(false));

        let data = extract(bytes).expect("extract");

        assert_eq!(data.keyframe_ticks, vec![0, 20_020_000]);
        assert_eq!(data.total_duration, 30_000_000);
    }

    /// A 2-byte size (an element of 127 bytes or more), next to the 1-byte
    /// sizes of its children.
    #[test]
    fn a_cues_element_with_a_two_byte_size_is_read() {
        let points: Vec<(u64, &[u64])> = (0..20).map(|i| (i * 1_000, &[1u64][..])).collect();
        let cues = cues(&points);
        assert_eq!(cues[4] & 0xC0, 0x40, "the Cues size is the 2-byte form");
        let bytes = file_with(
            &[
                (mc::INFO, info(1_000_000, None)),
                (mc::TRACKS, tracks(&[(1, 1)])),
                (mc::CUES, cues),
            ],
            SegmentSize::Known,
        );

        let data = extract(bytes).expect("extract");

        assert_eq!(
            data.keyframe_ticks,
            (0..20).map(|i| i * 10_000_000).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_missing_duration_is_zero() {
        let bytes = file_with(
            &[
                (mc::INFO, info(1_000_000, None)),
                (mc::TRACKS, tracks(&[(1, 1)])),
                (mc::CUES, cues(&[(0, &[1])])),
            ],
            SegmentSize::Known,
        );

        assert_eq!(extract(bytes).expect("extract").total_duration, 0);
    }

    #[test]
    fn the_timestamp_scale_and_a_4_byte_duration_are_honoured() {
        let bytes = file_with(
            &[
                (
                    mc::INFO,
                    info(100_000, Some(element(mc::DURATION, &1.5f32.to_be_bytes()))),
                ),
                (mc::TRACKS, tracks(&[(1, 1)])),
                (mc::CUES, cues(&[(25, &[1])])),
            ],
            SegmentSize::Known,
        );

        let data = extract(bytes).expect("extract");

        // 25 units of 100 µs = 2.5 ms = 25 000 ticks; 1.5 units = 1 500 ticks.
        assert_eq!(data.keyframe_ticks, vec![25_000]);
        assert_eq!(data.total_duration, 1_500);
    }

    /// `Convert.ToInt64` rounds the duration half to even.
    #[rstest]
    #[case(0.5, 0)]
    #[case(1.5, 2)]
    #[case(2.5, 2)]
    #[case(2.6, 3)]
    fn the_duration_rounds_half_to_even(#[case] duration: f64, #[case] ticks: i64) {
        assert_eq!(
            scale_duration_to_ticks(duration, 100).expect("in range"),
            ticks
        );
    }

    #[test]
    fn a_duration_out_of_range_is_an_error() {
        assert!(matches!(
            scale_duration_to_ticks(f64::MAX, 1_000_000),
            Err(KeyframesError::InvalidMatroska(_))
        ));
        assert!(scale_duration_to_ticks(f64::NAN, 1_000_000).is_err());
    }

    #[test]
    fn only_a_cue_points_first_track_position_counts() {
        let bytes = file_with(
            &[
                (mc::INFO, info(1_000_000, None)),
                (mc::TRACKS, tracks(&[(1, 2), (2, 1)])),
                (mc::CUES, cues(&[(0, &[2, 1]), (1_000, &[1, 2])])),
            ],
            SegmentSize::Known,
        );

        assert_eq!(extract(bytes).expect("extract").keyframe_ticks, vec![0]);
    }

    #[test]
    fn a_seek_head_without_cues_is_an_error() {
        let bytes = file_with(
            &[
                (mc::INFO, info(1_000_000, None)),
                (mc::TRACKS, tracks(&[(1, 1)])),
            ],
            SegmentSize::Known,
        );

        let error = extract(bytes).expect_err("no cues");

        assert!(
            error.to_string().starts_with("SeekHead is missing"),
            "{error}"
        );
    }

    #[test]
    fn a_file_without_a_video_track_is_an_error() {
        let bytes = file_with(
            &[
                (mc::INFO, info(1_000_000, None)),
                (mc::TRACKS, tracks(&[(1, 2)])),
                (mc::CUES, cues(&[(0, &[1])])),
            ],
            SegmentSize::Known,
        );

        let error = extract(bytes).expect_err("no video");

        assert_eq!(error.to_string(), "No stream with type 1 found");
    }

    #[test]
    fn a_file_without_a_segment_is_an_error() {
        let error = extract(ebml_header()).expect_err("no segment");

        assert_eq!(error.to_string(), "Expected a segment container");
    }

    #[test]
    fn a_segment_without_a_seek_head_is_an_error() {
        let bytes = [
            ebml_header(),
            container(mc::SEGMENT_CONTAINER, &[info(1, None)]),
        ]
        .concat();

        let error = extract(bytes).expect_err("no seek head");

        assert_eq!(error.to_string(), "Expected a SeekHead");
    }

    #[test]
    fn a_file_cut_off_before_its_cues_is_an_error() {
        let mut bytes = file_with(
            &[
                (mc::INFO, info(1_000_000, None)),
                (mc::TRACKS, tracks(&[(1, 1)])),
                (mc::CUES, cues(&[(0, &[1]), (1_000, &[1])])),
            ],
            SegmentSize::Known,
        );
        bytes.truncate(bytes.len() - 30);

        assert!(extract(bytes).is_err());
    }

    #[test]
    fn a_file_that_is_not_ebml_is_an_error() {
        assert!(extract(vec![0u8; 64]).is_err());
        assert!(extract(Vec::new()).is_err());
    }

    #[test]
    fn a_missing_file_is_an_io_error() {
        let error = get_keyframe_data("/nonexistent/ferrofin/missing.mkv").expect_err("missing");

        assert!(matches!(error, KeyframesError::MatroskaIo(_)), "{error}");
    }
}
