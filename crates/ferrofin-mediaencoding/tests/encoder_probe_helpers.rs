//! Coverage for the pure encoder/probing helpers: the `FfmpegVersion`
//! `System.Version` model, the `EncoderValidator` decision logic, the ffprobe
//! tag helpers, and the flexible bool/int ffprobe deserializers.
//!
//! All of this is deterministic string/number logic with no process I/O.

use std::collections::HashMap;

use ferrofin_mediaencoding::encoder::version::FfmpegVersion;
use ferrofin_mediaencoding::encoder::{EncoderValidator, MAX_VERSION, MIN_VERSION};
use ferrofin_mediaencoding::probing::InternalMediaInfoResult;
use ferrofin_mediaencoding::probing::ff_probe_helpers::{
    CaseInsensitiveTags, flatten_tags, get_dictionary_date_time, get_dictionary_numeric_value,
    get_dictionary_value, parse_flexible_date_time,
};

// -- FfmpegVersion (System.Version semantics) ------------------------------

#[test]
fn version_try_parse_component_counts() {
    assert_eq!(
        FfmpegVersion::try_parse("4.4"),
        Some(FfmpegVersion::new(4, 4))
    );
    assert_eq!(
        FfmpegVersion::try_parse("4.4.1"),
        Some(FfmpegVersion::with_build(4, 4, 1))
    );
    assert!(FfmpegVersion::try_parse("4.4.1.2").is_some());
    // Too few / too many components.
    assert_eq!(FfmpegVersion::try_parse("4"), None);
    assert_eq!(FfmpegVersion::try_parse("1.2.3.4.5"), None);
    // Non-numeric and negative components.
    assert_eq!(FfmpegVersion::try_parse("4.x"), None);
    assert_eq!(FfmpegVersion::try_parse("4.-1"), None);
    assert_eq!(FfmpegVersion::try_parse("4."), None);
}

#[test]
fn version_ordering_matches_dotnet_unspecified_component_rule() {
    // Version(4,4) has build -1, sorts BEFORE Version(4,4,0) whose build is 0.
    assert!(FfmpegVersion::new(4, 4) < FfmpegVersion::with_build(4, 4, 0));
    assert!(FfmpegVersion::new(4, 3) < FfmpegVersion::new(4, 4));
    assert!(FfmpegVersion::with_build(5, 0, 0) > FfmpegVersion::new(4, 9));
    assert_eq!(FfmpegVersion::new(4, 4), FfmpegVersion::new(4, 4));
}

#[test]
fn version_display_only_emits_specified_components() {
    assert_eq!(FfmpegVersion::new(4, 4).to_string(), "4.4");
    assert_eq!(FfmpegVersion::with_build(4, 4, 1).to_string(), "4.4.1");
    let full = FfmpegVersion::try_parse("4.4.1.2").unwrap();
    assert_eq!(full.to_string(), "4.4.1.2");
}

// -- EncoderValidator decision logic ---------------------------------------

#[test]
fn validator_reports_configured_path() {
    let v = EncoderValidator::new("/usr/bin/ffmpeg");
    assert_eq!(v.encoder_path(), "/usr/bin/ffmpeg");
}

#[test]
fn validator_rejects_avconv_output() {
    let v = EncoderValidator::new("ffmpeg");
    let out = "ffmpeg version 4.4 Copyright (c) the Libav developers";
    assert!(!v.validate_version_internal(out));
}

#[test]
fn validator_rejects_unparseable_version() {
    let v = EncoderValidator::new("ffmpeg");
    assert!(!v.validate_version_internal("no version here at all"));
}

#[test]
fn validator_rejects_below_minimum() {
    let v = EncoderValidator::new("ffmpeg");
    // A version well below the recommended minimum.
    let out = "ffmpeg version 2.0 Copyright (c) 2000-2013 the FFmpeg developers";
    assert!(!v.validate_version_internal(out));
}

#[test]
fn validator_accepts_in_range_prebuilt_version() {
    let v = EncoderValidator::new("ffmpeg");
    // Construct a version string from the recommended minimum so it's in-range
    // regardless of what MIN/MAX are pinned to.
    let out = format!("ffmpeg version {MIN_VERSION} Copyright (c) the FFmpeg developers");
    assert!(v.validate_version_internal(&out));
    // Sanity: the pinned range is coherent.
    if let Some(max) = MAX_VERSION {
        assert!(MIN_VERSION <= max);
    }
}

// -- ffprobe tag helpers ---------------------------------------------------

fn tags(pairs: &[(&str, &str)]) -> CaseInsensitiveTags {
    let mut m = CaseInsensitiveTags::new();
    for (k, val) in pairs {
        m.insert((*k).to_owned(), (*val).to_owned());
    }
    m
}

#[test]
fn get_dictionary_value_is_case_insensitive() {
    let t = tags(&[("title", "Hello")]);
    assert_eq!(get_dictionary_value(&t, "TITLE"), Some("Hello"));
    assert_eq!(get_dictionary_value(&t, "Title"), Some("Hello"));
    assert_eq!(get_dictionary_value(&t, "missing"), None);
}

#[test]
fn get_dictionary_numeric_value_trims_and_parses() {
    let t = tags(&[("track", "  7 "), ("bad", "x")]);
    assert_eq!(get_dictionary_numeric_value(&t, "TRACK"), Some(7));
    assert_eq!(get_dictionary_numeric_value(&t, "bad"), None);
    assert_eq!(get_dictionary_numeric_value(&t, "absent"), None);
}

#[test]
fn flatten_tags_drops_nulls_and_lowercases_keys_first_wins() {
    let mut raw: HashMap<String, Option<String>> = HashMap::new();
    raw.insert("Artist".to_owned(), Some("A".to_owned()));
    raw.insert("Missing".to_owned(), None);
    let flat = flatten_tags(&raw);
    assert_eq!(get_dictionary_value(&flat, "artist"), Some("A"));
    assert_eq!(get_dictionary_value(&flat, "missing"), None);
}

#[test]
fn get_dictionary_date_time_handles_year_and_full_timestamp() {
    let t = tags(&[("date", " 2021 "), ("created", "2021-06-15T12:30:00Z")]);
    let year = get_dictionary_date_time(&t, "date").unwrap();
    assert_eq!(year.format("%Y-%m-%d").to_string(), "2021-01-01");
    let full = get_dictionary_date_time(&t, "created").unwrap();
    assert_eq!(
        full.format("%Y-%m-%dT%H:%M:%S").to_string(),
        "2021-06-15T12:30:00"
    );
}

#[test]
fn parse_flexible_date_time_covers_all_shapes() {
    assert!(parse_flexible_date_time("").is_none());
    assert!(parse_flexible_date_time("not a date").is_none());
    assert!(parse_flexible_date_time("2021-06-15T12:30:00").is_some());
    assert!(parse_flexible_date_time("2021-06-15 12:30:00").is_some());
    assert!(parse_flexible_date_time("2021-06-15T12:30Z").is_some());
    assert!(parse_flexible_date_time("2021-06-15T12:30").is_some());
    assert!(parse_flexible_date_time("2021-06-15").is_some());
    assert!(parse_flexible_date_time("2021").is_some());
    assert!(parse_flexible_date_time("2021-06-15T12:30:00+02:00").is_some());
    // Four chars but not a year -> None.
    assert!(parse_flexible_date_time("abcd").is_none());
}

// -- flexible ffprobe deserializers ----------------------------------------

#[test]
fn dtos_de_bool_flexible_accepts_bool_and_strings() {
    // is_avc on MediaStreamInfo uses de_bool_flexible.
    let json = r#"{"streams":[
        {"is_avc": true},
        {"is_avc": "false"},
        {"is_avc": "1"},
        {"is_avc": "0"},
        {"is_avc": "maybe"},
        {}
    ]}"#;
    let r: InternalMediaInfoResult = serde_json::from_str(json).unwrap();
    let streams = r.streams.unwrap();
    // `IsAvc` is a non-nullable `bool` upstream, so an unrecognized or absent
    // value lands on the CLR default `false` rather than on null.
    assert!(streams[0].is_avc);
    assert!(!streams[1].is_avc);
    assert!(streams[2].is_avc);
    assert!(!streams[3].is_avc);
    assert!(!streams[4].is_avc); // unrecognized string
    assert!(!streams[5].is_avc); // absent
}

#[test]
fn dtos_de_int_flexible_accepts_int_string_and_defaults() {
    // bits_per_raw_sample uses de_int_flexible (defaults to 0).
    let json = r#"{"streams":[
        {"bits_per_raw_sample": 8},
        {"bits_per_raw_sample": "10"},
        {"bits_per_raw_sample": "junk"},
        {}
    ]}"#;
    let r: InternalMediaInfoResult = serde_json::from_str(json).unwrap();
    let streams = r.streams.unwrap();
    assert_eq!(streams[0].bits_per_raw_sample, 8);
    assert_eq!(streams[1].bits_per_raw_sample, 10);
    assert_eq!(streams[2].bits_per_raw_sample, 0); // unparseable -> 0
    assert_eq!(streams[3].bits_per_raw_sample, 0); // absent -> 0
}
