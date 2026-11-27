//! Readers of `BaseItems.Data`, the JSON payload Jellyfin serializes each item
//! into (PascalCase, enums by name): the parse, one field at a time, and a
//! video's stored format. They read rows and nothing else, so the crates that
//! sit beside `ferrofin-core` (the compiled-in extensions) use them too.
//! `ferrofin-core`'s `item_data` re-exports them beside the writers.

use serde_json::{Map, Value};

use crate::entities::base_items::BaseItemEntity;

/// Parses a `Data` column value into its JSON object, tolerating `NULL`,
/// empty, and malformed payloads (each yields an empty object).
#[must_use]
pub fn parse_data(data: Option<&str>) -> Map<String, Value> {
    data.and_then(|d| serde_json::from_str::<Value>(d).ok())
        .and_then(|v| match v {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// Reads one `Data` field as a string.
#[must_use]
pub fn read_data_string(data: &Map<String, Value>, key: &str) -> Option<String> {
    data.get(key)?.as_str().map(str::to_owned)
}

/// Reads one `Data` field as an `f64` (accepting the JSON number form).
#[must_use]
pub fn read_data_f64(data: &Map<String, Value>, key: &str) -> Option<f64> {
    data.get(key)?.as_f64()
}

/// Reads one `Data` field as an `i32`.
#[must_use]
pub fn read_data_i32(data: &Map<String, Value>, key: &str) -> Option<i32> {
    i32::try_from(data.get(key)?.as_i64()?).ok()
}

/// Whether a video's `Data` makes its path a disc rip's folder: a DVD or
/// Blu-ray `VideoType` on a video that is no placeholder (`.disc` stub) —
/// the case `Video.ContainingFolderPath` (`Video.cs:201-219`, unless the rip
/// is stacked) and `MovieNfoSaver.GetMovieSavePaths` treat the path itself as
/// the folder.
/// The text is read first, so a blob naming neither type is not parsed.
#[must_use]
pub fn is_disc_folder(data: Option<&str>) -> bool {
    let Some(text) = data.filter(|t| t.contains("\"Dvd\"") || t.contains("\"BluRay\"")) else {
        return false;
    };
    let data = parse_data(Some(text));
    matches!(
        read_data_string(&data, "VideoType").as_deref(),
        Some("Dvd" | "BluRay")
    ) && !data
        .get("IsPlaceHolder")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The folder `video` stored at `path` lives in — `BaseItem.ContainingFolderPath`
/// (`BaseItem.cs:292-303`) with `Video`'s override (`Video.cs:201-219`): a
/// folder's own path; for a video, the parent directory of a file — of the
/// first part for a stacked one (`IsStacked`, checked first), so a multi-disc
/// set's is the set's folder — and the path itself for a disc rip that is no
/// placeholder ([`is_disc_folder`]). `path` is the row's path, or it with the
/// `%AppDataPath%`-style prefixes expanded. A path with no parent (`/`) has
/// none: .NET's `GetDirectoryName` is `null` there, so the result is empty.
#[must_use]
pub fn containing_folder_path_at(video: &BaseItemEntity, path: &str) -> String {
    if video.is_folder {
        return path.to_owned();
    }
    // Only a disc rip (a cheap text test first) can name its own folder;
    // only then is the blob read for a stack.
    if is_disc_folder(video.data.as_deref())
        && !video.data.as_deref().is_some_and(|text| {
            text.contains("AdditionalParts")
                && parse_data(Some(text))
                    .get("AdditionalParts")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| !parts.is_empty())
        })
    {
        return path.to_owned();
    }
    std::path::Path::new(path)
        .parent()
        .map_or_default(|p| p.to_string_lossy().into_owned())
}

/// A video's format as its `Data` stores it: `Video.VideoType` (the
/// `VideoFile` default when the blob names none), `IsoType` and
/// `Video3DFormat`, each serialized by name (`BaseItemMapper` keeps them in
/// the blob, not in a column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFormat {
    /// `Video.VideoType`.
    pub video_type: ferrofin_model::entities::VideoType,
    /// `Video.IsoType`, for a disc image.
    pub iso_type: Option<ferrofin_model::entities::IsoType>,
    /// `Video.Video3DFormat`, for a 3D video.
    pub video3d_format: Option<ferrofin_model::entities::Video3DFormat>,
}

/// The [`VideoFormat`] `data` stores. The text is read first: a blob naming
/// no disc, image or 3D format — nearly every video's — is not parsed.
#[must_use]
pub fn video_format(data: Option<&str>) -> VideoFormat {
    let plain = VideoFormat {
        video_type: ferrofin_model::entities::VideoType::VideoFile,
        iso_type: None,
        video3d_format: None,
    };
    let Some(text) = data.filter(|t| {
        ["Video3DFormat", "\"BluRay\"", "\"Dvd\"", "\"Iso\""]
            .iter()
            .any(|needle| t.contains(needle))
    }) else {
        return plain;
    };
    let data = parse_data(Some(text));
    let field = |key: &str| data.get(key).cloned().filter(|v| !v.is_null());
    VideoFormat {
        video_type: field("VideoType")
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or(plain.video_type),
        iso_type: field("IsoType").and_then(|v| serde_json::from_value(v).ok()),
        video3d_format: field("Video3DFormat").and_then(|v| serde_json::from_value(v).ok()),
    }
}

/// The index of the item a merge makes primary when none of `items` already
/// has versions: 3D and disc (not `VideoType::VideoFile`) videos after plain
/// files, then the widest, the first of equals — the fallback of
/// `VideosController.MergeVersions` (`VideosController.cs:204-217`) and of
/// the MergeVersions plugin. The item's `Width` column stands in for its
/// default video stream's width. `0` for an empty slice.
#[must_use]
pub fn preferred_primary_version(items: &[BaseItemEntity]) -> usize {
    items
        .iter()
        .enumerate()
        .min_by_key(|(_, item)| {
            let format = video_format(item.data.as_deref());
            let demoted = format.video3d_format.is_some()
                || format.video_type != ferrofin_model::entities::VideoType::VideoFile;
            (demoted, std::cmp::Reverse(item.width.unwrap_or(0)))
        })
        .map_or(0, |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use ferrofin_model::entities::{IsoType, Video3DFormat, VideoType};
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(None)]
    #[case(Some(""))]
    #[case(Some("not json"))]
    #[case(Some("[1, 2]"))]
    fn a_missing_or_malformed_payload_parses_as_empty(#[case] data: Option<&str>) {
        assert!(parse_data(data).is_empty());
    }

    #[test]
    fn fields_read_by_their_json_type() {
        let data = parse_data(Some(
            r#"{"Name":"x","Ratio":1.5,"End":7,"Big":4294967296,"Text":"7"}"#,
        ));
        assert_eq!(read_data_string(&data, "Name").as_deref(), Some("x"));
        assert_eq!(read_data_f64(&data, "Ratio"), Some(1.5));
        assert_eq!(read_data_i32(&data, "End"), Some(7));
        assert_eq!(read_data_i32(&data, "Big"), None, "out of i32 range");
        assert_eq!(read_data_i32(&data, "Text"), None, "a string is no number");
        assert_eq!(read_data_string(&data, "Missing"), None);
    }

    #[rstest]
    #[case(None, VideoType::VideoFile, None, None)]
    #[case(Some(r#"{"Name":"plain"}"#), VideoType::VideoFile, None, None)]
    #[case(Some(r#"{"VideoType":"BluRay"}"#), VideoType::BluRay, None, None)]
    #[case(
        Some(r#"{"VideoType":"Iso","IsoType":"Dvd"}"#),
        VideoType::Iso,
        Some(IsoType::Dvd),
        None
    )]
    #[case(
        Some(r#"{"Video3DFormat":"HalfSideBySide","VideoType":null}"#),
        VideoType::VideoFile,
        None,
        Some(Video3DFormat::HalfSideBySide)
    )]
    fn the_video_format_reads_the_stored_names(
        #[case] data: Option<&str>,
        #[case] video_type: VideoType,
        #[case] iso_type: Option<IsoType>,
        #[case] video3d_format: Option<Video3DFormat>,
    ) {
        assert_eq!(
            video_format(data),
            VideoFormat {
                video_type,
                iso_type,
                video3d_format,
            }
        );
    }

    fn video(width: i64, data: Option<&str>) -> BaseItemEntity {
        BaseItemEntity {
            width: Some(width),
            data: data.map(str::to_owned),
            ..BaseItemEntity::default()
        }
    }

    #[test]
    fn the_primary_version_is_the_widest_plain_file() {
        assert_eq!(preferred_primary_version(&[]), 0);
        let items = [
            video(3840, Some(r#"{"VideoType":"BluRay"}"#)),
            video(1280, None),
            video(1920, None),
            video(3840, Some(r#"{"Video3DFormat":"FullTopAndBottom"}"#)),
            video(1920, None),
        ];
        assert_eq!(preferred_primary_version(&items), 2, "the first of equals");
    }
}
