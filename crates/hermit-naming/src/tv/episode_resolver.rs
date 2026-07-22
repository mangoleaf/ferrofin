//! Port of `Emby.Naming.TV.EpisodeResolver`.

use crate::common::NamingOptions;
use crate::path;
use crate::tv::{EpisodeInfo, EpisodePathParser};
use crate::video::{format_3d_parser, stub_resolver};

/// Resolves information about an episode from a path.
pub struct EpisodeResolver<'a> {
    options: &'a NamingOptions,
}

impl<'a> EpisodeResolver<'a> {
    /// Creates a new [`EpisodeResolver`].
    #[must_use]
    pub fn new(options: &'a NamingOptions) -> Self {
        Self { options }
    }

    /// Resolves information about an episode from a path.
    #[must_use]
    pub fn resolve(
        &self,
        path_str: &str,
        is_directory: bool,
        is_named: Option<bool>,
        is_optimistic: Option<bool>,
        supports_absolute_numbers: Option<bool>,
        fill_extended_info: bool,
    ) -> Option<EpisodeInfo> {
        let mut is_stub = false;
        let mut container: Option<String> = None;
        let mut stub_type: Option<String> = None;

        if !is_directory {
            let extension = path::extension(path_str);
            if self
                .options
                .video_file_extensions
                .iter()
                .any(|e| e.eq_ignore_ascii_case(extension))
            {
                container = Some(extension.trim_start_matches('.').to_string());
            } else {
                let stub = stub_resolver::try_resolve_file(path_str, self.options);
                if !stub.is_stub {
                    return None;
                }
                stub_type = stub.stub_type;
                is_stub = true;
                container = Some(extension.trim_start_matches('.').to_string());
            }
        }

        let format_3d_result = format_3d_parser::parse(path_str, self.options);

        let parsing_result = EpisodePathParser::new(self.options).parse(
            path_str,
            is_directory,
            is_named,
            is_optimistic,
            supports_absolute_numbers,
            fill_extended_info,
        );

        if !parsing_result.success && !is_stub {
            return None;
        }

        Some(EpisodeInfo {
            path: path_str.to_string(),
            container,
            is_stub,
            ending_episode_number: parsing_result.ending_episode_number,
            episode_number: parsing_result.episode_number,
            season_number: parsing_result.season_number,
            series_name: parsing_result.series_name,
            stub_type,
            is_3d: format_3d_result.is_3d,
            format_3d: format_3d_result.format_3d,
            is_by_date: parsing_result.is_by_date,
            day: parsing_result.day,
            month: parsing_result.month,
            year: parsing_result.year,
        })
    }

    /// Convenience wrapper mirroring the common two-argument C# call.
    #[must_use]
    pub fn resolve_simple(&self, path_str: &str, is_directory: bool) -> Option<EpisodeInfo> {
        self.resolve(path_str, is_directory, None, None, None, true)
    }
}
