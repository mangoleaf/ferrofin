//! The Intro Skipper extension — Ferrofin's first built-in extension.
//!
//! Port of the Intro Skipper plugin's orchestration (`QueueManager` +
//! `BaseItemAnalyzerTask` + the I/O half of `ChromaprintAnalyzer`,
//! GPL-3.0-only): group episodes by season, fingerprint each one's intro and
//! credits windows, compare them within the season via [`ferrofin_chromaprint`],
//! and write the shared regions as `Intro`/`Outro`
//! [`MediaSegmentDto`](ferrofin_model::media_segments::MediaSegmentDto)s — which is
//! what makes jellyfin-web show the "Skip Intro" / "Skip Credits" button.
//!
//! It surfaces on `/Plugins` as "Intro Skipper"; its analysis runs as the
//! `IntroSkipper.Detect` scheduled task and as the provider behind the
//! "Media Segment Scan" dashboard task (`TaskExtractMediaSegments`). The task
//! self-gates on the plugin's enabled flag and no-ops when Chromaprint
//! (`fpcalc`) is absent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use ferrofin_chromaprint::{AnalysisMode, CompareConfig, TimeRange, compare_episodes};
use ferrofin_core::{PluginConfigPage, ScheduledTask, TaskProgress};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::media_segments::MediaSegmentManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fingerprint::Fingerprinter;
use crate::{Extension, ExtensionContext};

/// The Intro Skipper's stable plugin id (also its `/Plugins` id) — the
/// **upstream plugin's GUID**, because the vendored dashboard app and third-party
/// client integrations address the plugin by that id.
const EXTENSION_ID: Uuid = Uuid::from_u128(0xc83d_86bb_a1e0_4c35_a113_e210_1cf4_ee6b);

/// The media-segment provider id stamped on segments this extension writes, so a
/// re-run replaces only its own rows (never user-authored ones).
const PROVIDER_ID: &str = "IntroSkipper";

/// 100-nanosecond ticks per second (Jellyfin's `MediaSegment` time unit).
const TICKS_PER_SECOND: f64 = 10_000_000.0;

/// Converts seconds to 100-nanosecond ticks (segment start/end are far below the
/// `i64` range, so the truncation is intentional and lossless in practice).
#[allow(clippy::cast_possible_truncation)]
fn secs_to_ticks(secs: f64) -> i64 {
    (secs * TICKS_PER_SECOND) as i64
}

/// Converts 100-nanosecond ticks to seconds (episode runtimes are well within
/// `f64`'s exact-integer range).
#[allow(clippy::cast_precision_loss)]
fn ticks_to_secs(ticks: i64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND
}

/// The Intro Skipper extension.
#[derive(Debug, Default, Clone, Copy)]
pub struct IntroSkipperExtension;

impl IntroSkipperExtension {
    /// Creates the extension.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Extension for IntroSkipperExtension {
    fn id(&self) -> Uuid {
        EXTENSION_ID
    }

    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: EXTENSION_ID,
            name: "Intro Skipper".to_owned(),
            version: "1.0.0".to_owned(),
            description: "Detects TV episode intros and end credits (Chromaprint audio \
                          fingerprinting) and exposes them as media segments so clients show \
                          Skip Intro / Skip Credits."
                .to_owned(),
            enabled: true,
            has_image: false,
            can_uninstall: false,
        }
    }

    fn default_config(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(&IntroSkipperConfig::default()).unwrap_or_else(|_| b"{}".to_vec())
    }

    fn config_pages(&self) -> Vec<PluginConfigPage> {
        // The upstream plugin's own dashboard pages, vendored at build time
        // (see `build.rs`): the "Intro Skipper" shell page (main-menu-enabled,
        // vetoed at list time by the `EnableMainMenu` config toggle) plus the
        // built JS/CSS app it loads by name — so the settings page renders
        // exactly as on a Jellyfin-hosted install.
        vec![
            PluginConfigPage {
                name: "Intro Skipper".to_owned(),
                bytes: include_bytes!(concat!(env!("OUT_DIR"), "/introskipper/configPage.html"))
                    .to_vec(),
                enable_in_main_menu: true,
            },
            PluginConfigPage {
                name: "introskipper.js".to_owned(),
                bytes: include_bytes!(concat!(env!("OUT_DIR"), "/introskipper/introskipper.js"))
                    .to_vec(),
                enable_in_main_menu: false,
            },
            PluginConfigPage {
                name: "introskipper.css".to_owned(),
                bytes: include_bytes!(concat!(env!("OUT_DIR"), "/introskipper/introskipper.css"))
                    .to_vec(),
                enable_in_main_menu: false,
            },
        ]
    }

    fn tasks(&self, cx: &ExtensionContext) -> Vec<Arc<dyn ScheduledTask>> {
        let detect = Arc::new(DetectSegmentsTask {
            library: Arc::clone(&cx.library),
            media_segments: Arc::clone(&cx.media_segments),
            plugins: Arc::clone(&cx.plugins),
            fingerprinter: cx.fingerprinter.clone(),
            cache_dir: cx.cache_dir.join("introskipper"),
            running: Arc::new(AtomicBool::new(false)),
        });
        vec![
            Arc::clone(&detect) as Arc<dyn ScheduledTask>,
            Arc::new(MediaSegmentScanTask { detect }),
        ]
    }
}

/// The "Media Segment Scan" task — port of Jellyfin's
/// `MediaSegmentExtractionTask` (`TaskExtractMediaSegments`), which runs every
/// media-segment provider over the library. Ferrofin's one segment provider is
/// the Intro Skipper, so the scan drives the same detection pass as
/// [`DetectSegmentsTask`] (which self-gates on the plugin's enabled flag, the
/// `fpcalc` binary, and its one-pass-at-a-time latch; re-runs replace only the
/// provider's own segments and reuse the on-disk fingerprint cache).
struct MediaSegmentScanTask {
    detect: Arc<DetectSegmentsTask>,
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for MediaSegmentScanTask {
    fn key(&self) -> &str {
        "TaskExtractMediaSegments"
    }
    fn name(&self) -> &str {
        "Media Segment Scan"
    }
    fn description(&self) -> &str {
        "Extracts or obtains media segments from MediaSegment enabled plugins."
    }
    fn category(&self) -> &str {
        "Library"
    }
    fn default_triggers(&self) -> Vec<ferrofin_model::tasks::TaskTriggerInfo> {
        vec![ferrofin_model::tasks::TaskTriggerInfo {
            type_: ferrofin_model::tasks::TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(12 * 3600 * 10_000_000),
            ..ferrofin_model::tasks::TaskTriggerInfo::default()
        }]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        self.detect.execute(progress).await
    }
}

/// The Intro Skipper's configuration — the full schema of the upstream plugin
/// (`IntroSkipper/Configuration/PluginConfiguration.cs`), so the settings page is
/// 1:1 with it. Field names (via `PascalCase`) and defaults match the C# exactly;
/// `#[serde(default)]` lets a partial JSON fill the rest.
///
/// Not every field drives Ferrofin's current analysis pipeline (which fingerprints
/// intros/credits) — the black-frame, chapter, silence, and keyframe refiners are
/// stored for parity and consumed as those analyzers land. The knobs the pipeline
/// uses today: the `Scan*` toggles, the min/max durations, the analysis window
/// (`AnalysisPercent`/`AnalysisLengthLimit`), the fingerprint match parameters,
/// and the intro offsets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)] // a faithful mirror of the plugin's config
pub struct IntroSkipperConfig {
    // ---- General ----------------------------------------------------------
    /// Automatically analyze newly added media.
    pub auto_detect_intros: bool,
    /// Re-analyze a season once it has settled (no new episodes for a while).
    pub reanalyze_settled_seasons: bool,
    /// Hours without new episodes before a season is "settled".
    pub settled_season_delay_hours: i32,
    /// Update missing media segments during a library scan.
    pub update_media_segments: bool,
    /// Legacy comma-separated excluded series (superseded by `SeriesExclusions`).
    pub exclude_series: String,
    /// Series excluded from analysis (exact, case-insensitive names).
    pub series_exclusions: Vec<String>,
    /// Movies excluded from analysis.
    pub movie_exclusions: Vec<String>,
    /// Paths excluded from analysis.
    pub path_exclusions: Vec<String>,
    /// Detect introductions.
    pub scan_introduction: bool,
    /// Detect end credits.
    pub scan_credits: bool,
    /// Detect recaps.
    pub scan_recap: bool,
    /// Detect previews.
    pub scan_preview: bool,
    /// Detect commercials.
    pub scan_commercial: bool,
    /// Analyze Season 0 (specials / extras).
    pub analyze_season_zero: bool,
    /// Use the File Transformation plugin to patch the web UI.
    pub use_file_transformation_plugin: bool,
    /// Seconds before the client's skip button auto-hides (0 = never).
    pub skipbutton_hide_delay: i32,
    /// Show an Intro Skipper entry in the client's main menu.
    pub enable_main_menu: bool,

    // ---- Analysis ---------------------------------------------------------
    /// Prefer Chromaprint analysis over other analyzers.
    pub prefer_chromaprint: bool,
    /// Allow a chapter-derived segment to ignore the duration limits.
    pub full_length_chapters: bool,
    /// Percent of each item, from the start, to analyze (1–50).
    pub analysis_percent: i32,
    /// Hard cap (minutes) on the analysis window.
    pub analysis_length_limit: i32,
    /// Cache fingerprints to disk between runs.
    pub cache_fingerprints: bool,
    /// Minimum recap length (seconds).
    pub minimum_recap_duration: i32,
    /// Maximum recap length (seconds).
    pub maximum_recap_duration: i32,
    /// Minimum detected-recap length (seconds).
    pub minimum_recap_detection_duration: i32,
    /// Maximum detected-recap length (seconds).
    pub maximum_recap_detection_duration: i32,
    /// Minimum intro length (seconds) to accept.
    pub minimum_intro_duration: i32,
    /// Maximum intro length (seconds).
    pub maximum_intro_duration: i32,
    /// Minimum credits length (seconds).
    pub minimum_credits_duration: i32,
    /// Maximum credits length (seconds).
    pub maximum_credits_duration: i32,
    /// Maximum movie-credits length (seconds).
    pub maximum_movie_credits_duration: i32,
    /// Minimum preview length (seconds).
    pub minimum_preview_duration: i32,
    /// Maximum preview length (seconds).
    pub maximum_preview_duration: i32,
    /// Minimum commercial length (seconds).
    pub minimum_commercial_duration: i32,
    /// Maximum commercial length (seconds).
    pub maximum_commercial_duration: i32,

    // ---- Detection --------------------------------------------------------
    /// Adjust segment endpoints to the nearest silence point.
    pub adjust_intro_based_on_silence: bool,
    /// Silence noise tolerance (negative dB).
    pub silence_detection_maximum_noise: i32,
    /// Minimum silence duration (seconds) before adjusting.
    pub silence_detection_minimum_duration: f64,
    /// Adjust segment endpoints to the nearest video keyframe.
    pub snap_to_keyframe: bool,
    /// Adjust segment endpoints to the nearest chapter boundary.
    pub adjust_intro_based_on_chapters: bool,
    /// Seconds searched toward a segment's interior for an adjustment point.
    pub adjust_window_inward: f64,
    /// Seconds searched away from a segment for an adjustment point.
    pub adjust_window_outward: f64,
    /// Snap a segment start/end within this many seconds of the episode boundary.
    pub end_snap_threshold: f64,
    /// Ignore intros for the first episode of a season.
    pub skip_first_episode: bool,
    /// Restrict the first-episode-ignore rule to anime seasons.
    pub skip_first_episode_anime: bool,
    /// For anime with no detected preview, treat the after-credits scene as one.
    pub anime_preview_from_credits_end: bool,
    /// Seconds of the intro to play before the skip point.
    pub intro_start_offset: i32,
    /// Seconds before the intro end to resume playback.
    pub intro_end_offset: i32,

    // ---- Black Frame ------------------------------------------------------
    /// Fall back to black-frame detection for recaps.
    pub detect_recap_using_black_frames: bool,
    /// Use the experimental alternative black-frame analyzer.
    pub use_alternative_black_frame_analyzer: bool,
    /// Frame-level refine of the credits boundary (alt analyzer).
    pub refine_credits_boundary: bool,
    /// Also detect credits on a near-uniform (non-black) card.
    pub detect_non_black_credits: bool,
    /// Use chapter markers to locate credits via nearby black frames.
    pub use_chapter_markers_black_frame: bool,
    /// Minimum percent of black pixels for a frame to count as black.
    pub black_frame_minimum_percentage: i32,
    /// Luma value below which a pixel is considered black.
    pub black_frame_threshold: i32,

    // ---- Chapters ---------------------------------------------------------
    /// Regex identifying introduction chapters.
    pub chapter_analyzer_introduction_pattern: String,
    /// Regex identifying end-credits chapters.
    pub chapter_analyzer_end_credits_pattern: String,
    /// Regex identifying preview chapters.
    pub chapter_analyzer_preview_pattern: String,
    /// Regex identifying recap chapters.
    pub chapter_analyzer_recap_pattern: String,
    /// Regex identifying commercial chapters.
    pub chapter_analyzer_commercial_pattern: String,
    /// Also detect known SponsorBlock chapter labels.
    pub enable_sponsor_block_chapter_detection: bool,

    // ---- FFmpeg -----------------------------------------------------------
    /// Maximum simultaneous episode-analysis operations.
    pub max_parallelism: i32,
    /// ffmpeg process priority (`Idle`/`BelowNormal`/`Normal`/`AboveNormal`/`High`/`RealTime`).
    pub process_priority: String,
    /// ffmpeg threads (0 = auto).
    pub process_threads: i32,
    /// Probe the audio-stream duration for credits fingerprinting.
    pub probe_audio_duration: bool,
    /// Detection-cache compression (`NoCompression`/`Fastest`/`Optimal`/`SmallestSize`).
    pub cache_compression_level: String,

    // ---- Advanced matching (Analysis tab, advanced) -----------------------
    /// Max Hamming distance (bits) two fingerprint points may differ.
    pub maximum_fingerprint_point_differences: i32,
    /// Max gap (seconds) between matched points before a run breaks.
    pub maximum_time_skip: f64,
    /// Fuzzy point-value tolerance when matching points across episodes.
    pub inverted_index_shift: i32,

    /// Server-managed flag: whether the File Transformation plugin is present.
    /// Read-only from the dashboard's perspective; the File Transformation
    /// extension is compiled in, so this is always `true` (the settings page
    /// uses it to enable the `UseFileTransformationPlugin` toggle).
    pub file_transformation_plugin_enabled: bool,
}

impl Default for IntroSkipperConfig {
    fn default() -> Self {
        Self {
            // General
            auto_detect_intros: true,
            reanalyze_settled_seasons: false,
            settled_season_delay_hours: 24,
            update_media_segments: true,
            exclude_series: String::new(),
            series_exclusions: Vec::new(),
            movie_exclusions: Vec::new(),
            path_exclusions: Vec::new(),
            scan_introduction: true,
            scan_credits: true,
            scan_recap: true,
            scan_preview: true,
            scan_commercial: true,
            analyze_season_zero: false,
            use_file_transformation_plugin: false,
            skipbutton_hide_delay: 8,
            enable_main_menu: true,
            // Analysis
            prefer_chromaprint: false,
            full_length_chapters: false,
            analysis_percent: 25,
            analysis_length_limit: 10,
            cache_fingerprints: true,
            minimum_recap_duration: 15,
            maximum_recap_duration: 120,
            minimum_recap_detection_duration: 15,
            maximum_recap_detection_duration: 120,
            minimum_intro_duration: 15,
            maximum_intro_duration: 120,
            minimum_credits_duration: 15,
            maximum_credits_duration: 450,
            maximum_movie_credits_duration: 900,
            minimum_preview_duration: 15,
            maximum_preview_duration: 120,
            minimum_commercial_duration: 15,
            maximum_commercial_duration: 120,
            // Detection
            adjust_intro_based_on_silence: true,
            silence_detection_maximum_noise: -50,
            silence_detection_minimum_duration: 0.33,
            snap_to_keyframe: true,
            adjust_intro_based_on_chapters: true,
            adjust_window_inward: 5.0,
            adjust_window_outward: 2.0,
            end_snap_threshold: 2.0,
            skip_first_episode: false,
            skip_first_episode_anime: false,
            anime_preview_from_credits_end: false,
            intro_start_offset: 0,
            intro_end_offset: 0,
            // Black Frame
            detect_recap_using_black_frames: false,
            use_alternative_black_frame_analyzer: false,
            refine_credits_boundary: true,
            detect_non_black_credits: true,
            use_chapter_markers_black_frame: true,
            black_frame_minimum_percentage: 85,
            black_frame_threshold: 28,
            // Chapters
            chapter_analyzer_introduction_pattern:
                r"(^|\s)(Intro|Introduction|OP|Opening)(?![\s:]+End)(\s|:|$)".to_owned(),
            chapter_analyzer_end_credits_pattern:
                r"(^|\s)(Credits?|ED|Ending|Outro)(?![\s:]+End)(\s|:|$)".to_owned(),
            chapter_analyzer_preview_pattern: r"(^|\s)(Preview|PV|Sneak\s?Peek|Coming\s?(Up|Soon)|Next\s+(time|on|episode)|Extra|Teaser|Trailer)(?![\s:]+End)(\s|:|$)".to_owned(),
            chapter_analyzer_recap_pattern: r"(^|\s)(Re?cap|Sum{1,2}ary|Prev(ious(ly)?)?|(Last|Earlier)(\s\w+)?|Catch[ -]up)(?![\s:]+End)(\s|:|$)".to_owned(),
            chapter_analyzer_commercial_pattern:
                r"(^|\s)(Ad(vert(isement)?)?|Commercial|Intermission)(?![\s:]+End)(\s|:|$)".to_owned(),
            enable_sponsor_block_chapter_detection: true,
            // FFmpeg
            max_parallelism: 2,
            process_priority: "BelowNormal".to_owned(),
            process_threads: 0,
            probe_audio_duration: false,
            cache_compression_level: "Optimal".to_owned(),
            // Advanced matching
            maximum_fingerprint_point_differences: 6,
            maximum_time_skip: 3.5,
            inverted_index_shift: 2,
            // The File Transformation extension is compiled in, so it is
            // always "installed" (the upstream flag = plugin presence).
            file_transformation_plugin_enabled: true,
        }
    }
}

impl IntroSkipperConfig {
    /// The [`CompareConfig`] for a given mode.
    fn compare_config(&self, mode: AnalysisMode) -> CompareConfig {
        let min_seconds = match mode {
            AnalysisMode::Credits => self.minimum_credits_duration,
            AnalysisMode::Recap => self.minimum_recap_duration,
            AnalysisMode::Introduction => self.minimum_intro_duration,
        };
        CompareConfig {
            inverted_index_shift: self.inverted_index_shift,
            max_bit_diff: u32::try_from(self.maximum_fingerprint_point_differences).unwrap_or(6),
            max_time_skip: self.maximum_time_skip,
            min_region_duration: f64::from(min_seconds),
        }
    }
}

/// A single episode reduced to what the analyzer needs.
struct Episode {
    id: Uuid,
    path: String,
    duration_secs: f64,
}

/// The scheduled task that detects intros/credits across the library.
#[derive(Clone)]
struct DetectSegmentsTask {
    library: Arc<dyn LibraryManager>,
    media_segments: Arc<dyn MediaSegmentManager>,
    plugins: Arc<dyn PluginManager>,
    fingerprinter: Option<Arc<dyn Fingerprinter>>,
    cache_dir: PathBuf,
    /// `true` while an analysis pass is running, so a second trigger is a no-op
    /// instead of a duplicate concurrent pass.
    running: Arc<AtomicBool>,
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for DetectSegmentsTask {
    fn key(&self) -> &str {
        "IntroSkipper.Detect"
    }
    fn name(&self) -> &str {
        "Detect intros and credits"
    }
    fn description(&self) -> &str {
        "Fingerprints episode audio to find shared intros and end credits, writing them as \
         media segments (Skip Intro / Skip Credits)."
    }
    fn category(&self) -> &str {
        "Intro Skipper"
    }

    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        // Gate on the plugin being enabled (live toggle — no restart needed).
        if !self.enabled().await {
            tracing::debug!("intro skipper disabled; skipping analysis");
            return Ok(());
        }
        let Some(fingerprinter) = self.fingerprinter.clone() else {
            tracing::warn!(
                "intro skipper: fpcalc (Chromaprint) not found — install `chromaprint`/`fpcalc` \
                 to enable intro/credits detection"
            );
            return Ok(());
        };
        // One pass at a time (belt to the task manager's Running guard, since
        // `/Intros/ScanSeason` can also start the task).
        if self.running.swap(true, Ordering::SeqCst) {
            tracing::info!("intro skipper: an analysis pass is already running");
            return Ok(());
        }
        // Release the latch even if the run is aborted mid-await (the task
        // manager cancels a queued run by aborting its tokio task, which drops
        // this future at an await point without running the code after it).
        let _release = ReleaseOnDrop(Arc::clone(&self.running));
        let config = self.load_config().await;
        // Run inline: the task manager queues the execution on its own spawned
        // task, so this body's runtime IS the dashboard's Running state — a
        // fire-and-forget here would flip the task back to Idle in
        // milliseconds and the run button would appear to do nothing.
        self.run_analysis(progress, &fingerprinter, &config).await;
        Ok(())
    }
}

/// Clears the intro skipper's one-pass-at-a-time latch when dropped, so an
/// aborted (cancelled) analysis run cannot leave it stuck.
struct ReleaseOnDrop(Arc<AtomicBool>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl DetectSegmentsTask {
    /// The background analysis pass: enumerate episodes by season, then detect +
    /// write segments for each season with enough episodes to compare.
    #[tracing::instrument(name = "extension_run", skip_all, fields(extension = "intro_skipper"))]
    async fn run_analysis(
        &self,
        progress: &TaskProgress,
        fingerprinter: &Arc<dyn Fingerprinter>,
        config: &IntroSkipperConfig,
    ) {
        let seasons = match self.episodes_by_season().await {
            Ok(seasons) => seasons,
            Err(err) => {
                tracing::warn!(%err, "intro skipper: could not enumerate episodes");
                return;
            }
        };
        // Only seasons with a comparable pair do work; drive progress off those
        // so the percentage tracks the fingerprinting rather than the skipped
        // singletons.
        let analyzable: Vec<&Vec<Episode>> =
            seasons.values().filter(|eps| eps.len() >= 2).collect();
        let total = analyzable.len();
        tracing::info!(
            seasons = seasons.len(),
            analyzable = total,
            "intro skipper: analysis started"
        );
        let mut written = 0usize;
        for (idx, episodes) in analyzable.into_iter().enumerate() {
            // Log + report BEFORE the season's fingerprinting (the slow part), so
            // a stall shows up as the last "analyzing season N" line with silence
            // after — pinpointing which season's media reads wedged.
            #[allow(clippy::cast_precision_loss)]
            progress.report(100.0 * idx as f64 / total.max(1) as f64);
            tracing::info!(
                season = idx + 1,
                of = total,
                episodes = episodes.len(),
                segments = written,
                "intro skipper: analyzing season"
            );
            written += self
                .analyze_season(episodes, config, fingerprinter.as_ref())
                .await;
        }
        progress.report(100.0);
        tracing::info!(segments = written, "intro skipper: analysis complete");
    }

    /// Whether the extension's plugin is currently enabled.
    async fn enabled(&self) -> bool {
        matches!(
            self.plugins.get_plugin(EXTENSION_ID).await,
            Ok(Some(p)) if p.enabled
        )
    }

    /// Loads the persisted configuration, falling back to defaults.
    async fn load_config(&self) -> IntroSkipperConfig {
        match self.plugins.get_plugin_configuration(EXTENSION_ID).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => IntroSkipperConfig::default(),
        }
    }

    /// Every non-virtual episode grouped by season id (episodes without a season
    /// or a path are skipped).
    async fn episodes_by_season(&self) -> Result<HashMap<String, Vec<Episode>>, ServiceError> {
        let rows = self
            .library
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Episode],
                recursive: true,
                is_virtual_item: Some(false),
                ..InternalItemsQuery::default()
            })
            .await?;

        let mut by_season: HashMap<String, Vec<Episode>> = HashMap::new();
        for row in rows {
            if let Some(ep) = to_episode(&row) {
                by_season
                    .entry(row.season_id.unwrap_or_default())
                    .or_default()
                    .push(ep);
            }
        }
        Ok(by_season)
    }

    /// Fingerprints, compares and writes segments for one season's episodes,
    /// returning how many segments were written.
    async fn analyze_season(
        &self,
        episodes: &[Episode],
        config: &IntroSkipperConfig,
        fingerprinter: &dyn Fingerprinter,
    ) -> usize {
        let mut written = 0;
        if config.scan_introduction {
            let mut best = self
                .detect(episodes, config, fingerprinter, AnalysisMode::Introduction)
                .await;
            apply_intro_offsets(&mut best, config);
            written += self
                .write_segments(
                    &best,
                    MediaSegmentType::Intro,
                    f64::from(config.maximum_intro_duration),
                )
                .await;
        }
        if config.scan_credits {
            let best = self
                .detect(episodes, config, fingerprinter, AnalysisMode::Credits)
                .await;
            written += self
                .write_segments(
                    &best,
                    MediaSegmentType::Outro,
                    f64::from(config.maximum_credits_duration),
                )
                .await;
        }
        written
    }

    /// Fingerprints each episode's window for `mode`, compares every pair, and
    /// returns the longest valid shared region found per episode.
    async fn detect(
        &self,
        episodes: &[Episode],
        config: &IntroSkipperConfig,
        fingerprinter: &dyn Fingerprinter,
        mode: AnalysisMode,
    ) -> HashMap<Uuid, TimeRange> {
        // Fingerprint (cached) each episode's window for this mode.
        struct Print<'a> {
            episode: &'a Episode,
            start: f64,
            fp: Vec<u32>,
        }
        let mut fingerprints: Vec<Print> = Vec::new();
        for ep in episodes {
            let (start, end) = window(ep.duration_secs, config, mode);
            let min_window = f64::from(
                config
                    .minimum_intro_duration
                    .min(config.minimum_credits_duration),
            );
            if end - start < min_window {
                continue;
            }
            match self
                .fingerprint_cached(ep, start, end, mode, fingerprinter)
                .await
            {
                Ok(fp) => fingerprints.push(Print {
                    episode: ep,
                    start,
                    fp,
                }),
                Err(err) => tracing::debug!(%err, path = ep.path, "fingerprint failed"),
            }
        }

        let cmp = config.compare_config(mode);
        let mut best: HashMap<Uuid, TimeRange> = HashMap::new();
        for i in 0..fingerprints.len() {
            for j in (i + 1)..fingerprints.len() {
                let a = &fingerprints[i];
                let b = &fingerprints[j];
                let (seg_a, seg_b) = compare_episodes(&a.fp, &b.fp, mode, &cmp);
                consider(&mut best, a.episode.id, seg_a, a.start);
                consider(&mut best, b.episode.id, seg_b, b.start);
            }
        }
        best
    }

    /// Writes the detected regions as segments of `seg_type`, replacing this
    /// provider's prior segments on each item. Skips regions over `max_duration`.
    async fn write_segments(
        &self,
        regions: &HashMap<Uuid, TimeRange>,
        seg_type: MediaSegmentType,
        max_duration: f64,
    ) -> usize {
        let mut count = 0;
        for (&item_id, range) in regions {
            if range.duration() > max_duration || range.duration() <= 0.0 {
                continue;
            }
            // Replace only our own prior rows for this type; leave user/other segments.
            if let Err(err) = self
                .media_segments
                .delete_provider_segments(item_id, PROVIDER_ID, Some(seg_type))
                .await
            {
                tracing::debug!(%err, %item_id, "clear prior segment failed");
            }
            let dto = MediaSegmentDto {
                id: Uuid::new_v4(),
                item_id,
                type_: seg_type,
                start_ticks: secs_to_ticks(range.start),
                end_ticks: secs_to_ticks(range.end),
            };
            match self.media_segments.create_segment(&dto, PROVIDER_ID).await {
                Ok(_) => count += 1,
                Err(err) => tracing::debug!(%err, %item_id, "write segment failed"),
            }
        }
        count
    }

    /// Fingerprints an episode window, using an on-disk cache keyed by the window
    /// so a re-run skips the (expensive) decode.
    async fn fingerprint_cached(
        &self,
        ep: &Episode,
        start: f64,
        end: f64,
        mode: AnalysisMode,
        fingerprinter: &dyn Fingerprinter,
    ) -> Result<Vec<u32>, String> {
        let cache = self.cache_dir.join(format!(
            "{}.{}.{start:.0}-{end:.0}.fp",
            ep.id,
            mode_tag(mode),
        ));
        if let Ok(bytes) = tokio::fs::read(&cache).await
            && let Some(points) = decode_points(&bytes)
        {
            return Ok(points);
        }
        let points = fingerprinter.fingerprint(&ep.path, start, end).await?;
        let _ = tokio::fs::create_dir_all(&self.cache_dir).await;
        let _ = tokio::fs::write(&cache, encode_points(&points)).await;
        Ok(points)
    }
}

/// Records `segment` as episode `id`'s best region if it is the first found or
/// longer than the prior one, shifting credits times by the fingerprint start.
fn consider(
    best: &mut HashMap<Uuid, TimeRange>,
    id: Uuid,
    segment: Option<TimeRange>,
    fingerprint_start: f64,
) {
    let Some(mut seg) = segment else {
        return;
    };
    // The fingerprint began at `fingerprint_start` (0 for intros), so shift the
    // reported times back into episode time (the C# credits offset fix-up).
    seg.start += fingerprint_start;
    seg.end += fingerprint_start;
    if best
        .get(&id)
        .is_none_or(|prev| seg.duration() > prev.duration())
    {
        best.insert(id, seg);
    }
}

/// Applies the configured intro offsets to each detected intro: play
/// `IntroStartOffset` seconds before skipping, and resume `IntroEndOffset` seconds
/// before the intro ends (Intro Skipper's `IntroStartOffset`/`IntroEndOffset`).
fn apply_intro_offsets(regions: &mut HashMap<Uuid, TimeRange>, config: &IntroSkipperConfig) {
    let start_offset = f64::from(config.intro_start_offset).max(0.0);
    let end_offset = f64::from(config.intro_end_offset).max(0.0);
    if start_offset == 0.0 && end_offset == 0.0 {
        return;
    }
    for range in regions.values_mut() {
        let new_start = range.start + start_offset;
        let new_end = range.end - end_offset;
        if new_end > new_start {
            range.start = new_start;
            range.end = new_end;
        }
    }
}

/// The `[start, end]` seconds window to fingerprint for an episode + mode.
fn window(duration_secs: f64, config: &IntroSkipperConfig, mode: AnalysisMode) -> (f64, f64) {
    if mode == AnalysisMode::Credits {
        // The tail of the episode (a little longer than the max credits).
        let win = f64::from(config.maximum_credits_duration) + 30.0;
        ((duration_secs - win).max(0.0), duration_secs)
    } else {
        // The first AnalysisPercent of the episode, capped by the limit.
        let by_percent = duration_secs * f64::from(config.analysis_percent) / 100.0;
        let cap = f64::from(config.analysis_length_limit) * 60.0;
        (0.0, by_percent.min(cap).min(duration_secs))
    }
}

/// Projects an episode row into the analyzer's [`Episode`] (or `None` when it
/// lacks a path or a known duration).
fn to_episode(row: &BaseItemEntity) -> Option<Episode> {
    let path = row.path.clone().filter(|p| !p.is_empty())?;
    let ticks = row.run_time_ticks.filter(|t| *t > 0)?;
    Some(Episode {
        id: Uuid::parse_str(&row.id).ok()?,
        path,
        duration_secs: ticks_to_secs(ticks),
    })
}

/// A short filename tag for an analysis mode (part of the fingerprint-cache
/// key).
fn mode_tag(mode: AnalysisMode) -> &'static str {
    match mode {
        AnalysisMode::Introduction => "intro",
        // "credits2": the v1 cache was fingerprinted under fpcalc's default
        // 120 s `-length` cap (a truncated tail window); the tag bump orphans
        // those files so a re-run re-fingerprints the full window.
        AnalysisMode::Credits => "credits2",
        AnalysisMode::Recap => "recap",
    }
}

/// Encodes fingerprint points as little-endian bytes for the cache.
fn encode_points(points: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(points.len() * 4);
    for p in points {
        bytes.extend_from_slice(&p.to_le_bytes());
    }
    bytes
}

/// Decodes cached little-endian bytes back into fingerprint points.
fn decode_points(bytes: &[u8]) -> Option<Vec<u32>> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact compares on deterministic window/tick math
mod tests {
    use std::sync::atomic::AtomicUsize;

    use ferrofin_traits::persistence::ItemPersistenceService;
    use rstest::rstest;

    use super::*;

    #[test]
    fn default_config_round_trips_json() {
        let bytes = IntroSkipperExtension.default_config();
        let cfg: IntroSkipperConfig = serde_json::from_slice(&bytes).expect("parse");
        assert!(cfg.scan_introduction);
        assert_eq!(cfg.analysis_percent, 25);
        assert_eq!(cfg.maximum_fingerprint_point_differences, 6);
    }

    #[test]
    fn partial_config_fills_defaults() {
        let cfg: IntroSkipperConfig =
            serde_json::from_str(r#"{"ScanCredits":false,"AnalysisPercent":40}"#).unwrap();
        assert!(!cfg.scan_credits);
        assert!(cfg.scan_introduction); // default kept
        assert_eq!(cfg.analysis_percent, 40);
    }

    #[test]
    fn intro_window_is_capped_by_percent_and_limit() {
        let cfg = IntroSkipperConfig::default();
        // 2000 s episode, 25% = 500 s, capped at 10 min (600) → 500.
        assert_eq!(
            window(2000.0, &cfg, AnalysisMode::Introduction),
            (0.0, 500.0)
        );
        // 4000 s episode, 25% = 1000 s, capped at 600.
        assert_eq!(
            window(4000.0, &cfg, AnalysisMode::Introduction),
            (0.0, 600.0)
        );
    }

    #[test]
    fn credits_window_is_the_tail() {
        let cfg = IntroSkipperConfig::default();
        let (start, end) = window(3600.0, &cfg, AnalysisMode::Credits);
        assert_eq!(end, 3600.0);
        assert_eq!(start, 3600.0 - (450.0 + 30.0));
    }

    #[test]
    fn points_cache_round_trips() {
        let points = vec![1u32, 2, 3, u32::MAX, 0];
        assert_eq!(decode_points(&encode_points(&points)), Some(points));
        assert_eq!(decode_points(&[1, 2, 3]), None); // not a multiple of 4
    }

    #[test]
    fn consider_keeps_the_longest_and_shifts_credits() {
        let mut best = HashMap::new();
        let id = Uuid::from_u128(1);
        consider(&mut best, id, Some(TimeRange::new(0.0, 20.0)), 0.0);
        consider(&mut best, id, Some(TimeRange::new(0.0, 10.0)), 0.0); // shorter → ignored
        assert_eq!(best[&id].end, 20.0);
        // A credits region fingerprinted from t=3000 is shifted into episode time.
        let cid = Uuid::from_u128(2);
        consider(&mut best, cid, Some(TimeRange::new(5.0, 40.0)), 3000.0);
        assert_eq!(best[&cid].start, 3005.0);
        assert_eq!(best[&cid].end, 3040.0);
    }

    #[test]
    fn consider_ignores_a_missing_segment() {
        let mut best = HashMap::new();
        consider(&mut best, Uuid::from_u128(1), None, 0.0);
        assert!(best.is_empty());
    }

    #[rstest]
    #[case(AnalysisMode::Introduction, "intro")]
    // "credits2" orphans the v1 cache fingerprinted under fpcalc's 120 s default.
    #[case(AnalysisMode::Credits, "credits2")]
    #[case(AnalysisMode::Recap, "recap")]
    fn mode_tag_is_the_cache_key_discriminator(#[case] mode: AnalysisMode, #[case] tag: &str) {
        assert_eq!(mode_tag(mode), tag);
    }

    #[rstest]
    #[case(AnalysisMode::Introduction, 20.0)]
    #[case(AnalysisMode::Credits, 40.0)]
    #[case(AnalysisMode::Recap, 30.0)]
    fn compare_config_takes_the_modes_minimum_duration(
        #[case] mode: AnalysisMode,
        #[case] expected: f64,
    ) {
        let cfg = IntroSkipperConfig {
            minimum_intro_duration: 20,
            minimum_credits_duration: 40,
            minimum_recap_duration: 30,
            ..IntroSkipperConfig::default()
        };
        let cmp = cfg.compare_config(mode);
        assert_eq!(cmp.min_region_duration, expected);
        assert_eq!(cmp.inverted_index_shift, 2);
        assert_eq!(cmp.max_bit_diff, 6);
    }

    #[test]
    fn compare_config_falls_back_when_the_bit_diff_is_negative() {
        let cfg = IntroSkipperConfig {
            maximum_fingerprint_point_differences: -1,
            ..IntroSkipperConfig::default()
        };
        assert_eq!(
            cfg.compare_config(AnalysisMode::Introduction).max_bit_diff,
            6
        );
    }

    // ---- to_episode --------------------------------------------------------

    fn row(id: &str, path: Option<&str>, ticks: Option<i64>) -> BaseItemEntity {
        BaseItemEntity {
            id: id.to_owned(),
            path: path.map(str::to_owned),
            run_time_ticks: ticks,
            ..BaseItemEntity::default()
        }
    }

    #[rstest]
    #[case::no_path(row("00000000-0000-0000-0000-000000000001", None, Some(10)))]
    #[case::empty_path(row("00000000-0000-0000-0000-000000000001", Some(""), Some(10)))]
    #[case::no_runtime(row("00000000-0000-0000-0000-000000000001", Some("/a.mkv"), None))]
    #[case::zero_runtime(row("00000000-0000-0000-0000-000000000001", Some("/a.mkv"), Some(0)))]
    #[case::bad_uuid(row("not-a-uuid", Some("/a.mkv"), Some(10)))]
    fn to_episode_rejects_unanalyzable_rows(#[case] row: BaseItemEntity) {
        assert!(to_episode(&row).is_none());
    }

    #[test]
    fn to_episode_converts_ticks_to_seconds() {
        let ep = to_episode(&row(
            "00000000-0000-0000-0000-00000000002a",
            Some("/media/s01e01.mkv"),
            Some(1_800 * 10_000_000),
        ))
        .expect("episode");
        assert_eq!(ep.id, Uuid::from_u128(42));
        assert_eq!(ep.path, "/media/s01e01.mkv");
        assert_eq!(ep.duration_secs, 1800.0);
    }

    #[test]
    fn ticks_round_trip_through_seconds() {
        assert_eq!(secs_to_ticks(1.5), 15_000_000);
        assert_eq!(ticks_to_secs(15_000_000), 1.5);
    }

    // ---- intro offsets -----------------------------------------------------

    fn one_region(start: f64, end: f64) -> HashMap<Uuid, TimeRange> {
        HashMap::from([(Uuid::from_u128(1), TimeRange::new(start, end))])
    }

    #[rstest]
    // (start_offset, end_offset) → the resulting (start, end) of a 10..70 intro.
    #[case::no_offsets(0, 0, (10.0, 70.0))]
    #[case::start_only(5, 0, (15.0, 70.0))]
    #[case::end_only(0, 5, (10.0, 65.0))]
    #[case::both(5, 5, (15.0, 65.0))]
    // Offsets that would invert (or empty) the region are refused wholesale.
    #[case::inverting(40, 40, (10.0, 70.0))]
    // Negative offsets clamp to zero rather than widening the region.
    #[case::negative_clamps(-5, -5, (10.0, 70.0))]
    fn apply_intro_offsets_shifts_within_bounds(
        #[case] start_offset: i32,
        #[case] end_offset: i32,
        #[case] expected: (f64, f64),
    ) {
        let cfg = IntroSkipperConfig {
            intro_start_offset: start_offset,
            intro_end_offset: end_offset,
            ..IntroSkipperConfig::default()
        };
        let mut regions = one_region(10.0, 70.0);
        apply_intro_offsets(&mut regions, &cfg);
        let r = &regions[&Uuid::from_u128(1)];
        assert_eq!((r.start, r.end), expected);
    }

    // ---- extension surface -------------------------------------------------

    #[test]
    fn descriptor_uses_the_upstream_guid() {
        let d = IntroSkipperExtension::new().descriptor();
        assert_eq!(
            d.id.to_string(),
            "c83d86bb-a1e0-4c35-a113-e2101cf4ee6b",
            "clients and the vendored dashboard address the plugin by this id"
        );
        assert_eq!(IntroSkipperExtension.id(), d.id);
        assert_eq!(d.name, "Intro Skipper");
        assert!(!d.can_uninstall);
    }

    #[test]
    fn config_pages_ship_the_vendored_dashboard_app() {
        let pages = IntroSkipperExtension.config_pages();
        let names: Vec<&str> = pages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["Intro Skipper", "introskipper.js", "introskipper.css"]
        );
        // Only the shell page is a main-menu entry; the JS/CSS are loaded by name.
        assert!(pages[0].enable_in_main_menu);
        assert!(!pages[1].enable_in_main_menu);
        assert!(!pages[2].enable_in_main_menu);
        assert!(pages.iter().all(|p| !p.bytes.is_empty()));
    }

    // ---- the detection task over real managers ------------------------------

    /// A fingerprinter returning a canned fingerprint per path, counting calls so
    /// a test can prove the on-disk cache short-circuits the (expensive) decode.
    struct FakeFingerprinter {
        prints: HashMap<String, Vec<u32>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Fingerprinter for FakeFingerprinter {
        async fn fingerprint(
            &self,
            path: &str,
            _start: f64,
            _end: f64,
        ) -> Result<Vec<u32>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.prints
                .get(path)
                .cloned()
                .ok_or_else(|| format!("no canned fingerprint for {path}"))
        }
    }

    /// Two episodes sharing a 30 s "intro" (identical leading points) and
    /// diverging afterwards — the shape `compare_episodes` is built to find.
    fn shared_intro_prints() -> (Vec<u32>, Vec<u32>) {
        // ferrofin-chromaprint samples at ~0.1238 s/point, so 30 s ≈ 242 points.
        let shared: Vec<u32> = (0..400u32).map(|i| i.wrapping_mul(0x9E37_79B9)).collect();
        let mut a = shared.clone();
        let mut b = shared;
        a.extend((0..400u32).map(|i| i.wrapping_mul(0x1234_5678) | 1));
        b.extend((0..400u32).map(|i| i.wrapping_mul(0x8765_4321) | 2));
        (a, b)
    }

    struct Harness {
        task: DetectSegmentsTask,
        segments: Arc<dyn MediaSegmentManager>,
        calls: Arc<AtomicUsize>,
        _cache: tempfile::TempDir,
    }

    /// Builds the task over an in-memory database seeded with `episodes`
    /// (`(id, path, duration_secs)`), all in one season.
    async fn harness(
        episodes: &[(Uuid, &str, f64)],
        plugin_enabled: bool,
        config: &str,
        with_fingerprinter: bool,
    ) -> Harness {
        let db = ferrofin_db::Database::connect_in_memory()
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrations");

        let persistence = Arc::new(ferrofin_core::FerrofinItemPersistenceService::new(
            db.clone(),
        ));
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new());
        let items = Arc::new(ferrofin_core::FerrofinItemRepository::new(
            db.clone(),
            lookup,
        ));
        let library: Arc<dyn LibraryManager> =
            Arc::new(ferrofin_core::FerrofinLibraryManager::new(
                items,
                Arc::new(ferrofin_core::FerrofinItemCountService::new(db.clone())),
                persistence.clone(),
                Arc::new(ferrofin_core::FerrofinPeopleRepository::new(db.clone())),
            ));

        let type_name = ferrofin_core::item_type_lookup::stored_type_name(BaseItemKind::Episode)
            .expect("stored type name");
        let rows: Vec<BaseItemEntity> = episodes
            .iter()
            .map(|(id, path, secs)| BaseItemEntity {
                id: id.to_string(),
                type_: type_name.to_owned(),
                path: Some((*path).to_owned()),
                #[allow(clippy::cast_possible_truncation)]
                run_time_ticks: Some((secs * TICKS_PER_SECOND) as i64),
                season_id: Some("season-1".to_owned()),
                ..BaseItemEntity::default()
            })
            .collect();
        persistence.save_items(&rows).await.expect("seed episodes");

        let segments: Arc<dyn MediaSegmentManager> = Arc::new(
            ferrofin_core::FerrofinMediaSegmentManager::new(db.clone(), Arc::clone(&library)),
        );

        let (a, b) = shared_intro_prints();
        let calls = Arc::new(AtomicUsize::new(0));
        let prints = episodes
            .iter()
            .enumerate()
            .map(|(i, (_, path, _))| {
                (
                    (*path).to_owned(),
                    if i % 2 == 0 { a.clone() } else { b.clone() },
                )
            })
            .collect();
        let fingerprinter: Option<Arc<dyn Fingerprinter>> = with_fingerprinter.then(|| {
            Arc::new(FakeFingerprinter {
                prints,
                calls: Arc::clone(&calls),
            }) as Arc<dyn Fingerprinter>
        });

        let cache = tempfile::tempdir().expect("cache dir");
        Harness {
            task: DetectSegmentsTask {
                library,
                media_segments: Arc::clone(&segments),
                plugins: Arc::new(FakePlugins {
                    enabled: plugin_enabled,
                    config: config.as_bytes().to_vec(),
                }),
                fingerprinter,
                cache_dir: cache.path().join("introskipper"),
                running: Arc::new(AtomicBool::new(false)),
            },
            segments,
            calls,
            _cache: cache,
        }
    }

    /// A plugin manager reporting one plugin with a fixed enabled flag + config.
    struct FakePlugins {
        enabled: bool,
        config: Vec<u8>,
    }

    #[async_trait]
    impl PluginManager for FakePlugins {
        async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_plugin(&self, id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
            Ok(Some(PluginDescriptor {
                id,
                enabled: self.enabled,
                ..PluginDescriptor::default()
            }))
        }
        async fn enable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn disable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn remove_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_plugin_configuration(&self, _id: Uuid) -> Result<Vec<u8>, ServiceError> {
            Ok(self.config.clone())
        }
        async fn set_plugin_configuration(
            &self,
            _id: Uuid,
            _config: Vec<u8>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn plugin_image(
            &self,
            _id: Uuid,
        ) -> Result<Option<ferrofin_traits::plugins::PluginImage>, ServiceError> {
            Ok(None)
        }
        async fn get_repositories(
            &self,
        ) -> Result<Vec<ferrofin_model::updates::RepositoryInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn set_repositories(
            &self,
            _repositories: Vec<ferrofin_model::updates::RepositoryInfo>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn list_packages(
            &self,
        ) -> Result<Vec<ferrofin_model::updates::PackageInfo>, ServiceError> {
            Ok(Vec::new())
        }
    }

    const EP_A: Uuid = Uuid::from_u128(0xA1);
    const EP_B: Uuid = Uuid::from_u128(0xA2);
    const TWO_EPISODES: [(Uuid, &str, f64); 2] = [
        (EP_A, "/media/s01e01.mkv", 1800.0),
        (EP_B, "/media/s01e02.mkv", 1800.0),
    ];

    async fn intros_of(segments: &Arc<dyn MediaSegmentManager>, id: Uuid) -> Vec<MediaSegmentDto> {
        segments
            .get_segments(id, Some(&[MediaSegmentType::Intro]), false)
            .await
            .expect("segments")
    }

    #[tokio::test]
    async fn detects_a_shared_intro_and_writes_segments() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        let progress = TaskProgress::default();
        h.task.execute(&progress).await.expect("execute");

        assert_eq!(progress.current(), 100.0);
        for id in [EP_A, EP_B] {
            let found = intros_of(&h.segments, id).await;
            assert_eq!(found.len(), 1, "episode {id} should get one Intro segment");
            assert!(
                found[0].end_ticks > found[0].start_ticks,
                "segment must be non-empty"
            );
        }
    }

    #[tokio::test]
    async fn a_rerun_reuses_the_fingerprint_cache() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.task.execute(&TaskProgress::default()).await.expect("run");
        let first = h.calls.load(Ordering::SeqCst);
        assert!(first > 0, "the first pass must fingerprint");

        h.task.execute(&TaskProgress::default()).await.expect("run");
        assert_eq!(
            h.calls.load(Ordering::SeqCst),
            first,
            "the on-disk cache must short-circuit the second pass"
        );
        // Re-running replaces this provider's rows rather than duplicating them.
        assert_eq!(intros_of(&h.segments, EP_A).await.len(), 1);
    }

    #[rstest]
    // A disabled plugin, an absent fpcalc, and a season of one all no-op.
    #[case::disabled(false, true, &TWO_EPISODES[..])]
    #[case::no_fpcalc(true, false, &TWO_EPISODES[..])]
    #[case::single_episode(true, true, &TWO_EPISODES[..1])]
    #[tokio::test]
    async fn analysis_no_ops(
        #[case] enabled: bool,
        #[case] with_fingerprinter: bool,
        #[case] episodes: &[(Uuid, &str, f64)],
    ) {
        let h = harness(episodes, enabled, "{}", with_fingerprinter).await;
        h.task.execute(&TaskProgress::default()).await.expect("run");
        assert!(intros_of(&h.segments, EP_A).await.is_empty());
    }

    #[tokio::test]
    async fn a_second_concurrent_pass_is_refused() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        // Simulate a pass in flight (e.g. `/Intros/ScanSeason` started one).
        h.task.running.store(true, Ordering::SeqCst);
        h.task.execute(&TaskProgress::default()).await.expect("run");
        assert_eq!(h.calls.load(Ordering::SeqCst), 0);
        assert!(
            h.task.running.load(Ordering::SeqCst),
            "the refused run must not clear another pass's latch"
        );
    }

    #[tokio::test]
    async fn scan_toggles_off_skip_their_analysis() {
        let h = harness(
            &TWO_EPISODES,
            true,
            r#"{"ScanIntroduction":false,"ScanCredits":false}"#,
            true,
        )
        .await;
        h.task.execute(&TaskProgress::default()).await.expect("run");
        assert_eq!(h.calls.load(Ordering::SeqCst), 0);
        assert!(intros_of(&h.segments, EP_A).await.is_empty());
    }

    #[tokio::test]
    async fn an_over_long_region_is_not_written() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        // MaximumIntroDuration is the write-side veto; 0 rejects everything.
        let regions = HashMap::from([(EP_A, TimeRange::new(0.0, 30.0))]);
        assert_eq!(
            h.task
                .write_segments(&regions, MediaSegmentType::Intro, 0.0)
                .await,
            0
        );
        // A zero-length region is rejected too.
        let empty = HashMap::from([(EP_A, TimeRange::new(5.0, 5.0))]);
        assert_eq!(
            h.task
                .write_segments(&empty, MediaSegmentType::Intro, 600.0)
                .await,
            0
        );
        assert!(intros_of(&h.segments, EP_A).await.is_empty());
    }

    #[tokio::test]
    async fn a_malformed_stored_config_falls_back_to_defaults() {
        let h = harness(&TWO_EPISODES, true, "not json", true).await;
        assert_eq!(h.task.load_config().await.analysis_percent, 25);
    }

    #[tokio::test]
    async fn an_unfingerprintable_episode_is_skipped() {
        // A 1 s episode: the intro window is shorter than the minimum, so it is
        // never fingerprinted and the season has no comparable pair.
        let eps = [
            (EP_A, "/media/short1.mkv", 1.0),
            (EP_B, "/media/short2.mkv", 1.0),
        ];
        let h = harness(&eps, true, "{}", true).await;
        h.task.execute(&TaskProgress::default()).await.expect("run");
        assert_eq!(h.calls.load(Ordering::SeqCst), 0);
        assert!(intros_of(&h.segments, EP_A).await.is_empty());
    }

    #[tokio::test]
    async fn a_fingerprinter_error_does_not_abort_the_pass() {
        let h = harness(
            &[
                (EP_A, "/media/unknown-a.mkv", 1800.0),
                (EP_B, "/media/unknown-b.mkv", 1800.0),
            ],
            true,
            "{}",
            true,
        )
        .await;
        // The fake only knows the paths it was seeded with — here it errors for
        // both, which must be logged and skipped, not propagated.
        let h = Harness {
            task: DetectSegmentsTask {
                fingerprinter: Some(Arc::new(FakeFingerprinter {
                    prints: HashMap::new(),
                    calls: Arc::clone(&h.calls),
                })),
                ..h.task.clone()
            },
            ..h
        };
        h.task.execute(&TaskProgress::default()).await.expect("run");
        assert!(intros_of(&h.segments, EP_A).await.is_empty());
    }

    // ---- the Media Segment Scan task ---------------------------------------

    #[tokio::test]
    async fn media_segment_scan_task_drives_the_same_detection() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        let scan = MediaSegmentScanTask {
            detect: Arc::new(h.task.clone()),
        };
        assert_eq!(scan.key(), "TaskExtractMediaSegments");
        assert_eq!(scan.name(), "Media Segment Scan");
        assert!(!scan.description().is_empty());
        assert_eq!(scan.category(), "Library");
        let triggers = scan.default_triggers();
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].interval_ticks, Some(12 * 3600 * 10_000_000));

        scan.execute(&TaskProgress::default()).await.expect("run");
        assert_eq!(intros_of(&h.segments, EP_A).await.len(), 1);
    }

    #[tokio::test]
    async fn detect_task_metadata_is_stable() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        assert_eq!(h.task.key(), "IntroSkipper.Detect");
        assert_eq!(h.task.name(), "Detect intros and credits");
        assert!(h.task.description().contains("media segments"));
        assert_eq!(h.task.category(), "Intro Skipper");
    }
}
