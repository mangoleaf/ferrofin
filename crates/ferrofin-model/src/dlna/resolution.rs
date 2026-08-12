//! Port of `ResolutionConfiguration`, `ResolutionOptions` and
//! `ResolutionNormalizer` from `MediaBrowser.Model.Dlna`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A max-width / max-bitrate pairing used to pick a downscale target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ResolutionConfiguration {
    /// The maximum width for this configuration.
    pub max_width: i32,
    /// The maximum bitrate for this configuration.
    pub max_bitrate: i32,
}

impl ResolutionConfiguration {
    /// Creates a new configuration.
    #[must_use]
    pub fn new(max_width: i32, max_bitrate: i32) -> Self {
        Self {
            max_width,
            max_bitrate,
        }
    }
}

/// The resolution constraints produced by [`ResolutionNormalizer::normalize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ResolutionOptions {
    /// The maximum width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<i32>,
    /// The maximum height.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<i32>,
}

/// Reference bitrate at which the normalizer stops downscaling; the reference
/// frame rate the bitrate curve is calibrated against (SDR h264 at 30fps).
const REFERENCE_FPS: f32 = 30.0;
/// HDR content is allotted a lower reference bitrate than SDR.
const HDR_BITRATE_SCALE: f32 = 0.8;

/// The bitrate curve, in the scale of SDR h264 at 30fps.
const CONFIGURATIONS: [ResolutionConfiguration; 8] = [
    ResolutionConfiguration {
        max_width: 416,
        max_bitrate: 365_000,
    },
    ResolutionConfiguration {
        max_width: 640,
        max_bitrate: 730_000,
    },
    ResolutionConfiguration {
        max_width: 768,
        max_bitrate: 1_100_000,
    },
    ResolutionConfiguration {
        max_width: 960,
        max_bitrate: 3_000_000,
    },
    ResolutionConfiguration {
        max_width: 1280,
        max_bitrate: 6_000_000,
    },
    ResolutionConfiguration {
        max_width: 1920,
        max_bitrate: 13_500_000,
    },
    ResolutionConfiguration {
        max_width: 2560,
        max_bitrate: 28_000_000,
    },
    ResolutionConfiguration {
        max_width: 3840,
        max_bitrate: 50_000_000,
    },
];

/// Chooses a downscale resolution based on the target output bitrate.
pub struct ResolutionNormalizer;

impl ResolutionNormalizer {
    /// Normalizes the requested resolution against the output bitrate,
    /// downscaling only when the bitrate curve calls for it.
    ///
    /// Mirrors `ResolutionNormalizer.Normalize`. HDR transcoding is not
    /// performed yet; the `is_hdr` flag exists for future use.
    #[must_use]
    pub fn normalize(
        input_bitrate: Option<i32>,
        output_bitrate: i32,
        h264_equivalent_output_bitrate: i32,
        max_width: Option<i32>,
        max_height: Option<i32>,
        target_fps: Option<f32>,
        is_hdr: bool,
    ) -> ResolutionOptions {
        // If the bitrate isn't changing, then don't downscale the resolution.
        if let Some(input_bitrate) = input_bitrate
            && output_bitrate >= input_bitrate
            && (max_width.is_some() || max_height.is_some())
        {
            return ResolutionOptions {
                max_width,
                max_height,
            };
        }

        // The reference bitrate is based on SDR h264 at 30fps.
        let reference_fps = target_fps.unwrap_or(REFERENCE_FPS);
        let reference_scale = if reference_fps <= REFERENCE_FPS {
            REFERENCE_FPS / reference_fps
        } else {
            1.0 / (reference_fps / REFERENCE_FPS).sqrt()
        };
        #[allow(clippy::cast_precision_loss)]
        let mut reference_bitrate = h264_equivalent_output_bitrate as f32 * reference_scale;

        if is_hdr {
            reference_bitrate *= HDR_BITRATE_SCALE;
        }

        let Some(resolution_config) = Self::configuration_for(convert_to_i32(reference_bitrate))
        else {
            return ResolutionOptions {
                max_width,
                max_height,
            };
        };

        let origin_width_value = max_width;

        let new_max_width = resolution_config
            .max_width
            .min(max_width.unwrap_or(resolution_config.max_width));
        let new_max_height = match origin_width_value {
            Some(w) if w == new_max_width => max_height,
            _ => None,
        };

        ResolutionOptions {
            max_width: Some(new_max_width),
            max_height: new_max_height,
        }
    }

    /// Returns the first configuration whose `max_bitrate` covers
    /// `output_bitrate`.
    fn configuration_for(output_bitrate: i32) -> Option<ResolutionConfiguration> {
        CONFIGURATIONS
            .into_iter()
            .find(|config| output_bitrate <= config.max_bitrate)
    }
}

/// Round-half-to-even conversion matching C#'s `Convert.ToInt32(float)`.
#[allow(clippy::cast_possible_truncation)]
fn convert_to_i32(value: f32) -> i32 {
    f32::round_ties_even(value) as i32
}
