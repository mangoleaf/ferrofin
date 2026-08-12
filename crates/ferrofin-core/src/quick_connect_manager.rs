//! [`FerrofinQuickConnect`] — the concrete [`QuickConnect`] pairing flow.
//!
//! Port of `Emby.Server.Implementations.QuickConnect.QuickConnectManager`. Quick
//! Connect is an in-memory pairing dance: a device initiates a request and gets
//! a short numeric **code** plus a long **secret**; a signed-in user authorizes
//! the code; the device then exchanges its secret for an authenticated session.
//! This unit-8 impl keeps the two C# `ConcurrentDictionary`s (pending requests
//! keyed by code, authorized sessions keyed by secret) behind a [`Mutex`], gates
//! everything on the injected
//! [`ServerConfigurationManager`](ferrofin_traits::configuration::ServerConfigurationManager)'s
//! `quick_connect_available` flag, and delegates the actual session creation to
//! the injected [`SessionManager`](ferrofin_traits::session::SessionManager)'s
//! `authenticate_direct` (as the C# manager delegates to `AuthenticateDirect`).
//!
//! The trait's [`QuickConnect::get_authorized_request`] returns a
//! [`SessionInfoDto`] directly (the ported stand-in for the C#
//! `AuthenticationResult` envelope that has no `ferrofin-model` port yet).
//!
//! Expiry uses the same [`REQUEST_TIMEOUT_MINUTES`] window as C# (`Timeout = 10`);
//! stale requests and secrets are swept lazily on each state-changing call.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use ferrofin_model::dto::SessionInfoDto;
use ferrofin_model::quick_connect::QuickConnectResult;
use uuid::Uuid;

use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::security::QuickConnect;
use ferrofin_traits::session::{AuthenticationRequest, SessionManager};

use std::sync::Arc;

/// Minutes a pending request stays live before it expires (C# `Timeout = 10`).
const REQUEST_TIMEOUT_MINUTES: i64 = 10;

/// Number of decimal digits in a user-facing code (C# `CodeLength = 6`).
const CODE_LENGTH: u32 = 6;

/// Byte length of the opaque request secret (C# `GenerateSecureRandom(32)`).
const SECRET_BYTES: usize = 32;

/// An authorized secret's captured session plus the time it was authorized (for
/// expiry), mirroring the C# `(DateTime, AuthenticationResult)` tuple.
struct AuthorizedSecret {
    authorized_at: chrono::DateTime<Utc>,
    session: SessionInfoDto,
}

/// The concrete Quick Connect manager.
pub struct FerrofinQuickConnect {
    configuration_manager: Arc<dyn ServerConfigurationManager>,
    session_manager: Arc<dyn SessionManager>,
    /// Pending requests keyed by their user-facing code.
    current_requests: Mutex<HashMap<String, QuickConnectResult>>,
    /// Authorized sessions keyed by request secret.
    authorized_secrets: Mutex<HashMap<String, AuthorizedSecret>>,
}

impl std::fmt::Debug for FerrofinQuickConnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinQuickConnect")
            .finish_non_exhaustive()
    }
}

impl FerrofinQuickConnect {
    /// Creates a Quick Connect manager from its injected collaborators.
    #[must_use]
    pub fn new(
        configuration_manager: Arc<dyn ServerConfigurationManager>,
        session_manager: Arc<dyn SessionManager>,
    ) -> Self {
        Self {
            configuration_manager,
            session_manager,
            current_requests: Mutex::new(HashMap::new()),
            authorized_secrets: Mutex::new(HashMap::new()),
        }
    }

    /// Errors unless Quick Connect is enabled in configuration (C#
    /// `AssertActive`).
    async fn assert_active(&self) -> Result<(), ServiceError> {
        if self.is_enabled_inner().await? {
            Ok(())
        } else {
            Err(ServiceError::unauthorized(
                "quick connect is not active on this server",
            ))
        }
    }

    /// Reads the live `quick_connect_available` toggle.
    async fn is_enabled_inner(&self) -> Result<bool, ServiceError> {
        Ok(self
            .configuration_manager
            .configuration()
            .await?
            .quick_connect_available)
    }

    /// A pseudo-random `CODE_LENGTH`-digit numeric code, derived from a v4 UUID's
    /// entropy (the C# version fills random bytes; a fresh UUID is an equally
    /// good CSPRNG source here and avoids adding a `rand` dependency).
    fn generate_code() -> String {
        let n = u128::from_be_bytes(*Uuid::new_v4().as_bytes());
        let modulo = 10u128.pow(CODE_LENGTH);
        let min = 10u128.pow(CODE_LENGTH - 1);
        let value = min + (n % (modulo - min));
        value.to_string()
    }

    /// A hex-encoded `SECRET_BYTES`-byte opaque secret.
    fn generate_secret() -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(SECRET_BYTES * 2);
        // Two UUIDs supply 32 bytes of entropy.
        for uuid in [Uuid::new_v4(), Uuid::new_v4()] {
            for byte in uuid.as_bytes() {
                let _ = write!(out, "{byte:02X}");
            }
        }
        out
    }

    /// Removes pending requests and authorized secrets older than the timeout.
    fn expire_stale(&self) {
        let cutoff = Utc::now() - Duration::minutes(REQUEST_TIMEOUT_MINUTES);
        if let Ok(mut requests) = self.current_requests.lock() {
            requests.retain(|_, r| r.date_added >= cutoff);
        }
        if let Ok(mut secrets) = self.authorized_secrets.lock() {
            secrets.retain(|_, s| s.authorized_at >= cutoff);
        }
    }
}

#[async_trait]
impl QuickConnect for FerrofinQuickConnect {
    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        self.is_enabled_inner().await
    }

    async fn try_connect(
        &self,
        authorization_info: &AuthorizationInfo,
    ) -> Result<QuickConnectResult, ServiceError> {
        // C# asserts the four device/client fields are present.
        let device_id = authorization_info
            .device_id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ServiceError::invalid_input("device id is required"))?;
        let device = authorization_info
            .device
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ServiceError::invalid_input("device name is required"))?;
        let client = authorization_info
            .client
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ServiceError::invalid_input("client name is required"))?;
        let version = authorization_info
            .version
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ServiceError::invalid_input("client version is required"))?;

        self.assert_active().await?;
        self.expire_stale();

        let result = QuickConnectResult {
            authenticated: false,
            secret: Self::generate_secret().into(),
            code: Self::generate_code(),
            device_id,
            device_name: device,
            app_name: client,
            app_version: version,
            date_added: Utc::now(),
        };

        self.current_requests
            .lock()
            .map_err(|_| ServiceError::backend("quick connect state poisoned"))?
            .insert(result.code.clone(), result.clone());
        Ok(result)
    }

    async fn check_request_status(&self, secret: &str) -> Result<QuickConnectResult, ServiceError> {
        self.assert_active().await?;
        self.expire_stale();

        let requests = self
            .current_requests
            .lock()
            .map_err(|_| ServiceError::backend("quick connect state poisoned"))?;
        requests
            .values()
            .find(|r| r.secret.expose() == secret)
            .cloned()
            .ok_or_else(|| ServiceError::not_found("unable to find request with provided secret"))
    }

    async fn authorize_request(&self, user_id: Uuid, code: &str) -> Result<bool, ServiceError> {
        self.assert_active().await?;
        self.expire_stale();

        // Snapshot the pending request (release the lock before the async call).
        let request = {
            let requests = self
                .current_requests
                .lock()
                .map_err(|_| ServiceError::backend("quick connect state poisoned"))?;
            requests
                .get(code)
                .cloned()
                .ok_or_else(|| ServiceError::not_found("unable to find request"))?
        };
        if request.authenticated {
            return Err(ServiceError::invalid_input("request is already authorized"));
        }

        // Open a session for the authorizing user with the request's device.
        // The minted access token is not carried through the Quick Connect seam
        // (its `get_authorized_request` surfaces only the session DTO), so only
        // the session is retained here.
        let session = self
            .session_manager
            .authenticate_direct(&AuthenticationRequest {
                user_id: Some(user_id),
                device_id: Some(request.device_id.clone()),
                device_name: Some(request.device_name.clone()),
                app: Some(request.app_name.clone()),
                app_version: Some(request.app_version.clone()),
                ..AuthenticationRequest::default()
            })
            .await?
            .session;

        // Record the authorized secret and flip the request's flag. Push the
        // expiry one minute out so the client can still observe authorization
        // (mirrors the C# `DateAdded = UtcNow + 1min`).
        self.authorized_secrets
            .lock()
            .map_err(|_| ServiceError::backend("quick connect state poisoned"))?
            .insert(
                request.secret.expose().to_owned(),
                AuthorizedSecret {
                    authorized_at: Utc::now(),
                    session,
                },
            );

        let mut requests = self
            .current_requests
            .lock()
            .map_err(|_| ServiceError::backend("quick connect state poisoned"))?;
        if let Some(pending) = requests.get_mut(code) {
            pending.authenticated = true;
            pending.date_added = Utc::now() + Duration::minutes(1);
        }
        Ok(true)
    }

    async fn get_authorized_request(&self, secret: &str) -> Result<SessionInfoDto, ServiceError> {
        self.assert_active().await?;
        self.expire_stale();

        let secrets = self
            .authorized_secrets
            .lock()
            .map_err(|_| ServiceError::backend("quick connect state poisoned"))?;
        secrets
            .get(secret)
            .map(|s| s.session.clone())
            .ok_or_else(|| ServiceError::not_found("unable to find request"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use ferrofin_db::entities::security::DeviceEntity;
    use ferrofin_model::dto::SessionInfoDto;
    use ferrofin_model::session::{
        ClientCapabilities, GeneralCommand, MessageCommand, PlayRequest, PlaybackProgressInfo,
        PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest, SessionMessageType, TranscodingInfo,
    };
    use uuid::Uuid;

    use ferrofin_traits::configuration::ServerConfigurationManager;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::options::AuthorizationInfo;
    use ferrofin_traits::security::QuickConnect;
    use ferrofin_traits::session::{
        AuthenticationRequest, AuthenticationResultData, SessionManager,
    };

    use crate::configuration_manager::default_server_configuration;

    use super::FerrofinQuickConnect;

    struct FixedConfig {
        enabled: bool,
    }
    #[async_trait]
    impl ServerConfigurationManager for FixedConfig {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            unreachable!("not used")
        }
        async fn configuration(
            &self,
        ) -> Result<ferrofin_model::configuration::ServerConfiguration, ServiceError> {
            let mut c = default_server_configuration();
            c.quick_connect_available = self.enabled;
            Ok(c)
        }
        async fn update_configuration(
            &self,
            _config: &ferrofin_model::configuration::ServerConfiguration,
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

    /// A session manager whose `authenticate_direct` returns a DTO tagged with
    /// the authenticated user's id; every other method is an unused stub.
    struct FakeSessionManager;

    #[async_trait]
    impl SessionManager for FakeSessionManager {
        async fn authenticate_direct(
            &self,
            request: &AuthenticationRequest,
        ) -> Result<AuthenticationResultData, ServiceError> {
            Ok(AuthenticationResultData {
                session: SessionInfoDto {
                    user_id: request.user_id.unwrap_or_default(),
                    device_id: request.device_id.clone(),
                    ..SessionInfoDto::default()
                },
                access_token: "quick-connect-token".into(),
            })
        }
        async fn authenticate_new_session(
            &self,
            _request: &AuthenticationRequest,
        ) -> Result<AuthenticationResultData, ServiceError> {
            unimplemented!()
        }
        async fn log_session_activity(
            &self,
            _app_name: &str,
            _app_version: &str,
            _device_id: &str,
            _device_name: &str,
            _remote_endpoint: &str,
            _user: &ferrofin_db::entities::users::UserEntity,
        ) -> Result<SessionInfoDto, ServiceError> {
            unimplemented!()
        }
        async fn update_device_name(
            &self,
            _session_id: &str,
            _reported_device_name: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn on_playback_start(&self, _info: &PlaybackStartInfo) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn on_playback_progress(
            &self,
            _info: &PlaybackProgressInfo,
            _is_automated: bool,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn on_playback_stopped(&self, _info: &PlaybackStopInfo) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn report_session_ended(&self, _session_id: &str) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn send_general_command(
            &self,
            _controlling_session_id: &str,
            _session_id: &str,
            _command: &GeneralCommand,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn send_message_command(
            &self,
            _controlling_session_id: &str,
            _session_id: &str,
            _command: &MessageCommand,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn send_play_command(
            &self,
            _controlling_session_id: &str,
            _session_id: &str,
            _command: &PlayRequest,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn send_playstate_command(
            &self,
            _controlling_session_id: &str,
            _session_id: &str,
            _command: &PlaystateRequest,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn send_message_to_admin_sessions(
            &self,
            _message_type: SessionMessageType,
            _data: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn send_message_to_user_sessions(
            &self,
            _user_ids: &[Uuid],
            _message_type: SessionMessageType,
            _data: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn send_message_to_user_device_sessions(
            &self,
            _device_id: &str,
            _message_type: SessionMessageType,
            _data: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn send_restart_required_notification(&self) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn add_additional_user(
            &self,
            _session_id: &str,
            _user_id: Uuid,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn remove_additional_user(
            &self,
            _session_id: &str,
            _user_id: Uuid,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn report_now_viewing_item(
            &self,
            _session_id: &str,
            _item_id: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn report_capabilities(
            &self,
            _session_id: &str,
            _capabilities: &ClientCapabilities,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn report_transcoding_info(
            &self,
            _device_id: &str,
            _info: &TranscodingInfo,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn clear_transcoding_info(&self, _device_id: &str) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn get_sessions(
            &self,
            _user_id: Uuid,
            _device_id: Option<&str>,
            _active_within_seconds: Option<i32>,
            _controllable_user_to_check: Option<Uuid>,
            _is_api_key: bool,
        ) -> Result<Vec<SessionInfoDto>, ServiceError> {
            unimplemented!()
        }
        async fn get_session_by_authentication_token(
            &self,
            _token: &str,
            _device_id: &str,
            _remote_endpoint: &str,
        ) -> Result<SessionInfoDto, ServiceError> {
            unimplemented!()
        }
        async fn logout(&self, _access_token: &str) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn logout_device(&self, _device: &DeviceEntity) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn revoke_user_tokens(
            &self,
            _user_id: Uuid,
            _current_access_token: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn close_live_stream_if_needed(
            &self,
            _live_stream_id: &str,
            _session_id: &str,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
    }

    fn manager(enabled: bool) -> FerrofinQuickConnect {
        FerrofinQuickConnect::new(
            Arc::new(FixedConfig { enabled }),
            Arc::new(FakeSessionManager),
        )
    }

    fn auth_info() -> AuthorizationInfo {
        AuthorizationInfo {
            device_id: Some("dev-1".to_owned()),
            device: Some("Phone".to_owned()),
            client: Some("Jellyfin Mobile".to_owned()),
            version: Some("1.0".to_owned()),
            ..AuthorizationInfo::default()
        }
    }

    #[tokio::test]
    async fn full_pairing_flow() {
        let mgr = manager(true);
        let user_id = Uuid::new_v4();

        let request = mgr.try_connect(&auth_info()).await.expect("connect");
        assert!(!request.authenticated);
        assert_eq!(request.code.len(), 6);

        // Status by secret is visible and still unauthenticated.
        let status = mgr
            .check_request_status(request.secret.expose())
            .await
            .expect("status");
        assert!(!status.authenticated);

        // User authorizes the code.
        assert!(
            mgr.authorize_request(user_id, &request.code)
                .await
                .expect("authorize")
        );

        // The device exchanges its secret for the authenticated session.
        let session = mgr
            .get_authorized_request(request.secret.expose())
            .await
            .expect("authorized");
        assert_eq!(session.user_id, user_id);
        assert_eq!(session.device_id.as_deref(), Some("dev-1"));

        // Re-authorizing the same code now fails.
        let err = mgr
            .authorize_request(user_id, &request.code)
            .await
            .expect_err("double");
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn disabled_rejects_everything() {
        let mgr = manager(false);
        assert!(!mgr.is_enabled().await.expect("enabled"));
        let err = mgr.try_connect(&auth_info()).await.expect_err("disabled");
        assert!(matches!(err, ServiceError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn missing_device_fields_are_rejected() {
        let mgr = manager(true);
        let err = mgr
            .try_connect(&AuthorizationInfo::default())
            .await
            .expect_err("no device");
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn unknown_secret_and_code_are_not_found() {
        let mgr = manager(true);
        assert!(matches!(
            mgr.check_request_status("nope").await.expect_err("secret"),
            ServiceError::NotFound(_)
        ));
        assert!(matches!(
            mgr.authorize_request(Uuid::new_v4(), "000000")
                .await
                .expect_err("code"),
            ServiceError::NotFound(_)
        ));
        assert!(matches!(
            mgr.get_authorized_request("nope").await.expect_err("auth"),
            ServiceError::NotFound(_)
        ));
    }
}
