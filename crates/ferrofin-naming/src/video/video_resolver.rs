//! Port of `Emby.Naming.Video.VideoResolver`.

use crate::common::NamingOptions;
use crate::path;
use crate::video::{
    CleanDateTimeResult, ExtraResult, VideoFileInfo, clean_date_time_parser, clean_string_parser,
    extra_rule_resolver, format_3d_parser, stub_resolver,
};

/// Resolves a directory into a [`VideoFileInfo`].
#[must_use]
pub fn resolve_directory(
    path_str: Option<&str>,
    naming_options: &NamingOptions,
    parse_name: bool,
    library_root: Option<&str>,
) -> Option<VideoFileInfo> {
    resolve(path_str, true, naming_options, parse_name, library_root)
}

/// Resolves a file into a [`VideoFileInfo`].
#[must_use]
pub fn resolve_file(
    path_str: Option<&str>,
    naming_options: &NamingOptions,
    library_root: Option<&str>,
) -> Option<VideoFileInfo> {
    resolve(path_str, false, naming_options, true, library_root)
}

/// Resolves the specified path into a [`VideoFileInfo`].
#[must_use]
pub fn resolve(
    path_str: Option<&str>,
    is_directory: bool,
    naming_options: &NamingOptions,
    parse_name: bool,
    library_root: Option<&str>,
) -> Option<VideoFileInfo> {
    let path_str = path_str?;
    if path_str.is_empty() {
        return None;
    }

    let mut is_stub = false;
    let mut container: Option<String> = None;
    let mut stub_type: Option<String> = None;

    if !is_directory {
        let extension = path::extension(path_str);

        if naming_options
            .video_file_extensions
            .iter()
            .any(|e| e.eq_ignore_ascii_case(extension))
        {
            container = Some(extension.trim_start_matches('.').to_string());
        } else {
            // Not supported. Check stub extensions.
            let stub = stub_resolver::try_resolve_file(path_str, naming_options);
            if !stub.is_stub {
                return None;
            }
            stub_type = stub.stub_type;
            is_stub = true;
            container = Some(extension.trim_start_matches('.').to_string());
        }
    }

    let format_3d_result = format_3d_parser::parse(path_str, naming_options);
    let extra_result: ExtraResult =
        extra_rule_resolver::get_extra_info(path_str, naming_options, library_root);

    let mut name = path::file_name_without_extension(path_str).to_string();
    let mut year: Option<i32> = None;

    if parse_name {
        let clean = clean_date_time(&name, naming_options);
        name = clean.name;
        year = clean.year;

        if let Some(new_name) = try_clean_string(Some(&name), naming_options) {
            name = new_name;
        }
    }

    let container = container.filter(|c| !c.is_empty());

    Some(VideoFileInfo {
        path: path_str.to_string(),
        container,
        name,
        year,
        extra_type: extra_result.extra_type,
        extra_rule: extra_result.rule,
        format_3d: format_3d_result.format_3d,
        is_3d: format_3d_result.is_3d,
        is_stub,
        stub_type,
        is_directory,
    })
}

/// Determines if the path is a video file based on its extension.
#[must_use]
pub fn is_video_file(path_str: &str, naming_options: &NamingOptions) -> bool {
    let extension = path::extension(path_str);
    naming_options
        .video_file_extensions
        .iter()
        .any(|e| e.eq_ignore_ascii_case(extension))
}

/// Determines if the path is a video-file stub based on its extension.
#[must_use]
pub fn is_stub_file(path_str: &str, naming_options: &NamingOptions) -> bool {
    let extension = path::extension(path_str);
    naming_options
        .stub_file_extensions
        .iter()
        .any(|e| e.eq_ignore_ascii_case(extension))
}

/// Tries to clean the name of clutter.
#[must_use]
pub fn try_clean_string(name: Option<&str>, naming_options: &NamingOptions) -> Option<String> {
    clean_string_parser::try_clean(name, &naming_options.clean_string_regexes)
}

/// Tries to extract a name and year from a raw name.
#[must_use]
pub fn clean_date_time(name: &str, naming_options: &NamingOptions) -> CleanDateTimeResult {
    clean_date_time_parser::clean(name, &naming_options.clean_date_time_regexes)
}
