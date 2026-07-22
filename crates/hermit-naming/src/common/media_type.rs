//! Port of `Emby.Naming.Common.MediaType`.

/// Type of audiovisual media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    /// The audio.
    Audio = 0,
    /// The photo.
    Photo = 1,
    /// The video.
    Video = 2,
}
