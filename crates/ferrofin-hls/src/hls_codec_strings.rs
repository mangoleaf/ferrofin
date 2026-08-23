//! HLS `CODECS` attribute strings — port of
//! `Jellyfin.Api/Helpers/HlsCodecStringHelpers.cs` (10.11.8).
//!
//! Helpers to generate HLS codec strings according to
//! [RFC 6381 section 3.3](https://datatracker.ietf.org/doc/html/rfc6381#section-3.3)
//! and the [MP4 Registration Authority](https://mp4ra.org). Pure functions, no
//! state; every table is copied verbatim from the C#.

/// Codec name for MP3 (`HlsCodecStringHelpers.MP3`).
pub const MP3: &str = "mp4a.40.34";

/// Codec name for AC-3 (`HlsCodecStringHelpers.AC3`).
pub const AC3: &str = "ac-3";

/// Codec name for E-AC-3 (`HlsCodecStringHelpers.EAC3`).
pub const EAC3: &str = "ec-3";

/// Codec name for FLAC (`HlsCodecStringHelpers.FLAC`).
pub const FLAC: &str = "fLaC";

/// Codec name for ALAC (`HlsCodecStringHelpers.ALAC`).
pub const ALAC: &str = "alac";

/// Codec name for OPUS (`HlsCodecStringHelpers.OPUS`).
pub const OPUS: &str = "Opus";

/// Gets an AAC codec string. Port of `GetAACString(profile)`: `HE`
/// (case-insensitive) → `mp4a.40.5`, anything else (including an invalid or
/// absent profile) → the LC default `mp4a.40.2`.
#[must_use]
pub fn aac_string(profile: Option<&str>) -> String {
    let object_type = if profile.is_some_and(|p| p.eq_ignore_ascii_case("HE")) {
        ".40.5"
    } else {
        // Default to LC if profile is invalid
        ".40.2"
    };
    format!("mp4a{object_type}")
}

/// Gets a H.264 codec string. Port of `GetH264String(profile, level)`:
/// `avc1` + the profile's `profile_idc`/constraint byte (`high` → `6400`,
/// `main` → `4D40`, `baseline` → `42E0`, anything else → the constrained
/// baseline `4240`) + the level as two upper-case hex digits (`41` → `29`).
#[must_use]
pub fn h264_string(profile: Option<&str>, level: i32) -> String {
    let profile = profile.unwrap_or_default();
    let profile_idc = if profile.eq_ignore_ascii_case("high") {
        ".6400"
    } else if profile.eq_ignore_ascii_case("main") {
        ".4D40"
    } else if profile.eq_ignore_ascii_case("baseline") {
        ".42E0"
    } else {
        // Default to constrained baseline if profile is invalid
        ".4240"
    };
    // `level.ToString("X2")`: upper-case hex, at least two digits.
    format!("avc1{profile_idc}{level:02X}")
}

/// Gets a H.265 codec string. Port of `GetH265String(profile, level)`:
/// `hvc1` + `.2.4` for `main10`/`main 10` (else the `.1.4` Main default) +
/// `.L{level}.B0` — e.g. `hvc1.1.4.L120.B0`.
#[must_use]
pub fn h265_string(profile: Option<&str>, level: i32) -> String {
    // The h265 syntax is a bit of a mystery at the time this comment was written.
    // This is what I've found through various sources:
    // FORMAT: [codecTag].[profile].[constraint?].L[level * 30].[UNKNOWN]
    let profile = profile.unwrap_or_default();
    let profile_part =
        if profile.eq_ignore_ascii_case("main10") || profile.eq_ignore_ascii_case("main 10") {
            ".2.4"
        } else {
            // Default to main if profile is invalid
            ".1.4"
        };
    format!("hvc1{profile_part}.L{level}.B0")
}

/// Gets a VP9 codec string. Port of
/// `GetVp9String(width, height, pixelFormat, framerate, bitDepth)` (see
/// <https://www.webmproject.org/vp9/mp4/>): `vp09.{profile}.{level}.{bitDepth}`
/// with the profile from the pixel format, the level from the luma picture
/// size and luma sample rate tables, and a bit depth outside `{8, 10, 12}`
/// defaulting to 8.
#[must_use]
pub fn vp9_string(
    width: i32,
    height: i32,
    pixel_format: Option<&str>,
    framerate: f32,
    bit_depth: i32,
) -> String {
    // The upstream table lists `yuv420p`/`yuvj420p` explicitly before the `00`
    // fallback; kept verbatim rather than folded into the default arm.
    #[allow(
        clippy::match_same_arms,
        reason = "verbatim upstream pixel-format table"
    )]
    let profile_string = match pixel_format.unwrap_or_default() {
        "yuv420p" | "yuvj420p" => "00",
        "yuv422p" | "yuv444p" => "01",
        "yuv420p10le" | "yuv420p12le" => "02",
        "yuv422p10le" | "yuv422p12le" | "yuv444p10le" | "yuv444p12le" => "03",
        _ => "00",
    };

    // C#: `int * int` (wrapping) for the picture size, then `int * float` — a
    // SINGLE-precision product compared against the table's thresholds (every
    // one of which is exactly representable in f32). Computing in f32 keeps the
    // bucket choice bit-identical at the edges; an f64 product put a handful of
    // odd dimensions (e.g. 4206x3108@90) one level above Jellyfin.
    let luma_picture_size = width.wrapping_mul(height);
    #[allow(
        clippy::cast_precision_loss,
        reason = "the C# `int * float` multiply is single precision by design"
    )]
    let luma_sample_rate = luma_picture_size as f32 * framerate;
    let level_string = match luma_picture_size {
        s if s <= 0 => "00",
        s if s <= 36_864 => "10",
        s if s <= 73_728 => "11",
        s if s <= 122_880 => "20",
        s if s <= 245_760 => "21",
        s if s <= 552_960 => "30",
        s if s <= 983_040 => "31",
        s if s <= 2_228_224 => {
            if luma_sample_rate <= 83_558_400.0_f32 {
                "40"
            } else {
                "41"
            }
        }
        s if s <= 8_912_896 => {
            if luma_sample_rate <= 311_951_360.0_f32 {
                "50"
            } else if luma_sample_rate <= 588_251_136.0_f32 {
                "51"
            } else {
                "52"
            }
        }
        s if s <= 35_651_584 => {
            if luma_sample_rate <= 1_176_502_272.0_f32 {
                "60"
            } else if luma_sample_rate <= 4_706_009_088.0_f32 {
                "61"
            } else {
                "62"
            }
        }
        _ => "00", // This should not happen
    };

    let bit_depth = if matches!(bit_depth, 8 | 10 | 12) {
        bit_depth
    } else {
        // Default to 8 bits
        8
    };

    // `bitDepth.ToString("D2")`.
    format!("vp09.{profile_string}.{level_string}.{bit_depth:02}")
}

/// Gets an AV1 codec string. Port of
/// `GetAv1String(profile, level, tierFlag, bitDepth)` (see
/// <https://aomediacodec.github.io/av1-isobmff/#codecsparam>):
/// `av01.{profile}.{level:02}{M|H}.{bitDepth:02}` — `Main`/`High`/
/// `Professional` → `0`/`1`/`2` (else Main), a level outside `1..=31`
/// defaults to 19 (level 6.3), a bit depth outside `{8, 10, 12}` to 8.
#[must_use]
pub fn av1_string(profile: Option<&str>, level: i32, tier_flag: bool, bit_depth: i32) -> String {
    // FORMAT: [codecTag].[profile].[level][tier].[bitDepth]
    let profile = profile.unwrap_or_default();
    let profile_part = if profile.eq_ignore_ascii_case("Main") {
        ".0"
    } else if profile.eq_ignore_ascii_case("High") {
        ".1"
    } else if profile.eq_ignore_ascii_case("Professional") {
        ".2"
    } else {
        // Default to Main
        ".0"
    };

    let level = if level <= 0 || level > 31 {
        // Default to the maximum defined level 6.3
        19
    } else {
        level
    };

    let bit_depth = if matches!(bit_depth, 8 | 10 | 12) {
        bit_depth
    } else {
        // Default to 8 bits
        8
    };

    let tier = if tier_flag { 'H' } else { 'M' };
    // Needed to pad it double digits; otherwise, browsers will reject the stream.
    format!("av01{profile_part}.{level:02}{tier}.{bit_depth:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // No upstream xUnit file exists for `HlsCodecStringHelpers` in 10.11.8; the
    // cases below are derived directly from its tables.

    #[rstest]
    #[case(None, "mp4a.40.2")]
    #[case(Some(""), "mp4a.40.2")]
    #[case(Some("LC"), "mp4a.40.2")]
    #[case(Some("HE"), "mp4a.40.5")]
    #[case(Some("he"), "mp4a.40.5")]
    #[case(Some("bogus"), "mp4a.40.2")]
    fn aac(#[case] profile: Option<&str>, #[case] expected: &str) {
        assert_eq!(aac_string(profile), expected);
    }

    #[rstest]
    #[case(None, 41, "avc1.424029")]
    #[case(Some(""), 41, "avc1.424029")]
    #[case(Some("high"), 51, "avc1.640033")]
    #[case(Some("High"), 41, "avc1.640029")]
    #[case(Some("Main"), 30, "avc1.4D401E")]
    #[case(Some("baseline"), 31, "avc1.42E01F")]
    #[case(Some("high"), 10, "avc1.64000A")]
    #[case(Some("constrained baseline"), 40, "avc1.424028")]
    // A negative level renders as C#'s `ToString("X2")` two's complement —
    // deliberately not "fixed": callers never pass one (the level is parsed
    // from a non-negative string), and the output is what upstream would emit.
    #[case(Some("high"), -1, "avc1.6400FFFFFFFF")]
    fn h264(#[case] profile: Option<&str>, #[case] level: i32, #[case] expected: &str) {
        assert_eq!(h264_string(profile, level), expected);
    }

    #[rstest]
    #[case(Some("main"), 120, "hvc1.1.4.L120.B0")]
    #[case(None, 150, "hvc1.1.4.L150.B0")]
    #[case(Some("Main 10"), 153, "hvc1.2.4.L153.B0")]
    #[case(Some("main10"), 120, "hvc1.2.4.L120.B0")]
    fn h265(#[case] profile: Option<&str>, #[case] level: i32, #[case] expected: &str) {
        assert_eq!(h265_string(profile, level), expected);
    }

    #[rstest]
    #[case(1920, 1080, Some("yuv420p"), 30.0, 8, "vp09.00.40.08")]
    #[case(1920, 1080, Some("yuv420p"), 60.0, 8, "vp09.00.41.08")]
    #[case(3840, 2160, Some("yuv420p10le"), 60.0, 10, "vp09.02.51.10")]
    #[case(3840, 2160, Some("yuv420p10le"), 30.0, 10, "vp09.02.50.10")]
    #[case(3840, 2160, Some("yuv422p10le"), 120.0, 12, "vp09.03.52.12")]
    #[case(7680, 4320, Some("yuv444p"), 30.0, 8, "vp09.01.60.08")]
    #[case(7680, 4320, Some("yuv444p"), 60.0, 8, "vp09.01.61.08")]
    #[case(7680, 4320, Some("yuv444p"), 144.0, 8, "vp09.01.62.08")]
    #[case(640, 480, Some("yuvj420p"), 30.0, 8, "vp09.00.30.08")]
    #[case(0, 0, Some("x"), 0.0, 9, "vp09.00.00.08")]
    #[case(320, 240, None, 10.0, 8, "vp09.00.20.08")]
    // Exact `<=` boundaries of the upstream tables: 36864 luma samples is still
    // level 1.0; 2228224 × 37.5 = 83558400 exactly is still 4.0 (not 4.1);
    // 8912896 luma at 35 fps = 311951360 exactly is still 5.0 (not 5.1).
    #[case(256, 144, Some("yuv420p"), 30.0, 8, "vp09.00.10.08")]
    #[case(2048, 1088, Some("yuv420p"), 37.5, 8, "vp09.00.40.08")]
    #[case(4096, 2176, Some("yuv420p"), 35.0, 8, "vp09.00.50.08")]
    #[case(4096, 2176, Some("yuv420p"), 36.0, 8, "vp09.00.51.08")]
    fn vp9(
        #[case] width: i32,
        #[case] height: i32,
        #[case] pixel_format: Option<&str>,
        #[case] framerate: f32,
        #[case] bit_depth: i32,
        #[case] expected: &str,
    ) {
        assert_eq!(
            vp9_string(width, height, pixel_format, framerate, bit_depth),
            expected
        );
    }

    #[rstest]
    #[case(Some("Main"), 19, false, 8, "av01.0.19M.08")]
    #[case(Some("High"), 0, true, 10, "av01.1.19H.10")]
    #[case(Some("Professional"), 32, false, 12, "av01.2.19M.12")]
    #[case(Some("bogus"), 5, false, 7, "av01.0.05M.08")]
    #[case(None, 31, true, 10, "av01.0.31H.10")]
    #[case(Some("main"), -1, false, 8, "av01.0.19M.08")]
    fn av1(
        #[case] profile: Option<&str>,
        #[case] level: i32,
        #[case] tier_flag: bool,
        #[case] bit_depth: i32,
        #[case] expected: &str,
    ) {
        assert_eq!(av1_string(profile, level, tier_flag, bit_depth), expected);
    }

    #[test]
    fn constants_match_upstream() {
        assert_eq!(MP3, "mp4a.40.34");
        assert_eq!(AC3, "ac-3");
        assert_eq!(EAC3, "ec-3");
        assert_eq!(FLAC, "fLaC");
        assert_eq!(ALAC, "alac");
        assert_eq!(OPUS, "Opus");
    }
}
