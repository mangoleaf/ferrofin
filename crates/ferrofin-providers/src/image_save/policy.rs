//! Media-adjacent destinations from the pinned ImageSaver naming conventions.
use std::path::{Path, PathBuf};

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::ImageSavingConvention;
use ferrofin_model::entities::ImageType;
use ferrofin_traits::options::ItemImageInfo;

pub(super) fn short_kind(item: &BaseItemEntity) -> &str {
    item.type_.rsplit('.').next().unwrap_or_default()
}

fn filesystem(item: &BaseItemEntity) -> bool {
    item.path
        .as_deref()
        .is_some_and(|path| Path::new(path).is_absolute())
        && !item.channel_id.as_deref().is_some_and(|channel| {
            !channel.is_empty() && uuid::Uuid::parse_str(channel).map_or(true, |id| !id.is_nil())
        })
}

pub(super) fn eligible(
    item: &BaseItemEntity,
    series: Option<&BaseItemEntity>,
    kind: ImageType,
) -> bool {
    if item.extra_type.is_some()
        || matches!(short_kind(item), "Audio" | "Photo")
        || (short_kind(item) == "Episode" && kind != ImageType::Primary)
    {
        return false;
    }
    filesystem(item) || (short_kind(item) == "Season" && series.is_some_and(filesystem))
}

fn containing_folder(item: &BaseItemEntity) -> Option<&Path> {
    let path = Path::new(item.path.as_deref()?);
    let data: serde_json::Value = item
        .data
        .as_deref()
        .and_then(|data| serde_json::from_str(data).ok())
        .unwrap_or_default();
    let disc = matches!(data["VideoType"].as_str(), Some("Dvd" | "BluRay"))
        && data["IsPlaceHolder"].as_bool() != Some(true);
    if item.is_folder || disc {
        Some(path)
    } else {
        path.parent()
    }
}

fn season_filename(item: &BaseItemEntity, name: &str, extension: &str) -> Option<String> {
    let season = item.index_number?;
    let marker = if season == 0 {
        "-specials".to_owned()
    } else {
        format!(
            "{}{:02}",
            if season < 0 { "-" } else { "" },
            season.unsigned_abs()
        )
    };
    Some(format!("season{marker}-{name}{extension}"))
}

fn mixed_path(
    item: &BaseItemEntity,
    kind: ImageType,
    name: &str,
    extension: &str,
) -> Option<PathBuf> {
    let path = Path::new(item.path.as_deref()?);
    let name = if kind == ImageType::Primary {
        "poster"
    } else {
        name
    };
    Some(
        path.parent()?
            .join(format!("{}-{name}{extension}", path.file_stem()?.to_str()?)),
    )
}

fn backdrop_name(images: &[ItemImageInfo], zero: &str, prefix: &str, index: usize) -> String {
    if index == 0 {
        return zero.to_owned();
    }
    let mut number = 1_u64;
    loop {
        let name = format!("{prefix}{number}");
        if !images.iter().any(|image| {
            image.image_type == ImageType::Backdrop
                && Path::new(&image.path)
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .is_some_and(|stem| stem.eq_ignore_ascii_case(&name))
        }) {
            return name;
        }
        number += 1;
    }
}

pub(super) struct DestinationPolicy<'a> {
    pub item: &'a BaseItemEntity,
    pub series: Option<&'a BaseItemEntity>,
    pub convention: ImageSavingConvention,
    pub extra_thumbs: bool,
    pub images: &'a [ItemImageInfo],
}

impl DestinationPolicy<'_> {
    // Keep the pinned per-kind naming table together for comparison with ImageSaver.
    #[allow(clippy::too_many_lines)]
    pub fn destinations(&self, kind: ImageType, extension: &str, index: usize) -> Vec<PathBuf> {
        let Self {
            item,
            series,
            convention,
            extra_thumbs,
            images,
        } = *self;
        let Some(folder) = containing_folder(item).or_else(|| series.and_then(containing_folder))
        else {
            return Vec::new();
        };
        let series_folder = series
            .and_then(|series| series.path.as_deref())
            .map(Path::new);
        let season = short_kind(item) == "Season";
        let compatible = convention != ImageSavingConvention::Legacy;
        let normalized = extension.to_ascii_lowercase();
        let extension = if !compatible && extension.eq_ignore_ascii_case(".jpeg") {
            ".jpg"
        } else if !compatible {
            &normalized
        } else {
            extension
        };
        if compatible && kind == ImageType::Backdrop {
            if item.is_in_mixed_folder {
                return mixed_path(
                    item,
                    kind,
                    &if index == 0 {
                        "fanart".into()
                    } else {
                        format!("fanart{index}")
                    },
                    extension,
                )
                .into_iter()
                .collect();
            }
            if index == 0 {
                if season
                    && let (Some(series), Some(name)) =
                        (series_folder, season_filename(item, "fanart", extension))
                {
                    return vec![series.join(name)];
                }
                return vec![folder.join(format!("fanart{extension}"))];
            }
            let name = backdrop_name(images, "fanart", "fanart", index);
            let mut paths = vec![
                folder
                    .join("extrafanart")
                    .join(format!("{name}{extension}")),
            ];
            if extra_thumbs {
                paths.push(
                    folder
                        .join("extrathumbs")
                        .join(format!("thumb{index}{extension}")),
                );
            }
            return paths;
        }
        let season_name = match kind {
            ImageType::Primary => Some("poster"),
            ImageType::Thumb => Some("landscape"),
            ImageType::Banner => Some("banner"),
            ImageType::Logo => Some("logo"),
            ImageType::Backdrop if index == 0 => Some("fanart"),
            _ => None,
        };
        if season
            && let (Some(series), Some(name)) = (
                series_folder,
                season_name.and_then(|name| season_filename(item, name, extension)),
            )
        {
            return vec![series.join(name)];
        }
        if kind == ImageType::Primary && short_kind(item) == "Episode" {
            let Some(path) = item.path.as_deref().map(Path::new) else {
                return Vec::new();
            };
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                return Vec::new();
            };
            return vec![folder.join(format!("{stem}-thumb{extension}"))];
        }
        if compatible
            && kind == ImageType::Primary
            && (item.is_in_mixed_folder || short_kind(item) == "MusicVideo")
        {
            return mixed_path(item, kind, "", extension).into_iter().collect();
        }
        let name = match kind {
            ImageType::Primary => {
                if !compatible || matches!(short_kind(item), "MusicAlbum" | "MusicArtist") {
                    "folder".into()
                } else {
                    "poster".into()
                }
            }
            ImageType::Backdrop => backdrop_name(images, "backdrop", "backdrop", index),
            ImageType::Thumb => "landscape".into(),
            ImageType::Art => "clearart".into(),
            ImageType::BoxRear => "back".into(),
            ImageType::Disc => {
                if short_kind(item) == "MusicAlbum" {
                    "cdart".into()
                } else {
                    "disc".into()
                }
            }
            _ => format!("{kind:?}").to_ascii_lowercase(),
        };
        if item.is_in_mixed_folder {
            mixed_path(item, kind, &name, extension)
                .into_iter()
                .collect()
        } else {
            vec![folder.join(format!("{name}{extension}"))]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(kind: &str, path: &str, folder: bool) -> BaseItemEntity {
        BaseItemEntity {
            type_: format!("MediaBrowser.Controller.Entities.{kind}"),
            path: Some(path.into()),
            is_folder: folder,
            ..Default::default()
        }
    }
    fn image(path: &str) -> ItemImageInfo {
        ItemImageInfo {
            path: path.into(),
            image_type: ImageType::Backdrop,
            date_modified: chrono::Utc::now(),
            width: 1,
            height: 1,
            blur_hash: None,
        }
    }
    fn destination(
        item: &BaseItemEntity,
        convention: ImageSavingConvention,
        kind: ImageType,
    ) -> Vec<PathBuf> {
        DestinationPolicy {
            item,
            series: None,
            convention,
            extra_thumbs: false,
            images: &[],
        }
        .destinations(kind, ".png", 0)
    }
    #[test]
    fn movie_and_music_names_follow_both_conventions() {
        for (kind, folder, compatible) in [
            ("Movie", false, "poster"),
            ("MusicVideo", false, "Film-poster"),
            ("MusicAlbum", true, "folder"),
            ("MusicArtist", true, "folder"),
            ("PhotoAlbum", true, "poster"),
            ("Person", true, "poster"),
        ] {
            let item = row(
                kind,
                if folder {
                    "/media/Album"
                } else {
                    "/media/Album/Film.mkv"
                },
                folder,
            );
            assert_eq!(
                destination(&item, ImageSavingConvention::Legacy, ImageType::Primary),
                vec![PathBuf::from("/media/Album/folder.png")]
            );
            assert_eq!(
                destination(&item, ImageSavingConvention::Compatible, ImageType::Primary),
                vec![PathBuf::from(format!("/media/Album/{compatible}.png"))]
            );
        }
        for (kind, name) in [
            (ImageType::Thumb, "landscape"),
            (ImageType::Art, "clearart"),
            (ImageType::BoxRear, "back"),
            (ImageType::Disc, "disc"),
            (ImageType::Logo, "logo"),
            (ImageType::Banner, "banner"),
        ] {
            assert_eq!(
                destination(
                    &row("Movie", "/media/Film.mkv", false),
                    ImageSavingConvention::Legacy,
                    kind
                ),
                vec![PathBuf::from(format!("/media/{name}.png"))]
            );
        }
        assert_eq!(
            destination(
                &row("MusicAlbum", "/music/Album", true),
                ImageSavingConvention::Compatible,
                ImageType::Disc
            ),
            vec![PathBuf::from("/music/Album/cdart.png")]
        );
    }
    #[test]
    fn episodes_mixed_folders_and_discs_use_the_media_path() {
        let mut movie = row("Movie", "/media/Film.mkv", false);
        movie.is_in_mixed_folder = true;
        for convention in [
            ImageSavingConvention::Legacy,
            ImageSavingConvention::Compatible,
        ] {
            assert_eq!(
                destination(&movie, convention, ImageType::Primary),
                vec![PathBuf::from("/media/Film-poster.png")]
            );
            assert_eq!(
                destination(
                    &row("Episode", "/tv/Season/Show.S01E01.mkv", false),
                    convention,
                    ImageType::Primary
                ),
                vec![PathBuf::from("/tv/Season/Show.S01E01-thumb.png")]
            );
        }
        let policy = DestinationPolicy {
            item: &movie,
            series: None,
            convention: ImageSavingConvention::Compatible,
            extra_thumbs: true,
            images: &[],
        };
        assert_eq!(
            policy.destinations(ImageType::Backdrop, ".png", 2),
            vec![PathBuf::from("/media/Film-fanart2.png")]
        );
        movie.is_in_mixed_folder = false;
        movie.path = Some("/media/DVD".into());
        movie.data = Some(r#"{"VideoType":"Dvd"}"#.into());
        assert_eq!(
            destination(&movie, ImageSavingConvention::Legacy, ImageType::Primary),
            vec![PathBuf::from("/media/DVD/folder.png")]
        );
        movie.data = Some(r#"{"VideoType":"Dvd","IsPlaceHolder":true}"#.into());
        assert_eq!(
            destination(&movie, ImageSavingConvention::Legacy, ImageType::Primary),
            vec![PathBuf::from("/media/folder.png")]
        );
    }
    #[test]
    fn physical_and_virtual_seasons_publish_series_art_and_specials() {
        let series = row("Series", "/tv/Show", true);
        for path in [Some("/tv/Show/Season 01".into()), None] {
            let mut season = row("Season", "", true);
            season.path = path;
            for index in [-1_i64, 0, 1, 12] {
                season.index_number = Some(index);
                for (kind, name) in [
                    (ImageType::Primary, "poster"),
                    (ImageType::Backdrop, "fanart"),
                    (ImageType::Thumb, "landscape"),
                    (ImageType::Banner, "banner"),
                    (ImageType::Logo, "logo"),
                ] {
                    let policy = DestinationPolicy {
                        item: &season,
                        series: Some(&series),
                        convention: ImageSavingConvention::Legacy,
                        extra_thumbs: false,
                        images: &[],
                    };
                    let marker = if index == 0 {
                        "-specials".into()
                    } else {
                        format!(
                            "{}{:02}",
                            if index < 0 { "-" } else { "" },
                            index.unsigned_abs()
                        )
                    };
                    assert_eq!(
                        policy.destinations(kind, ".jpeg", 0),
                        vec![PathBuf::from(format!("/tv/Show/season{marker}-{name}.jpg"))]
                    );
                }
            }
        }
    }
    #[test]
    fn backdrops_choose_unused_names_and_optional_duplicate_outputs() {
        let item = row("Movie", "/media/Film/Film.mkv", false);
        let images = [
            image("/media/Film/backdrop1.jpg"),
            image("/media/Film/BACKDROP2.png"),
            image("/media/Film/extrafanart/fanart1.png"),
        ];
        for (convention, index, expected) in [
            (ImageSavingConvention::Legacy, 0, "backdrop.png"),
            (ImageSavingConvention::Legacy, 1, "backdrop3.png"),
            (ImageSavingConvention::Compatible, 0, "fanart.png"),
            (
                ImageSavingConvention::Compatible,
                1,
                "extrafanart/fanart2.png",
            ),
        ] {
            let policy = DestinationPolicy {
                item: &item,
                series: None,
                convention,
                extra_thumbs: false,
                images: &images,
            };
            assert_eq!(
                policy.destinations(ImageType::Backdrop, ".png", index),
                vec![PathBuf::from("/media/Film").join(expected)]
            );
        }
        let policy = DestinationPolicy {
            item: &item,
            series: None,
            convention: ImageSavingConvention::Compatible,
            extra_thumbs: true,
            images: &images,
        };
        assert_eq!(
            policy.destinations(ImageType::Backdrop, ".png", 3),
            vec![
                PathBuf::from("/media/Film/extrafanart/fanart2.png"),
                PathBuf::from("/media/Film/extrathumbs/thumb3.png")
            ]
        );
    }
    #[test]
    fn eligibility_excludes_tracks_photos_extras_remote_items_and_episode_backdrops() {
        for kind in ["Audio", "Photo"] {
            assert!(!eligible(
                &row(kind, "/media/item", false),
                None,
                ImageType::Primary
            ));
        }
        let mut item = row("Movie", "/media/Film.mkv", false);
        assert!(eligible(&item, None, ImageType::Primary));
        item.extra_type = Some(1);
        assert!(!eligible(&item, None, ImageType::Primary));
        item.extra_type = None;
        item.channel_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(!eligible(&item, None, ImageType::Primary));
        item.channel_id = Some(uuid::Uuid::nil().to_string());
        assert!(eligible(&item, None, ImageType::Primary));
        item.path = Some("https://remote/Film.mkv".into());
        assert!(!eligible(&item, None, ImageType::Primary));
        assert!(!eligible(
            &row("Episode", "/tv/episode.mkv", false),
            None,
            ImageType::Backdrop
        ));
        let mut season = row("Season", "", true);
        season.path = None;
        assert!(eligible(
            &season,
            Some(&row("Series", "/tv/Show", true)),
            ImageType::Primary
        ));
        assert!(!eligible(&season, None, ImageType::Primary));
    }
}
