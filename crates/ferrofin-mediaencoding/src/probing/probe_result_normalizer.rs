//! ffprobe JSON -> [`MediaInfo`] normalization — port of
//! `MediaBrowser.MediaEncoding.Probing.ProbeResultNormalizer`.
//!
//! Pure transformation: no ffmpeg/ffprobe invocation, no file I/O beyond the
//! MPEG-TS timestamp sniff (which is skipped for anything but a local TS file).

use std::sync::LazyLock;

use chrono::Datelike;
use ferrofin_model::data::PersonKind;
use ferrofin_model::dto::BaseItemPerson;
use ferrofin_model::entities::{MediaStreamType, Video3DFormat, VideoType};
use ferrofin_model::entities_media::{ChapterInfo, MediaAttachment, MediaStream};
use ferrofin_model::media_info::{MediaInfo, MediaProtocol};
use regex::Regex;
use uuid::Uuid;

use super::dtos::{
    CodecType, InternalMediaInfoResult, MediaFormatInfo, MediaFrameInfo, MediaStreamInfo,
};
use super::ff_probe_helpers::{
    CaseInsensitiveTags, flatten_tags, get_dictionary_date_time, get_dictionary_numeric_value,
    get_dictionary_value, normalize_ffprobe_result,
};
use super::localization::LocalizationManager;

const ARTIST_REPLACE_VALUE: &str = " | ";

const BASIC_DELIMITERS: &[char] = &['/', ';'];
const NAME_DELIMITERS: &[char] = &['/', ';', '|', '\\'];
const GENRE_DELIMITERS: &[char] = &['/', ';', ','];
const WEBM_VIDEO_CODECS: &[&str] = &["av1", "vp8", "vp9"];
const WEBM_AUDIO_CODECS: &[&str] = &["opus", "vorbis"];

/// `(?<name>.*) \((?<instrument>.*)\)`.
static PERFORMER_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*) \((.*)\)$").expect("valid performer regex"));

/// `(\.\d{7})\d+` — trims Matroska nanosecond DURATION over-precision.
static DURATION_OVERPRECISION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\.\d{7})\d+").expect("valid duration regex"));

/// Artist strings that contain a delimiter char but must not be split.
const SPLIT_WHITELIST: &[&str] = &[
    "AC/DC",
    "A/T/O/S",
    "As/Hi Soundworks",
    "Au/Ra",
    "Bremer/McCoy",
    "b/bqスタヂオ",
    "DOV/S",
    "DJ'TEKINA//SOMETHING",
    "IX/ON",
    "J-CORE SLi//CER",
    "M(a/u)SH",
    "Kaoru/Brilliance",
    "signum/ii",
    "Richiter(LORB/DUGEM DI BARAT)",
    "이달의 소녀 1/3",
    "R!N / Gemie",
    "LOONA 1/3",
    "LOONA / yyxy",
    "LOONA / ODD EYE CIRCLE",
    "K/DA",
    "22/7",
    "諭吉佳作/men",
    "//dARTH nULL",
    "Phantom/Ghost",
    "She/Her/Hers",
    "5/8erl in Ehr'n",
    "Smith/Kotzen",
    "We;Na",
    "LSR/CITY",
    "Kairon; IRSE!",
];

/// Normalizes ffprobe output into a [`MediaInfo`].
///
/// Holds a [`LocalizationManager`] used to stamp localized labels onto each
/// stream, matching the upstream constructor's `ILocalizationManager`.
pub struct ProbeResultNormalizer<L: LocalizationManager> {
    localization: L,
}

impl<L: LocalizationManager> ProbeResultNormalizer<L> {
    /// Creates a normalizer with the given localization manager.
    pub fn new(localization: L) -> Self {
        Self { localization }
    }

    /// Transforms an ffprobe response into its [`MediaInfo`] equivalent.
    ///
    /// Mirrors `ProbeResultNormalizer.GetMediaInfo`.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn get_media_info(
        &self,
        mut data: InternalMediaInfoResult,
        video_type: Option<VideoType>,
        is_audio: bool,
        path: &str,
        protocol: MediaProtocol,
    ) -> MediaInfo {
        let mut info = MediaInfo::default();
        info.media_source.path = Some(path.to_owned());
        info.media_source.protocol = protocol;
        info.media_source.video_type = video_type;

        normalize_ffprobe_result(&mut data);
        set_size(&data, &mut info);

        let format = data.format.clone();
        let frames = data.frames.clone().unwrap_or_default();
        let streams = data.streams.clone().unwrap_or_default();

        let media_streams: Vec<MediaStream> = streams
            .iter()
            .filter_map(|s| self.get_media_stream(is_audio, s, format.as_ref(), &frames))
            // Drop subtitle streams with an unknown codec.
            .filter(|s| {
                s.stream_type != MediaStreamType::Subtitle
                    || s.codec.as_deref().is_some_and(|c| !c.trim().is_empty())
            })
            .collect();

        let media_attachments: Vec<MediaAttachment> =
            streams.iter().filter_map(get_media_attachment).collect();

        if let Some(format) = format.as_ref() {
            info.media_source.container =
                normalize_format(format.format_name.as_deref(), &media_streams);
            if let Some(value) = format
                .bit_rate
                .as_deref()
                .and_then(|b| b.trim().parse::<i32>().ok())
            {
                info.media_source.bitrate = Some(value);
            }
        }

        // Build a merged, case-insensitive tag map: the first matching stream's
        // tags, overlaid by the format tags.
        let mut tags = CaseInsensitiveTags::new();
        let tag_stream_type = if is_audio {
            CodecType::Audio
        } else {
            CodecType::Video
        };
        if let Some(tag_stream) = streams
            .iter()
            .find(|s| s.codec_type == Some(tag_stream_type))
            && let Some(stream_tags) = tag_stream.tags.as_ref()
        {
            for (k, v) in flatten_tags(stream_tags) {
                tags.insert(k, v);
            }
        }

        if let Some(format) = format.as_ref()
            && let Some(format_tags) = format.tags.as_ref()
        {
            for (k, v) in flatten_tags(format_tags) {
                tags.insert(k, v);
            }
        }

        fetch_genres(&mut info, &tags);

        info.media_source.name = get_first_not_blank(&tags, &["title", "title-eng"]);
        info.forced_sort_name =
            get_first_not_blank(&tags, &["sort_name", "title-sort", "titlesort"]);
        info.overview = get_first_not_blank(&tags, &["synopsis", "description", "desc", "comment"]);

        info.parent_index_number = get_dictionary_numeric_value(&tags, "season_number");
        info.index_number = get_dictionary_numeric_value(&tags, "episode_sort")
            .or_else(|| get_dictionary_numeric_value(&tags, "episode_id"));
        info.show_name = get_dictionary_value(&tags, "show_name")
            .or_else(|| get_dictionary_value(&tags, "show"))
            .map(str::to_owned);
        info.production_year = get_dictionary_numeric_value(&tags, "date");

        info.premiere_date = get_dictionary_date_time(&tags, "originaldate")
            .or_else(|| get_dictionary_date_time(&tags, "retaildate"))
            .or_else(|| get_dictionary_date_time(&tags, "retail date"))
            .or_else(|| get_dictionary_date_time(&tags, "retail_date"))
            .or_else(|| get_dictionary_date_time(&tags, "date_released"))
            .or_else(|| get_dictionary_date_time(&tags, "date"))
            .or_else(|| get_dictionary_date_time(&tags, "creation_time"));

        info.album = get_dictionary_value(&tags, "album").map(str::to_owned);

        if let Some(artists) =
            get_dictionary_value(&tags, "artists").filter(|s| !s.trim().is_empty())
        {
            info.artists = split_distinct_artists(artists, BASIC_DELIMITERS, false);
        } else if let Some(artist) = get_first_not_blank(&tags, &["artist"]) {
            info.artists = split_distinct_artists(&artist, NAME_DELIMITERS, true);
        } else {
            info.artists = Vec::new();
        }

        if info.production_year.is_none()
            && let Some(date) = info.premiere_date
        {
            info.production_year = Some(date.year());
        }

        if let Some(chapters) = data.chapters.as_ref() {
            info.chapters = chapters.iter().map(get_chapter_info).collect();
        }

        info.media_source.media_streams = media_streams;
        info.media_source.media_attachments = media_attachments;

        if is_audio {
            set_audio_runtime_ticks(&data, &mut info);
            set_audio_info_from_tags(&mut info, &tags);
        } else {
            fetch_studios(&mut info, &tags, "copyright");

            if let Some(itun_extc) = get_first_not_blank(&tags, &["iTunEXTC"]) {
                let parts: Vec<&str> = itun_extc.split('|').filter(|p| !p.is_empty()).collect();
                if parts.len() > 1 {
                    info.official_rating = Some(parts[1].to_owned());
                    if parts.len() > 3 {
                        info.official_rating_description = Some(parts[3].to_owned());
                    }
                }
            }

            if let Some(itun_xml) = get_first_not_blank(&tags, &["iTunMOVI"]) {
                fetch_from_itunes_info(&itun_xml, &mut info);
            }

            if let Some(format) = format.as_ref()
                && let Some(duration) = format.duration.as_deref().filter(|d| !d.is_empty())
                && let Ok(seconds) = duration.parse::<f64>()
            {
                info.media_source.run_time_ticks = Some(seconds_to_ticks(seconds));
            }

            fetch_wtv_info(&mut info, &data);

            if get_dictionary_value(&tags, "stereo_mode")
                .is_some_and(|m| m.eq_ignore_ascii_case("left_right"))
            {
                // Video3DFormat is carried on MediaSourceInfo.
                info.media_source.video3d_format = Some(Video3DFormat::FullSideBySide);
            }

            for stream in &mut info.media_source.media_streams {
                if stream.stream_type == MediaStreamType::Audio && stream.bit_rate.is_none() {
                    stream.bit_rate = get_estimated_audio_bitrate(
                        stream.codec.as_deref(),
                        stream.profile.as_deref(),
                        stream.channels,
                    );
                }
            }

            estimate_missing_video_bitrate(&mut info);

            info.media_source.infer_total_bitrate(false);
        }

        info
    }

    /// Converts an ffprobe stream to a [`MediaStream`].
    #[allow(clippy::too_many_lines)]
    fn get_media_stream(
        &self,
        is_audio: bool,
        stream_info: &MediaStreamInfo,
        format_info: Option<&MediaFormatInfo>,
        frames: &[MediaFrameInfo],
    ) -> Option<MediaStream> {
        let mut stream = MediaStream {
            codec: stream_info.codec_name.clone(),
            profile: stream_info.profile.clone(),
            width: stream_info.width,
            height: stream_info.height,
            level: stream_info.level.map(f64::from),
            index: stream_info.index,
            pixel_format: stream_info.pixel_format.clone(),
            nal_length_size: stream_info.nal_length_size.clone(),
            time_base: stream_info.time_base.clone(),
            codec_time_base: stream_info.codec_time_base.clone(),
            ..Default::default()
        };

        // Filter out junk codec tags. NOTE: `codec_tag_string` binds to the
        // typo'd JSON key upstream, so this is effectively always `None`.
        if let Some(tag) = stream_info.codec_tag_string.as_deref()
            && !tag.trim().is_empty()
            && !tag.to_ascii_lowercase().contains("[0]")
        {
            stream.codec_tag = Some(tag.to_owned());
        }

        let flat_tags = stream_info.tags.as_ref().map(flatten_tags);
        if let Some(tags) = flat_tags.as_ref() {
            stream.language = get_dictionary_value(tags, "language").map(str::to_owned);
            stream.comment = get_dictionary_value(tags, "comment").map(str::to_owned);
            stream.title = get_dictionary_value(tags, "title").map(str::to_owned);
        }

        match stream_info.codec_type {
            Some(CodecType::Audio) => {
                stream.stream_type = MediaStreamType::Audio;
                stream.localized_default = Some(self.localization.get_localized_string("Default"));
                stream.localized_external =
                    Some(self.localization.get_localized_string("External"));
                if let Some(lang) = stream.language.as_deref().filter(|l| !l.is_empty()) {
                    stream.localized_language =
                        Some(self.localization.get_language_display_name(lang));
                }

                stream.channels = stream_info.channels;
                if let Some(rate) = stream_info
                    .sample_rate
                    .as_deref()
                    .and_then(|r| r.trim().parse::<i32>().ok())
                {
                    stream.sample_rate = Some(rate);
                }

                stream.channel_layout = parse_channel_layout(stream_info.channel_layout.as_deref());

                if stream_info.bits_per_sample > 0 {
                    stream.bit_depth = Some(stream_info.bits_per_sample);
                } else if stream_info.bits_per_raw_sample > 0 {
                    stream.bit_depth = Some(stream_info.bits_per_raw_sample);
                }

                if stream.title.as_deref().is_none_or(str::is_empty)
                    && let Some(handler) = flat_tags
                        .as_ref()
                        .and_then(|t| get_dictionary_value(t, "handler_name"))
                        .filter(|h| !h.is_empty() && !h.eq_ignore_ascii_case("SoundHandler"))
                {
                    stream.title = Some(handler.to_owned());
                }
            }
            Some(CodecType::Subtitle) => {
                stream.stream_type = MediaStreamType::Subtitle;
                stream.codec = stream.codec.as_deref().map(normalize_subtitle_codec);
                stream.localized_undefined =
                    Some(self.localization.get_localized_string("Undefined"));
                stream.localized_default = Some(self.localization.get_localized_string("Default"));
                stream.localized_forced = Some(self.localization.get_localized_string("Forced"));
                stream.localized_external =
                    Some(self.localization.get_localized_string("External"));
                stream.localized_hearing_impaired =
                    Some(self.localization.get_localized_string("HearingImpaired"));
                if let Some(lang) = stream.language.as_deref().filter(|l| !l.is_empty()) {
                    stream.localized_language =
                        Some(self.localization.get_language_display_name(lang));
                }

                if stream.title.as_deref().is_none_or(str::is_empty)
                    && let Some(handler) = flat_tags
                        .as_ref()
                        .and_then(|t| get_dictionary_value(t, "handler_name"))
                        .filter(|h| !h.is_empty() && !h.eq_ignore_ascii_case("SubtitleHandler"))
                {
                    stream.title = Some(handler.to_owned());
                }
            }
            Some(CodecType::Video) => {
                stream.is_avc = stream_info.is_avc;
                stream.average_frame_rate =
                    get_frame_rate(stream_info.average_frame_rate.as_deref());
                stream.real_frame_rate = get_frame_rate(stream_info.r_frame_rate.as_deref());

                stream.is_interlaced = stream_info.field_order.as_deref().is_some_and(|f| {
                    !f.trim().is_empty() && !f.eq_ignore_ascii_case("progressive")
                });

                let codec = stream.codec.as_deref().unwrap_or("");
                if is_audio
                    || codec.eq_ignore_ascii_case("bmp")
                    || codec.eq_ignore_ascii_case("gif")
                    || codec.eq_ignore_ascii_case("png")
                    || codec.eq_ignore_ascii_case("webp")
                {
                    stream.stream_type = MediaStreamType::EmbeddedImage;
                } else if codec.eq_ignore_ascii_case("mjpeg") {
                    if stream
                        .codec_tag
                        .as_deref()
                        .is_some_and(|t| !t.trim().is_empty())
                    {
                        stream.stream_type = MediaStreamType::Video;
                    } else {
                        stream.stream_type = MediaStreamType::EmbeddedImage;
                    }
                } else {
                    stream.stream_type = MediaStreamType::Video;
                }

                stream.aspect_ratio = get_aspect_ratio(stream_info);

                if stream_info.bits_per_sample > 0 {
                    stream.bit_depth = Some(stream_info.bits_per_sample);
                } else if stream_info.bits_per_raw_sample > 0 {
                    stream.bit_depth = Some(stream_info.bits_per_raw_sample);
                }

                if stream.bit_depth.is_none()
                    && let Some(pix) = stream_info
                        .pixel_format
                        .as_deref()
                        .filter(|p| !p.is_empty())
                {
                    if pix.eq_ignore_ascii_case("yuv420p") || pix.eq_ignore_ascii_case("yuv444p") {
                        stream.bit_depth = Some(8);
                    } else if pix.eq_ignore_ascii_case("yuv420p10le")
                        || pix.eq_ignore_ascii_case("yuv444p10le")
                    {
                        stream.bit_depth = Some(10);
                    } else if pix.eq_ignore_ascii_case("yuv420p12le")
                        || pix.eq_ignore_ascii_case("yuv444p12le")
                    {
                        stream.bit_depth = Some(12);
                    }
                }

                set_anamorphic(&mut stream, stream_info);

                if stream_info.refs > 0 {
                    stream.ref_frames = Some(stream_info.refs);
                }

                stream.color_range = stream_info.color_range.clone().filter(|s| !s.is_empty());
                stream.color_space = stream_info.color_space.clone().filter(|s| !s.is_empty());
                stream.color_transfer =
                    stream_info.color_transfer.clone().filter(|s| !s.is_empty());
                stream.color_primaries = stream_info
                    .color_primaries
                    .clone()
                    .filter(|s| !s.is_empty());

                if let Some(side_data_list) = stream_info.side_data_list.as_ref() {
                    for data in side_data_list {
                        let side_type = data.side_data_type.as_deref().unwrap_or("");
                        if side_type.eq_ignore_ascii_case("DOVI configuration record") {
                            stream.dv_version_major = data.dv_version_major;
                            stream.dv_version_minor = data.dv_version_minor;
                            stream.dv_profile = data.dv_profile;
                            stream.dv_level = data.dv_level;
                            stream.rpu_present_flag = data.rpu_present_flag;
                            stream.el_present_flag = data.el_present_flag;
                            stream.bl_present_flag = data.bl_present_flag;
                            stream.dv_bl_signal_compatibility_id =
                                data.dv_bl_signal_compatibility_id;
                        } else if side_type.eq_ignore_ascii_case("Display Matrix") {
                            stream.rotation = data.rotation;
                        } else if side_type.eq_ignore_ascii_case("Frame Cropping") {
                            stream.is_anamorphic = Some(false);
                        }
                    }
                }

                let frame = frames.iter().find(|f| f.stream_index == Some(stream.index));
                if let Some(frame) = frame
                    && let Some(list) = frame.side_data_list.as_ref()
                    && list.iter().any(|d| {
                        d.side_data_type.as_deref().is_some_and(|t| {
                            t.eq_ignore_ascii_case("HDR Dynamic Metadata SMPTE2094-40 (HDR10+)")
                        })
                    })
                {
                    stream.hdr10_plus_present_flag = Some(true);
                }
            }
            Some(CodecType::Data) => {
                stream.stream_type = MediaStreamType::Data;
            }
            _ => return None,
        }

        // Stream bitrate.
        let mut bitrate = stream_info
            .bit_rate
            .as_deref()
            .and_then(|b| b.trim().parse::<i32>().ok())
            .unwrap_or(0);

        // FLAC audio bitrate lives in the format info.
        if bitrate == 0
            && is_audio
            && stream.stream_type == MediaStreamType::Audio
            && let Some(format) = format_info
            && let Some(value) = format
                .bit_rate
                .as_deref()
                .and_then(|b| b.trim().parse::<i32>().ok())
        {
            bitrate = value;
        }

        if bitrate > 0 {
            stream.bit_rate = Some(bitrate);
        }

        // Fall back to BPS / (NUMBER_OF_BYTES, DURATION) tags.
        if stream.bit_rate.is_none()
            && matches!(
                stream_info.codec_type,
                Some(CodecType::Audio | CodecType::Video)
            )
        {
            if let Some(bps) = get_bps_from_tags(flat_tags.as_ref()).filter(|&b| b > 0) {
                stream.bit_rate = Some(bps);
            } else {
                let duration = get_runtime_seconds_from_tags(flat_tags.as_ref());
                let bytes = get_number_of_bytes_from_tags(flat_tags.as_ref());
                if let (Some(duration), Some(bytes)) = (duration, bytes)
                    && duration >= 1.0
                {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                    let bps = ((bytes as f64) * 8.0 / duration).round() as i32;
                    if bps > 0 {
                        stream.bit_rate = Some(bps);
                    }
                }
            }
        }

        if let Some(disposition) = stream_info.disposition.as_ref() {
            stream.is_default = disposition.get("default").copied() == Some(1);
            stream.is_forced = disposition.get("forced").copied() == Some(1);
            stream.is_hearing_impaired = disposition.get("hearing_impaired").copied() == Some(1);
        }

        normalize_stream_title(&mut stream);

        Some(stream)
    }
}

fn split_distinct_artists(val: &str, delimiters: &[char], split_featuring: bool) -> Vec<String> {
    let mut val = val.to_owned();
    if split_featuring {
        val = replace_ignore_case(&val, " featuring ", ARTIST_REPLACE_VALUE);
        val = replace_ignore_case(&val, " feat. ", ARTIST_REPLACE_VALUE);
    }

    let mut artists_found: Vec<String> = Vec::new();
    for whitelist_artist in SPLIT_WHITELIST {
        let original = val.clone();
        val = replace_ignore_case(&val, whitelist_artist, "|");
        if !original.eq_ignore_ascii_case(&val) {
            artists_found.push((*whitelist_artist).to_owned());
        }
    }

    for artist in val.split(delimiters) {
        let trimmed = artist.trim();
        if !trimmed.is_empty() {
            artists_found.push(trimmed.to_owned());
        }
    }

    distinct_names(artists_found)
}

fn fetch_studios(info: &mut MediaInfo, tags: &CaseInsensitiveTags, tag_name: &str) {
    let Some(val) = get_dictionary_value(tags, tag_name).filter(|s| !s.is_empty()) else {
        return;
    };

    let studios = split(val, NAME_DELIMITERS, true);
    let mut studio_list: Vec<String> = Vec::new();
    for studio in studios {
        let studio = studio.trim();
        if studio.is_empty() {
            continue;
        }
        let in_artists = info.artists.iter().any(|a| a.eq_ignore_ascii_case(studio));
        let in_album_artists = info
            .album_artists
            .iter()
            .any(|a| a.eq_ignore_ascii_case(studio));
        if in_artists || in_album_artists {
            continue;
        }
        studio_list.push(studio.to_owned());
    }

    info.studios = distinct_ignore_case(studio_list);
}

fn fetch_genres(info: &mut MediaInfo, tags: &CaseInsensitiveTags) {
    let Some(genre_val) = get_dictionary_value(tags, "genre").filter(|s| !s.is_empty()) else {
        return;
    };

    let mut genres: Vec<String> = info.genres.clone();
    for genre in split(genre_val, NAME_DELIMITERS, true) {
        if genre.is_empty() {
            continue;
        }
        genres.push(genre.to_owned());
    }

    info.genres = distinct_ignore_case(genres);
}

fn set_audio_info_from_tags(audio: &mut MediaInfo, tags: &CaseInsensitiveTags) {
    let mut people: Vec<BaseItemPerson> = Vec::new();

    add_people(&mut people, tags, "composer", PersonKind::Composer);
    add_people(&mut people, tags, "conductor", PersonKind::Conductor);
    add_people(&mut people, tags, "lyricist", PersonKind::Lyricist);

    if let Some(performer) =
        get_dictionary_value(tags, "performer").filter(|s| !s.trim().is_empty())
    {
        for person in split(performer, NAME_DELIMITERS, false) {
            if let Some(caps) = PERFORMER_REGEX.captures(person) {
                people.push(person_with_role(
                    caps.get(1).map_or("", |m| m.as_str()),
                    PersonKind::Actor,
                    Some(title_case(caps.get(2).map_or("", |m| m.as_str()))),
                ));
            }
        }
    }

    add_people(&mut people, tags, "writer", PersonKind::Writer);
    add_people(&mut people, tags, "arranger", PersonKind::Arranger);
    add_people(&mut people, tags, "engineer", PersonKind::Engineer);
    add_people(&mut people, tags, "mixer", PersonKind::Mixer);
    add_people(&mut people, tags, "remixer", PersonKind::Remixer);

    audio.people = people;

    // Album artist.
    let album_artist = get_first_not_blank(tags, &["albumartist", "album artist", "album_artist"]);
    audio.album_artists = album_artist
        .map(|a| split_distinct_artists(&a, NAME_DELIMITERS, true))
        .unwrap_or_default();
    if audio.album_artists.is_empty() {
        audio.album_artists = audio.artists.clone();
    }

    audio.index_number = get_dictionary_track_or_disc_number(tags, "track");
    audio.parent_index_number = get_dictionary_track_or_disc_number(tags, "disc");

    fetch_studios(audio, tags, "organization");
    fetch_studios(audio, tags, "ensemble");
    fetch_studios(audio, tags, "publisher");
    fetch_studios(audio, tags, "label");

    set_musicbrainz_ids(audio, tags);
}

/// The MusicBrainz provider id ← its embedded-tag key variants (underscore,
/// spaced, and mka `track.*` forms), a verbatim port of `AudioFileProber`'s tag
/// reads. The tag map is case-insensitive, so one case per spelling suffices.
const MUSICBRAINZ_TAGS: &[(&str, &[&str])] = &[
    (
        "MusicBrainzAlbumArtist",
        &[
            "MUSICBRAINZ_ALBUMARTISTID",
            "MusicBrainz Album Artist Id",
            "track.musicbrainz_album_artist_id",
        ],
    ),
    (
        "MusicBrainzArtist",
        &[
            "MUSICBRAINZ_ARTISTID",
            "MusicBrainz Artist Id",
            "track.musicbrainz_artist_id",
        ],
    ),
    (
        "MusicBrainzAlbum",
        &[
            "MUSICBRAINZ_ALBUMID",
            "MusicBrainz Album Id",
            "track.musicbrainz_album_id",
        ],
    ),
    (
        "MusicBrainzReleaseGroup",
        &[
            "MUSICBRAINZ_RELEASEGROUPID",
            "MusicBrainz Release Group Id",
            "track.musicbrainz_release_group_id",
        ],
    ),
    (
        "MusicBrainzTrack",
        &[
            "MUSICBRAINZ_RELEASETRACKID",
            "MusicBrainz Release Track Id",
            "track.musicbrainz_release_track_id",
        ],
    ),
    (
        "MusicBrainzRecording",
        &[
            "MUSICBRAINZ_TRACKID",
            "MusicBrainz Track Id",
            "track.musicbrainz_track_id",
        ],
    ),
];

/// Reads the six embedded MusicBrainz ids into `provider_ids`. When a tag is
/// multi-valued (separated by the internal unit separator), the **first** id is
/// kept — Jellyfin's behavior.
fn set_musicbrainz_ids(audio: &mut MediaInfo, tags: &CaseInsensitiveTags) {
    for (provider, keys) in MUSICBRAINZ_TAGS {
        if let Some(raw) = get_first_not_blank(tags, keys) {
            let first = raw
                .split(['\u{001F}', ';', '/'])
                .map(str::trim)
                .find(|s| !s.is_empty());
            if let Some(id) = first {
                audio
                    .provider_ids
                    .entry((*provider).to_owned())
                    .or_insert_with(|| id.to_owned());
            }
        }
    }
}

/// Estimates the missing single-video-stream bitrate as the container bitrate
/// minus the other (non-external) streams' bitrates (#16248).
fn estimate_missing_video_bitrate(info: &mut MediaInfo) {
    let container_bitrate = info.media_source.bitrate;
    let video_indices: Vec<usize> = info
        .media_source
        .media_streams
        .iter()
        .enumerate()
        .filter(|(_, s)| s.stream_type == MediaStreamType::Video)
        .map(|(i, _)| i)
        .collect();

    let (Some(container_bitrate), [video_idx]) = (container_bitrate, video_indices.as_slice())
    else {
        return;
    };

    if info.media_source.media_streams[*video_idx]
        .bit_rate
        .is_some()
    {
        return;
    }

    let others: Vec<&MediaStream> = info
        .media_source
        .media_streams
        .iter()
        .filter(|s| s.stream_type != MediaStreamType::Video && !s.is_external)
        .collect();

    let audio_bitrates_known = others
        .iter()
        .filter(|s| s.stream_type == MediaStreamType::Audio)
        .all(|s| s.bit_rate.is_some());

    if audio_bitrates_known {
        let sum: i32 = others.iter().map(|s| s.bit_rate.unwrap_or(0)).sum();
        let estimated = container_bitrate - sum;
        if estimated > 0 {
            info.media_source.media_streams[*video_idx].bit_rate = Some(estimated);
        }
    }
}

// The branch bodies are intentionally identical assignments over distinct
// conditions; the chain mirrors the upstream anamorphic-detection ladder.
#[allow(clippy::if_same_then_else)]
fn set_anamorphic(stream: &mut MediaStream, info: &MediaStreamInfo) {
    let sar = info.sample_aspect_ratio.as_deref();
    let dar = info.display_aspect_ratio.as_deref();

    if sar.is_none_or(str::is_empty) && dar.is_none_or(str::is_empty) {
        stream.is_anamorphic = Some(false);
    } else if is_near_square_pixel_sar(sar) {
        stream.is_anamorphic = Some(false);
    } else if sar != Some("0:1") {
        stream.is_anamorphic = Some(true);
    } else if dar == Some("0:1") {
        stream.is_anamorphic = Some(false);
    } else {
        // Force GetAspectRatio to derive the ratio from Width/Height only.
        let derived = get_aspect_ratio(&MediaStreamInfo {
            width: info.width,
            height: info.height,
            display_aspect_ratio: None,
            ..Default::default()
        });
        stream.is_anamorphic = Some(dar != derived.as_deref());
    }
}

fn normalize_stream_title(stream: &mut MediaStream) {
    if stream
        .title
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case("cc"))
        || stream.stream_type == MediaStreamType::EmbeddedImage
    {
        stream.title = None;
    }
}

fn normalize_subtitle_codec(codec: &str) -> String {
    if codec.eq_ignore_ascii_case("dvb_subtitle") {
        "DVBSUB".to_owned()
    } else if codec.eq_ignore_ascii_case("dvb_teletext") {
        "DVBTXT".to_owned()
    } else if codec.eq_ignore_ascii_case("dvd_subtitle") {
        "DVDSUB".to_owned()
    } else if codec.eq_ignore_ascii_case("hdmv_pgs_subtitle") {
        "PGSSUB".to_owned()
    } else {
        codec.to_owned()
    }
}

fn get_media_attachment(stream_info: &MediaStreamInfo) -> Option<MediaAttachment> {
    let is_attached_pic = stream_info
        .disposition
        .as_ref()
        .and_then(|d| d.get("attached_pic"))
        .copied()
        == Some(1);
    if stream_info.codec_type != Some(CodecType::Attachment) && !is_attached_pic {
        return None;
    }

    let mut attachment = MediaAttachment {
        codec: stream_info.codec_name.clone(),
        index: stream_info.index,
        ..Default::default()
    };

    if let Some(tag) = stream_info
        .codec_tag_string
        .as_deref()
        .filter(|t| !t.trim().is_empty())
    {
        attachment.codec_tag = Some(tag.to_owned());
    }

    if let Some(tags) = stream_info.tags.as_ref().map(flatten_tags) {
        attachment.file_name = get_dictionary_value(&tags, "filename").map(str::to_owned);
        attachment.mime_type = get_dictionary_value(&tags, "mimetype").map(str::to_owned);
        attachment.comment = get_dictionary_value(&tags, "comment").map(str::to_owned);
    }

    Some(attachment)
}

fn normalize_format(format: Option<&str>, media_streams: &[MediaStream]) -> Option<String> {
    let format = format?;
    if format.trim().is_empty() {
        return None;
    }

    let mut parts: Vec<String> = format.split(',').map(str::to_owned).collect();
    for part in &mut parts {
        if part.eq_ignore_ascii_case("mpegvideo") {
            "mpeg".clone_into(part);
        } else if part.eq_ignore_ascii_case("mpegts") {
            "ts".clone_into(part);
        } else if part.eq_ignore_ascii_case("matroska") {
            "mkv".clone_into(part);
        } else if part.eq_ignore_ascii_case("webm") {
            let disqualified = media_streams.iter().any(|s| {
                !matches!(
                    s.stream_type,
                    MediaStreamType::Video | MediaStreamType::Audio
                )
            }) || media_streams.iter().any(|s| {
                let codec = s.codec.as_deref().unwrap_or("");
                (s.stream_type == MediaStreamType::Video
                    && !WEBM_VIDEO_CODECS
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(codec)))
                    || (s.stream_type == MediaStreamType::Audio
                        && !WEBM_AUDIO_CODECS
                            .iter()
                            .any(|c| c.eq_ignore_ascii_case(codec)))
            });
            if disqualified {
                part.clear();
            }
        }
    }

    Some(
        parts
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// Estimated fallback audio bitrate — mirrors `GetEstimatedAudioBitrate`.
#[must_use]
pub fn get_estimated_audio_bitrate(
    codec: Option<&str>,
    profile: Option<&str>,
    channels: Option<i32>,
) -> Option<i32> {
    let codec = codec.filter(|c| !c.is_empty())?;
    let channel_count = channels.filter(|&c| c >= 1)?;
    let is_multichannel = channel_count > 2;

    match codec.to_ascii_lowercase().as_str() {
        "aac" | "mp3" | "mp2" => Some(if is_multichannel { 320_000 } else { 192_000 }),
        "ac3" | "eac3" => Some(if is_multichannel { 640_000 } else { 192_000 }),
        "dts" | "dca" => Some(if is_dts_lossless(profile) {
            channel_count * 700_000
        } else if is_multichannel {
            1_509_000
        } else {
            768_000
        }),
        "opus" => Some(if is_multichannel { 256_000 } else { 128_000 }),
        "vorbis" => Some(if is_multichannel { 320_000 } else { 160_000 }),
        "wmav1" | "wmav2" | "wmapro" => Some(if is_multichannel { 384_000 } else { 192_000 }),
        "flac" | "alac" => Some(channel_count * 480_000),
        "truehd" | "mlp" => Some(channel_count * 700_000),
        _ => None,
    }
}

fn is_dts_lossless(profile: Option<&str>) -> bool {
    profile.is_some_and(|p| contains_ignore_case(p, "HD MA"))
}

/// Whether a sample aspect ratio is (near-)square — mirrors
/// `IsNearSquarePixelSar`.
#[must_use]
pub fn is_near_square_pixel_sar(sar: Option<&str>) -> bool {
    let Some(sar) = sar.filter(|s| !s.is_empty()) else {
        return false;
    };

    let parts: Vec<&str> = sar.split(':').collect();
    if parts.len() == 2
        && let (Ok(num), Ok(den)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>())
        && den > 0.0
    {
        return is_close(num / den, 1.0, 0.01);
    }

    sar == "1:1"
}

/// Parses an ffprobe frame-rate fraction — mirrors `GetFrameRate`.
#[must_use]
pub fn get_frame_rate(value: Option<&str>) -> Option<f32> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    let index = value.find('/')?;
    let dividend = value[..index].parse::<f32>().ok()?;
    let divisor = value[index + 1..].parse::<f32>().ok()?;
    if divisor == 0.0 {
        None
    } else {
        Some(dividend / divisor)
    }
}

fn get_aspect_ratio(info: &MediaStreamInfo) -> Option<String> {
    let original = info.display_aspect_ratio.clone();

    let parts: Vec<&str> = original.as_deref().unwrap_or("").split(':').collect();
    let (mut width, mut height) = (0i64, 0i64);
    let parsed = parts.len() == 2
        && parts[0].parse::<i64>().map(|w| width = w).is_ok()
        && parts[1].parse::<i64>().map(|h| height = h).is_ok()
        && width > 0
        && height > 0;

    if !parsed {
        width = info.width.unwrap_or(0).into();
        height = info.height.unwrap_or(0).into();
    }

    if width > 0 && height > 0 {
        #[allow(clippy::cast_precision_loss)]
        let ratio = width as f64 / height as f64;

        if is_close(ratio, 1.777_777_778, 0.03) {
            return Some("16:9".to_owned());
        }
        if is_close(ratio, 1.333_333_333_3, 0.05) {
            return Some("4:3".to_owned());
        }
        if is_close(ratio, 1.41, 0.005) {
            return Some("1.41:1".to_owned());
        }
        if is_close(ratio, 1.5, 0.005) {
            return Some("1.5:1".to_owned());
        }
        if is_close(ratio, 1.6, 0.005) {
            return Some("1.6:1".to_owned());
        }
        if is_close(ratio, 1.666_666_666_67, 0.005) {
            return Some("5:3".to_owned());
        }
        if is_close(ratio, 1.85, 0.02) {
            return Some("1.85:1".to_owned());
        }
        if is_close(ratio, 2.35, 0.025) {
            return Some("2.35:1".to_owned());
        }
        if is_close(ratio, 2.4, 0.025) {
            return Some("2.40:1".to_owned());
        }
    }

    original
}

fn is_close(d1: f64, d2: f64, variance: f64) -> bool {
    (d1 - d2).abs() <= variance
}

fn parse_channel_layout(input: Option<&str>) -> Option<String> {
    let input = input.filter(|s| !s.is_empty())?;
    Some(left_part(input, '(').to_owned())
}

fn set_size(data: &InternalMediaInfoResult, info: &mut MediaInfo) {
    if let Some(format) = data.format.as_ref() {
        info.media_source.size = format
            .size
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<i64>().ok());
    }
}

fn set_audio_runtime_ticks(result: &InternalMediaInfoResult, data: &mut MediaInfo) {
    let Some(streams) = result.streams.as_ref() else {
        return;
    };
    let Some(stream) = streams
        .iter()
        .find(|s| s.codec_type == Some(CodecType::Audio))
    else {
        return;
    };

    let mut duration = stream.duration.clone();
    if duration.as_deref().is_none_or(str::is_empty) {
        duration = result.format.as_ref().and_then(|f| f.duration.clone());
    }

    if let Some(duration) = duration.as_deref().filter(|d| !d.is_empty())
        && let Ok(seconds) = duration.parse::<f64>()
    {
        data.media_source.run_time_ticks = Some(seconds_to_ticks(seconds));
    }
}

fn get_bps_from_tags(tags: Option<&CaseInsensitiveTags>) -> Option<i32> {
    let tags = tags?;
    let bps =
        get_dictionary_value(tags, "BPS-eng").or_else(|| get_dictionary_value(tags, "BPS"))?;
    bps.trim().parse::<i32>().ok()
}

fn get_runtime_seconds_from_tags(tags: Option<&CaseInsensitiveTags>) -> Option<f64> {
    let tags = tags?;
    let duration = get_dictionary_value(tags, "DURATION-eng")
        .or_else(|| get_dictionary_value(tags, "DURATION"))
        .filter(|d| !d.is_empty())?;
    let trimmed = DURATION_OVERPRECISION_REGEX.replace(duration, "$1");
    parse_timespan_seconds(&trimmed)
}

fn get_number_of_bytes_from_tags(tags: Option<&CaseInsensitiveTags>) -> Option<i64> {
    let tags = tags?;
    let bytes = get_dictionary_value(tags, "NUMBER_OF_BYTES-eng")
        .or_else(|| get_dictionary_value(tags, "NUMBER_OF_BYTES"))?;
    bytes.trim().parse::<i64>().ok()
}

/// Parses an `HH:MM:SS(.fffffff)` timespan into total seconds.
fn parse_timespan_seconds(value: &str) -> Option<f64> {
    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let hours = parts[0].parse::<f64>().ok()?;
    let minutes = parts[1].parse::<f64>().ok()?;
    let seconds = parts[2].parse::<f64>().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn get_dictionary_track_or_disc_number(tags: &CaseInsensitiveTags, tag_name: &str) -> Option<i32> {
    let val = get_dictionary_value(tags, tag_name).unwrap_or("");
    left_part(val, '/').trim().parse::<i32>().ok()
}

fn get_chapter_info(chapter: &super::dtos::MediaChapter) -> ChapterInfo {
    let mut info = ChapterInfo::default();
    if let Some(tags) = chapter.tags.as_ref()
        && let Some(Some(name)) = tags.get("title")
    {
        info.name = Some(name.clone());
    }

    if let Some(seconds) = chapter
        .start_time
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
    {
        let ms = (seconds * 1000.0).round();
        #[allow(clippy::cast_possible_truncation)]
        {
            info.start_position_ticks = (ms * 10_000.0) as i64;
        }
    }

    info
}

fn fetch_wtv_info(video: &mut MediaInfo, data: &InternalMediaInfoResult) {
    let Some(tags) = data
        .format
        .as_ref()
        .and_then(|f| f.tags.as_ref())
        .map(flatten_tags)
    else {
        return;
    };

    if let Some(genres) = get_dictionary_value(&tags, "WM/Genre").filter(|s| !s.trim().is_empty()) {
        let genre_list: Vec<String> = genres
            .split(GENRE_DELIMITERS)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if !genre_list.is_empty() {
            video.genres = genre_list;
        }
    }

    if let Some(rating) =
        get_dictionary_value(&tags, "WM/ParentalRating").filter(|s| !s.trim().is_empty())
    {
        video.official_rating = Some(rating.to_owned());
    }

    if let Some(people) = get_dictionary_value(&tags, "WM/MediaCredits").filter(|s| !s.is_empty()) {
        video.people = people
            .split(BASIC_DELIMITERS)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|name| person_with_role(name, PersonKind::Actor, None))
            .collect();
    }

    if let Some(year) = get_dictionary_value(&tags, "WM/OriginalReleaseTime")
        .and_then(|y| y.trim().parse::<i32>().ok())
    {
        video.production_year = Some(year);
    }

    // Upstream bug preserved: parses the year string, not the broadcast string.
    if get_dictionary_value(&tags, "WM/MediaOriginalBroadcastDateTime").is_some()
        && let Some(dt) = get_dictionary_value(&tags, "WM/OriginalReleaseTime")
            .and_then(super::ff_probe_helpers::parse_flexible_date_time)
    {
        video.premiere_date = Some(dt);
    }

    let description = get_dictionary_value(&tags, "WM/SubTitleDescription").map(str::to_owned);
    let sub_title = get_dictionary_value(&tags, "WM/SubTitle").map(str::to_owned);

    let mut description = description;
    if sub_title.as_deref().is_none_or(|s| s.trim().is_empty())
        && let Some(desc) = description.clone()
    {
        let limit = desc.len().min(100);
        if desc.get(..limit).is_some_and(|d| d.contains(':')) {
            let desc_parts: Vec<&str> = desc.split(':').collect();
            if !desc_parts.is_empty() {
                let episode_subtitle = desc_parts[0];
                if episode_subtitle.contains('/') {
                    let subtitle_parts: Vec<&str> = episode_subtitle.split(' ').collect();
                    let cleaned = subtitle_parts[0].replace('.', "");
                    if let Some(idx) = cleaned
                        .split('/')
                        .next()
                        .and_then(|n| n.parse::<i32>().ok())
                    {
                        video.index_number = Some(idx);
                    }
                    description = Some(subtitle_parts[1..].join(" ").trim().to_owned());
                } else if episode_subtitle.contains('.') {
                    let subtitle_parts: Vec<&str> = episode_subtitle.split('.').collect();
                    description = Some(subtitle_parts[1..].join(".").trim().to_owned());
                } else {
                    description = Some(episode_subtitle.trim().to_owned());
                }
            }
        }
    }

    if let Some(desc) = description.filter(|d| !d.trim().is_empty()) {
        video.overview = Some(desc);
    }
}

/// Parses iTunes `iTunMOVI` plist XML into `Studios`/`People`.
fn fetch_from_itunes_info(xml: &str, info: &mut MediaInfo) {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut xml = xml.to_owned();
    if let Some(idx) = xml.to_ascii_lowercase().find("<plist") {
        xml = xml[idx..].to_owned();
    }

    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);

    // Flatten to a sequence of (tag, text) and reconstruct the key/array shape.
    let mut current_key: Option<String> = None;
    let mut in_array = false;
    let mut collecting: Vec<String> = Vec::new();
    let mut last_element: Option<String> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "array" {
                    in_array = true;
                    collecting.clear();
                }
                last_element = Some(name);
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "array" {
                    in_array = false;
                    if let Some(key) = current_key.take() {
                        process_pairs(&key, &collecting, info);
                    }
                    collecting.clear();
                }
                last_element = None;
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().trim().to_owned();
                if text.is_empty() {
                    continue;
                }
                match last_element.as_deref() {
                    Some("key") => {
                        // A new key ends the previous scalar key/value pair.
                        if let Some(prev) = current_key.take()
                            && !collecting.is_empty()
                        {
                            process_pairs(&prev, &collecting, info);
                        }
                        current_key = Some(text);
                        collecting.clear();
                    }
                    Some("string") if (in_array || current_key.is_some()) => {
                        collecting.push(text);
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    if let Some(key) = current_key.take()
        && !collecting.is_empty()
    {
        process_pairs(&key, &collecting, info);
    }
}

fn process_pairs(key: &str, values: &[String], info: &mut MediaInfo) {
    let distinct: Vec<String> = distinct_ignore_case(
        values
            .iter()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .collect(),
    );

    if key.eq_ignore_ascii_case("studio") {
        info.studios = distinct;
    } else if key.eq_ignore_ascii_case("screenwriters") {
        info.people = distinct
            .into_iter()
            .map(|n| person_with_role(&n, PersonKind::Writer, None))
            .collect();
    } else if key.eq_ignore_ascii_case("producers") {
        info.people = distinct
            .into_iter()
            .map(|n| person_with_role(&n, PersonKind::Producer, None))
            .collect();
    } else if key.eq_ignore_ascii_case("directors") {
        info.people = distinct
            .into_iter()
            .map(|n| person_with_role(&n, PersonKind::Director, None))
            .collect();
    }
}

fn add_people(
    people: &mut Vec<BaseItemPerson>,
    tags: &CaseInsensitiveTags,
    key: &str,
    kind: PersonKind,
) {
    if let Some(val) = get_dictionary_value(tags, key).filter(|s| !s.trim().is_empty()) {
        for person in split(val, NAME_DELIMITERS, false) {
            people.push(person_with_role(person, kind, None));
        }
    }
}

fn person_with_role(name: &str, kind: PersonKind, role: Option<String>) -> BaseItemPerson {
    BaseItemPerson {
        name: Some(name.to_owned()),
        id: Uuid::nil(),
        role,
        type_: kind,
        primary_image_tag: None,
        image_blur_hashes: None,
    }
}

/// C# `Split`: split on name delimiters, or on comma only when no name
/// delimiter is present and `allow_comma_delimiter` is set.
fn split<'a>(val: &'a str, name_delimiters: &[char], allow_comma_delimiter: bool) -> Vec<&'a str> {
    let has_name_delim = name_delimiters.iter().any(|d| val.contains(*d));
    let delims: &[char] = if !allow_comma_delimiter || has_name_delim {
        name_delimiters
    } else {
        &[',']
    };
    val.split(delims)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

#[allow(clippy::cast_possible_truncation)]
fn seconds_to_ticks(seconds: f64) -> i64 {
    (seconds * 10_000_000.0).round() as i64
}

fn left_part(s: &str, delimiter: char) -> &str {
    match s.find(delimiter) {
        Some(idx) => &s[..idx],
        None => s,
    }
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn replace_ignore_case(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_owned();
    }
    let lower_hay = haystack.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut result = String::with_capacity(haystack.len());
    let mut start = 0;
    while let Some(pos) = lower_hay[start..].find(&lower_needle) {
        let abs = start + pos;
        result.push_str(&haystack[start..abs]);
        result.push_str(replacement);
        start = abs + needle.len();
    }
    result.push_str(&haystack[start..]);
    result
}

/// Case-insensitive distinct that keeps the first occurrence and drops empties.
fn distinct_names(values: Vec<String>) -> Vec<String> {
    distinct_ignore_case(
        values
            .into_iter()
            .filter(|v| !v.trim().is_empty())
            .collect(),
    )
}

fn distinct_ignore_case(values: Vec<String>) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result = Vec::new();
    for v in values {
        if seen.insert(v.to_ascii_lowercase()) {
            result.push(v);
        }
    }
    result
}

fn get_first_not_blank(tags: &CaseInsensitiveTags, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(val) = get_dictionary_value(tags, key).filter(|s| !s.trim().is_empty()) {
            return Some(val.to_owned());
        }
    }
    None
}

/// Title-cases each whitespace-separated word (invariant-culture `ToTitleCase`).
fn title_case(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    input
        .split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod musicbrainz_tests {
    use super::{MediaInfo, set_musicbrainz_ids};
    use crate::probing::ff_probe_helpers::CaseInsensitiveTags;

    /// The tag map stores keys lowercased (as `flatten_tags` produces them).
    fn tags(pairs: &[(&str, &str)]) -> CaseInsensitiveTags {
        pairs
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn reads_all_six_ids_across_key_spellings() {
        // underscore, spaced, and mka `track.*` spellings all resolve.
        let t = tags(&[
            ("MUSICBRAINZ_ARTISTID", "artist-mbid"),
            ("MusicBrainz Album Artist Id", "albumartist-mbid"),
            ("track.musicbrainz_album_id", "album-mbid"),
            ("MUSICBRAINZ_RELEASEGROUPID", "rg-mbid"),
            ("MUSICBRAINZ_RELEASETRACKID", "reltrack-mbid"),
            ("MUSICBRAINZ_TRACKID", "recording-mbid"),
        ]);
        let mut info = MediaInfo::default();
        set_musicbrainz_ids(&mut info, &t);
        assert_eq!(info.provider_ids["MusicBrainzArtist"], "artist-mbid");
        assert_eq!(
            info.provider_ids["MusicBrainzAlbumArtist"],
            "albumartist-mbid"
        );
        assert_eq!(info.provider_ids["MusicBrainzAlbum"], "album-mbid");
        assert_eq!(info.provider_ids["MusicBrainzReleaseGroup"], "rg-mbid");
        assert_eq!(info.provider_ids["MusicBrainzTrack"], "reltrack-mbid");
        assert_eq!(info.provider_ids["MusicBrainzRecording"], "recording-mbid");
    }

    #[test]
    fn takes_first_of_a_multivalue_id_and_skips_blanks() {
        let t = tags(&[
            ("MUSICBRAINZ_ARTISTID", "first-mbid\u{001F}second-mbid"),
            ("MUSICBRAINZ_ALBUMID", "   "),
        ]);
        let mut info = MediaInfo::default();
        set_musicbrainz_ids(&mut info, &t);
        assert_eq!(info.provider_ids["MusicBrainzArtist"], "first-mbid");
        assert!(!info.provider_ids.contains_key("MusicBrainzAlbum"));
    }
}
