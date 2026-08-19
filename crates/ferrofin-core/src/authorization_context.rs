//! [`FerrofinAuthorizationContext`] + [`FerrofinAuthService`] — request → auth-info.
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

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ferrofin_db::Database;
use ferrofin_db::entities::security::{ApiKeyEntity, DeviceEntity};
use ferrofin_db::store::datetime_to_db;
use uuid::Uuid;

use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::UserManager;
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::system::ServerApplicationHost;

use crate::auth_cache::AuthCache;
use crate::db_error::db_err;

/// How stale a device's `DateLastActivity` may be before a request touch
/// refreshes it (C# `> 3` minutes). Beyond this window the device row's
/// last-activity timestamp is rewritten to now.
const DEVICE_ACTIVITY_REFRESH_MINUTES: i64 = 3;

/// The concrete authorization context.
#[derive(Clone)]
pub struct FerrofinAuthorizationContext {
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
    /// The shared token-resolution cache (see [`AuthCache`]): drops the
    /// two-query-per-request floor on the authenticated hot path. The
    /// composition root installs the instance the user/device managers clear
    /// on auth-relevant mutations; the default (private) instance is only for
    /// tests and still TTL-correct.
    auth_cache: Arc<AuthCache>,
}

impl std::fmt::Debug for FerrofinAuthorizationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinAuthorizationContext")
            .field("system_id", &self.system_id)
            .finish_non_exhaustive()
    }
}

impl FerrofinAuthorizationContext {
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
            auth_cache: Arc::new(AuthCache::default()),
        }
    }

    /// Installs the shared [`AuthCache`] (composition root only) — the same
    /// instance must be handed to the user/device managers so their mutations
    /// invalidate what this context serves.
    #[must_use]
    pub fn with_auth_cache(mut self, auth_cache: Arc<AuthCache>) -> Self {
        self.auth_cache = auth_cache;
        self
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
    ///
    /// `legacy` is the already-resolved `EnableLegacyAuthorization` flag: the
    /// caller reads it once per request so this path never re-clones the whole
    /// [`ferrofin_model::configuration::ServerConfiguration`].
    fn resolve_token(
        request: &RequestContext,
        header_token: Option<String>,
        legacy: bool,
    ) -> Option<String> {
        if let Some(token) = header_token.filter(|t| !t.is_empty()) {
            return Some(token);
        }

        if legacy {
            for header in ["X-Emby-Token", "X-MediaBrowser-Token"] {
                if let Some(token) = request.header(header).filter(|t| !t.is_empty()) {
                    return Some(token.to_owned());
                }
            }
        }

        if let Some(token) = query_value(request, "ApiKey").filter(|t| !t.is_empty()) {
            return Some(token.into_owned());
        }
        if let Some(token) = query_value(request, "api_key").filter(|t| legacy && !t.is_empty()) {
            return Some(token.into_owned());
        }
        None
    }

    /// Reads the (single) authorization header, honouring the legacy
    /// `X-Emby-Authorization` header when legacy auth is enabled.
    ///
    /// Takes the pre-resolved `legacy` flag for the same reason as
    /// [`Self::resolve_token`].
    fn authorization_header(request: &RequestContext, legacy: bool) -> Option<&str> {
        if let Some(value) = request.header("Authorization") {
            return Some(value);
        }
        if legacy {
            return request.header("X-Emby-Authorization");
        }
        None
    }

    /// Resolves a device token into the auth info, refreshing stale/changed
    /// device rows (C# device branch). Returns `true` when the token matched a
    /// device.
    async fn resolve_device_token(
        &self,
        info: &mut AuthorizationInfo,
        token: &str,
    ) -> Result<bool, ServiceError> {
        // Read-through: a fresh cached resolution answers without touching the
        // database (the two per-request queries this path otherwise costs).
        // The row-refresh bookkeeping below is deliberately skipped on a hit —
        // it re-runs on the next TTL expiry, the same cadence Jellyfin's
        // in-memory device map effectively gives it.
        if let Some((device, user)) = self.auth_cache.get(token) {
            info.is_authenticated = true;
            fill_blank_client_fields(info, &device);
            info.user = user;
            return Ok(true);
        }

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
        // Cache the row exactly as it now exists (post-refresh), so a hit
        // serves what a re-read would.
        self.auth_cache.put(token, device, info.user.clone());
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
        info.token = Some(key.access_token.into());
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
        .bind(datetime_to_db(device.date_last_activity))
        .bind(datetime_to_db(device.date_modified))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }
}

#[async_trait]
impl AuthorizationContext for FerrofinAuthorizationContext {
    async fn get_authorization_info(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        // The one configuration read of the request: every legacy fallback
        // below is gated on this flag, and re-reading it deep-clones the whole
        // `ServerConfiguration`.
        let legacy = self.legacy_authorization_enabled().await?;
        let header = Self::authorization_header(request, legacy);
        let mut parts = header
            .and_then(|h| parse_authorization_header(h, legacy))
            .unwrap_or_default();

        let mut info = AuthorizationInfo {
            device_id: parts.device_id.take(),
            device: parts.device.take(),
            client: parts.client.take(),
            version: parts.version.take(),
            token: None,
            is_api_key: false,
            user: None,
            is_authenticated: false,
        };

        info.token = Self::resolve_token(request, parts.token.take(), legacy).map(Into::into);

        if !info.has_token() {
            return Ok(info);
        }
        let token = info.token.clone().unwrap_or_default();

        // Device tokens take precedence over api keys (C# order).
        if !self.resolve_device_token(&mut info, token.expose()).await? {
            self.resolve_api_key_token(&mut info, token.expose())
                .await?;
        }
        Ok(info)
    }
}

#[async_trait]
impl AuthService for FerrofinAuthorizationContext {
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
/// wrapped [`FerrofinAuthorizationContext`].
#[derive(Clone, Debug)]
pub struct FerrofinAuthService {
    inner: FerrofinAuthorizationContext,
}

impl FerrofinAuthService {
    /// Wraps an existing authorization context as an [`AuthService`].
    #[must_use]
    pub fn new(inner: FerrofinAuthorizationContext) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl AuthService for FerrofinAuthService {
    async fn authenticate(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        self.inner.authenticate(request).await
    }
}

/// Fills the auth info's blank client/device fields from a device row — the
/// hit-path half of `resolve_device_token`'s defaults (the C# branch that only
/// *reads* the row; the rename/refresh half runs on cache misses only).
fn fill_blank_client_fields(
    info: &mut AuthorizationInfo,
    device: &ferrofin_db::entities::security::DeviceEntity,
) {
    if info.client.as_deref().unwrap_or("").trim().is_empty() {
        info.client = Some(device.app_name.clone());
    }
    if info.device_id.as_deref().unwrap_or("").trim().is_empty() {
        info.device_id = Some(device.device_id.clone());
    }
    if info.device.as_deref().unwrap_or("").trim().is_empty() {
        info.device = Some(device.device_name.clone());
    }
    if info.version.as_deref().unwrap_or("").trim().is_empty() {
        info.version = Some(device.app_version.clone());
    }
}

/// Reads the first value of a query-string parameter, case-insensitively on the
/// key. The `RequestContext` carries the raw query string (no leading `?`).
fn query_value<'r>(request: &'r RequestContext, key: &str) -> Option<Cow<'r, str>> {
    let query = request.query_string.as_deref()?;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k.eq_ignore_ascii_case(key) {
            return Some(url_decode(v));
        }
    }
    None
}

/// The five authorization-header fields Ferrofin consumes (C# reads exactly
/// these out of `GetParts`' dictionary). Holding them in a struct instead of a
/// `HashMap<String, String>` keeps the per-request cost to at most one
/// allocation per *present* field — no map, no owned keys, and nothing at all
/// for parts the server never reads.
///
/// Keys are matched **case-sensitively**, matching the ordinal
/// `Dictionary<string, string>` lookups upstream (and the previous map).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct AuthorizationParts {
    /// `DeviceId="…"`.
    device_id: Option<String>,
    /// `Device="…"` (the device's display name).
    device: Option<String>,
    /// `Client="…"`.
    client: Option<String>,
    /// `Version="…"`.
    version: Option<String>,
    /// `Token="…"` (the access token).
    token: Option<String>,
}

impl AuthorizationParts {
    /// Records one `key`/raw-`value` pair, URL-decoding the value and stripping
    /// its surrounding quotes. Unrecognised keys are dropped.
    fn set(&mut self, key: &str, raw: &str) {
        let slot = match key {
            "DeviceId" => &mut self.device_id,
            "Device" => &mut self.device,
            "Client" => &mut self.client,
            "Version" => &mut self.version,
            "Token" => &mut self.token,
            _ => return,
        };
        *slot = Some(url_decode(raw.trim_matches('"')).into_owned());
    }
}

/// Parses a `MediaBrowser`/`Emby` authorization header into its component
/// fields (C# `GetAuthorization` + `GetParts`). Returns `None` when the scheme
/// name is missing or is not a recognised scheme (`Emby` only when `legacy`).
fn parse_authorization_header(header: &str, legacy: bool) -> Option<AuthorizationParts> {
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
///
/// Scans `header.as_bytes()` and slices the original `&str`. Every byte the
/// state machine reacts to (`"`, `,`, `=`) is ASCII, and UTF-8 continuation
/// bytes are all `>= 0x80`, so a byte scan visits exactly the same decision
/// points a `char` scan did and every recorded offset lands on a character
/// boundary — the slicing is infallible and the parse is byte-for-byte the one
/// the `Vec<char>` version produced.
fn parse_parts(header: &str) -> AuthorizationParts {
    let mut result = AuthorizationParts::default();
    let bytes = header.as_bytes();
    let mut escaped = false;
    let mut start = 0usize;
    let mut key: &str = "";

    for (i, &token) in bytes.iter().enumerate() {
        match token {
            // A quote toggles the escape state; a comma closes a value only when
            // not inside quotes (mirrors the C# `escaped` XOR bookkeeping).
            b'"' => escaped = !escaped,
            b',' if !escaped => {
                if start < i {
                    result.set(key, &header[start..i]);
                    key = "";
                }
                start = i + 1;
            }
            b'=' if !escaped => {
                key = header[start..i].trim();
                start = i + 1;
            }
            _ => {}
        }
    }

    if start < bytes.len() {
        result.set(key, &header[start..]);
    }
    result
}

/// Minimal `application/x-www-form-urlencoded` decoder for header/query values:
/// `+` → space and `%XX` → byte, leaving anything malformed as-is. Sufficient
/// for the client/device/version/token fields Jellyfin sends.
///
/// Borrows when there is nothing to decode — the overwhelmingly common case for
/// the auth header on every request — so the hot path allocates only for values
/// that really are encoded.
fn url_decode(input: &str) -> Cow<'_, str> {
    let bytes = input.as_bytes();
    if !bytes.iter().any(|b| matches!(b, b'+' | b'%')) {
        return Cow::Borrowed(input);
    }
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
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration_manager::default_server_configuration;
    use crate::user_manager::FerrofinUserManager;
    use chrono::Utc;
    use ferrofin_db::Database;
    use ferrofin_model::configuration::ServerConfiguration;
    use ferrofin_traits::net::RequestContext as HostRequestContext;
    use ferrofin_traits::system::ServerApplicationPaths;

    /// A minimal [`ServerConfigurationManager`] returning a fixed configuration;
    /// only [`configuration`](ServerConfigurationManager::configuration) is
    /// exercised by the authorization context.
    struct FakeConfig {
        config: ServerConfiguration,
        /// How many times the whole `ServerConfiguration` was deep-cloned out
        /// of this manager — the cost finding #8 is about.
        reads: std::sync::atomic::AtomicUsize,
    }

    impl FakeConfig {
        fn new(config: ServerConfiguration) -> Self {
            Self {
                config,
                reads: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn reads(&self) -> usize {
            self.reads.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ServerConfigurationManager for FakeConfig {
        fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
            unreachable!("not used by the authorization context")
        }

        async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.config.clone())
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
        assert_eq!(parts.client.as_deref(), Some("Web"));
        assert_eq!(parts.device.as_deref(), Some("Firefox"));
        assert_eq!(parts.device_id.as_deref(), Some("abc"));
        assert_eq!(parts.version.as_deref(), Some("1.2"));
        assert_eq!(parts.token.as_deref(), Some("tok"));
    }

    #[test]
    fn rejects_unknown_scheme_and_emby_without_legacy() {
        assert!(parse_authorization_header("Bearer abc", true).is_none());
        assert!(parse_authorization_header(r#"Emby Token="t""#, false).is_none());
        assert!(parse_authorization_header(r#"Emby Token="t""#, true).is_some());
        // No space at all → no scheme → rejected outright.
        assert!(parse_authorization_header("MediaBrowser", false).is_none());
        assert!(parse_authorization_header("", false).is_none());
    }

    #[test]
    fn url_decode_handles_plus_and_percent() {
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(url_decode("bad%zz"), "bad%zz");
        // Nothing to decode → borrowed, not a fresh allocation.
        assert!(matches!(
            url_decode("plain value"),
            Cow::Borrowed("plain value")
        ));
        assert!(matches!(url_decode("a+b"), Cow::Owned(_)));
        // A truncated escape at the very end is left verbatim (C# parity).
        assert_eq!(url_decode("tail%4"), "tail%4");
        assert_eq!(url_decode("%41%42"), "AB");
    }

    /// The parser's edge cases, asserted directly on the struct. These are the
    /// header shapes real clients (and hand-rolled scripts) actually send.
    #[test]
    fn parses_edge_case_header_shapes() {
        // Unquoted values, no spaces after the commas.
        let p = parse_authorization_header("MediaBrowser Client=Web,Device=TV,Token=t1", false)
            .expect("parsed");
        assert_eq!(p.client.as_deref(), Some("Web"));
        assert_eq!(p.device.as_deref(), Some("TV"));
        assert_eq!(p.token.as_deref(), Some("t1"));

        // Extra whitespace around a KEY is trimmed. Whitespace around a VALUE
        // is NOT — `trim_matches('"')` only strips quotes, so a padded value
        // keeps its padding and its quotes. That is the upstream C# behaviour
        // and clients never send it; pinned here so the rewrite can't quietly
        // "fix" it into a parity divergence.
        let p = parse_authorization_header(
            r#"MediaBrowser   Client = "Jellyfin Web" ,  Token = "t2""#,
            false,
        )
        .expect("parsed");
        assert_eq!(p.client.as_deref(), Some(r#" "Jellyfin Web" "#));
        assert_eq!(p.token.as_deref(), Some(r#" "t2"#));

        // A comma inside a quoted value does not split the value.
        let p = parse_authorization_header(
            r#"MediaBrowser Device="Smith, John's TV", Token="t3""#,
            false,
        )
        .expect("parsed");
        assert_eq!(p.device.as_deref(), Some("Smith, John's TV"));
        assert_eq!(p.token.as_deref(), Some("t3"));

        // Empty value → an empty string, not a missing field; and an empty
        // trailing segment is dropped rather than recorded.
        let p = parse_authorization_header(r#"MediaBrowser Client="", Token="t4","#, false)
            .expect("parsed");
        assert_eq!(p.client.as_deref(), Some(""));
        assert_eq!(p.token.as_deref(), Some("t4"));

        // Unknown keys are ignored, known ones still land.
        let p = parse_authorization_header(
            r#"MediaBrowser Bogus="x", Client="Web", DeviceProfile="y""#,
            false,
        )
        .expect("parsed");
        assert_eq!(p.client.as_deref(), Some("Web"));
        assert_eq!(
            p,
            AuthorizationParts {
                client: Some("Web".into()),
                ..Default::default()
            }
        );

        // Keys are case-SENSITIVE (ordinal, as upstream) — `token` is not `Token`.
        let p = parse_authorization_header(r#"MediaBrowser token="t5""#, false).expect("parsed");
        assert_eq!(p.token, None);

        // Percent/plus encoding is decoded in values.
        let p = parse_authorization_header(r#"MediaBrowser Device="Ken%27s+TV""#, false)
            .expect("parsed");
        assert_eq!(p.device.as_deref(), Some("Ken's TV"));

        // A value with no `=` at all contributes nothing (empty key).
        let p = parse_authorization_header("MediaBrowser garbage", false).expect("parsed");
        assert_eq!(p, AuthorizationParts::default());

        // Multibyte UTF-8 survives the byte scan intact.
        let p =
            parse_authorization_header(r#"MediaBrowser Device="Café — 日本", Token="t6""#, false)
                .expect("parsed");
        assert_eq!(p.device.as_deref(), Some("Café — 日本"));
        assert_eq!(p.token.as_deref(), Some("t6"));

        // A later duplicate wins, matching the map's insert semantics.
        let p = parse_authorization_header(r#"MediaBrowser Token="a", Token="b""#, false)
            .expect("parsed");
        assert_eq!(p.token.as_deref(), Some("b"));

        // A key is consumed by the value it introduced: a trailing key-less
        // segment must NOT be re-filed under the previous key (that would let a
        // malformed tail silently overwrite the token).
        let p =
            parse_authorization_header(r#"MediaBrowser Token="a", bare"#, false).expect("parsed");
        assert_eq!(p.token.as_deref(), Some("a"));
        let p = parse_authorization_header(r#"MediaBrowser Client="Web", Token"#, false)
            .expect("parsed");
        assert_eq!(p.client.as_deref(), Some("Web"));
        assert_eq!(p.token, None);
    }

    /// Verbatim copy of the pre-optimisation `Vec<char>` parser, kept as the
    /// parity oracle for the byte-scanning replacement. If the two ever
    /// disagree on any header shape below, the rewrite changed the trust
    /// boundary's behaviour and the differential test fails.
    fn oracle_parse_parts(header: &str) -> std::collections::HashMap<String, String> {
        let mut result = std::collections::HashMap::new();
        let chars: Vec<char> = header.chars().collect();
        let mut escaped = false;
        let mut start = 0usize;
        let mut key = String::new();

        let mut i = 0usize;
        while i < chars.len() {
            let token = chars[i];
            if token == '"' || token == ',' {
                let is_quote = token == '"';
                escaped = if is_quote { !escaped } else { escaped };
                if token == ',' && !escaped && start < i {
                    let raw: String = chars[start..i].iter().collect();
                    result.insert(
                        std::mem::take(&mut key),
                        url_decode(raw.trim_matches('"')).into_owned(),
                    );
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
            result.insert(key, url_decode(raw.trim_matches('"')).into_owned());
        }
        result
    }

    #[test]
    fn byte_scan_parser_matches_the_char_scan_oracle() {
        let corpus = [
            r#"Client="Web", Device="Firefox", DeviceId="abc", Version="1.2", Token="tok""#,
            "Client=Web,Device=TV,Token=t1",
            r#"   Client = "Jellyfin Web" ,  Token = "t2" "#,
            r#"Device="Smith, John's TV", Token="t3""#,
            r#"Client="", Token="t4","#,
            r#"Bogus="x", Client="Web", DeviceProfile="y""#,
            r#"token="t5""#,
            r#"Device="Ken%27s+TV""#,
            "garbage",
            "",
            ",,,,",
            "=",
            "==",
            r#"Token="unterminated"#,
            r#"Token=""""#,
            r#"Device="Café — 日本", Token="t6""#,
            r#"Device="日本,語", Client="Ünïcödé""#,
            r#"Token="a", Token="b""#,
            r#"Token="a", bare"#,
            r#"Client="Web", Token"#,
            r#"Client="Web",,Token="t""#,
            r#"Client="Web"Device="TV""#,
            r#"Token=a"b"c"#,
            "Client=,Device=,Token=",
            r#"Token   =   "  spaced  ""#,
            "Version=1.2.3, Client=Web",
            r#"DeviceId="%E6%97%A5", Token="t""#,
            r#"Client="a,b,c", Device="d,e", Token="f""#,
        ];

        for header in corpus {
            let expected = oracle_parse_parts(header);
            let actual = parse_parts(header);
            for (key, slot) in [
                ("DeviceId", &actual.device_id),
                ("Device", &actual.device),
                ("Client", &actual.client),
                ("Version", &actual.version),
                ("Token", &actual.token),
            ] {
                assert_eq!(
                    slot.as_deref(),
                    expected.get(key).map(String::as_str),
                    "key {key} diverged for header {header:?}"
                );
            }
        }
    }

    /// Builds a context over a database, wired to fake host/config dependencies.
    fn context(db: Database) -> FerrofinAuthorizationContext {
        context_with_config(
            db,
            Arc::new(FakeConfig::new(default_server_configuration())),
        )
        .0
    }

    /// Builds a context over a caller-supplied config manager, handing the
    /// manager back so a test can inspect how often it was read.
    fn context_with_config(
        db: Database,
        config: Arc<FakeConfig>,
    ) -> (FerrofinAuthorizationContext, Arc<FakeConfig>) {
        let host = Arc::new(FakeHost {
            name: "test-machine".to_owned(),
        });
        let users = Arc::new(FerrofinUserManager::new(db.clone()));
        let ctx = FerrofinAuthorizationContext::new(
            db,
            users,
            host,
            Arc::clone(&config) as Arc<dyn ServerConfigurationManager>,
            "sys-1",
            "10.9.0",
        );
        (ctx, config)
    }

    /// Finding #8: the legacy-authorization flag is read ONCE per request.
    /// Re-reading it deep-clones the entire `ServerConfiguration` (policy,
    /// paths, every `Vec` and `Option` in it) on the authenticated hot path,
    /// which the header/token fallbacks used to do one or two extra times.
    #[tokio::test]
    async fn resolving_auth_reads_the_configuration_exactly_once() {
        let db = crate::test_support::test_db().await;
        let uid = Uuid::from_u128(0x2c);
        crate::test_support::seed_user(&db, uid).await;
        seed_device(&db, uid).await;

        // The worst case for the old code: no `Authorization` header at all, so
        // the header lookup, the parse gate and the token fallbacks each wanted
        // the flag.
        let bare = RequestContext {
            query_string: Some("api_key=dev-tok".to_owned()),
            ..Default::default()
        };
        let (ctx, config) = context_with_config(
            db.clone(),
            Arc::new(FakeConfig::new(default_server_configuration())),
        );
        assert!(
            ctx.get_authorization_info(&bare)
                .await
                .unwrap()
                .is_authenticated
        );
        assert_eq!(
            config.reads(),
            1,
            "no-header request: one configuration read"
        );

        // And the common case: a full MediaBrowser header.
        let (ctx, config) = context_with_config(
            db.clone(),
            Arc::new(FakeConfig::new(default_server_configuration())),
        );
        assert!(
            ctx.get_authorization_info(&token_request())
                .await
                .unwrap()
                .is_authenticated
        );
        assert_eq!(config.reads(), 1, "header request: one configuration read");
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
        seed_api_key(&db, "key-tok", "Automation").await;

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
        seed_device(&db, uid).await;

        let ctx = context(db);
        let info = ctx
            .get_authorization_info(&token_request())
            .await
            .expect("info");
        assert!(info.is_authenticated);
        assert!(!info.is_api_key);
        assert_eq!(info.client.as_deref(), Some("Web"));
        assert_eq!(info.device.as_deref(), Some("Firefox"));
        assert_eq!(info.user_id(), uid);
    }

    /// Seeds an api-key row with the given token and key name (the name becomes
    /// the api-key branch's client fallback, so tests assert on it).
    async fn seed_api_key(db: &Database, token: &str, name: &str) {
        sqlx::query(
            r#"INSERT INTO "ApiKeys" ("AccessToken", "DateCreated", "DateLastActivity", "Name")
               VALUES (?1, ?2, ?2, ?3)"#,
        )
        .bind(token)
        .bind(datetime_to_db(Utc::now()))
        .bind(name)
        .execute(db.writer())
        .await
        .expect("insert api key");
    }

    /// Seeds the standard device row (`dev-tok` → user `uid`).
    async fn seed_device(db: &Database, uid: Uuid) {
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
        .bind(datetime_to_db(now))
        .bind("dev-1")
        .bind("Firefox")
        .bind(ferrofin_db::store::guid_to_db(uid))
        .execute(db.writer())
        .await
        .expect("insert device");
    }

    fn token_request() -> RequestContext {
        RequestContext {
            headers: vec![(
                "Authorization".to_owned(),
                r#"MediaBrowser Token="dev-tok""#.to_owned(),
            )],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn bare_api_key_query_param_authenticates_a_user_token() {
        // jellyfin-web's browser-initiated downloads carry the USER token as
        // `?api_key=<token>` with no Authorization header — the only auth the
        // request has. It must resolve like any header token.
        let db = crate::test_support::test_db().await;
        let uid = Uuid::from_u128(0x21);
        crate::test_support::seed_user(&db, uid).await;
        seed_device(&db, uid).await;

        let ctx = context(db.clone());
        let request = RequestContext {
            query_string: Some("api_key=dev-tok".to_owned()),
            ..Default::default()
        };
        let info = ctx.get_authorization_info(&request).await.unwrap();
        assert!(info.is_authenticated, "query api_key user token accepted");
        assert_eq!(info.user_id(), uid);
    }

    #[tokio::test]
    async fn cached_token_is_served_without_the_database_until_cleared() {
        let db = crate::test_support::test_db().await;
        let uid = Uuid::from_u128(8);
        crate::test_support::seed_user(&db, uid).await;
        seed_device(&db, uid).await;

        let cache = Arc::new(crate::auth_cache::AuthCache::default());
        let ctx = context(db.clone()).with_auth_cache(Arc::clone(&cache));

        // Miss → resolves from the DB and caches.
        let info = ctx.get_authorization_info(&token_request()).await.unwrap();
        assert!(info.is_authenticated);
        assert_eq!(cache.len(), 1);

        // Delete the row OUT-OF-BAND (no manager, no invalidation): within the
        // TTL the cache still answers — proof the DB is not being consulted.
        sqlx::query(r#"DELETE FROM "Devices""#)
            .execute(db.writer())
            .await
            .unwrap();
        let info = ctx.get_authorization_info(&token_request()).await.unwrap();
        assert!(info.is_authenticated, "hit served from cache");
        assert_eq!(info.user_id(), uid, "cached user rides along");

        // After a clear the DB is authoritative again → token is gone.
        cache.clear();
        let info = ctx.get_authorization_info(&token_request()).await.unwrap();
        assert!(!info.is_authenticated, "cleared cache falls through to DB");
    }

    #[tokio::test]
    async fn device_delete_revokes_cached_auth_immediately() {
        let db = crate::test_support::test_db().await;
        let uid = Uuid::from_u128(9);
        crate::test_support::seed_user(&db, uid).await;
        seed_device(&db, uid).await;

        let cache = Arc::new(crate::auth_cache::AuthCache::default());
        let ctx = context(db.clone()).with_auth_cache(Arc::clone(&cache));
        let devices = crate::device_manager::FerrofinDeviceManager::new(db.clone())
            .with_auth_cache(Arc::clone(&cache));

        assert!(
            ctx.get_authorization_info(&token_request())
                .await
                .unwrap()
                .is_authenticated
        );

        // Revoke through the manager — the shared cache must be dropped
        // synchronously, never waiting out the TTL.
        // The seeded row is Id=1; a literal entity avoids a fixture query.
        let device = ferrofin_db::entities::security::DeviceEntity {
            id: 1,
            access_token: "dev-tok".into(),
            app_name: "Web".into(),
            app_version: "1.0".into(),
            date_created: Utc::now(),
            date_last_activity: Utc::now(),
            date_modified: Utc::now(),
            device_id: "dev-1".into(),
            device_name: "Firefox".into(),
            is_active: true,
            user_id: uid.to_string(),
        };
        ferrofin_traits::devices::DeviceManager::delete_device(&devices, &device)
            .await
            .unwrap();
        let info = ctx.get_authorization_info(&token_request()).await.unwrap();
        assert!(!info.is_authenticated, "revoked token rejected immediately");
    }

    #[tokio::test]
    async fn password_change_clears_cached_auth() {
        let db = crate::test_support::test_db().await;
        let uid = Uuid::from_u128(10);
        crate::test_support::seed_user(&db, uid).await;
        seed_device(&db, uid).await;

        let cache = Arc::new(crate::auth_cache::AuthCache::default());
        let ctx = context(db.clone()).with_auth_cache(Arc::clone(&cache));
        let users = FerrofinUserManager::new(db.clone()).with_auth_cache(Arc::clone(&cache));

        ctx.get_authorization_info(&token_request()).await.unwrap();
        assert_eq!(cache.len(), 1);

        ferrofin_traits::library::UserManager::change_password(&users, uid, "new-password-1")
            .await
            .unwrap();
        assert!(
            cache.is_empty(),
            "password change dropped every cached resolution"
        );
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
        seed_api_key(&db, "qtok", "QueryKey").await;

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
