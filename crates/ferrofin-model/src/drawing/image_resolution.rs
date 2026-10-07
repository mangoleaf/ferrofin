//! `ImageResolution` — port of `MediaBrowser.Model.Drawing.ImageResolution`.

use serde::Serialize;
use utoipa::ToSchema;

/// Enum `ImageResolution` — a standard output resolution tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, ToSchema)]
#[repr(i32)]
pub enum ImageResolution {
    /// Match the source resolution.
    #[default]
    MatchSource = 0,
    /// 144p.
    P144 = 1,
    /// 240p.
    P240 = 2,
    /// 360p.
    P360 = 3,
    /// 480p.
    P480 = 4,
    /// 720p.
    P720 = 5,
    /// 1080p.
    P1080 = 6,
    /// 1440p.
    P1440 = 7,
    /// 2160p.
    P2160 = 8,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    ImageResolution, None, {
        MatchSource => ("MatchSource", 0),
        P144 => ("P144", 1),
        P240 => ("P240", 2),
        P360 => ("P360", 3),
        P480 => ("P480", 4),
        P720 => ("P720", 5),
        P1080 => ("P1080", 6),
        P1440 => ("P1440", 7),
        P2160 => ("P2160", 8),
    }
}
