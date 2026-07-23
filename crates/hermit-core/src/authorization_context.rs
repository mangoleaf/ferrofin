//! [`HermitAuthorizationContext`] + [`HermitAuthService`] — request → auth-info.
//!
//! Port of `Jellyfin.Server.Implementations.Security.AuthorizationContext` (the
//! `IAuthorizationContext` implementation) and the trivial `IAuthService`
//! wrapper that promotes an unauthenticated request into an error.
//!
//! The context parses the `MediaBrowser`/`Emby` authorization header (and the
//! legacy `X-Emby-Token` / `X-MediaBrowser-Token` headers and `ApiKey`/`api_key`
//! query parameters when legacy authorization is enabled), then resolves the
//! token against the `Devices` and `ApiKeys` tables, filling in the client /
//! device / version fields the client omitted.
//!
//! Dependency-injection boundaries (per the Wave-4 rule):
//! - the user lookup goes through the injected [`UserManager`] trait object;
//! - server-identity fallbacks for the api-key branch use the injected
//!   [`ServerApplicationHost`] (friendly name) plus a `system_id` /
//!   `server_version` supplied by the composition root, because the host trait
//!   does not surface those two as synchronous getters;
//! - `EnableLegacyAuthorization` is read from the injected
//!   [`ServerConfigurationManager`].
//!
//! Faithful port simplifications:
//! - the C# per-request `HttpContext.Items["AuthorizationInfo"]` cache is dropped
//!   (the HTTP layer, Wave 7, may cache the resolved value itself);
//! - token freshness bookkeeping is preserved: a device seen more than
//!   [`DEVICE_ACTIVITY_REFRESH_MINUTES`] ago, or reporting a changed
//!   device-name/app-version, has its row updated (skipping cast receivers,
//!   matching upstream `allowTokenInfoUpdate`).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use hermit_db::Database;
use hermit_db::entities::security::{ApiKeyEntity, DeviceEntity};
use uuid::Uuid;

use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::error::ServiceError;
use hermit_traits::library::UserManager;
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::AuthorizationInfo;
use hermit_traits::system::ServerApplicationHost;

use crate::db_error::db_err;

/// How stale a device's `DateLastActivity` may be before a request touch
/// refreshes it (C# `> 3` minutes). Beyond this window the device row's
/// last-activity timestamp is rewritten to now.
const DEVICE_ACTIVITY_REFRESH_MINUTES: i64 = 3;

/// The concrete authorization context.
#[derive(Clone)]
pub struct HermitAuthorizationContext {
    db: Database,
    user_manager: Arc<dyn UserManager>,
    application_host: Arc<dyn ServerApplicationHost>,
    configuration_manager: Arc<dyn ServerConfigurationManager>,
    /// The server's unique id (C# `IServerApplicationHost.SystemId`), used as the
    /// api-key device-id fallback. Injected because the host trait has no
    /// synchronous getter for it.
    system_id: String,
    /// The server's version string (C#
    /// `IServerApplicationHost.ApplicationVersionString`), the api-key version
    /// fallback. Injected for the same reason.
    server_version: String,
}

impl std::fmt::Debug for HermitAuthorizationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitAuthorizationContext")
            .field("system_id", &self.system_id)
            .finish_non_exhaustive()
    }
}

impl HermitAuthorizationContext {
    /// Creates the authorization context over its dependencies.
    #[must_use]
    pub fn new(
        db: Database,
        user_manager: Arc<dyn UserManager>,
        application_host: Arc<dyn ServerApplicationHost>,
        configuration_manager: Arc<dyn ServerConfigurationManager>,
        system_id: impl Into<String>,
        server_version: impl Into<String>,
    ) -> Self {
        Self {
            db,
            user_manager,
            application_host,
            configuration_manager,
            system_id: system_id.into(),
            server_version: server_version.into(),
        }
    }

    /// Whether legacy (Emby-era) authorization is enabled in the current
    /// configuration, gating the legacy header/query fallbacks.
    async fn legacy_authorization_enabled(&self) -> Result<bool, ServiceError> {
        Ok(self
            .configuration_manager
            .configuration()
            .await?
            .enable_legacy_authorization)
    }

    /// Resolves the access token from the parsed auth header fields, then the
    /// legacy headers / query parameters (the latter gated on legacy auth).
    async fn resolve_token(
        &self,
        request: &RequestContext,
        header_token: Option<&str>,
    ) -> Result<Option<String>, ServiceError> {
        if let Some(token) = header_token.filter(|t| !t.is_empty()) {
            return Ok(Some(token.to_owned()));
        }

        let legacy = self.legacy_authorization_enabled().await?;
        if legacy {
            for header in ["X-Emby-Token", "X-MediaBrowser-Token"] {
                if let Some(token) = request.header(header).filter(|t| !t.is_empty()) {
                    return Ok(Some(token.to_owned()));
                }
            }
        }

        if let Some(token) = query_value(request, "ApiKey").filter(|t| !t.is_empty()) {
            return Ok(Some(token));
        }
        if let Some(token) = query_value(request, "api_key").filter(|t| legacy && !t.is_empty()) {
            return Ok(Some(token));
        }
        Ok(None)
    }

    /// Reads the (single) authorization header, honouring the legacy
    /// `X-Emby-Authorization` header when legacy auth is enabled.
    async fn authorization_header<'r>(
        &self,
        request: &'r RequestContext,
    ) -> Result<Option<&'r str>, ServiceError> {
        if let Some(value) = request.header("Authorization") {
            return Ok(Some(value));
        }
        if self.legacy_authorization_enabled().await? {
            return Ok(request.header("X-Emby-Authorization"));
        }
        Ok(None)
    }

    /// Resolves a device token into the auth info, refreshing stale/changed
    /// device rows (C# device branch). Returns `true` when the token matched a
    /// device.
    async fn resolve_device_token(
        &self,
        info: &mut AuthorizationInfo,
        token: &str,
    ) -> Result<bool, ServiceError> {
        let Some(mut device) = self.device_by_token(token).await? else {
            return Ok(false);
        };

        info.is_authenticated = true;
        let mut update = false;

        if info.client.as_deref().unwrap_or("").trim().is_empty() {
            info.client = Some(device.app_name.clone());
        }
        if info.device_id.as_deref().unwrap_or("").trim().is_empty() {
            info.device_id = Some(device.device_id.clone());
        }

        // Casting devices share a token; don't rewrite the row from their reports.
        let allow_update = !info
            .client
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("chromecast");

        match info.device.as_deref().map(str::trim) {
            None | Some("") => info.device = Some(device.device_name.clone()),
            Some(name) if !name.eq_ignore_ascii_case(&device.device_name) && allow_update => {
                update = true;
                device.device_name = name.to_owned();
            }
            _ => {}
        }

        match info.version.as_deref().map(str::trim) {
            None | Some("") => info.version = Some(device.app_version.clone()),
            Some(version) if !version.eq_ignore_ascii_case(&device.app_version) && allow_update => {
                update = true;
                device.app_version = version.to_owned();
            }
            _ => {}
        }

        if (Utc::now() - device.date_last_activity).num_minutes() > DEVICE_ACTIVITY_REFRESH_MINUTES
        {
            device.date_last_activity = Utc::now();
            update = true;
        }

        info.user = match Uuid::parse_str(&device.user_id) {
            Ok(id) => self.user_manager.get_user_by_id(id).await?,
            Err(_) => None,
        };

        if update {
            device.date_modified = Utc::now();
            self.update_device(&device).await?;
        }
        Ok(true)
    }

    /// Resolves an api-key token into the auth info (C# api-key branch). Returns
    /// `true` when the token matched a key.
    async fn resolve_api_key_token(
        &self,
        info: &mut AuthorizationInfo,
        token: &str,
    ) -> Result<bool, ServiceError> {
        let Some(key) = self.api_key_by_token(token).await? else {
            return Ok(false);
        };

        info.is_authenticated = true;
        info.client = Some(key.name);
        info.token = Some(key.access_token);
        if info.device_id.as_deref().unwrap_or("").trim().is_empty() {
            info.device_id = Some(self.system_id.clone());
        }
        if info.device.as_deref().unwrap_or("").trim().is_empty() {
            info.device = Some(self.application_host.friendly_name());
        }
        if info.version.as_deref().unwrap_or("").trim().is_empty() {
            info.version = Some(self.server_version.clone());
        }
        info.is_api_key = true;
        Ok(true)
    }

    /// Looks up the most-recently-active device row bearing this access token.
    async fn device_by_token(&self, token: &str) -> Result<Option<DeviceEntity>, ServiceError> {
        sqlx::query_as::<_, DeviceEntity>(
            r#"SELECT * FROM "Devices" WHERE "AccessToken" = ?1
               ORDER BY "DateLastActivity" DESC LIMIT 1"#,
        )
        .bind(token)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)
    }

    /// Looks up the api-key row bearing this access token.
    async fn api_key_by_token(&self, token: &str) -> Result<Option<ApiKeyEntity>, ServiceError> {
        sqlx::query_as::<_, ApiKeyEntity>(
            r#"SELECT * FROM "ApiKeys" WHERE "AccessToken" = ?1 LIMIT 1"#,
        )
        .bind(token)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)
    }

    /// Persists a device row's refreshed name/version/activity (C#
    /// `IDeviceManager.UpdateDevice`). Kept local so the context needs no
    /// `DeviceManager` handle just to write back the columns it touched.
    async fn update_device(&self, device: &DeviceEntity) -> Result<(), ServiceError> {
        sqlx::query(
            r#"UPDATE "Devices" SET
                "DeviceName" = ?2, "AppVersion" = ?3,
                "DateLastActivity" = ?4, "DateModified" = ?5
               WHERE "Id" = ?1"#,
        )
        .bind(device.id)
        .bind(&device.device_name)
        .bind(&device.app_version)
        .bind(device.date_last_activity)
        .bind(device.date_modified)
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }
}

#[async_trait]
impl AuthorizationContext for HermitAuthorizationContext {
    async fn get_authorization_info(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        let header = self.authorization_header(request).await?;
        let legacy = self.legacy_authorization_enabled().await?;
        let parts = header.and_then(|h| parse_authorization_header(h, legacy));

        let mut info = AuthorizationInfo {
            device_id: parts.as_ref().and_then(|p| p.get("DeviceId").cloned()),
            device: parts.as_ref().and_then(|p| p.get("Device").cloned()),
            client: parts.as_ref().and_then(|p| p.get("Client").cloned()),
            version: parts.as_ref().and_then(|p| p.get("Version").cloned()),
            token: None,
            is_api_key: false,
            user: None,
            is_authenticated: false,
        };

        let header_token = parts
            .as_ref()
            .and_then(|p| p.get("Token").map(String::as_str));
        info.token = self.resolve_token(request, header_token).await?;

        if !info.has_token() {
            return Ok(info);
        }
        let token = info.token.clone().unwrap_or_default();

        // Device tokens take precedence over api keys (C# order).
        if !self.resolve_device_token(&mut info, &token).await? {
            self.resolve_api_key_token(&mut info, &token).await?;
        }
        Ok(info)
    }
}

#[async_trait]
impl AuthService for HermitAuthorizationContext {
    async fn authenticate(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        let info = self.get_authorization_info(request).await?;
        if info.is_authenticated {
            Ok(info)
        } else {
            Err(ServiceError::unauthorized(
                "Request does not contain valid authentication credentials.",
            ))
        }
    }
}

/// A distinct newtype so the [`AuthService`] impl can be handed out
/// independently of the [`AuthorizationContext`] when the composition root wants
/// two separate `Arc<dyn _>` handles over the same logic. Delegates to the
/// wrapped [`HermitAuthorizationContext`].
#[derive(Clone, Debug)]
pub struct HermitAuthService {
    inner: HermitAuthorizationContext,
}

impl HermitAuthService {
    /// Wraps an existing authorization context as an [`AuthService`].
    #[must_use]
    pub fn new(inner: HermitAuthorizationContext) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl AuthService for HermitAuthService {
    async fn authenticate(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        self.inner.authenticate(request).await
    }
}

/// Reads the first value of a query-string parameter, case-insensitively on the
/// key. The `RequestContext` carries the raw query string (no leading `?`).
fn query_value(request: &RequestContext, key: &str) -> Option<String> {
    let query = request.query_string.as_deref()?;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k.eq_ignore_ascii_case(key) {
            return Some(url_decode(v));
        }
    }
    None
}

/// Parses a `MediaBrowser`/`Emby` authorization header into its component map
/// (C# `GetAuthorization` + `GetParts`). Returns `None` when the scheme name is
/// missing or is not a recognised scheme (`Emby` only when `legacy`).
fn parse_authorization_header(
    header: &str,
    legacy: bool,
) -> Option<std::collections::HashMap<String, String>> {
    let (scheme, rest) = header.split_once(' ')?;
    let valid = scheme.eq_ignore_ascii_case("MediaBrowser")
        || (legacy && scheme.eq_ignore_ascii_case("Emby"));
    if !valid {
        return None;
    }
    Some(parse_parts(rest))
}

/// Parses the comma-separated `Key="Value"` parts of an authorization header
/// (C# `GetParts`), URL-decoding each value and honouring quoted values that may
/// themselves contain commas.
fn parse_parts(header: &str) -> std::collections::HashMap<String, String> {
    let mut result = std::collections::HashMap::new();
    let chars: Vec<char> = header.chars().collect();
    let mut escaped = false;
    let mut start = 0usize;
    let mut key = String::new();

    let mut i = 0usize;
    while i < chars.len() {
        let token = chars[i];
        if token == '"' || token == ',' {
            // A quote toggles the escape state; a comma closes a value only when
            // not inside quotes (mirrors the C# `escaped` XOR bookkeeping).
            let is_quote = token == '"';
            escaped = if is_quote { !escaped } else { escaped };
            if token == ',' && !escaped && start < i {
                let raw: String = chars[start..i].iter().collect();
                result.insert(std::mem::take(&mut key), url_decode(raw.trim_matches('"')));
                start = i + 1;
            } else if token == ',' && !escaped {
                start = i + 1;
            }
        } else if !escaped && token == '=' {
            let raw: String = chars[start..i].iter().collect();
            key = String::from(raw.trim());
            start = i + 1;
        }
        i += 1;
    }

    if start < i {
        let raw: String = chars[start..i].iter().collect();
        result.insert(key, url_decode(raw.trim_matches('"')));
    }
    result
}

/// Minimal `application/x-www-form-urlencoded` decoder for header/query values:
/// `+` → space and `%XX` → byte, leaving anything malformed as-is. Sufficient
/// for the client/device/version/token fields Jellyfin sends.
fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    // Two hex digits are always in `0..=255`, so this never fails.
                    out.push(u8::try_from(hi * 16 + lo).unwrap_or(b'?'));
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration_manager::default_server_configuration;
    use crate::user_manager::HermitUserManager;
    use chrono::Utc;
    use hermit_db::Database;
    use hermit_model::configuration::ServerConfiguration;
    use hermit_traits::net::RequestContext as HostRequestContext;
    use hermit_traits::system::ServerApplicationPaths;

    /// A minimal [`ServerConfigurationManager`] returning a fixed configuration;
    /// only [`configuration`](ServerConfigurationManager::configuration) is
    /// exercised by the authorization context.
    struct FakeConfig {
        config: ServerConfiguration,
    }

    #[async_trait]
    impl ServerConfigurationManager for FakeConfig {
        fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
            unreachable!("not used by the authorization context")
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
    }

    /// A minimal [`ServerApplicationHost`] exposing only the friendly name the
    /// api-key branch reads; the remaining methods are unreachable in these tests.
    struct FakeHost {
        name: String,
    }

    #[async_trait]
    impl ServerApplicationHost for FakeHost {
        fn core_startup_has_completed(&self) -> bool {
            true
        }
        fn http_port(&self) -> u16 {
            8096
        }
        fn https_port(&self) -> u16 {
            8920
        }
        fn listen_with_https(&self) -> bool {
            false
        }
        fn friendly_name(&self) -> String {
            self.name.clone()
        }
        async fn get_smart_api_url(
            &self,
            _request: &HostRequestContext,
        ) -> Result<String, ServiceError> {
            unreachable!()
        }
        async fn get_local_api_url(
            &self,
            _hostname: &str,
            _scheme: Option<&str>,
            _port: Option<u16>,
        ) -> Result<String, ServiceError> {
            unreachable!()
        }
        fn expand_virtual_path(&self, path: &str) -> String {
            path.to_owned()
        }
        fn reverse_virtual_path(&self, path: &str) -> String {
            path.to_owned()
        }
    }

    #[test]
    fn parses_authorization_header_parts() {
        let header = r#"MediaBrowser Client="Web", Device="Firefox", DeviceId="abc", Version="1.2", Token="tok""#;
        let parts = parse_authorization_header(header, false).expect("parsed");
        assert_eq!(parts.get("Client").map(String::as_str), Some("Web"));
        assert_eq!(parts.get("Device").map(String::as_str), Some("Firefox"));
        assert_eq!(parts.get("DeviceId").map(String::as_str), Some("abc"));
        assert_eq!(parts.get("Version").map(String::as_str), Some("1.2"));
        assert_eq!(parts.get("Token").map(String::as_str), Some("tok"));
    }

    #[test]
    fn rejects_unknown_scheme_and_emby_without_legacy() {
        assert!(parse_authorization_header("Bearer abc", true).is_none());
        assert!(parse_authorization_header(r#"Emby Token="t""#, false).is_none());
        assert!(parse_authorization_header(r#"Emby Token="t""#, true).is_some());
    }

    #[test]
    fn url_decode_handles_plus_and_percent() {
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(url_decode("bad%zz"), "bad%zz");
    }

    /// Builds a context over a database, wired to fake host/config dependencies.
    fn context(db: Database) -> HermitAuthorizationContext {
        let config = Arc::new(FakeConfig {
            config: default_server_configuration(),
        });
        let host = Arc::new(FakeHost {
            name: "test-machine".to_owned(),
        });
        let users = Arc::new(HermitUserManager::new(db.clone()));
        HermitAuthorizationContext::new(db, users, host, config, "sys-1", "10.9.0")
    }

    #[tokio::test]
    async fn no_token_is_unauthenticated() {
        let db = crate::test_support::test_db().await;
        let ctx = context(db);
        let info = ctx
            .get_authorization_info(&RequestContext::default())
            .await
            .expect("info");
        assert!(!info.is_authenticated);
        assert!(!info.is_api_key);
    }

    #[tokio::test]
    async fn api_key_token_authenticates_with_server_fallbacks() {
        let db = crate::test_support::test_db().await;
        sqlx::query(
            r#"INSERT INTO "ApiKeys" ("AccessToken", "DateCreated", "DateLastActivity", "Name")
               VALUES (?1, ?2, ?2, ?3)"#,
        )
        .bind("key-tok")
        .bind(Utc::now())
        .bind("Automation")
        .execute(db.pool())
        .await
        .expect("insert api key");

        let ctx = context(db);
        let request = RequestContext {
            headers: vec![(
                "Authorization".to_owned(),
                r#"MediaBrowser Token="key-tok""#.to_owned(),
            )],
            ..Default::default()
        };
        let info = ctx.get_authorization_info(&request).await.expect("info");
        assert!(info.is_authenticated);
        assert!(info.is_api_key);
        assert_eq!(info.client.as_deref(), Some("Automation"));
        assert_eq!(info.device_id.as_deref(), Some("sys-1"));
        assert_eq!(info.version.as_deref(), Some("10.9.0"));
        assert_eq!(info.device.as_deref(), Some("test-machine"));
    }

    #[tokio::test]
    async fn device_token_authenticates_and_resolves_user() {
        let db = crate::test_support::test_db().await;
        let uid = Uuid::from_u128(7);
        crate::test_support::seed_user(&db, uid).await;
        let now = Utc::now();
        sqlx::query(
            r#"INSERT INTO "Devices"
               ("AccessToken", "AppName", "AppVersion", "DateCreated",
                "DateLastActivity", "DateModified", "DeviceId", "DeviceName",
                "IsActive", "UserId")
               VALUES (?1, ?2, ?3, ?4, ?4, ?4, ?5, ?6, 1, ?7)"#,
        )
        .bind("dev-tok")
        .bind("Web")
        .bind("1.0")
        .bind(now)
        .bind("dev-1")
        .bind("Firefox")
        .bind(uid.to_string())
        .execute(db.pool())
        .await
        .expect("insert device");

        let ctx = context(db);
        let request = RequestContext {
            headers: vec![(
                "Authorization".to_owned(),
                r#"MediaBrowser Token="dev-tok""#.to_owned(),
            )],
            ..Default::default()
        };
        let info = ctx.get_authorization_info(&request).await.expect("info");
        assert!(info.is_authenticated);
        assert!(!info.is_api_key);
        assert_eq!(info.client.as_deref(), Some("Web"));
        assert_eq!(info.device.as_deref(), Some("Firefox"));
        assert_eq!(info.user_id(), uid);
    }

    #[tokio::test]
    async fn auth_service_rejects_missing_credentials() {
        let db = crate::test_support::test_db().await;
        let ctx = context(db);
        let err = AuthService::authenticate(&ctx, &RequestContext::default())
            .await
            .expect_err("should reject");
        assert!(matches!(err, ServiceError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn api_key_via_query_parameter() {
        let db = crate::test_support::test_db().await;
        sqlx::query(
            r#"INSERT INTO "ApiKeys" ("AccessToken", "DateCreated", "DateLastActivity", "Name")
               VALUES (?1, ?2, ?2, ?3)"#,
        )
        .bind("qtok")
        .bind(Utc::now())
        .bind("QueryKey")
        .execute(db.pool())
        .await
        .expect("insert");

        let ctx = context(db);
        let request = RequestContext {
            query_string: Some("ApiKey=qtok&foo=bar".to_owned()),
            ..Default::default()
        };
        let info = ctx.get_authorization_info(&request).await.expect("info");
        assert!(info.is_authenticated);
        assert!(info.is_api_key);
        assert_eq!(info.client.as_deref(), Some("QueryKey"));
    }
}
