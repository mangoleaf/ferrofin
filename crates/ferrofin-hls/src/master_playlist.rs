//! The HLS **master** playlist (`master.m3u8`) — port of
//! `Jellyfin.Api/Helpers/DynamicHlsHelper.cs` (10.11.8), the
//! `GetMasterPlaylistInternal` assembly and its `AppendPlaylist*` helpers.
//!
//! A pure builder over an already-resolved [`TranscodePlan`]: no I/O, so the
//! exact bytes a client receives are unit-testable against the upstream oracle.
//! The pieces that upstream reads off `HttpContext` (the session token, the
//! peer's locality, the `enableAdaptiveBitrateStreaming`/`enableTrickplay`
//! flags) arrive on the [`HlsStreamRequest`]; the trickplay resolutions the
//! upstream helper fetches from `ITrickplayManager` arrive in a
//! [`MasterPlaylistContext`].
//!
//! Deliberate fidelity notes:
//! - No `#EXT-X-VERSION` line: upstream's master playlist never emits one (only
//!   `CreateMainPlaylist` does, per container).
//! - `BANDWIDTH` is `OutputAudioBitrate + OutputVideoBitrate` with **no floor
//!   and no media-source fallback** — exactly `AppendPlaylist`'s `totalBitrate`.
//! - The SDR/adaptive variant URLs are re-serialised through a port of
//!   `QueryHelpers.ParseQuery` + `AddQueryString` (case-insensitive keys that
//!   keep their original spelling, `UrlEncoder.Default` percent-encoding), and
//!   the variants share ONE mutable query dictionary just as the C# does, so a
//!   later variant inherits the keys an earlier one set.
//! - `IsDoviRemoved`/`IsHdr10PlusRemoved` are constant `false`: Ferrofin's
//!   remux has no bitstream-filter metadata removal, so a copied stream keeps
//!   (and advertises) its dynamic HDR metadata.

use std::fmt::Write as _;

use ferrofin_mediaencoding::EncodingJobInfo;
use ferrofin_mediaencoding::encoding_helper::helper::normalize_transcoding_level;
use ferrofin_model::data::{VideoRange, VideoRangeType};
use ferrofin_model::dlna::SubtitleDeliveryMethod;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::media_encoding::HlsStreamRequest;

use crate::hls_codec_strings;
use crate::hls_stream_manager::TranscodePlan;

/// The `SegmentLength` the subtitle playlist URIs advertise
/// (`DynamicHlsHelper.AddSubtitles`' literal `30`).
const SUBTITLE_SEGMENT_LENGTH: i32 = 30;

/// The default level the H.264 `CODECS` entry assumes for a re-encode with no
/// requested level (`GetOutputVideoCodecLevel`'s `?? "41"`).
const DEFAULT_H264_LEVEL: &str = "41";

/// The default level the HEVC `CODECS` entry assumes for a re-encode with no
/// requested level (`GetOutputVideoCodecLevel`'s `?? "120"`).
const DEFAULT_HEVC_LEVEL: &str = "120";

/// The default level the AV1 `CODECS` entry assumes for a re-encode with no
/// requested level (`GetOutputVideoCodecLevel`'s `?? "19"`).
const DEFAULT_AV1_LEVEL: &str = "19";

/// The HEVC level above which a stream-copied SDR HEVC source gets a duplicate
/// variant advertising level 5.0 (`GetMasterPlaylistInternal`'s `Level > 150`
/// / `Level = 150` compatibility entrance for e.g. Apple A10 chips).
const HEVC_COMPAT_LEVEL: f64 = 150.0;

/// The framerate assumed for a VP9 remux whose stream has no reference
/// framerate (`GetPlaylistVideoCodecs`' `ReferenceFrameRate ?? 30`).
const DEFAULT_VP9_FRAMERATE: f32 = 30.0;

/// One trickplay tile resolution available for the media source, as the
/// master playlist advertises it (`TrickplayInfo` width/height/bandwidth,
/// keyed by the tile width).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrickplayResolution {
    /// The thumbnail width — the dictionary key upstream, and the `{width}` in
    /// the `Trickplay/{width}/tiles.m3u8` URI.
    pub width: i32,
    /// The thumbnail height (`RESOLUTION={width}x{height}`).
    pub height: i32,
    /// The tile playlist's advertised `BANDWIDTH`.
    pub bandwidth: i32,
}

/// The request-independent inputs the master playlist needs beyond the plan.
#[derive(Debug, Clone, Default)]
pub struct MasterPlaylistContext {
    /// The trickplay resolutions available for the media source
    /// (`ITrickplayManager.GetTrickplayResolutions`). Emitted sorted by width.
    pub trickplay_resolutions: Vec<TrickplayResolution>,
}

/// The decoded `(key, value)` pairs of a raw query string, in order — the
/// flattened view of `QueryHelpers.ParseQuery` (a leading `?` is ignored, a
/// bare key has the value `""`, `+` and `%XX` are decoded in both halves).
///
/// Shared with the planner's `ParseStreamOptions` port so both read the query
/// the way ASP.NET's `IQueryCollection` presents it.
#[must_use]
pub fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .trim_start_matches('?')
        .split('&')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let (key, value) = segment.split_once('=').unwrap_or((segment, ""));
            (url_decode(key), url_decode(value))
        })
        .collect()
}

/// An ordered query dictionary with case-insensitive keys — the shape of
/// `QueryHelpers.ParseQuery`'s `Dictionary<string, StringValues>` (insertion
/// order, `OrdinalIgnoreCase` comparer, a key set through the indexer keeps its
/// original spelling and position).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ParsedQuery(Vec<(String, Vec<String>)>);

impl ParsedQuery {
    /// Port of `QueryHelpers.ParseQuery`: strips a leading `?`, splits on `&`,
    /// splits each pair at its first `=` (a bare key has the value `""`), and
    /// decodes `+` → space and `%XX` in both halves. Empty segments are skipped.
    fn parse(query: &str) -> Self {
        let mut parsed = Self::default();
        for (key, value) in query_pairs(query) {
            parsed.append(&key, value);
        }
        parsed
    }

    /// Adds `value` to `key`'s values (the accumulator's `Append`), creating
    /// the key at the end when absent.
    fn append(&mut self, key: &str, value: String) {
        if let Some((_, values)) = self.0.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
            values.push(value);
        } else {
            self.0.push((key.to_owned(), vec![value]));
        }
    }

    /// `Request.Query[key].ToString()`: the values joined with `,`, or `""`
    /// when absent (`StringValues.Empty`).
    fn joined(&self, key: &str) -> String {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, values)| values.join(","))
            .unwrap_or_default()
    }

    /// The dictionary indexer set: replaces `key`'s values (keeping the
    /// existing spelling and position) or appends a new key. `None` is the
    /// null `StringValues` — the key stays but serialises to nothing.
    fn set(&mut self, key: &str, value: Option<&str>) {
        let values = value.map(|v| vec![v.to_owned()]).unwrap_or_default();
        if let Some((_, existing)) = self.0.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
            *existing = values;
        } else {
            self.0.push((key.to_owned(), values));
        }
    }

    /// Port of `QueryHelpers.AddQueryString(uri, IEnumerable<KeyValuePair<
    /// string, StringValues>>)`: every value of every key, percent-encoded with
    /// `UrlEncoder.Default`, `?`-joined onto `uri` (or `&`-joined once `uri`
    /// already carries a `?`).
    fn add_query_string(&self, uri: &str) -> String {
        let mut out = uri.to_owned();
        let mut has_query = uri.contains('?');
        for (key, values) in &self.0 {
            for value in values {
                out.push(if has_query { '&' } else { '?' });
                out.push_str(&url_encode(key));
                out.push('=');
                out.push_str(&url_encode(value));
                has_query = true;
            }
        }
        out
    }
}

/// `Uri.UnescapeDataString(value.Replace('+', ' '))`: decodes `+` to a space
/// and runs of `%XX` escapes to the UTF-8 text they spell; a malformed escape,
/// or a run that is not valid UTF-8, is left escaped exactly as .NET does.
fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' => {
                // Gather the whole `%XX%XX…` run, so a multi-byte character
                // decodes as one unit and an invalid run stays escaped.
                let start = i;
                let mut run = Vec::new();
                while i + 2 < bytes.len() + 1
                    && bytes[i] == b'%'
                    && let Some(byte) = std::str::from_utf8(&bytes[i + 1..(i + 3).min(bytes.len())])
                        .ok()
                        .filter(|h| h.len() == 2)
                        .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    run.push(byte);
                    i += 3;
                }
                if run.is_empty() {
                    // A lone `%` with no hex pair after it.
                    out.push('%');
                    i += 1;
                } else if let Ok(text) = std::str::from_utf8(&run) {
                    out.push_str(text);
                } else {
                    out.push_str(&value[start..i]);
                }
            }
            _ => {
                // Copy the next whole character (the input is valid UTF-8).
                let ch = value[i..].chars().next().unwrap_or('\u{FFFD}');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// `UrlEncoder.Default.Encode`: ASCII letters and digits, the RFC 3986
/// unreserved `-` `_` `.` `~`, and the sub-delims .NET's `DefaultUrlEncoder`
/// leaves alone (`!` `$` `(` `)` `*` `,` `;` `@`) pass through; every other
/// byte of the UTF-8 encoding becomes upper-case `%XX`. (It forbids the
/// query-significant `&` `'` `+` `=` `#` and the gen-delims, so a comma list
/// such as `VideoCodec=hevc,h264` survives the re-serialisation verbatim.)
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'~' | b'!' | b'$' | b'(' | b')' | b'*' | b',' | b';' | b'@'
            )
        {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Whether `codec` is the stream-copy sentinel (`EncodingHelper.IsCopyCodec`).
fn is_copy(codec: Option<&str>) -> bool {
    EncodingJobInfo::is_copy_codec(codec)
}

/// `EncodingHelper.IsDoviWithHdr10Bl`.
fn is_dovi_with_hdr10_bl(stream: Option<&MediaStream>) -> bool {
    stream.is_some_and(|s| {
        matches!(
            s.video_range_type(),
            VideoRangeType::DoviWithHdr10
                | VideoRangeType::DoviWithEl
                | VideoRangeType::DoviWithHdr10Plus
                | VideoRangeType::DoviWithElhdr10Plus
                | VideoRangeType::DoviInvalid
        )
    })
}

/// `EncodingHelper.IsDovi`.
fn is_dovi(stream: Option<&MediaStream>) -> bool {
    is_dovi_with_hdr10_bl(stream)
        || stream.is_some_and(|s| {
            matches!(
                s.video_range_type(),
                VideoRangeType::Dovi | VideoRangeType::DoviWithHlg | VideoRangeType::DoviWithSdr
            )
        })
}

/// `EncodingHelper.IsHdr10Plus`.
fn is_hdr10_plus(stream: Option<&MediaStream>) -> bool {
    stream.is_some_and(|s| {
        matches!(
            s.video_range_type(),
            VideoRangeType::Hdr10Plus
                | VideoRangeType::DoviWithHdr10Plus
                | VideoRangeType::DoviWithElhdr10Plus
        )
    })
}

/// Builds the master playlist text for `plan` + `request`.
///
/// Port of `DynamicHlsHelper.GetMasterPlaylistInternal` after
/// `GetStreamingState` (which is the [`TranscodePlan`]): the `#EXTM3U` header,
/// the optional subtitle `#EXT-X-MEDIA` group, the main `#EXT-X-STREAM-INF`
/// variant, the SDR compatibility variants for an HDR stream copy, the HEVC
/// level-5.0 duplicate, the two adaptive-bitrate variants, and the trickplay
/// `#EXT-X-IMAGE-STREAM-INF` entries.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one linear port of GetMasterPlaylistInternal; splitting it would scatter the shared query/state mutations"
)]
pub fn build_master_playlist(
    plan: &TranscodePlan,
    request: &HlsStreamRequest,
    ctx: &MasterPlaylistContext,
) -> String {
    // The C# mutates the state in place for the compatibility variants (it
    // swaps OutputVideoCodec / VideoStream.Level and restores them); a local
    // copy keeps the plan itself untouched.
    let mut state = plan.state.clone();
    let total_bitrate = state
        .output_audio_bitrate
        .unwrap_or(0)
        .saturating_add(state.output_video_bitrate.unwrap_or(0));

    let mut builder = String::new();
    builder.push_str("#EXTM3U\n");

    let is_live_stream = state.is_segmented_live_stream();
    // `state.VideoRequest is not null` / `IsOutputVideo`: a request on a
    // video route. The planner sets `is_input_video = !is_audio` from the
    // route, so it carries exactly that meaning here (not the item's media
    // type, which upstream's own `IsInputVideo` tracks separately).
    let is_video_request = state.is_input_video;

    let mut query_string = request.query_string.clone();

    // from universal audio service, need to override the AudioCodec when the
    // actual request differs from original query
    let requested_audio_codec = ParsedQuery::parse(&query_string).joined("AudioCodec");
    let audio_codec_matches = state
        .output_audio_codec
        .as_deref()
        .is_some_and(|c| c.eq_ignore_ascii_case(&requested_audio_codec));
    if !audio_codec_matches {
        let mut new_query = ParsedQuery::parse(&query_string);
        new_query.set("AudioCodec", state.output_audio_codec.as_deref());
        query_string = new_query.add_query_string("");
    }

    // from universal audio service
    if let Some(container) = request
        .segment_container
        .as_deref()
        .filter(|c| !c.trim().is_empty())
        && !contains_ignore_ascii_case(&query_string, "SegmentContainer")
    {
        let _ = write!(query_string, "&SegmentContainer={container}");
    }

    // from universal audio service
    if let Some(reasons) = request
        .transcode_reasons
        .as_deref()
        .filter(|r| !r.trim().is_empty())
        && !contains_ignore_ascii_case(&query_string, "TranscodeReasons=")
    {
        let _ = write!(query_string, "&TranscodeReasons={reasons}");
    }

    // Video rotation metadata is only supported in fMP4 remuxing
    if let Some(video_stream) = state.video_stream.as_ref()
        && is_video_request
        && video_stream.rotation.unwrap_or(0) != 0
        && is_copy(state.output_video_codec.as_deref())
        && let Some(container) = request
            .segment_container
            .as_deref()
            .filter(|c| !c.trim().is_empty())
        && !container.eq_ignore_ascii_case("mp4")
    {
        query_string.push_str("&AllowVideoStreamCopy=false");
    }

    // Main stream
    let base_url = if is_live_stream {
        "live.m3u8"
    } else {
        "main.m3u8"
    };
    let playlist_url = format!("{base_url}{query_string}");
    // ONE shared dictionary for every derived variant, as upstream's
    // `sdrPlaylistQuery = playlistQuery` / `variantQuery = playlistQuery` are
    // reference copies.
    let mut playlist_query = ParsedQuery::parse(&query_string);

    let subtitle_streams: Vec<&MediaStream> = state
        .media_source
        .media_streams
        .iter()
        .filter(|s| s.is_text_subtitle_stream())
        .collect();

    let mut subtitle_group = (!subtitle_streams.is_empty()
        && (state.subtitle_delivery_method == SubtitleDeliveryMethod::Hls
            || request.enable_subtitles_in_manifest))
        .then_some("subs");

    // If we're burning in subtitles then don't add additional subs to the manifest
    if state.subtitle_stream.is_some()
        && state.subtitle_delivery_method == SubtitleDeliveryMethod::Encode
    {
        subtitle_group = None;
    }

    if subtitle_group.is_some() {
        add_subtitles(&state, &subtitle_streams, &mut builder, request);
    }

    let basic_playlist = append_playlist(
        &mut builder,
        &state,
        &playlist_url,
        total_bitrate,
        subtitle_group,
    );

    if state.video_stream.is_some() && is_video_request {
        let encoding_options = &plan.encoding_options;

        // Provide AV1 and HEVC SDR entrances for backward compatibility.
        for sdr_video_codec in ["av1", "hevc"] {
            let actual = state.actual_output_video_codec().unwrap_or_default();
            let is_av1_encoding_allowed = encoding_options.allow_av1_encoding
                && sdr_video_codec.eq_ignore_ascii_case("av1")
                && actual.eq_ignore_ascii_case("av1");
            let is_hevc_encoding_allowed = encoding_options.allow_hevc_encoding
                && sdr_video_codec.eq_ignore_ascii_case("hevc")
                && actual.eq_ignore_ascii_case("hevc");
            let is_encoding_allowed = is_av1_encoding_allowed || is_hevc_encoding_allowed;

            if is_encoding_allowed
                && is_copy(state.output_video_codec.as_deref())
                && state
                    .video_stream
                    .as_ref()
                    .is_some_and(|v| v.video_range() == VideoRange::Hdr)
            {
                // Force AV1 and HEVC Main Profile and disable video stream copy.
                state.output_video_codec = Some(sdr_video_codec.to_owned());

                playlist_query.set("VideoCodec", Some(sdr_video_codec));
                playlist_query.set(&format!("{sdr_video_codec}-profile"), Some("main"));
                playlist_query.set("AllowVideoStreamCopy", Some("false"));

                let sdr_video_url = playlist_query.add_query_string(base_url);

                // HACK: Use the same bitrate so that the client can choose by
                // other attributes, such as color range.
                append_playlist(
                    &mut builder,
                    &state,
                    &sdr_video_url,
                    total_bitrate,
                    subtitle_group,
                );

                // Restore the video codec
                state.output_video_codec = Some("copy".to_owned());
            }
        }

        // Provide H.264 SDR entrance for backward compatibility.
        if is_copy(state.output_video_codec.as_deref())
            && state
                .video_stream
                .as_ref()
                .is_some_and(|v| v.video_range() == VideoRange::Hdr)
        {
            // Force H.264 and disable video stream copy.
            state.output_video_codec = Some("h264".to_owned());

            playlist_query.set("VideoCodec", Some("h264"));
            playlist_query.set("AllowVideoStreamCopy", Some("false"));

            let sdr_video_url = playlist_query.add_query_string(base_url);

            // HACK: Use the same bitrate so that the client can choose by other
            // attributes, such as color range.
            append_playlist(
                &mut builder,
                &state,
                &sdr_video_url,
                total_bitrate,
                subtitle_group,
            );

            // Restore the video codec
            state.output_video_codec = Some("copy".to_owned());
        }

        // Provide Level 5.0 entrance for backward compatibility.
        // e.g. Apple A10 chips refuse the master playlist containing SDR HEVC
        // Main Level 5.1 video, but in fact it is capable of playing videos up
        // to Level 6.1.
        let needs_level_50_entrance = is_copy(state.output_video_codec.as_deref())
            && state.video_stream.as_ref().is_some_and(|v| {
                v.level.is_some_and(|l| l > HEVC_COMPAT_LEVEL) && v.video_range() == VideoRange::Sdr
            })
            && state
                .actual_output_video_codec()
                .is_some_and(|c| c.eq_ignore_ascii_case("hevc"));
        if needs_level_50_entrance {
            let mut playlist_codecs_field = String::new();
            append_playlist_codecs_field(&mut playlist_codecs_field, &state);

            // Force the video level to 5.0.
            let original_level = state.video_stream.as_ref().and_then(|v| v.level);
            if let Some(v) = state.video_stream.as_mut() {
                v.level = Some(HEVC_COMPAT_LEVEL);
            }
            let mut new_playlist_codecs_field = String::new();
            append_playlist_codecs_field(&mut new_playlist_codecs_field, &state);

            // Restore the video level.
            if let Some(v) = state.video_stream.as_mut() {
                v.level = original_level;
            }
            let new_playlist = replace_playlist_codecs_field(
                &basic_playlist,
                &playlist_codecs_field,
                &new_playlist_codecs_field,
            );
            builder.push_str(&new_playlist);
        }
    }

    if enable_adaptive_bitrate_streaming(&state, plan, request, is_live_stream, is_video_request) {
        let requested_video_bitrate = request.video_bitrate.unwrap_or(0);

        // By default, vary by just 200k
        let mut variation = bitrate_variation(total_bitrate);

        let mut new_bitrate = total_bitrate.saturating_sub(variation);
        playlist_query.set(
            "VideoBitrate",
            Some(
                &requested_video_bitrate
                    .saturating_sub(variation)
                    .to_string(),
            ),
        );
        let variant_url = playlist_query.add_query_string(base_url);
        append_playlist(
            &mut builder,
            &state,
            &variant_url,
            new_bitrate,
            subtitle_group,
        );

        variation = variation.saturating_mul(2);
        new_bitrate = total_bitrate.saturating_sub(variation);
        playlist_query.set(
            "VideoBitrate",
            Some(
                &requested_video_bitrate
                    .saturating_sub(variation)
                    .to_string(),
            ),
        );
        let variant_url = playlist_query.add_query_string(base_url);
        append_playlist(
            &mut builder,
            &state,
            &variant_url,
            new_bitrate,
            subtitle_group,
        );
    }

    if !is_live_stream && request.enable_trickplay {
        add_trickplay(&ctx.trickplay_resolutions, &mut builder, request);
    }

    builder
}

/// Case-insensitive substring test (`string.Contains(…, OrdinalIgnoreCase)`).
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// Port of `AppendPlaylist`: appends one `#EXT-X-STREAM-INF` entry + its URL
/// to `builder` and returns that entry on its own (the HEVC level-5.0 duplicate
/// is made by string-replacing the `CODECS` field inside it).
fn append_playlist(
    builder: &mut String,
    state: &EncodingJobInfo,
    url: &str,
    bitrate: i32,
    subtitle_group: Option<&str>,
) -> String {
    let mut playlist_builder = String::new();
    let _ = write!(
        playlist_builder,
        "#EXT-X-STREAM-INF:BANDWIDTH={bitrate},AVERAGE-BANDWIDTH={bitrate}"
    );

    append_playlist_video_range_field(&mut playlist_builder, state);
    append_playlist_codecs_field(&mut playlist_builder, state);
    append_playlist_supplemental_codecs_field(&mut playlist_builder, state);
    append_playlist_resolution_field(&mut playlist_builder, state);
    append_playlist_framerate_field(&mut playlist_builder, state);

    if let Some(group) = subtitle_group.filter(|g| !g.trim().is_empty()) {
        let _ = write!(playlist_builder, ",SUBTITLES=\"{group}\"");
    }

    playlist_builder.push('\n');
    playlist_builder.push_str(url);
    playlist_builder.push('\n');
    builder.push_str(&playlist_builder);

    playlist_builder
}

/// Appends a `VIDEO-RANGE` field containing the range of the output video
/// stream (`AppendPlaylistVideoRangeField`).
fn append_playlist_video_range_field(builder: &mut String, state: &EncodingJobInfo) {
    let Some(video_stream) = state.video_stream.as_ref() else {
        return;
    };
    let video_range = video_stream.video_range();
    if video_range == VideoRange::Unknown {
        return;
    }
    if is_copy(state.output_video_codec.as_deref()) {
        match video_range {
            VideoRange::Sdr => builder.push_str(",VIDEO-RANGE=SDR"),
            VideoRange::Hdr => match video_stream.video_range_type() {
                VideoRangeType::Hlg | VideoRangeType::DoviWithHlg => {
                    builder.push_str(",VIDEO-RANGE=HLG");
                }
                _ => builder.push_str(",VIDEO-RANGE=PQ"),
            },
            VideoRange::Unknown => {}
        }
    } else {
        // Currently we only encode to SDR.
        builder.push_str(",VIDEO-RANGE=SDR");
    }
}

/// Appends a `CODECS` field containing formatted strings of the active
/// streams' output video and audio codecs (`AppendPlaylistCodecsField`).
fn append_playlist_codecs_field(builder: &mut String, state: &EncodingJobInfo) {
    // Video
    let mut video_codecs = String::new();
    let video_codec_level = output_video_codec_level(state);
    if let Some(actual) = state.actual_output_video_codec().filter(|c| !c.is_empty())
        && let Some(level) = video_codec_level
    {
        video_codecs = playlist_video_codecs(state, actual, level);
    }

    // Audio
    let mut audio_codecs = String::new();
    if state
        .actual_output_audio_codec()
        .is_some_and(|c| !c.is_empty())
    {
        audio_codecs = playlist_audio_codecs(state);
    }

    let mut codecs = String::new();
    codecs.push_str(&video_codecs);
    if !video_codecs.is_empty() && !audio_codecs.is_empty() {
        codecs.push(',');
    }
    codecs.push_str(&audio_codecs);

    // Upstream's literal `codecs.Length > 1`.
    if codecs.len() > 1 {
        let _ = write!(builder, ",CODECS=\"{codecs}\"");
    }
}

/// Appends a `SUPPLEMENTAL-CODECS` field for a stream-copied Dolby Vision /
/// HDR10+ video (`AppendPlaylistSupplementalCodecsField`).
fn append_playlist_supplemental_codecs_field(builder: &mut String, state: &EncodingJobInfo) {
    // HDR dynamic metadata currently cannot exist when transcoding
    if !is_copy(state.output_video_codec.as_deref()) {
        return;
    }

    // Ferrofin's remux never strips dynamic HDR metadata (no bitstream-filter
    // removal), so upstream's `IsDoviRemoved` / `IsHdr10PlusRemoved` are
    // constant `false` here.
    let video_stream = state.video_stream.as_ref();
    if is_dovi(video_stream) {
        append_dv_string(builder, state);
    } else if is_hdr10_plus(video_stream) {
        append_hdr10_plus_string(builder, state);
    }
}

/// The Dolby Vision `SUPPLEMENTAL-CODECS` entry
/// (`AppendPlaylistSupplementalCodecsField.AppendDvString`).
fn append_dv_string(builder: &mut String, state: &EncodingJobInfo) {
    let Some(video_stream) = state.video_stream.as_ref() else {
        return;
    };
    // Upstream's switch lists DOVIWithHDR10 and DOVIWithHDR10Plus as separate
    // arms (the HDR10+ metadata would be removed if the Dovi metadata is not);
    // kept verbatim.
    #[allow(clippy::match_same_arms, reason = "verbatim upstream range table")]
    let dv_range_string = match video_stream.video_range_type() {
        VideoRangeType::DoviWithHdr10 => "db1p",
        VideoRangeType::DoviWithHlg => "db4h",
        VideoRangeType::DoviWithHdr10Plus => "db1p",
        // Don't label Dovi with EL and SDR due to compatability issues, ignore
        // invalid configurations
        _ => "",
    };

    let (Some(dv_profile), Some(dv_level)) = (video_stream.dv_profile, video_stream.dv_level)
    else {
        return;
    };
    if dv_range_string.is_empty() {
        return;
    }

    let dv_four_cc = if state
        .actual_output_video_codec()
        .is_some_and(|c| c.eq_ignore_ascii_case("av1"))
    {
        "dav1"
    } else {
        "dvh1"
    };
    let _ = write!(
        builder,
        ",SUPPLEMENTAL-CODECS=\"{dv_four_cc}.{dv_profile:02}.{dv_level:02}/{dv_range_string}\""
    );
}

/// The HDR10+ `SUPPLEMENTAL-CODECS` entry
/// (`AppendPlaylistSupplementalCodecsField.AppendHdr10PlusString`).
fn append_hdr10_plus_string(builder: &mut String, state: &EncodingJobInfo) {
    let video_codec_level = output_video_codec_level(state);
    let (Some(actual), Some(level)) = (
        state.actual_output_video_codec().filter(|c| !c.is_empty()),
        video_codec_level,
    ) else {
        return;
    };
    let video_codec_string = playlist_video_codecs(state, actual, level);
    let _ = write!(
        builder,
        ",SUPPLEMENTAL-CODECS=\"{video_codec_string}/cdm4\""
    );
}

/// Appends a `RESOLUTION` field containing the resolution of the output stream
/// (`AppendPlaylistResolutionField`).
fn append_playlist_resolution_field(builder: &mut String, state: &EncodingJobInfo) {
    if let (Some(width), Some(height)) = (state.output_width(), state.output_height()) {
        let _ = write!(builder, ",RESOLUTION={width}x{height}");
    }
}

/// Appends a `FRAME-RATE` field containing the framerate of the output stream
/// (`AppendPlaylistFramerateField`): the target framerate, else the source's
/// real framerate, rounded to 3 decimals (`Math.Round(double, 3)`, ties to
/// even) and printed like `double.ToString(InvariantCulture)` (`10`, `23.976`).
fn append_playlist_framerate_field(builder: &mut String, state: &EncodingJobInfo) {
    let framerate = state
        .target_framerate()
        .or_else(|| state.video_stream.as_ref().and_then(|v| v.real_frame_rate))
        .map(|f| (f64::from(f) * 1000.0).round_ties_even() / 1000.0);
    if let Some(framerate) = framerate {
        let _ = write!(builder, ",FRAME-RATE={framerate}");
    }
}

/// Port of `EnableAdaptiveBitrateStreaming`: whether the two lower-bitrate
/// variants are added.
fn enable_adaptive_bitrate_streaming(
    state: &EncodingJobInfo,
    plan: &TranscodePlan,
    request: &HlsStreamRequest,
    is_live_stream: bool,
    is_output_video: bool,
) -> bool {
    // Within the local network this will likely do more harm than good.
    if request.is_in_local_network {
        return false;
    }

    if !request.enable_adaptive_bitrate_streaming {
        return false;
    }

    if is_live_stream || plan.media_path.trim().is_empty() {
        // Opening live streams is so slow it's not even worth it
        return false;
    }

    if is_copy(state.output_video_codec.as_deref()) {
        return false;
    }

    if is_copy(state.output_audio_codec.as_deref()) {
        return false;
    }

    if !is_output_video {
        return false;
    }

    request.video_bitrate.is_some()
}

/// Port of `AddSubtitles`: one `#EXT-X-MEDIA:TYPE=SUBTITLES` line per text
/// subtitle stream, `DEFAULT=YES` only for the HLS-delivered selected stream.
fn add_subtitles(
    state: &EncodingJobInfo,
    subtitles: &[&MediaStream],
    builder: &mut String,
    request: &HlsStreamRequest,
) {
    if state.subtitle_delivery_method == SubtitleDeliveryMethod::Drop {
        return;
    }

    let selected_index = match state.subtitle_stream.as_ref() {
        Some(sub) if state.subtitle_delivery_method == SubtitleDeliveryMethod::Hls => {
            Some(sub.index)
        }
        _ => None,
    };
    let media_source_id = request.media_source_id.as_deref().unwrap_or_default();
    let token = request.api_key.as_deref().unwrap_or_default();

    for stream in subtitles {
        // `MediaStream.DisplayTitle` is a computed getter upstream; the stored
        // DTO slot is the same value when it was materialised.
        let name = stream
            .display_title()
            .or_else(|| stream.display_title.clone())
            .unwrap_or_default();

        let is_default = selected_index == Some(stream.index);
        let is_forced = stream.is_forced;

        let url = format!(
            "{media_source_id}/Subtitles/{}/subtitles.m3u8?SegmentLength={SUBTITLE_SEGMENT_LENGTH}&ApiKey={token}",
            stream.index
        );

        let _ = writeln!(
            builder,
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{name}\",DEFAULT={},FORCED={},AUTOSELECT=YES,URI=\"{url}\",LANGUAGE=\"{}\"",
            if is_default { "YES" } else { "NO" },
            if is_forced { "YES" } else { "NO" },
            stream.language.as_deref().unwrap_or("Unknown"),
        );
    }
}

/// Port of `AddTrickplay`: one `#EXT-X-IMAGE-STREAM-INF` line per trickplay
/// resolution, in ascending width order.
fn add_trickplay(
    resolutions: &[TrickplayResolution],
    builder: &mut String,
    request: &HlsStreamRequest,
) {
    let mut sorted: Vec<&TrickplayResolution> = resolutions.iter().collect();
    sorted.sort_by_key(|r| r.width);
    let media_source_id = request.media_source_id.as_deref().unwrap_or_default();
    let token = request.api_key.as_deref().unwrap_or_default();

    for resolution in sorted {
        let url = format!(
            "Trickplay/{}/tiles.m3u8?MediaSourceId={media_source_id}&ApiKey={token}",
            resolution.width
        );
        let _ = writeln!(
            builder,
            "#EXT-X-IMAGE-STREAM-INF:BANDWIDTH={},RESOLUTION={}x{},CODECS=\"jpeg\",URI=\"{url}\"",
            resolution.bandwidth, resolution.width, resolution.height
        );
    }
}

/// Get the H.26X level of the output video stream (`GetOutputVideoCodecLevel`).
///
/// A stream copy reports the source level formatted as a double (`41` parses,
/// `4.1` does not — faithfully `None`); a re-encode uses the requested level
/// (or the per-codec default) through `NormalizeTranscodingLevel`.
fn output_video_codec_level(state: &EncodingJobInfo) -> Option<i32> {
    let mut level_string = String::new();
    if is_copy(state.output_video_codec.as_deref())
        && let Some(level) = state.video_stream.as_ref().and_then(|v| v.level)
    {
        // `double.ToString(InvariantCulture)`: `41` for 41.0, `4.1` for 4.1.
        level_string = level.to_string();
    } else {
        let actual = state.actual_output_video_codec().unwrap_or_default();

        if actual.eq_ignore_ascii_case("h264") {
            let requested = state
                .requested_level(actual)
                .unwrap_or_else(|| DEFAULT_H264_LEVEL.to_owned());
            level_string = normalize_transcoding_level(state, Some(&requested)).unwrap_or_default();
        }

        if actual.eq_ignore_ascii_case("h265") || actual.eq_ignore_ascii_case("hevc") {
            let requested = state
                .requested_level("h265")
                .or_else(|| state.requested_level("hevc"))
                .unwrap_or_else(|| DEFAULT_HEVC_LEVEL.to_owned());
            level_string = normalize_transcoding_level(state, Some(&requested)).unwrap_or_default();
        }

        if actual.eq_ignore_ascii_case("av1") {
            let requested = state
                .requested_level("av1")
                .unwrap_or_else(|| DEFAULT_AV1_LEVEL.to_owned());
            level_string = normalize_transcoding_level(state, Some(&requested)).unwrap_or_default();
        }
    }

    // `int.TryParse(levelString, NumberStyles.Integer, InvariantCulture)`.
    level_string.trim().parse::<i32>().ok()
}

/// Get the profile of the output video stream (`GetOutputVideoCodecProfile`).
///
/// Upstream's `profileString ??= "high"/"main"` fallbacks never fire —
/// `FirstOrDefault() ?? string.Empty` is already non-null — so a re-encode
/// with no requested profile yields `""` (and H.264 then advertises
/// constrained baseline `avc1.4240xx`). Ported as-is: that is the oracle.
fn output_video_codec_profile(state: &EncodingJobInfo, codec: &str) -> String {
    if is_copy(state.output_video_codec.as_deref())
        && let Some(profile) = state
            .video_stream
            .as_ref()
            .and_then(|v| v.profile.as_deref())
            .filter(|p| !p.is_empty())
    {
        return profile.to_owned();
    }
    if !codec.is_empty() {
        return state
            .requested_profiles(codec)
            .into_iter()
            .next()
            .unwrap_or_default();
    }
    String::new()
}

/// Gets a formatted string of the output audio codec, for use in the `CODECS`
/// field (`GetPlaylistAudioCodecs`).
fn playlist_audio_codecs(state: &EncodingJobInfo) -> String {
    let actual = state.actual_output_audio_codec().unwrap_or_default();
    if actual.eq_ignore_ascii_case("aac") {
        let profiles = state.requested_profiles("aac");
        return hls_codec_strings::aac_string(profiles.first().map(String::as_str));
    }
    if actual.eq_ignore_ascii_case("mp3") {
        return hls_codec_strings::MP3.to_owned();
    }
    if actual.eq_ignore_ascii_case("ac3") {
        return hls_codec_strings::AC3.to_owned();
    }
    if actual.eq_ignore_ascii_case("eac3") {
        return hls_codec_strings::EAC3.to_owned();
    }
    if actual.eq_ignore_ascii_case("flac") {
        return hls_codec_strings::FLAC.to_owned();
    }
    if actual.eq_ignore_ascii_case("alac") {
        return hls_codec_strings::ALAC.to_owned();
    }
    if actual.eq_ignore_ascii_case("opus") {
        return hls_codec_strings::OPUS.to_owned();
    }
    String::new()
}

/// Gets a formatted string of the output video codec, for use in the `CODECS`
/// field (`GetPlaylistVideoCodecs`).
fn playlist_video_codecs(state: &EncodingJobInfo, codec: &str, level: i32) -> String {
    if level == 0 {
        // This is 0 when there's no requested level in the device profile
        // and the source is not encoded in H.26X or AV1
        tracing::error!("Got invalid level when building CODECS field for HLS master playlist");
        return String::new();
    }

    if codec.eq_ignore_ascii_case("h264") {
        let profile = output_video_codec_profile(state, "h264");
        return hls_codec_strings::h264_string(Some(&profile), level);
    }

    if codec.eq_ignore_ascii_case("h265") || codec.eq_ignore_ascii_case("hevc") {
        let profile = output_video_codec_profile(state, "hevc");
        return hls_codec_strings::h265_string(Some(&profile), level);
    }

    if codec.eq_ignore_ascii_case("av1") {
        let profile = output_video_codec_profile(state, "av1");

        // Currently we only transcode to 8 bits AV1
        let mut bit_depth = 8;
        if is_copy(state.output_video_codec.as_deref())
            && let Some(depth) = state.video_stream.as_ref().and_then(|v| v.bit_depth)
        {
            bit_depth = depth;
        }

        return hls_codec_strings::av1_string(Some(&profile), level, false, bit_depth);
    }

    // VP9 HLS is for video remuxing only, everything is probed from the original video
    if codec.eq_ignore_ascii_case("vp9") {
        let video_stream = state.video_stream.as_ref();
        let width = video_stream.and_then(|v| v.width).unwrap_or(0);
        let height = video_stream.and_then(|v| v.height).unwrap_or(0);
        let framerate = video_stream
            .and_then(MediaStream::reference_frame_rate)
            .unwrap_or(DEFAULT_VP9_FRAMERATE);
        let bit_depth = video_stream.and_then(|v| v.bit_depth).unwrap_or(8);
        return hls_codec_strings::vp9_string(
            width,
            height,
            video_stream.and_then(|v| v.pixel_format.as_deref()),
            framerate,
            bit_depth,
        );
    }

    String::new()
}

/// Port of `GetBitrateVariation`: how far below the main variant each adaptive
/// variant sits, by total bitrate (the table is upstream's verbatim).
fn bitrate_variation(bitrate: i32) -> i32 {
    // By default, vary by just 50k
    if bitrate >= 10_000_000 {
        2_000_000
    } else if bitrate >= 5_000_000 {
        1_500_000
    } else if bitrate >= 3_000_000 {
        1_000_000
    } else if bitrate >= 2_000_000 {
        500_000
    } else if bitrate >= 1_000_000 {
        300_000
    } else if bitrate >= 600_000 {
        200_000
    } else if bitrate >= 400_000 {
        100_000
    } else {
        50_000
    }
}

/// Port of `ReplacePlaylistCodecsField`: `playlist` with `old_value` replaced
/// by `new_value` (ordinal). An empty `old_value` is a no-op (C#'s
/// `string.Replace` would throw; it cannot happen on the only call path, which
/// always has a `CODECS` field).
fn replace_playlist_codecs_field(playlist: &str, old_value: &str, new_value: &str) -> String {
    if old_value.is_empty() {
        return playlist.to_owned();
    }
    playlist.replace(old_value, new_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_mediaencoding::{BaseEncodingJobOptions, TranscodeDisplayNames};
    use ferrofin_model::configuration::EncodingOptions;
    use ferrofin_model::dto::MediaSourceInfo;
    use ferrofin_model::entities::MediaStreamType;
    use ferrofin_traits::media_encoding::TranscodingJobType;
    use std::path::PathBuf;

    /// The parity fixture's query: the order the harness (and jellyfin-web)
    /// sends it in.
    const FIXTURE_QUERY: &str = "?mediaSourceId=abc&deviceId=parity-streams&playSessionId=parity-1&\
         audioCodec=aac&audioBitRate=128000&segmentContainer=ts&videoCodec=h264&\
         videoBitRate=1000000&maxWidth=320&transcodingMaxAudioChannels=2";

    fn video_stream() -> MediaStream {
        MediaStream {
            codec: Some("h264".to_owned()),
            index: 0,
            stream_type: MediaStreamType::Video,
            is_default: true,
            width: Some(320),
            height: Some(240),
            bit_rate: Some(6_000_000),
            average_frame_rate: Some(10.0),
            real_frame_rate: Some(10.0),
            profile: Some("High".to_owned()),
            level: Some(13.0),
            pixel_format: Some("yuv420p".to_owned()),
            bit_depth: Some(8),
            color_transfer: Some("bt709".to_owned()),
            color_primaries: Some("bt709".to_owned()),
            ..MediaStream::default()
        }
    }

    fn audio_stream() -> MediaStream {
        MediaStream {
            codec: Some("aac".to_owned()),
            index: 1,
            stream_type: MediaStreamType::Audio,
            is_default: true,
            channels: Some(1),
            ..MediaStream::default()
        }
    }

    fn text_subtitle(index: i32, language: &str, forced: bool) -> MediaStream {
        MediaStream {
            codec: Some("subrip".to_owned()),
            index,
            stream_type: MediaStreamType::Subtitle,
            language: Some(language.to_owned()),
            is_forced: forced,
            ..MediaStream::default()
        }
    }

    fn state(streams: Vec<MediaStream>) -> EncodingJobInfo {
        EncodingJobInfo {
            display: TranscodeDisplayNames::default(),
            base_request: BaseEncodingJobOptions {
                audio_codec: Some("aac".to_owned()),
                audio_bit_rate: Some(128_000),
                video_bit_rate: Some(1_000_000),
                max_width: Some(320),
                transcoding_max_audio_channels: Some(2),
                ..BaseEncodingJobOptions::default()
            },
            video_stream: streams
                .iter()
                .find(|s| s.stream_type == MediaStreamType::Video)
                .cloned(),
            audio_stream: streams
                .iter()
                .find(|s| s.stream_type == MediaStreamType::Audio)
                .cloned(),
            subtitle_stream: None,
            media_source: MediaSourceInfo {
                id: Some("abc".to_owned()),
                path: Some("/media/fixture.mkv".to_owned()),
                bitrate: Some(6_000_000),
                media_streams: streams,
                ..MediaSourceInfo::default()
            },
            output_video_codec: Some("h264".to_owned()),
            output_audio_codec: Some("aac".to_owned()),
            output_video_bitrate: Some(1_000_000),
            output_audio_bitrate: Some(128_000),
            output_audio_channels: Some(1),
            output_container: Some("ts".to_owned()),
            output_video_sync: None,
            output_file_path: "/cache/transcodes/out.m3u8".to_owned(),
            input_container: Some("mkv".to_owned()),
            is_input_video: true,
            subtitle_delivery_method: SubtitleDeliveryMethod::External,
            run_time_ticks: Some(10_000_000),
            transcoding_type: TranscodingJobType::Hls,
            supported_video_codecs: vec!["h264".to_owned()],
            supported_audio_codecs: vec!["aac".to_owned()],
            segment_length_secs: 3,
            wait_for_path: None,
            segment_container: Some("ts".to_owned()),
            play_session_id: Some("parity-1".to_owned()),
            device_id: Some("parity-streams".to_owned()),
        }
    }

    fn plan(state: EncodingJobInfo) -> TranscodePlan {
        TranscodePlan {
            state,
            playlist_path: PathBuf::from("/cache/transcodes/out.m3u8"),
            arguments: Vec::new(),
            media_path: "/media/fixture.mkv".to_owned(),
            run_time_ticks: 10_000_000,
            segment_length_ms: 3000,
            is_remuxing_video: false,
            segment_container: "ts".to_owned(),
            encoding_options: EncodingOptions::default(),
            min_segments: 3,
        }
    }

    fn request() -> HlsStreamRequest {
        HlsStreamRequest {
            item_id: uuid::Uuid::from_u128(1),
            media_source_id: Some("abc".to_owned()),
            device_id: Some("parity-streams".to_owned()),
            play_session_id: Some("parity-1".to_owned()),
            segment_container: Some("ts".to_owned()),
            audio_codec: Some("aac".to_owned()),
            video_codec: Some("h264".to_owned()),
            audio_bitrate: Some(128_000),
            video_bitrate: Some(1_000_000),
            max_width: Some(320),
            transcoding_max_audio_channels: Some(2),
            api_key: Some("tok".to_owned()),
            query_string: FIXTURE_QUERY.to_owned(),
            ..HlsStreamRequest::default()
        }
    }

    fn fixture_plan() -> TranscodePlan {
        plan(state(vec![video_stream(), audio_stream()]))
    }

    /// The parity oracle: Jellyfin 10.11.8's raw `/Videos/{id}/master.m3u8` for
    /// the 320x240 h264/aac fixture under the harness query — byte for byte.
    #[test]
    fn video_master_matches_the_jellyfin_oracle() {
        let pl = build_master_playlist(
            &fixture_plan(),
            &request(),
            &MasterPlaylistContext::default(),
        );
        let expected = format!(
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=1128000,AVERAGE-BANDWIDTH=1128000,VIDEO-RANGE=SDR,\
             CODECS=\"avc1.424029,mp4a.40.2\",RESOLUTION=320x240,FRAME-RATE=10\n\
             main.m3u8{FIXTURE_QUERY}\n"
        );
        assert_eq!(pl, expected);
    }

    /// `/Audio/{id}/master.m3u8`: flac mono → aac 128k, no video fields.
    #[test]
    fn audio_master_matches_the_jellyfin_oracle() {
        let mut audio = audio_stream();
        audio.codec = Some("flac".to_owned());
        let mut s = state(vec![audio]);
        s.is_input_video = false;
        s.output_video_codec = None;
        s.output_video_bitrate = None;
        let query = "?mediaSourceId=abc&deviceId=parity-streams&playSessionId=parity-1&\
                     audioCodec=aac&audioBitRate=128000&segmentContainer=ts";
        let req = HlsStreamRequest {
            video_codec: None,
            video_bitrate: None,
            max_width: None,
            query_string: query.to_owned(),
            ..request()
        };
        let pl = build_master_playlist(&plan(s), &req, &MasterPlaylistContext::default());
        assert_eq!(
            pl,
            format!(
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000,AVERAGE-BANDWIDTH=128000,\
                 CODECS=\"mp4a.40.2\"\nmain.m3u8{query}\n"
            )
        );
    }

    /// A fake plan with neither bitrate gives BANDWIDTH=0 — upstream has no
    /// floor and no media-source fallback.
    #[test]
    fn bandwidth_is_the_plain_sum_with_no_floor() {
        let mut s = state(vec![video_stream(), audio_stream()]);
        s.output_video_bitrate = None;
        s.output_audio_bitrate = None;
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(pl.contains("BANDWIDTH=0,AVERAGE-BANDWIDTH=0,"), "got: {pl}");
        assert!(!pl.contains("#EXT-X-VERSION"), "got: {pl}");
    }

    /// A stream copy reports the source level as a double: `4.1` does not
    /// parse as an int, so no video CODECS entry (faithful quirk).
    #[test]
    fn copy_with_fractional_level_drops_the_video_codecs() {
        let mut video = video_stream();
        video.level = Some(4.1);
        let mut s = state(vec![video, audio_stream()]);
        s.output_video_codec = Some("copy".to_owned());
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(pl.contains("CODECS=\"mp4a.40.2\""), "got: {pl}");
        assert!(!pl.contains("avc1"), "got: {pl}");

        // An integral source level (41.0 → "41") does parse; the copy keeps the
        // source profile (High) and its reference framerate.
        let mut video = video_stream();
        video.level = Some(41.0);
        let mut s = state(vec![video, audio_stream()]);
        s.output_video_codec = Some("copy".to_owned());
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(pl.contains("CODECS=\"avc1.640029,mp4a.40.2\""), "got: {pl}");
    }

    /// Re-encode with a requested `h264-profile`/`h264-level` stream option.
    #[test]
    fn requested_profile_and_level_drive_the_codecs_entry() {
        let mut s = state(vec![video_stream(), audio_stream()]);
        s.base_request.stream_options = vec![
            ("h264-profile".to_owned(), "high".to_owned()),
            ("h264-level".to_owned(), "51".to_owned()),
            ("aac-profile".to_owned(), "HE".to_owned()),
        ];
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(pl.contains("CODECS=\"avc1.640033,mp4a.40.5\""), "got: {pl}");
        // An over-limit level is clamped by NormalizeTranscodingLevel (h264 → 51).
        let mut s = state(vec![video_stream(), audio_stream()]);
        s.base_request.level = Some("62".to_owned());
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(pl.contains("avc1.424033"), "got: {pl}");
    }

    fn hdr_hevc_stream() -> MediaStream {
        MediaStream {
            codec: Some("hevc".to_owned()),
            width: Some(3840),
            height: Some(2160),
            profile: Some("Main 10".to_owned()),
            level: Some(153.0),
            bit_depth: Some(10),
            color_transfer: Some("smpte2084".to_owned()),
            color_primaries: Some("bt2020".to_owned()),
            average_frame_rate: Some(23.976),
            real_frame_rate: Some(23.976),
            ..video_stream()
        }
    }

    /// Copying an HDR source adds the H.264 SDR entrance with
    /// `VideoCodec=h264&AllowVideoStreamCopy=false` re-serialised through
    /// the parsed query (same bitrate, SDR range, re-encode CODECS).
    #[test]
    fn hdr_copy_adds_the_h264_sdr_variant() {
        let mut s = state(vec![hdr_hevc_stream(), audio_stream()]);
        s.output_video_codec = Some("copy".to_owned());
        s.base_request.max_width = None;
        let req = HlsStreamRequest {
            query_string:
                "?mediaSourceId=abc&videoCodec=hevc,h264&audioCodec=aac&segmentContainer=ts"
                    .to_owned(),
            ..request()
        };
        let pl = build_master_playlist(&plan(s), &req, &MasterPlaylistContext::default());
        let lines: Vec<&str> = pl.lines().collect();
        assert_eq!(lines.len(), 5, "got: {pl}");
        assert_eq!(
            lines[1],
            "#EXT-X-STREAM-INF:BANDWIDTH=1128000,AVERAGE-BANDWIDTH=1128000,VIDEO-RANGE=PQ,\
             CODECS=\"hvc1.2.4.L153.B0,mp4a.40.2\",RESOLUTION=3840x2160,FRAME-RATE=23.976"
        );
        assert_eq!(
            lines[2],
            "main.m3u8?mediaSourceId=abc&videoCodec=hevc,h264&audioCodec=aac&segmentContainer=ts"
        );
        // The SDR variant: transcode → SDR, default h264 level 41, no profile
        // → constrained baseline, FRAME-RATE from the request (none) → the
        // source's real framerate.
        assert_eq!(
            lines[3],
            "#EXT-X-STREAM-INF:BANDWIDTH=1128000,AVERAGE-BANDWIDTH=1128000,VIDEO-RANGE=SDR,\
             CODECS=\"avc1.424029,mp4a.40.2\",RESOLUTION=3840x2160,FRAME-RATE=23.976"
        );
        // The existing `videoCodec` key keeps its spelling and position with
        // its value replaced; new keys append.
        assert_eq!(
            lines[4],
            "main.m3u8?mediaSourceId=abc&videoCodec=h264&audioCodec=aac&segmentContainer=ts&AllowVideoStreamCopy=false"
        );
    }

    /// With HEVC encoding allowed and an HDR HEVC copy, the HEVC SDR entrance
    /// comes first, and the later H.264 entrance inherits its `hevc-profile`
    /// key (the C# shares one dictionary across the variants).
    #[test]
    fn hdr_copy_with_hevc_allowed_adds_both_sdr_variants_sharing_the_query() {
        let mut s = state(vec![hdr_hevc_stream(), audio_stream()]);
        s.output_video_codec = Some("copy".to_owned());
        let mut p = plan(s);
        p.encoding_options.allow_hevc_encoding = true;
        let req = HlsStreamRequest {
            query_string: "?mediaSourceId=abc&videoCodec=hevc&audioCodec=aac&segmentContainer=ts"
                .to_owned(),
            ..request()
        };
        let pl = build_master_playlist(&p, &req, &MasterPlaylistContext::default());
        let lines: Vec<&str> = pl.lines().collect();
        assert_eq!(lines.len(), 7, "got: {pl}");
        assert!(lines[3].contains("VIDEO-RANGE=SDR,CODECS=\"hvc1.1.4.L120.B0,mp4a.40.2\""));
        assert_eq!(
            lines[4],
            "main.m3u8?mediaSourceId=abc&videoCodec=hevc&audioCodec=aac&segmentContainer=ts&hevc-profile=main&AllowVideoStreamCopy=false"
        );
        assert!(lines[5].contains("CODECS=\"avc1.424029,mp4a.40.2\""));
        assert_eq!(
            lines[6],
            "main.m3u8?mediaSourceId=abc&videoCodec=h264&audioCodec=aac&segmentContainer=ts&hevc-profile=main&AllowVideoStreamCopy=false"
        );
    }

    /// A copied SDR HEVC source above level 5.0 gets a duplicate entry whose
    /// CODECS advertise level 150.
    #[test]
    fn sdr_hevc_copy_above_level_150_gets_the_level_50_duplicate() {
        let mut video = hdr_hevc_stream();
        video.color_transfer = Some("bt709".to_owned());
        video.color_primaries = Some("bt709".to_owned());
        video.bit_depth = Some(8);
        video.profile = Some("Main".to_owned());
        let mut s = state(vec![video, audio_stream()]);
        s.output_video_codec = Some("copy".to_owned());
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        let lines: Vec<&str> = pl.lines().collect();
        assert_eq!(lines.len(), 5, "got: {pl}");
        assert!(lines[1].contains("CODECS=\"hvc1.1.4.L153.B0,mp4a.40.2\""));
        assert!(lines[3].contains("CODECS=\"hvc1.1.4.L150.B0,mp4a.40.2\""));
        assert_eq!(lines[2], lines[4]);
    }

    /// Dolby Vision / HDR10+ copies advertise SUPPLEMENTAL-CODECS; a
    /// transcode never does.
    #[test]
    fn supplemental_codecs_for_dovi_and_hdr10plus_copies() {
        let mut video = hdr_hevc_stream();
        video.dv_profile = Some(8);
        video.dv_level = Some(6);
        video.dv_bl_signal_compatibility_id = Some(1);
        video.rpu_present_flag = Some(1);
        video.bl_present_flag = Some(1);
        video.el_present_flag = Some(0);
        let mut s = state(vec![video.clone(), audio_stream()]);
        s.output_video_codec = Some("copy".to_owned());
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(
            pl.contains(",SUPPLEMENTAL-CODECS=\"dvh1.08.06/db1p\","),
            "got: {pl}"
        );

        // HDR10+ (no DoVi): the video codec string + /cdm4.
        let mut plus = hdr_hevc_stream();
        plus.hdr10_plus_present_flag = Some(true);
        let mut s = state(vec![plus, audio_stream()]);
        s.output_video_codec = Some("copy".to_owned());
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(
            pl.contains(",SUPPLEMENTAL-CODECS=\"hvc1.2.4.L153.B0/cdm4\","),
            "got: {pl}"
        );

        // Transcoding: no supplemental field.
        let s = state(vec![video, audio_stream()]);
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(!pl.contains("SUPPLEMENTAL-CODECS"), "got: {pl}");
    }

    /// `OutputAudioCodec != query AudioCodec` rewrites the whole query through
    /// ParseQuery/AddQueryString: `audioCodec=copy`, spellings kept, and the
    /// other comma lists survive verbatim (`UrlEncoder.Default` leaves `,`
    /// alone) — the common jellyfin-web case, whose `AudioCodec=aac,mp3,…`
    /// never equals the single output codec.
    #[test]
    fn audio_codec_mismatch_rewrites_the_query() {
        let mut s = state(vec![video_stream(), audio_stream()]);
        s.output_audio_codec = Some("copy".to_owned());
        let req = HlsStreamRequest {
            query_string: "?mediaSourceId=abc&AudioCodec=aac,mp3&videoCodec=hevc,h264&\
                           TranscodeReasons=ContainerNotSupported,AudioCodecNotSupported&\
                           segmentContainer=ts"
                .to_owned(),
            ..request()
        };
        let pl = build_master_playlist(&plan(s), &req, &MasterPlaylistContext::default());
        assert!(
            pl.contains(
                "\nmain.m3u8?mediaSourceId=abc&AudioCodec=copy&videoCodec=hevc,h264&\
                 TranscodeReasons=ContainerNotSupported,AudioCodecNotSupported&segmentContainer=ts\n"
            ),
            "got: {pl}"
        );
        // Copy makes the CODECS entry the SOURCE audio codec.
        assert!(pl.contains("CODECS=\"avc1.424029,mp4a.40.2\""), "got: {pl}");

        // A matching codec leaves the raw query untouched (commas and all).
        let s = state(vec![video_stream(), audio_stream()]);
        let req = HlsStreamRequest {
            query_string:
                "?mediaSourceId=abc&audioCodec=aac&videoCodec=h264,hevc&segmentContainer=ts"
                    .to_owned(),
            ..request()
        };
        let pl = build_master_playlist(&plan(s), &req, &MasterPlaylistContext::default());
        assert!(
            pl.contains(
                "\nmain.m3u8?mediaSourceId=abc&audioCodec=aac&videoCodec=h264,hevc&segmentContainer=ts\n"
            ),
            "got: {pl}"
        );
    }

    /// `SegmentContainer` / `TranscodeReasons` are appended when absent from
    /// the query (the universal-audio path); rotation + ts copy vetoes copy.
    #[test]
    fn universal_audio_appends_and_rotation_vetoes_copy() {
        let s = state(vec![video_stream(), audio_stream()]);
        let req = HlsStreamRequest {
            query_string: "?mediaSourceId=abc&audioCodec=aac".to_owned(),
            transcode_reasons: Some("ContainerNotSupported".to_owned()),
            ..request()
        };
        let pl = build_master_playlist(&plan(s), &req, &MasterPlaylistContext::default());
        assert!(
            pl.contains(
                "main.m3u8?mediaSourceId=abc&audioCodec=aac&SegmentContainer=ts&TranscodeReasons=ContainerNotSupported\n"
            ),
            "got: {pl}"
        );

        let mut video = video_stream();
        video.rotation = Some(90);
        let mut s = state(vec![video, audio_stream()]);
        s.output_video_codec = Some("copy".to_owned());
        let req = HlsStreamRequest {
            query_string: "?mediaSourceId=abc&audioCodec=aac&segmentContainer=ts".to_owned(),
            ..request()
        };
        let pl = build_master_playlist(&plan(s), &req, &MasterPlaylistContext::default());
        assert!(
            pl.contains("&segmentContainer=ts&AllowVideoStreamCopy=false\n"),
            "got: {pl}"
        );
    }

    /// The subtitle group appears only with `SubtitleMethod=Hls` or
    /// `EnableSubtitlesInManifest=true`; burn-in suppresses it; the HLS-selected
    /// stream is DEFAULT=YES.
    #[test]
    fn subtitle_group_follows_delivery_method_and_manifest_flag() {
        let streams = vec![
            video_stream(),
            audio_stream(),
            text_subtitle(2, "eng", false),
            text_subtitle(3, "fra", true),
        ];
        // Default (External, flag off): no group.
        let pl = build_master_playlist(
            &plan(state(streams.clone())),
            &request(),
            &MasterPlaylistContext::default(),
        );
        assert!(!pl.contains("EXT-X-MEDIA"), "got: {pl}");
        assert!(!pl.contains("SUBTITLES="), "got: {pl}");

        // Flag on: group listed, none default.
        let req = HlsStreamRequest {
            enable_subtitles_in_manifest: true,
            ..request()
        };
        let pl = build_master_playlist(
            &plan(state(streams.clone())),
            &req,
            &MasterPlaylistContext::default(),
        );
        assert!(
            pl.starts_with(
                "#EXTM3U\n\
                 #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Eng - SUBRIP\",DEFAULT=NO,FORCED=NO,AUTOSELECT=YES,URI=\"abc/Subtitles/2/subtitles.m3u8?SegmentLength=30&ApiKey=tok\",LANGUAGE=\"eng\"\n\
                 #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Fra - Forced - SUBRIP\",DEFAULT=NO,FORCED=YES,AUTOSELECT=YES,URI=\"abc/Subtitles/3/subtitles.m3u8?SegmentLength=30&ApiKey=tok\",LANGUAGE=\"fra\"\n\
                 #EXT-X-STREAM-INF:"
            ),
            "got: {pl}"
        );
        assert!(
            pl.contains(",FRAME-RATE=10,SUBTITLES=\"subs\"\n"),
            "got: {pl}"
        );

        // Hls delivery of stream 3: group listed, 3 is DEFAULT=YES.
        let mut s = state(streams.clone());
        s.subtitle_stream = Some(text_subtitle(3, "fra", true));
        s.subtitle_delivery_method = SubtitleDeliveryMethod::Hls;
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(
            pl.contains("NAME=\"Fra - Forced - SUBRIP\",DEFAULT=YES,FORCED=YES"),
            "got: {pl}"
        );
        assert!(pl.contains("NAME=\"Eng - SUBRIP\",DEFAULT=NO"), "got: {pl}");

        // Burn-in (Encode) of a selected stream: no group even with the flag.
        let mut s = state(streams.clone());
        s.subtitle_stream = Some(text_subtitle(2, "eng", false));
        s.subtitle_delivery_method = SubtitleDeliveryMethod::Encode;
        let req = HlsStreamRequest {
            enable_subtitles_in_manifest: true,
            ..request()
        };
        let pl = build_master_playlist(&plan(s), &req, &MasterPlaylistContext::default());
        assert!(!pl.contains("EXT-X-MEDIA"), "got: {pl}");

        // Drop: the group name is set but AddSubtitles emits nothing.
        let mut s = state(streams);
        s.subtitle_delivery_method = SubtitleDeliveryMethod::Drop;
        let req = HlsStreamRequest {
            enable_subtitles_in_manifest: true,
            ..request()
        };
        let pl = build_master_playlist(&plan(s), &req, &MasterPlaylistContext::default());
        assert!(!pl.contains("EXT-X-MEDIA"), "got: {pl}");
        assert!(pl.contains(",SUBTITLES=\"subs\"\n"), "got: {pl}");
    }

    /// Adaptive variants: off on the LAN, on otherwise with two extra entries
    /// at `VideoBitrate` minus the variation (1.128M → 300k steps).
    #[test]
    fn adaptive_variants_only_off_lan() {
        let lan = HlsStreamRequest {
            enable_adaptive_bitrate_streaming: true,
            is_in_local_network: true,
            ..request()
        };
        let pl = build_master_playlist(&fixture_plan(), &lan, &MasterPlaylistContext::default());
        assert_eq!(pl.lines().count(), 3, "got: {pl}");

        let remote = HlsStreamRequest {
            enable_adaptive_bitrate_streaming: true,
            ..request()
        };
        let pl = build_master_playlist(&fixture_plan(), &remote, &MasterPlaylistContext::default());
        let lines: Vec<&str> = pl.lines().collect();
        assert_eq!(lines.len(), 7, "got: {pl}");
        assert!(
            lines[3].starts_with("#EXT-X-STREAM-INF:BANDWIDTH=828000,AVERAGE-BANDWIDTH=828000,")
        );
        assert_eq!(
            lines[4],
            "main.m3u8?mediaSourceId=abc&deviceId=parity-streams&playSessionId=parity-1&\
             audioCodec=aac&audioBitRate=128000&segmentContainer=ts&videoCodec=h264&\
             videoBitRate=700000&maxWidth=320&transcodingMaxAudioChannels=2"
        );
        assert!(
            lines[5].starts_with("#EXT-X-STREAM-INF:BANDWIDTH=528000,AVERAGE-BANDWIDTH=528000,")
        );
        assert!(lines[6].contains("&videoBitRate=400000&"), "got: {pl}");

        // A copy (video or audio) disables it.
        let mut s = state(vec![video_stream(), audio_stream()]);
        s.output_audio_codec = Some("copy".to_owned());
        let pl = build_master_playlist(&plan(s), &remote, &MasterPlaylistContext::default());
        assert!(pl.contains("audioCodec=copy"), "got: {pl}");
        assert_eq!(pl.lines().count(), 3, "got: {pl}");
    }

    /// Trickplay image playlists, sorted by width; suppressed by the flag.
    #[test]
    fn trickplay_lines_sorted_by_width() {
        let ctx = MasterPlaylistContext {
            trickplay_resolutions: vec![
                TrickplayResolution {
                    width: 320,
                    height: 180,
                    bandwidth: 120_000,
                },
                TrickplayResolution {
                    width: 160,
                    height: 90,
                    bandwidth: 40_000,
                },
            ],
        };
        let pl = build_master_playlist(&fixture_plan(), &request(), &ctx);
        assert!(
            pl.ends_with(
                "#EXT-X-IMAGE-STREAM-INF:BANDWIDTH=40000,RESOLUTION=160x90,CODECS=\"jpeg\",URI=\"Trickplay/160/tiles.m3u8?MediaSourceId=abc&ApiKey=tok\"\n\
                 #EXT-X-IMAGE-STREAM-INF:BANDWIDTH=120000,RESOLUTION=320x180,CODECS=\"jpeg\",URI=\"Trickplay/320/tiles.m3u8?MediaSourceId=abc&ApiKey=tok\"\n"
            ),
            "got: {pl}"
        );
        let req = HlsStreamRequest {
            enable_trickplay: false,
            ..request()
        };
        let pl = build_master_playlist(&fixture_plan(), &req, &ctx);
        assert!(!pl.contains("IMAGE-STREAM"), "got: {pl}");
    }

    /// An open-ended source (no runtime) points the variant at `live.m3u8` and
    /// lists no trickplay.
    #[test]
    fn segmented_live_stream_points_at_live_playlist() {
        let mut s = state(vec![video_stream(), audio_stream()]);
        s.run_time_ticks = None;
        let ctx = MasterPlaylistContext {
            trickplay_resolutions: vec![TrickplayResolution {
                width: 320,
                height: 180,
                bandwidth: 1,
            }],
        };
        let pl = build_master_playlist(&plan(s), &request(), &ctx);
        assert!(pl.contains("\nlive.m3u8?mediaSourceId=abc"), "got: {pl}");
        assert!(!pl.contains("IMAGE-STREAM"), "got: {pl}");
    }

    /// VP9 and AV1 remux / transcode CODECS.
    #[test]
    fn vp9_and_av1_codecs() {
        let mut vp9 = video_stream();
        vp9.codec = Some("vp9".to_owned());
        vp9.width = Some(1920);
        vp9.height = Some(1080);
        vp9.level = Some(0.0);
        let mut s = state(vec![vp9, audio_stream()]);
        s.output_video_codec = Some("copy".to_owned());
        s.base_request.max_width = None;
        // Level 0 → "invalid level" → no video codec string.
        let pl = build_master_playlist(
            &plan(s.clone()),
            &request(),
            &MasterPlaylistContext::default(),
        );
        assert!(pl.contains("CODECS=\"mp4a.40.2\""), "got: {pl}");
        // Any non-zero level → the probed VP9 string.
        s.video_stream.as_mut().unwrap().level = Some(1.0);
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(
            pl.contains("CODECS=\"vp09.00.40.08,mp4a.40.2\""),
            "got: {pl}"
        );

        // AV1 transcode: the default level 19 is clamped to 15 by
        // NormalizeTranscodingLevel (AV1 5.3), Main, 8-bit.
        let mut s = state(vec![video_stream(), audio_stream()]);
        s.output_video_codec = Some("av1".to_owned());
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(
            pl.contains("CODECS=\"av01.0.15M.08,mp4a.40.2\""),
            "got: {pl}"
        );
    }

    #[test]
    fn bitrate_variation_table_is_upstream() {
        assert_eq!(bitrate_variation(0), 50_000);
        assert_eq!(bitrate_variation(399_999), 50_000);
        assert_eq!(bitrate_variation(400_000), 100_000);
        assert_eq!(bitrate_variation(600_000), 200_000);
        assert_eq!(bitrate_variation(1_000_000), 300_000);
        assert_eq!(bitrate_variation(2_000_000), 500_000);
        assert_eq!(bitrate_variation(3_000_000), 1_000_000);
        assert_eq!(bitrate_variation(5_000_000), 1_500_000);
        assert_eq!(bitrate_variation(10_000_000), 2_000_000);
    }

    #[test]
    fn parsed_query_round_trips_like_query_helpers() {
        let mut q = ParsedQuery::parse("?a=1&B=x%2Cy&c&A=2&=&d=p+q");
        assert_eq!(q.joined("a"), "1,2");
        assert_eq!(q.joined("b"), "x,y");
        assert_eq!(q.joined("c"), "");
        assert_eq!(q.joined("missing"), "");
        // The indexer set keeps the original key and position.
        q.set("b", Some("z w"));
        q.set("New", Some("v"));
        q.set("c", None);
        assert_eq!(
            q.add_query_string("main.m3u8"),
            "main.m3u8?a=1&a=2&B=z%20w&=&d=p%20q&New=v"
        );
        assert_eq!(ParsedQuery::parse("").add_query_string(""), "");
        assert_eq!(
            ParsedQuery::parse("x=%E2%82%AC").add_query_string("u?k=v"),
            "u?k=v&x=%E2%82%AC"
        );
        assert_eq!(url_decode("100%zz%2"), "100%zz%2");
        assert_eq!(url_decode("%E2%82%AC+%41"), "€ A");
        // An invalid UTF-8 run stays escaped, as `Uri.UnescapeDataString` does.
        assert_eq!(url_decode("a%E4%BDb"), "a%E4%BDb");
        // .NET's `DefaultUrlEncoder`: unreserved + `!$()*,;@` pass, the
        // query-significant `&'+=#` and gen-delims are escaped (the runtime's
        // own `Hello <>&'"+ there!` case).
        assert_eq!(url_encode("a-b_c.d~e!*'()"), "a-b_c.d~e!*%27()");
        assert_eq!(url_encode("$,;@"), "$,;@");
        assert_eq!(
            url_encode("Hello <>&'\"+ there!"),
            "Hello%20%3C%3E%26%27%22%2B%20there!"
        );
        assert_eq!(url_encode("a=b#c/d:e?f[g]"), "a%3Db%23c%2Fd%3Ae%3Ff%5Bg%5D");
    }

    #[test]
    fn framerate_rounds_to_three_decimals_like_math_round() {
        let mut video = video_stream();
        video.average_frame_rate = Some(29.97);
        video.real_frame_rate = Some(29.97);
        let mut s = state(vec![video, audio_stream()]);
        s.output_video_codec = Some("copy".to_owned());
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(pl.contains(",FRAME-RATE=29.97\n"), "got: {pl}");

        // A re-encode with a requested MaxFramerate advertises that.
        let mut s = state(vec![video_stream(), audio_stream()]);
        s.base_request.max_framerate = Some(23.976_f32);
        let pl = build_master_playlist(&plan(s), &request(), &MasterPlaylistContext::default());
        assert!(pl.contains(",FRAME-RATE=23.976\n"), "got: {pl}");
    }
}
