//! A session's `NowPlayingItem` is built once per item, on the report that
//! names it, and kept until another item plays or playback stops (C#
//! `SessionManager.UpdateNowPlayingItem`: `current is null ||
//! !info.ItemId.Equals(current.Id)`, and `RemoveNowPlayingItem`). Listing the
//! sessions copies the stored item and builds nothing. The session summaries
//! read the same stored item.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::session::{
    PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo, TranscodingInfo,
};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::DtoOptions;
use ferrofin_traits::session::{SessionManager, SessionSummary};
use uuid::Uuid;

use super::super::FerrofinSessionManager;
use super::{FixedConfig, library_manager, seed_named_user, test_db};
use crate::configuration_manager::default_server_configuration;
use crate::device_manager::FerrofinDeviceManager;
use crate::event_manager::FerrofinEventManager;
use crate::user_data_manager::FerrofinUserDataManager;
use crate::user_manager::FerrofinUserManager;

/// A DTO service that counts its builds and answers with the bare item (id
/// and name). `fail_next` makes the next build fail.
#[derive(Default)]
struct CountingDtoService {
    builds: AtomicUsize,
    fail_next: AtomicBool,
}

#[async_trait]
impl DtoService for CountingDtoService {
    async fn get_primary_image_aspect_ratio(
        &self,
        _item_id: Uuid,
    ) -> Result<Option<f64>, ServiceError> {
        unreachable!("not reached from a NowPlayingItem build")
    }
    async fn get_base_item_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        unreachable!("not reached from a NowPlayingItem build")
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(ServiceError::backend("the build failed"));
        }
        Ok(items
            .iter()
            .map(|row| BaseItemDto {
                id: Uuid::parse_str(&row.id).expect("item id"),
                name: row.name.clone(),
                ..BaseItemDto::default()
            })
            .collect())
    }
    async fn get_item_by_name_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        unreachable!("not reached from a NowPlayingItem build")
    }
}

/// A session manager over a fresh database with one user's session.
struct Fixture {
    db: Database,
    mgr: Arc<FerrofinSessionManager>,
    dtos: Arc<CountingDtoService>,
    user: UserEntity,
    session_id: String,
}

impl Fixture {
    async fn new() -> Self {
        let db = test_db().await;
        let config: Arc<dyn ServerConfigurationManager> = Arc::new(FixedConfig {
            config: default_server_configuration(),
        });
        let dtos = Arc::new(CountingDtoService::default());
        let mgr = Arc::new(FerrofinSessionManager::new(
            Arc::new(FerrofinUserManager::new(db.clone())),
            Arc::new(FerrofinDeviceManager::new(db.clone())),
            Arc::new(FerrofinUserDataManager::new(db.clone(), config)),
            library_manager(&db),
            Arc::clone(&dtos) as Arc<dyn DtoService>,
            Arc::new(FerrofinEventManager::new()) as Arc<dyn ferrofin_traits::events::EventManager>,
            db.clone(),
            "server-1".to_owned(),
        ));
        let user = seed_named_user(&db, Uuid::new_v4(), "watcher").await;
        let session = mgr
            .log_session_activity("Web", "1.0", "dev-watcher", "TV", "e", Some(&user))
            .await
            .expect("session");
        Self {
            db,
            mgr,
            dtos,
            user,
            session_id: session.id.expect("session id"),
        }
    }

    /// Seeds a movie named `name`.
    async fn item(&self, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        crate::test_support::seed_named_item(&self.db, id, BaseItemKind::Movie, name).await;
        id
    }

    /// Renames the item in the library.
    async fn rename(&self, id: Uuid, name: &str) {
        let mut row = crate::test_support::fetch_item(&self.db, id).await;
        row.name = Some(name.to_owned());
        crate::test_support::save_item(&self.db, &row).await;
    }

    fn report(&self, item_id: Uuid) -> PlaybackProgressInfo {
        PlaybackStartInfo {
            session_id: Some(self.session_id.clone()),
            item_id,
            position_ticks: Some(10),
            ..PlaybackStartInfo::default()
        }
    }

    async fn start(&self, item_id: Uuid) {
        self.mgr
            .on_playback_start(&self.report(item_id))
            .await
            .expect("start");
    }

    async fn progress(&self, item_id: Uuid) {
        self.mgr
            .on_playback_progress(&self.report(item_id), false)
            .await
            .expect("progress");
    }

    async fn stop(&self, item_id: Uuid) {
        self.mgr
            .on_playback_stopped(&PlaybackStopInfo {
                session_id: Some(self.session_id.clone()),
                item_id,
                position_ticks: Some(20),
                ..PlaybackStopInfo::default()
            })
            .await
            .expect("stop");
    }

    /// The id and name of the item `GET /Sessions` shows for the session.
    async fn shown(&self) -> Option<(Uuid, String)> {
        let listed = self
            .mgr
            .get_sessions(Uuid::nil(), None, None, None, true)
            .await
            .expect("sessions");
        assert_eq!(listed.len(), 1);
        listed
            .into_iter()
            .next()
            .and_then(|session| session.now_playing_item)
            .map(|item| (item.id, item.name.unwrap_or_default()))
    }

    /// The item id the session's summary shows.
    async fn summarised(&self) -> Option<Uuid> {
        let summaries = self.mgr.get_session_summaries().await.expect("summaries");
        assert_eq!(summaries.len(), 1);
        summaries[0].now_playing_item_id
    }

    fn builds(&self) -> usize {
        self.dtos.builds.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn the_now_playing_item_is_built_once_per_item_and_kept_until_stop() {
    let fixture = Fixture::new().await;
    let first = fixture.item("First").await;
    let second = fixture.item("Second").await;

    fixture.start(first).await;
    assert_eq!(fixture.builds(), 1, "the start report builds the item");

    // Listing copies the stored item: no read builds one.
    for _ in 0..3 {
        assert_eq!(fixture.shown().await, Some((first, "First".to_owned())));
    }
    assert_eq!(fixture.summarised().await, Some(first));
    assert_eq!(fixture.builds(), 1, "a read builds nothing");

    // C# `info.Item = current`: a report naming the shown item again keeps
    // it, so an edit made while it plays shows once another item plays.
    fixture.rename(first, "Renamed").await;
    fixture.progress(first).await;
    fixture.start(first).await;
    assert_eq!(fixture.shown().await, Some((first, "First".to_owned())));
    assert_eq!(fixture.builds(), 1, "the same item is not built again");

    // A report naming another item builds that one.
    fixture.progress(second).await;
    assert_eq!(fixture.builds(), 2);
    assert_eq!(fixture.shown().await, Some((second, "Second".to_owned())));
    assert_eq!(fixture.summarised().await, Some(second));

    // C# `RemoveNowPlayingItem`: a stop clears it.
    fixture.stop(second).await;
    assert_eq!(fixture.shown().await, None);
    assert_eq!(fixture.summarised().await, None);

    // The edited item, played again, is built afresh.
    fixture.start(first).await;
    assert_eq!(fixture.builds(), 3);
    assert_eq!(fixture.shown().await, Some((first, "Renamed".to_owned())));
}

/// Every other way playback ends clears the shown item too: a report naming
/// an item the library does not have (C# leaves `NowPlayingItem` null), and
/// the idle-playback stop.
#[tokio::test]
async fn an_unknown_item_and_the_idle_stop_clear_the_shown_item() {
    let fixture = Fixture::new().await;
    let item = fixture.item("Film").await;

    fixture.start(item).await;
    assert_eq!(fixture.summarised().await, Some(item));
    fixture.progress(Uuid::new_v4()).await;
    assert_eq!(fixture.shown().await, None);
    assert_eq!(fixture.summarised().await, None);

    fixture.start(item).await;
    assert_eq!(fixture.summarised().await, Some(item));
    fixture
        .mgr
        .check_for_idle_playback(chrono::Utc::now() + chrono::Duration::minutes(6))
        .await;
    assert_eq!(fixture.shown().await, None);
    assert_eq!(fixture.summarised().await, None);
}

/// A build that fails leaves the session showing no item while it still
/// plays (the maintenance guard holds), and the next report naming the item
/// builds it again, as upstream's next report does after its own failed one.
#[tokio::test]
async fn a_failed_build_shows_no_item_and_the_next_report_builds_it() {
    let fixture = Fixture::new().await;
    let item = fixture.item("Film").await;

    fixture.dtos.fail_next.store(true, Ordering::SeqCst);
    fixture.start(item).await;
    assert_eq!(fixture.builds(), 1);
    assert_eq!(fixture.shown().await, None);
    assert_eq!(fixture.summarised().await, None);
    assert!(fixture.mgr.has_active_playback().await.expect("probe"));

    fixture.progress(item).await;
    assert_eq!(fixture.builds(), 2);
    assert_eq!(fixture.shown().await, Some((item, "Film".to_owned())));
    assert_eq!(fixture.summarised().await, Some(item));
}

/// The summaries list every session, whoever's, with its user, the item it
/// shows and whether it transcodes, and agree with the full listing they
/// stand in for.
#[tokio::test]
async fn session_summaries_list_every_session_with_the_item_it_shows() {
    let fixture = Fixture::new().await;
    let item = fixture.item("Film").await;
    fixture.start(item).await;
    fixture
        .mgr
        .report_transcoding_info("dev-watcher", &TranscodingInfo::default())
        .await
        .expect("transcoding info");
    let other = seed_named_user(&fixture.db, Uuid::new_v4(), "other").await;
    fixture
        .mgr
        .log_session_activity("Android", "2.0", "dev-other", "Phone", "e", Some(&other))
        .await
        .expect("other session");
    fixture
        .mgr
        .log_session_activity("Kodi", "3.0", "dev-keyless", "Box", "e", None)
        .await
        .expect("session with no user");
    let builds = fixture.builds();

    let watcher = Uuid::parse_str(&fixture.user.id).expect("user id");
    let other = Uuid::parse_str(&other.id).expect("user id");
    let sorted = |mut summaries: Vec<SessionSummary>| {
        summaries.sort_by_key(|s| s.user_id);
        summaries
    };
    let summaries = sorted(
        fixture
            .mgr
            .get_session_summaries()
            .await
            .expect("summaries"),
    );
    let session = |user_id, now_playing_item_id, is_transcoding| SessionSummary {
        user_id,
        now_playing_item_id,
        play_method: None,
        is_transcoding,
    };
    assert_eq!(
        summaries,
        sorted(vec![
            session(watcher, Some(item), true),
            session(other, None, false),
            session(Uuid::nil(), None, false),
        ])
    );
    assert_eq!(fixture.builds(), builds, "the summaries build nothing");

    // The full elevated listing, reduced through `SessionSummary::from` (what
    // the trait's default does), agrees.
    let listed = fixture
        .mgr
        .get_sessions(Uuid::nil(), None, None, None, true)
        .await
        .expect("sessions");
    assert_eq!(
        sorted(listed.iter().map(SessionSummary::from).collect()),
        summaries
    );
}
