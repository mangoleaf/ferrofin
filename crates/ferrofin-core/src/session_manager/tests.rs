//! Tests for [`FerrofinSessionManager`] over a real in-memory `ferrofin-db` plus the
//! real concrete sibling managers (user/device/user-data/library) and a minimal
//! fake [`DtoService`] (only its unused-here trait surface).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::PermissionKind;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_model::dto::{BaseItemDto, SessionInfoDto};
use ferrofin_model::secret::Secret;
use ferrofin_model::session::{ClientCapabilities, MessageCommand, SessionMessageType};
use uuid::Uuid;

use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, UserManager};
use ferrofin_traits::net::WebSocketConnection;
use ferrofin_traits::options::{AuthorizationInfo, DtoOptions};
use ferrofin_traits::persistence::ItemPersistenceService;
use ferrofin_traits::session::{AuthenticationRequest, SessionManager};
use ferrofin_traits::system::ServerApplicationPaths;

use super::FerrofinSessionManager;
use crate::configuration_manager::default_server_configuration;
use crate::device_manager::FerrofinDeviceManager;
use crate::event_manager::FerrofinEventManager;
use crate::item_count_service::FerrofinItemCountService;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_repository::FerrofinItemRepository;
use crate::item_type_lookup::ItemTypeLookup;
use crate::library_manager::FerrofinLibraryManager;
use crate::people_repository::FerrofinPeopleRepository;
use crate::user_data_manager::FerrofinUserDataManager;
use crate::user_entity_ext::set_permission;
use crate::user_manager::FerrofinUserManager;

/// A config manager returning the factory-default configuration.
struct FixedConfig {
    config: ServerConfiguration,
}

#[async_trait]
impl ServerConfigurationManager for FixedConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        unreachable!("not used in these tests")
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(Arc::new(self.config.clone()))
    }
    async fn update_configuration(
        &self,
        _configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_branding(
        &self,
    ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
        Ok(ferrofin_model::branding::BrandingOptions::default())
    }
    async fn update_branding(
        &self,
        _branding: &ferrofin_model::branding::BrandingOptions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// A DTO service that is never invoked by the tested paths (the manager holds it
/// only for the deferred now-playing-item enrichment).
struct UnusedDtoService;

#[async_trait]
impl DtoService for UnusedDtoService {
    async fn get_primary_image_aspect_ratio(
        &self,
        _item_id: Uuid,
    ) -> Result<Option<f64>, ServiceError> {
        unreachable!("dto service is not exercised by these tests")
    }
    async fn get_base_item_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        unreachable!("dto service is not exercised by these tests")
    }
    async fn get_base_item_dtos(
        &self,
        _items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        unreachable!("dto service is not exercised by these tests")
    }
    async fn get_item_by_name_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        unreachable!("dto service is not exercised by these tests")
    }
}

/// A fake WebSocket connection that records every pushed frame.
struct FakeConnection {
    auth: AuthorizationInfo,
    open: bool,
    sent: Mutex<Vec<Vec<u8>>>,
}

impl FakeConnection {
    fn new(auth: AuthorizationInfo) -> Arc<Self> {
        Arc::new(Self {
            auth,
            open: true,
            sent: Mutex::new(Vec::new()),
        })
    }
    fn sent_count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
}

#[async_trait]
impl WebSocketConnection for FakeConnection {
    fn remote_endpoint(&self) -> Option<&str> {
        None
    }
    fn authorization_info(&self) -> &AuthorizationInfo {
        &self.auth
    }
    fn is_open(&self) -> bool {
        self.open
    }
    async fn send(&self, message: &[u8]) -> Result<(), ServiceError> {
        self.sent.lock().unwrap().push(message.to_vec());
        Ok(())
    }
    async fn apply_request_culture(&self) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// The real library manager over `db` — shared by the session manager under
/// test and the fixtures that write through it.
fn library_manager(db: &Database) -> Arc<FerrofinLibraryManager> {
    let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
        Arc::new(ItemTypeLookup::new());
    Arc::new(FerrofinLibraryManager::new(
        Arc::new(FerrofinItemRepository::new(db.clone(), lookup)),
        Arc::new(FerrofinItemCountService::new(db.clone())),
        Arc::new(FerrofinItemPersistenceService::new(db.clone())),
        Arc::new(FerrofinPeopleRepository::new(db.clone())),
    ))
}

/// Builds a session manager wired over `db` with the real sibling managers.
fn manager(db: &Database) -> Arc<FerrofinSessionManager> {
    let config: Arc<dyn ServerConfigurationManager> = Arc::new(FixedConfig {
        config: default_server_configuration(),
    });
    let library = library_manager(db);
    Arc::new(FerrofinSessionManager::new(
        Arc::new(FerrofinUserManager::new(db.clone())),
        Arc::new(FerrofinDeviceManager::new(db.clone())),
        Arc::new(FerrofinUserDataManager::new(db.clone(), config)),
        library,
        Arc::new(UnusedDtoService),
        Arc::new(FerrofinEventManager::new()),
        db.clone(),
        "server-1".to_owned(),
    ))
}

/// Inserts a minimal `Users` row with the given username, returning the row.
async fn seed_named_user(db: &Database, id: Uuid, username: &str) -> UserEntity {
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, ?2)"#,
    )
    .bind(guid_to_db(id))
    .bind(username)
    .execute(db.writer())
    .await
    .expect("insert user");
    sqlx::query_as::<_, UserEntity>(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .fetch_one(db.pool())
        .await
        .expect("fetch user")
}

async fn test_db() -> Database {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    db
}

#[tokio::test]
async fn log_session_activity_creates_and_reuses_a_session() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;

    let first = mgr
        .log_session_activity("Web", "1.0", "dev-1", "Chrome", "1.2.3.4", &user)
        .await
        .unwrap();
    assert_eq!(first.client.as_deref(), Some("Web"));
    assert_eq!(first.device_name.as_deref(), Some("Chrome"));
    assert_eq!(first.device_id.as_deref(), Some("dev-1"));
    assert_eq!(first.server_id.as_deref(), Some("server-1"));

    // Same app+device → same session id (reused, not duplicated).
    let again = mgr
        .log_session_activity("Web", "1.1", "dev-1", "Chrome", "1.2.3.4", &user)
        .await
        .unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(again.application_version.as_deref(), Some("1.1"));
}

#[tokio::test]
async fn empty_device_name_falls_back_to_network_device() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "bob").await;

    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-x", "", "e", &user)
        .await
        .unwrap();
    assert_eq!(dto.device_name.as_deref(), Some("Network Device"));
}

#[tokio::test]
async fn additional_users_add_and_remove() {
    let db = test_db().await;
    let mgr = manager(&db);
    let primary = seed_named_user(&db, Uuid::new_v4(), "primary").await;
    let guest_id = Uuid::new_v4();
    let _guest = seed_named_user(&db, guest_id, "guest").await;

    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", &primary)
        .await
        .unwrap();
    let session_id = dto.id.unwrap();

    mgr.add_additional_user(&session_id, guest_id)
        .await
        .unwrap();
    let sessions = mgr
        .get_sessions(Uuid::nil(), None, None, None, true)
        .await
        .unwrap();
    let s = sessions
        .iter()
        .find(|s| s.id.as_deref() == Some(&session_id))
        .unwrap();
    assert_eq!(s.additional_users.as_ref().unwrap().len(), 1);

    mgr.remove_additional_user(&session_id, guest_id)
        .await
        .unwrap();
    let sessions = mgr
        .get_sessions(Uuid::nil(), None, None, None, true)
        .await
        .unwrap();
    let s = sessions
        .iter()
        .find(|s| s.id.as_deref() == Some(&session_id))
        .unwrap();
    assert!(s.additional_users.as_ref().unwrap().is_empty());
}

#[tokio::test]
async fn adding_the_primary_user_as_additional_is_rejected() {
    let db = test_db().await;
    let mgr = manager(&db);
    let primary_id = Uuid::new_v4();
    let primary = seed_named_user(&db, primary_id, "primary").await;
    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", &primary)
        .await
        .unwrap();
    let err = mgr
        .add_additional_user(&dto.id.unwrap(), primary_id)
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::InvalidInput(_)));
}

#[tokio::test]
async fn report_capabilities_updates_session_and_persists() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", &user)
        .await
        .unwrap();
    let session_id = dto.id.unwrap();

    let caps = ClientCapabilities {
        supports_media_control: true,
        ..ClientCapabilities::default()
    };
    mgr.report_capabilities(&session_id, &caps).await.unwrap();

    let sessions = mgr
        .get_sessions(Uuid::nil(), None, None, None, true)
        .await
        .unwrap();
    let s = sessions
        .iter()
        .find(|s| s.id.as_deref() == Some(&session_id))
        .unwrap();
    assert!(s.supports_media_control);
}

#[tokio::test]
async fn bus_connected_session_is_controllable_and_receives_play() {
    let db = test_db().await;
    let bus: Arc<dyn ferrofin_traits::session_bus::SessionMessageBus> =
        Arc::new(crate::FerrofinSessionMessageBus::new());
    let mgr = Arc::new(
        manager(&db)
            .as_ref()
            .clone()
            .with_session_bus(Arc::clone(&bus)),
    );
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "alice").await;
    let dto = mgr
        .log_session_activity("TV App", "1.0", "dev-tv", "Living Room", "e", &user)
        .await
        .unwrap();
    let session_id = dto.id.unwrap();
    mgr.report_capabilities(
        &session_id,
        &ClientCapabilities {
            supports_media_control: true,
            ..ClientCapabilities::default()
        },
    )
    .await
    .unwrap();

    // Not yet connected → not remote-controllable, not listed as castable.
    let listed = mgr
        .get_sessions(user_id, None, None, Some(user_id), false)
        .await
        .unwrap();
    assert!(listed.is_empty());

    // The `/socket` handler registers a sink on the bus for this session.
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink_received = Arc::clone(&received);
    bus.register(
        session_id.clone(),
        Box::new(move |msg| sink_received.lock().unwrap().push(msg)),
    );

    let listed = mgr
        .get_sessions(user_id, None, None, Some(user_id), false)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].supports_remote_control);
    assert!(listed[0].is_active);

    // A Play command reaches the session over the bus.
    let play = ferrofin_model::session::PlayRequest {
        item_ids: vec![Uuid::new_v4()],
        play_command: ferrofin_model::session::PlayCommand::PlayNow,
        ..ferrofin_model::session::PlayRequest::default()
    };
    mgr.send_play_command("", &session_id, &play).await.unwrap();
    let messages = received.lock().unwrap();
    assert_eq!(messages.len(), 1);
    let envelope: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
    assert_eq!(envelope["MessageType"], "Play");
    assert_eq!(envelope["Data"]["PlayCommand"], "PlayNow");
}

#[tokio::test]
async fn report_now_viewing_rejects_bad_id() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", &user)
        .await
        .unwrap();
    let err = mgr
        .report_now_viewing_item(&dto.id.unwrap(), "not-a-uuid")
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::InvalidInput(_)));
}

#[tokio::test]
async fn authenticate_direct_opens_a_session_and_mints_a_token() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "alice").await;
    set_permission(
        db.writer(),
        &user.id,
        PermissionKind::EnableAllDevices,
        true,
    )
    .await
    .unwrap();

    let request = AuthenticationRequest {
        user_id: Some(user_id),
        app: Some("Web".to_owned()),
        app_version: Some("1.0".to_owned()),
        device_id: Some("dev-1".to_owned()),
        device_name: Some("Chrome".to_owned()),
        remote_endpoint: Some("1.2.3.4".to_owned()),
        ..AuthenticationRequest::default()
    };
    let result = mgr.authenticate_direct(&request).await.unwrap();
    let session: &SessionInfoDto = &result.session;
    assert_eq!(session.user_id, user_id);
    assert_eq!(session.server_id.as_deref(), Some("server-1"));

    // The result carries a non-empty minted access token (the bug: it used to be
    // dropped, leaving the API's `AccessToken` null).
    assert!(
        !result.access_token.expose().is_empty(),
        "authenticate returns a non-empty access token"
    );

    // A device row (with an access token) now exists for the user, and it is the
    // *same* token the result returned.
    let persisted: String =
        sqlx::query_scalar(r#"SELECT "AccessToken" FROM "Devices" WHERE "DeviceId" = ?1"#)
            .bind("dev-1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(!persisted.is_empty());
    assert_eq!(
        result.access_token.expose(),
        persisted.as_str(),
        "the returned token equals the persisted Devices.AccessToken"
    );

    // The freshly minted token resolves back to a session.
    let resolved = mgr
        .get_session_by_authentication_token(result.access_token.expose(), "dev-1", "1.2.3.4")
        .await
        .unwrap();
    assert_eq!(resolved.user_id, user_id);
}

#[tokio::test]
async fn authenticate_new_session_enforces_password_and_returns_the_token() {
    // The interactive path (`authenticate_new_session`) runs the password check
    // (empty password authenticates a passwordless user) and must return the same
    // minted token it persists — the fix that lets the API echo `AccessToken`.
    let db = test_db().await;
    let mgr = manager(&db);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "bob").await;
    set_permission(
        db.writer(),
        &user.id,
        PermissionKind::EnableAllDevices,
        true,
    )
    .await
    .unwrap();

    let request = AuthenticationRequest {
        username: Some("bob".to_owned()),
        password: Some(Secret::new("")),
        app: Some("Web".to_owned()),
        app_version: Some("1.0".to_owned()),
        device_id: Some("dev-9".to_owned()),
        device_name: Some("Firefox".to_owned()),
        remote_endpoint: Some("1.2.3.4".to_owned()),
        ..AuthenticationRequest::default()
    };
    let result = mgr.authenticate_new_session(&request).await.unwrap();
    assert_eq!(result.session.user_id, user_id);
    assert!(
        !result.access_token.expose().is_empty(),
        "authenticate_new_session returns a non-empty token"
    );

    let persisted: String =
        sqlx::query_scalar(r#"SELECT "AccessToken" FROM "Devices" WHERE "DeviceId" = ?1"#)
            .bind("dev-9")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(result.access_token.expose(), persisted.as_str());

    // The token authenticates future requests.
    let resolved = mgr
        .get_session_by_authentication_token(result.access_token.expose(), "dev-9", "1.2.3.4")
        .await
        .unwrap();
    assert_eq!(resolved.user_id, user_id);
}

#[tokio::test]
async fn authenticate_requires_the_mandatory_fields() {
    let db = test_db().await;
    let mgr = manager(&db);
    let err = mgr
        .authenticate_direct(&AuthenticationRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::InvalidInput(_)));
}

#[tokio::test]
async fn authenticate_unknown_user_is_unauthorized() {
    let db = test_db().await;
    let mgr = manager(&db);
    let request = AuthenticationRequest {
        username: Some("ghost".to_owned()),
        app: Some("Web".to_owned()),
        app_version: Some("1.0".to_owned()),
        device_id: Some("dev-1".to_owned()),
        device_name: Some("Chrome".to_owned()),
        ..AuthenticationRequest::default()
    };
    let err = mgr.authenticate_direct(&request).await.unwrap_err();
    assert!(matches!(err, ServiceError::Unauthorized(_)));
}

#[tokio::test]
async fn logout_by_token_ends_the_session() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "alice").await;
    set_permission(
        db.writer(),
        &user.id,
        PermissionKind::EnableAllDevices,
        true,
    )
    .await
    .unwrap();
    let request = AuthenticationRequest {
        user_id: Some(user_id),
        app: Some("Web".to_owned()),
        app_version: Some("1.0".to_owned()),
        device_id: Some("dev-1".to_owned()),
        device_name: Some("Chrome".to_owned()),
        ..AuthenticationRequest::default()
    };
    let token = mgr
        .authenticate_direct(&request)
        .await
        .unwrap()
        .access_token;

    mgr.logout(token.expose()).await.unwrap();

    // The device row is gone and the token no longer resolves.
    let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Devices""#)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    let err = mgr
        .get_session_by_authentication_token(token.expose(), "dev-1", "e")
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::Unauthorized(_)));
}

#[tokio::test]
async fn get_sessions_non_admin_sees_only_own() {
    let db = test_db().await;
    let mgr = manager(&db);
    let alice_id = Uuid::new_v4();
    let bob_id = Uuid::new_v4();
    let alice = seed_named_user(&db, alice_id, "alice").await;
    let bob = seed_named_user(&db, bob_id, "bob").await;
    mgr.log_session_activity("Web", "1.0", "a", "A", "e", &alice)
        .await
        .unwrap();
    mgr.log_session_activity("Web", "1.0", "b", "B", "e", &bob)
        .await
        .unwrap();

    // Alice (non-admin) sees only her own session.
    let sessions = mgr
        .get_sessions(alice_id, None, None, None, false)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].user_id, alice_id);
}

#[tokio::test]
async fn get_sessions_admin_sees_all() {
    let db = test_db().await;
    let mgr = manager(&db);
    let admin_id = Uuid::new_v4();
    let admin = seed_named_user(&db, admin_id, "admin").await;
    set_permission(
        db.writer(),
        &admin.id,
        PermissionKind::IsAdministrator,
        true,
    )
    .await
    .unwrap();
    let other = seed_named_user(&db, Uuid::new_v4(), "other").await;
    mgr.log_session_activity("Web", "1.0", "a", "A", "e", &admin)
        .await
        .unwrap();
    mgr.log_session_activity("Web", "1.0", "b", "B", "e", &other)
        .await
        .unwrap();

    let sessions = mgr
        .get_sessions(admin_id, None, None, None, false)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 2);
}

#[tokio::test]
async fn broadcast_pushes_to_attached_connections() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "alice").await;
    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", &user)
        .await
        .unwrap();
    let session_id = dto.id.unwrap();

    let conn = FakeConnection::new(AuthorizationInfo::default());
    mgr.add_web_socket(&session_id, conn.clone() as Arc<dyn WebSocketConnection>)
        .await
        .unwrap();

    mgr.send_message_to_user_sessions(&[user_id], SessionMessageType::RestartRequired, "")
        .await
        .unwrap();
    assert_eq!(conn.sent_count(), 1);
    let frame: serde_json::Value = serde_json::from_slice(&conn.sent.lock().unwrap()[0]).unwrap();
    assert_eq!(frame["MessageType"], "RestartRequired");
}

// Progress reported from one device must be pushed (`UserDataChanged`) to the
// same user's OTHER sessions — that's what keeps resume positions in sync when
// jumping between devices — and must not leak to other users' sessions.
#[tokio::test]
async fn playback_progress_pushes_user_data_changed_to_the_users_other_sessions() {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::session::PlaybackProgressInfo;

    let db = test_db().await;
    let mgr = manager(&db);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "shared").await;
    let other_id = Uuid::new_v4();
    let other = seed_named_user(&db, other_id, "other").await;
    let item_id = Uuid::new_v4();
    crate::test_support::seed_item(&db, item_id, BaseItemKind::Movie).await;

    // The same user on two devices, plus an unrelated user's session.
    let tv = mgr
        .log_session_activity("wolphin", "1.0", "dev-tv", "Shield", "e", &user)
        .await
        .unwrap();
    let web = mgr
        .log_session_activity("Jellyfin Web", "1.0", "dev-web", "Mac", "e", &user)
        .await
        .unwrap();
    let other_session = mgr
        .log_session_activity("Web", "1.0", "dev-o", "PC", "e", &other)
        .await
        .unwrap();

    let web_conn = FakeConnection::new(AuthorizationInfo::default());
    mgr.add_web_socket(
        &web.id.unwrap(),
        web_conn.clone() as Arc<dyn WebSocketConnection>,
    )
    .await
    .unwrap();
    let other_conn = FakeConnection::new(AuthorizationInfo::default());
    mgr.add_web_socket(
        &other_session.id.unwrap(),
        other_conn.clone() as Arc<dyn WebSocketConnection>,
    )
    .await
    .unwrap();

    let info = PlaybackProgressInfo {
        session_id: tv.id.clone(),
        item_id,
        position_ticks: Some(1000),
        ..PlaybackProgressInfo::default()
    };
    mgr.on_playback_progress(&info, false).await.unwrap();

    // The same user's other device received the play-state push …
    assert_eq!(web_conn.sent_count(), 1);
    let frame: serde_json::Value =
        serde_json::from_slice(&web_conn.sent.lock().unwrap()[0]).unwrap();
    assert_eq!(frame["MessageType"], "UserDataChanged");
    assert_eq!(frame["Data"]["UserId"], user_id.simple().to_string());
    assert_eq!(
        frame["Data"]["UserDataList"][0]["ItemId"],
        item_id.simple().to_string()
    );
    // … and the unrelated user's session did not.
    assert_eq!(other_conn.sent_count(), 0);
}

#[tokio::test]
async fn send_message_command_accepts_any_existing_session() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", &user)
        .await
        .unwrap();
    let session_id = dto.id.unwrap();

    let command = MessageCommand {
        text: "hi".to_owned(),
        ..MessageCommand::default()
    };
    // Jellyfin's SendMessageToSession hands the message to whatever controllers the
    // session has (here none → a no-op) and returns success — it does NOT gate on
    // remote-control support or an open connection.
    mgr.send_message_command("", &session_id, &command)
        .await
        .unwrap();

    // A command to a session that does not exist is a NotFound (C#
    // GetSessionToRemoteControl → ResourceNotFoundException → 404).
    let err = mgr
        .send_message_command("", &Uuid::new_v4().to_string(), &command)
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::NotFound(_)));
}

#[tokio::test]
async fn report_session_ended_removes_the_session() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", &user)
        .await
        .unwrap();
    let session_id = dto.id.unwrap();

    mgr.report_session_ended(&session_id).await.unwrap();
    let sessions = mgr
        .get_sessions(Uuid::nil(), None, None, None, true)
        .await
        .unwrap();
    assert!(sessions.is_empty());
}

/// A [`UserDataManager`](ferrofin_traits::library::UserDataManager) decorator
/// that counts the DTO reads the push path makes, delegating the rest to the
/// real manager.
struct CountingUserData {
    inner: Arc<dyn ferrofin_traits::library::UserDataManager>,
    dto_reads: std::sync::atomic::AtomicUsize,
    play_state_writes: std::sync::atomic::AtomicUsize,
}

impl CountingUserData {
    fn new(inner: Arc<dyn ferrofin_traits::library::UserDataManager>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            dto_reads: std::sync::atomic::AtomicUsize::new(0),
            play_state_writes: std::sync::atomic::AtomicUsize::new(0),
        })
    }
    fn dto_reads(&self) -> usize {
        self.dto_reads.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn play_state_writes(&self) -> usize {
        self.play_state_writes
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl ferrofin_traits::library::UserDataManager for CountingUserData {
    async fn save_user_data(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        user_data: &ferrofin_model::dto::UpdateUserItemDataDto,
    ) -> Result<(), ServiceError> {
        self.inner.save_user_data(user_id, item_id, user_data).await
    }
    async fn get_user_data_dto(
        &self,
        item_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<ferrofin_model::dto::UserItemDataDto>, ServiceError> {
        self.dto_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get_user_data_dto(item_id, user_id).await
    }
    async fn get_user_data_batch(
        &self,
        item_ids: &[Uuid],
        user_id: Uuid,
    ) -> Result<std::collections::HashMap<Uuid, ferrofin_model::dto::UserItemDataDto>, ServiceError>
    {
        self.inner.get_user_data_batch(item_ids, user_id).await
    }
    async fn update_play_state(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        reported_position_ticks: Option<i64>,
    ) -> Result<bool, ServiceError> {
        self.play_state_writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner
            .update_play_state(user_id, item_id, reported_position_ticks)
            .await
    }
    async fn mark_played(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        date_played: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<ferrofin_model::dto::UserItemDataDto, ServiceError> {
        self.inner.mark_played(user_id, item_id, date_played).await
    }
    async fn mark_unplayed(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<ferrofin_model::dto::UserItemDataDto, ServiceError> {
        self.inner.mark_unplayed(user_id, item_id).await
    }
    async fn reset_playback_stream_selections(
        &self,
        user_id: Uuid,
        item_id: Uuid,
    ) -> Result<(), ServiceError> {
        self.inner
            .reset_playback_stream_selections(user_id, item_id)
            .await
    }
}

/// Builds a session manager whose user-data manager is the counting decorator.
fn manager_counting(
    db: &Database,
    user_data: Arc<CountingUserData>,
) -> Arc<FerrofinSessionManager> {
    let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
        Arc::new(ItemTypeLookup::new());
    let library = Arc::new(FerrofinLibraryManager::new(
        Arc::new(FerrofinItemRepository::new(db.clone(), lookup)),
        Arc::new(FerrofinItemCountService::new(db.clone())),
        Arc::new(FerrofinItemPersistenceService::new(db.clone())),
        Arc::new(FerrofinPeopleRepository::new(db.clone())),
    ));
    Arc::new(FerrofinSessionManager::new(
        Arc::new(FerrofinUserManager::new(db.clone())),
        Arc::new(FerrofinDeviceManager::new(db.clone())),
        user_data,
        library,
        Arc::new(UnusedDtoService),
        Arc::new(FerrofinEventManager::new()),
        db.clone(),
        "server-1".to_owned(),
    ))
}

// A progress report still persists play state when the user has no session that
// could receive the `UserDataChanged` push — but it must not pay the extra
// `UserData` read to build a message nobody can receive. Playstate progress is
// the hottest write in the server, so that read is per-report waste.
#[tokio::test]
async fn progress_without_a_listening_session_skips_the_push_read() {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::session::PlaybackProgressInfo;

    let db = test_db().await;
    let config: Arc<dyn ServerConfigurationManager> = Arc::new(FixedConfig {
        config: default_server_configuration(),
    });
    let counting =
        CountingUserData::new(Arc::new(FerrofinUserDataManager::new(db.clone(), config)));
    let mgr = manager_counting(&db, counting.clone());

    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "solo").await;
    let item_id = Uuid::new_v4();
    crate::test_support::seed_item(&db, item_id, BaseItemKind::Movie).await;
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-solo", "TV", "e", &user)
        .await
        .unwrap();
    let info = PlaybackProgressInfo {
        session_id: session.id.clone(),
        item_id,
        position_ticks: Some(1000),
        ..PlaybackProgressInfo::default()
    };

    // No WebSocket, no bus sink: the push would reach nobody.
    mgr.on_playback_progress(&info, false).await.unwrap();
    assert_eq!(counting.play_state_writes(), 1, "the write still happens");
    assert_eq!(counting.dto_reads(), 0, "no push means no read for it");

    // Attach a socket to that same session: the push is deliverable again and
    // the read is paid for exactly one message.
    let conn = FakeConnection::new(AuthorizationInfo::default());
    mgr.add_web_socket(
        session.id.as_deref().unwrap(),
        conn.clone() as Arc<dyn WebSocketConnection>,
    )
    .await
    .unwrap();
    mgr.on_playback_progress(&info, false).await.unwrap();
    assert_eq!(counting.play_state_writes(), 2);
    assert_eq!(counting.dto_reads(), 1);
    assert_eq!(conn.sent_count(), 1);
}

/// A media-source manager that records every `close_live_stream` id. Only the
/// live-stream half of the trait is reachable from the session manager.
struct RecordingMediaSources {
    closed: Mutex<Vec<String>>,
}

#[async_trait]
impl ferrofin_traits::library::MediaSourceManager for RecordingMediaSources {
    async fn get_media_streams(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::entities_media::MediaStream>, ServiceError> {
        unreachable!("not reached from the session manager")
    }
    async fn get_media_attachments(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::entities_media::MediaAttachment>, ServiceError> {
        unreachable!("not reached from the session manager")
    }
    async fn get_playback_media_sources(
        &self,
        _item_id: Uuid,
        _user_id: Uuid,
        _allow_media_probe: bool,
        _enable_path_substitution: bool,
    ) -> Result<Vec<ferrofin_model::dto::MediaSourceInfo>, ServiceError> {
        unreachable!("not reached from the session manager")
    }
    async fn get_static_media_sources(
        &self,
        _item_id: Uuid,
        _enable_path_substitution: bool,
        _user_id: Option<Uuid>,
    ) -> Result<Vec<ferrofin_model::dto::MediaSourceInfo>, ServiceError> {
        unreachable!("not reached from the session manager")
    }
    async fn open_live_stream(
        &self,
        _request: &ferrofin_model::media_info::LiveStreamRequest,
    ) -> Result<ferrofin_model::dto::MediaSourceInfo, ServiceError> {
        unreachable!("not reached from the session manager")
    }
    async fn get_live_stream(
        &self,
        _id: &str,
    ) -> Result<ferrofin_model::dto::MediaSourceInfo, ServiceError> {
        unreachable!("not reached from the session manager")
    }
    async fn close_live_stream(&self, id: &str) -> Result<(), ServiceError> {
        self.closed
            .lock()
            .expect("closed mutex")
            .push(id.to_owned());
        Ok(())
    }
    async fn refresh_media_streams(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        unreachable!("not reached from the session manager")
    }
}

/// A stopped playback closes the live stream it names (C# `OnPlaybackStopped` →
/// `CloseLiveStreamIfNeededAsync`). Without it the media source manager's
/// open-stream table only ever shrinks on an explicit `/LiveStreams/Close`, so a
/// client that stops playing and goes away leaks its `MediaSourceInfo` forever.
#[tokio::test]
async fn stopping_playback_closes_the_reported_live_stream() {
    use ferrofin_model::session::PlaybackStopInfo;

    let db = test_db().await;
    let sources = Arc::new(RecordingMediaSources {
        closed: Mutex::new(Vec::new()),
    });
    let mgr = Arc::new(
        Arc::try_unwrap(manager(&db))
            .expect("sole owner")
            .with_media_sources(
                Arc::clone(&sources) as Arc<dyn ferrofin_traits::library::MediaSourceManager>
            ),
    );
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-live", "TV", "e", &user)
        .await
        .unwrap();

    // No live stream named: nothing to close.
    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: session.id.clone(),
        item_id: Uuid::nil(),
        ..PlaybackStopInfo::default()
    })
    .await
    .unwrap();
    assert!(
        sources.closed.lock().expect("closed mutex").is_empty(),
        "a report without a live stream id closes nothing"
    );

    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: session.id.clone(),
        item_id: Uuid::nil(),
        live_stream_id: Some("live-42".to_owned()),
        ..PlaybackStopInfo::default()
    })
    .await
    .unwrap();
    assert_eq!(
        *sources.closed.lock().expect("closed mutex"),
        vec!["live-42".to_owned()],
        "the reported live stream is closed exactly once"
    );
}

/// A session recreated after its previous one ended (the socket closed, or an
/// explicit logout) inherits the device's last reported capabilities — C#
/// `OnSessionStarted` → `ReportCapabilities(info, _deviceManager.GetCapabilities
/// (deviceId), saveCapabilities: false)`. This is what makes ending a session on
/// socket close safe: the client stays remote-controllable without having to
/// re-post `/Sessions/Capabilities/Full`, and it is why the device manager's
/// capabilities map is never evicted.
#[tokio::test]
async fn a_recreated_session_inherits_the_devices_capabilities() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", &user)
        .await
        .unwrap();
    let session_id = dto.id.clone().unwrap();
    assert!(
        !dto.supports_media_control,
        "nothing reported yet, so the first session starts with defaults"
    );

    mgr.report_capabilities(
        &session_id,
        &ClientCapabilities {
            supports_media_control: true,
            ..ClientCapabilities::default()
        },
    )
    .await
    .unwrap();

    // The socket closed: the session leaves the pool (C# `CloseIfNeededAsync`).
    mgr.report_session_ended(&session_id).await.unwrap();
    assert!(
        mgr.get_sessions(Uuid::nil(), None, None, None, true)
            .await
            .unwrap()
            .is_empty(),
        "ending the session removes it from the pool"
    );

    // The client's next request recreates it — same id, capabilities intact.
    let again = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", &user)
        .await
        .unwrap();
    assert_eq!(again.id.as_deref(), Some(session_id.as_str()));
    assert!(
        again.supports_media_control,
        "the recreated session inherits the device's reported capabilities"
    );
}

// ── cast play translation (C# `SendPlayCommand`) ───────────────────────────

/// A music manager returning a canned instant mix, so the `PlayInstantMix`
/// translation is testable without the real recommendation logic.
struct CannedMix {
    items: Vec<Uuid>,
}

#[async_trait]
impl ferrofin_traits::library::MusicManager for CannedMix {
    async fn get_instant_mix_from_item(
        &self,
        _item_id: Uuid,
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(self
            .items
            .iter()
            .map(|id| BaseItemEntity {
                id: guid_to_db(*id),
                ..BaseItemEntity::default()
            })
            .collect())
    }
    async fn get_instant_mix_from_artist(
        &self,
        _artist_id: Uuid,
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_instant_mix_from_genres(
        &self,
        _genres: &[String],
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(Vec::new())
    }
}

/// A bus-wired manager plus that bus — the shape every cast test needs.
fn cast_manager(
    db: &Database,
) -> (
    Arc<FerrofinSessionManager>,
    Arc<dyn ferrofin_traits::session_bus::SessionMessageBus>,
) {
    let bus: Arc<dyn ferrofin_traits::session_bus::SessionMessageBus> =
        Arc::new(crate::FerrofinSessionMessageBus::new());
    let mgr = Arc::new(
        manager(db)
            .as_ref()
            .clone()
            .with_session_bus(Arc::clone(&bus)),
    );
    (mgr, bus)
}

/// Opens a bus-connected session for `user`, returning `(session id, its pushes)`.
async fn cast_target(
    mgr: &FerrofinSessionManager,
    bus: &dyn ferrofin_traits::session_bus::SessionMessageBus,
    user: &UserEntity,
    device_id: &str,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let session_id = mgr
        .log_session_activity("Web", "1.0", device_id, "TV", "e", user)
        .await
        .unwrap()
        .id
        .unwrap();
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&received);
    bus.register(
        session_id.clone(),
        Box::new(move |msg| sink.lock().unwrap().push(msg)),
    );
    (session_id, received)
}

/// The `Data` object of the single message pushed to a cast target.
fn only_pushed_data(received: &Arc<Mutex<Vec<String>>>) -> serde_json::Value {
    let messages = received.lock().unwrap();
    assert_eq!(messages.len(), 1, "expected exactly one push: {messages:?}");
    let envelope: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
    envelope["Data"].clone()
}

/// The `ItemIds` of a pushed `Play`, as `Uuid`s.
fn pushed_item_ids(data: &serde_json::Value) -> Vec<Uuid> {
    data["ItemIds"]
        .as_array()
        .expect("ItemIds array")
        .iter()
        .map(|v| Uuid::parse_str(v.as_str().unwrap()).unwrap())
        .collect()
}

/// Records `child` as a descendant of `parent` — the row the recursive
/// (`AncestorIds`) child query joins through.
async fn seed_ancestor(db: &Database, child: Uuid, parent: Uuid) {
    // Through the production writer, so the fixture cannot drift from how the
    // scanner actually registers a descendant.
    FerrofinItemPersistenceService::new(db.clone())
        .set_ancestors(child, &[parent])
        .await
        .expect("set ancestors");
}

/// Sets an item's `SortName` through the production writer — the fixture insert
/// leaves it NULL, which makes a `SortName` ordering assertion meaningless.
async fn set_sort_name(db: &Database, id: Uuid, sort_name: &str) {
    let library = library_manager(db);
    let mut row = library
        .get_item_by_id(id)
        .await
        .expect("load item")
        .expect("item present");
    row.sort_name = Some(sort_name.to_owned());
    library
        .update_items(std::slice::from_ref(&row), None)
        .await
        .expect("set sort name");
}

/// Grants the playback permission `GetPlayAccess` gates every cast on.
async fn allow_playback(db: &Database, user: &UserEntity) {
    set_permission(
        db.writer(),
        &user.id,
        PermissionKind::EnableMediaPlayback,
        true,
    )
    .await
    .unwrap();
}

/// A `PlayNow` cast of exactly these ids.
fn play_now(item_ids: Vec<Uuid>) -> ferrofin_model::session::PlayRequest {
    ferrofin_model::session::PlayRequest {
        item_ids,
        play_command: ferrofin_model::session::PlayCommand::PlayNow,
        ..ferrofin_model::session::PlayRequest::default()
    }
}

#[tokio::test]
async fn casting_a_folder_expands_to_its_playable_children() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    allow_playback(&db, &user).await;

    // A series with two real episodes and one virtual (missing) one.
    let series = Uuid::new_v4();
    crate::test_support::seed_folder_item(
        &db,
        series,
        ferrofin_model::data::BaseItemKind::Series,
        "Show",
        None,
    )
    .await;
    let ep_a = Uuid::new_v4();
    let ep_b = Uuid::new_v4();
    let ep_missing = Uuid::new_v4();
    crate::test_support::seed_named_item(
        &db,
        ep_a,
        ferrofin_model::data::BaseItemKind::Episode,
        "A Episode",
    )
    .await;
    crate::test_support::seed_named_item(
        &db,
        ep_b,
        ferrofin_model::data::BaseItemKind::Episode,
        "B Episode",
    )
    .await;
    crate::test_support::seed_episode(&db, ep_missing, "k", 1, 3, true, None).await;
    for child in [ep_a, ep_b, ep_missing] {
        seed_ancestor(&db, child, series).await;
    }

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command("", &session_id, &play_now(vec![series]))
        .await
        .unwrap();

    assert_eq!(
        pushed_item_ids(&only_pushed_data(&received)),
        vec![ep_a, ep_b],
        "the series expands to its real episodes in SortName order, minus the virtual one"
    );
}

#[tokio::test]
async fn casting_a_box_set_expands_to_its_linked_members() {
    use crate::linked_children_service::FerrofinLinkedChildrenService;
    use ferrofin_traits::persistence::LinkedChildrenService;

    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    allow_playback(&db, &user).await;

    // A box set's membership is a manual LinkedChildren edge, NOT the physical
    // `AncestorIds` closure the recursive folder query walks — so expanding it
    // the folder way yields nothing and the cast silently plays an empty queue.
    let boxset = Uuid::new_v4();
    crate::test_support::seed_folder_item(
        &db,
        boxset,
        ferrofin_model::data::BaseItemKind::BoxSet,
        "Trilogy",
        None,
    )
    .await;
    let links = FerrofinLinkedChildrenService::new(db.clone());
    let mut members = Vec::new();
    for name in ["A Part", "B Part"] {
        let id = Uuid::new_v4();
        crate::test_support::seed_named_item(
            &db,
            id,
            ferrofin_model::data::BaseItemKind::Movie,
            name,
        )
        .await;
        set_sort_name(&db, id, name).await;
        links
            .upsert_linked_child(boxset, id, 0)
            .await
            .expect("link");
        members.push(id);
    }

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command("", &session_id, &play_now(vec![boxset]))
        .await
        .unwrap();

    assert_eq!(
        pushed_item_ids(&only_pushed_data(&received)),
        members,
        "the box set expands to its linked members, in SortName order"
    );
}

#[tokio::test]
async fn casting_a_playlist_expands_through_a_nested_box_set() {
    use crate::linked_children_service::FerrofinLinkedChildrenService;
    use ferrofin_traits::persistence::LinkedChildrenService;

    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    allow_playback(&db, &user).await;

    let playlist = Uuid::new_v4();
    let boxset = Uuid::new_v4();
    for (id, kind, name) in [
        (
            playlist,
            ferrofin_model::data::BaseItemKind::Playlist,
            "Mix",
        ),
        (
            boxset,
            ferrofin_model::data::BaseItemKind::BoxSet,
            "Trilogy",
        ),
    ] {
        crate::test_support::seed_folder_item(&db, id, kind, name, None).await;
    }
    let movie = Uuid::new_v4();
    crate::test_support::seed_named_item(
        &db,
        movie,
        ferrofin_model::data::BaseItemKind::Movie,
        "Part One",
    )
    .await;

    let links = FerrofinLinkedChildrenService::new(db.clone());
    links
        .upsert_linked_child(playlist, boxset, 0)
        .await
        .expect("link");
    links
        .upsert_linked_child(boxset, movie, 0)
        .await
        .expect("link");

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command("", &session_id, &play_now(vec![playlist]))
        .await
        .unwrap();

    assert_eq!(
        pushed_item_ids(&only_pushed_data(&received)),
        vec![movie],
        "nesting is flattened, not left as a container id the client cannot play"
    );
}

#[tokio::test]
async fn a_member_reachable_twice_is_queued_once() {
    use crate::linked_children_service::FerrofinLinkedChildrenService;
    use ferrofin_traits::persistence::LinkedChildrenService;

    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    allow_playback(&db, &user).await;

    // The movie is linked into the playlist directly AND through a nested box
    // set, so a blind flatten would queue it twice.
    let playlist = Uuid::new_v4();
    let boxset = Uuid::new_v4();
    for (id, kind, name) in [
        (
            playlist,
            ferrofin_model::data::BaseItemKind::Playlist,
            "Mix",
        ),
        (
            boxset,
            ferrofin_model::data::BaseItemKind::BoxSet,
            "Trilogy",
        ),
    ] {
        crate::test_support::seed_folder_item(&db, id, kind, name, None).await;
    }
    let movie = Uuid::new_v4();
    crate::test_support::seed_named_item(
        &db,
        movie,
        ferrofin_model::data::BaseItemKind::Movie,
        "Part One",
    )
    .await;

    let links = FerrofinLinkedChildrenService::new(db.clone());
    links
        .upsert_linked_child(playlist, movie, 0)
        .await
        .expect("link");
    links
        .upsert_linked_child(playlist, boxset, 0)
        .await
        .expect("link");
    links
        .upsert_linked_child(boxset, movie, 0)
        .await
        .expect("link");

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command("", &session_id, &play_now(vec![playlist]))
        .await
        .unwrap();

    assert_eq!(
        pushed_item_ids(&only_pushed_data(&received)),
        vec![movie],
        "a member reachable by two paths appears once, as upstream's keyed accumulation gives"
    );
}

#[tokio::test]
async fn a_container_member_with_an_unparseable_id_is_skipped_not_nil() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    allow_playback(&db, &user).await;

    // Degrading the unparseable id to the nil GUID would be far worse than a
    // bad entry: `translate_query` skips the parent predicate entirely when
    // `parent_id` is nil, so the "container" would expand to the whole library.
    let boxset = Uuid::new_v4();
    crate::test_support::seed_folder_item(
        &db,
        boxset,
        ferrofin_model::data::BaseItemKind::BoxSet,
        "Trilogy",
        None,
    )
    .await;
    let good = Uuid::new_v4();
    crate::test_support::seed_named_item(
        &db,
        good,
        ferrofin_model::data::BaseItemKind::Movie,
        "Part One",
    )
    .await;
    {
        use ferrofin_traits::persistence::LinkedChildrenService;
        crate::linked_children_service::FerrofinLinkedChildrenService::new(db.clone())
            .upsert_linked_child(boxset, good, 0)
            .await
            .expect("link");
    }
    crate::test_support::seed_child_with_raw_id(
        &db,
        "not-a-guid",
        ferrofin_model::data::BaseItemKind::Movie,
        boxset,
    )
    .await;
    // An unrelated item that must NOT appear: its presence would mean the
    // parent scope was dropped.
    let elsewhere = Uuid::new_v4();
    crate::test_support::seed_named_item(
        &db,
        elsewhere,
        ferrofin_model::data::BaseItemKind::Movie,
        "Unrelated",
    )
    .await;

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command("", &session_id, &play_now(vec![boxset]))
        .await
        .unwrap();

    assert_eq!(
        pushed_item_ids(&only_pushed_data(&received)),
        vec![good],
        "the corrupt row is dropped and the scope holds — no nil GUID, no library-wide queue"
    );
}

#[tokio::test]
async fn casting_a_plain_item_passes_through_unexpanded() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    allow_playback(&db, &user).await;

    let movie = Uuid::new_v4();
    crate::test_support::seed_named_item(
        &db,
        movie,
        ferrofin_model::data::BaseItemKind::Movie,
        "Movie",
    )
    .await;

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command(
        "",
        &session_id,
        &ferrofin_model::session::PlayRequest {
            play_command: ferrofin_model::session::PlayCommand::PlayNext,
            ..play_now(vec![movie])
        },
    )
    .await
    .unwrap();

    let data = only_pushed_data(&received);
    assert_eq!(pushed_item_ids(&data), vec![movie]);
    assert_eq!(
        data["PlayCommand"], "PlayNext",
        "a queue command is preserved, not rewritten"
    );
}

#[tokio::test]
async fn casting_a_genre_expands_to_the_items_tagged_with_it() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    allow_playback(&db, &user).await;

    // A genre is stored as a by-name folder row; tagged items reference it.
    let genre = Uuid::new_v4();
    crate::test_support::seed_folder_item(
        &db,
        genre,
        ferrofin_model::data::BaseItemKind::Genre,
        "Jazz",
        None,
    )
    .await;
    let track = Uuid::new_v4();
    crate::test_support::seed_named_item(
        &db,
        track,
        ferrofin_model::data::BaseItemKind::Audio,
        "Track",
    )
    .await;
    crate::test_support::seed_item_genre(&db, track, "Jazz").await;
    // The by-name filter joins the item's `ItemValues.CleanValue` to the genre
    // row's `CleanName`, which the scanner writes and the fixture must too.
    crate::test_support::set_clean_name(&db, genre, "jazz").await;

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command("", &session_id, &play_now(vec![genre]))
        .await
        .unwrap();

    assert_eq!(
        pushed_item_ids(&only_pushed_data(&received)),
        vec![track],
        "the genre expands to the tagged track, not to itself"
    );
}

#[tokio::test]
async fn casting_a_nonexistent_id_contributes_nothing() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command("", &session_id, &play_now(vec![Uuid::new_v4()]))
        .await
        .unwrap();

    // C# logs and drops the id rather than failing the whole command.
    assert!(pushed_item_ids(&only_pushed_data(&received)).is_empty());
}

#[tokio::test]
async fn play_shuffle_becomes_play_now_over_the_expanded_list() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    allow_playback(&db, &user).await;

    let album = Uuid::new_v4();
    crate::test_support::seed_folder_item(
        &db,
        album,
        ferrofin_model::data::BaseItemKind::MusicAlbum,
        "Album",
        None,
    )
    .await;
    let mut tracks = Vec::new();
    for n in 0..8 {
        let id = Uuid::new_v4();
        crate::test_support::seed_named_item(
            &db,
            id,
            ferrofin_model::data::BaseItemKind::Audio,
            &format!("Track {n}"),
        )
        .await;
        seed_ancestor(&db, id, album).await;
        tracks.push(id);
    }

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command(
        "",
        &session_id,
        &ferrofin_model::session::PlayRequest {
            play_command: ferrofin_model::session::PlayCommand::PlayShuffle,
            ..play_now(vec![album])
        },
    )
    .await
    .unwrap();

    let data = only_pushed_data(&received);
    assert_eq!(
        data["PlayCommand"], "PlayNow",
        "the client never sees an unresolved PlayShuffle"
    );
    let mut ids = pushed_item_ids(&data);
    let mut expected = tracks.clone();
    ids.sort();
    expected.sort();
    assert_eq!(
        ids, expected,
        "shuffling permutes the queue, it does not drop from it"
    );
}

#[tokio::test]
async fn play_instant_mix_becomes_play_now_over_the_mix() {
    let db = test_db().await;
    let bus: Arc<dyn ferrofin_traits::session_bus::SessionMessageBus> =
        Arc::new(crate::FerrofinSessionMessageBus::new());
    let mix: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
    let mgr = Arc::new(
        manager(&db)
            .as_ref()
            .clone()
            .with_session_bus(Arc::clone(&bus))
            .with_music_manager(Arc::new(CannedMix { items: mix.clone() })),
    );
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    allow_playback(&db, &user).await;

    let seed = Uuid::new_v4();
    crate::test_support::seed_named_item(
        &db,
        seed,
        ferrofin_model::data::BaseItemKind::Audio,
        "Seed",
    )
    .await;

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command(
        "",
        &session_id,
        &ferrofin_model::session::PlayRequest {
            play_command: ferrofin_model::session::PlayCommand::PlayInstantMix,
            ..play_now(vec![seed])
        },
    )
    .await
    .unwrap();

    let data = only_pushed_data(&received);
    assert_eq!(data["PlayCommand"], "PlayNow");
    assert_eq!(pushed_item_ids(&data), mix);
}

#[tokio::test]
async fn instant_mix_without_a_music_manager_plays_the_seed_item() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    allow_playback(&db, &user).await;

    let seed = Uuid::new_v4();
    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command(
        "",
        &session_id,
        &ferrofin_model::session::PlayRequest {
            play_command: ferrofin_model::session::PlayCommand::PlayInstantMix,
            ..play_now(vec![seed])
        },
    )
    .await
    .unwrap();

    let data = only_pushed_data(&received);
    assert_eq!(data["PlayCommand"], "PlayNow");
    assert_eq!(
        pushed_item_ids(&data),
        vec![seed],
        "the cast degrades to the seed item rather than failing"
    );
}

#[tokio::test]
async fn casting_stamps_the_controlling_user_on_play_and_playstate() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let controller_id = Uuid::new_v4();
    let controller = seed_named_user(&db, controller_id, "controller").await;
    let target_user = seed_named_user(&db, Uuid::new_v4(), "target").await;
    allow_playback(&db, &target_user).await;

    let controlling_session = mgr
        .log_session_activity("Web", "1.0", "dev-controller", "Phone", "e", &controller)
        .await
        .unwrap()
        .id
        .unwrap();
    let (target_session, received) =
        cast_target(&mgr, bus.as_ref(), &target_user, "dev-target").await;

    let movie = Uuid::new_v4();
    crate::test_support::seed_named_item(
        &db,
        movie,
        ferrofin_model::data::BaseItemKind::Movie,
        "Movie",
    )
    .await;
    mgr.send_play_command(
        &controlling_session,
        &target_session,
        &play_now(vec![movie]),
    )
    .await
    .unwrap();
    assert_eq!(
        only_pushed_data(&received)["ControllingUserId"],
        serde_json::json!(controller_id.simple().to_string()),
        "the target learns who cast to it"
    );

    received.lock().unwrap().clear();
    mgr.send_playstate_command(
        &controlling_session,
        &target_session,
        &ferrofin_model::session::PlaystateRequest {
            command: ferrofin_model::session::PlaystateCommand::Pause,
            ..ferrofin_model::session::PlaystateRequest::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        only_pushed_data(&received)["ControllingUserId"],
        serde_json::json!(controller_id.simple().to_string()),
        "C# stamps the playstate command with the dashless guid form"
    );
}

#[tokio::test]
async fn casting_to_a_user_without_playback_permission_is_rejected() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    // `EnableMediaPlayback` is off for a bare seeded user.
    let user = seed_named_user(&db, Uuid::new_v4(), "noplay").await;
    let movie = Uuid::new_v4();
    crate::test_support::seed_named_item(
        &db,
        movie,
        ferrofin_model::data::BaseItemKind::Movie,
        "Movie",
    )
    .await;

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    let err = mgr
        .send_play_command("", &session_id, &play_now(vec![movie]))
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::InvalidInput(_)), "{err:?}");
    assert!(
        received.lock().unwrap().is_empty(),
        "a rejected cast pushes nothing"
    );
}

#[tokio::test]
async fn casting_one_episode_queues_the_rest_of_the_series() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let user_id = Uuid::new_v4();
    let mut user = seed_named_user(&db, user_id, "alice").await;
    allow_playback(&db, &user).await;
    // Through the real user writer, so the fixture exercises the same column
    // mapping production does.
    user.enable_next_episode_auto_play = true;
    let users = FerrofinUserManager::new(db.clone());
    users.update_user(&user).await.expect("update user");

    let series = Uuid::new_v4();
    crate::test_support::seed_folder_item(
        &db,
        series,
        ferrofin_model::data::BaseItemKind::Series,
        "Show",
        None,
    )
    .await;
    let library = library_manager(&db);
    let mut episodes = Vec::new();
    for n in 0..3 {
        let id = Uuid::new_v4();
        crate::test_support::seed_named_item(
            &db,
            id,
            ferrofin_model::data::BaseItemKind::Episode,
            &format!("Ep {n}"),
        )
        .await;
        let mut row = library
            .get_item_by_id(id)
            .await
            .expect("load episode")
            .expect("episode present");
        row.series_id = Some(guid_to_db(series));
        library
            .update_items(std::slice::from_ref(&row), Some(series))
            .await
            .expect("set series id");
        seed_ancestor(&db, id, series).await;
        episodes.push(id);
    }

    let (session_id, received) = cast_target(&mgr, bus.as_ref(), &user, "dev-cast").await;
    mgr.send_play_command("", &session_id, &play_now(vec![episodes[1]]))
        .await
        .unwrap();

    assert_eq!(
        pushed_item_ids(&only_pushed_data(&received)),
        vec![episodes[1], episodes[2]],
        "auto-play queues from the cast episode to the end of the series"
    );
}

/// `MaxActiveSessions` is a check-then-act: the count is read from the live
/// session pool, but the session it is counting for is only inserted several
/// awaits later (device row, `upsert_session`). Without the admission gate every
/// login in a concurrent burst reads the same pre-burst count and all of them
/// are admitted — a user capped at one session gets as many as the burst is
/// wide (measured over real HTTP: 23 of 24 accepted, where the sequential
/// control accepts exactly 1).
///
/// Each login uses a DISTINCT device id, so every admitted login is its own
/// session; a shared device id would collapse them into one and hide the bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_logins_cannot_exceed_max_active_sessions() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "capped").await;
    // Cap the account through the same API an admin uses, then allow the
    // devices (`update_policy` does not touch `EnableAllDevices`).
    let users: Arc<dyn ferrofin_traits::library::UserManager> =
        Arc::new(crate::user_manager::FerrofinUserManager::new(db.clone()));
    users
        .update_policy(
            user_id,
            &ferrofin_model::users::UserPolicy {
                max_active_sessions: 1,
                ..ferrofin_model::users::UserPolicy::default()
            },
        )
        .await
        .unwrap();
    set_permission(
        db.writer(),
        &user.id,
        PermissionKind::EnableAllDevices,
        true,
    )
    .await
    .unwrap();

    let logins = 16;
    let gate = Arc::new(tokio::sync::Barrier::new(logins));
    let mut tasks = Vec::new();
    for i in 0..logins {
        let mgr = Arc::clone(&mgr);
        let gate = Arc::clone(&gate);
        tasks.push(tokio::spawn(async move {
            let request = AuthenticationRequest {
                user_id: Some(user_id),
                app: Some("Web".to_owned()),
                app_version: Some("1.0".to_owned()),
                device_id: Some(format!("dev-{i}")),
                device_name: Some("Chrome".to_owned()),
                ..AuthenticationRequest::default()
            };
            gate.wait().await;
            mgr.authenticate_direct(&request).await
        }));
    }

    let mut admitted = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => admitted += 1,
            Err(ServiceError::Unauthorized(msg)) => {
                assert!(msg.contains("maximum number of sessions"), "{msg}");
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    assert_eq!(
        admitted, 1,
        "MaxActiveSessions = 1 must admit exactly one login, not {admitted}"
    );
    let live = mgr
        .get_sessions(Uuid::nil(), None, None, None, true)
        .await
        .unwrap()
        .len();
    assert_eq!(live, 1, "and exactly one session may be live afterwards");
}
