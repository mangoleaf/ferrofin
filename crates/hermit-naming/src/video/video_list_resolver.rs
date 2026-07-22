//! Port of `Emby.Naming.Video.VideoListResolver`.

use std::sync::OnceLock;

use hermit_model::data::CollectionType;
use regex::Regex;

use crate::common::NamingOptions;
use crate::io::FileSystemMetadata;
use crate::path;
use crate::tv::EpisodePathParser;
use crate::video::numeric_ordering::numeric_ordinal_cmp;
use crate::video::{VideoFileInfo, VideoInfo, clean_string_parser, stack_resolver, video_resolver};

fn resolution_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)[0-9]{2}[0-9]+[ip]").expect("resolution regex valid"))
}

fn check_multi_version_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\[([^]]*)\]").expect("multi-version regex valid"))
}

/// Resolves alternative versions and extras from a list of video files.
pub struct VideoListResolver<'a> {
    naming_options: &'a NamingOptions,
}

impl<'a> VideoListResolver<'a> {
    /// Creates a new [`VideoListResolver`].
    #[must_use]
    pub fn new(naming_options: &'a NamingOptions) -> Self {
        Self { naming_options }
    }

    /// Resolves alternative versions and extras from a list of video files.
    #[must_use]
    pub fn resolve(
        &self,
        video_infos: &[VideoFileInfo],
        support_multi_version: bool,
        parse_name: bool,
        library_root: Option<&str>,
        collection_type: Option<CollectionType>,
    ) -> Vec<VideoInfo> {
        // Filter out extras so they don't break stack resolution.
        let non_extras: Vec<FileSystemMetadata> = video_infos
            .iter()
            .filter(|i| i.extra_type.is_none())
            .map(|i| FileSystemMetadata::new(i.path.clone(), i.is_directory))
            .collect();

        let stack_result = stack_resolver::resolve(&non_extras, self.naming_options);

        let mut remaining_files: Vec<VideoFileInfo> = Vec::new();
        let mut standalone_media: Vec<VideoFileInfo> = Vec::new();

        for current in video_infos {
            if stack_result
                .iter()
                .any(|s| s.contains_file(&current.path, current.is_directory))
            {
                continue;
            }

            if current.extra_type.is_none() {
                standalone_media.push(current.clone());
            } else {
                remaining_files.push(current.clone());
            }
        }

        let mut list: Vec<VideoInfo> = Vec::new();

        for stack in &stack_result {
            let files: Vec<VideoFileInfo> = stack
                .files
                .iter()
                .filter_map(|i| {
                    video_resolver::resolve(
                        Some(i),
                        stack.is_directory_stack,
                        self.naming_options,
                        parse_name,
                        library_root,
                    )
                })
                .collect();

            let mut info = VideoInfo::new(Some(stack.name.clone()));
            info.year = files.first().and_then(|f| f.year);
            info.files = files;
            list.push(info);
        }

        for media in standalone_media {
            let mut info = VideoInfo::new(Some(media.name.clone()));
            info.files = vec![media];
            info.year = info.files.first().and_then(|f| f.year);
            list.push(info);
        }

        if support_multi_version {
            list = if collection_type == Some(CollectionType::tvshows) {
                self.get_episodes_grouped_by_version(list)
            } else {
                self.get_videos_grouped_by_version(list)
            };
        }

        // Whatever files are left, just add them.
        for i in remaining_files {
            let mut info = VideoInfo::new(Some(i.name.clone()));
            info.year = i.year;
            info.extra_type = i.extra_type;
            info.files = vec![i];
            list.push(info);
        }

        list
    }

    /// Convenience wrapper mirroring the common single-argument C# call.
    #[must_use]
    pub fn resolve_simple(&self, video_infos: &[VideoFileInfo]) -> Vec<VideoInfo> {
        self.resolve(video_infos, true, true, None, None)
    }

    fn get_videos_grouped_by_version(&self, videos: Vec<VideoInfo>) -> Vec<VideoInfo> {
        if videos.is_empty() {
            return videos;
        }

        let first_path = &videos[0].files[0].path;
        let folder_name = path::directory_name(first_path).map_or("", path::file_name);

        if folder_name.chars().count() <= 1 || !have_same_year(&videos) {
            return videos;
        }

        let mut primary_index: Option<usize> = None;
        for (idx, video) in videos.iter().enumerate() {
            if video.extra_type.is_some() {
                continue;
            }

            let test_filename = video.files[0].file_name_without_extension();
            if !self.is_eligible_for_multi_version(folder_name, test_filename) {
                return videos;
            }

            if folder_name == test_filename {
                primary_index = Some(idx);
            }
        }

        let folder_name = folder_name.to_string();
        let organized = organize_alternate_versions(videos, primary_index, Some(folder_name));
        vec![organized]
    }

    fn is_eligible_for_multi_version(&self, folder_name: &str, test_filename: &str) -> bool {
        if !starts_with_ignore_ascii_case(test_filename, folder_name) {
            return false;
        }

        // Remove the folder name before cleaning.
        let mut test = test_filename;
        if folder_name.len() <= test_filename.len() {
            test = test_filename[folder_name.len()..].trim();
        }

        let cleaned;
        if let Some(clean_name) =
            clean_string_parser::try_clean(Some(test), &self.naming_options.clean_string_regexes)
        {
            cleaned = clean_name;
            test = cleaned.as_str().trim();
        }

        test.is_empty()
            || test.starts_with('-')
            || test.starts_with('_')
            || test.starts_with('.')
            || check_multi_version_regex().is_match(test)
    }

    fn get_episodes_grouped_by_version(&self, videos: Vec<VideoInfo>) -> Vec<VideoInfo> {
        if videos.len() < 2 {
            return videos;
        }

        let parser = EpisodePathParser::new(self.naming_options);
        let mut result: Vec<VideoInfo> = Vec::new();

        // Insertion-ordered grouping by episode key.
        let mut keys: Vec<String> = Vec::new();
        let mut groups: Vec<Vec<VideoInfo>> = Vec::new();

        for video in videos {
            let episode_result = parser.parse(&video.files[0].path, false, None, None, None, false);
            let mut key: Option<String> = None;
            if episode_result.success {
                if let (true, Some(y), Some(m), Some(d)) = (
                    episode_result.is_by_date,
                    episode_result.year,
                    episode_result.month,
                    episode_result.day,
                ) {
                    key = Some(format!("D{y}{m:02}{d:02}"));
                } else if let Some(ep) = episode_result.episode_number {
                    key = Some(format!(
                        "S{}E{ep}",
                        episode_result.season_number.unwrap_or(0)
                    ));
                }
            }

            let Some(key) = key else {
                result.push(video);
                continue;
            };

            if let Some(pos) = keys.iter().position(|k| k.eq_ignore_ascii_case(&key)) {
                groups[pos].push(video);
            } else {
                keys.push(key);
                groups.push(vec![video]);
            }
        }

        for group in groups {
            if group.len() == 1 {
                result.extend(group);
                continue;
            }
            result.push(organize_alternate_versions(group, None, None));
        }

        result
    }
}

fn have_same_year(videos: &[VideoInfo]) -> bool {
    if videos.len() == 1 {
        return true;
    }
    let first_year = videos[0].year.unwrap_or(-1);
    videos[1..]
        .iter()
        .all(|v| v.year.unwrap_or(-1) == first_year)
}

fn organize_alternate_versions(
    mut videos: Vec<VideoInfo>,
    primary_override: Option<usize>,
    name_override: Option<String>,
) -> VideoInfo {
    // `primary_override` is an index into the *incoming* `videos`; capture the
    // corresponding path so we can re-find it after re-ordering.
    let primary_override_path = primary_override.map(|idx| videos[idx].files[0].path.clone());

    if videos.len() > 1 {
        // Pair each video with (filename, resolution match value).
        let mut matched: Vec<(String, String, VideoInfo)> = Vec::new();
        let mut unmatched: Vec<(String, VideoInfo)> = Vec::new();

        for v in videos {
            let filename = v.files[0].file_name_without_extension().to_string();
            match resolution_regex().find(&filename) {
                Some(m) => matched.push((filename.clone(), m.as_str().to_string(), v)),
                None => unmatched.push((filename, v)),
            }
        }

        // Matched: order by resolution value desc, then filename asc.
        matched.sort_by(|a, b| {
            numeric_ordinal_cmp(&b.1, &a.1).then_with(|| numeric_ordinal_cmp(&a.0, &b.0))
        });
        // Unmatched: order by filename asc.
        unmatched.sort_by(|a, b| numeric_ordinal_cmp(&a.0, &b.0));

        // Matched are prepended (InsertRange(0)), unmatched appended.
        videos = matched
            .into_iter()
            .map(|(_, _, v)| v)
            .chain(unmatched.into_iter().map(|(_, v)| v))
            .collect();
    }

    // Prefer a stacked entry (more than one part) as primary.
    let primary_index = primary_override_path
        .and_then(|p| videos.iter().position(|v| v.files[0].path == p))
        .or_else(|| videos.iter().position(|v| v.files.len() > 1))
        .unwrap_or(0);

    let mut primary = videos.remove(primary_index);
    primary.alternate_versions = videos;

    if let Some(name) = name_override {
        primary.name = Some(name);
    }

    primary
}

fn starts_with_ignore_ascii_case(haystack: &str, prefix: &str) -> bool {
    haystack
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}
