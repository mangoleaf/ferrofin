//! A session's `PlayState`, play queue, transcoding info and now-viewing item
//! follow the reports as upstream's do (v12.4 `SessionManager`):
//! `UpdateNowPlayingItem` copies the report's play state (its `MediaSourceId`
//! the item's own id when none is reported) and `PlaylistItemId`; a start or
//! progress report that does not transcode clears the device's
//! `TranscodingInfo`; `OnPlaybackStopped` keeps a reported `NowPlayingQueue`
//! and `PlaylistItemId`, then `RemoveNowPlayingItem` resets the play state and
//! clears the transcoding info; `ReportNowViewingItem` keeps the item as a
//! session shows it.

use std::sync::Arc;

use ferrofin_db::Database;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SessionInfoDto;
use ferrofin_model::session::{
    PlayMethod, PlaybackOrder, PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo,
    PlayerStateInfo, QueueItem, RepeatMode, TranscodingInfo,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::session::SessionManager;
use uuid::Uuid;

use super::super::FerrofinSessionManager;
use super::{manager, seed_named_user, test_db};

const DEVICE: &str = "dev-play-state";

/// One user's session over a fresh database, with a movie to play.
struct Fixture {
    mgr: Arc<FerrofinSessionManager>,
    session_id: String,
    item: Uuid,
    db: Database,
}

impl Fixture {
    async fn new() -> Self {
        let db = test_db().await;
        let mgr = manager(&db);
        let user = seed_named_user(&db, Uuid::new_v4(), "watcher").await;
        let session_id = mgr
            .log_session_activity("Web", "1.0", DEVICE, "TV", "e", Some(&user))
            .await
            .expect("session")
            .id
            .expect("session id");
        let item = Uuid::new_v4();
        crate::test_support::seed_named_item(&db, item, BaseItemKind::Movie, "Film").await;
        Self {
            mgr,
            session_id,
            item,
            db,
        }
    }

    /// The session as `GET /Sessions` lists it.
    async fn listed(&self) -> SessionInfoDto {
        let mut listed = self
            .mgr
            .get_sessions(Uuid::nil(), None, None, None, true)
            .await
            .expect("sessions");
        assert_eq!(listed.len(), 1);
        listed.remove(0)
    }

    async fn transcoding(&self) {
        self.mgr
            .report_transcoding_info(
                DEVICE,
                &TranscodingInfo {
                    video_codec: Some("h264".to_owned()),
                    ..TranscodingInfo::default()
                },
            )
            .await
            .expect("transcoding info");
    }

    fn report(&self, play_method: PlayMethod) -> PlaybackProgressInfo {
        PlaybackProgressInfo {
            session_id: Some(self.session_id.clone()),
            item_id: self.item,
            play_method,
            ..PlaybackProgressInfo::default()
        }
    }

    fn stop(&self) -> PlaybackStopInfo {
        PlaybackStopInfo {
            session_id: Some(self.session_id.clone()),
            item_id: self.item,
            position_ticks: Some(30),
            ..PlaybackStopInfo::default()
        }
    }
}

#[tokio::test]
async fn the_play_state_is_what_the_reports_say_and_a_stop_resets_it() {
    let fixture = Fixture::new().await;
    let start = PlaybackStartInfo {
        can_seek: true,
        is_muted: true,
        volume_level: Some(42),
        audio_stream_index: Some(1),
        subtitle_stream_index: Some(2),
        position_ticks: Some(10),
        repeat_mode: RepeatMode::RepeatAll,
        playback_order: PlaybackOrder::Shuffle,
        live_stream_id: Some("live-1".to_owned()),
        playlist_item_id: Some("playlist-item-1".to_owned()),
        ..fixture.report(PlayMethod::DirectStream)
    };
    fixture.mgr.on_playback_start(&start).await.expect("start");

    let session = fixture.listed().await;
    assert_eq!(
        session.play_state,
        Some(PlayerStateInfo {
            position_ticks: Some(10),
            can_seek: true,
            is_paused: false,
            is_muted: true,
            volume_level: Some(42),
            audio_stream_index: Some(1),
            subtitle_stream_index: Some(2),
            // No MediaSourceId reported: the item's own, as upstream fills in.
            media_source_id: Some(fixture.item.simple().to_string()),
            play_method: Some(PlayMethod::DirectStream),
            repeat_mode: RepeatMode::RepeatAll,
            playback_order: PlaybackOrder::Shuffle,
            live_stream_id: Some("live-1".to_owned()),
        })
    );
    assert_eq!(session.playlist_item_id.as_deref(), Some("playlist-item-1"));

    // Each report replaces the whole play state.
    let progress = PlaybackProgressInfo {
        is_paused: true,
        volume_level: Some(7),
        media_source_id: Some("source-2".to_owned()),
        position_ticks: Some(20),
        ..fixture.report(PlayMethod::DirectPlay)
    };
    fixture
        .mgr
        .on_playback_progress(&progress, false)
        .await
        .expect("progress");
    let session = fixture.listed().await;
    assert_eq!(
        session.play_state,
        Some(PlayerStateInfo {
            position_ticks: Some(20),
            is_paused: true,
            volume_level: Some(7),
            media_source_id: Some("source-2".to_owned()),
            play_method: Some(PlayMethod::DirectPlay),
            ..PlayerStateInfo::default()
        })
    );
    assert_eq!(session.playlist_item_id, None);
    assert_eq!(session.now_playing_queue, Some(Vec::new()));

    // The stop keeps its queue and playlist item, and resets the play state.
    let queue = vec![QueueItem {
        id: fixture.item,
        playlist_item_id: Some("playlist-item-2".to_owned()),
    }];
    fixture
        .mgr
        .on_playback_stopped(&PlaybackStopInfo {
            now_playing_queue: Some(queue.clone()),
            playlist_item_id: Some("playlist-item-2".to_owned()),
            ..fixture.stop()
        })
        .await
        .expect("stop");
    let session = fixture.listed().await;
    assert_eq!(session.play_state, Some(PlayerStateInfo::default()));
    assert_eq!(session.now_playing_queue, Some(queue.clone()));
    assert_eq!(session.playlist_item_id.as_deref(), Some("playlist-item-2"));

    // A stop reporting no queue leaves the last one (`if (info.NowPlayingQueue
    // is not null)`), and its playlist item replaces the last.
    fixture
        .mgr
        .on_playback_stopped(&fixture.stop())
        .await
        .expect("stop");
    let session = fixture.listed().await;
    assert_eq!(session.now_playing_queue, Some(queue));
    assert_eq!(session.playlist_item_id, None);
}

#[tokio::test]
async fn transcoding_info_is_cleared_by_a_report_that_does_not_transcode_and_by_the_stop() {
    let fixture = Fixture::new().await;

    fixture.transcoding().await;
    fixture
        .mgr
        .on_playback_start(&fixture.report(PlayMethod::Transcode))
        .await
        .expect("start");
    assert!(
        fixture.listed().await.transcoding_info.is_some(),
        "a transcoding report keeps it"
    );
    fixture
        .mgr
        .on_playback_progress(&fixture.report(PlayMethod::Transcode), false)
        .await
        .expect("progress");
    assert!(fixture.listed().await.transcoding_info.is_some());

    fixture
        .mgr
        .on_playback_progress(&fixture.report(PlayMethod::DirectPlay), false)
        .await
        .expect("progress");
    assert_eq!(fixture.listed().await.transcoding_info, None);

    fixture.transcoding().await;
    fixture
        .mgr
        .on_playback_start(&fixture.report(PlayMethod::DirectStream))
        .await
        .expect("start");
    assert_eq!(fixture.listed().await.transcoding_info, None);

    fixture.transcoding().await;
    fixture
        .mgr
        .on_playback_stopped(&fixture.stop())
        .await
        .expect("stop");
    assert_eq!(
        fixture.listed().await.transcoding_info,
        None,
        "RemoveNowPlayingItem clears it"
    );
}

/// `ReportTranscodingInfo` and `ClearTranscodingInfo` act on the first of
/// `Sessions`, ordered by `LastActivityDate` descending, on the device: with
/// two users signed in on one device, the one active last; no other device's.
#[tokio::test]
async fn transcoding_info_goes_to_the_devices_session_active_last() {
    let fixture = Fixture::new().await;
    let other = seed_named_user(&fixture.db, Uuid::new_v4(), "other").await;
    let sign_in = |device: &'static str| {
        fixture
            .mgr
            .log_session_activity("Web", "1.0", device, "TV", "e", Some(&other))
    };
    sign_in("dev-elsewhere").await.expect("session");
    // Later than the fixture's own session on the device.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let other_session = sign_in(DEVICE).await.expect("session").id;
    fixture
        .mgr
        .report_transcoding_info("dev-elsewhere", &TranscodingInfo::default())
        .await
        .expect("transcoding info");
    let transcoding = || async {
        let mut listed: Vec<(String, bool)> = fixture
            .mgr
            .get_sessions(Uuid::nil(), None, None, None, true)
            .await
            .expect("sessions")
            .into_iter()
            .map(|s| {
                let is_watcher = s.id.as_deref() == Some(fixture.session_id.as_str());
                let is_other = s.id == other_session;
                let who = if is_watcher {
                    "watcher"
                } else if is_other {
                    "other"
                } else {
                    "elsewhere"
                };
                (who.to_owned(), s.transcoding_info.is_some())
            })
            .collect();
        listed.sort();
        listed
    };

    fixture.transcoding().await;
    assert_eq!(
        transcoding().await,
        [("elsewhere", true), ("other", true), ("watcher", false)].map(|(w, t)| (w.to_owned(), t))
    );

    // The watcher's report makes theirs the session active last, so its clear
    // leaves the other user's transcode shown, as upstream does.
    fixture
        .mgr
        .on_playback_progress(&fixture.report(PlayMethod::DirectPlay), false)
        .await
        .expect("progress");
    assert_eq!(
        transcoding().await,
        [("elsewhere", true), ("other", true), ("watcher", false)].map(|(w, t)| (w.to_owned(), t))
    );
    fixture.transcoding().await;
    assert_eq!(
        transcoding().await,
        [("elsewhere", true), ("other", true), ("watcher", true)].map(|(w, t)| (w.to_owned(), t))
    );
    // A report that does not transcode now clears the watcher's, the session
    // active last, and only theirs.
    fixture
        .mgr
        .on_playback_progress(&fixture.report(PlayMethod::DirectPlay), false)
        .await
        .expect("progress");
    assert_eq!(
        transcoding().await,
        [("elsewhere", true), ("other", true), ("watcher", false)].map(|(w, t)| (w.to_owned(), t))
    );
}

#[tokio::test]
async fn the_now_viewing_item_is_the_item_as_a_session_shows_it() {
    let fixture = Fixture::new().await;
    assert_eq!(fixture.listed().await.now_viewing_item, None);

    fixture
        .mgr
        .report_now_viewing_item(&fixture.session_id, &fixture.item.to_string())
        .await
        .expect("viewing");
    let viewing = fixture.listed().await.now_viewing_item.expect("an item");
    assert_eq!(viewing.id, fixture.item);
    assert_eq!(viewing.name.as_deref(), Some("Film"));

    // `GetItemInfo(null)` throws `ArgumentNullException`: a 400.
    let err = fixture
        .mgr
        .report_now_viewing_item(&fixture.session_id, &Uuid::new_v4().to_string())
        .await
        .expect_err("an unknown item");
    assert!(matches!(err, ServiceError::InvalidInput(_)), "{err:?}");
}

/// `GetSessionInfo` stamps the user's profile-image tag on every request on
/// the session, so a new or removed image shows on the next one.
#[tokio::test]
async fn every_request_stamps_the_users_profile_image_tag() {
    use ferrofin_traits::library::UserManager as _;

    let db = test_db().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let users = Arc::new(
        crate::user_manager::FerrofinUserManager::new(db.clone())
            .with_profile_image_dir(dir.path().join("users"))
            .with_image_processor(Arc::new(ferrofin_drawing::ImageProcessor::new(
                Arc::new(ferrofin_drawing::NullImageEncoder),
                dir.path().join("cache"),
            ))),
    );
    let config: Arc<dyn ferrofin_traits::configuration::ServerConfigurationManager> =
        Arc::new(super::FixedConfig {
            config: crate::configuration_manager::default_server_configuration(),
        });
    let mgr = FerrofinSessionManager::new(
        Arc::clone(&users) as Arc<dyn ferrofin_traits::library::UserManager>,
        Arc::new(crate::device_manager::FerrofinDeviceManager::new(
            db.clone(),
        )),
        Arc::new(crate::user_data_manager::FerrofinUserDataManager::new(
            db.clone(),
            config,
        )),
        super::library_manager(&db),
        Arc::new(super::UnusedDtoService),
        Arc::new(crate::event_manager::FerrofinEventManager::new()),
        db.clone(),
        "server-1".to_owned(),
    );
    let user = users.create_user("avatar").await.expect("user");
    let request = || mgr.log_session_activity("Web", "1.0", DEVICE, "TV", "e", Some(&user));

    assert_eq!(
        request().await.expect("session").user_primary_image_tag,
        None
    );

    users
        .save_profile_image(&user, b"image", "image/png", ".png")
        .await
        .expect("save");
    let tag = users
        .get_user_dto(&user, None)
        .await
        .expect("dto")
        .primary_image_tag
        .expect("a tag");
    assert_eq!(
        request().await.expect("session").user_primary_image_tag,
        Some(tag.clone())
    );
    let listed = mgr
        .get_sessions(Uuid::nil(), None, None, None, true)
        .await
        .expect("sessions");
    assert_eq!(listed[0].user_primary_image_tag, Some(tag));

    users.clear_profile_image(&user).await.expect("clear");
    assert_eq!(
        request().await.expect("session").user_primary_image_tag,
        None
    );
}
