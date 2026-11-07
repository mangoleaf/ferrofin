//! What a session request reads from its device. A new session takes the
//! device's custom name and last reported capabilities (C# `CreateSessionInfo`
//! and `OnSessionStarted`); a request on a session that already exists reads
//! neither. Upstream reads the custom name on every request, but from a
//! dictionary (`DeviceManager._deviceOptions`); here it is a SQLite statement,
//! which a playing client's progress report every few seconds would pay.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::security::{DeviceEntity, DeviceOptionsEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::devices::DeviceInfo;
use ferrofin_model::dto::{ClientCapabilitiesDto, DeviceInfoDto};
use ferrofin_model::querying::QueryResult;
use ferrofin_model::session::{
    ClientCapabilities, PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo,
};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::devices::{DeviceManager, DeviceQuery};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::session::{SessionEvent, SessionManager};
use uuid::Uuid;

use super::super::FerrofinSessionManager;
use super::{FixedConfig, UnusedDtoService, library_manager, raised, seed_named_user, test_db};
use crate::configuration_manager::default_server_configuration;
use crate::device_manager::FerrofinDeviceManager;
use crate::event_manager::FerrofinEventManager;
use crate::user_data_manager::FerrofinUserDataManager;
use crate::user_manager::FerrofinUserManager;

/// The real device manager, counting the two reads a new session takes from
/// its device. With a rendezvous, each options read waits until as many reads
/// have arrived as it counts, so concurrent first requests are all inside the
/// read at once.
struct CountingDevices {
    inner: FerrofinDeviceManager,
    options_reads: AtomicUsize,
    capability_reads: AtomicUsize,
    rendezvous: Option<tokio::sync::Barrier>,
}

impl CountingDevices {
    fn new(db: &Database, rendezvous: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            inner: FerrofinDeviceManager::new(db.clone()),
            options_reads: AtomicUsize::new(0),
            capability_reads: AtomicUsize::new(0),
            rendezvous: rendezvous.map(tokio::sync::Barrier::new),
        })
    }

    /// The (options, capabilities) reads so far.
    fn reads(&self) -> (usize, usize) {
        (
            self.options_reads.load(Ordering::SeqCst),
            self.capability_reads.load(Ordering::SeqCst),
        )
    }
}

#[async_trait]
impl DeviceManager for CountingDevices {
    async fn create_device(&self, device: &DeviceEntity) -> Result<DeviceEntity, ServiceError> {
        self.inner.create_device(device).await
    }
    async fn save_capabilities(
        &self,
        device_id: &str,
        capabilities: &ClientCapabilities,
    ) -> Result<(), ServiceError> {
        self.inner.save_capabilities(device_id, capabilities).await
    }
    async fn get_capabilities(
        &self,
        device_id: Option<&str>,
    ) -> Result<ClientCapabilities, ServiceError> {
        self.capability_reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_capabilities(device_id).await
    }
    async fn get_device(&self, id: &str) -> Result<Option<DeviceInfoDto>, ServiceError> {
        self.inner.get_device(id).await
    }
    async fn get_devices(
        &self,
        query: &DeviceQuery,
    ) -> Result<QueryResult<DeviceEntity>, ServiceError> {
        self.inner.get_devices(query).await
    }
    async fn get_device_infos(
        &self,
        query: &DeviceQuery,
    ) -> Result<QueryResult<DeviceInfo>, ServiceError> {
        self.inner.get_device_infos(query).await
    }
    async fn get_devices_for_user(
        &self,
        user_id: Option<Uuid>,
    ) -> Result<QueryResult<DeviceInfoDto>, ServiceError> {
        self.inner.get_devices_for_user(user_id).await
    }
    async fn delete_device(&self, device: &DeviceEntity) -> Result<(), ServiceError> {
        self.inner.delete_device(device).await
    }
    async fn update_device(&self, device: &DeviceEntity) -> Result<(), ServiceError> {
        self.inner.update_device(device).await
    }
    async fn can_access_device(
        &self,
        user: &UserEntity,
        device_id: &str,
    ) -> Result<bool, ServiceError> {
        self.inner.can_access_device(user, device_id).await
    }
    async fn can_access_devices(
        &self,
        user: &UserEntity,
        device_ids: &[&str],
    ) -> Result<Vec<bool>, ServiceError> {
        self.inner.can_access_devices(user, device_ids).await
    }
    async fn update_device_options(
        &self,
        device_id: &str,
        device_name: Option<&str>,
    ) -> Result<(), ServiceError> {
        self.inner
            .update_device_options(device_id, device_name)
            .await
    }
    async fn get_device_options(
        &self,
        device_id: &str,
    ) -> Result<Option<DeviceOptionsEntity>, ServiceError> {
        self.options_reads.fetch_add(1, Ordering::SeqCst);
        if let Some(rendezvous) = &self.rendezvous {
            rendezvous.wait().await;
        }
        self.inner.get_device_options(device_id).await
    }
    async fn to_client_capabilities_dto(
        &self,
        capabilities: &ClientCapabilities,
    ) -> Result<ClientCapabilitiesDto, ServiceError> {
        self.inner.to_client_capabilities_dto(capabilities).await
    }
}

/// A session manager over `db` whose device manager is `devices`.
fn manager_over(db: &Database, devices: Arc<CountingDevices>) -> Arc<FerrofinSessionManager> {
    let config: Arc<dyn ServerConfigurationManager> = Arc::new(FixedConfig {
        config: default_server_configuration(),
    });
    Arc::new(FerrofinSessionManager::new(
        Arc::new(FerrofinUserManager::new(db.clone())),
        devices,
        Arc::new(FerrofinUserDataManager::new(db.clone(), config)),
        library_manager(db),
        Arc::new(UnusedDtoService),
        Arc::new(FerrofinEventManager::new()),
        db.clone(),
        "server-1".to_owned(),
    ))
}

/// Only the request that creates a session reads its device. What a playing
/// client sends afterwards (the playstate handlers log the session's activity,
/// then report) reads nothing more from it. Another user on the same client
/// and device is another session, so its first request reads again.
#[tokio::test]
async fn a_request_on_a_known_session_reads_nothing_from_its_device() {
    let db = test_db().await;
    let devices = CountingDevices::new(&db, None);
    let mgr = manager_over(&db, Arc::clone(&devices));
    let alice = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let log = |user: &UserEntity| {
        let user = user.clone();
        let mgr = Arc::clone(&mgr);
        async move {
            mgr.log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
                .await
                .unwrap()
        }
    };

    let session_id = log(&alice).await.id.unwrap();
    assert_eq!(
        devices.reads(),
        (1, 1),
        "the request that creates the session reads its device once"
    );

    let item_id = Uuid::new_v4();
    log(&alice).await;
    mgr.on_playback_start(&PlaybackStartInfo {
        session_id: Some(session_id.clone()),
        item_id,
        ..PlaybackStartInfo::default()
    })
    .await
    .unwrap();
    for ticks in [10, 20, 30] {
        log(&alice).await;
        let progress = PlaybackProgressInfo {
            session_id: Some(session_id.clone()),
            item_id,
            position_ticks: Some(ticks),
            ..PlaybackProgressInfo::default()
        };
        mgr.on_playback_progress(&progress, false).await.unwrap();
    }
    log(&alice).await;
    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: Some(session_id.clone()),
        item_id,
        position_ticks: Some(40),
        ..PlaybackStopInfo::default()
    })
    .await
    .unwrap();
    assert_eq!(
        devices.reads(),
        (1, 1),
        "requests on the known session read nothing from its device"
    );

    let bob = seed_named_user(&db, Uuid::new_v4(), "bob").await;
    log(&bob).await;
    assert_eq!(
        devices.reads(),
        (2, 2),
        "bob's first request creates his session"
    );
}

/// A new session still takes what its device has stored: the custom name an
/// admin gave it, which later reported names do not replace (C#
/// `HasCustomDeviceName`), and the capabilities it last reported.
#[tokio::test]
async fn a_new_session_takes_its_devices_custom_name_and_capabilities() {
    let db = test_db().await;
    let devices = CountingDevices::new(&db, None);
    let mgr = manager_over(&db, Arc::clone(&devices));
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    devices
        .update_device_options("dev-1", Some("Den"))
        .await
        .unwrap();
    devices
        .save_capabilities(
            "dev-1",
            &ClientCapabilities {
                supports_media_control: true,
                ..ClientCapabilities::default()
            },
        )
        .await
        .unwrap();

    let created = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
        .await
        .unwrap();
    assert_eq!(created.device_name.as_deref(), Some("Den"));
    assert!(created.has_custom_device_name);
    assert!(created.supports_media_control);

    let again = mgr
        .log_session_activity("Web", "1.0", "dev-1", "Phone", "e", Some(&user))
        .await
        .unwrap();
    assert_eq!(again.id, created.id);
    assert_eq!(
        again.device_name.as_deref(),
        Some("Den"),
        "the custom name outlasts the reported one"
    );
    assert!(again.supports_media_control);
}

/// Two first requests for one session, both inside the device read at once,
/// create it once: the first insert wins, as with C#'s `GetOrAdd`, the other
/// request joins that session, and `SessionStarted` is raised once.
#[tokio::test]
async fn concurrent_first_requests_create_one_session() {
    let db = test_db().await;
    let devices = CountingDevices::new(&db, Some(2));
    let mgr = manager_over(&db, Arc::clone(&devices));
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let mut events = mgr
        .subscribe_session_events()
        .expect("the session manager raises events");

    let (first, second) = tokio::join!(
        mgr.log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user)),
        mgr.log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user)),
    );
    let (first, second) = (first.unwrap(), second.unwrap());

    assert_eq!(
        devices.reads().0,
        2,
        "both requests were inside the device read at once"
    );
    assert_eq!(first.id, second.id);
    let started = raised(&mut events)
        .into_iter()
        .filter(|event| *event == SessionEvent::SessionStarted)
        .count();
    assert_eq!(started, 1, "the session is created once");
    let sessions = mgr
        .get_sessions(Uuid::nil(), None, None, None, true)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
}

/// `log_session_activity_id`, what the playstate and session handlers call, is
/// `log_session_activity` answering only the id: the same session, the same
/// events (a new session raises `SessionStarted` and `SessionActivity`, a
/// known one neither), the same device reads, and the same refusal of an empty
/// device id.
#[tokio::test]
async fn the_id_path_logs_the_same_activity() {
    let db = test_db().await;
    let devices = CountingDevices::new(&db, None);
    let mgr = manager_over(&db, Arc::clone(&devices));
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let mut events = mgr
        .subscribe_session_events()
        .expect("the session manager raises events");

    let id = mgr
        .log_session_activity_id("Web", "1.0", "dev-1", "TV", "e", Some(&user))
        .await
        .unwrap();
    assert!(id.is_some());
    assert_eq!(
        raised(&mut events),
        [SessionEvent::SessionStarted, SessionEvent::SessionActivity]
    );
    assert_eq!(devices.reads(), (1, 1));

    let again = mgr
        .log_session_activity_id("Web", "1.0", "dev-1", "TV", "e", Some(&user))
        .await
        .unwrap();
    assert_eq!(again, id);
    assert_eq!(raised(&mut events), [], "a known session starts nothing");
    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
        .await
        .unwrap();
    assert_eq!(dto.id, id);
    assert_eq!(devices.reads(), (1, 1));

    let refused = mgr
        .log_session_activity_id("Web", "1.0", "", "TV", "e", Some(&user))
        .await;
    assert!(
        matches!(refused, Err(ServiceError::InvalidInput(_))),
        "{refused:?}"
    );
}
