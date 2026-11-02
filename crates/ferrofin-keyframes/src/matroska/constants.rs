//! Port of `Jellyfin.MediaEncoding.Keyframes.Matroska.MatroskaConstants`.
//!
//! Element ids keep their length marker, as the Matroska specification spells
//! them.

pub(super) const SEGMENT_CONTAINER: u64 = 0x1853_8067;

pub(super) const SEEK_HEAD: u64 = 0x114D_9B74;
pub(super) const SEEK: u64 = 0x4DBB;

pub(super) const INFO: u64 = 0x1549_A966;
pub(super) const TIMESTAMP_SCALE: u64 = 0x2A_D7B1;
pub(super) const DURATION: u64 = 0x4489;

pub(super) const TRACKS: u64 = 0x1654_AE6B;
pub(super) const TRACK_ENTRY: u64 = 0xAE;
pub(super) const TRACK_NUMBER: u64 = 0xD7;
pub(super) const TRACK_TYPE: u64 = 0x83;

pub(super) const TRACK_TYPE_VIDEO: u64 = 0x1;

pub(super) const CUES: u64 = 0x1C53_BB6B;
pub(super) const CUE_TIME: u64 = 0xB3;
pub(super) const CUE_POINT: u64 = 0xBB;
pub(super) const CUE_TRACK_POSITIONS: u64 = 0xB7;
pub(super) const CUE_POINT_TRACK_NUMBER: u64 = 0xF7;
