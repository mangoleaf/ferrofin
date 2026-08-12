//! `ImageResolution` — port of `MediaBrowser.Model.Drawing.ImageResolution`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Enum `ImageResolution` — a standard output resolution tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
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
}
