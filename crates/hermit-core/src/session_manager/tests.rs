//! Tests for [`HermitSessionManager`] over a real in-memory `hermit-db` plus the
//! real concrete sibling managers (user/device/user-data/library) and a minimal
//! fake [`DtoService`] (only its unused-here trait surface).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use hermit_db::enums::PermissionKind;
use hermit_model::configuration::ServerConfiguration;
use hermit_model::dto::{BaseItemDto, SessionInfoDto};
use hermit_model::session::{ClientCapabilities, MessageCommand, SessionMessageType};
use uuid::Uuid;

use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::dto::DtoService;
use hermit_traits::error::ServiceError;
use hermit_traits::net::WebSocketConnection;
use hermit_traits::options::{AuthorizationInfo, DtoOptions};
use hermit_traits::session::{AuthenticationRequest, SessionManager};
use hermit_traits::system::ServerApplicationPaths;

use super::HermitSessionManager;
use crate::configuration_manager::default_server_configuration;
use crate::device_manager::HermitDeviceManager;
use crate::event_manager::HermitEventManager;
use crate::item_count_service::HermitItemCountService;
use crate::item_persistence_service::HermitItemPersistenceService;
use crate::item_repository::HermitItemRepository;
use crate::item_type_lookup::ItemTypeLookup;
use crate::library_manager::HermitLibraryManager;
use crate::people_repository::HermitPeopleRepository;
use crate::user_data_manager::HermitUserDataManager;
use crate::user_entity_ext::set_permission;
use crate::user_manager::HermitUserManager;

/// A config manager returning the factory-default configuration.
struct FixedConfig {
    config: ServerConfiguration,
}

#[async_trait]
impl ServerConfigurationManager for FixedConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        unreachable!("not used in these tests")
    }
    async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
        Ok(self.config.clone())
    }
    async fn update_configuration(
        &self,
        _configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_branding(&self) -> Result<hermit_model::branding::BrandingOptions, ServiceError> {
        Ok(hermit_model::branding::BrandingOptions::default())
    }
    async fn update_branding(
        &self,
        _branding: &hermit_model::branding::BrandingOptions,
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

/// Builds a session manager wired over `db` with the real sibling managers.
fn manager(db: &Database) -> Arc<HermitSessionManager> {
    let config: Arc<dyn ServerConfigurationManager> = Arc::new(FixedConfig {
        config: default_server_configuration(),
    });
    let lookup: Arc<dyn hermit_traits::persistence::ItemTypeLookup> =
        Arc::new(ItemTypeLookup::new());
    let library = Arc::new(HermitLibraryManager::new(
        Arc::new(HermitItemRepository::new(db.clone(), lookup)),
        Arc::new(HermitItemCountService::new(db.clone())),
        Arc::new(HermitItemPersistenceService::new(db.clone())),
        Arc::new(HermitPeopleRepository::new(db.clone())),
    ));
    Arc::new(HermitSessionManager::new(
        Arc::new(HermitUserManager::new(db.clone())),
        Arc::new(HermitDeviceManager::new(db.clone())),
        Arc::new(HermitUserDataManager::new(db.clone(), config)),
        library,
        Arc::new(UnusedDtoService),
        Arc::new(HermitEventManager::new()),
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
            "MaxActiveSessions", "MustUpdatePassword", "NormalizedUsername",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, ?2, '', 1, 1, 1, 0, 0, 0, ?3)"#,
    )
    .bind(id.to_string())
    .bind(username.to_uppercase())
    .bind(username)
    .execute(db.pool())
    .await
    .expect("insert user");
    sqlx::query_as::<_, UserEntity>(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
        .bind(id.to_string())
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
    set_permission(db.pool(), &user.id, PermissionKind::EnableAllDevices, true)
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
    let session: SessionInfoDto = mgr.authenticate_direct(&request).await.unwrap();
    assert_eq!(session.user_id, user_id);
    assert_eq!(session.server_id.as_deref(), Some("server-1"));

    // A device row (with an access token) now exists for the user.
    let token: String =
        sqlx::query_scalar(r#"SELECT "AccessToken" FROM "Devices" WHERE "DeviceId" = ?1"#)
            .bind("dev-1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(!token.is_empty());

    // The freshly minted token resolves back to a session.
    let resolved = mgr
        .get_session_by_authentication_token(&token, "dev-1", "1.2.3.4")
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
    set_permission(db.pool(), &user.id, PermissionKind::EnableAllDevices, true)
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
    mgr.authenticate_direct(&request).await.unwrap();
    let token: String =
        sqlx::query_scalar(r#"SELECT "AccessToken" FROM "Devices" WHERE "DeviceId" = ?1"#)
            .bind("dev-1")
            .fetch_one(db.pool())
            .await
            .unwrap();

    mgr.logout(&token).await.unwrap();

    // The device row is gone and the token no longer resolves.
    let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Devices""#)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    let err = mgr
        .get_session_by_authentication_token(&token, "dev-1", "e")
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
    set_permission(db.pool(), &admin.id, PermissionKind::IsAdministrator, true)
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

#[tokio::test]
async fn send_message_command_requires_remote_control_support() {
    let db = test_db().await;
    let mgr = manager(&db);
    let user = seed_named_user(&db, Uuid::new_v4(), "alice").await;
    let dto = mgr
        .log_session_activity("Web", "1.0", "dev-1", "TV", "e", &user)
        .await
        .unwrap();
    let session_id = dto.id.unwrap();

    // No capabilities / no open connection → not controllable.
    let command = MessageCommand {
        text: "hi".to_owned(),
        ..MessageCommand::default()
    };
    let err = mgr
        .send_message_command("", &session_id, &command)
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::InvalidInput(_)));
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
