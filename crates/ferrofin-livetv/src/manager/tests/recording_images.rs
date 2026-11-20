//! "Save EPG images": `RecordingsMetadataManager.SaveRecordingImages`, which
//! copies the guide programme's artwork beside a recording as it starts.

use std::path::Path;
use std::sync::Arc;

use chrono::{Duration, Utc};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::ImageType;
use ferrofin_model::live_tv::{BaseTimerInfoDto, LiveTvOptions, TimerInfoDto};
use ferrofin_traits::options::{InternalItemsQuery, ItemImageInfo};
use ferrofin_traits::stubs::LiveTvManager as _;
use uuid::Uuid;

use super::{ArtServices, ChannelItemLab, M3U, dated, item_lab, wait_for};
use crate::dvr::TimerRecordingInfo;
use crate::manager::LiveTvPaths;

/// A film with a Primary, a Thumb and a Backdrop, and a series episode (its
/// sub-title makes it one) with a Primary and a Thumb.
const GUIDE: &str = "<tv>\
    <channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
    <programme start=\"20260725060000 +0000\" stop=\"20260725070000 +0000\" channel=\"one.tv\">\
    <title>Film</title><icon src=\"http://img/film.png\"/>\
    <image type=\"still\">http://img/still.jpg</image><image type=\"backdrop\">http://img/back.jpg</image>\
    </programme>\
    <programme start=\"20260725070000 +0000\" stop=\"20260725080000 +0000\" channel=\"one.tv\">\
    <title>Dr. Show</title><sub-title>Pilot</sub-title><icon src=\"http://img/show.png\"/>\
    <image type=\"still\">http://img/pilot.jpg</image>\
    </programme></tv>";

/// The lab with [`GUIDE`] refreshed, its remote images "downloaded" into
/// `{dir}/downloads` but for the `refused` URLs.
async fn lab(dir: &Path, refused: &[&str]) -> ChannelItemLab {
    let downloads = dir.join("downloads");
    std::fs::create_dir_all(&downloads).expect("downloads");
    let lab = item_lab(M3U, Some(dated(GUIDE))).await;
    let art = Arc::new(ArtServices {
        downloads: Some(downloads),
        refused: refused.iter().map(|url| (*url).to_owned()).collect(),
        ..ArtServices::default()
    });
    lab.mgr.set_images(art.clone(), art);
    lab.mgr.refresh_guide().await.expect("refresh");
    lab
}

/// Starts recording the programme titled `title` into `target`, with the
/// facts a fired timer reads from its guide row.
async fn start(
    lab: &ChannelItemLab,
    title: &str,
    target: &Path,
    series: Option<&Path>,
    options: &LiveTvOptions,
) {
    let id = program_id(lab, title).await;
    let row = lab.mgr.program_row(id).await.expect("read").expect("row");
    let mut timer = TimerInfoDto {
        base: BaseTimerInfoDto {
            id: Some(format!("timer-{title}")),
            name: Some(title.to_owned()),
            start_date: Utc::now(),
            end_date: Utc::now() + Duration::minutes(30),
            ..BaseTimerInfoDto::default()
        },
        ..TimerInfoDto::default()
    };
    let mut info = TimerRecordingInfo::from_timer(&timer);
    crate::manager::apply_program_to_recording_info(&row, &mut info);
    lab.mgr
        .start_capture(
            &mut timer,
            &info,
            target,
            &MediaSourceInfo::default(),
            (series, options),
        )
        .await
        .expect("started");
}

/// The item id of the programme titled `title`.
async fn program_id(lab: &ChannelItemLab, title: &str) -> Uuid {
    lab.items
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::LiveTvProgram],
            ..InternalItemsQuery::default()
        })
        .await
        .expect("programmes")
        .iter()
        .find(|p| p.name.as_deref() == Some(title))
        .and_then(|p| Uuid::parse_str(&p.id).ok())
        .expect("the programme")
}

/// The files in `folder`, by name.
fn names(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .expect("folder")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Waits until `file` exists, holding `body` when one is given: a copy
/// creates its file before it writes it.
async fn saved(file: &Path, body: Option<&str>) {
    let whole = wait_for(|| async {
        let read = std::fs::read_to_string(file).ok()?;
        body.is_none_or(|body| read == body).then_some(())
    })
    .await;
    assert!(whole.is_some(), "{} is saved", file.display());
}

/// Anything but a series episode gets its Primary as `poster`, its Backdrop
/// as `fanart` and its Thumb as `landscape`, each keeping its extension.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recording_gets_its_programmes_poster_fanart_and_landscape() {
    let dir = tempfile::tempdir().expect("directory");
    let lab = lab(dir.path(), &[]).await;
    let folder = dir.path().join("Movies").join("Film");

    let target = folder.join("Film.ts");
    start(&lab, "Film", &target, None, &LiveTvOptions::default()).await;

    for (name, url) in [
        ("poster.png", "http://img/film.png"),
        ("fanart.jpg", "http://img/back.jpg"),
        ("landscape.jpg", "http://img/still.jpg"),
    ] {
        saved(&folder.join(name), Some(url)).await;
    }
    saved(&folder.join("Film.nfo"), None).await;
    assert_eq!(
        names(&folder),
        ["Film.nfo", "fanart.jpg", "landscape.jpg", "poster.png"]
    );
}

/// A series episode gets only its Thumb, named after the recording: the whole
/// name, dots included, with `-thumb` and the image's extension. The images
/// do not need the NFO.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_series_episode_gets_its_thumb_named_after_it() {
    let dir = tempfile::tempdir().expect("directory");
    let lab = lab(dir.path(), &[]).await;
    let series = dir.path().join("Series").join("Dr. Show");
    let season = series.join("Season 1");
    let options = LiveTvOptions {
        save_recording_nfo: false,
        ..LiveTvOptions::default()
    };

    let target = season.join("Dr. Show - Pilot.ts");
    start(&lab, "Dr. Show", &target, Some(&series), &options).await;

    let thumb = season.join("Dr. Show - Pilot-thumb.jpg");
    saved(&thumb, Some("http://img/pilot.jpg")).await;
    assert_eq!(names(&season), ["Dr. Show - Pilot-thumb.jpg"]);
}

/// An image that cannot be made local is skipped, and the next one is still
/// saved: the refused Primary leaves no `poster`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_image_leaves_the_others_saved() {
    let dir = tempfile::tempdir().expect("directory");
    let lab = lab(dir.path(), &["http://img/film.png"]).await;
    let folder = dir.path().join("Movies").join("Film");

    let target = folder.join("Film.ts");
    start(&lab, "Film", &target, None, &LiveTvOptions::default()).await;

    saved(&folder.join("fanart.jpg"), Some("http://img/back.jpg")).await;
    saved(&folder.join("landscape.jpg"), Some("http://img/still.jpg")).await;
    assert_eq!(names(&folder), ["Film.nfo", "fanart.jpg", "landscape.jpg"]);
}

/// A local image row is copied only from the metadata directory, as every
/// other read of a Live TV image row: a guide supplied it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_image_is_copied_only_from_the_metadata_directory() {
    let dir = tempfile::tempdir().expect("directory");
    let mut lab = lab(dir.path(), &[]).await;
    let metadata = dir.path().join("metadata");
    lab.mgr = lab.mgr.clone().with_paths(LiveTvPaths {
        metadata_dir: metadata.clone().into(),
        ..LiveTvPaths::default()
    });
    let inside = metadata.join("library").join("ab").join("backdrop.jpg");
    let outside = dir.path().join("elsewhere").join("primary.png");
    for (file, body) in [(&inside, "inside"), (&outside, "outside")] {
        std::fs::create_dir_all(file.parent().expect("folder")).expect("folder");
        std::fs::write(file, body).expect("image");
    }
    let row = |image_type, path: &Path| ItemImageInfo {
        image_type,
        path: path.display().to_string(),
        ..ItemImageInfo::default()
    };
    let film = program_id(&lab, "Film").await;
    lab.persistence
        .save_item_images(
            film,
            &[
                row(ImageType::Primary, &outside),
                row(ImageType::Backdrop, &inside),
            ],
        )
        .await
        .expect("image rows");
    let folder = dir.path().join("Movies").join("Film");

    start(
        &lab,
        "Film",
        &folder.join("Film.ts"),
        None,
        &LiveTvOptions::default(),
    )
    .await;

    // The Backdrop is copied after the Primary was tried.
    saved(&folder.join("fanart.jpg"), Some("inside")).await;
    assert_eq!(names(&folder), ["Film.nfo", "fanart.jpg"]);
}

/// No image is saved with "Save EPG images" off, nor after the NFO failed (a
/// series recording without its series folder), as upstream's one catch-all
/// skips the rest. Nothing marks either end, so each waits a little.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_image_is_saved_with_the_option_off_or_after_a_failed_nfo() {
    let dir = tempfile::tempdir().expect("directory");
    let lab = lab(dir.path(), &[]).await;
    let film = dir.path().join("Movies").join("Film");
    let options = LiveTvOptions {
        save_recording_images: false,
        ..LiveTvOptions::default()
    };
    start(&lab, "Film", &film.join("Film.ts"), None, &options).await;
    let season = dir.path().join("Series").join("Dr. Show").join("Season 1");
    let episode = season.join("Dr. Show - Pilot.ts");
    start(&lab, "Dr. Show", &episode, None, &LiveTvOptions::default()).await;

    saved(&film.join("Film.nfo"), None).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(names(&film), ["Film.nfo"]);
    assert!(names(&season).is_empty(), "{:?}", names(&season));
}
