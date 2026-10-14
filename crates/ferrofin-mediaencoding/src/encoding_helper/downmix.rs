//! Downmix-to-stereo algorithm filter table and channel layout inference.
//!
//! Port of `MediaBrowser.Controller.MediaEncoding.DownMixAlgorithmsHelper` — the
//! lookup table that maps `(algorithm, channel_layout)` pairs to ffmpeg `pan=`
//! filter strings, and the heuristic that infers a layout string when the
//! metadata is absent.

use ferrofin_model::entities::DownMixStereoAlgorithms;
use ferrofin_model::entities_media::MediaStream;

/// Returns the ffmpeg filter string for the given downmix algorithm and channel
/// layout, if one exists in the upstream table.
///
/// Port of `DownMixAlgorithmsHelper.AlgorithmFilterStrings.TryGetValue`.
#[must_use]
pub fn algorithm_filter_string(
    algorithm: DownMixStereoAlgorithms,
    layout: &str,
) -> Option<&'static str> {
    // Port of the static `AlgorithmFilterStrings` dictionary.  The table is
    // small and searched rarely (once per transcode start), so a linear scan
    // is fine and avoids pulling in a map dependency.
    #[allow(clippy::match_same_arms)] // arms are data rows, not logic
    match (algorithm, layout) {
        // ---- Dave750 --------------------------------------------------------
        (DownMixStereoAlgorithms::Dave750, "5.1") => Some(
            "pan=stereo|c0=0.5*c2+0.707*c0+0.707*c4+0.5*c3\
             |c1=0.5*c2+0.707*c1+0.707*c5+0.5*c3",
        ),
        // Use AC-4 algorithm to downmix 7.1 inputs to 5.1 first
        (DownMixStereoAlgorithms::Dave750, "7.1") => Some(
            "pan=5.1(side)|c0=c0|c1=c1|c2=c2|c3=c3|c4=0.707*c4+0.707*c6|c5=0.707*c5+0.707*c7,\
             pan=stereo|c0=0.5*c2+0.707*c0+0.707*c4+0.5*c3\
             |c1=0.5*c2+0.707*c1+0.707*c5+0.5*c3",
        ),

        // ---- NightmodeDialogue ---------------------------------------------
        (DownMixStereoAlgorithms::NightmodeDialogue, "5.1") => {
            Some("pan=stereo|c0=c2+0.30*c0+0.30*c4|c1=c2+0.30*c1+0.30*c5")
        }
        // Use AC-4 algorithm to downmix 7.1 inputs to 5.1 first
        (DownMixStereoAlgorithms::NightmodeDialogue, "7.1") => Some(
            "pan=5.1(side)|c0=c0|c1=c1|c2=c2|c3=c3|c4=0.707*c4+0.707*c6|c5=0.707*c5+0.707*c7,\
             pan=stereo|c0=c2+0.30*c0+0.30*c4|c1=c2+0.30*c1+0.30*c5",
        ),

        // ---- Rfc7845 --------------------------------------------------------
        (DownMixStereoAlgorithms::Rfc7845, "3.0") => {
            Some("pan=stereo|c0=0.414214*c2+0.585786*c0|c1=0.414214*c2+0.585786*c1")
        }
        (DownMixStereoAlgorithms::Rfc7845, "quad") => Some(
            "pan=stereo|c0=0.422650*c0+0.366025*c2+0.211325*c3\
             |c1=0.422650*c1+0.366025*c3+0.211325*c2",
        ),
        (DownMixStereoAlgorithms::Rfc7845, "5.0") => Some(
            "pan=stereo|c0=0.460186*c2+0.650802*c0+0.563611*c3+0.325401*c4\
             |c1=0.460186*c2+0.650802*c1+0.563611*c4+0.325401*c3",
        ),
        (DownMixStereoAlgorithms::Rfc7845, "5.1") => Some(
            "pan=stereo|c0=0.374107*c2+0.529067*c0+0.458186*c4+0.264534*c5+0.374107*c3\
             |c1=0.374107*c2+0.529067*c1+0.458186*c5+0.264534*c4+0.374107*c3",
        ),
        (DownMixStereoAlgorithms::Rfc7845, "6.1") => Some(
            "pan=stereo|c0=0.321953*c2+0.455310*c0+0.394310*c5+0.227655*c6+0.278819*c4+0.321953*c3\
             |c1=0.321953*c2+0.455310*c1+0.394310*c6+0.227655*c5+0.278819*c4+0.321953*c3",
        ),
        (DownMixStereoAlgorithms::Rfc7845, "7.1") => Some(
            "pan=stereo|c0=0.274804*c2+0.388631*c0+0.336565*c6+0.194316*c7+0.336565*c4+0.194316*c5+0.274804*c3\
             |c1=0.274804*c2+0.388631*c1+0.336565*c7+0.194316*c6+0.336565*c5+0.194316*c4+0.274804*c3",
        ),

        // ---- Ac4 ------------------------------------------------------------
        (DownMixStereoAlgorithms::Ac4, "3.0") => Some("pan=stereo|c0=c0+0.707*c2|c1=c1+0.707*c2"),
        (DownMixStereoAlgorithms::Ac4, "5.0") => {
            Some("pan=stereo|c0=c0+0.707*c2+0.707*c3|c1=c1+0.707*c2+0.707*c4")
        }
        (DownMixStereoAlgorithms::Ac4, "5.1") => {
            Some("pan=stereo|c0=c0+0.707*c2+0.707*c4|c1=c1+0.707*c2+0.707*c5")
        }
        (DownMixStereoAlgorithms::Ac4, "7.0") => Some(
            "pan=5.0(side)|c0=c0|c1=c1|c2=c2|c3=0.707*c3+0.707*c5|c4=0.707*c4+0.707*c6,\
             pan=stereo|c0=c0+0.707*c2+0.707*c3|c1=c1+0.707*c2+0.707*c4",
        ),
        (DownMixStereoAlgorithms::Ac4, "7.1") => Some(
            "pan=5.1(side)|c0=c0|c1=c1|c2=c2|c3=c3|c4=0.707*c4+0.707*c6|c5=0.707*c5+0.707*c7,\
             pan=stereo|c0=c0+0.707*c2+0.707*c4|c1=c1+0.707*c2+0.707*c5",
        ),

        _ => None,
    }
}

/// Infers a channel layout string from the audio stream's metadata.
///
/// Port of `DownMixAlgorithmsHelper.InferChannelLayout`: returns the stream's
/// own `channel_layout` when present, otherwise guesses from the channel count.
/// The guesses prefer the more common surround layout at each count (e.g. 6
/// channels → `"5.1"` rather than `"6.0"`).
#[must_use]
pub fn infer_channel_layout(audio_stream: &MediaStream) -> &str {
    if let Some(layout) = audio_stream.channel_layout.as_deref()
        && !layout.trim().is_empty()
    {
        return layout;
    }

    match audio_stream.channels {
        Some(1) => "mono",
        Some(2) => "stereo",
        Some(3) => "2.1",
        Some(4) => "4.0",
        Some(5) => "5.0",
        Some(6) => "5.1",
        Some(7) => "6.1",
        Some(8) => "7.1",
        _ => "",
    }
}

/// Whether the downmix algorithm filter is active for this state/options pair.
///
/// Port of the `useDownMixAlgorithm` condition in the upstream
/// `GetDynamicHlsAudioArgs` / `GetProgressiveAudioFullCommandLine`: output
/// channels must be 2, source must be >2, and the `(algorithm, layout)` pair
/// must have an entry in the filter table. Used to suppress `-ac 2` when the
/// `pan=stereo|…` filter already sets the output channel count.
#[must_use]
pub fn uses_downmix_algorithm(
    state: &super::transcode_state::EncodingJobInfo,
    options: &ferrofin_model::configuration::EncodingOptions,
) -> bool {
    state.output_audio_channels == Some(2)
        && state.audio_stream.as_ref().is_some_and(|s| {
            s.channels.is_some_and(|c| c > 2)
                && algorithm_filter_string(
                    options.down_mix_stereo_algorithm,
                    infer_channel_layout(s),
                )
                .is_some()
        })
}

#[cfg(test)]
mod tests {
    use ferrofin_model::entities::MediaStreamType;

    use super::*;

    fn audio_stream_with(channels: Option<i32>, layout: Option<&str>) -> MediaStream {
        MediaStream {
            channels,
            channel_layout: layout.map(str::to_owned),
            stream_type: MediaStreamType::Audio,
            ..MediaStream::default()
        }
    }

    // ----- infer_channel_layout ----------------------------------------------

    #[test]
    fn infer_uses_explicit_layout_when_present() {
        let s = audio_stream_with(Some(6), Some("5.1(side)"));
        assert_eq!(infer_channel_layout(&s), "5.1(side)");
    }

    #[test]
    fn infer_ignores_whitespace_only_layout() {
        let s = audio_stream_with(Some(6), Some("  "));
        assert_eq!(infer_channel_layout(&s), "5.1");
    }

    #[test]
    fn infer_guesses_from_channel_count() {
        let cases = [
            (1, "mono"),
            (2, "stereo"),
            (3, "2.1"),
            (4, "4.0"),
            (5, "5.0"),
            (6, "5.1"),
            (7, "6.1"),
            (8, "7.1"),
        ];
        for (ch, expected) in cases {
            let s = audio_stream_with(Some(ch), None);
            assert_eq!(infer_channel_layout(&s), expected, "channels={ch}");
        }
    }

    #[test]
    fn infer_returns_empty_for_unknown_count() {
        let s = audio_stream_with(Some(9), None);
        assert_eq!(infer_channel_layout(&s), "");
        let s = audio_stream_with(None, None);
        assert_eq!(infer_channel_layout(&s), "");
    }

    // ----- algorithm_filter_string -------------------------------------------

    #[test]
    fn dave750_5_1_returns_expected_filter() {
        let f = algorithm_filter_string(DownMixStereoAlgorithms::Dave750, "5.1").unwrap();
        assert!(f.starts_with("pan=stereo|"), "{f}");
        assert!(f.contains("0.5*c2"), "{f}");
        assert!(f.contains("0.707*c0"), "{f}");
    }

    #[test]
    fn dave750_7_1_chains_two_pans() {
        let f = algorithm_filter_string(DownMixStereoAlgorithms::Dave750, "7.1").unwrap();
        // 7.1 → 5.1 → stereo: two pan= filters chained with comma.
        assert!(f.starts_with("pan=5.1(side)"), "{f}");
        assert!(f.contains(",pan=stereo|"), "{f}");
    }

    #[test]
    fn nightmode_5_1_boosts_center() {
        let f = algorithm_filter_string(DownMixStereoAlgorithms::NightmodeDialogue, "5.1").unwrap();
        // Centre (c2) has coefficient 1, surrounds 0.30.
        assert!(f.contains("|c0=c2+0.30*c0"), "{f}");
    }

    #[test]
    fn rfc7845_covers_3_0_through_7_1() {
        for layout in ["3.0", "quad", "5.0", "5.1", "6.1", "7.1"] {
            assert!(
                algorithm_filter_string(DownMixStereoAlgorithms::Rfc7845, layout).is_some(),
                "Rfc7845 missing {layout}"
            );
        }
    }

    #[test]
    fn ac4_covers_3_0_through_7_1() {
        for layout in ["3.0", "5.0", "5.1", "7.0", "7.1"] {
            assert!(
                algorithm_filter_string(DownMixStereoAlgorithms::Ac4, layout).is_some(),
                "Ac4 missing {layout}"
            );
        }
    }

    #[test]
    fn none_algorithm_never_matches() {
        for layout in ["mono", "stereo", "5.1", "7.1"] {
            assert!(
                algorithm_filter_string(DownMixStereoAlgorithms::None, layout).is_none(),
                "None should never match {layout}"
            );
        }
    }

    #[test]
    fn unknown_layout_returns_none() {
        assert!(algorithm_filter_string(DownMixStereoAlgorithms::Dave750, "hexagonal").is_none());
        assert!(algorithm_filter_string(DownMixStereoAlgorithms::Rfc7845, "").is_none());
    }

    // ----- uses_downmix_algorithm --------------------------------------------

    #[test]
    fn uses_downmix_when_algorithm_and_layout_match() {
        use super::super::transcode_state::{
            BaseEncodingJobOptions, EncodingJobInfo, TranscodeDisplayNames,
        };
        use ferrofin_model::configuration::EncodingOptions;
        use ferrofin_model::dlna::SubtitleDeliveryMethod;
        use ferrofin_model::dto::MediaSourceInfo;

        let mut surround = audio_stream_with(Some(6), Some("5.1"));
        surround.stream_type = MediaStreamType::Audio;
        let state = EncodingJobInfo {
            display: TranscodeDisplayNames::default(),
            base_request: BaseEncodingJobOptions::default(),
            video_stream: None,
            audio_stream: Some(surround),
            subtitle_stream: None,
            media_source: MediaSourceInfo::default(),
            output_video_codec: None,
            output_audio_codec: None,
            output_video_bitrate: None,
            output_audio_bitrate: None,
            output_audio_channels: Some(2),
            output_container: None,
            output_video_sync: None,
            output_file_path: String::new(),
            input_container: None,
            is_input_video: false,
            subtitle_delivery_method: SubtitleDeliveryMethod::Encode,
            run_time_ticks: None,
            transcoding_type: ferrofin_traits::media_encoding::TranscodingJobType::Hls,
            supported_video_codecs: Vec::new(),
            supported_audio_codecs: Vec::new(),
            segment_length_secs: 0,
            wait_for_path: None,
            segment_container: None,
            play_session_id: None,
            device_id: None,
        };

        let opts = EncodingOptions {
            down_mix_stereo_algorithm: DownMixStereoAlgorithms::Dave750,
            ..EncodingOptions::default()
        };
        assert!(uses_downmix_algorithm(&state, &opts));

        // None algorithm → not active.
        let opts_none = EncodingOptions {
            down_mix_stereo_algorithm: DownMixStereoAlgorithms::None,
            ..EncodingOptions::default()
        };
        assert!(!uses_downmix_algorithm(&state, &opts_none));
    }
}
