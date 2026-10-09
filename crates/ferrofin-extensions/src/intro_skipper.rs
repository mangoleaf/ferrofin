//! The Intro Skipper extension — Ferrofin's first built-in extension.
//!
//! Port of the Intro Skipper plugin's orchestration (`QueueManager` +
//! `BaseItemAnalyzerTask` + the I/O half of `ChromaprintAnalyzer`,
//! GPL-3.0-only): group episodes by season, fingerprint each one's intro and
//! credits windows, compare them within the season via [`ferrofin_chromaprint`],
//! and store the shared regions in the plugin's own segment tier
//! ([`ferrofin_traits::intro_skipper`]), publishing them as `Intro`/`Outro`
//! [`MediaSegmentDto`](ferrofin_model::media_segments::MediaSegmentDto)s — which is
//! what makes jellyfin-web show the "Skip Intro" / "Skip Credits" button.
//!
//! It surfaces on `/Plugins` as "Intro Skipper"; its analysis runs as the
//! `IntroSkipper.Detect` scheduled task and as the provider behind the
//! "Media Segment Scan" dashboard task (`TaskExtractMediaSegments`). The task
//! self-gates on the plugin's enabled flag; without a Chromaprint backend it
//! warns and runs the analyzers that do not need one.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use ferrofin_chromaprint::AnalysisMode;
use ferrofin_core::{PluginConfigPage, ScheduledTask, TaskProgress};
use ferrofin_model::intro_skipper::AnalysisMode as WireMode;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::intro_skipper::{self as intro_store, IntroSkipperAnalysis};
use ferrofin_traits::library::{LibraryManager, VirtualFolderManager};
use ferrofin_traits::media_segments::MediaSegmentManager;
use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ffmpeg::{FfmpegService, ProcessOptions};
use crate::fingerprint::Fingerprinter;

mod config_hash;
mod engine;
mod queue;
use crate::{Extension, ExtensionContext};

/// The Intro Skipper's stable plugin id (also its `/Plugins` id) — the
/// **upstream plugin's GUID**, because the vendored dashboard app and third-party
/// client integrations address the plugin by that id.
const EXTENSION_ID: Uuid = Uuid::from_u128(0xc83d_86bb_a1e0_4c35_a113_e210_1cf4_ee6b);

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
            configuration_file_name: None,
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
        let detect = Arc::new(detector(cx));
        vec![
            Arc::clone(&detect) as Arc<dyn ScheduledTask>,
            Arc::new(MediaSegmentScanTask { detect }),
        ]
    }
}

/// The detection pass over `cx`. Every instance shares the context's latch, so
/// the scheduled tasks and the routes' [`analysis`] handle run one pass at a
/// time between them (the plugin's `ScheduledTaskSemaphore`).
fn detector(cx: &ExtensionContext) -> DetectSegmentsTask {
    DetectSegmentsTask {
        library: Arc::clone(&cx.library),
        media_segments: Arc::clone(&cx.media_segments),
        plugins: Arc::clone(&cx.plugins),
        fingerprinter: cx.fingerprinter.clone(),
        ffmpeg: Arc::clone(&cx.ffmpeg),
        cache_dir: cx.cache_dir.join("introskipper"),
        running: Arc::clone(&cx.intro_skipper_running),
        actions: Arc::clone(&cx.intro_skipper),
        virtual_folders: Arc::clone(&cx.virtual_folders),
        chapters: Arc::clone(&cx.chapters),
    }
}

/// The Intro Skipper's analysis runtime for the plugin routes (`ScanSeason`,
/// `ScanStatus`, `eraseCache`).
#[must_use]
pub fn analysis(cx: &ExtensionContext) -> Arc<dyn IntroSkipperAnalysis> {
    Arc::new(detector(cx))
}

#[async_trait]
impl IntroSkipperAnalysis for DetectSegmentsTask {
    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    async fn rescan(&self, season_id: Uuid) -> Result<bool, ServiceError> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(false);
        }
        // Background, not bound to the request, as upstream's `Task.Run`.
        let task = self.clone();
        tokio::spawn(async move {
            let _release = ReleaseOnDrop(Arc::clone(&task.running));
            tracing::info!(%season_id, "intro skipper: rescanning a season");
            task.rescan_season(season_id).await;
        });
        Ok(true)
    }

    async fn erase_cache(
        &self,
        items: Option<&[Uuid]>,
        mode: Option<WireMode>,
    ) -> Result<u64, ServiceError> {
        self.erase_cache_files(items, mode).await
    }

    async fn clear_excluded(&self) -> Result<intro_store::ExcludedClear, ServiceError> {
        let config = self.load_config().await;
        // `GetExcludedInventoryAsync`: the queue with the excluded items in.
        let mut ids: Vec<Uuid> = self
            .queue(&config)
            .await?
            .into_iter()
            .flat_map(|(_, entries)| entries)
            .filter(|e| e.is_excluded)
            .map(|e| e.episode_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(intro_store::ExcludedClear::default());
        }
        let removed_segments = self.actions.delete_items(&ids).await?;
        // Out of every season's analysed lists, as upstream edits each state.
        self.actions.remove_episode_ids(None, None, &ids).await?;
        let removed_cache_entries = self.erase_cache_files(Some(&ids), None).await?;
        if config.update_media_segments {
            for &item_id in &ids {
                if let Err(err) = intro_store::refresh(
                    self.actions.as_ref(),
                    self.media_segments.as_ref(),
                    item_id,
                )
                .await
                {
                    tracing::error!(%err, %item_id, "intro skipper: publishing media segments failed");
                }
            }
        }
        Ok(intro_store::ExcludedClear {
            affected_items: ids.len() as u64,
            removed_segments,
            removed_cache_entries,
        })
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
        self.detect.execute(progress).await?;
        // Upstream this task runs the plugin's `SegmentProvider`, which
        // publishes the stored segments whatever `UpdateMediaSegments` says;
        // detection here publishes only when it is on, so the scan publishes
        // the rest itself.
        if self.detect.enabled().await && !self.detect.load_config().await.update_media_segments {
            self.detect.publish_stored().await;
        }
        Ok(())
    }
}

/// `ExclusionListJsonConverter`: an array of strings (nulls skipped), or the
/// legacy comma-separated string (trimmed, empties dropped). Anything else is
/// refused, as upstream throws.
fn exclusion_list<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum List {
        Legacy(String),
        Items(Vec<Option<String>>),
    }
    Ok(match List::deserialize(deserializer)? {
        List::Legacy(text) => text
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        List::Items(items) => items.into_iter().flatten().collect(),
    })
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
    #[serde(deserialize_with = "exclusion_list")]
    pub series_exclusions: Vec<String>,
    /// Movies excluded from analysis.
    #[serde(deserialize_with = "exclusion_list")]
    pub movie_exclusions: Vec<String>,
    /// Paths excluded from analysis.
    #[serde(deserialize_with = "exclusion_list")]
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

/// The scheduled task that detects intros/credits across the library.
#[derive(Clone)]
struct DetectSegmentsTask {
    library: Arc<dyn LibraryManager>,
    media_segments: Arc<dyn MediaSegmentManager>,
    plugins: Arc<dyn PluginManager>,
    fingerprinter: Option<Arc<dyn Fingerprinter>>,
    /// Silence and keyframe detection, the audio duration probe.
    ffmpeg: Arc<dyn FfmpegService>,
    cache_dir: PathBuf,
    /// `true` while an analysis pass is running, so a second trigger is a no-op
    /// instead of a duplicate concurrent pass.
    running: Arc<AtomicBool>,
    /// The per-season analyzer actions (`POST /Intros/AnalyzerActions/UpdateSeason`).
    actions: Arc<dyn ferrofin_traits::intro_skipper::IntroSkipperStore>,
    /// The libraries, whose options decide which ones are analysed.
    virtual_folders: Arc<dyn VirtualFolderManager>,
    /// Items' chapters (segment bounds snap to them).
    chapters: Arc<dyn ferrofin_traits::chapters::ChapterManager>,
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
        // Upstream analyses without Chromaprint too: the other analyzers still
        // run, and the season state records it (a later Chromaprint changes
        // the configuration hash, so those episodes are analysed again).
        if self.fingerprinter.is_none() {
            tracing::warn!(
                "intro skipper: no Chromaprint backend — use an ffmpeg built with the \
                 `chromaprint` muxer (jellyfin-ffmpeg is), or install `chromaprint`/`fpcalc`, \
                 to enable intro/credits detection"
            );
        }
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
        self.run_analysis(progress, &config).await;
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
    /// The queue (`QueueManager.GetMediaItems(includeExcluded: true)`): every
    /// entry, excluded ones flagged.
    async fn queue(&self, config: &IntroSkipperConfig) -> Result<queue::Queue, ServiceError> {
        let folders = self.virtual_folders.get_virtual_folders().await?;
        queue::build(
            self.library.as_ref(),
            self.ffmpeg.as_ref(),
            &folders,
            config,
            true,
        )
        .await
    }

    /// The background analysis pass (`DetectSegmentsTask` →
    /// `AnalyzeItemsAsync`): build the queue, analyse it, then drop the state
    /// of seasons no longer in it.
    #[tracing::instrument(name = "extension_run", skip_all, fields(extension = "intro_skipper"))]
    async fn run_analysis(&self, progress: &TaskProgress, config: &IntroSkipperConfig) {
        let queue = match self.queue(config).await {
            Ok(queue) => queue,
            Err(err) => {
                tracing::warn!(%err, "intro skipper: could not build the analysis queue");
                return;
            }
        };
        let stored = self.analyze_queue(&queue, config, progress).await;
        // `CleanCacheTask` → `CleanSeasonStateAsync` over the whole queue
        // (excluded seasons included): drop the state of seasons that no
        // longer have items, so a season re-created under the same id does not
        // inherit it.
        let live: Vec<Uuid> = queue.iter().map(|(season, _)| *season).collect();
        if let Err(err) = self.actions.retain_seasons(&live).await {
            tracing::warn!(%err, "intro skipper: could not drop stale season state");
        }
        progress.report(100.0);
        tracing::info!(analyzed = stored, "intro skipper: analysis complete");
    }

    /// Publishes every stored item's segments (the plugin's `SegmentProvider`
    /// as the Media Segment Scan runs it), logging a failed item and going on.
    async fn publish_stored(&self) {
        let items = match self.actions.stored_item_ids().await {
            Ok(items) => items,
            Err(err) => {
                tracing::error!(%err, "intro skipper: could not list stored segments to publish");
                return;
            }
        };
        for item_id in items {
            if let Err(err) =
                intro_store::refresh(self.actions.as_ref(), self.media_segments.as_ref(), item_id)
                    .await
            {
                tracing::error!(%err, %item_id, "intro skipper: publishing media segments failed");
            }
        }
    }

    /// `ScanSeason`'s background body: `EraseSeasonAsync(eraseCache: true)`
    /// (segments, cache, republish), then `AnalyzeItemsAsync([seasonId])`.
    async fn rescan_season(&self, season_id: Uuid) {
        let config = self.load_config().await;
        let queue = match self.queue(&config).await {
            Ok(queue) => queue,
            Err(err) => {
                tracing::error!(%err, %season_id, "intro skipper: could not enumerate the season to rescan");
                return;
            }
        };
        // `EraseSeasonAsync` works on the queued (not excluded) items.
        let Some(entries) = queue
            .into_iter()
            .find(|(key, _)| *key == season_id)
            .map(|(_, entries)| {
                entries
                    .into_iter()
                    .filter(|e| !e.is_excluded)
                    .collect::<Vec<_>>()
            })
            .filter(|entries| !entries.is_empty())
        else {
            tracing::info!(%season_id, "intro skipper: the season to rescan has no queued items");
            return;
        };
        let ids: Vec<Uuid> = entries.iter().map(|e| e.episode_id).collect();
        if let Err(err) = self.actions.delete_items(&ids).await {
            tracing::error!(%err, %season_id, "intro skipper: could not erase the season to rescan");
            return;
        }
        if let Err(err) = self.actions.clear_episode_ids(season_id, None).await {
            tracing::error!(%err, %season_id, "intro skipper: could not reset the season to rescan");
            return;
        }
        if let Err(err) = self.erase_cache_files(Some(&ids), None).await {
            tracing::warn!(%err, %season_id, "intro skipper: could not erase the season's cache");
        }
        if config.update_media_segments {
            for &item_id in &ids {
                if let Err(err) = intro_store::refresh(
                    self.actions.as_ref(),
                    self.media_segments.as_ref(),
                    item_id,
                )
                .await
                {
                    tracing::error!(%err, %item_id, "intro skipper: publishing media segments failed");
                }
            }
        }
        // `AnalyzeItemsAsync(seasonsToAnalyze: [seasonId])`.
        let stored = self
            .analyze_queue(&[(season_id, entries)], &config, &TaskProgress::default())
            .await;
        tracing::info!(%season_id, analyzed = stored, "intro skipper: season rescanned");
    }

    /// Deletes the detection caches (`{item}.{mode tag}.…`: prints, silences,
    /// keyframes) of `items` (all when `None`) for `mode` (all when `None`).
    /// Like the plugin's `DeleteByMode`, a mode nothing is cached for (Preview,
    /// Commercial) has nothing to erase.
    async fn erase_cache_files(
        &self,
        items: Option<&[Uuid]>,
        mode: Option<WireMode>,
    ) -> Result<u64, ServiceError> {
        let tags: Option<&[&str]> = match mode {
            None => None,
            // Older tags are orphaned caches of the same mode (see `mode_tag`).
            Some(WireMode::Introduction) => Some(&["intro2", "intro"]),
            Some(WireMode::Credits) => Some(&["credits3", "credits2", "credits"]),
            Some(WireMode::Recap) => Some(&["recap2", "recap"]),
            Some(_) => return Ok(0),
        };
        let mut entries = match tokio::fs::read_dir(&self.cache_dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(ServiceError::backend(err.to_string())),
        };
        let mut removed = 0;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|err| ServiceError::backend(err.to_string()))?
        {
            let path = entry.path();
            // Every detection's cache (`DeleteForItem`/`DeleteByMode`).
            if path
                .extension()
                .is_none_or(|ext| ext != "fp" && ext != SILENCE_CACHE && ext != KEYFRAME_CACHE)
            {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let mut parts = name.splitn(3, '.');
            let (Some(item), Some(tag)) = (parts.next(), parts.next()) else {
                continue;
            };
            let item_matches =
                items.is_none_or(|items| Uuid::parse_str(item).is_ok_and(|id| items.contains(&id)));
            let tag_matches = tags.is_none_or(|tags| tags.contains(&tag));
            if item_matches && tag_matches && tokio::fs::remove_file(entry.path()).await.is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
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

    /// The cache file of an episode window's fingerprint.
    fn print_path(&self, id: Uuid, start: f64, end: f64, mode: AnalysisMode) -> PathBuf {
        self.cache_dir
            .join(format!("{id}.{}.{start:.0}-{end:.0}.fp", mode_tag(mode)))
    }

    /// Fingerprints an episode window, using an on-disk cache keyed by the window
    /// so a re-run skips the (expensive) decode.
    /// TODO(intro-skipper step 10): gate on `CacheFingerprints` and key by
    /// `ConfigHasher.DetectionCache`, as [`Self::detection_cached`] does.
    async fn fingerprint_cached(
        &self,
        entry: &queue::QueuedEpisode,
        (start, end): (f64, f64),
        mode: AnalysisMode,
        fingerprinter: &dyn Fingerprinter,
        options: ProcessOptions,
    ) -> Result<Vec<u32>, String> {
        let cache = self.print_path(entry.episode_id, start, end, mode);
        if let Ok(bytes) = tokio::fs::read(&cache).await
            && let Some(points) = decode_points(&bytes)
        {
            return Ok(points);
        }
        let points = fingerprinter
            .fingerprint(&entry.path, start, end, options)
            .await?;
        let _ = tokio::fs::create_dir_all(&self.cache_dir).await;
        let _ = tokio::fs::write(&cache, encode_points(&points)).await;
        Ok(points)
    }

    /// A silence or keyframe detection, through its cache file when
    /// `CacheFingerprints` (`DetectionCacheService.TryRead`/`Write`, kept as
    /// files beside the prints): keyed by the item, the mode, the exact window
    /// and the detection's `ConfigHasher.DetectionCache` hash, so a setting the
    /// detection reads misses the cache.
    async fn detection_cached<T, F>(
        &self,
        (entry, mode, window): (&queue::QueuedEpisode, AnalysisMode, (f64, f64)),
        (kind, hash, enabled): (&str, &str, bool),
        detect: F,
    ) -> Result<Vec<T>, String>
    where
        T: Serialize + serde::de::DeserializeOwned,
        F: Future<Output = Result<Vec<T>, String>>,
    {
        let cache = self.cache_dir.join(format!(
            "{}.{}.{}-{}.{hash}.{kind}",
            entry.episode_id,
            mode_tag(mode),
            window.0,
            window.1
        ));
        if enabled
            && let Ok(bytes) = tokio::fs::read(&cache).await
            && let Ok(found) = serde_json::from_slice(&bytes)
        {
            return Ok(found);
        }
        let found = detect.await?;
        if enabled && let Ok(bytes) = serde_json::to_vec(&found) {
            let _ = tokio::fs::create_dir_all(&self.cache_dir).await;
            let _ = tokio::fs::write(&cache, bytes).await;
        }
        Ok(found)
    }

    /// `DetectSilenceAsync` over the window.
    async fn silences(
        &self,
        entry: &queue::QueuedEpisode,
        mode: AnalysisMode,
        window: (f64, f64),
        config: &IntroSkipperConfig,
        options: ProcessOptions,
    ) -> Result<Vec<(f64, f64)>, String> {
        let hash = config_hash::detection_cache(config, config_hash::Detection::Silence);
        self.detection_cached(
            (entry, mode, window),
            (SILENCE_CACHE, &hash, config.cache_fingerprints),
            self.ffmpeg.detect_silence(
                &entry.path,
                window.0,
                window.1,
                config.silence_detection_maximum_noise,
                options,
            ),
        )
        .await
    }

    /// `DetectKeyFramesAsync` over the window.
    async fn keyframes(
        &self,
        entry: &queue::QueuedEpisode,
        mode: AnalysisMode,
        window: (f64, f64),
        config: &IntroSkipperConfig,
        options: ProcessOptions,
    ) -> Result<Vec<f64>, String> {
        let hash = config_hash::detection_cache(config, config_hash::Detection::Keyframe);
        self.detection_cached(
            (entry, mode, window),
            (KEYFRAME_CACHE, &hash, config.cache_fingerprints),
            self.ffmpeg
                .detect_keyframes(&entry.path, window.0, window.1, options),
        )
        .await
    }
}

/// The cache-file extensions of the detections besides the prints (`.fp`).
const SILENCE_CACHE: &str = "silence";
const KEYFRAME_CACHE: &str = "keyframes";

/// A short filename tag for an analysis mode (part of the fingerprint-cache
/// key).
fn mode_tag(mode: AnalysisMode) -> &'static str {
    match mode {
        // Each bump orphans prints made by an older command, which would
        // otherwise be compared with new ones: "credits2" left fpcalc's
        // default 120 s `-length` cap (a truncated tail window); "intro2",
        // "credits3" and "recap2" the mono downmix and rounded-up `-t` that
        // preceded upstream's `-ac 2` and exact `-to`.
        AnalysisMode::Introduction => "intro2",
        AnalysisMode::Credits => "credits3",
        AnalysisMode::Recap => "recap2",
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
            .as_chunks::<4>()
            .0
            .iter()
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
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::CollectionTypeOptions;
    use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
    use ferrofin_traits::intro_skipper::IntroSkipperStore as _;
    use ferrofin_traits::intro_skipper::{SeasonState, StoredSegment};
    use std::collections::HashMap;

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
    fn points_cache_round_trips() {
        let points = vec![1u32, 2, 3, u32::MAX, 0];
        assert_eq!(decode_points(&encode_points(&points)), Some(points));
        assert_eq!(decode_points(&[1, 2, 3]), None); // not a multiple of 4
    }

    #[rstest]
    #[case(AnalysisMode::Introduction, "intro2")]
    #[case(AnalysisMode::Credits, "credits3")]
    #[case(AnalysisMode::Recap, "recap2")]
    fn mode_tag_is_the_cache_key_discriminator(#[case] mode: AnalysisMode, #[case] tag: &str) {
        assert_eq!(mode_tag(mode), tag);
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
            _options: ProcessOptions,
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

    /// The season every harness episode belongs to.
    const SEASON: Uuid = Uuid::from_u128(0x5ea5_0001);
    /// The series every harness episode belongs to.
    const SERIES: Uuid = Uuid::from_u128(0x5e_0001);

    struct Harness {
        task: DetectSegmentsTask,
        segments: Arc<dyn MediaSegmentManager>,
        actions: Arc<ferrofin_traits::intro_skipper::InMemoryIntroSkipperStore>,
        calls: Arc<AtomicUsize>,
        persistence: Arc<ferrofin_core::FerrofinItemPersistenceService>,
        cache: tempfile::TempDir,
    }

    /// Builds the task over an in-memory database seeded with `episodes`
    /// (`(id, path, duration_secs)`), all in one season.
    async fn harness(
        episodes: &[(Uuid, &str, f64)],
        plugin_enabled: bool,
        config: &str,
        with_fingerprinter: bool,
    ) -> Harness {
        // Real (empty) files: the queue's verification skips a missing one,
        // as upstream's `File.Exists` does. `/media/...` maps under `media`.
        let cache = tempfile::tempdir().expect("cache dir");
        let media = cache.path().join("media");
        let real = |path: &str| -> String {
            let file = media.join(path.trim_start_matches("/media/"));
            std::fs::create_dir_all(file.parent().expect("parent")).expect("media dir");
            std::fs::write(&file, b"").expect("media file");
            file.to_string_lossy().into_owned()
        };
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
                id: guid_to_db(*id),
                type_: type_name.to_owned(),
                path: Some(real(path)),
                #[allow(clippy::cast_possible_truncation)]
                run_time_ticks: Some((secs * 10_000_000.0) as i64),
                season_id: Some(guid_to_db(SEASON)),
                series_id: Some(guid_to_db(SERIES)),
                series_name: Some("Show".to_owned()),
                parent_index_number: Some(1),
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
            .map(|(i, (_, path, _))| (real(path), if i % 2 == 0 { a.clone() } else { b.clone() }))
            .collect();
        let fingerprinter: Option<Arc<dyn Fingerprinter>> = with_fingerprinter.then(|| {
            Arc::new(FakeFingerprinter {
                prints,
                calls: Arc::clone(&calls),
            }) as Arc<dyn Fingerprinter>
        });

        let actions =
            Arc::new(ferrofin_traits::intro_skipper::InMemoryIntroSkipperStore::default());
        Harness {
            task: DetectSegmentsTask {
                library,
                media_segments: Arc::clone(&segments),
                plugins: Arc::new(FakePlugins {
                    enabled: plugin_enabled,
                    config: config.as_bytes().to_vec(),
                }),
                fingerprinter,
                ffmpeg: Arc::new(crate::ffmpeg::FakeFfmpeg::default()),
                cache_dir: cache.path().join("introskipper"),
                running: Arc::new(AtomicBool::new(false)),
                virtual_folders: Arc::new(MediaLibrary(media.to_string_lossy().into_owned())),
                chapters: Arc::new(ferrofin_traits::stubs::chapters::NoChapters),
                actions: Arc::clone(&actions)
                    as Arc<dyn ferrofin_traits::intro_skipper::IntroSkipperStore>,
            },
            segments,
            actions,
            persistence,
            calls,
            cache,
        }
    }

    /// One library holding everything under `/media`.
    struct MediaLibrary(String);

    #[async_trait]
    impl VirtualFolderManager for MediaLibrary {
        async fn get_virtual_folders(
            &self,
        ) -> Result<Vec<ferrofin_model::entities_media::VirtualFolderInfo>, ServiceError> {
            Ok(vec![ferrofin_model::entities_media::VirtualFolderInfo {
                name: Some("TV".to_owned()),
                locations: vec![self.0.clone()],
                ..Default::default()
            }])
        }
        async fn add_virtual_folder(
            &self,
            _name: &str,
            _collection_type: Option<CollectionTypeOptions>,
            _options: &LibraryOptions,
        ) -> Result<(), ServiceError> {
            unreachable!("read-only")
        }
        async fn remove_virtual_folder(&self, _name: &str) -> Result<(), ServiceError> {
            unreachable!("read-only")
        }
        async fn rename_virtual_folder(
            &self,
            _name: &str,
            _new_name: &str,
        ) -> Result<(), ServiceError> {
            unreachable!("read-only")
        }
        async fn add_media_path(
            &self,
            _folder: &str,
            _path: &MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unreachable!("read-only")
        }
        async fn update_media_path(
            &self,
            _folder: &str,
            _path: &MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unreachable!("read-only")
        }
        async fn remove_media_path(&self, _folder: &str, _path: &str) -> Result<(), ServiceError> {
            unreachable!("read-only")
        }
        async fn update_library_options(
            &self,
            _folder: &str,
            _options: &LibraryOptions,
        ) -> Result<(), ServiceError> {
            unreachable!("read-only")
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
    async fn a_season_whose_intro_action_is_none_is_not_analysed() {
        use ferrofin_model::intro_skipper::{AnalysisMode, AnalyzerAction};
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.actions
            .set_analyzer_actions(
                SEASON,
                &[(AnalysisMode::Introduction, AnalyzerAction::None)],
            )
            .await
            .expect("store");
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("execute");
        for id in [EP_A, EP_B] {
            assert!(intros_of(&h.segments, id).await.is_empty(), "episode {id}");
        }
        // Any other action analyses as before, and a season with no episodes
        // left loses its actions on the run (`CleanSeasonStateAsync`).
        h.actions
            .set_analyzer_actions(
                SEASON,
                &[(AnalysisMode::Introduction, AnalyzerAction::Chromaprint)],
            )
            .await
            .expect("store");
        let gone = Uuid::from_u128(0x5ea5_0002);
        h.actions
            .set_analyzer_actions(gone, &[(AnalysisMode::Credits, AnalyzerAction::None)])
            .await
            .expect("store");
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("execute");
        assert_eq!(intros_of(&h.segments, EP_A).await.len(), 1);
        assert!(
            h.actions
                .analyzer_actions(gone)
                .await
                .expect("read")
                .is_empty()
        );
        assert_eq!(
            h.actions.analyzer_actions(SEASON).await.expect("read")[&AnalysisMode::Introduction],
            AnalyzerAction::Chromaprint,
            "a live season keeps its state"
        );
    }

    /// `Plugin.UpdateTimestampAsync`: analysis never replaces a user-provided
    /// segment, so a corrected intro survives the next detection run (it used
    /// to be clobbered: both wrote the same provider's `MediaSegments` row).
    #[tokio::test]
    async fn a_user_provided_intro_survives_detection() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.actions
            .update_timestamp(StoredSegment {
                item_id: EP_A,
                mode: WireMode::Introduction,
                start: 1.0,
                end: 2.5,
                is_user_provided: true,
                config_hash: String::new(),
            })
            .await
            .expect("store");
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("execute");
        let a = intros_of(&h.segments, EP_A).await;
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].start_ticks, a[0].end_ticks), (10_000_000, 25_000_000));
        // The other episode got the detected intro, under Jellyfin's provider id.
        assert_eq!(intros_of(&h.segments, EP_B).await.len(), 1);
    }

    /// With `UpdateMediaSegments` off, detection only stores; the Media
    /// Segment Scan then publishes the stored segments, as upstream's
    /// `SegmentProvider` does when that task runs it.
    #[tokio::test]
    async fn the_segment_scan_publishes_when_detection_does_not() {
        let h = harness(
            &TWO_EPISODES,
            true,
            r#"{"UpdateMediaSegments":false}"#,
            true,
        )
        .await;
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("detect");
        let tier = h.actions.segments(EP_A).await.expect("tier");
        assert!(
            tier.iter().any(|s| s.mode == WireMode::Introduction),
            "stored"
        );
        assert!(
            intros_of(&h.segments, EP_A).await.is_empty(),
            "not published yet"
        );
        let scan = MediaSegmentScanTask {
            detect: Arc::new(h.task.clone()),
        };
        scan.execute(&TaskProgress::default()).await.expect("scan");
        assert_eq!(intros_of(&h.segments, EP_A).await.len(), 1);
        assert_eq!(intros_of(&h.segments, EP_B).await.len(), 1);
    }

    /// The cache files a detection run left behind for `id`.
    fn cache_files(h: &Harness, id: Uuid) -> Vec<String> {
        std::fs::read_dir(&h.task.cache_dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .filter(|name| name.starts_with(&id.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `DeleteForItem` / `DeleteByMode`: by item, by mode, or everything —
    /// every detection's cache (print, silence, keyframes).
    #[tokio::test]
    async fn erase_cache_deletes_by_item_and_mode() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("detect");
        let before_a = cache_files(&h, EP_A);
        assert!(
            before_a.iter().any(|f| f.contains(".intro2.")),
            "{before_a:?}"
        );
        assert_eq!(
            h.task
                .erase_cache(Some(&[EP_A]), Some(WireMode::Introduction))
                .await
                .expect("erase"),
            3
        );
        let after_a = cache_files(&h, EP_A);
        assert!(!after_a.iter().any(|f| f.contains(".intro2.")));
        assert_eq!(after_a.len(), before_a.len() - 3, "the credits caches stay");
        assert_eq!(
            h.task
                .erase_cache(None, Some(WireMode::Recap))
                .await
                .expect("recap"),
            0,
            "nothing analysed as a recap"
        );
        h.task.erase_cache(None, None).await.expect("all");
        assert!(cache_files(&h, EP_A).is_empty() && cache_files(&h, EP_B).is_empty());
    }

    /// `ScanSeason`: the season is erased (segments + cache) and analysed again
    /// in the background under the scan lock; a second request while it runs
    /// is refused.
    #[tokio::test]
    async fn a_rescan_erases_then_reanalyses_the_season() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("detect");
        let calls = h.calls.load(Ordering::SeqCst);
        // A user edit is erased too: the rescan starts the season over.
        h.actions
            .update_timestamp(StoredSegment {
                item_id: EP_A,
                mode: WireMode::Recap,
                start: 0.0,
                end: 5.0,
                is_user_provided: true,
                config_hash: String::new(),
            })
            .await
            .expect("store");
        h.task.running.store(true, Ordering::SeqCst);
        assert!(!h.task.rescan(SEASON).await.expect("busy"));
        h.task.running.store(false, Ordering::SeqCst);
        assert!(h.task.rescan(SEASON).await.expect("started"));
        for _ in 0..200 {
            if !h.task.is_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !h.task.is_running(),
            "the rescan finished and released the lock"
        );
        assert!(
            h.calls.load(Ordering::SeqCst) > calls,
            "the erased cache was re-fingerprinted"
        );
        let modes: Vec<_> = h
            .actions
            .segments(EP_A)
            .await
            .expect("tier")
            .iter()
            .map(|s| s.mode)
            .collect();
        assert!(
            modes.contains(&WireMode::Introduction) && !modes.contains(&WireMode::Recap),
            "{modes:?}"
        );
        assert_eq!(intros_of(&h.segments, EP_A).await.len(), 1);
    }

    /// `QueueManager.GetMediaItems`: a disabled library is skipped, an
    /// in-season special joins the season it aired in, an excluded series is
    /// flagged, a movie is queued under its own id, and anime is read off the
    /// series.
    #[tokio::test]
    async fn the_queue_follows_queue_manager() {
        let h = harness(&[], true, "{}", true).await;
        let ty = |kind| ferrofin_core::item_type_lookup::stored_type_name(kind).expect("type");
        let id = |n: u128| Uuid::from_u128(n);
        let (series, season, specials, other) = (id(0x51), id(0x52), id(0x53), id(0x54));
        let episode =
            |n: u128, path: &str, season_id: Uuid, series_id: Uuid, number: i64| BaseItemEntity {
                id: id(n).to_string(),
                type_: ty(BaseItemKind::Episode).to_owned(),
                path: Some(path.to_owned()),
                run_time_ticks: Some(1_800 * 10_000_000),
                season_id: Some(season_id.to_string()),
                series_id: Some(series_id.to_string()),
                series_name: Some(if series_id == other { "Other" } else { "Show" }.to_owned()),
                parent_index_number: Some(number),
                ..BaseItemEntity::default()
            };
        let mut special = episode(0x63, "/media/show/s00e01.mkv", specials, series, 0);
        special.data = Some(r#"{"AirsBeforeSeasonNumber":1}"#.to_owned());
        let rows = vec![
            BaseItemEntity {
                id: series.to_string(),
                type_: ty(BaseItemKind::Series).to_owned(),
                genres: Some("Anime".to_owned()),
                ..BaseItemEntity::default()
            },
            episode(0x61, "/media/show/s01e01.mkv", season, series, 1),
            episode(0x62, "/media/show/s01e02.mkv", season, series, 1),
            special,
            episode(0x64, "/media/other/s01e01.mkv", id(0x55), other, 1),
            episode(0x65, "/disabled/show/s01e01.mkv", id(0x56), series, 1),
            BaseItemEntity {
                id: id(0x66).to_string(),
                type_: ty(BaseItemKind::Movie).to_owned(),
                path: Some("/media/movies/film.mkv".to_owned()),
                name: Some("Film".to_owned()),
                run_time_ticks: Some(6_000 * 10_000_000),
                ..BaseItemEntity::default()
            },
        ];
        h.persistence.save_items(&rows).await.expect("seed");
        let folders = vec![
            ferrofin_model::entities_media::VirtualFolderInfo {
                locations: vec!["/media".to_owned()],
                ..Default::default()
            },
            ferrofin_model::entities_media::VirtualFolderInfo {
                locations: vec!["/disabled".to_owned()],
                library_options: Some(LibraryOptions {
                    disabled_media_segment_providers: vec!["Intro Skipper".to_owned()],
                    ..LibraryOptions::default()
                }),
                ..Default::default()
            },
        ];
        let config: IntroSkipperConfig =
            serde_json::from_str(r#"{"SeriesExclusions":"Other"}"#).expect("legacy string list");
        let queue = queue::build(
            h.task.library.as_ref(),
            h.task.ffmpeg.as_ref(),
            &folders,
            &config,
            true,
        )
        .await
        .expect("queue");
        let find = |key: Uuid| queue.iter().find(|(k, _)| *k == key).map(|(_, v)| v);
        let shows = find(season).expect("season 1");
        let ids: Vec<Uuid> = shows.iter().map(|e| e.episode_id).collect();
        assert_eq!(ids.len(), 3, "the special joined season 1: {ids:?}");
        assert!(ids.contains(&id(0x63)));
        assert!(
            shows
                .iter()
                .all(|e| e.category == queue::Category::AnimeEpisode)
        );
        assert!(find(specials).is_none());
        assert!(find(id(0x55)).expect("excluded, flagged")[0].is_excluded);
        assert!(find(id(0x56)).is_none(), "the library disables the plugin");
        let movie = &find(id(0x66)).expect("movie")[0];
        assert_eq!(movie.category, queue::Category::Movie);
        assert_eq!(movie.credits_fingerprint_start, 6_000.0 - 900.0);
        // Excluded items stay out unless asked for.
        let strict = queue::build(
            h.task.library.as_ref(),
            h.task.ffmpeg.as_ref(),
            &folders,
            &config,
            false,
        )
        .await
        .expect("queue");
        assert!(strict.iter().all(|(k, _)| *k != id(0x55)));
    }

    /// `ClearExcludedTimestampsAsync`: the items the exclusion policy now
    /// matches lose their stored segments and cache, and are republished.
    #[tokio::test]
    async fn clearing_excluded_items_removes_their_data() {
        // Detect first, then exclude the series.
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("detect");
        assert_eq!(
            h.task.clear_excluded().await.expect("nothing excluded"),
            intro_store::ExcludedClear::default()
        );
        let segments = h.actions.stored_item_ids().await.expect("ids").len();
        let cache = cache_files(&h, EP_A).len() + cache_files(&h, EP_B).len();
        assert!(segments == 2 && cache > 0);
        let excluding = DetectSegmentsTask {
            plugins: Arc::new(FakePlugins {
                enabled: true,
                config: br#"{"SeriesExclusions":["show"]}"#.to_vec(),
            }),
            ..h.task.clone()
        };
        let cleared = excluding.clear_excluded().await.expect("clear");
        assert_eq!(cleared.affected_items, 2);
        assert!(cleared.removed_segments >= 2);
        assert_eq!(
            usize::try_from(cleared.removed_cache_entries).unwrap(),
            cache
        );
        assert!(h.actions.stored_item_ids().await.expect("ids").is_empty());
        assert!(
            intros_of(&h.segments, EP_A).await.is_empty(),
            "republished as empty"
        );
    }

    /// `VerifyQueueAsync` + the configuration hash: an analysed season is not
    /// analysed again (no fingerprinting at all), until a setting the mode
    /// reads changes; a user-provided segment survives both.
    #[tokio::test]
    async fn analysis_is_incremental_under_the_config_hash() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("first");
        let first = h.calls.load(Ordering::SeqCst);
        let intro = |segments: Vec<StoredSegment>| {
            segments
                .into_iter()
                .find(|s| s.mode == WireMode::Introduction)
                .expect("intro")
        };
        let hash = intro(h.actions.segments(EP_A).await.expect("tier")).config_hash;
        assert!(first > 0 && !hash.is_empty());
        h.actions
            .update_timestamp(StoredSegment {
                item_id: EP_B,
                mode: WireMode::Introduction,
                start: 1.0,
                end: 2.0,
                is_user_provided: true,
                config_hash: String::new(),
            })
            .await
            .expect("user edit");
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("second");
        assert_eq!(
            h.calls.load(Ordering::SeqCst),
            first,
            "nothing to analyse again"
        );
        // A setting the intro hash reads: re-analysed (from the cached prints)
        // and stored under the new hash; the user's intro stays.
        let changed = DetectSegmentsTask {
            plugins: Arc::new(FakePlugins {
                enabled: true,
                config: br#"{"MaximumIntroDuration":119}"#.to_vec(),
            }),
            ..h.task.clone()
        };
        changed
            .execute(&TaskProgress::default())
            .await
            .expect("third");
        let rehashed = intro(h.actions.segments(EP_A).await.expect("tier"));
        assert_ne!(rehashed.config_hash, hash);
        let user = intro(h.actions.segments(EP_B).await.expect("tier"));
        assert!(user.is_user_provided && user.start == 1.0);
    }

    const THREE_EPISODES: [(Uuid, &str, f64); 3] = [
        (EP_A, "/media/s01e01.mkv", 1800.0),
        (EP_B, "/media/s01e02.mkv", 1800.0),
        (Uuid::from_u128(0xA3), "/media/s01e03.mkv", 1800.0),
    ];

    /// An intro end not snapped to the episode end moves to the first long
    /// enough silence starting in its window, then to the nearest keyframe;
    /// both detections are cached beside the prints.
    #[tokio::test]
    async fn an_intro_end_snaps_to_silence_then_a_keyframe() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        let ffmpeg = Arc::new(crate::ffmpeg::FakeFfmpeg {
            // The match ends at ~49.5 s; its window is [~44.5, ~51.5].
            silences: vec![(40.0, 45.0), (46.0, 46.1), (46.0, 47.0)],
            keyframes: vec![45.9, 48.0],
            ..crate::ffmpeg::FakeFfmpeg::default()
        });
        let task = DetectSegmentsTask {
            ffmpeg: Arc::clone(&ffmpeg) as Arc<dyn FfmpegService>,
            ..h.task.clone()
        };
        task.execute(&TaskProgress::default()).await.expect("run");
        let intro = intros_of(&h.segments, EP_A).await;
        assert_eq!((intro[0].start_ticks, intro[0].end_ticks), (0, 459_000_000));
        // Silence and keyframes for each episode's intro and credits, over
        // the window around the matched end, with the configured noise and
        // process settings.
        assert_eq!(ffmpeg.calls.load(Ordering::SeqCst), 8);
        let log = ffmpeg.log.lock().expect("log").clone();
        let intro_calls: Vec<_> = log.iter().filter(|c| c.2 < 100.0).collect();
        assert_eq!(intro_calls.len(), 4, "{log:?}");
        for (kind, _, start, end, noise, options) in intro_calls {
            assert!(
                (44.0..45.0).contains(start) && (51.0..52.0).contains(end),
                "{kind} {start}-{end}"
            );
            assert_eq!(*noise, if *kind == "silence" { -50 } else { 0 });
            assert_eq!(*options, ProcessOptions::default());
        }
        let files = std::fs::read_dir(h.cache.path().join("introskipper"))
            .expect("cache")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect::<Vec<_>>();
        assert!(files.iter().any(|f| f.ends_with(".silence")), "{files:?}");
        assert!(files.iter().any(|f| f.ends_with(".keyframes")), "{files:?}");
    }

    /// A detection is cached under the settings it reads
    /// (`ConfigHasher.DetectionCache`), and only with `CacheFingerprints`.
    #[tokio::test]
    async fn detections_are_cached_under_their_settings() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        let ffmpeg = Arc::new(crate::ffmpeg::FakeFfmpeg {
            silences: vec![(1.0, 2.0)],
            ..crate::ffmpeg::FakeFfmpeg::default()
        });
        let task = DetectSegmentsTask {
            ffmpeg: Arc::clone(&ffmpeg) as Arc<dyn FfmpegService>,
            ..h.task.clone()
        };
        let base = IntroSkipperConfig::default();
        let queue = task.queue(&base).await.expect("queue");
        let entry = &queue[0].1[0];
        let detect = |config: IntroSkipperConfig| {
            let task = task.clone();
            async move {
                task.silences(
                    entry,
                    AnalysisMode::Introduction,
                    (0.0, 5.0),
                    &config,
                    ProcessOptions::default(),
                )
                .await
                .expect("silences")
            }
        };
        let calls = || ffmpeg.calls.load(Ordering::SeqCst);
        assert_eq!(detect(base.clone()).await, [(1.0, 2.0)]);
        assert_eq!(detect(base.clone()).await, [(1.0, 2.0)]);
        assert_eq!(calls(), 1, "the second read is the cache's");
        let noisier = IntroSkipperConfig {
            silence_detection_maximum_noise: -40,
            ..base.clone()
        };
        detect(noisier).await;
        assert_eq!(calls(), 2, "a setting the detection reads misses");
        let uncached = IntroSkipperConfig {
            cache_fingerprints: false,
            silence_detection_maximum_noise: -30,
            ..base
        };
        detect(uncached.clone()).await;
        detect(uncached).await;
        assert_eq!(calls(), 4, "CacheFingerprints off: always detected");
    }

    /// `ProbeAudioDuration`: credits end where a shorter audio stream does.
    #[tokio::test]
    async fn the_credits_window_ends_with_a_shorter_audio_stream() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        let task = DetectSegmentsTask {
            ffmpeg: Arc::new(crate::ffmpeg::FakeFfmpeg {
                audio_duration: Some(1700.0),
                ..crate::ffmpeg::FakeFfmpeg::default()
            }),
            ..h.task.clone()
        };
        let window = |config: &str| {
            let config: IntroSkipperConfig = serde_json::from_str(config).expect("config");
            let task = task.clone();
            async move {
                let queue = task.queue(&config).await.expect("queue");
                queue[0].1[0].credits_fingerprint_end
            }
        };
        assert_eq!(window("{}").await, 1800.0);
        assert_eq!(window(r#"{"ProbeAudioDuration":true}"#).await, 1700.0);
    }

    /// The task over the harness with a plugin configuration.
    fn configured(h: &Harness, config: &str) -> DetectSegmentsTask {
        DetectSegmentsTask {
            plugins: Arc::new(FakePlugins {
                enabled: true,
                config: config.as_bytes().to_vec(),
            }),
            ..h.task.clone()
        }
    }

    async fn intro_state(h: &Harness) -> SeasonState {
        h.actions
            .season_states(SEASON)
            .await
            .expect("states")
            .remove(&WireMode::Introduction)
            .unwrap_or_default()
    }

    /// A failed segment write ends the season's pass without recording the
    /// episodes as analysed (upstream's exception), so the next pass retries
    /// them instead of taking them for "analysed, nothing found".
    #[tokio::test]
    async fn a_failed_write_is_retried_on_the_next_pass() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.actions.set_read_only(true);
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("failing pass");
        assert!(h.actions.segments(EP_A).await.expect("tier").is_empty());
        assert!(intro_state(&h).await.episode_ids.is_empty());

        h.actions.set_read_only(false);
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("retry");
        assert_eq!(intros_of(&h.segments, EP_A).await.len(), 1);
        assert_eq!(intro_state(&h).await.episode_ids.len(), 2);
    }

    /// A match longer than the mode's maximum is rejected; the episodes are
    /// still recorded as analysed without a result, so the next pass skips
    /// them.
    #[tokio::test]
    async fn an_over_long_match_is_not_stored() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        // The fixture's shared region is ~49.5 s.
        let task = configured(&h, r#"{"MaximumIntroDuration":30}"#);
        task.execute(&TaskProgress::default()).await.expect("run");
        assert!(intros_of(&h.segments, EP_A).await.is_empty());
        assert_eq!(intro_state(&h).await.episode_ids.len(), 2);
        let calls = h.calls.load(Ordering::SeqCst);
        task.execute(&TaskProgress::default()).await.expect("rerun");
        assert_eq!(
            h.calls.load(Ordering::SeqCst),
            calls,
            "NoSegments is cached"
        );
    }

    /// An item whose file is gone leaves the queue (`File.Exists`); a lone
    /// remaining episode has nothing to be compared with.
    #[tokio::test]
    async fn an_item_without_its_file_is_skipped() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        std::fs::remove_file(h.cache.path().join("media/s01e02.mkv")).expect("remove");
        h.task.execute(&TaskProgress::default()).await.expect("run");
        assert_eq!(h.calls.load(Ordering::SeqCst), 0);
        assert!(intros_of(&h.segments, EP_A).await.is_empty());
    }

    /// The only episode still needing analysis is compared with its analysed
    /// (here user-provided) neighbour, whose segment stays.
    #[tokio::test]
    async fn a_lone_episode_is_compared_with_its_neighbour() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.actions
            .update_timestamp(StoredSegment {
                item_id: EP_B,
                mode: WireMode::Introduction,
                start: 1.0,
                end: 2.0,
                is_user_provided: true,
                config_hash: String::new(),
            })
            .await
            .expect("user edit");
        h.task.execute(&TaskProgress::default()).await.expect("run");
        let a = h.actions.segments(EP_A).await.expect("tier");
        assert!(
            a.iter()
                .any(|s| s.mode == WireMode::Introduction && !s.is_user_provided)
        );
        let b = h.actions.segments(EP_B).await.expect("tier");
        assert!(
            b.iter()
                .any(|s| s.mode == WireMode::Introduction && s.is_user_provided && s.end == 2.0)
        );
    }

    /// A settled season (no new episode for the delay; an unknown date counts
    /// as long settled) is analysed again once, and the episodes it covered
    /// are recorded so the next pass does not repeat it.
    #[tokio::test]
    async fn a_settled_season_is_reanalysed_once() {
        let h = harness(&THREE_EPISODES, true, "{}", true).await;
        let task = configured(&h, r#"{"ReanalyzeSettledSeasons":true}"#);
        task.execute(&TaskProgress::default()).await.expect("run");
        assert_eq!(intro_state(&h).await.settled_episode_ids.len(), 3);
        assert_eq!(intros_of(&h.segments, EP_A).await.len(), 1);
        let before = h.actions.segments(EP_A).await.expect("tier");
        task.execute(&TaskProgress::default()).await.expect("rerun");
        assert_eq!(h.actions.segments(EP_A).await.expect("tier"), before);
    }

    /// An anime preview derived from the credits survives the Preview mode's
    /// stale-segment cleanup that follows it (upstream stores it under the
    /// Credits hash, so that cleanup deletes it — a bug not ported).
    #[tokio::test]
    async fn an_anime_preview_survives_the_preview_pass() {
        let h = harness(
            &TWO_EPISODES,
            true,
            r#"{"AnimePreviewFromCreditsEnd":true}"#,
            true,
        )
        .await;
        let series_type = ferrofin_core::item_type_lookup::stored_type_name(BaseItemKind::Series)
            .expect("stored type name");
        h.persistence
            .save_items(&[BaseItemEntity {
                id: guid_to_db(SERIES),
                type_: series_type.to_owned(),
                name: Some("Show".to_owned()),
                genres: Some("Anime".to_owned()),
                ..BaseItemEntity::default()
            }])
            .await
            .expect("anime series");
        h.task.execute(&TaskProgress::default()).await.expect("run");
        let preview = |segments: Vec<StoredSegment>| {
            segments.into_iter().find(|s| s.mode == WireMode::Preview)
        };
        let first = preview(h.actions.segments(EP_A).await.expect("tier")).expect("anime preview");
        assert_eq!(first.end, 1800.0);
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("rerun");
        assert_eq!(
            preview(h.actions.segments(EP_A).await.expect("tier")),
            Some(first)
        );
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
    async fn a_malformed_stored_config_falls_back_to_defaults() {
        let h = harness(&TWO_EPISODES, true, "not json", true).await;
        assert_eq!(h.task.load_config().await.analysis_percent, 25);
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
