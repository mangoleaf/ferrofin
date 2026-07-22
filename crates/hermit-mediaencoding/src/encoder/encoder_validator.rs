//! Port of `MediaBrowser.MediaEncoding.Encoder.EncoderValidator`.
//!
//! The *pure* half is ported here: parsing the `ffmpeg -version` output into a
//! [`FfmpegVersion`] ([`EncoderValidator::get_ffmpeg_version_internal`]) and
//! validating it against the recommended [`MIN_VERSION`] plus the minimum
//! library-version cross-check ([`EncoderValidator::validate_version_internal`]).
//!
//! The capability-probe methods (`GetCodecs` / `GetHwaccels` / `GetFilters`)
//! and the hardware `Check*` device probes all shell out to ffmpeg, so they sit
//! behind the [`Transcoder`] seam (see [`crate::encoder::transcoder`]) and are
//! not implemented in this pure unit.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use super::version::FfmpegVersion;

/// The minimum recommended ffmpeg version (`4.4`).
///
/// When changing this, also change [`FFMPEG_MINIMUM_LIBRARY_VERSIONS`].
pub const MIN_VERSION: FfmpegVersion = FfmpegVersion::new(4, 4);

/// The maximum recommended ffmpeg version (unbounded — C# `MaxVersion` is `null`).
pub const MAX_VERSION: Option<FfmpegVersion> = None;

/// `^ffmpeg version n?((?:[0-9]+\.?)+)` — extracts the version from the first line.
static FFMPEG_VERSION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ffmpeg version n?((?:[0-9]+\.?)+)").expect("valid regex"));

/// `((?<name>lib\w+)\s+(?<major>[0-9]+)\.\s*(?<minor>[0-9]+))` (multiline) — matches
/// each `libavcodec 58.134` style line.
static LIBRARY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)((?P<name>lib\w+)\s+(?P<major>[0-9]+)\.\s*(?P<minor>[0-9]+))")
        .expect("valid regex")
});

/// The library versions corresponding to the minimum ffmpeg version 4.4.
///
/// Refers to the versions in <https://ffmpeg.org/download.html>. Used to work
/// out the ffmpeg version when the version string is missing from the output.
static FFMPEG_MINIMUM_LIBRARY_VERSIONS: LazyLock<HashMap<&'static str, FfmpegVersion>> =
    LazyLock::new(|| {
        HashMap::from([
            ("libavutil", FfmpegVersion::new(56, 70)),
            ("libavcodec", FfmpegVersion::new(58, 134)),
            ("libavformat", FfmpegVersion::new(58, 76)),
            ("libavdevice", FfmpegVersion::new(58, 13)),
            ("libavfilter", FfmpegVersion::new(7, 110)),
            ("libswscale", FfmpegVersion::new(5, 9)),
            ("libswresample", FfmpegVersion::new(3, 9)),
        ])
    });

/// Validates ffmpeg version output (the pure half of `EncoderValidator`).
///
/// Construct with the encoder path; the version-parsing/validation methods work
/// purely on captured `ffmpeg -version` text without spawning a process.
#[derive(Debug, Clone)]
pub struct EncoderValidator {
    encoder_path: String,
}

impl EncoderValidator {
    /// Creates a validator for the given `ffmpeg` executable path.
    #[must_use]
    pub fn new(encoder_path: impl Into<String>) -> Self {
        Self {
            encoder_path: encoder_path.into(),
        }
    }

    /// The `ffmpeg` executable path this validator was configured with.
    #[must_use]
    pub fn encoder_path(&self) -> &str {
        &self.encoder_path
    }

    /// Validates captured `ffmpeg -version` output against the recommended range.
    ///
    /// Returns `false` for avconv (Libav) output, an unparseable version, or a
    /// version below [`MIN_VERSION`] / above [`MAX_VERSION`]. Mirrors C#
    /// `ValidateVersionInternal`.
    #[must_use]
    pub fn validate_version_internal(&self, version_output: &str) -> bool {
        if version_output
            .to_ascii_lowercase()
            .contains("libav developers")
        {
            // avconv instead of ffmpeg is not supported
            return false;
        }

        // Work out what the version under test is
        let Some(version) = self.get_ffmpeg_version_internal(version_output) else {
            // Version is unknown
            return false;
        };

        if version < MIN_VERSION {
            // Version is below what we recommend
            return false;
        }

        if let Some(max) = MAX_VERSION
            && version > max
        {
            // Version is above what we recommend
            return false;
        }

        true
    }

    /// Works out the ffmpeg version from `ffmpeg -version` output.
    ///
    /// For pre-built binaries the version is at the very start of the output and
    /// is parsed directly. Otherwise the library versions are matched against
    /// [`FFMPEG_MINIMUM_LIBRARY_VERSIONS`]; if every required library is present
    /// and at least its minimum, [`MIN_VERSION`] is returned, else `None`.
    /// Mirrors C# `GetFFmpegVersionInternal`.
    #[must_use]
    pub fn get_ffmpeg_version_internal(&self, output: &str) -> Option<FfmpegVersion> {
        // For pre-built binaries the FFmpeg version should be mentioned at the very start of the output
        if let Some(caps) = FFMPEG_VERSION_REGEX.captures(output)
            && let Some(result) = FfmpegVersion::try_parse(&caps[1])
        {
            return Some(result);
        }

        let version_map = Self::get_ffmpeg_library_versions(output);

        let mut all_versions_validated = true;

        for (library, minimum_version) in FFMPEG_MINIMUM_LIBRARY_VERSIONS.iter() {
            match version_map.get(*library) {
                Some(found_version) if *found_version >= *minimum_version => {}
                _ => all_versions_validated = false,
            }
        }

        if all_versions_validated {
            Some(MIN_VERSION)
        } else {
            None
        }
    }

    /// Grabs the library names and `major.minor` versions from `ffmpeg -version`
    /// output. Mirrors C# `GetFFmpegLibraryVersions`.
    fn get_ffmpeg_library_versions(output: &str) -> HashMap<String, FfmpegVersion> {
        let mut map = HashMap::new();

        for caps in LIBRARY_REGEX.captures_iter(output) {
            let major: i32 = caps["major"].parse().expect("regex guarantees digits");
            let minor: i32 = caps["minor"].parse().expect("regex guarantees digits");
            let version = FfmpegVersion::new(major, minor);
            map.insert(caps["name"].to_owned(), version);
        }

        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    use super::super::test_data as d;

    fn validator() -> EncoderValidator {
        EncoderValidator::new("ffmpeg")
    }

    #[rstest]
    #[case(d::FFMPEG_V701_OUTPUT, Some(FfmpegVersion::with_build(7, 0, 1)))]
    #[case(d::FFMPEG_V611_OUTPUT, Some(FfmpegVersion::with_build(6, 1, 1)))]
    #[case(d::FFMPEG_V60_OUTPUT, Some(FfmpegVersion::new(6, 0)))]
    #[case(d::FFMPEG_V512_OUTPUT, Some(FfmpegVersion::with_build(5, 1, 2)))]
    #[case(d::FFMPEG_V44_OUTPUT, Some(FfmpegVersion::new(4, 4)))]
    #[case(d::FFMPEG_V432_OUTPUT, Some(FfmpegVersion::with_build(4, 3, 2)))]
    #[case(d::FFMPEG_GIT_UNKNOWN_OUTPUT2, Some(FfmpegVersion::new(4, 4)))]
    #[case(
        d::FFMPEG_GIT_WITHOUT_LIBPOSTPROC_OUTPUT,
        Some(FfmpegVersion::new(4, 4))
    )]
    #[case(d::FFMPEG_GIT_UNKNOWN_OUTPUT, None)]
    fn get_ffmpeg_version_test(
        #[case] version_output: &str,
        #[case] version: Option<FfmpegVersion>,
    ) {
        assert_eq!(
            version,
            validator().get_ffmpeg_version_internal(version_output)
        );
    }

    #[rstest]
    #[case(d::FFMPEG_V701_OUTPUT, true)]
    #[case(d::FFMPEG_V611_OUTPUT, true)]
    #[case(d::FFMPEG_V60_OUTPUT, true)]
    #[case(d::FFMPEG_V512_OUTPUT, true)]
    #[case(d::FFMPEG_V44_OUTPUT, true)]
    #[case(d::FFMPEG_V432_OUTPUT, false)]
    #[case(d::FFMPEG_GIT_UNKNOWN_OUTPUT2, true)]
    #[case(d::FFMPEG_GIT_WITHOUT_LIBPOSTPROC_OUTPUT, true)]
    #[case(d::FFMPEG_GIT_UNKNOWN_OUTPUT, false)]
    fn validate_version_internal_test(#[case] version_output: &str, #[case] valid: bool) {
        assert_eq!(valid, validator().validate_version_internal(version_output));
    }
}
