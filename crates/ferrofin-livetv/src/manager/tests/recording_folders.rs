//! `RecordingsManager.CreateRecordingFolders` and its triggers, through the
//! manager, over a fake library: what becomes a library, what never does
//! again, and what `MediaLocationsCreated` remembers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ferrofin_model::configuration::LibraryOptions;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::live_tv::LiveTvOptions;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::RecordingLibrary;
use ferrofin_traits::options::DtoOptions;
use ferrofin_traits::stubs::LiveTvManager;
use uuid::Uuid;

use super::{manager_with_dvr, mapping_manager, now_playing, timer_from_defaults, wait_for};
use crate::manager::{FerrofinLiveTvManager, LiveTvPaths};

/// The library, in memory.
#[derive(Default)]
struct FakeLibrary {
    folders: Mutex<Vec<VirtualFolderInfo>>,
    /// `add_virtual_folder` fails while set.
    refuse_adds: AtomicBool,
    /// Calls of `add_virtual_folder`, successful or not.
    adds: AtomicUsize,
    /// Calls of `queue_library_scan`.
    scans: AtomicUsize,
    /// The paths refreshed, in order.
    refreshed: Mutex<Vec<String>>,
    /// When set, `add_virtual_folder` waits here before it adds anything.
    add_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

impl FakeLibrary {
    fn with(folders: Vec<VirtualFolderInfo>) -> Arc<Self> {
        Arc::new(Self {
            folders: Mutex::new(folders),
            ..Self::default()
        })
    }

    /// `(Name, Locations)` of every library.
    fn libraries(&self) -> Vec<(String, Vec<String>)> {
        self.folders
            .lock()
            .unwrap()
            .iter()
            .map(|folder| (folder.name.clone().unwrap(), folder.locations.clone()))
            .collect()
    }

    fn refreshed(&self) -> Vec<String> {
        self.refreshed.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl RecordingLibrary for FakeLibrary {
    async fn virtual_folders(&self) -> Result<Vec<VirtualFolderInfo>, ServiceError> {
        Ok(self.folders.lock().unwrap().clone())
    }

    async fn add_virtual_folder(
        &self,
        name: &str,
        collection_type: Option<CollectionTypeOptions>,
        options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        self.adds.fetch_add(1, Ordering::SeqCst);
        let gate = self.add_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        // Long enough that a second pass running alongside would be seen.
        tokio::task::yield_now().await;
        if self.refuse_adds.load(Ordering::SeqCst) {
            return Err(ServiceError::backend("refused"));
        }
        // `AddVirtualFolder` numbers a taken name.
        let mut folders = self.folders.lock().unwrap();
        let taken = |name: &str| {
            folders
                .iter()
                .any(|folder| folder.name.as_deref() == Some(name))
        };
        let mut stored = name.to_owned();
        let mut count = 1;
        while taken(&stored) {
            count += 1;
            stored = format!("{name}{count}");
        }
        folders.push(VirtualFolderInfo {
            name: Some(stored),
            collection_type,
            locations: options.path_infos.iter().map(|p| p.path.clone()).collect(),
            ..VirtualFolderInfo::default()
        });
        Ok(())
    }

    async fn remove_virtual_folder(&self, name: &str) -> Result<(), ServiceError> {
        self.folders
            .lock()
            .unwrap()
            .retain(|folder| folder.name.as_deref() != Some(name));
        Ok(())
    }

    async fn remove_media_path(&self, name: &str, path: &str) -> Result<(), ServiceError> {
        for folder in self.folders.lock().unwrap().iter_mut() {
            if folder.name.as_deref() == Some(name) {
                folder.locations.retain(|location| location != path);
            }
        }
        Ok(())
    }

    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        self.scans.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn refresh_path(&self, path: &str) -> Result<(), ServiceError> {
        self.refreshed.lock().unwrap().push(path.to_owned());
        Ok(())
    }
}

/// A library at `locations`.
fn library(name: &str, locations: &[&str]) -> VirtualFolderInfo {
    VirtualFolderInfo {
        name: Some(name.to_owned()),
        locations: locations.iter().map(|&path| path.to_owned()).collect(),
        ..VirtualFolderInfo::default()
    }
}

/// A manager under `root` with `library` attached.
async fn manager(root: &Path, library: &Arc<FakeLibrary>) -> FerrofinLiveTvManager {
    let mgr = manager_with_dvr(root).await;
    mgr.set_recording_library(library.clone(), Arc::default());
    mgr
}

fn options_file(root: &Path) -> PathBuf {
    root.join("named").join("livetv.json")
}

fn write_options(root: &Path, options: &LiveTvOptions) {
    let path = options_file(root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec(options).unwrap()).unwrap();
}

fn saved_locations(root: &Path) -> Vec<String> {
    let saved: LiveTvOptions =
        serde_json::from_slice(&std::fs::read(options_file(root)).unwrap()).unwrap();
    saved.media_locations_created
}

/// `{root}/{name}`, made.
fn directory(root: &Path, name: &str) -> String {
    let path = root.join(name);
    std::fs::create_dir_all(&path).unwrap();
    path.display().to_string()
}

#[tokio::test]
async fn each_existing_folder_becomes_a_library_once() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    let default = directory(dir.path(), "dvr");
    let movies = directory(dir.path(), "dvr-movies");
    write_options(
        dir.path(),
        &LiveTvOptions {
            recording_path: Some(default.clone()),
            movie_recording_path: Some(movies.clone()),
            // No directory there: no folder, and no library.
            series_recording_path: Some(dir.path().join("absent").display().to_string()),
            ..LiveTvOptions::default()
        },
    );

    mgr.create_recording_folders().await;

    assert_eq!(
        library.libraries(),
        [
            ("Recordings".to_owned(), vec![default.clone()]),
            ("Recorded Movies".to_owned(), vec![movies.clone()]),
        ]
    );
    assert_eq!(saved_locations(dir.path()), [default, movies]);
}

#[tokio::test]
async fn a_second_pass_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    let default = directory(dir.path(), "dvr");
    write_options(
        dir.path(),
        &LiveTvOptions {
            recording_path: Some(default.clone()),
            ..LiveTvOptions::default()
        },
    );
    mgr.create_recording_folders().await;
    // A hand edit a save would overwrite: only a change is saved.
    let edited = "{\"RecordingPath\":\"".to_owned()
        + &default
        + "\",\"MediaLocationsCreated\":[\""
        + &default
        + "\"],\"TunerHosts\":[],\"ListingProviders\":[]}";
    std::fs::write(options_file(dir.path()), &edited).unwrap();

    mgr.create_recording_folders().await;

    assert_eq!(library.adds.load(Ordering::SeqCst), 1);
    assert_eq!(library.libraries().len(), 1);
    assert_eq!(
        std::fs::read_to_string(options_file(dir.path())).unwrap(),
        edited
    );
}

/// The location is in a library whatever that library is, so a Jellyfin
/// database's "Recordings", or one the administrator made, is reused; the
/// spelling differs by a trailing separator, which `AreEqual` ignores.
#[tokio::test]
async fn a_location_a_library_already_holds_is_left_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let default = directory(dir.path(), "dvr");
    let held = format!("{default}/");
    let library = FakeLibrary::with(vec![library("My Shows", &[&held])]);
    let mgr = manager(dir.path(), &library).await;
    write_options(
        dir.path(),
        &LiveTvOptions {
            recording_path: Some(default),
            ..LiveTvOptions::default()
        },
    );

    mgr.create_recording_folders().await;

    assert_eq!(library.adds.load(Ordering::SeqCst), 0);
    assert_eq!(
        library.libraries(),
        [("My Shows".to_owned(), vec![held.clone()])]
    );
    let value = saved_locations(dir.path());
    assert!(value.is_empty(), "{value:?}");
}

/// A pass over an adopted configuration: the recording paths were denied, so
/// the location Jellyfin made is not a recording folder here, and upstream's
/// rule would delete the library holding the recordings.
#[tokio::test]
async fn an_adopted_location_is_never_removed() {
    let dir = tempfile::tempdir().unwrap();
    let adopted = directory(dir.path(), "jellyfin-recordings");
    let library = FakeLibrary::with(vec![library("Recordings", &[&adopted])]);
    let mgr = manager(dir.path(), &library).await;
    write_options(
        dir.path(),
        &LiveTvOptions {
            media_locations_created: vec![adopted.clone()],
            ..LiveTvOptions::default()
        },
    );

    mgr.create_recording_folders().await;
    mgr.create_recording_folders().await;

    assert_eq!(
        library.libraries(),
        [("Recordings".to_owned(), vec![adopted])]
    );
    assert_eq!(library.scans.load(Ordering::SeqCst), 0);
    let value = saved_locations(dir.path());
    assert!(value.is_empty(), "{value:?}");
}

/// An adopted location that still is a recording folder keeps its entry, so it
/// is removed by the usual rule when it stops being one.
#[tokio::test]
async fn an_adopted_location_that_is_a_recording_folder_stays_managed() {
    let dir = tempfile::tempdir().unwrap();
    let default = directory(dir.path(), "data/livetv/recordings");
    let library = FakeLibrary::with(vec![library("Recordings", &[&default])]);
    let mgr = manager(dir.path(), &library).await;
    write_options(
        dir.path(),
        &LiveTvOptions {
            media_locations_created: vec![default.clone()],
            ..LiveTvOptions::default()
        },
    );

    mgr.create_recording_folders().await;
    assert_eq!(library.libraries().len(), 1);
    assert_eq!(saved_locations(dir.path()), std::slice::from_ref(&default));

    let moved = directory(dir.path(), "moved");
    write_options(
        dir.path(),
        &LiveTvOptions {
            recording_path: Some(moved.clone()),
            media_locations_created: vec![default],
            ..LiveTvOptions::default()
        },
    );
    mgr.create_recording_folders().await;
    assert_eq!(
        library.libraries(),
        [("Recordings2".to_owned(), vec![moved.clone()])],
        "the name was taken when the new library was added"
    );
    assert_eq!(saved_locations(dir.path()), [moved]);
}

/// `RemovePathFromLibraryAsync`: the only location takes its library with it,
/// one of several just leaves it, followed by a scan.
#[tokio::test]
async fn a_location_that_stopped_being_a_folder_leaves_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    // The first pass runs on a server with nothing to inherit.
    write_options(dir.path(), &LiveTvOptions::default());
    mgr.create_recording_folders().await;
    library.folders.lock().unwrap().extend([
        self::library("Recorded Movies", &["/old/movies"]),
        self::library("Shared", &["/old/shows", "/other/shows"]),
    ]);
    write_options(
        dir.path(),
        &LiveTvOptions {
            media_locations_created: vec!["/old/movies".to_owned(), "/old/shows".to_owned()],
            ..LiveTvOptions::default()
        },
    );

    mgr.create_recording_folders().await;

    assert_eq!(
        library.libraries(),
        [("Shared".to_owned(), vec!["/other/shows".to_owned()])]
    );
    assert_eq!(library.scans.load(Ordering::SeqCst), 1);
    let value = saved_locations(dir.path());
    assert!(value.is_empty(), "{value:?}");
}

#[tokio::test]
async fn a_failed_add_holds_nothing_up_and_is_tried_again() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    library.refuse_adds.store(true, Ordering::SeqCst);
    let mgr = manager(dir.path(), &library).await;
    let default = directory(dir.path(), "dvr");
    let movies = directory(dir.path(), "dvr-movies");
    write_options(
        dir.path(),
        &LiveTvOptions {
            recording_path: Some(default.clone()),
            movie_recording_path: Some(movies.clone()),
            ..LiveTvOptions::default()
        },
    );

    mgr.create_recording_folders().await;
    assert_eq!(library.adds.load(Ordering::SeqCst), 2);
    let value = library.libraries();
    assert!(value.is_empty(), "{value:?}");

    library.refuse_adds.store(false, Ordering::SeqCst);
    mgr.create_recording_folders().await;
    assert_eq!(library.libraries().len(), 2);
    assert_eq!(saved_locations(dir.path()), [default, movies]);
}

#[tokio::test]
async fn overlapping_passes_add_one_library() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    write_options(
        dir.path(),
        &LiveTvOptions {
            recording_path: Some(directory(dir.path(), "dvr")),
            ..LiveTvOptions::default()
        },
    );

    tokio::join!(
        mgr.create_recording_folders(),
        mgr.create_recording_folders(),
        mgr.create_recording_folders(),
    );

    assert_eq!(library.adds.load(Ordering::SeqCst), 1);
}

/// A save must not replace the members this manager does not read, and a
/// document it cannot read is the administrator's to fix, not to lose.
#[tokio::test]
async fn saving_the_locations_keeps_the_rest_of_the_document() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    let default = directory(dir.path(), "dvr");
    let mut document = serde_json::to_value(LiveTvOptions {
        recording_path: Some(default.clone()),
        ..LiveTvOptions::default()
    })
    .unwrap();
    document["NotModelled"] = serde_json::json!("kept");
    std::fs::create_dir_all(options_file(dir.path()).parent().unwrap()).unwrap();
    std::fs::write(
        options_file(dir.path()),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();

    mgr.create_recording_folders().await;

    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(options_file(dir.path())).unwrap()).unwrap();
    assert_eq!(saved["NotModelled"], "kept");
    assert_eq!(saved["MediaLocationsCreated"], serde_json::json!([default]));

    std::fs::write(options_file(dir.path()), b"{ broken").unwrap();
    std::fs::create_dir_all(dir.path().join("data/livetv/recordings")).unwrap();
    mgr.create_recording_folders().await;
    assert_eq!(
        std::fs::read(options_file(dir.path())).unwrap(),
        b"{ broken"
    );
}

/// A list that was never read settles nothing: with the document unreadable
/// or missing on a pass, the entries it holds are still inherited ones when it
/// can be read, which is what a failed adoption seed retried at the next boot
/// looks like.
#[tokio::test]
async fn nothing_settles_until_the_document_can_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let adopted = directory(dir.path(), "jellyfin-recordings");
    let library = FakeLibrary::with(vec![library("Recordings", &[&adopted])]);
    let mgr = manager(dir.path(), &library).await;

    // Missing, then broken: and an existing default directory gets no library
    // from options that were not read.
    mgr.create_recording_folders().await;
    directory(dir.path(), "data/livetv/recordings");
    std::fs::create_dir_all(options_file(dir.path()).parent().unwrap()).unwrap();
    std::fs::write(options_file(dir.path()), b"{ broken").unwrap();
    mgr.create_recording_folders().await;
    assert_eq!(library.adds.load(Ordering::SeqCst), 0);

    write_options(
        dir.path(),
        &LiveTvOptions {
            media_locations_created: vec![adopted.clone()],
            ..LiveTvOptions::default()
        },
    );
    mgr.create_recording_folders().await;

    let names: Vec<String> = library.libraries().into_iter().map(|row| row.0).collect();
    assert_eq!(names, ["Recordings", "Recordings2"]);
    assert_eq!(library.libraries()[0].1, [adopted]);
}

/// The pass that creates the document made every entry in it, so a path taken
/// away later leaves the library by the usual rule.
#[tokio::test]
async fn a_pass_that_creates_the_document_settles_it() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    let default = directory(dir.path(), "data/livetv/recordings");

    mgr.create_recording_folders().await;
    assert_eq!(saved_locations(dir.path()), std::slice::from_ref(&default));

    std::fs::remove_dir(&default).unwrap();
    mgr.create_recording_folders().await;
    let value = library.libraries();
    assert!(value.is_empty(), "{value:?}");
}

/// The marker cannot be written: the pass still makes its libraries, and the
/// inherited list has already been saved without the entry.
#[tokio::test]
async fn a_marker_that_cannot_be_written_holds_nothing_up() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    // A file where its directory goes.
    std::fs::write(dir.path().join("data/livetv"), b"").unwrap();
    let default = directory(dir.path(), "dvr");
    write_options(
        dir.path(),
        &LiveTvOptions {
            recording_path: Some(default.clone()),
            media_locations_created: vec!["/inherited".to_owned()],
            ..LiveTvOptions::default()
        },
    );

    mgr.create_recording_folders().await;

    assert_eq!(library.libraries().len(), 1);
    assert_eq!(saved_locations(dir.path()), [default]);
}

#[tokio::test]
async fn nothing_is_touched_until_the_library_is_attached() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = manager_with_dvr(dir.path()).await;
    directory(dir.path(), "data/livetv/recordings");
    mgr.create_recording_folders().await;
    assert!(!options_file(dir.path()).exists());
    assert!(
        !dir.path()
            .join("data/livetv/media-locations-settled")
            .exists()
    );
    assert_eq!(mgr.get_recording_folders().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_guide_refresh_makes_the_libraries() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    directory(dir.path(), "data/livetv/recordings");

    mgr.refresh_guide().await.expect("refresh");

    assert_eq!(library.libraries().len(), 1);
}

/// Upstream keeps tuners and providers in the `livetv` configuration, so each
/// save of one is a save of it, and `OnNamedConfigurationUpdated` runs, with
/// nothing waiting for it. Each step flips the default directory, so each one
/// shows in the libraries.
#[tokio::test]
async fn every_tuner_and_provider_change_runs_the_pass() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let (mgr, tuner_id, provider_id) = mapping_manager().await;
    let mgr = mgr.with_paths(LiveTvPaths {
        transcode_dir: dir.path().join("transcodes").into(),
        data_dir: dir.path().join("data"),
        options_file: options_file(dir.path()),
    });
    mgr.set_recording_library(library.clone(), Arc::default());
    let default = dir.path().join("data/livetv/recordings");
    let flip = || {
        if default.exists() {
            std::fs::remove_dir(&default).unwrap();
        } else {
            std::fs::create_dir_all(&default).unwrap();
        }
    };
    let settles_at = |libraries: usize| {
        let library = Arc::clone(&library);
        async move {
            wait_for(|| async { (library.libraries().len() == libraries).then_some(()) })
                .await
                .is_some()
        }
    };

    flip();
    let provider = mgr.get_listing_providers().await.expect("providers");
    mgr.save_listing_provider(provider[0].clone(), false, false)
        .await
        .expect("save the provider");
    assert!(settles_at(1).await, "save_listing_provider");

    flip();
    let two = crate::mapping::m3u_channel_id("http://tuner/playlist.m3u", "http://tuner/two");
    mgr.set_channel_mapping(&provider_id, &two, "one.tv")
        .await
        .expect("map");
    assert!(settles_at(0).await, "set_channel_mapping");

    flip();
    mgr.delete_listing_provider(&provider_id)
        .await
        .expect("delete the provider");
    assert!(settles_at(1).await, "delete_listing_provider");

    flip();
    let tuner = mgr.get_tuner_hosts().await.expect("tuners").remove(0);
    mgr.save_tuner_host(tuner).await.expect("save the tuner");
    assert!(settles_at(0).await, "save_tuner_host");

    flip();
    mgr.delete_tuner_host(&tuner_id)
        .await
        .expect("delete the tuner");
    assert!(settles_at(1).await, "delete_tuner_host");
}

/// Dropping the caller's future, as a client hanging up does to a request's,
/// leaves the pass running: a library it added is always recorded.
#[tokio::test]
async fn a_caller_that_goes_away_does_not_stop_the_pass() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    *library.add_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let mgr = manager(dir.path(), &library).await;
    let default = directory(dir.path(), "dvr");
    write_options(
        dir.path(),
        &LiveTvOptions {
            recording_path: Some(default.clone()),
            ..LiveTvOptions::default()
        },
    );

    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        mgr.create_recording_folders(),
    )
    .await;
    assert!(dropped.is_err(), "the pass waits at the gate");
    gate.notify_one();

    let saved =
        wait_for(|| async { (!saved_locations(dir.path()).is_empty()).then_some(()) }).await;
    assert!(saved.is_some(), "the pass finished without its caller");
    assert_eq!(saved_locations(dir.path()), [default]);
}

/// The whole of a first recording: its directory does not exist until it
/// starts, the file stays out of scans while it is written, and the refresh at
/// the end is what lets a scan find it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_recording_makes_its_library_and_is_refreshed_when_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    let program = now_playing(&mgr).await;
    let defaults = mgr
        .get_new_timer_defaults(Some(Uuid::parse_str(&program.id).expect("guid")))
        .await
        .expect("defaults");
    let timer_id = mgr
        .create_timer(timer_from_defaults(&defaults))
        .await
        .expect("create");

    let path = wait_for(|| async {
        mgr.get_active_recording_path(&timer_id)
            .await
            .ok()
            .flatten()
    })
    .await
    .expect("the capture must start");
    let made = wait_for(|| async { (!library.libraries().is_empty()).then_some(()) }).await;
    assert!(made.is_some(), "the recording's start makes the library");
    assert_eq!(library.libraries()[0].0, "Recordings");
    assert_eq!(mgr.paths_being_recorded(), std::slice::from_ref(&path));
    assert!(library.refreshed().is_empty(), "nothing to scan yet");

    mgr.cancel_timer(&timer_id).await.expect("cancel");

    let refreshed = wait_for(|| async { (!library.refreshed().is_empty()).then_some(()) }).await;
    assert!(
        refreshed.is_some(),
        "the end of a capture refreshes its file"
    );
    assert_eq!(library.refreshed(), std::slice::from_ref(&path));
    let value = mgr.paths_being_recorded();
    assert!(value.is_empty(), "{value:?}");
    let _ = tokio::fs::remove_file(path).await;
}

/// What is left of a recording after its file went another way than
/// `DELETE /LiveTv/Recordings/{id}`: a library delete, or the file gone from
/// under the server. And that delete takes the library item with the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recording_lists_while_it_has_a_file_and_deletes_with_its_item() {
    let dir = tempfile::tempdir().unwrap();
    let library = FakeLibrary::with(Vec::new());
    let mgr = manager(dir.path(), &library).await;
    let list = |mgr: FerrofinLiveTvManager| async move {
        mgr.get_recordings_matching(
            &ferrofin_model::live_tv::RecordingQuery::default(),
            None,
            &DtoOptions::default(),
        )
        .await
        .expect("recordings")
    };
    let program = now_playing(&mgr).await;
    let defaults = mgr
        .get_new_timer_defaults(Some(Uuid::parse_str(&program.id).expect("guid")))
        .await
        .expect("defaults");
    let timer_id = mgr
        .create_timer(timer_from_defaults(&defaults))
        .await
        .expect("create");
    let path = wait_for(|| async {
        mgr.get_active_recording_path(&timer_id)
            .await
            .ok()
            .flatten()
    })
    .await
    .expect("the capture must start");
    let bytes = wait_for(|| async {
        let written = tokio::fs::metadata(&path).await.is_ok_and(|m| m.len() > 0);
        written.then_some(())
    })
    .await;
    assert!(bytes.is_some(), "the capture wrote something");
    // Still listed while it is written.
    assert_eq!(list(mgr.clone()).await.items.len(), 1);
    mgr.cancel_timer(&timer_id).await.expect("cancel");
    let settled = wait_for(|| async {
        let mgr = mgr.clone();
        let items = list(mgr).await.items;
        (items.len() == 1 && items[0].status.is_none()).then_some(())
    })
    .await;
    assert!(settled.is_some(), "the capture settles as a recording");
    let id = list(mgr.clone()).await.items[0].id;

    tokio::fs::rename(&path, format!("{path}.moved"))
        .await
        .expect("move");
    let listed = list(mgr.clone()).await;
    assert!(listed.items.is_empty(), "{:?}", listed.items);
    assert_eq!(listed.total_record_count, 0);
    assert!(mgr.get_recording(id).await.expect("read").is_none());

    tokio::fs::rename(format!("{path}.moved"), &path)
        .await
        .expect("move back");
    let listed = list(mgr.clone()).await;
    assert_eq!(listed.items.len(), 1);
    assert!(
        mgr.get_recording(listed.items[0].id)
            .await
            .expect("read")
            .is_some()
    );

    // Its folder gone too is an unmounted share, not a deleted recording.
    let folder = Path::new(&path).parent().expect("a folder").to_path_buf();
    let offline = folder.with_extension("offline");
    tokio::fs::rename(&folder, &offline).await.expect("unmount");
    assert_eq!(list(mgr.clone()).await.items.len(), 1);
    tokio::fs::rename(&offline, &folder).await.expect("mount");

    let before = library.refreshed().len();
    mgr.delete_recording(id).await.expect("delete");
    assert!(!Path::new(&path).exists());
    assert_eq!(library.refreshed()[before..], [path]);
}
