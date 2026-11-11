//! A session's `NowPlayingItem` is built once per item and kept until another
//! item plays or playback stops (C# `SessionManager.UpdateNowPlayingItem`:
//! `current is null || !info.ItemId.Equals(current.Id)`, and
//! `RemoveNowPlayingItem`). The report that names the item builds nothing: the
//! first listing of the sessions builds it, from the row the report read, and
//! later listings copy it (gl-1242). The session summaries read the reported
//! item's id and build nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::session::{
    PlayMethod, PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo, TranscodingInfo,
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
/// and name), yielding once first so concurrent listings overlap. `fail_next`
/// makes the next build fail.
#[derive(Default)]
struct CountingDtoService {
    builds: AtomicUsize,
    fail_next: AtomicBool,
}

#[async_trait]
impl DtoService for CountingDtoService {
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
        tokio::task::yield_now().await;
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

    /// The library item the session plays (C# `FullNowPlayingItem`).
    async fn library_item(&self) -> Option<Uuid> {
        self.mgr
            .now_playing_library_item_id(&self.session_id)
            .await
            .expect("library item")
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
    assert_eq!(fixture.builds(), 0, "the start report builds nothing");
    assert_eq!(fixture.summarised().await, Some(first));
    assert_eq!(fixture.builds(), 0, "the summaries build nothing");

    // The first listing builds the item; the later ones copy it.
    for _ in 0..3 {
        assert_eq!(fixture.shown().await, Some((first, "First".to_owned())));
    }
    assert_eq!(fixture.builds(), 1, "only the first read builds the item");

    // C# `info.Item = current`: a report naming the shown item again keeps
    // it, so an edit made while it plays shows once another item plays.
    fixture.rename(first, "Renamed").await;
    fixture.progress(first).await;
    fixture.start(first).await;
    assert_eq!(fixture.shown().await, Some((first, "First".to_owned())));
    assert_eq!(fixture.builds(), 1, "the same item is not built again");

    // A report naming another item replaces it, and the next read builds
    // that one.
    fixture.progress(second).await;
    assert_eq!(fixture.builds(), 1);
    assert_eq!(fixture.shown().await, Some((second, "Second".to_owned())));
    assert_eq!(fixture.builds(), 2);
    assert_eq!(fixture.summarised().await, Some(second));

    // C# `RemoveNowPlayingItem`: a stop clears it.
    fixture.stop(second).await;
    assert_eq!(fixture.shown().await, None);
    assert_eq!(fixture.summarised().await, None);

    // The edited item, played again, is built afresh.
    fixture.start(first).await;
    assert_eq!(fixture.shown().await, Some((first, "Renamed".to_owned())));
    assert_eq!(fixture.builds(), 3);

    // The item is built from the row the report read: an edit made between
    // the report and the first read shows once another item plays, as it
    // would had the report built it.
    fixture.stop(first).await;
    fixture.start(second).await;
    fixture.rename(second, "Edited").await;
    assert_eq!(fixture.shown().await, Some((second, "Second".to_owned())));
}

/// Listings at the same moment share one build: the second waits for the
/// first's instead of building the item again.
#[tokio::test]
async fn listings_at_the_same_moment_share_one_build() {
    let fixture = Fixture::new().await;
    let item = fixture.item("Film").await;
    fixture.start(item).await;

    let (first, second) = tokio::join!(fixture.shown(), fixture.shown());
    assert_eq!(first, Some((item, "Film".to_owned())));
    assert_eq!(second, first);
    assert_eq!(fixture.builds(), 1);
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
/// plays (the maintenance guard holds), and later reads do not retry it. The
/// next report naming the item lets the next read build it again, as
/// upstream's next report does after its own failed one.
#[tokio::test]
async fn a_failed_build_shows_no_item_until_the_next_report_names_it() {
    let fixture = Fixture::new().await;
    let item = fixture.item("Film").await;

    fixture.dtos.fail_next.store(true, Ordering::SeqCst);
    fixture.start(item).await;
    assert_eq!(fixture.shown().await, None);
    assert_eq!(fixture.shown().await, None);
    assert_eq!(
        fixture.builds(),
        1,
        "a failed build is not retried by a read"
    );
    assert_eq!(fixture.summarised().await, None);
    assert!(fixture.mgr.has_active_playback().await.expect("probe"));

    fixture.progress(item).await;
    assert_eq!(fixture.shown().await, Some((item, "Film".to_owned())));
    assert_eq!(fixture.builds(), 2);
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
    let session = |user_id, now_playing_item_id, play_method, is_transcoding| SessionSummary {
        user_id,
        now_playing_item_id,
        play_method,
        is_transcoding,
    };
    assert_eq!(
        summaries,
        sorted(vec![
            // The reported PlayMethod: the fixture's report leaves the default.
            session(watcher, Some(item), Some(PlayMethod::Transcode), true),
            session(other, None, None, false),
            session(Uuid::nil(), None, None, false),
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

/// C# `UpdateNowPlayingItem` builds `info.Item` only when the report carries
/// none, then shows it (`session.NowPlayingItem = info.Item`): a report's own
/// `Item` is shown as sent, whether or not the library has the item, and
/// builds nothing. Without one, an item the library lacks shows nothing, and
/// a library item reported again keeps the item shown (`info.Item = current`)
/// (gl-1062).
#[tokio::test]
async fn a_reports_own_item_is_shown_as_sent() {
    let fixture = Fixture::new().await;
    let reported = |id, name: &str| BaseItemDto {
        id,
        name: Some(name.to_owned()),
        ..BaseItemDto::default()
    };

    let external = Uuid::new_v4();
    fixture
        .mgr
        .on_playback_start(&PlaybackStartInfo {
            item: Some(reported(external, "Broadcast")),
            ..fixture.report(external)
        })
        .await
        .expect("start");
    assert_eq!(
        fixture.shown().await,
        Some((external, "Broadcast".to_owned()))
    );
    assert_eq!(fixture.summarised().await, Some(external));
    fixture
        .mgr
        .on_playback_progress(
            &PlaybackProgressInfo {
                item: Some(reported(external, "Broadcast, part 2")),
                ..fixture.report(external)
            },
            false,
        )
        .await
        .expect("progress");
    assert_eq!(
        fixture.shown().await,
        Some((external, "Broadcast, part 2".to_owned()))
    );
    fixture.progress(external).await;
    assert_eq!(fixture.shown().await, None);
    assert_eq!(fixture.summarised().await, None);

    let film = fixture.item("Film").await;
    fixture
        .mgr
        .on_playback_start(&PlaybackStartInfo {
            item: Some(reported(film, "Client Title")),
            ..fixture.report(film)
        })
        .await
        .expect("start");
    fixture.progress(film).await;
    fixture.start(film).await;
    assert_eq!(
        fixture.shown().await,
        Some((film, "Client Title".to_owned()))
    );
    assert_eq!(fixture.builds(), 0, "a reported item is never built");
    assert_eq!(fixture.library_item().await, Some(film));
    fixture.stop(film).await;
    assert_eq!(fixture.shown().await, None);
    assert_eq!(fixture.summarised().await, None);
    assert_eq!(fixture.library_item().await, None);
}

/// The edges of a report's own `Item` whose `Id` is not its `ItemId`: it is
/// shown with its own `Id` (C# `NowPlayingItem.Id`) while the library item is
/// the `ItemId`'s (`FullNowPlayingItem`). A later report naming its `Id`
/// keeps it only while the library has that id too (`info.Item = current`),
/// and one naming the `ItemId` shows the library item built from the row the
/// session holds (`current.Id` is not the `ItemId`). One with a nil `Id`, or
/// sent with a nil `ItemId`, is shown as sent, and the nil `ItemId` names no
/// library item.
#[tokio::test]
async fn a_reported_item_keeps_its_own_id() {
    let fixture = Fixture::new().await;
    let film = fixture.item("Film").await;
    let sequel = fixture.item("Sequel").await;
    let external = Uuid::new_v4();
    let reported = |id, item_id| PlaybackStartInfo {
        item: Some(BaseItemDto {
            id,
            name: Some("Client Title".to_owned()),
            ..BaseItemDto::default()
        }),
        ..fixture.report(item_id)
    };
    let client_title = |id| Some((id, "Client Title".to_owned()));

    for id in [external, sequel] {
        fixture
            .mgr
            .on_playback_start(&reported(id, film))
            .await
            .expect("start");
        assert_eq!(fixture.shown().await, client_title(id));
        assert_eq!(fixture.summarised().await, Some(id));
        assert_eq!(fixture.library_item().await, Some(film));
        fixture.progress(id).await;
        if id == external {
            assert_eq!(fixture.shown().await, None);
            assert_eq!(fixture.library_item().await, None);
        } else {
            assert_eq!(fixture.shown().await, client_title(sequel));
            assert_eq!(fixture.library_item().await, Some(sequel));
        }
    }

    // A start naming the shown Item's id keeps it too.
    fixture
        .mgr
        .on_playback_start(&reported(sequel, film))
        .await
        .expect("start");
    fixture.start(sequel).await;
    assert_eq!(fixture.shown().await, client_title(sequel));
    assert_eq!(fixture.library_item().await, Some(sequel));

    fixture
        .mgr
        .on_playback_start(&reported(external, film))
        .await
        .expect("start");
    fixture.rename(film, "Renamed").await;
    fixture.progress(film).await;
    assert_eq!(fixture.shown().await, Some((film, "Film".to_owned())));

    for (id, item_id) in [(Uuid::nil(), film), (external, Uuid::nil())] {
        fixture
            .mgr
            .on_playback_progress(&reported(id, item_id), false)
            .await
            .expect("progress");
        assert_eq!(fixture.shown().await, client_title(id));
        assert_eq!(fixture.summarised().await, Some(id));
    }
    assert_eq!(fixture.library_item().await, None);
}
