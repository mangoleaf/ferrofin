//! The Intro Skipper's analysis queue — port of the plugin's `QueueManager`
//! (`GetMediaItems`), `ExclusionPolicy` and `SeriesHelper` (intro-skipper
//! `db09359`).
//!
//! For every library whose `DisabledMediaSegmentProviders` does not name the
//! plugin, each Episode and Movie with a path is queued under its season (a
//! movie under its own id), unless an exclusion matches. In-season specials
//! join the season they aired in; each entry carries its fingerprint windows.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::options::InternalItemsQuery;
use uuid::Uuid;

use super::IntroSkipperConfig;
use crate::ffmpeg::{FfmpegService, ProcessOptions};

/// The plugin's name, as a library's `DisabledMediaSegmentProviders` lists it.
const PLUGIN_NAME: &str = "Intro Skipper";
/// Below this many seconds an item is fingerprinted whole (`5 * 60`).
const FULL_LENGTH_BELOW: f64 = 300.0;
/// `PluginConfiguration.MinimumAnalysisPercent` / `MaximumAnalysisPercent`.
const ANALYSIS_PERCENT: (i32, i32) = (1, 50);

/// What a queued item is (`QueuedMediaCategory`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Category {
    /// A TV episode.
    Episode,
    /// An episode of a series tagged or genred "anime".
    AnimeEpisode,
    /// A movie (queued under its own id).
    Movie,
}

/// One queued item (`QueuedEpisode`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct QueuedEpisode {
    /// The series (a movie's own) name.
    pub series_name: String,
    /// The aired season number (0 for a movie).
    pub season_number: i64,
    /// The series (a movie's own) id.
    pub series_id: Uuid,
    /// The season (a movie's own) id.
    pub season_id: Uuid,
    /// The episode number.
    pub episode_number: i64,
    /// The item.
    pub episode_id: Uuid,
    /// The item's name.
    pub name: String,
    /// What it is.
    pub category: Category,
    /// Matched an exclusion (only queued when asked to include those).
    pub is_excluded: bool,
    /// The media file.
    pub path: String,
    /// Its runtime, seconds.
    pub duration: f64,
    /// When it became available (`DateCreated`, else `DateLastSaved`).
    pub date_added: Option<DateTime<Utc>>,
    /// The intro window's end (it starts at 0).
    pub intro_fingerprint_end: f64,
    /// The credits window's start.
    pub credits_fingerprint_start: f64,
    /// The credits window's end.
    pub credits_fingerprint_end: f64,
}

/// The queue: each season (or movie) id with its items, in the order the
/// libraries were walked (series sort name, seasons descending, episodes
/// ascending).
pub(super) type Queue = Vec<(Uuid, Vec<QueuedEpisode>)>;

/// Builds the queue (`QueueManager.GetMediaItems(includeExcluded)`).
///
/// # Errors
/// A library or virtual-folder read failed.
pub(super) async fn build(
    library: &dyn LibraryManager,
    ffmpeg: &dyn FfmpegService,
    folders: &[VirtualFolderInfo],
    config: &IntroSkipperConfig,
    include_excluded: bool,
) -> Result<Queue, ServiceError> {
    let policy = ExclusionPolicy::from_config(config);
    let analysis_percent = f64::from(
        config
            .analysis_percent
            .clamp(ANALYSIS_PERCENT.0, ANALYSIS_PERCENT.1),
    ) / 100.0;
    let items = library
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Episode, BaseItemKind::Movie],
            recursive: true,
            is_virtual_item: Some(false),
            order_by: vec![
                (ItemSortBy::SeriesSortName, SortOrder::Ascending),
                (ItemSortBy::ParentIndexNumber, SortOrder::Descending),
                (ItemSortBy::IndexNumber, SortOrder::Ascending),
            ],
            ..InternalItemsQuery::default()
        })
        .await?;
    let mut queue = QueueBuilder {
        queue: Vec::new(),
        positions: HashMap::new(),
        anime: HashMap::new(),
    };
    let mut seen = HashSet::new();
    for folder in folders {
        // "If libraries have been selected for analysis, ensure this library
        // was selected."
        if folder.library_options.as_ref().is_some_and(|o| {
            o.disabled_media_segment_providers
                .iter()
                .any(|p| p == PLUGIN_NAME)
        }) {
            tracing::debug!(
                library = folder.name.as_deref().unwrap_or_default(),
                "intro skipper: library disabled for analysis"
            );
            continue;
        }
        for item in items.iter().filter(|item| in_folder(item, folder)) {
            if !seen.insert(item.id.clone()) {
                continue;
            }
            match kind(item) {
                Some(BaseItemKind::Episode) => {
                    queue
                        .episode(
                            (library, ffmpeg),
                            item,
                            &policy,
                            config,
                            analysis_percent,
                            include_excluded,
                        )
                        .await;
                }
                Some(BaseItemKind::Movie) => {
                    queue
                        .movie(ffmpeg, item, &policy, config, include_excluded)
                        .await;
                }
                _ => {}
            }
        }
    }
    Ok(queue.queue)
}

/// Whether `item`'s file lives under one of the folder's locations
/// (`GetLibraryOptions(item)`'s resolution, as the library tasks use).
fn in_folder(item: &BaseItemEntity, folder: &VirtualFolderInfo) -> bool {
    item.path.as_deref().is_some_and(|path| {
        folder
            .locations
            .iter()
            .any(|location| Path::new(path).starts_with(location))
    })
}

/// The stored type's kind (the last dotted segment of the CLR name).
pub(super) fn kind(item: &BaseItemEntity) -> Option<BaseItemKind> {
    match item.type_.rsplit('.').next() {
        Some("Episode") => Some(BaseItemKind::Episode),
        Some("Movie") => Some(BaseItemKind::Movie),
        _ => None,
    }
}

/// A stored GUID column.
fn guid(value: Option<&str>) -> Uuid {
    value
        .and_then(|v| Uuid::parse_str(v).ok())
        .unwrap_or_default()
}

/// The item's runtime, seconds.
#[allow(clippy::cast_precision_loss)]
fn duration(item: &BaseItemEntity) -> f64 {
    item.run_time_ticks.unwrap_or(0) as f64 / 10_000_000.0
}

/// `Episode.AiredSeasonNumber`: `AirsAfterSeasonNumber ?? AirsBeforeSeasonNumber
/// ?? ParentIndexNumber` (the airs-numbers live in the item's `Data` blob).
fn aired_season_number(item: &BaseItemEntity) -> Option<i64> {
    let blob = item
        .data
        .as_deref()
        .and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok());
    let number = |key: &str| {
        blob.as_ref()
            .and_then(|b| b.get(key))
            .and_then(serde_json::Value::as_i64)
    };
    number("AirsAfterSeasonNumber")
        .or_else(|| number("AirsBeforeSeasonNumber"))
        .or(item.parent_index_number)
}

/// The queue under construction, with the anime lookups it caches per series.
struct QueueBuilder {
    queue: Queue,
    /// Each season's index in `queue`, so a large library's queue builds in
    /// linear time (the queue keeps upstream's insertion order).
    positions: HashMap<Uuid, usize>,
    anime: HashMap<Uuid, bool>,
}

impl QueueBuilder {
    fn season(&mut self, id: Uuid) -> &mut Vec<QueuedEpisode> {
        let queue = &mut self.queue;
        let index = *self.positions.entry(id).or_insert_with(|| {
            queue.push((id, Vec::new()));
            queue.len() - 1
        });
        &mut self.queue[index].1
    }

    /// `QueueManager.QueueEpisode`.
    async fn episode(
        &mut self,
        (library, ffmpeg): (&dyn LibraryManager, &dyn FfmpegService),
        item: &BaseItemEntity,
        policy: &ExclusionPolicy,
        config: &IntroSkipperConfig,
        analysis_percent: f64,
        include_excluded: bool,
    ) {
        let Some(path) = item.path.clone().filter(|p| !p.is_empty()) else {
            return;
        };
        let Ok(episode_id) = Uuid::parse_str(&item.id) else {
            return;
        };
        let series_name = item.series_name.clone().unwrap_or_default();
        let series_id = guid(item.series_id.as_deref());
        let excluded = policy.series(&series_name, &path);
        if excluded && !include_excluded {
            tracing::debug!(
                item = item.name.as_deref().unwrap_or_default(),
                "intro skipper: excluded"
            );
            return;
        }
        let aired_season = aired_season_number(item).unwrap_or(0);
        let season_id = self.season_id(item, episode_id, series_id, aired_season);
        let duration = duration(item);
        let credits_duration = if excluded {
            duration
        } else {
            credits_end(ffmpeg, &path, duration, config).await
        };
        let (intro_end, credits_start, credits_end) =
            episode_windows(duration, credits_duration, analysis_percent, config);
        let category = self.category(library, series_id, season_id).await;
        self.season(season_id).push(QueuedEpisode {
            series_name,
            season_number: aired_season,
            series_id,
            season_id,
            episode_number: item.index_number.unwrap_or(0),
            episode_id,
            name: item.name.clone().unwrap_or_default(),
            category,
            is_excluded: excluded,
            path,
            duration,
            date_added: item.date_created.or(item.date_last_saved),
            intro_fingerprint_end: intro_end,
            credits_fingerprint_start: credits_start,
            credits_fingerprint_end: credits_end,
        });
    }

    /// `QueueManager.GetSeasonId`: an in-season special (season 0 that aired
    /// within a season) joins that season's queue when one is already there.
    /// A missing season id falls back to the episode's own (upstream first
    /// re-runs a metadata refresh; Ferrofin's scanner always assigns one, so
    /// only a broken row gets here).
    fn season_id(
        &self,
        item: &BaseItemEntity,
        episode_id: Uuid,
        series_id: Uuid,
        aired_season: i64,
    ) -> Uuid {
        if item.parent_index_number == Some(0) && aired_season != 0 {
            for (key, episodes) in &self.queue {
                if let Some(first) = episodes.first()
                    && first.series_id == series_id
                    && first.season_number == aired_season
                {
                    return *key;
                }
            }
        }
        let season = guid(item.season_id.as_deref());
        if season.is_nil() { episode_id } else { season }
    }

    /// `ResolveEpisodeCategory`: the season's first entry decides, else the
    /// series' tags and genres (`SeriesHelper.IsAnime`).
    async fn category(
        &mut self,
        library: &dyn LibraryManager,
        series_id: Uuid,
        season_id: Uuid,
    ) -> Category {
        if let Some(&index) = self.positions.get(&season_id)
            && let Some(first) = self.queue[index].1.first()
            && first.category != Category::Movie
        {
            return first.category;
        }
        let anime = if let Some(anime) = self.anime.get(&series_id) {
            *anime
        } else {
            let anime = matches!(
                library.get_item_by_id(series_id).await,
                Ok(Some(series)) if is_anime(&series)
            );
            self.anime.insert(series_id, anime);
            anime
        };
        if anime {
            Category::AnimeEpisode
        } else {
            Category::Episode
        }
    }

    /// `QueueManager.QueueMovieAsync`: queued under its own id, credits only.
    async fn movie(
        &mut self,
        ffmpeg: &dyn FfmpegService,
        item: &BaseItemEntity,
        policy: &ExclusionPolicy,
        config: &IntroSkipperConfig,
        include_excluded: bool,
    ) {
        let Some(path) = item.path.clone().filter(|p| !p.is_empty()) else {
            return;
        };
        let Ok(movie_id) = Uuid::parse_str(&item.id) else {
            return;
        };
        let name = item.name.clone().unwrap_or_default();
        let excluded = policy.movie(&name, &path);
        if excluded && !include_excluded {
            tracing::debug!(item = name, "intro skipper: excluded");
            return;
        }
        let duration = duration(item);
        let credits_duration = if excluded {
            duration
        } else {
            credits_end(ffmpeg, &path, duration, config).await
        };
        self.season(movie_id).push(QueuedEpisode {
            series_name: name.clone(),
            season_number: 0,
            series_id: movie_id,
            season_id: movie_id,
            episode_number: 0,
            episode_id: movie_id,
            name,
            category: Category::Movie,
            is_excluded: excluded,
            path,
            duration,
            date_added: None,
            intro_fingerprint_end: 0.0,
            credits_fingerprint_start: (credits_duration
                - f64::from(config.maximum_movie_credits_duration))
            .max(0.0),
            credits_fingerprint_end: credits_duration,
        });
    }
}

/// `ResolveCreditsFingerprintEndAsync`: with `ProbeAudioDuration`, the audio
/// stream's end when it is shorter than the runtime (a file whose video
/// outlasts its audio), else the runtime.
async fn credits_end(
    ffmpeg: &dyn FfmpegService,
    path: &str,
    duration: f64,
    config: &IntroSkipperConfig,
) -> f64 {
    if !config.probe_audio_duration {
        return duration;
    }
    let options = ProcessOptions::new(&config.process_priority, config.process_threads);
    ffmpeg
        .probe_audio_duration(path, options)
        .await
        .filter(|audio| *audio > 0.0 && *audio < duration)
        .unwrap_or(duration)
}

/// An episode's fingerprint windows, `(intro end, credits start, credits end)`,
/// as `QueueEpisode` computes them: an item of 5 minutes or more is
/// fingerprinted over its `AnalysisPercent`, a shorter one whole; the intro
/// window is capped at `AnalysisLengthLimit` minutes and the credits window
/// at `MaximumCreditsDuration` minutes (the plugin's `60 *` on a setting it
/// otherwise reads in seconds — kept, it only ever caps far beyond the
/// percent). `credits_duration` is where the credits window ends (the
/// runtime, or a probed shorter audio duration).
fn episode_windows(
    duration: f64,
    credits_duration: f64,
    analysis_percent: f64,
    config: &IntroSkipperConfig,
) -> (f64, f64, f64) {
    let share = |length: f64| {
        if length >= FULL_LENGTH_BELOW {
            length * analysis_percent
        } else {
            length
        }
    };
    let intro_end = share(duration).min(60.0 * f64::from(config.analysis_length_limit));
    let max_credits =
        share(credits_duration).min(60.0 * f64::from(config.maximum_credits_duration));
    (
        intro_end,
        (credits_duration - max_credits).max(0.0),
        credits_duration,
    )
}

/// `SeriesHelper.IsAnime`: a tag or genre equal to "anime", in any case.
pub(super) fn is_anime(series: &BaseItemEntity) -> bool {
    [series.tags.as_deref(), series.genres.as_deref()]
        .into_iter()
        .flatten()
        .flat_map(|list| list.split('|'))
        .any(|value| value.trim().eq_ignore_ascii_case("anime"))
}

/// `ExclusionPolicy`: series and movie names (exact, any case) and path roots.
#[derive(Debug, Default)]
pub(super) struct ExclusionPolicy {
    series: HashSet<String>,
    movies: HashSet<String>,
    roots: Vec<String>,
}

impl ExclusionPolicy {
    /// `ExclusionPolicy.FromConfiguration`, with the legacy comma-separated
    /// `ExcludeSeries` folded in while `SeriesExclusions` is empty
    /// (`Plugin.MigrateLegacyExcludeSeries`).
    pub(super) fn from_config(config: &IntroSkipperConfig) -> Self {
        let names = |entries: &mut dyn Iterator<Item = &str>| -> HashSet<String> {
            entries
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_lowercase)
                .collect()
        };
        let series = if config.series_exclusions.is_empty() {
            names(&mut config.exclude_series.split(','))
        } else {
            names(&mut config.series_exclusions.iter().map(String::as_str))
        };
        let mut roots: Vec<String> = Vec::new();
        for root in config.path_exclusions.iter().map(|p| normalize(p)) {
            if !root.is_empty() && !roots.iter().any(|r| r.eq_ignore_ascii_case(&root)) {
                roots.push(root);
            }
        }
        let broad = roots.iter().filter(|r| is_broad_root(r)).count();
        if broad > 0 {
            tracing::warn!(
                broad,
                "intro skipper: path exclusions include whole drives or shares"
            );
        }
        Self {
            series,
            movies: names(&mut config.movie_exclusions.iter().map(String::as_str)),
            roots,
        }
    }

    /// `EvaluateSeries`: the path first, then the series name.
    pub(super) fn series(&self, series_name: &str, path: &str) -> bool {
        self.path(path) || self.series.contains(&series_name.trim().to_lowercase())
    }

    /// `EvaluateMovie`: the path first, then the movie name.
    pub(super) fn movie(&self, movie_name: &str, path: &str) -> bool {
        self.path(path) || self.movies.contains(&movie_name.trim().to_lowercase())
    }

    /// `IsPathExcluded`: the path equals a root or lies under it, in any case.
    pub(super) fn path(&self, path: &str) -> bool {
        let path = normalize(path);
        !path.is_empty()
            && self.roots.iter().any(|root| {
                path.eq_ignore_ascii_case(root)
                    || (root == "/" && path.starts_with('/'))
                    || path
                        .to_lowercase()
                        .starts_with(&format!("{}/", root.to_lowercase()))
            })
    }
}

/// `NormalizePath`: trimmed, `\` → `/`, no trailing `/` (a drive root `C:/`
/// becomes `C:`).
fn normalize(path: &str) -> String {
    let mut path = path.trim().replace('\\', "/");
    if path.is_empty() {
        return path;
    }
    while path.len() > 1 && path.ends_with('/') && !is_drive_root_with_separator(&path) {
        path.pop();
    }
    if is_drive_root_with_separator(&path) {
        path.truncate(2);
    }
    path
}

fn is_drive_root_with_separator(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() == 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

/// `IsBroadPathRoot`: `/`, a drive (`C:`), or a bare `//server/share`.
fn is_broad_root(path: &str) -> bool {
    let bytes = path.as_bytes();
    path == "/"
        || (bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || (path.starts_with("//") && path.split('/').filter(|s| !s.is_empty()).count() == 2)
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact window arithmetic
mod tests {
    use super::*;

    fn config(json: &str) -> IntroSkipperConfig {
        serde_json::from_str(json).expect("config")
    }

    #[test]
    fn exclusions_follow_the_exclusion_policy() {
        let policy = ExclusionPolicy::from_config(&config(
            r#"{"SeriesExclusions":[" The Show "],"MovieExclusions":["Film"],
                "PathExclusions":["/media/kids/","C:\\Rips\\"]}"#,
        ));
        assert!(policy.series("the show", "/media/tv/a.mkv"));
        assert!(!policy.series("The Show 2", "/media/tv/a.mkv"));
        assert!(policy.series("Other", "/media/kids/b.mkv"));
        assert!(policy.series("Other", "/MEDIA/KIDS"));
        assert!(!policy.series("Other", "/media/kidsclub/b.mkv"));
        assert!(policy.movie("FILM", "/media/movies/f.mkv"));
        assert!(policy.movie("Other", "c:/rips/x.mkv"));
        assert!(!policy.movie("Other", ""));
    }

    #[test]
    fn the_legacy_exclude_series_string_still_excludes() {
        let policy = ExclusionPolicy::from_config(&config(r#"{"ExcludeSeries":"A, B ,"}"#));
        assert!(policy.series("a", "/x") && policy.series("B", "/x"));
        // Superseded once the list has entries.
        let policy = ExclusionPolicy::from_config(&config(
            r#"{"ExcludeSeries":"A","SeriesExclusions":["C"]}"#,
        ));
        assert!(!policy.series("A", "/x") && policy.series("c", "/x"));
    }

    #[test]
    fn paths_normalize_like_normalize_path() {
        assert_eq!(normalize(" /media/tv/ "), "/media/tv");
        assert_eq!(normalize("C:\\"), "C:");
        assert_eq!(normalize("/"), "/");
        assert!(is_broad_root("/") && is_broad_root("D:") && is_broad_root("//nas/share"));
        assert!(!is_broad_root("//nas/share/tv") && !is_broad_root("/media"));
        let all = ExclusionPolicy::from_config(&config(r#"{"PathExclusions":["/"]}"#));
        assert!(all.path("/anything"));
    }

    #[test]
    fn windows_follow_queue_episode() {
        let cfg = IntroSkipperConfig::default();
        // 25 % of 2000 s = 500 s, under the 10-minute cap.
        assert_eq!(episode_windows(2000.0, 2000.0, 0.25, &cfg).0, 500.0);
        // 25 % of 4000 s = 1000 s, capped at 600 s.
        assert_eq!(episode_windows(4000.0, 4000.0, 0.25, &cfg).0, 600.0);
        // The credits window is the last 25 % of the episode.
        assert_eq!(
            episode_windows(3600.0, 3600.0, 0.25, &cfg),
            (600.0, 2700.0, 3600.0)
        );
        // Under 5 minutes an item is fingerprinted whole.
        assert_eq!(
            episode_windows(200.0, 200.0, 0.25, &cfg),
            (200.0, 0.0, 200.0)
        );
    }

    #[test]
    fn anime_is_a_tag_or_genre() {
        let series = BaseItemEntity {
            genres: Some("Action|Anime".to_owned()),
            ..BaseItemEntity::default()
        };
        assert!(is_anime(&series));
        assert!(!is_anime(&BaseItemEntity {
            tags: Some("animation".to_owned()),
            ..BaseItemEntity::default()
        }));
    }

    #[test]
    fn aired_season_reads_the_airs_numbers_first() {
        let special = BaseItemEntity {
            parent_index_number: Some(0),
            data: Some(r#"{"AirsBeforeSeasonNumber":2}"#.to_owned()),
            ..BaseItemEntity::default()
        };
        assert_eq!(aired_season_number(&special), Some(2));
        let after = BaseItemEntity {
            parent_index_number: Some(0),
            data: Some(r#"{"AirsBeforeSeasonNumber":2,"AirsAfterSeasonNumber":3}"#.to_owned()),
            ..BaseItemEntity::default()
        };
        assert_eq!(aired_season_number(&after), Some(3));
        assert_eq!(
            aired_season_number(&BaseItemEntity {
                parent_index_number: Some(4),
                ..BaseItemEntity::default()
            }),
            Some(4)
        );
    }
}
