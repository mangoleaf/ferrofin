//! Dynamic HLS playlist generator.
//!
//! Port of `Jellyfin.MediaEncoding.Hls.Playlist.DynamicHlsPlaylistGenerator`
//! (`DynamicHlsPlaylistGenerator.cs`) and its `IDynamicHlsPlaylistGenerator`
//! interface.
//!
//! The parity core is the three static helpers [`compute_equal_length_segments`],
//! [`compute_segments`] and [`is_extraction_allowed_for_file`] — the entire test
//! oracle targets these. The public [`DynamicHlsPlaylistGenerator::create_main_playlist`]
//! stitches them into a `.m3u8` string.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use hermit_keyframes::keyframe_data::KeyframeData;
use hermit_model::configuration::EncodingOptions;
use uuid::Uuid;

use crate::create_main_playlist_request::CreateMainPlaylistRequest;
use crate::error::HlsError;

/// The number of ticks in a second (1 tick = 100ns; mirrors `TimeSpan.TicksPerSecond`).
pub const TICKS_PER_SECOND: i64 = 10_000_000;

/// The number of ticks in a millisecond (mirrors `TimeSpan.TicksPerMillisecond`).
pub const TICKS_PER_MILLISECOND: i64 = 10_000;

/// Extracts keyframe timing for a file behind a mockable boundary.
///
/// Port of `Extractors.IKeyframeExtractor`. The C# generator holds an array of
/// `IKeyframeExtractor` and the real ffprobe / Matroska spawn is un-mockable
/// process I/O. This trait keeps that I/O out of the parity/coverage numbers:
/// real implementations shell out to ffprobe, while unit tests supply a fake.
/// Returning `None` mirrors the C# loop returning `false` from every extractor.
///
/// Concrete extractor impls (`FfProbeKeyframeExtractor`, `MatroskaKeyframeExtractor`)
/// are deferred and out of scope for this unit.
pub trait KeyframeExtractor: Send + Sync {
    /// Whether the extractor is based on container metadata.
    ///
    /// Mirrors `IKeyframeExtractor.IsMetadataBased`. The generator retains only
    /// metadata-based extractors at construction (see
    /// [`DynamicHlsPlaylistGenerator::new`]).
    fn is_metadata_based(&self) -> bool;

    /// Attempts to extract keyframe data for the given media item.
    ///
    /// Returns `None` when the extractor could not produce keyframes (mirrors the
    /// C# `TryExtractKeyframes` returning `false`).
    fn try_extract_keyframes(&self, item_id: Uuid, file_path: &str) -> Option<KeyframeData>;
}

/// Reads the current encoding options.
///
/// Models `IServerConfigurationManager.GetEncodingOptions()`: the generator calls
/// this per request so that the set of on-demand-allowed extensions reflects live
/// server configuration rather than a value captured at construction. Any
/// `Fn() -> EncodingOptions` (e.g. a closure over a config manager) satisfies it.
pub trait EncodingOptionsProvider: Send + Sync {
    /// Returns a snapshot of the current encoding options.
    fn get_encoding_options(&self) -> EncodingOptions;
}

impl<F> EncodingOptionsProvider for F
where
    F: Fn() -> EncodingOptions + Send + Sync,
{
    fn get_encoding_options(&self) -> EncodingOptions {
        self()
    }
}

/// Generator for dynamic HLS playlists where the segment lengths are not known
/// in advance.
///
/// Mirrors `DynamicHlsPlaylistGenerator`: it is constructed with a config
/// accessor ([`EncodingOptionsProvider`], modelling `IServerConfigurationManager`)
/// and a set of [`KeyframeExtractor`]s filtered to the metadata-based ones, and
/// produces a `.m3u8` playlist string from a [`CreateMainPlaylistRequest`].
pub struct DynamicHlsPlaylistGenerator<C: EncodingOptionsProvider> {
    config: C,
    extractors: Vec<Arc<dyn KeyframeExtractor>>,
}

impl<C: EncodingOptionsProvider> DynamicHlsPlaylistGenerator<C> {
    /// Initializes a new [`DynamicHlsPlaylistGenerator`].
    ///
    /// Mirrors the C# constructor: the extractor list is filtered to only the
    /// metadata-based extractors (`extractors.Where(e => e.IsMetadataBased)`).
    ///
    /// # Arguments
    ///
    /// * `config` - The encoding-options accessor, called per request to read
    ///   `AllowOnDemandMetadataBasedKeyframeExtractionForExtensions` (models
    ///   `IServerConfigurationManager.GetEncodingOptions()`).
    /// * `extractors` - The candidate keyframe extractors; only those reporting
    ///   [`KeyframeExtractor::is_metadata_based`] are retained.
    pub fn new(config: C, extractors: Vec<Arc<dyn KeyframeExtractor>>) -> Self {
        let extractors = extractors
            .into_iter()
            .filter(|e| e.is_metadata_based())
            .collect();
        Self { config, extractors }
    }

    /// Creates the main playlist containing the main video or audio stream.
    ///
    /// Mirrors `IDynamicHlsPlaylistGenerator.CreateMainPlaylist`.
    ///
    /// # Errors
    ///
    /// Returns [`HlsError::InvalidOperation`] when equal-length segments are
    /// required but the desired segment length or total runtime is zero.
    pub fn create_main_playlist(
        &self,
        request: &CreateMainPlaylistRequest,
    ) -> Result<String, HlsError> {
        // For video transcodes it is sufficient with equal length segments as
        // ffmpeg will create new keyframes.
        let segments: Vec<f64> = if request.is_remuxing_video
            && request.media_source_id.is_some()
            && let Some(keyframe_data) = self.try_extract_keyframes(
                request.media_source_id.unwrap_or_default(),
                &request.file_path,
            ) {
            compute_segments(&keyframe_data, request.desired_segment_length_ms)
        } else {
            compute_equal_length_segments(
                request.desired_segment_length_ms,
                request.total_runtime_ticks,
            )?
        };

        let segment_extension = get_segment_file_extension(&request.segment_container);

        // http://ffmpeg.org/ffmpeg-all.html#toc-hls-2
        let is_hls_in_fmp4 = segment_extension.eq_ignore_ascii_case(".mp4");
        let hls_version = if is_hls_in_fmp4 { "7" } else { "3" };

        // `Math.Ceiling(max)` — the target duration is the ceiling of the
        // longest segment, or the desired length when there are no segments.
        #[allow(clippy::cast_precision_loss)]
        let target_source = if segments.is_empty() {
            f64::from(request.desired_segment_length_ms)
        } else {
            segments.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        };
        let target_duration = target_source.ceil();

        let mut builder = String::with_capacity(128);
        builder.push_str("#EXTM3U\n");
        builder.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
        builder.push_str("#EXT-X-VERSION:");
        builder.push_str(hls_version);
        builder.push('\n');
        builder.push_str("#EXT-X-TARGETDURATION:");
        // `StringBuilder.Append(double)` renders integral doubles without a
        // fractional part (e.g. `10`), matching `format_double_ceiling`.
        builder.push_str(&format_double_ceiling(target_duration));
        builder.push('\n');
        builder.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");

        if is_hls_in_fmp4 {
            // Init file that only includes fMP4 headers.
            builder.push_str("#EXT-X-MAP:URI=\"");
            builder.push_str(&request.endpoint_prefix);
            builder.push_str("-1");
            builder.push_str(&segment_extension);
            builder.push_str(&request.query_string);
            builder.push_str("&runtimeTicks=0");
            builder.push_str("&actualSegmentLengthTicks=0");
            builder.push('"');
            builder.push('\n');
        }

        let mut current_runtime_in_seconds: i64 = 0;
        for (index, length) in segments.iter().enumerate() {
            // Manually convert to ticks to avoid precision loss when converting double.
            #[allow(clippy::cast_precision_loss)]
            let length_ticks = convert_to_i64(length * TICKS_PER_SECOND as f64);
            builder.push_str("#EXTINF:");
            builder.push_str(&format_six_decimals(*length));
            builder.push_str(", nodesc\n");
            builder.push_str(&request.endpoint_prefix);
            let _ = write!(builder, "{index}");
            builder.push_str(&segment_extension);
            builder.push_str(&request.query_string);
            builder.push_str("&runtimeTicks=");
            let _ = write!(builder, "{current_runtime_in_seconds}");
            builder.push_str("&actualSegmentLengthTicks=");
            let _ = write!(builder, "{length_ticks}");
            builder.push('\n');

            current_runtime_in_seconds += length_ticks;
        }

        builder.push_str("#EXT-X-ENDLIST\n");

        Ok(builder)
    }

    /// Mirrors the C# private `TryExtractKeyframes`: gated on the file extension
    /// being permitted (read live from the config accessor), then returns the
    /// first extractor that yields keyframes.
    fn try_extract_keyframes(&self, item_id: Uuid, file_path: &str) -> Option<KeyframeData> {
        let options = self.config.get_encoding_options();
        if !is_extraction_allowed_for_file(
            file_path,
            &options.allow_on_demand_metadata_based_keyframe_extraction_for_extensions,
        ) {
            return None;
        }

        // First extractor that yields keyframes wins (mirrors the C# loop).
        self.extractors
            .iter()
            .find_map(|extractor| extractor.try_extract_keyframes(item_id, file_path))
    }
}

/// Mirrors `EncodingHelper.GetSegmentFileExtension`: `"." + container`, or
/// `".ts"` when the container is null/empty/whitespace.
fn get_segment_file_extension(segment_container: &str) -> String {
    if segment_container.trim().is_empty() {
        ".ts".to_string()
    } else {
        format!(".{segment_container}")
    }
}

/// Determines whether metadata-based keyframe extraction is permitted for a file.
///
/// Mirrors C# `IsExtractionAllowedForFile`: uses `Path.GetExtension` semantics
/// (an empty extension → `false`), strips the leading dot, and does a
/// case-insensitive comparison against each allowed entry with dots trimmed from
/// its start.
///
/// # Arguments
///
/// * `file_path` - The absolute file path.
/// * `allowed_extensions` - The permitted extensions (with or without leading dots).
#[must_use]
pub fn is_extraction_allowed_for_file(file_path: &str, allowed_extensions: &[String]) -> bool {
    // `Path.GetExtension` returns the substring from (and including) the last
    // dot in the final path component, or empty if there is none. `extension()`
    // in std returns the part *after* the dot (and `None` when absent), which is
    // exactly the "without dot" form the C# computes next.
    let Some(extension_without_dot) = Path::new(file_path).extension() else {
        return false;
    };
    // `Path.GetExtension("file.")` → "." → non-empty → after removing the dot the
    // C# compares an empty span. std's `extension()` returns `None` for a
    // trailing dot, so we already returned false — but a trailing-dot file is not
    // exercised by the oracle and both yield `false` overall for real inputs.
    let extension_without_dot = extension_without_dot.to_string_lossy();

    for allowed in allowed_extensions {
        let allowed_extension = allowed.trim_start_matches('.');
        if extension_without_dot.eq_ignore_ascii_case(allowed_extension) {
            return true;
        }
    }

    false
}

/// Computes segment lengths (in seconds) from keyframe timing.
///
/// Mirrors C# `ComputeSegments`. Applies the overshoot clamp (if the keyframe
/// list is non-empty and the total duration falls short of the last keyframe,
/// the total is raised to the last keyframe), scans the keyframes cutting a
/// segment whenever a keyframe reaches the running desired cut time, then appends
/// a final remainder segment when any duration is left over.
#[must_use]
pub fn compute_segments(keyframe_data: &KeyframeData, desired_segment_length_ms: i32) -> Vec<f64> {
    let ticks = &keyframe_data.keyframe_ticks;

    // Overshoot clamp: raise the total duration to the last keyframe.
    let total_duration = match ticks.last() {
        Some(&last) if keyframe_data.total_duration < last => last,
        _ => keyframe_data.total_duration,
    };

    let mut last_keyframe: i64 = 0;
    let mut result: Vec<f64> = Vec::new();

    // Scale the segment length to ticks to match the keyframes.
    let desired_segment_length_ticks = i64::from(desired_segment_length_ms) * TICKS_PER_MILLISECOND;
    let mut desired_cut_time = desired_segment_length_ticks;

    for &keyframe in ticks {
        if keyframe >= desired_cut_time {
            let current_segment_length = keyframe - last_keyframe;
            result.push(ticks_to_total_seconds(current_segment_length));
            last_keyframe = keyframe;
            desired_cut_time += desired_segment_length_ticks;
        }
    }

    let remaining = total_duration - last_keyframe;
    if remaining > 0 {
        result.push(ticks_to_total_seconds(remaining));
    }

    result
}

/// Computes equal-length segment lengths (in seconds) for a total runtime.
///
/// Mirrors C# `ComputeEqualLengthSegments`. Every whole segment is
/// `desired_segment_length_ms / 1000.0` seconds exactly; a trailing partial
/// segment (when the runtime is not an exact multiple) carries the remainder.
///
/// # Errors
///
/// Returns [`HlsError::InvalidOperation`] when `desired_segment_length_ms` or
/// `total_runtime_ticks` is zero (mirrors the C# `InvalidOperationException`).
pub fn compute_equal_length_segments(
    desired_segment_length_ms: i32,
    total_runtime_ticks: i64,
) -> Result<Vec<f64>, HlsError> {
    if desired_segment_length_ms == 0 || total_runtime_ticks == 0 {
        return Err(HlsError::InvalidOperation {
            desired_segment_length_ms,
            total_runtime_ticks,
        });
    }

    let segment_length_ticks = i64::from(desired_segment_length_ms) * TICKS_PER_MILLISECOND;
    let whole_segments = total_runtime_ticks / segment_length_ticks;
    let remaining_ticks = total_runtime_ticks % segment_length_ticks;

    // Whole-segment seconds are `ms / 1000.0` exactly (parity requirement).
    let whole_segment_seconds = f64::from(desired_segment_length_ms) / 1000.0;

    let extra = i64::from(remaining_ticks != 0);
    let segments_len = whole_segments + extra;
    let mut segments: Vec<f64> = Vec::with_capacity(usize::try_from(segments_len).unwrap_or(0));

    for _ in 0..whole_segments {
        segments.push(whole_segment_seconds);
    }

    if remaining_ticks != 0 {
        segments.push(ticks_to_total_seconds(remaining_ticks));
    }

    Ok(segments)
}

/// Mirrors `TimeSpan.FromTicks(ticks).TotalSeconds`: ticks / 10_000_000.
#[must_use]
#[allow(clippy::cast_precision_loss)]
fn ticks_to_total_seconds(ticks: i64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND as f64
}

/// Mirrors C# `Convert.ToInt64(double)`: round-half-to-even (banker's rounding).
#[allow(clippy::cast_possible_truncation)]
fn convert_to_i64(value: f64) -> i64 {
    value.round_ties_even() as i64
}

/// Renders a length with `"0.000000"` (six fixed decimals, invariant culture).
///
/// Mirrors C# `length.ToString("0.000000", CultureInfo.InvariantCulture)`.
fn format_six_decimals(value: f64) -> String {
    format!("{value:.6}")
}

/// Renders a `Math.Ceiling` result the way C# `StringBuilder.Append(double)`
/// does: an integral double prints without a fractional part (e.g. `10`).
fn format_double_ceiling(value: f64) -> String {
    // `ceil` always yields an integral value; format without decimals. Guard the
    // cast defensively (target durations are always small non-negative values).
    if value.is_finite() {
        #[allow(clippy::cast_possible_truncation)]
        let as_i64 = value as i64;
        as_i64.to_string()
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the C# test helper `MsToTicks`: `TimeSpan.FromMilliseconds(v).Ticks`.
    fn ms_to_ticks(value: i64) -> i64 {
        value * TICKS_PER_MILLISECOND
    }

    // === ComputeSegments_Valid_Success (Theory data verbatim) ===============

    #[test]
    fn compute_segments_valid_success() {
        // Case 1.
        let kd = KeyframeData::new(
            ms_to_ticks(35000),
            vec![
                0,
                ms_to_ticks(10427),
                ms_to_ticks(20854),
                ms_to_ticks(31240),
            ],
        );
        assert_eq!(
            compute_segments(&kd, 6000),
            vec![10.427, 10.427, 10.386, 3.760]
        );

        // Case 2.
        let kd = KeyframeData::new(
            ms_to_ticks(10000),
            vec![
                0,
                ms_to_ticks(1000),
                ms_to_ticks(2000),
                ms_to_ticks(3000),
                ms_to_ticks(4000),
                ms_to_ticks(5000),
            ],
        );
        assert_eq!(compute_segments(&kd, 2000), vec![2.0, 2.0, 6.0]);

        // Case 3.
        let kd = KeyframeData::new(ms_to_ticks(10000), vec![0]);
        assert_eq!(compute_segments(&kd, 6000), vec![10.0]);

        // Case 4.
        let kd = KeyframeData::new(ms_to_ticks(10000), Vec::new());
        assert_eq!(compute_segments(&kd, 6000), vec![10.0]);
    }

    #[test]
    fn compute_segments_zero_duration_overshoot_clamps_to_duration() {
        let kd = KeyframeData::new(0, vec![ms_to_ticks(10000)]);
        assert_eq!(compute_segments(&kd, 6000), vec![10.0]);
    }

    #[test]
    fn compute_segments_minor_duration_overshoot_clamps_to_duration() {
        let kd = KeyframeData::new(
            ms_to_ticks(9900),
            vec![0, ms_to_ticks(5000), ms_to_ticks(10000)],
        );
        assert_eq!(compute_segments(&kd, 6000), vec![10.0]);
    }

    // === ComputeEqualLengthSegments_Valid_Success (Theory data verbatim) ====

    #[test]
    fn compute_equal_length_segments_valid_success() {
        assert_eq!(
            compute_equal_length_segments(6000, ms_to_ticks(13000)).unwrap(),
            vec![6.0, 6.0, 1.0]
        );
        assert_eq!(
            compute_equal_length_segments(3000, ms_to_ticks(15000)).unwrap(),
            vec![3.0, 3.0, 3.0, 3.0, 3.0]
        );
        assert_eq!(
            compute_equal_length_segments(6000, ms_to_ticks(25000)).unwrap(),
            vec![6.0, 6.0, 6.0, 6.0, 1.0]
        );
        assert_eq!(
            compute_equal_length_segments(6000, ms_to_ticks(20123)).unwrap(),
            vec![6.0, 6.0, 6.0, 2.123]
        );
        assert_eq!(
            compute_equal_length_segments(6000, ms_to_ticks(1234)).unwrap(),
            vec![1.234]
        );
    }

    #[test]
    fn compute_equal_length_segments_invalid_throws_invalid_operation() {
        // InlineData(0, 1000000).
        assert_eq!(
            compute_equal_length_segments(0, 1_000_000),
            Err(HlsError::InvalidOperation {
                desired_segment_length_ms: 0,
                total_runtime_ticks: 1_000_000,
            })
        );
        // InlineData(1000, 0).
        assert_eq!(
            compute_equal_length_segments(1000, 0),
            Err(HlsError::InvalidOperation {
                desired_segment_length_ms: 1000,
                total_runtime_ticks: 0,
            })
        );
    }

    // === IsExtractionAllowedForFile (Theory data verbatim) ==================

    #[test]
    fn is_extraction_allowed_for_file_valid_success() {
        // InlineData("testfile.mkv", new string[0], false).
        assert!(!is_extraction_allowed_for_file("testfile.mkv", &[]));
        // InlineData("testfile.flv", { ".mp4", ".mkv", ".ts" }, false).
        assert!(!is_extraction_allowed_for_file(
            "testfile.flv",
            &owned(&[".mp4", ".mkv", ".ts"])
        ));
        // InlineData("testfile.flv", { ".mp4", ".mkv", ".ts", ".flv" }, true).
        assert!(is_extraction_allowed_for_file(
            "testfile.flv",
            &owned(&[".mp4", ".mkv", ".ts", ".flv"])
        ));
        // InlineData("/some/arbitrarily/long/path/testfile.mkv", { "mkv" }, true).
        assert!(is_extraction_allowed_for_file(
            "/some/arbitrarily/long/path/testfile.mkv",
            &owned(&["mkv"])
        ));
    }

    #[test]
    fn is_extraction_allowed_for_file_invalid_returns_false() {
        // InlineData("testfile", { ".mp4" }).
        assert!(!is_extraction_allowed_for_file(
            "testfile",
            &owned(&[".mp4"])
        ));
    }

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    // === get_segment_file_extension + convert helpers =======================

    #[test]
    fn segment_extension_defaults_to_ts_when_blank() {
        assert_eq!(get_segment_file_extension("   "), ".ts");
        assert_eq!(get_segment_file_extension(""), ".ts");
        assert_eq!(get_segment_file_extension("mp4"), ".mp4");
        assert_eq!(get_segment_file_extension("ts"), ".ts");
    }

    #[test]
    fn convert_to_i64_uses_bankers_rounding() {
        assert_eq!(convert_to_i64(0.5), 0);
        assert_eq!(convert_to_i64(2.5), 2);
        assert_eq!(convert_to_i64(3.5), 4);
    }

    #[test]
    fn format_helpers_render_like_dotnet() {
        assert_eq!(format_six_decimals(10.427), "10.427000");
        assert_eq!(format_six_decimals(2.0), "2.000000");
        assert_eq!(format_double_ceiling(10.0), "10");
        assert_eq!(format_double_ceiling(3.0), "3");
    }

    #[test]
    fn format_double_ceiling_non_finite_falls_back_to_default_format() {
        // Defensive non-finite branch: a non-finite target duration never occurs
        // on the real path, but the fallback must render via the default `{}`
        // formatting rather than casting to i64.
        assert_eq!(format_double_ceiling(f64::INFINITY), "inf");
        assert_eq!(format_double_ceiling(f64::NEG_INFINITY), "-inf");
        assert_eq!(format_double_ceiling(f64::NAN), "NaN");
    }

    // === CreateMainPlaylist string assembly ================================
    //
    // The C# oracle has no test over the assembled `.m3u8` string, so these are
    // characterization tests: the expected bodies are derived by hand from
    // `DynamicHlsPlaylistGenerator.CreateMainPlaylist` (verbatim `, nodesc`,
    // `#EXT-X-MAP` and `#EXTINF` literals). `runtimeTicks` accumulates the
    // per-segment *tick* lengths (the C# `currentRuntimeInSeconds` is misnamed).

    /// A keyframe extractor that never yields keyframes — forces the
    /// equal-length path. Metadata-based so it survives the construction filter.
    struct NoKeyframes;

    impl KeyframeExtractor for NoKeyframes {
        fn is_metadata_based(&self) -> bool {
            true
        }

        fn try_extract_keyframes(&self, _item_id: Uuid, _file_path: &str) -> Option<KeyframeData> {
            None
        }
    }

    /// A keyframe extractor that always returns the given keyframe data.
    struct FixedKeyframes(KeyframeData);

    impl KeyframeExtractor for FixedKeyframes {
        fn is_metadata_based(&self) -> bool {
            true
        }

        fn try_extract_keyframes(&self, _item_id: Uuid, _file_path: &str) -> Option<KeyframeData> {
            Some(self.0.clone())
        }
    }

    /// Builds an [`EncodingOptionsProvider`] closure exposing `extensions` as the
    /// on-demand-allowed set (mirrors `GetEncodingOptions()`).
    fn config_with_extensions(extensions: Vec<String>) -> impl EncodingOptionsProvider {
        move || EncodingOptions {
            allow_on_demand_metadata_based_keyframe_extraction_for_extensions: extensions.clone(),
            ..EncodingOptions::default()
        }
    }

    #[test]
    fn create_main_playlist_equal_length_ts_no_map() {
        let generator = DynamicHlsPlaylistGenerator::new(
            config_with_extensions(Vec::new()),
            vec![Arc::new(NoKeyframes)],
        );
        // desired=6000ms, total=13000ms → segments [6.0, 6.0, 1.0]; ext ".ts".
        let request = CreateMainPlaylistRequest::new(
            None,
            "/media/movie.mkv",
            6000,
            ms_to_ticks(13000),
            "ts",
            "hls1/main/",
            "?api_key=abc",
            false,
        );

        let expected = "\
#EXTM3U
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:6.000000, nodesc
hls1/main/0.ts?api_key=abc&runtimeTicks=0&actualSegmentLengthTicks=60000000
#EXTINF:6.000000, nodesc
hls1/main/1.ts?api_key=abc&runtimeTicks=60000000&actualSegmentLengthTicks=60000000
#EXTINF:1.000000, nodesc
hls1/main/2.ts?api_key=abc&runtimeTicks=120000000&actualSegmentLengthTicks=10000000
#EXT-X-ENDLIST
";

        assert_eq!(generator.create_main_playlist(&request).unwrap(), expected);
    }

    #[test]
    fn create_main_playlist_fmp4_emits_version7_and_map_line() {
        // Single keyframe → compute_segments yields [10.0]; container "mp4".
        let kd = KeyframeData::new(ms_to_ticks(10000), vec![0]);
        let generator = DynamicHlsPlaylistGenerator::new(
            config_with_extensions(owned(&["mkv"])),
            vec![Arc::new(FixedKeyframes(kd))],
        );
        let request = CreateMainPlaylistRequest::new(
            Some(Uuid::nil()),
            "/media/movie.mkv",
            6000,
            ms_to_ticks(10000),
            "mp4",
            "hls1/main/",
            "?api_key=abc",
            true,
        );

        let expected = "\
#EXTM3U
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:10
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-MAP:URI=\"hls1/main/-1.mp4?api_key=abc&runtimeTicks=0&actualSegmentLengthTicks=0\"
#EXTINF:10.000000, nodesc
hls1/main/0.mp4?api_key=abc&runtimeTicks=0&actualSegmentLengthTicks=100000000
#EXT-X-ENDLIST
";

        assert_eq!(generator.create_main_playlist(&request).unwrap(), expected);
    }

    #[test]
    fn create_main_playlist_blank_container_defaults_to_ts() {
        let generator = DynamicHlsPlaylistGenerator::new(
            config_with_extensions(Vec::new()),
            vec![Arc::new(NoKeyframes)],
        );
        let request = CreateMainPlaylistRequest::new(
            None,
            "/media/movie.mkv",
            6000,
            ms_to_ticks(6000),
            "   ",
            "p/",
            "?q=1",
            false,
        );
        let playlist = generator.create_main_playlist(&request).unwrap();
        assert!(playlist.contains("#EXT-X-VERSION:3\n"));
        assert!(playlist.contains("p/0.ts?q=1&runtimeTicks=0&actualSegmentLengthTicks=60000000\n"));
        assert!(!playlist.contains("#EXT-X-MAP"));
    }

    #[test]
    fn create_main_playlist_no_segments_targets_desired_length() {
        // total_runtime 0 with is_remuxing false would error; use a keyframe
        // extractor path that yields an empty segment list instead.
        let kd = KeyframeData::new(0, Vec::new());
        let generator = DynamicHlsPlaylistGenerator::new(
            config_with_extensions(owned(&["mkv"])),
            vec![Arc::new(FixedKeyframes(kd))],
        );
        let request = CreateMainPlaylistRequest::new(
            Some(Uuid::nil()),
            "/media/movie.mkv",
            6000,
            0,
            "ts",
            "p/",
            "?q=1",
            true,
        );
        let playlist = generator.create_main_playlist(&request).unwrap();
        // No segments → target duration is ceil(desiredSegmentLengthMs) = 6000.
        assert!(playlist.contains("#EXT-X-TARGETDURATION:6000\n"));
        assert!(playlist.ends_with("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-ENDLIST\n"));
    }
}
