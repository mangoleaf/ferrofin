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
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username", "NormalizedUsername")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, ?2, ?3)"#,
    )
    .bind(guid_to_db(id))
    .bind(username)
    .bind(ferrofin_util::string_extensions::upper_invariant(username))
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
        .log_session_activity("Web", "1.0", "dev-1", "Chrome", "1.2.3.4", Some(&user))
        .await
        .unwrap();
    assert_eq!(first.client.as_deref(), Some("Web"));
    assert_eq!(first.device_name.as_deref(), Some("Chrome"));
    assert_eq!(first.device_id.as_deref(), Some("dev-1"));
    assert_eq!(first.server_id.as_deref(), Some("server-1"));

    // Same app+device → same session id (reused, not duplicated).
    let again = mgr
        .log_session_activity("Web", "1.1", "dev-1", "Chrome", "1.2.3.4", Some(&user))
        .await
        .unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(again.application_version.as_deref(), Some("1.1"));
}

/// `RequestHelpers.GetSession` logs an API-key caller's activity with no user:
/// the session's `UserId` is `Guid.Empty`, it has no `UserName`, and it is
/// keyed like any other (`{appName}{deviceId}`).
#[tokio::test]
async fn a_session_logged_without_a_user_has_none() {
    let db = test_db().await;
    let mgr = manager(&db);

    let session = mgr
        .log_session_activity("my-key", "12.2", "server-id", "Jellyfin Server", "e", None)
        .await
        .unwrap();
    assert_eq!(session.user_id, Uuid::nil());
    assert_eq!(session.user_name, None);
    assert_eq!(session.client.as_deref(), Some("my-key"));
    let again = mgr
        .log_session_activity("my-key", "12.2", "server-id", "Jellyfin Server", "e", None)
        .await
        .unwrap();
    assert_eq!(
        session.id, again.id,
        "the same key and device reuse the session"
    );
}

#[tokio::test]
async fn empty_device_name_falls_back_to_network_device() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "bob").await;

    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-x", "", "e", Some(&user))
        .await
        .unwrap();
    assert_eq!(dto.device_name.as_deref(), Some("Network Device"));
}

#[tokio::test]
async fn a_non_admin_sees_the_transcode_but_not_the_silicon() {
    // Upstream zeroes only the accelerator, so a user still sees that their own
    // stream is transcoding and to what. Dropping the whole object would hide
    // more than Jellyfin does.
    let db = test_db().await;
    // A non-admin only sees sessions connected to the bus, so this one has to
    // be registered for the redaction to be observable at all.
    let bus: Arc<dyn ferrofin_traits::session_bus::SessionMessageBus> =
        Arc::new(crate::FerrofinSessionMessageBus::new());
    let mgr = Arc::new(
        manager(&db)
            .as_ref()
            .clone()
            .with_session_bus(Arc::clone(&bus)),
    );
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "viewer").await;
    set_permission(
        db.writer(),
        &user.id,
        PermissionKind::EnableAllDevices,
        true,
    )
    .await
    .unwrap();
    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
        .await
        .unwrap();
    let session_id = dto.id.clone().unwrap();
    mgr.report_capabilities(
        &session_id,
        &ClientCapabilities {
            supports_media_control: true,
            ..ClientCapabilities::default()
        },
    )
    .await
    .unwrap();
    bus.register(session_id, Box::new(|_| {}));

    mgr.report_transcoding_info(
        "dev-1",
        &ferrofin_model::session::TranscodingInfo {
            video_codec: Some("h264".to_owned()),
            hardware_acceleration_type: Some(
                ferrofin_model::entities::HardwareAccelerationType::nvenc,
            ),
            ..ferrofin_model::session::TranscodingInfo::default()
        },
    )
    .await
    .unwrap();

    let admin_view = mgr
        .get_sessions(Uuid::nil(), None, None, None, true)
        .await
        .unwrap();
    let seen = admin_view
        .iter()
        .find(|s| s.id == dto.id)
        .and_then(|s| s.transcoding_info.as_ref())
        .expect("admin sees the transcode");
    assert_eq!(
        seen.hardware_acceleration_type,
        Some(ferrofin_model::entities::HardwareAccelerationType::nvenc)
    );

    let user_view = mgr
        .get_sessions(user_id, None, None, Some(user_id), false)
        .await
        .unwrap();
    let seen = user_view
        .iter()
        .find(|s| s.id == dto.id)
        .and_then(|s| s.transcoding_info.as_ref())
        .expect("the user still sees their own transcode");
    assert_eq!(seen.video_codec.as_deref(), Some("h264"));
    assert_eq!(
        seen.hardware_acceleration_type,
        Some(ferrofin_model::entities::HardwareAccelerationType::none),
        "the accelerator must be zeroed for a non-admin"
    );
}

#[tokio::test]
async fn additional_users_add_and_remove() {
    let db = test_db().await;
    let mgr = manager(&db);
    let primary = seed_named_user(&db, Uuid::new_v4(), "primary").await;
    let guest_id = Uuid::new_v4();
    let _guest = seed_named_user(&db, guest_id, "guest").await;

    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&primary))
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
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&primary))
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
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
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
    set_permission(
        db.writer(),
        &user.id,
        PermissionKind::EnableAllDevices,
        true,
    )
    .await
    .unwrap();
    let dto = mgr
        .log_session_activity("TV App", "1.0", "dev-tv", "Living Room", "e", Some(&user))
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
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
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
    assert!(matches!(
        mgr.authenticate_new_session(&request).await,
        Err(ServiceError::Forbidden(_))
    ));
    set_permission(
        db.writer(),
        &user.id,
        PermissionKind::EnableRemoteAccess,
        true,
    )
    .await
    .unwrap();
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
    mgr.log_session_activity("Web", "1.0", "a", "A", "e", Some(&alice))
        .await
        .unwrap();
    mgr.log_session_activity("Web", "1.0", "b", "B", "e", Some(&bob))
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
    mgr.log_session_activity("Web", "1.0", "a", "A", "e", Some(&admin))
        .await
        .unwrap();
    mgr.log_session_activity("Web", "1.0", "b", "B", "e", Some(&other))
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
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
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

/// [`manager`] whose user-data manager announces its saves to the manager's
/// sessions, as the composition root wires them.
fn manager_announcing_user_data(db: &Database) -> Arc<FerrofinSessionManager> {
    let config: Arc<dyn ServerConfigurationManager> = Arc::new(FixedConfig {
        config: default_server_configuration(),
    });
    let user_data = Arc::new(FerrofinUserDataManager::new(db.clone(), config));
    let mgr = Arc::new(FerrofinSessionManager::new(
        Arc::new(FerrofinUserManager::new(db.clone())),
        Arc::new(FerrofinDeviceManager::new(db.clone())),
        user_data.clone(),
        library_manager(db),
        Arc::new(UnusedDtoService),
        Arc::new(FerrofinEventManager::new()),
        db.clone(),
        "server-1".to_owned(),
    ));
    let sessions: Arc<dyn SessionManager> = mgr.clone();
    user_data.set_user_data_changed_sink(Arc::new(crate::user_data_manager::SessionUserDataSink(
        Arc::downgrade(&sessions),
    )));
    mgr
}

// A stop reported from one device reaches the same user's OTHER sessions as
// `UserDataChanged`, which keeps resume positions in sync between devices, and
// never another user's. A progress report is not announced, as upstream's
// `UserDataChangeNotifier` skips `PlaybackProgress` saves.
#[tokio::test]
async fn playback_stop_pushes_user_data_changed_to_the_users_other_sessions_and_progress_does_not()
{
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::session::PlaybackProgressInfo;

    let db = test_db().await;
    let mgr = manager_announcing_user_data(&db);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "shared").await;
    let other_id = Uuid::new_v4();
    let other = seed_named_user(&db, other_id, "other").await;
    let item_id = Uuid::new_v4();
    crate::test_support::seed_item(&db, item_id, BaseItemKind::Movie).await;

    // The same user on two devices, plus an unrelated user's session.
    let tv = mgr
        .log_session_activity("wolphin", "1.0", "dev-tv", "Shield", "e", Some(&user))
        .await
        .unwrap();
    let web = mgr
        .log_session_activity("Jellyfin Web", "1.0", "dev-web", "Mac", "e", Some(&user))
        .await
        .unwrap();
    let other_session = mgr
        .log_session_activity("Web", "1.0", "dev-o", "PC", "e", Some(&other))
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
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    // A progress report is never announced (`UserDataChangeNotifier`) …
    assert_eq!(web_conn.sent_count(), 0);

    let stop = ferrofin_model::session::PlaybackStopInfo {
        session_id: tv.id.clone(),
        item_id,
        position_ticks: Some(2000),
        ..ferrofin_model::session::PlaybackStopInfo::default()
    };
    mgr.on_playback_stopped(&stop).await.unwrap();
    // … the stop is, to the same user's other device, once its batch window
    // closes (polled up to a deadline, so a loaded host does not fail it) …
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while web_conn.sent_count() == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let frames: Vec<serde_json::Value> = web_conn
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|raw| serde_json::from_slice(raw).unwrap())
        .filter(|frame: &serde_json::Value| frame["MessageType"] == "UserDataChanged")
        .collect();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["Data"]["UserId"], user_id.simple().to_string());
    assert_eq!(
        frames[0]["Data"]["UserDataList"][0]["ItemId"],
        item_id.simple().to_string()
    );
    // … and the unrelated user's session hears nothing.
    assert_eq!(other_conn.sent_count(), 0);
}

#[tokio::test]
async fn send_message_command_accepts_any_existing_session() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
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
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
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
    async fn update_play_progress(
        &self,
        user_id: Uuid,
        item_id: Uuid,
        reported_position_ticks: Option<i64>,
        streams: ferrofin_traits::library::ReportedStreams,
    ) -> Result<bool, ServiceError> {
        self.play_state_writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner
            .update_play_progress(user_id, item_id, reported_position_ticks, streams)
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

// A progress report persists play state and pays no `UserData` read for a
// push, with or without a session listening: progress is the hottest write in
// the server, and upstream announces no progress save.
#[tokio::test]
async fn progress_reports_pay_no_read_for_a_push() {
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
        .log_session_activity("Web", "1.0", "dev-solo", "TV", "e", Some(&user))
        .await
        .unwrap();
    let info = PlaybackProgressInfo {
        session_id: session.id.clone(),
        item_id,
        position_ticks: Some(1000),
        ..PlaybackProgressInfo::default()
    };

    mgr.on_playback_progress(&info, false).await.unwrap();
    assert_eq!(counting.play_state_writes(), 1, "the write still happens");
    assert_eq!(counting.dto_reads(), 0);

    // A socket on that same session changes nothing.
    let conn = FakeConnection::new(AuthorizationInfo::default());
    mgr.add_web_socket(
        session.id.as_deref().unwrap(),
        conn.clone() as Arc<dyn WebSocketConnection>,
    )
    .await
    .unwrap();
    mgr.on_playback_progress(&info, false).await.unwrap();
    assert_eq!(counting.play_state_writes(), 2);
    assert_eq!(counting.dto_reads(), 0);
    assert_eq!(conn.sent_count(), 0);
}

/// "Remember audio/subtitle selections": a progress report saves the tracks
/// the user picked, so the next play of the item starts on them
/// (`OnPlaybackProgress` → `UpdatePlaybackSettings`, v12.2
/// SessionManager.cs:971-1055, with jellyfin#16501, #16769 and #17044):
/// - a reported index is saved while its setting is on;
/// - a report carrying no index keeps the saved one;
/// - a report with no position but a new index still saves it;
/// - with a setting off, a stored index is cleared, and a reported index of
///   that kind resets both of the user's remembered tracks for the item.
#[tokio::test]
async fn progress_remembers_the_reported_audio_and_subtitle_tracks() {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::session::PlaybackProgressInfo;

    let db = test_db().await;
    let mgr = manager(&db);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "linguist").await;
    let item_id = Uuid::new_v4();
    crate::test_support::seed_item(&db, item_id, BaseItemKind::Movie).await;
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-tracks", "TV", "e", Some(&user))
        .await
        .unwrap();
    let report =
        |position: Option<i64>, audio: Option<i32>, subtitle: Option<i32>| PlaybackProgressInfo {
            session_id: session.id.clone(),
            item_id,
            position_ticks: position,
            audio_stream_index: audio,
            subtitle_stream_index: subtitle,
            ..PlaybackProgressInfo::default()
        };
    // The remembered tracks as playback reads them back
    // (`GetUserData(user, item)`'s `AudioStreamIndex`/`SubtitleStreamIndex`).
    let remembered = || {
        let db = db.clone();
        async move {
            use ferrofin_traits::library::UserDataManager as _;
            let config: Arc<dyn ServerConfigurationManager> = Arc::new(FixedConfig {
                config: default_server_configuration(),
            });
            FerrofinUserDataManager::new(db, config)
                .get_stream_default_data(item_id, user_id)
                .await
                .expect("remembered tracks")
        }
    };
    let set_remember = |audio: bool, subtitles: bool| {
        let db = db.clone();
        async move {
            let users = FerrofinUserManager::new(db);
            let mut row = users.get_user_by_id(user_id).await.unwrap().unwrap();
            row.remember_audio_selections = audio;
            row.remember_subtitle_selections = subtitles;
            users.update_user(&row).await.unwrap();
        }
    };

    mgr.on_playback_progress(&report(Some(1000), Some(2), Some(3)), false)
        .await
        .unwrap();
    assert_eq!(remembered().await, (Some(2), Some(3)), "both saved");

    mgr.on_playback_progress(&report(Some(2000), None, None), false)
        .await
        .unwrap();
    assert_eq!(
        remembered().await,
        (Some(2), Some(3)),
        "no index keeps them"
    );

    mgr.on_playback_progress(&report(None, Some(4), None), false)
        .await
        .unwrap();
    assert_eq!(
        remembered().await,
        (Some(4), Some(3)),
        "saved without a position"
    );

    set_remember(false, true).await;
    mgr.on_playback_progress(&report(Some(3000), None, None), false)
        .await
        .unwrap();
    assert_eq!(
        remembered().await,
        (None, Some(3)),
        "audio off clears audio"
    );

    mgr.on_playback_progress(&report(Some(4000), Some(1), Some(5)), false)
        .await
        .unwrap();
    assert_eq!(
        remembered().await,
        (None, None),
        "an audio index with audio off resets both, as upstream"
    );
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
        .log_session_activity("Web", "1.0", "dev-live", "TV", "e", Some(&user))
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

    // A stream no session registered (its start and progress reports did not
    // name it) is closed by the stop that does: upstream's
    // `_activeLiveStreamSessions` has no entry for it, and the `else` branch
    // sets `liveStreamNeedsToBeClosed`.
    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: session.id.clone(),
        item_id: Uuid::nil(),
        live_stream_id: Some("never-started".to_owned()),
        ..PlaybackStopInfo::default()
    })
    .await
    .unwrap();
    assert_eq!(
        *sources.closed.lock().expect("closed mutex"),
        vec!["never-started".to_owned()],
        "an unregistered live stream is closed once"
    );

    // Playback start registers the holder; the stop then releases it. A second
    // viewer holds the same stream, so its entry outlives this viewer's.
    let other = mgr
        .log_session_activity("Web", "1.0", "dev-live-2", "TV 2", "e", Some(&user))
        .await
        .unwrap();
    for (holder, play) in [(&session, "play-42"), (&other, "play-other")] {
        mgr.on_playback_start(&ferrofin_model::session::PlaybackStartInfo {
            session_id: holder.id.clone(),
            item_id: Uuid::nil(),
            live_stream_id: Some("live-42".to_owned()),
            play_session_id: Some(play.to_owned()),
            ..ferrofin_model::session::PlaybackStartInfo::default()
        })
        .await
        .unwrap();
    }
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
        vec!["never-started".to_owned(), "live-42".to_owned()],
        "the reported live stream is closed exactly once"
    );

    // The same viewer under its OTHER id (the play session) must not release a
    // second consumer while the stream still has a holder — that is what would
    // tear the tuner out from under the other viewer of the same channel.
    mgr.close_live_stream_if_needed("live-42", "play-42")
        .await
        .unwrap();
    assert_eq!(
        sources.closed.lock().expect("closed mutex").len(),
        2,
        "one viewer releases once, however many of its ids report"
    );
}

/// A release for a live stream no session registered closes it (C#
/// `CloseLiveStreamIfNeededAsync`'s `else { liveStreamNeedsToBeClosed = true; }`,
/// from jellyfin#17178 in 12.0-rc2). This is the transcode teardown of a
/// vanished client whose play-state reports never named the stream: the
/// releaser passes the job's play-session id, or nothing when the job has none.
/// Without the close the tuner stays open until a restart.
#[tokio::test]
async fn a_live_stream_no_session_registered_is_closed() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let db = test_db().await;
        let sources = Arc::new(RecordingMediaSources {
            closed: Mutex::new(Vec::new()),
        });
        let mgr = manager_recording_releases(&db, &sources);

        mgr.close_live_stream_if_needed("live-job", "play-job")
            .await
            .unwrap();
        assert_eq!(
            *sources.closed.lock().expect("closed mutex"),
            vec!["live-job".to_owned()],
            "a teardown release of an unregistered stream closes it once"
        );

        mgr.close_live_stream_if_needed("live-job2", "")
            .await
            .unwrap();
        assert_eq!(
            *sources.closed.lock().expect("closed mutex"),
            vec!["live-job".to_owned(), "live-job2".to_owned()],
            "a job without a play session closes its unregistered stream once"
        );
    })
    .await
    .expect("the test finished in time");
}

/// The case the holder map guards: a stream registered by one viewer is not
/// closed by a release under an id that is not among its holders (C#
/// `activeSessionMappings.TryRemove` fails, so `liveStreamNeedsToBeClosed`
/// stays false). The holder's own release still closes it.
#[tokio::test]
async fn a_release_by_a_stranger_does_not_close_a_registered_stream() {
    use ferrofin_model::session::{PlaybackStartInfo, PlaybackStopInfo};

    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let db = test_db().await;
        let sources = Arc::new(RecordingMediaSources {
            closed: Mutex::new(Vec::new()),
        });
        let mgr = manager_recording_releases(&db, &sources);
        let user = seed_named_user(&db, Uuid::new_v4(), "bob").await;
        let bob = mgr
            .log_session_activity("Web", "1.0", "dev-bob", "TV", "e", Some(&user))
            .await
            .unwrap();
        mgr.on_playback_start(&PlaybackStartInfo {
            session_id: bob.id.clone(),
            item_id: Uuid::nil(),
            live_stream_id: Some("shared".to_owned()),
            play_session_id: Some("play-bob".to_owned()),
            ..PlaybackStartInfo::default()
        })
        .await
        .unwrap();

        mgr.close_live_stream_if_needed("shared", "stranger")
            .await
            .unwrap();
        assert!(
            sources.closed.lock().expect("closed mutex").is_empty(),
            "a release by an id that holds nothing closes nothing"
        );

        mgr.on_playback_stopped(&PlaybackStopInfo {
            session_id: bob.id.clone(),
            item_id: Uuid::nil(),
            live_stream_id: Some("shared".to_owned()),
            ..PlaybackStopInfo::default()
        })
        .await
        .unwrap();
        assert_eq!(
            *sources.closed.lock().expect("closed mutex"),
            vec!["shared".to_owned()],
            "the holder's own stop closes it"
        );
    })
    .await
    .expect("the test finished in time");
}

/// Two viewers of one channel: the first to stop must not close the stream the
/// second is still watching. Each viewer is registered under its own session and
/// play-session ids, and only its own release counts.
#[tokio::test]
async fn one_viewer_stopping_does_not_release_another_viewers_stream() {
    use ferrofin_model::session::{PlaybackStartInfo, PlaybackStopInfo};

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
    let alice = mgr
        .log_session_activity("Web", "1.0", "dev-a", "TV A", "e", Some(&user))
        .await
        .unwrap();
    let bob = mgr
        .log_session_activity("Web", "1.0", "dev-b", "TV B", "e", Some(&user))
        .await
        .unwrap();

    for (session, play) in [(&alice, "play-a"), (&bob, "play-b")] {
        mgr.on_playback_start(&PlaybackStartInfo {
            session_id: session.id.clone(),
            item_id: Uuid::nil(),
            live_stream_id: Some("shared".to_owned()),
            play_session_id: Some(play.to_owned()),
            ..PlaybackStartInfo::default()
        })
        .await
        .unwrap();
    }

    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: alice.id.clone(),
        item_id: Uuid::nil(),
        live_stream_id: Some("shared".to_owned()),
        ..PlaybackStopInfo::default()
    })
    .await
    .unwrap();
    // One release for Alice — the consumer count still covers Bob, and it is
    // `MediaSourceManager::close_live_stream` that decides the tuner's fate.
    assert_eq!(sources.closed.lock().expect("closed mutex").len(), 1);

    // Alice reporting again (a duplicate stop, or an explicit /LiveStreams/Close)
    // must not release a second time.
    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: alice.id.clone(),
        item_id: Uuid::nil(),
        live_stream_id: Some("shared".to_owned()),
        ..PlaybackStopInfo::default()
    })
    .await
    .unwrap();
    assert_eq!(sources.closed.lock().expect("closed mutex").len(), 1);

    // Bob's own stop is his to make.
    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: bob.id.clone(),
        item_id: Uuid::nil(),
        live_stream_id: Some("shared".to_owned()),
        ..PlaybackStopInfo::default()
    })
    .await
    .unwrap();
    assert_eq!(sources.closed.lock().expect("closed mutex").len(), 2);
}

/// A manager whose live-stream releases `sources` records.
fn manager_recording_releases(
    db: &Database,
    sources: &Arc<RecordingMediaSources>,
) -> Arc<FerrofinSessionManager> {
    Arc::new(
        Arc::try_unwrap(manager(db))
            .expect("sole owner")
            .with_media_sources(
                Arc::clone(sources) as Arc<dyn ferrofin_traits::library::MediaSourceManager>
            ),
    )
}

/// C# `OnPlaybackStopped` resolves the reported item with `GetNowPlayingItem`,
/// which is null for an id the library no longer has (a channel a guide
/// refresh dropped mid-play, an item deleted while it played). Nothing is
/// saved for it, but the live stream is still released and the stop succeeds.
#[tokio::test]
async fn a_stop_for_an_item_the_library_no_longer_has_still_releases_the_live_stream() {
    use ferrofin_model::session::{PlaybackStartInfo, PlaybackStopInfo};

    let db = test_db().await;
    let sources = Arc::new(RecordingMediaSources {
        closed: Mutex::new(Vec::new()),
    });
    let mgr = manager_recording_releases(&db, &sources);
    let user = seed_named_user(&db, Uuid::new_v4(), "viewer").await;
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-gone", "TV", "e", Some(&user))
        .await
        .unwrap();
    let gone = Uuid::new_v4();
    crate::test_support::seed_item(&db, gone, ferrofin_model::data::BaseItemKind::Movie).await;
    mgr.on_playback_start(&PlaybackStartInfo {
        session_id: session.id.clone(),
        item_id: gone,
        live_stream_id: Some("live-gone".to_owned()),
        play_session_id: Some("play-gone".to_owned()),
        ..PlaybackStartInfo::default()
    })
    .await
    .unwrap();
    // The row goes while it plays.
    crate::FerrofinItemPersistenceService::new(db.clone())
        .delete_items(&[gone])
        .await
        .unwrap();

    let stopped = mgr
        .on_playback_stopped(&PlaybackStopInfo {
            session_id: session.id.clone(),
            item_id: gone,
            live_stream_id: Some("live-gone".to_owned()),
            position_ticks: Some(1000),
            ..PlaybackStopInfo::default()
        })
        .await;
    assert!(stopped.is_ok(), "{stopped:?}");
    assert_eq!(
        *sources.closed.lock().expect("closed mutex"),
        vec!["live-gone".to_owned()]
    );
}

/// A stop whose save fails (here the session's user was deleted mid-play, where
/// C# `GetUsers` throws) still releases the live stream and publishes, then
/// returns the error. Upstream skips the release then, which leaks the tuner.
#[tokio::test]
async fn a_stop_whose_save_fails_still_releases_the_live_stream_and_returns_the_error() {
    use ferrofin_model::session::{PlaybackStartInfo, PlaybackStopInfo};

    let db = test_db().await;
    let sources = Arc::new(RecordingMediaSources {
        closed: Mutex::new(Vec::new()),
    });
    let mgr = manager_recording_releases(&db, &sources);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "leaving").await;
    // The last user cannot be deleted.
    seed_named_user(&db, Uuid::new_v4(), "staying").await;
    let item = Uuid::new_v4();
    crate::test_support::seed_item(&db, item, ferrofin_model::data::BaseItemKind::Movie).await;
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-err", "TV", "e", Some(&user))
        .await
        .unwrap();
    mgr.on_playback_start(&PlaybackStartInfo {
        session_id: session.id.clone(),
        item_id: item,
        live_stream_id: Some("live-err".to_owned()),
        play_session_id: Some("play-err".to_owned()),
        ..PlaybackStartInfo::default()
    })
    .await
    .unwrap();
    FerrofinUserManager::new(db.clone())
        .delete_user(user_id)
        .await
        .unwrap();

    let stopped = mgr
        .on_playback_stopped(&PlaybackStopInfo {
            session_id: session.id.clone(),
            item_id: item,
            live_stream_id: Some("live-err".to_owned()),
            position_ticks: Some(1000),
            ..PlaybackStopInfo::default()
        })
        .await;
    assert!(
        matches!(stopped, Err(ServiceError::NotFound(_))),
        "{stopped:?}"
    );
    assert_eq!(
        *sources.closed.lock().expect("closed mutex"),
        vec!["live-err".to_owned()],
        "the live stream is released before the error is returned"
    );
}

/// C# `OnPlaybackStart` and `OnPlaybackProgress` guard their per-user save the
/// same way (`if (libraryItem is not null)`): a report for an item the library
/// does not have succeeds and saves nothing.
#[tokio::test]
async fn start_and_progress_for_an_item_the_library_does_not_have_save_nothing() {
    use ferrofin_model::session::{PlaybackProgressInfo, PlaybackStartInfo};
    use ferrofin_traits::library::UserDataManager as _;

    let db = test_db().await;
    let mgr = manager(&db);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "viewer").await;
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-none", "TV", "e", Some(&user))
        .await
        .unwrap();
    let unknown = Uuid::new_v4();

    let started = mgr
        .on_playback_start(&PlaybackStartInfo {
            session_id: session.id.clone(),
            item_id: unknown,
            ..PlaybackStartInfo::default()
        })
        .await;
    assert!(started.is_ok(), "{started:?}");
    let progressed = mgr
        .on_playback_progress(
            &PlaybackProgressInfo {
                session_id: session.id.clone(),
                item_id: unknown,
                position_ticks: Some(1000),
                ..PlaybackProgressInfo::default()
            },
            false,
        )
        .await;
    assert!(progressed.is_ok(), "{progressed:?}");
    let user_data = FerrofinUserDataManager::new(
        db.clone(),
        Arc::new(FixedConfig {
            config: default_server_configuration(),
        }),
    );
    // An item without a stored row reads as the empty row.
    let data = user_data
        .get_user_data_dto(unknown, user_id)
        .await
        .unwrap()
        .expect("the empty row");
    assert_eq!(
        (
            data.play_count,
            data.playback_position_ticks,
            data.last_played_date
        ),
        (0, 0, None),
        "nothing was saved"
    );
}

/// C# `OnPlaybackStopped` refuses a negative position (stalled clients report
/// one): it releases the live stream first, so no tuner leaks, then throws
/// before anything is saved. The resume point the last progress report left
/// stays.
#[tokio::test]
async fn a_stop_at_a_negative_position_releases_the_live_stream_and_keeps_the_resume_point() {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::session::{PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo};
    use ferrofin_traits::library::UserDataManager as _;

    let db = test_db().await;
    let sources = Arc::new(RecordingMediaSources {
        closed: Mutex::new(Vec::new()),
    });
    let mgr = manager_recording_releases(&db, &sources);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "viewer").await;
    let item = Uuid::new_v4();
    crate::test_support::seed_item(&db, item, BaseItemKind::Movie).await;
    let mut row = crate::test_support::fetch_item(&db, item).await;
    row.run_time_ticks = Some(72_000_000_000);
    crate::test_support::save_item(&db, &row).await;
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-neg", "TV", "e", Some(&user))
        .await
        .unwrap();
    mgr.on_playback_start(&PlaybackStartInfo {
        session_id: session.id.clone(),
        item_id: item,
        live_stream_id: Some("live-neg".to_owned()),
        play_session_id: Some("play-neg".to_owned()),
        ..PlaybackStartInfo::default()
    })
    .await
    .unwrap();
    let halfway = 36_000_000_000;
    mgr.on_playback_progress(
        &PlaybackProgressInfo {
            session_id: session.id.clone(),
            item_id: item,
            position_ticks: Some(halfway),
            ..PlaybackProgressInfo::default()
        },
        false,
    )
    .await
    .unwrap();

    let stopped = mgr
        .on_playback_stopped(&PlaybackStopInfo {
            session_id: session.id.clone(),
            item_id: item,
            live_stream_id: Some("live-neg".to_owned()),
            position_ticks: Some(-1),
            ..PlaybackStopInfo::default()
        })
        .await;
    assert!(
        matches!(stopped, Err(ServiceError::InvalidInput(_))),
        "{stopped:?}"
    );
    assert_eq!(
        *sources.closed.lock().expect("closed mutex"),
        vec!["live-neg".to_owned()],
        "the live stream is released before the stop is refused"
    );
    // Refused before `RemoveNowPlayingItem`: only automatic progress stopped.
    let snapshot = mgr
        .get_session_snapshot(session.id.as_deref().expect("session id"))
        .await
        .unwrap();
    assert_eq!(
        (snapshot.now_playing_item_id, snapshot.is_playing),
        (Some(item), false)
    );
    let user_data = FerrofinUserDataManager::new(
        db.clone(),
        Arc::new(FixedConfig {
            config: default_server_configuration(),
        }),
    );
    let resume = user_data
        .get_user_data_dto(item, user_id)
        .await
        .unwrap()
        .map_or(0, |data| data.playback_position_ticks);
    assert_eq!(resume, halfway, "nothing is saved for a refused stop");
}

/// C# `CloseIfNeededAsync`: when a session's last socket closes, the live
/// stream it was playing (`PlayState.LiveStreamId`) is released before the
/// session ends, so a client that vanishes without a stop does not hold its
/// tuner until a restart.
#[tokio::test]
async fn the_last_socket_closing_releases_the_sessions_live_stream() {
    use ferrofin_model::session::PlaybackStartInfo;

    let db = test_db().await;
    let sources = Arc::new(RecordingMediaSources {
        closed: Mutex::new(Vec::new()),
    });
    let mgr = manager_recording_releases(&db, &sources);
    let user = seed_named_user(&db, Uuid::new_v4(), "vanishing").await;
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-vanish", "TV", "e", Some(&user))
        .await
        .unwrap();
    let session_id = session.id.clone().expect("session id");
    mgr.on_playback_start(&PlaybackStartInfo {
        session_id: session.id.clone(),
        item_id: Uuid::nil(),
        live_stream_id: Some("live-vanish".to_owned()),
        play_session_id: Some("play-vanish".to_owned()),
        ..PlaybackStartInfo::default()
    })
    .await
    .unwrap();

    mgr.close_if_needed(&session_id).await.unwrap();
    assert_eq!(
        *sources.closed.lock().expect("closed mutex"),
        vec!["live-vanish".to_owned()]
    );
    assert!(
        mgr.get_session_snapshot(&session_id).await.is_err(),
        "the session ended"
    );
}

/// What the last socket closing does not release: a stream a stop already
/// released (the session's play state no longer names it), and nothing at
/// all for a logout (C# `ReportSessionEnded`, which only ends the session).
#[tokio::test]
async fn a_logout_or_an_already_stopped_stream_is_not_released_on_session_end() {
    use ferrofin_model::session::{PlaybackStartInfo, PlaybackStopInfo};

    let db = test_db().await;
    let sources = Arc::new(RecordingMediaSources {
        closed: Mutex::new(Vec::new()),
    });
    let mgr = manager_recording_releases(&db, &sources);
    let user = seed_named_user(&db, Uuid::new_v4(), "viewer").await;
    let start = |session: &SessionInfoDto, stream: &str| PlaybackStartInfo {
        session_id: session.id.clone(),
        item_id: Uuid::nil(),
        live_stream_id: Some(stream.to_owned()),
        play_session_id: Some(format!("play-{stream}")),
        ..PlaybackStartInfo::default()
    };

    let logged_out = mgr
        .log_session_activity("Web", "1.0", "dev-out", "TV", "e", Some(&user))
        .await
        .unwrap();
    mgr.on_playback_start(&start(&logged_out, "live-out"))
        .await
        .unwrap();
    mgr.report_session_ended(logged_out.id.as_deref().expect("id"))
        .await
        .unwrap();
    assert!(
        sources.closed.lock().expect("closed mutex").is_empty(),
        "a logout releases nothing"
    );

    let stopped = mgr
        .log_session_activity("Web", "1.0", "dev-stop", "TV", "e", Some(&user))
        .await
        .unwrap();
    mgr.on_playback_start(&start(&stopped, "live-stop"))
        .await
        .unwrap();
    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: stopped.id.clone(),
        item_id: Uuid::nil(),
        live_stream_id: Some("live-stop".to_owned()),
        ..PlaybackStopInfo::default()
    })
    .await
    .unwrap();
    assert_eq!(
        mgr.get_session_snapshot(stopped.id.as_deref().expect("id"))
            .await
            .unwrap()
            .now_playing_live_stream_id,
        None,
        "a stop clears the session's live stream (C# `RemoveNowPlayingItem`)"
    );
    mgr.close_if_needed(stopped.id.as_deref().expect("id"))
        .await
        .unwrap();
    assert_eq!(
        *sources.closed.lock().expect("closed mutex"),
        vec!["live-stop".to_owned()],
        "released once, by the stop"
    );
}

/// The session's live stream is what the latest report names (C#
/// `PlayState.LiveStreamId = info.LiveStreamId`), so a progress report
/// without one clears it. A stop refused for its negative position releases
/// the stream and keeps the play state, so the socket closing afterwards
/// releases it again: the refused stop emptied the stream's holders, and C#
/// `CloseLiveStreamIfNeededAsync` closes a stream with no holder entry. The
/// second close finds no consumer left unless an unregistered viewer shares it.
#[tokio::test]
async fn the_sessions_live_stream_follows_the_reports() {
    use ferrofin_model::session::{PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo};

    let db = test_db().await;
    let sources = Arc::new(RecordingMediaSources {
        closed: Mutex::new(Vec::new()),
    });
    let mgr = manager_recording_releases(&db, &sources);
    let user = seed_named_user(&db, Uuid::new_v4(), "viewer").await;
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-follow", "TV", "e", Some(&user))
        .await
        .unwrap();
    let session_id = session.id.clone().expect("session id");
    let stream = |mgr: &Arc<FerrofinSessionManager>| {
        let mgr = Arc::clone(mgr);
        let session_id = session_id.clone();
        async move {
            mgr.get_session_snapshot(&session_id)
                .await
                .unwrap()
                .now_playing_live_stream_id
        }
    };
    mgr.on_playback_start(&PlaybackStartInfo {
        session_id: session.id.clone(),
        item_id: Uuid::nil(),
        live_stream_id: Some("live-follow".to_owned()),
        play_session_id: Some("play-follow".to_owned()),
        ..PlaybackStartInfo::default()
    })
    .await
    .unwrap();
    assert_eq!(stream(&mgr).await.as_deref(), Some("live-follow"));
    mgr.on_playback_progress(
        &PlaybackProgressInfo {
            session_id: session.id.clone(),
            item_id: Uuid::nil(),
            ..PlaybackProgressInfo::default()
        },
        false,
    )
    .await
    .unwrap();
    assert_eq!(stream(&mgr).await, None, "a report without one clears it");

    mgr.on_playback_progress(
        &PlaybackProgressInfo {
            session_id: session.id.clone(),
            item_id: Uuid::nil(),
            live_stream_id: Some("live-follow".to_owned()),
            ..PlaybackProgressInfo::default()
        },
        false,
    )
    .await
    .unwrap();
    let refused = mgr
        .on_playback_stopped(&PlaybackStopInfo {
            session_id: session.id.clone(),
            item_id: Uuid::nil(),
            live_stream_id: Some("live-follow".to_owned()),
            position_ticks: Some(-1),
            ..PlaybackStopInfo::default()
        })
        .await;
    assert!(matches!(refused, Err(ServiceError::InvalidInput(_))));
    mgr.close_if_needed(&session_id).await.unwrap();
    assert_eq!(
        *sources.closed.lock().expect("closed mutex"),
        vec!["live-follow".to_owned(), "live-follow".to_owned()],
        "released by the refused stop, then by the socket closing, as upstream"
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
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
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
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", Some(&user))
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
        .log_session_activity("Web", "1.0", device_id, "TV", "e", Some(user))
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

/// Sets an item's `SortName` through the production repository save, which
/// keeps a key the caller supplies (the scanner's path) — the fixture insert
/// leaves it NULL, which makes a `SortName` ordering assertion meaningless. Not
/// through `LibraryManager::update_items`: an edit's save re-derives the key
/// from the name, as upstream's does.
async fn set_sort_name(db: &Database, id: Uuid, sort_name: &str) {
    let mut row = library_manager(db)
        .get_item_by_id(id)
        .await
        .expect("load item")
        .expect("item present");
    row.sort_name = Some(sort_name.to_owned());
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(std::slice::from_ref(&row))
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
    let user_uuid = Uuid::new_v4();
    let user = seed_named_user(&db, user_uuid, "alice").await;
    // Before `allow_playback`, which upserts over the default set.
    crate::test_support::grant_default_permissions(&db, user_uuid).await;
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
    // Expanding the genre queries with no scope but with a user, so the track
    // needs a library above it and the user needs to be able to see it.
    crate::test_support::seed_library_over(&db, &[track]).await;

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
    set_permission(
        db.writer(),
        &controller.id,
        PermissionKind::EnableRemoteControlOfOtherUsers,
        true,
    )
    .await
    .unwrap();
    let target_user = seed_named_user(&db, Uuid::new_v4(), "target").await;
    allow_playback(&db, &target_user).await;

    let controlling_session = mgr
        .log_session_activity(
            "Web",
            "1.0",
            "dev-controller",
            "Phone",
            "e",
            Some(&controller),
        )
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
            Err(ServiceError::Forbidden(msg)) => {
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

#[rstest::rstest]
#[case(-1, 3)]
#[case(0, 3)]
#[case(1, 1)]
#[case(2, 2)]
#[tokio::test]
async fn saved_session_limits_apply_to_new_logins(#[case] cap: i32, #[case] admitted: usize) {
    use ferrofin_traits::library::UserManager;
    let db = test_db().await;
    let mgr = manager(&db);
    let users = crate::user_manager::FerrofinUserManager::new(db.clone());
    let user = users.create_user("session-cap").await.unwrap();
    let uid = Uuid::parse_str(&user.id).unwrap();
    let mut policy = ferrofin_model::users::UserPolicy {
        max_active_sessions: cap,
        enable_all_devices: true,
        ..Default::default()
    };
    users.update_policy(uid, &policy).await.unwrap();
    let mut request = AuthenticationRequest {
        user_id: Some(uid),
        app: Some("Web".into()),
        app_version: Some("1.0".into()),
        device_name: Some("Browser".into()),
        ..Default::default()
    };
    for i in 0..3 {
        request.device_id = Some(format!("cap-device-{i}"));
        let result = mgr.authenticate_direct(&request).await;
        if i < admitted {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert!(matches!(result, Err(ServiceError::Forbidden(_))));
        }
    }
    if cap > 0 {
        request.device_id = Some("cap-device-0".into());
        assert!(
            matches!(
                mgr.authenticate_direct(&request).await,
                Err(ServiceError::Forbidden(_))
            ),
            "same-device login still checks the cap before replacing its session"
        );
    }
    policy.max_active_sessions = 1;
    users.update_policy(uid, &policy).await.unwrap();
    assert_eq!(
        mgr.get_sessions(Uuid::nil(), None, None, None, true)
            .await
            .unwrap()
            .len(),
        admitted,
        "lowering does not evict existing sessions"
    );
    request.device_id = Some("one-more".into());
    assert!(matches!(
        mgr.authenticate_direct(&request).await,
        Err(ServiceError::Forbidden(_))
    ));
}

#[tokio::test]
async fn remote_control_permission_gates_delivery_and_saved_changes() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let alice = seed_named_user(&db, Uuid::new_v4(), "controller").await;
    let alice_id = Uuid::parse_str(&alice.id).unwrap();
    let bob = seed_named_user(&db, Uuid::new_v4(), "target").await;
    let controller = mgr
        .log_session_activity("Web", "1", "control", "Control", "e", Some(&alice))
        .await
        .unwrap()
        .id
        .unwrap();
    let (target, received) = cast_target(&mgr, bus.as_ref(), &bob, "target").await;
    let command = ferrofin_model::session::PlaystateRequest {
        command: ferrofin_model::session::PlaystateCommand::Pause,
        ..Default::default()
    };
    for enabled in [false, true, false] {
        set_permission(
            db.writer(),
            &alice.id,
            PermissionKind::EnableRemoteControlOfOtherUsers,
            enabled,
        )
        .await
        .unwrap();
        received.lock().unwrap().clear();
        let result = mgr
            .send_playstate_command(&controller, &target, &command)
            .await;
        assert_eq!(result.is_ok(), enabled);
        assert_eq!(received.lock().unwrap().len(), usize::from(enabled));
    }
    set_permission(
        db.writer(),
        &alice.id,
        PermissionKind::IsAdministrator,
        true,
    )
    .await
    .unwrap();
    assert!(
        matches!(
            mgr.assert_can_control(&controller, &target).await,
            Err(ServiceError::Forbidden(_))
        ),
        "admin is not an override here"
    );
    mgr.assert_can_control(&controller, &controller)
        .await
        .unwrap();
    mgr.add_additional_user(&target, alice_id).await.unwrap();
    mgr.assert_can_control(&controller, &target).await.unwrap();
    mgr.remove_additional_user(&target, alice_id).await.unwrap();
    mgr.assert_can_control("", &target).await.unwrap();
    let public = mgr
        .upsert_session("Public", "1", "shared", "Shared", "e", None)
        .await
        .unwrap()
        .id
        .unwrap();
    mgr.assert_can_control(&controller, &public).await.unwrap();
    mgr.assert_can_control(&public, &target).await.unwrap();
    assert!(matches!(
        mgr.assert_can_control("missing", &target).await,
        Err(ServiceError::NotFound(_))
    ));
}

#[tokio::test]
async fn shared_device_filter_uses_the_requested_user_and_device_access() {
    let db = test_db().await;
    let (mgr, bus) = cast_manager(&db);
    let alice = seed_named_user(&db, Uuid::new_v4(), "viewer").await;
    let id = Uuid::parse_str(&alice.id).unwrap();
    set_permission(
        db.writer(),
        &alice.id,
        PermissionKind::EnableAllDevices,
        true,
    )
    .await
    .unwrap();
    let (own, _) = cast_target(&mgr, bus.as_ref(), &alice, "own").await;
    mgr.report_capabilities(
        &own,
        &ClientCapabilities {
            supports_media_control: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let public = mgr
        .upsert_session("Public", "1", "shared", "Shared", "e", None)
        .await
        .unwrap()
        .id
        .unwrap();
    mgr.report_capabilities(
        &public,
        &ClientCapabilities {
            supports_media_control: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    bus.register(public.clone(), Box::new(|_| {}));
    for enabled in [false, true, false] {
        set_permission(
            db.writer(),
            &alice.id,
            PermissionKind::EnableSharedDeviceControl,
            enabled,
        )
        .await
        .unwrap();
        let listed = mgr
            .get_sessions(id, None, None, Some(id), false)
            .await
            .unwrap();
        assert!(listed.iter().any(|s| s.id.as_deref() == Some(&own)));
        assert_eq!(
            listed.iter().any(|s| s.id.as_deref() == Some(&public)),
            enabled
        );
        let api_key = mgr
            .get_sessions(Uuid::nil(), None, None, Some(id), true)
            .await
            .unwrap();
        assert_eq!(
            api_key.iter().any(|s| s.id.as_deref() == Some(&public)),
            enabled,
            "API key filters by the named user's sharing preference"
        );
    }
    assert!(
        mgr.get_sessions(id, None, None, Some(Uuid::new_v4()), false)
            .await
            .unwrap()
            .is_empty()
    );
    set_permission(
        db.writer(),
        &alice.id,
        PermissionKind::EnableAllDevices,
        false,
    )
    .await
    .unwrap();
    assert!(
        mgr.get_sessions(id, None, None, Some(id), false)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !mgr.get_sessions(Uuid::nil(), None, None, Some(id), true)
            .await
            .unwrap()
            .is_empty()
    );
}

/// `SessionManager.GetProgressItem` (`SessionManager.cs:742-756`): playing a
/// version of the item the client shows reports the shown item as `ItemId`
/// and the version as `MediaSourceId`, and the progress is kept against the
/// version; a media source id that is no version of the item keeps the item.
#[tokio::test]
async fn progress_is_kept_against_the_version_the_media_source_names() {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::session::PlaybackProgressInfo;
    use ferrofin_traits::library::UserDataManager as _;

    let db = test_db().await;
    let mgr = manager(&db);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "viewer").await;
    let (primary, version, stranger) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for id in [primary, version, stranger] {
        crate::test_support::seed_item(&db, id, BaseItemKind::Movie).await;
        let mut row = crate::test_support::fetch_item(&db, id).await;
        row.run_time_ticks = Some(72_000_000_000);
        if id == version {
            row.owner_id = Some(guid_to_db(primary));
            row.primary_version_id = Some(guid_to_db(primary));
        }
        crate::test_support::save_item(&db, &row).await;
    }
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-v", "TV", "e", Some(&user))
        .await
        .unwrap();
    let report = |source: Uuid, ticks: i64| PlaybackProgressInfo {
        session_id: session.id.clone(),
        item_id: primary,
        media_source_id: Some(source.simple().to_string()),
        position_ticks: Some(ticks),
        ..PlaybackProgressInfo::default()
    };
    let user_data = FerrofinUserDataManager::new(
        db.clone(),
        Arc::new(FixedConfig {
            config: default_server_configuration(),
        }),
    );
    let position = |id: Uuid| {
        let user_data = &user_data;
        async move {
            user_data
                .get_user_data_dto(id, user_id)
                .await
                .unwrap()
                .map_or(0, |d| d.playback_position_ticks)
        }
    };

    let halfway = 36_000_000_000;
    mgr.on_playback_progress(&report(version, halfway), false)
        .await
        .unwrap();
    assert_eq!(position(version).await, halfway, "the version played");
    assert_eq!(position(primary).await, 0, "not the item shown");

    mgr.on_playback_progress(&report(stranger, halfway), false)
        .await
        .unwrap();
    assert_eq!(
        position(primary).await,
        halfway,
        "no version of it: the item"
    );
    assert_eq!(position(stranger).await, 0);
}

/// A session recreated mid-playback (a restart, a WebSocket-close session end,
/// a relogin) only ever receives progress reports: the client never resends
/// PlaybackStart. C# `OnPlaybackProgress` → `UpdateNowPlayingItem` restores the
/// now-playing item from the first one, and keeps the first pause's date.
#[tokio::test]
async fn progress_alone_restores_the_now_playing_item() {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::session::PlaybackProgressInfo;

    let db = test_db().await;
    let mgr = manager(&db);
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "viewer").await;
    let item_id = Uuid::new_v4();
    crate::test_support::seed_item(&db, item_id, BaseItemKind::Movie).await;
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-recreated", "TV", "e", Some(&user))
        .await
        .unwrap();
    assert!(!mgr.has_active_playback().await.unwrap());

    let paused = PlaybackProgressInfo {
        session_id: session.id.clone(),
        item_id,
        position_ticks: Some(1000),
        is_paused: true,
        ..PlaybackProgressInfo::default()
    };
    mgr.on_playback_progress(&paused, false).await.unwrap();
    assert!(
        mgr.has_active_playback().await.unwrap(),
        "progress names the now-playing item"
    );
    let id = session.id.as_deref().unwrap();
    let first = mgr.get_session_snapshot(id).await.unwrap();
    assert_eq!(first.now_playing_item_id, Some(item_id));
    assert!(
        super::session_info_to_dto(&first)
            .play_state
            .unwrap()
            .can_seek
    );
    let paused_since = first
        .last_paused_date
        .expect("a paused report dates the pause");

    // A later paused report keeps the date the pause began.
    mgr.on_playback_progress(&paused, false).await.unwrap();
    let again = mgr.get_session_snapshot(id).await.unwrap();
    assert_eq!(again.last_paused_date, Some(paused_since));
}

/// The rest of C# `UpdateNowPlayingItem`: an item the library cannot resolve
/// and a report without an item leave nothing playing, and an automated report
/// does not count as a check-in.
#[tokio::test]
async fn progress_names_only_items_the_library_knows() {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::session::PlaybackProgressInfo;

    let db = test_db().await;
    let mgr = manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "viewer").await;
    let item_id = Uuid::new_v4();
    crate::test_support::seed_item(&db, item_id, BaseItemKind::Movie).await;
    let session = mgr
        .log_session_activity("Web", "1.0", "dev-unknown", "TV", "e", Some(&user))
        .await
        .unwrap();
    let id = session.id.as_deref().unwrap();
    let report = |item_id| PlaybackProgressInfo {
        session_id: session.id.clone(),
        item_id,
        ..PlaybackProgressInfo::default()
    };

    // The user-data write then rejects the unknown item; only the session
    // state matters here.
    let _ = mgr
        .on_playback_progress(&report(Uuid::new_v4()), false)
        .await;
    assert!(!mgr.has_active_playback().await.unwrap(), "unknown item");

    mgr.on_playback_progress(&report(item_id), false)
        .await
        .unwrap();
    assert!(mgr.has_active_playback().await.unwrap());
    let checked_in = mgr
        .get_session_snapshot(id)
        .await
        .unwrap()
        .last_playback_check_in;

    mgr.on_playback_progress(&report(item_id), true)
        .await
        .unwrap();
    assert_eq!(
        mgr.get_session_snapshot(id)
            .await
            .unwrap()
            .last_playback_check_in,
        checked_in,
        "an automated report is no check-in"
    );

    mgr.on_playback_progress(&report(Uuid::nil()), false)
        .await
        .unwrap();
    assert!(
        !mgr.has_active_playback().await.unwrap(),
        "no item reported"
    );
}

/// `LogSessionActivity` (`SessionManager.cs:244-292`) stamps the signed-in
/// user's `LastActivityDate` when the user's own date is more than a minute
/// old (never set counts as old), and not again within the minute, even
/// though the caller still holds the entity it read before the stamp, as a
/// request served from the auth cache does. A user active within the minute
/// is left as is.
#[tokio::test]
async fn session_activity_stamps_the_users_last_activity_at_most_once_a_minute() {
    let db = test_db().await;
    let mgr = manager(&db);
    let users = crate::user_manager::FerrofinUserManager::new(db.clone());
    let stored = |id: Uuid| {
        let users = &users;
        async move {
            users
                .get_user_by_id(id)
                .await
                .unwrap()
                .unwrap()
                .last_activity_date
        }
    };

    let alice_id = Uuid::new_v4();
    let alice = seed_named_user(&db, alice_id, "alice").await;
    assert_eq!(alice.last_activity_date, None);
    let before = chrono::Utc::now();
    mgr.log_session_activity("Web", "1.0", "dev-1", "Chrome", "1.2.3.4", Some(&alice))
        .await
        .unwrap();
    let stamped = stored(alice_id).await.expect("stamped");
    assert!(
        stamped >= before - chrono::Duration::seconds(1),
        "{stamped}"
    );

    let row_version = || async {
        users
            .get_user_by_id(alice_id)
            .await
            .unwrap()
            .unwrap()
            .row_version
    };
    let written = row_version().await;
    mgr.log_session_activity("Web", "1.0", "dev-1", "Chrome", "1.2.3.4", Some(&alice))
        .await
        .unwrap();
    assert_eq!(
        stored(alice_id).await,
        Some(stamped),
        "not again within the minute"
    );
    assert_eq!(row_version().await, written, "no write at all");

    let bob_id = Uuid::new_v4();
    let mut bob = seed_named_user(&db, bob_id, "bob").await;
    bob.last_activity_date = Some(chrono::Utc::now() - chrono::Duration::seconds(10));
    mgr.log_session_activity("Web", "1.0", "dev-2", "Chrome", "1.2.3.4", Some(&bob))
        .await
        .unwrap();
    assert_eq!(stored(bob_id).await, None, "active within the minute");
}
