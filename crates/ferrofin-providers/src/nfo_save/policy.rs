//! Saver eligibility and the per-kind sidecar destination.
use std::path::{Path, PathBuf};

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::{LibraryOptions, MetadataOptions};
use ferrofin_traits::providers::ItemUpdateType;

use crate::xbmc::item::NfoItemKind;

pub(super) fn kind(row: &BaseItemEntity) -> Option<NfoItemKind> {
    Some(match row.type_.rsplit('.').next()? {
        "Movie" => NfoItemKind::Movie,
        "Video" | "Trailer" => NfoItemKind::Video,
        "MusicVideo" => NfoItemKind::MusicVideo,
        "Episode" => NfoItemKind::Episode,
        "Series" => NfoItemKind::Series,
        "Season" => NfoItemKind::Season,
        "MusicAlbum" => NfoItemKind::MusicAlbum,
        "MusicArtist" => NfoItemKind::MusicArtist,
        _ => return None,
    })
}

pub(super) fn save_path(row: &BaseItemEntity) -> Option<PathBuf> {
    let kind = kind(row)?;
    let path = Path::new(row.path.as_deref().filter(|path| !path.is_empty())?);
    // SupportsLocalMetadata: filesystem items only; video
    // extras do not have a local metadata identity to save.
    if !path.is_absolute() || (kind.is_video() && row.extra_type.is_some()) {
        return None;
    }
    let data: serde_json::Value = row
        .data
        .as_deref()
        .and_then(|data| serde_json::from_str(data).ok())
        .unwrap_or_default();
    if row.channel_id.as_deref().is_some_and(|channel| {
        !channel.is_empty() && uuid::Uuid::parse_str(channel).map_or(true, |id| !id.is_nil())
    }) {
        return None;
    }
    match kind {
        NfoItemKind::Series => Some(path.join("tvshow.nfo")),
        NfoItemKind::Season => Some(path.join("season.nfo")),
        NfoItemKind::MusicAlbum => Some(path.join("album.nfo")),
        NfoItemKind::MusicArtist => Some(path.join("artist.nfo")),
        NfoItemKind::Episode => Some(path.with_extension("nfo")),
        _ => {
            let video_type = data["VideoType"].as_str().unwrap_or("VideoFile");
            let disc = matches!(video_type, "Dvd" | "BluRay")
                && data["IsPlaceHolder"].as_bool() != Some(true);
            let folder = if row.is_folder || disc {
                path
            } else {
                path.parent()?
            };
            if disc && video_type == "Dvd" {
                Some(folder.join("VIDEO_TS/VIDEO_TS.nfo"))
            } else if kind == NfoItemKind::Movie && !row.is_in_mixed_folder {
                Some(folder.join("movie.nfo"))
            } else if disc {
                Some(folder.join(format!("{}.nfo", path.file_name()?.to_string_lossy())))
            } else {
                Some(path.with_extension("nfo"))
            }
        }
    }
}

pub(super) fn enabled(
    library: Option<&LibraryOptions>,
    global: Option<&MetadataOptions>,
    kind: NfoItemKind,
    update: ItemUpdateType,
    save_images: bool,
    path: &Path,
) -> bool {
    let existing_season = kind == NfoItemKind::Season && path.is_file();
    if update == ItemUpdateType::None
        || (update == ItemUpdateType::MetadataImport && !existing_season)
        || (update == ItemUpdateType::ImageUpdate && !save_images && !existing_season)
    {
        return false;
    }
    if let Some(savers) = library.and_then(|library| library.metadata_savers.as_ref()) {
        return savers.iter().any(|saver| saver.eq_ignore_ascii_case("Nfo"));
    }
    if global.is_some_and(|global| {
        global
            .disabled_metadata_savers
            .iter()
            .any(|saver| saver.eq_ignore_ascii_case("Nfo"))
    }) {
        return false;
    }
    library.is_some_and(|library| library.save_local_metadata)
        || (update == ItemUpdateType::MetadataEdit && path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(
        "Movie",
        "/media/Movie/video.mkv",
        false,
        None,
        "/media/Movie/movie.nfo"
    )]
    #[case("Movie", "/media/video.mkv", true, None, "/media/video.nfo")]
    #[case("Video", "/media/video.mkv", false, None, "/media/video.nfo")]
    #[case("Trailer", "/media/video.mkv", false, None, "/media/video.nfo")]
    #[case("MusicVideo", "/media/video.mkv", false, None, "/media/video.nfo")]
    #[case(
        "Episode",
        "/media/Series/S01E01.mkv",
        false,
        None,
        "/media/Series/S01E01.nfo"
    )]
    #[case("Series", "/media/Series", false, None, "/media/Series/tvshow.nfo")]
    #[case(
        "Season",
        "/media/Series/Season 1",
        false,
        None,
        "/media/Series/Season 1/season.nfo"
    )]
    #[case("MusicAlbum", "/media/Album", false, None, "/media/Album/album.nfo")]
    #[case(
        "MusicArtist",
        "/media/Artist",
        false,
        None,
        "/media/Artist/artist.nfo"
    )]
    #[case(
        "Movie",
        "/media/Movie",
        false,
        Some(r#"{"VideoType":"Dvd"}"#),
        "/media/Movie/VIDEO_TS/VIDEO_TS.nfo"
    )]
    #[case(
        "Movie",
        "/media/Movie.2020",
        true,
        Some(r#"{"VideoType":"BluRay"}"#),
        "/media/Movie.2020/Movie.2020.nfo"
    )]
    fn destinations(
        #[case] kind: &str,
        #[case] path: &str,
        #[case] mixed: bool,
        #[case] data: Option<&str>,
        #[case] expected: &str,
    ) {
        let row = BaseItemEntity {
            type_: format!("MediaBrowser.Controller.Entities.{kind}"),
            path: Some(path.into()),
            is_in_mixed_folder: mixed,
            data: data.map(str::to_owned),
            ..Default::default()
        };
        assert_eq!(save_path(&row).as_deref(), Some(Path::new(expected)));
    }

    #[test]
    fn unsupported_nonfilesystem_and_channel_items_have_no_saver() {
        for (kind, path, data, extra) in [
            ("BoxSet", "/media/set", None, None),
            ("Book", "/media/book.epub", None, None),
            ("Movie", "https://example/movie.mkv", None, None),
            ("Movie", "", None, None),
            (
                "Movie",
                "/media/Movie.mkv",
                Some(r#"{"SourceType":"Channel"}"#),
                None,
            ),
            ("Movie", "/media/Movie.mkv", None, Some(1)),
        ] {
            let row = BaseItemEntity {
                type_: kind.into(),
                path: Some(path.into()),
                data: data.map(str::to_owned),
                channel_id: data.map(|_| uuid::Uuid::from_u128(1).to_string()),
                extra_type: extra,
                ..Default::default()
            };
            assert_eq!(save_path(&row), None);
        }
    }

    #[test]
    fn explicit_selection_overrides_legacy_and_global_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("movie.nfo");
        let mut library = LibraryOptions {
            save_local_metadata: true,
            metadata_savers: Some(vec![]),
            ..Default::default()
        };
        let global = MetadataOptions {
            disabled_metadata_savers: vec!["NFO".into()],
            ..Default::default()
        };
        assert!(!enabled(
            Some(&library),
            None,
            NfoItemKind::Movie,
            ItemUpdateType::MetadataEdit,
            true,
            &path
        ));
        library.metadata_savers = Some(vec!["nFo".into()]);
        assert!(enabled(
            Some(&library),
            Some(&global),
            NfoItemKind::Movie,
            ItemUpdateType::MetadataDownload,
            true,
            &path
        ));
        library.metadata_savers = None;
        assert!(!enabled(
            Some(&library),
            Some(&global),
            NfoItemKind::Movie,
            ItemUpdateType::MetadataEdit,
            true,
            &path
        ));
        library.save_local_metadata = false;
        assert!(!enabled(
            Some(&library),
            None,
            NfoItemKind::Movie,
            ItemUpdateType::MetadataEdit,
            true,
            &path
        ));
        std::fs::write(&path, "<movie />").unwrap();
        assert!(enabled(
            Some(&library),
            None,
            NfoItemKind::Movie,
            ItemUpdateType::MetadataEdit,
            true,
            &path
        ));
        assert!(!enabled(
            Some(&library),
            None,
            NfoItemKind::Movie,
            ItemUpdateType::MetadataDownload,
            true,
            &path
        ));
        library.save_local_metadata = true;
        assert!(enabled(
            Some(&library),
            None,
            NfoItemKind::Movie,
            ItemUpdateType::MetadataDownload,
            true,
            &path
        ));
    }

    #[test]
    fn update_threshold_honors_image_setting_and_existing_season_exception() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("season.nfo");
        let library = LibraryOptions {
            metadata_savers: Some(vec!["Nfo".into()]),
            ..Default::default()
        };
        for images in [false, true] {
            assert!(!enabled(
                Some(&library),
                None,
                NfoItemKind::Movie,
                ItemUpdateType::None,
                images,
                &path
            ));
            assert!(!enabled(
                Some(&library),
                None,
                NfoItemKind::Season,
                ItemUpdateType::MetadataImport,
                images,
                &path
            ));
            assert_eq!(
                enabled(
                    Some(&library),
                    None,
                    NfoItemKind::Movie,
                    ItemUpdateType::ImageUpdate,
                    images,
                    &path
                ),
                images
            );
        }
        std::fs::write(&path, "<season />").unwrap();
        assert!(enabled(
            Some(&library),
            None,
            NfoItemKind::Season,
            ItemUpdateType::MetadataImport,
            false,
            &path
        ));
        assert!(enabled(
            Some(&library),
            None,
            NfoItemKind::Season,
            ItemUpdateType::ImageUpdate,
            false,
            &path
        ));
        assert!(!enabled(
            Some(&library),
            None,
            NfoItemKind::Movie,
            ItemUpdateType::MetadataImport,
            true,
            &path
        ));
    }
}
