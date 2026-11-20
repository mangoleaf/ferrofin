//! The dummy-chapter generator of `MediaBrowser.Providers.MediaInfo.FFProbeVideoInfo`.
//!
//! [`FFProbeVideoInfo::create_dummy_chapters`] is transliterated verbatim (it is
//! the parity oracle for this unit — see the xUnit `FFProbeVideoInfoTests`).
//!
//! The probe (`GetMediaInfo`) and `Fetch` belong to the library scan
//! (`ferrofin-core`), which owns the item rows: the probed container, runtime,
//! size, dimensions and streams, the external streams, and the container tags
//! and people (`FetchEmbeddedInfo` / `FetchPeople`, applied before the metadata
//! providers merge). The encoder probes a DVD folder rip
//! through its title's `.vob` files; a Blu-ray folder rip's probe is an open
//! work item (gl-1834).

use ferrofin_model::entities_media::ChapterInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_util::ticks::TICKS_PER_SECOND;

/// The upper runtime bound `CreateDummyChapters` treats as valid, in ticks
/// (`TimeSpan.FromHours(12).Ticks`). Runtimes above this are assumed corrupt.
///
/// Port of the literal `TimeSpan.FromHours(12).Ticks` guard.
const MAX_VALID_RUNTIME_TICKS: i64 = 12 * 60 * 60 * TICKS_PER_SECOND;

/// The item subset [`FFProbeVideoInfo::create_dummy_chapters`] reads — the
/// fields of `MediaBrowser.Controller.Entities.Video` it touches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoProbeInput {
    /// The item name (`Name`) — used only for the invalid-runtime error message.
    pub name: Option<String>,
    /// The total runtime, in ticks (`RunTimeTicks`).
    pub run_time_ticks: Option<i64>,
}

/// The dummy-chapter generator of `FFProbeVideoInfo`.
pub struct FFProbeVideoInfo {
    dummy_chapter_duration_seconds: i32,
}

impl FFProbeVideoInfo {
    /// Creates a generator for `dummy_chapter_duration_seconds`, the
    /// `ServerConfiguration.DummyChapterDuration` value (in seconds)
    /// [`Self::create_dummy_chapters`] reads; a value of `0` disables
    /// dummy-chapter generation upstream.
    #[must_use]
    pub fn new(dummy_chapter_duration_seconds: i32) -> Self {
        Self {
            dummy_chapter_duration_seconds,
        }
    }

    /// Generates evenly-spaced placeholder chapters for `video`.
    ///
    /// Port of `FFProbeVideoInfo.CreateDummyChapters` (transliterated verbatim —
    /// the xUnit `FFProbeVideoInfoTests` oracle). Only files with a runtime in
    /// `0..=12h` are processed; a runtime outside that range is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::invalid_input`] when the runtime is negative or
    /// exceeds 12 hours (the corrupt-file guard); mirrors the C#
    /// `ArgumentException`.
    pub fn create_dummy_chapters(
        &self,
        video: &VideoProbeInput,
    ) -> Result<Vec<ChapterInfo>, ServiceError> {
        let runtime = video.run_time_ticks.unwrap_or_default();

        // Only process files with a runtime greater than 0 and less than 12h.
        // The latter are likely corrupted.
        if !(0..=MAX_VALID_RUNTIME_TICKS).contains(&runtime) {
            // C# formats `TimeSpan.FromTicks(runtime).TotalMinutes`.
            let total_minutes =
                f64::from(i32::try_from(runtime / TICKS_PER_SECOND).unwrap_or(0)) / 60.0;
            return Err(ServiceError::invalid_input(format!(
                "{} has an invalid runtime of {total_minutes} minutes",
                video.name.as_deref().unwrap_or_default(),
            )));
        }

        let dummy_chapter_duration =
            i64::from(self.dummy_chapter_duration_seconds) * TICKS_PER_SECOND;

        if runtime <= 0 {
            return Ok(Vec::new());
        }

        // `runtime` is bounded by 12h of ticks and `dummy_chapter_duration >= 1s`
        // of ticks, so the quotient always fits in an `i32` (as the C# cast asserts).
        let chapter_count = std::cmp::max(1, runtime / dummy_chapter_duration);
        let mut chapters = Vec::with_capacity(usize::try_from(chapter_count).unwrap_or(0));

        let mut current_chapter_ticks: i64 = 0;
        for _ in 0..chapter_count {
            chapters.push(ChapterInfo {
                start_position_ticks: current_chapter_ticks,
                ..ChapterInfo::default()
            });

            current_chapter_ticks += dummy_chapter_duration;
        }

        Ok(chapters)
    }
}

#[cfg(test)]
mod tests {
    use super::{FFProbeVideoInfo, VideoProbeInput};
    use ferrofin_util::ticks::TICKS_PER_MINUTE;

    /// The `DummyChapterDuration` the xUnit fixture injects:
    /// `TimeSpan.FromMinutes(5).TotalSeconds` == 300 seconds.
    const DUMMY_CHAPTER_DURATION_SECONDS: i32 = 5 * 60;

    fn provider() -> FFProbeVideoInfo {
        FFProbeVideoInfo::new(DUMMY_CHAPTER_DURATION_SECONDS)
    }

    fn video(run_time_ticks: Option<i64>) -> VideoProbeInput {
        VideoProbeInput {
            run_time_ticks,
            ..VideoProbeInput::default()
        }
    }

    // ---- Transliteration of Jellyfin.Providers.Tests FFProbeVideoInfoTests ----

    /// `CreateDummyChapters_InvalidRuntime_ThrowsArgumentException`.
    #[test]
    fn create_dummy_chapters_invalid_runtime_throws() {
        for runtime in [Some(-1_i64), Some(i64::MIN), Some(i64::MAX)] {
            let err = provider()
                .create_dummy_chapters(&video(runtime))
                .expect_err("invalid runtime is rejected");
            // C# throws ArgumentException → we map to a bad-request ServiceError.
            let _ = err;
        }
    }

    /// `CreateDummyChapters_ValidRuntime_CorrectChaptersCount`.
    #[test]
    fn create_dummy_chapters_valid_runtime_correct_count() {
        let cases: [(Option<i64>, usize); 7] = [
            (None, 0),
            (Some(0), 0),
            (Some(1), 1),
            (Some(TICKS_PER_MINUTE * 3), 1),
            (Some(TICKS_PER_MINUTE * 5), 1),
            (Some(TICKS_PER_MINUTE * 5 + 1), 1),
            (Some(TICKS_PER_MINUTE * 50), 10),
        ];

        for (runtime, expected) in cases {
            let chapters = provider()
                .create_dummy_chapters(&video(runtime))
                .expect("valid runtime produces chapters");
            assert_eq!(chapters.len(), expected, "runtime = {runtime:?}");
        }
    }

    /// `CreateDummyChapters_PositiveRuntime_NoChapterBeyondRuntime`.
    #[test]
    fn create_dummy_chapters_positive_runtime_no_chapter_beyond_runtime() {
        for runtime in [
            1_i64,
            TICKS_PER_MINUTE * 3,
            TICKS_PER_MINUTE * 5,
            TICKS_PER_MINUTE * 5 + 1,
            TICKS_PER_MINUTE * 50 + 1,
        ] {
            let chapters = provider()
                .create_dummy_chapters(&video(Some(runtime)))
                .expect("positive runtime produces chapters");
            for chapter in &chapters {
                assert!(
                    chapter.start_position_ticks < runtime,
                    "chapter at {} must be < runtime {runtime}",
                    chapter.start_position_ticks
                );
            }
        }
    }
}
