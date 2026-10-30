//! OpenSubtitles subtitle provider — a [`SubtitleProvider`] over the
//! opensubtitles.com REST API v1.
//!
//! Faithful to Jellyfin's OpenSubtitles **plugin**: the provider is a
//! compiled-in Ferrofin plugin ([`PLUGIN_ID`] / [`PLUGIN_NAME`]) whose credentials
//! ([`OpenSubtitlesConfig`] — account + optional API key override) are set through
//! the dashboard plugin-configuration page (`POST /Plugins/{id}/Configuration`)
//! and read back via the [`PluginManager`]. Jellyfin's shared application key is
//! used by default. Downloads still require an OpenSubtitles account; with no
//! account or explicit key configured, the provider remains inactive.
//!
//! Downloads share one session, as the plugin's downloader does: the login's
//! token is reused until it expires or the configured account changes, and the
//! account's daily quota is tracked, so a spent quota stops downloads until its
//! reset time instead of logging in again for every file.
//!
//! API shape (<https://opensubtitles.stoplight.io>): every call carries the
//! `Api-Key` header and a descriptive `User-Agent`; `POST /login` exchanges the
//! account for a bearer token, `GET /subtitles` searches, and `POST /download`
//! turns a `file_id` into a one-time download link whose body is the subtitle.

use crate::rate_limit::CountedBody as _;
use crate::rate_limit::{LimitedRequest as _, RateLimiter, RequestError};
use std::sync::Arc;

use crate::error::ProvidersError;
use async_trait::async_trait;
use ferrofin_model::providers::RemoteSubtitleInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::PluginManager;
use ferrofin_traits::subtitles::{
    ProviderLogin, SubtitleMediaType, SubtitleProvider, SubtitleResponse, SubtitleSearchRequest,
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The stable id of the compiled-in OpenSubtitles plugin (dashboard config key).
pub const PLUGIN_ID: Uuid = Uuid::from_u128(0x4a3f_8e21_6c94_4d17_a2b8_0f5e_9c3d_7a10);

/// The provider's name — also the id namespace prefix in [`RemoteSubtitleInfo`].
pub const PLUGIN_NAME: &str = "opensubtitles";

/// The API base URL.
const API_BASE: &str = "https://api.opensubtitles.com/api/v1";

/// Jellyfin's shared application key (`OpenSubtitlesPlugin.ApiKey`).
const DEFAULT_API_KEY: &str = "gUCLWGoAg2PmyseoTM0INFFVPcDCeDlT";

/// The `User-Agent` OpenSubtitles requires (they reject generic agents).
const USER_AGENT: &str = concat!("Ferrofin/", env!("CARGO_PKG_VERSION"));

/// The dashboard-managed OpenSubtitles credentials (the plugin configuration).
///
/// Serialized as the plugin's opaque config bytes; PascalCase mirrors the C#
/// plugin's `PluginConfiguration` field names so a dashboard config page maps 1:1.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct OpenSubtitlesConfig {
    /// Optional API key override. Empty or whitespace uses Jellyfin's shared key.
    pub api_key: SecretString,
    /// The account username.
    pub username: String,
    /// The account password.
    pub password: SecretString,
}

impl OpenSubtitlesConfig {
    /// Whether an account or explicit API key is configured. An account uses
    /// the shared application key; an explicit key alone still permits searches.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.api_key.expose_secret().trim().is_empty()
            || (!self.username.trim().is_empty()
                && !self.password.expose_secret().trim().is_empty())
    }

    /// Whether an account is configured: both a username and a password
    /// (`Login`'s `IsNullOrWhiteSpace` checks). Downloads need one.
    #[must_use]
    pub fn has_account(&self) -> bool {
        !self.username.trim().is_empty() && !self.password.expose_secret().trim().is_empty()
    }

    /// The operator's override, or Jellyfin's shared application key.
    fn effective_api_key(&self) -> &str {
        let key = self.api_key.expose_secret().trim();
        if key.is_empty() { DEFAULT_API_KEY } else { key }
    }
}

// ── wire DTOs ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct LoginRequest<'a> {
    username: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct LoginResponse {
    token: String,
    /// The account the token belongs to (upstream's `LoginInfo.User`).
    #[serde(default)]
    user: Option<LoginUser>,
}

/// The account block of a login reply (upstream's `UserInfo`).
#[derive(Deserialize)]
struct LoginUser {
    /// The account's allowed downloads per day.
    #[serde(default)]
    allowed_downloads: Option<i32>,
}

/// An error reply's `{"message": …}` (upstream's `ErrorResponse`).
#[derive(Deserialize)]
struct ErrorReply {
    message: Option<String>,
}

/// Why a login produced no token.
enum LoginFailure {
    /// OpenSubtitles answered with a failure status and this body.
    Rejected {
        status: reqwest::StatusCode,
        body: String,
    },
    /// The request itself failed.
    Request(ServiceError),
}

impl LoginFailure {
    /// The failure as the download path reports it.
    fn into_service_error(self) -> ServiceError {
        match self {
            Self::Rejected { status, .. } => {
                ServiceError::unauthorized(format!("OpenSubtitles login failed: HTTP {status}"))
            }
            Self::Request(error) => error,
        }
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    #[serde(default)]
    attributes: SearchAttributes,
}

#[derive(Deserialize, Default)]
struct SearchAttributes {
    #[serde(default)]
    hearing_impaired: Option<bool>,
    #[serde(default)]
    moviehash_match: Option<bool>,
    #[serde(default)]
    foreign_parts_only: Option<bool>,
    #[serde(default)]
    release: Option<String>,
    #[serde(default)]
    download_count: Option<i64>,
    #[serde(default)]
    uploader: Option<Uploader>,
    #[serde(default)]
    files: Vec<SubFile>,
}

#[derive(Deserialize, Default)]
struct Uploader {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize, Default)]
struct SubFile {
    file_id: i64,
    #[serde(default)]
    file_name: Option<String>,
}

#[derive(Serialize)]
struct DownloadRequest {
    file_id: i64,
    sub_format: String,
}

/// `GET /infos/user`: the account's download quota (`EncapsulatedUserInfo`).
/// Read leniently, like every quota field: the quota only informs downloads,
/// so a reply missing part of it must not fail one.
#[derive(Deserialize)]
struct UserInfoResponse {
    #[serde(default)]
    data: Option<UserQuota>,
}

/// The plugin's `UserInfo`: how many downloads are left, and when the quota
/// resets.
#[derive(Deserialize)]
struct UserQuota {
    #[serde(default)]
    remaining_downloads: Option<i32>,
    #[serde(default, rename = "reset_time_utc", deserialize_with = "lenient_time")]
    reset_time: Option<chrono::DateTime<chrono::Utc>>,
}

/// `POST /download` (`SubtitleDownloadInfo`), read leniently: an error reply
/// may carry only part of it.
#[derive(Deserialize, Default)]
struct DownloadResponse {
    #[serde(default)]
    link: Option<String>,
    #[serde(default)]
    remaining: Option<i32>,
    #[serde(default, rename = "reset_time_utc", deserialize_with = "lenient_time")]
    reset_time: Option<chrono::DateTime<chrono::Utc>>,
}

/// An RFC 3339 time, or `None` for anything else (absent, null, malformed).
fn lenient_time<'de, D>(deserializer: D) -> Result<Option<chrono::DateTime<chrono::Utc>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&chrono::Utc)))
}

/// A logged-in OpenSubtitles session: the plugin downloader's `_login`.
struct Session {
    /// The account and key the token was issued for. A configuration naming
    /// another one needs a new login, as `ConfigurationChanged` forces.
    username: String,
    password: SecretString,
    api_key: SecretString,
    token: SecretString,
    /// The token's `exp` (`LoginInfo.ExpirationDate`).
    expires_at: chrono::DateTime<chrono::Utc>,
    /// The account's quota (`_login.User`), once read.
    quota: Option<UserQuota>,
}

impl Session {
    /// Whether this session belongs to `cfg`'s account and key.
    fn belongs_to(&self, cfg: &OpenSubtitlesConfig) -> bool {
        self.username == cfg.username
            && self.password.expose_secret() == cfg.password.expose_secret()
            && self.api_key.expose_secret() == cfg.effective_api_key()
    }

    /// Whether the token is still good (`DateTime.UtcNow < ExpirationDate`).
    fn is_live(&self) -> bool {
        chrono::Utc::now() < self.expires_at
    }

    /// Whether this session holds `token`.
    fn holds(&self, token: &SecretString) -> bool {
        self.token.expose_secret() == token.expose_secret()
    }

    /// Whether the quota is known to be spent (`RemainingDownloads <= 0`).
    fn quota_spent(&self) -> bool {
        self.quota
            .as_ref()
            .and_then(|quota| quota.remaining_downloads)
            .is_some_and(|remaining| remaining <= 0)
    }
}

/// What downloads share: the session and the quota's reset time (the
/// plugin's `_login` and `_limitReset`).
#[derive(Default)]
struct DownloadState {
    session: Option<Session>,
    limit_reset: Option<chrono::DateTime<chrono::Utc>>,
}

impl DownloadState {
    /// The quota of `cfg`'s account, if its session has read one. The quota
    /// is the account's, so a download's reply updates it whichever of the
    /// account's logins the download used.
    fn account_quota(&mut self, cfg: &OpenSubtitlesConfig) -> Option<&mut UserQuota> {
        self.session
            .as_mut()
            .filter(|session| session.belongs_to(cfg))
            .and_then(|session| session.quota.as_mut())
    }

    /// Drops the session when it still holds `rejected`, the token the API
    /// refused. A concurrent download may already have logged in again, and
    /// its fresh session is kept.
    fn forget_rejected(&mut self, rejected: &SecretString) {
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.holds(rejected))
        {
            self.session = None;
        }
    }
}

/// The expiry a JWT carries in its payload's `exp` (`LoginInfo.ExpirationDate`).
/// A token with no readable `exp` counts as already expired, as upstream's
/// `?.Exp ?? 0`.
fn token_expiry(token: &str) -> chrono::DateTime<chrono::Utc> {
    #[derive(Deserialize)]
    struct Payload {
        #[serde(default)]
        exp: i64,
    }
    token
        .split('.')
        .nth(1)
        .and_then(decode_base64url)
        .and_then(|payload| serde_json::from_slice::<Payload>(&payload).ok())
        .and_then(|payload| chrono::DateTime::from_timestamp(payload.exp, 0))
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
}

/// Decodes unpadded or padded base64url (a JWT segment).
fn decode_base64url(text: &str) -> Option<Vec<u8>> {
    let value = |byte: u8| -> Option<u32> {
        Some(u32::from(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        }))
    };
    let digits = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(digits.len() * 3 / 4);
    for chunk in digits.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut buffer = 0u32;
        for (index, &digit) in chunk.iter().enumerate() {
            buffer |= value(digit)? << (18 - 6 * index);
        }
        let bytes = buffer.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Some(out)
}

/// The provider-local id of one downloadable file, in the exact shape of the
/// Jellyfin plugin's `BuildSubtitleId`: `srt-{language}-{file_id}[-sdh][-forced]`.
///
/// The language and the flags ride along in the id because the download call
/// only receives the id back — that is how the downloaded stream knows its
/// language (and so its sidecar name) without a second search.
fn build_subtitle_id(language: &str, attrs: &SearchAttributes, file_id: i64) -> String {
    let mut id = format!("srt-{language}-{file_id}");
    if attrs.hearing_impaired.unwrap_or(false) {
        id.push_str("-sdh");
    }
    if attrs.foreign_parts_only.unwrap_or(false) {
        id.push_str("-forced");
    }
    id
}

/// A parsed provider-local id (the inverse of [`build_subtitle_id`]).
struct ParsedId {
    format: String,
    language: String,
    file_id: i64,
    is_hearing_impaired: bool,
    is_forced: bool,
}

/// Parses `srt-{language}-{file_id}[-sdh][-forced]` like the plugin's
/// `GetSubtitlesInternal`: fewer than three `-` parts or a non-numeric file id
/// is an invalid id.
fn parse_subtitle_id(id: &str) -> Option<ParsedId> {
    let parts: Vec<&str> = id.split('-').collect();
    if parts.len() < 3 {
        return None;
    }
    let lower = id.to_ascii_lowercase();
    Some(ParsedId {
        format: parts[0].to_owned(),
        language: parts[1].to_owned(),
        file_id: parts[2].parse().ok()?,
        is_hearing_impaired: lower.contains("-sdh"),
        is_forced: lower.contains("-forced"),
    })
}

/// Maps a search response into namespaced [`RemoteSubtitleInfo`] candidates.
///
/// Each candidate's `id` is the provider-local id ([`build_subtitle_id`]); the
/// subtitle manager namespaces it with the provider's MD5 id, as upstream
/// `SubtitleManager.Normalize` does. As in the plugin, the
/// language stamped on the candidate and into its id is the CALLER's 3-letter
/// code (`request.Language`), never the API's own tag: the search was already
/// filtered by it, and the API's tags (`pt-BR`, `zh-CN`) carry a `-` that would
/// break the id. Pulled out as a pure function so it is unit-testable without a
/// live API.
fn map_search(response: &SearchResponse, language: &str) -> Vec<RemoteSubtitleInfo> {
    let mut out = Vec::new();
    for item in &response.data {
        let attrs = &item.attributes;
        // A result can carry several files; each downloadable file is a candidate.
        for file in &attrs.files {
            out.push(RemoteSubtitleInfo {
                id: Some(build_subtitle_id(language, attrs, file.file_id)),
                provider_name: Some(PLUGIN_NAME.to_owned()),
                three_letter_iso_language_name: Some(language.to_owned()),
                name: file.file_name.clone().or_else(|| attrs.release.clone()),
                format: Some("srt".to_owned()),
                author: attrs.uploader.as_ref().and_then(|u| u.name.clone()),
                comment: attrs.release.clone(),
                download_count: attrs.download_count.and_then(|c| i32::try_from(c).ok()),
                is_hash_match: attrs.moviehash_match,
                hearing_impaired: attrs.hearing_impaired,
                forced: attrs.foreign_parts_only,
                ..Default::default()
            });
        }
    }
    out
}

/// A 3-letter ISO-639-2/T code to the 2-letter ISO-639-1 code the search `languages`
/// parameter expects; unknown codes pass through.
fn three_to_two_letter(code: &str) -> &str {
    match code.to_ascii_lowercase().as_str() {
        "eng" => "en",
        "spa" => "es",
        "fra" | "fre" => "fr",
        "deu" | "ger" => "de",
        "ita" => "it",
        // The API lists no bare pt/zh; the plugin's GetLanguage rewrites them.
        "por" => "pt-PT",
        "rus" => "ru",
        "jpn" => "ja",
        "zho" | "chi" => "zh-CN",
        "kor" => "ko",
        "nld" | "dut" => "nl",
        "pol" => "pl",
        "ara" => "ar",
        "swe" => "sv",
        "tur" => "tr",
        // Unknown: best-effort first two chars of the original input.
        _ => code.get(0..2).unwrap_or(code),
    }
}

/// The OpenSubtitles-backed subtitle provider.
pub struct OpenSubtitlesProvider {
    http: reqwest::Client,
    limiter: RateLimiter,
    plugins: Arc<dyn PluginManager>,
    base_url: String,
    /// Held across a login, so concurrent downloads share one.
    state: tokio::sync::Mutex<DownloadState>,
}

impl std::fmt::Debug for OpenSubtitlesProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenSubtitlesProvider")
            .finish_non_exhaustive()
    }
}

impl OpenSubtitlesProvider {
    /// Builds the provider, reading its credentials from the plugin config store.
    #[must_use]
    pub fn new(plugins: Arc<dyn PluginManager>) -> Self {
        Self {
            http: reqwest::Client::new(),
            limiter: RateLimiter::new("opensubtitles"),
            plugins,
            base_url: API_BASE.to_owned(),
            state: tokio::sync::Mutex::new(DownloadState::default()),
        }
    }

    /// Overrides the REST endpoint for local integration tests.
    #[must_use]
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        base_url
            .trim_end_matches('/')
            .clone_into(&mut self.base_url);
        self
    }

    /// Loads the current dashboard-configured credentials.
    async fn config(&self) -> Result<OpenSubtitlesConfig, ServiceError> {
        let bytes = self.plugins.get_plugin_configuration(PLUGIN_ID).await?;
        Ok(serde_json::from_slice(&bytes).unwrap_or_default())
    }

    /// Exchanges the account credentials for a bearer token.
    async fn login(&self, cfg: &OpenSubtitlesConfig) -> Result<String, ServiceError> {
        self.login_reply(cfg)
            .await
            .map(|reply| reply.token)
            .map_err(LoginFailure::into_service_error)
    }

    /// `OpenSubtitlesApi.LogInAsync`: the login reply, or why there is none.
    async fn login_reply(&self, cfg: &OpenSubtitlesConfig) -> Result<LoginResponse, LoginFailure> {
        let resp = self
            .http
            .post(format!("{}/login", self.base_url))
            .header("Api-Key", cfg.effective_api_key())
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&LoginRequest {
                username: &cfg.username,
                password: cfg.password.expose_secret(),
            })
            .send_limited(&self.limiter)
            .await
            .map_err(|e| LoginFailure::Request(request_err(e)))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.counted_text().await.unwrap_or_default();
            return Err(LoginFailure::Rejected { status, body });
        }
        resp.counted_json()
            .await
            .map_err(|e| LoginFailure::Request(net_err(e)))
    }

    /// `OpenSubtitlesApi.LogOutAsync`: ends the session `token` opened. Best
    /// effort: the login it follows already answered the question.
    async fn logout(&self, cfg: &OpenSubtitlesConfig, token: &str) {
        let result = self
            .http
            .delete(format!("{}/logout", self.base_url))
            .header("Api-Key", cfg.effective_api_key())
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .bearer_auth(token)
            .send_limited(&self.limiter)
            .await;
        match result {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => tracing::debug!(status = %resp.status(), "OpenSubtitles logout refused"),
            Err(error) => {
                tracing::debug!(error = %request_err(error), "OpenSubtitles logout failed");
            }
        }
    }

    /// The session's token for `cfg`, logging in when there is none for that
    /// account or it has expired (`OpenSubtitleDownloader.Login`), then
    /// reading the account's quota (`UpdateUserInfo`).
    async fn session_token(
        &self,
        state: &mut DownloadState,
        cfg: &OpenSubtitlesConfig,
    ) -> Result<SecretString, ServiceError> {
        if let Some(session) = state
            .session
            .as_ref()
            .filter(|session| session.belongs_to(cfg) && session.is_live())
        {
            return Ok(session.token.clone());
        }
        state.session = None;
        let token = self.login(cfg).await?;
        let expires_at = token_expiry(&token);
        if expires_at <= chrono::Utc::now() {
            tracing::debug!(
                "the OpenSubtitles token carries no future expiry; it serves one download"
            );
        }
        let token = SecretString::from(token);
        state.session = Some(Session {
            username: cfg.username.clone(),
            password: cfg.password.clone(),
            api_key: SecretString::from(cfg.effective_api_key().to_owned()),
            token: token.clone(),
            expires_at,
            quota: None,
        });
        self.update_user_info(state, cfg).await?;
        tracing::debug!(
            limit_reset = ?state.limit_reset,
            %expires_at,
            "logged in to OpenSubtitles"
        );
        Ok(token)
    }

    /// `UpdateUserInfo`: reads the session's quota and its reset time, and
    /// says whether it could. A refused or unreadable reply leaves both as
    /// they were, as upstream.
    async fn update_user_info(
        &self,
        state: &mut DownloadState,
        cfg: &OpenSubtitlesConfig,
    ) -> Result<bool, ServiceError> {
        let Some(session) = state.session.as_mut() else {
            return Ok(false);
        };
        let response = self
            .http
            .get(format!("{}/infos/user", self.base_url))
            .header("Api-Key", cfg.effective_api_key())
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .bearer_auth(session.token.expose_secret())
            .send_limited(&self.limiter)
            .await
            .map_err(request_err)?;
        let status = response.status();
        if !status.is_success() {
            tracing::debug!(%status, "OpenSubtitles refused the quota read");
            return Ok(false);
        }
        let body = response.counted_bytes().await.map_err(net_err)?;
        let Ok(info) = serde_json::from_slice::<UserInfoResponse>(&body) else {
            tracing::debug!("OpenSubtitles sent an unreadable quota");
            return Ok(false);
        };
        // A quota without a reset time counts as already reset: upstream's
        // `ResetTime` is then `default(DateTime)`.
        state.limit_reset = info.data.as_ref().map(|quota| {
            quota
                .reset_time
                .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC)
        });
        session.quota = info.data;
        Ok(true)
    }

    /// The start of `GetSubtitlesInternal`: while the session's quota is
    /// spent, no download is tried, until its reset time has passed and a
    /// fresh read of the quota shows downloads left.
    ///
    /// Past the reset time, a session whose token has expired, or whose
    /// quota read is refused, is dropped instead, so the login that follows
    /// reads the quota afresh. Upstream re-reads with the stale token, which a
    /// refusal leaves spent for good; there, a search's `Login` replaces the
    /// session, and Ferrofin's search does not log in.
    async fn ensure_quota(
        &self,
        state: &mut DownloadState,
        cfg: &OpenSubtitlesConfig,
    ) -> Result<(), ServiceError> {
        let spent = |state: &DownloadState| {
            state
                .session
                .as_ref()
                .is_some_and(|session| session.belongs_to(cfg) && session.quota_spent())
        };
        if !spent(state) {
            return Ok(());
        }
        if state
            .limit_reset
            .is_some_and(|reset| reset < chrono::Utc::now())
        {
            tracing::debug!("OpenSubtitles quota reset time passed; reading the quota again");
            let live = state.session.as_ref().is_some_and(Session::is_live);
            if !live || !self.update_user_info(state, cfg).await? {
                state.session = None;
                return Ok(());
            }
            if !spent(state) {
                return Ok(());
            }
        }
        Err(download_limit_reached())
    }

    /// Validates the supplied credentials by attempting a login, then logs the
    /// session it opened out again and reports the account's allowed downloads
    /// (the plugin's `ValidateLoginInfo`).
    ///
    /// # Errors
    /// Returns [`ServiceError::Unauthorized`] with upstream's message if the
    /// credentials are rejected, or [`ServiceError::Backend`] on a transport
    /// error.
    pub async fn validate_config(
        &self,
        cfg: &OpenSubtitlesConfig,
    ) -> Result<ProviderLogin, ServiceError> {
        if !cfg.is_configured() {
            return Err(ServiceError::invalid_input(
                "OpenSubtitles account or API key is not configured",
            ));
        }
        match self.login_reply(cfg).await {
            Ok(reply) => {
                self.logout(cfg, &reply.token).await;
                Ok(ProviderLogin {
                    allowed_downloads: reply
                        .user
                        .and_then(|user| user.allowed_downloads)
                        .unwrap_or(0),
                })
            }
            Err(LoginFailure::Rejected { status, body }) => Err(ServiceError::unauthorized(
                rejected_login_message(status, &body),
            )),
            Err(LoginFailure::Request(error)) => Err(error),
        }
    }
}

/// `ValidateLoginInfo`'s message for a rejected login: the status and, under
/// 150 characters, the body; or the body's own `message`, with the shared
/// key's refusal reworded.
///
/// The status is named by its reason phrase run together, which is how
/// .NET's `HttpStatusCode` prints the codes `/login` answers with (400, 401,
/// 403, 429, 5xx); a few rarer ones differ (`PayloadTooLarge` against
/// `RequestEntityTooLarge`, say).
///
/// Upstream deserializes any body containing `message":` and fails the
/// request (a 500) when it is not JSON after all, or answers
/// `{"Message": null}` when the JSON has no `message`; both keep the status
/// message here.
fn rejected_login_message(status: reqwest::StatusCode, body: &str) -> String {
    if body.contains("message\":")
        && let Ok(ErrorReply {
            message: Some(message),
        }) = serde_json::from_str(body)
    {
        return if message == "You cannot consume this service" {
            "Invalid API key provided".to_owned()
        } else {
            message
        };
    }
    let code = status.canonical_reason().map_or_else(
        || status.as_u16().to_string(),
        |reason| reason.replace([' ', '-', '\''], ""),
    );
    if body.encode_utf16().count() < 150 {
        format!("{code} - {body}")
    } else {
        code
    }
}

/// OpenSubtitles' hash: file size plus the little-endian 64-bit words in
/// the first and last 64 KiB, with wrapping addition (including overlap).
async fn movie_hash(path: &str) -> std::io::Result<Option<String>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    if size < 65_536 {
        return Ok(None);
    }
    let mut hash = size;
    let mut buffer = vec![0u8; 65_536];
    for offset in [0, size - 65_536] {
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read_exact(&mut buffer).await?;
        for word in buffer.as_chunks::<8>().0 {
            hash = hash.wrapping_add(u64::from_le_bytes(*word));
        }
    }
    Ok(Some(format!("{hash:016x}")))
}

/// Maps a transport / response-decode error to a backend [`ServiceError`],
/// preserving the underlying [`reqwest::Error`] as the source chain.
fn request_err(error: RequestError) -> ServiceError {
    match error {
        RequestError::Http(error) => net_err(error),
        other => ServiceError::backend(other.to_string()),
    }
}

fn net_err(e: reqwest::Error) -> ServiceError {
    ProvidersError::http("OpenSubtitles request failed", e).into()
}

/// The plugin's `RateLimitExceededException`.
fn download_limit_reached() -> ServiceError {
    ServiceError::backend("OpenSubtitles download limit reached")
}

/// A refused `/download`, with the API's reply unless it is an HTML page
/// (`GetSubtitlesInternal`'s `[html]`).
fn invalid_download_response(
    file_id: i64,
    status: reqwest::StatusCode,
    body: &[u8],
) -> ServiceError {
    let body = String::from_utf8_lossy(body);
    let body = if body.to_ascii_lowercase().contains("<html") {
        "[html]".into()
    } else {
        body
    };
    ServiceError::backend(format!(
        "Invalid response for file {file_id}: {status}\n\n{body}"
    ))
}

/// `UpdateResetTime`: the API's reset times wander by a second or two, so a
/// time within two seconds of the known one is not a change.
fn update_reset_time(state: &mut DownloadState, reset: chrono::DateTime<chrono::Utc>) {
    if state
        .limit_reset
        .is_some_and(|known| (known - reset).num_milliseconds().abs() <= 2_000)
    {
        return;
    }
    state.limit_reset = Some(reset);
    tracing::debug!(limit_reset = %reset, "OpenSubtitles quota reset time updated");
}

#[async_trait]
impl SubtitleProvider for OpenSubtitlesProvider {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }

    async fn search(
        &self,
        request: &SubtitleSearchRequest,
    ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
        let cfg = self.config().await?;
        if !cfg.is_configured() {
            // Unconfigured provider contributes no candidates (not an error).
            return Ok(Vec::new());
        }

        let hash = match request.media_path.as_deref() {
            Some(path) if !path.to_ascii_lowercase().ends_with(".strm") => {
                movie_hash(path).await.map_err(|e| {
                    ServiceError::backend(format!("subtitle movie hash failed: {e}"))
                })?
            }
            _ => None,
        };
        // A perfect match requires a file hash, never a title-only guess.
        if request.is_perfect_match == Some(true) && hash.is_none() {
            return Ok(Vec::new());
        }
        // Build the query params from the enriched request.
        let mut query: Vec<(String, String)> = Vec::new();
        if let Some(hash) = hash {
            query.push(("moviehash".to_owned(), hash));
            if request.is_perfect_match == Some(true) {
                query.push(("moviehash_match".to_owned(), "only".to_owned()));
            }
        }
        if !request.language.is_empty() {
            query.push((
                "languages".to_owned(),
                three_to_two_letter(&request.language).to_owned(),
            ));
        }
        if let Some(imdb) = request
            .imdb_id
            .as_deref()
            .map(|s| s.trim_start_matches("tt"))
            .filter(|s| !s.is_empty())
        {
            query.push(("imdb_id".to_owned(), imdb.to_owned()));
        }
        match request.content_type {
            SubtitleMediaType::Episode => {
                if let Some(series) = request.series_name.as_deref().filter(|s| !s.is_empty()) {
                    query.push(("query".to_owned(), series.to_owned()));
                }
                if let Some(s) = request.parent_index_number {
                    query.push(("season_number".to_owned(), s.to_string()));
                }
                if let Some(e) = request.index_number {
                    query.push(("episode_number".to_owned(), e.to_string()));
                }
            }
            SubtitleMediaType::Movie => {
                if let Some(name) = request.name.as_deref().filter(|s| !s.is_empty()) {
                    query.push(("query".to_owned(), name.to_owned()));
                }
                if let Some(year) = request.production_year {
                    query.push(("year".to_owned(), year.to_string()));
                }
            }
        }

        let resp = self
            .http
            .get(format!("{}/subtitles", self.base_url))
            .header("Api-Key", cfg.effective_api_key())
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&query)
            .send_limited(&self.limiter)
            .await
            .map_err(request_err)?;
        if !resp.status().is_success() {
            return Err(ServiceError::backend(format!(
                "OpenSubtitles search failed: HTTP {}",
                resp.status()
            )));
        }
        let body: SearchResponse = resp.counted_json().await.map_err(net_err)?;
        let mut results = map_search(&body, &request.language);
        if request.is_perfect_match == Some(true) {
            results.retain(|r| r.is_hash_match == Some(true));
        }
        results.sort_by_key(|r| r.is_hash_match != Some(true));
        Ok(results)
    }

    async fn validate_login(&self, config_json: &[u8]) -> Result<ProviderLogin, ServiceError> {
        let cfg: OpenSubtitlesConfig = serde_json::from_slice(config_json)
            .map_err(|_| ServiceError::invalid_input("invalid OpenSubtitles configuration"))?;
        self.validate_config(&cfg).await
    }

    async fn get_subtitles(
        &self,
        provider_local_id: &str,
    ) -> Result<SubtitleResponse, ServiceError> {
        let cfg = self.config().await?;
        if !cfg.has_account() {
            return Err(ServiceError::unauthorized(
                "OpenSubtitles account username and/or password are not set up",
            ));
        }
        let parsed = parse_subtitle_id(provider_local_id)
            .ok_or_else(|| ServiceError::invalid_input("invalid OpenSubtitles subtitle id"))?;
        let file_id = parsed.file_id;

        // Port of `GetSubtitlesInternal`. A rejected token is dropped and the
        // download tried once more with a fresh login; upstream retries
        // without a bound.
        let mut retried = false;
        let link = loop {
            let token = {
                let mut state = self.state.lock().await;
                self.ensure_quota(&mut state, &cfg).await?;
                self.session_token(&mut state, &cfg).await?
            };
            // Turn the file id into a one-time download link.
            let response = self
                .http
                .post(format!("{}/download", self.base_url))
                .header("Api-Key", cfg.effective_api_key())
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .header(reqwest::header::ACCEPT, "application/json")
                .bearer_auth(token.expose_secret())
                .json(&DownloadRequest {
                    file_id,
                    sub_format: parsed.format.clone(),
                })
                .send_limited(&self.limiter)
                .await
                .map_err(request_err)?;
            let status = response.status();
            let body = response.counted_bytes().await.map_err(net_err)?;
            let info: DownloadResponse = serde_json::from_slice(&body).unwrap_or_default();
            let mut state = self.state.lock().await;
            if let Some(reset) = info.reset_time {
                update_reset_time(&mut state, reset);
            }
            if status == reqwest::StatusCode::NOT_ACCEPTABLE
                && info.remaining.is_some_and(|remaining| remaining <= 0)
            {
                if let Some(quota) = state.account_quota(&cfg) {
                    quota.remaining_downloads = Some(0);
                }
                return Err(download_limit_reached());
            }
            if status == reqwest::StatusCode::UNAUTHORIZED {
                state.forget_rejected(&token);
                if !retried {
                    tracing::debug!(file_id, "OpenSubtitles refused the token; logging in again");
                    retried = true;
                    continue;
                }
            }
            if !status.is_success() {
                return Err(invalid_download_response(file_id, status, &body));
            }
            if let Some(quota) = state.account_quota(&cfg) {
                quota.remaining_downloads = info.remaining;
                // Info, as upstream: one line per subtitle a user downloads,
                // bounded by the account's daily quota.
                tracing::info!(
                    remaining = info.remaining,
                    "remaining OpenSubtitles downloads"
                );
            }
            break info
                .link
                .filter(|link| !link.trim().is_empty())
                .ok_or_else(|| {
                    ServiceError::backend(format!(
                        "Failed to obtain download link for file {file_id}: {status} (empty response)"
                    ))
                })?;
        };

        // Fetch the subtitle bytes from the link.
        let content = self
            .http
            .get(&link)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send_limited(&crate::image_download::limiter_for(&link, "opensubtitles"))
            .await
            .map_err(request_err)?
            .error_for_status()
            .map_err(net_err)?
            .counted_bytes()
            .await
            .map_err(net_err)?;

        Ok(SubtitleResponse {
            language: parsed.language,
            format: parsed.format,
            is_forced: parsed.is_forced,
            is_hearing_impaired: parsed.is_hearing_impaired,
            content,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn limiter_transport_errors_preserve_the_reqwest_source() {
        use std::error::Error as _;
        let error = reqwest::Client::new()
            .get("invalid URL")
            .build()
            .unwrap_err();
        let error = super::request_err(super::RequestError::Http(error));
        let mut source = error.source();
        let mut found = false;
        while let Some(error) = source {
            found |= error.downcast_ref::<reqwest::Error>().is_some();
            source = error.source();
        }
        assert!(found);
    }

    use super::*;

    #[tokio::test]
    async fn movie_hash_uses_file_size_and_little_endian_end_blocks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("movie.mkv");
        let mut bytes = vec![0u8; 131_072];
        bytes[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        bytes[131_064..].copy_from_slice(&2u64.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            movie_hash(path.to_str().unwrap()).await.unwrap().as_deref(),
            Some("0000000000020001")
        );
        // Exactly 64 KiB: the two blocks overlap completely.
        bytes.truncate(65_536);
        bytes[..8].copy_from_slice(&1u64.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            movie_hash(path.to_str().unwrap()).await.unwrap().as_deref(),
            Some("0000000000010002")
        );
        std::fs::write(&path, b"short").unwrap();
        assert_eq!(movie_hash(path.to_str().unwrap()).await.unwrap(), None);
    }

    #[test]
    fn config_parses_pascal_case_and_detects_configured() {
        let cfg: OpenSubtitlesConfig =
            serde_json::from_str(r#"{"ApiKey":"k","Username":"u","Password":"p"}"#).unwrap();
        assert_eq!(cfg.api_key.expose_secret(), "k");
        assert!(cfg.is_configured());
        assert!(!OpenSubtitlesConfig::default().is_configured());
    }

    #[test]
    fn account_uses_shared_key_unless_overridden() {
        for key in ["", "  ", " custom-key "] {
            let cfg: OpenSubtitlesConfig = serde_json::from_value(serde_json::json!({
                "Username": "user", "Password": "password", "ApiKey": key
            }))
            .unwrap();
            assert!(cfg.is_configured());
            assert_eq!(
                cfg.effective_api_key(),
                if key.trim().is_empty() {
                    DEFAULT_API_KEY
                } else {
                    "custom-key"
                }
            );
        }
        let cfg: OpenSubtitlesConfig =
            serde_json::from_str(r#"{"Username":"user","Password":"password"}"#).unwrap();
        assert!(cfg.is_configured(), "Jellyfin config has no ApiKey field");
        assert_eq!(cfg.effective_api_key(), DEFAULT_API_KEY);
        for config in [
            "{}",
            r#"{"Username":"user"}"#,
            r#"{"Password":"password"}"#,
            r#"{"Username":" ","Password":"password","ApiKey":" "}"#,
        ] {
            let cfg: OpenSubtitlesConfig = serde_json::from_str(config).unwrap();
            assert!(!cfg.is_configured(), "{config}");
        }
    }

    #[tokio::test]
    async fn login_sends_shared_key_or_operator_override() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        for key in ["", " custom-key "] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let response = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 4096];
                let n = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..n]).into_owned();
                let body = r#"{"token":"test-token"}"#;
                socket.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ).as_bytes()).await.unwrap();
                request
            });
            let plugins =
                crate::plugin_config::tests_support::manager_with(PLUGIN_ID, b"{}".to_vec());
            let mut provider = OpenSubtitlesProvider::new(plugins);
            provider.base_url = base_url;
            let cfg = OpenSubtitlesConfig {
                api_key: SecretString::from(key),
                username: "user".to_owned(),
                password: SecretString::from("password"),
            };
            provider
                .validate_config(&cfg)
                .await
                .expect("account can log in");
            let request = response.await.unwrap();
            assert!(request.starts_with("POST /login "));
            let expected = if key.is_empty() {
                DEFAULT_API_KEY
            } else {
                "custom-key"
            };
            assert!(
                request
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case(&format!("api-key: {expected}")))
            );
        }
    }

    /// gl-25: `ValidateLoginInfo` reports the account's allowed downloads and
    /// logs the session it opened out again (`LogOutAsync`); a refused login
    /// is never logged out.
    #[tokio::test]
    async fn validating_an_account_reports_its_downloads_and_logs_out() {
        let mock = ApiMock::start(|_| {
            vec![
                (
                    "/login",
                    vec![(
                        200,
                        format!(
                            r#"{{"token":"{}","user":{{"allowed_downloads":100}}}}"#,
                            token(FUTURE_PAYLOAD)
                        ),
                    )],
                ),
                ("/logout", vec![(200, "{}".to_owned())]),
            ]
        });
        let mock = mock.await;
        let (provider, _) = account_provider(&mock, "user");
        let login = provider
            .validate_login(br#"{"Username":"user","Password":"secret"}"#)
            .await
            .expect("valid account");
        assert_eq!(login.allowed_downloads, 100);
        assert_eq!(mock.count("POST /login"), 1);
        assert_eq!(mock.count("DELETE /logout"), 1);

        let refusing = ApiMock::start(|_| {
            vec![(
                "/login",
                vec![(
                    401,
                    r#"{"message":"Error, invalid username/password"}"#.to_owned(),
                )],
            )]
        })
        .await;
        let (provider, _) = account_provider(&refusing, "user");
        let error = provider
            .validate_login(br#"{"Username":"user","Password":"wrong"}"#)
            .await
            .expect_err("refused");
        assert!(
            matches!(&error, ServiceError::Unauthorized(m) if m == "Error, invalid username/password"),
            "{error:?}"
        );
        assert_eq!(refusing.count("DELETE /logout"), 0);
    }

    /// `ValidateLoginInfo`'s message: `"{response.Code} - {body}"` for a body
    /// under 150 characters, the bare code otherwise, and the body's own
    /// `message` when it has one, the shared key's refusal reworded.
    #[rstest::rstest]
    #[case(
        401,
        r#"{"message":"You cannot consume this service"}"#,
        "Invalid API key provided"
    )]
    #[case(
        401,
        r#"{"message":"Error, invalid username/password","status":401}"#,
        "Error, invalid username/password"
    )]
    #[case(403, "nope", "Forbidden - nope")]
    #[case(429, r#"x"message":y"#, "TooManyRequests - x\"message\":y")]
    #[case(502, &"z".repeat(150), "BadGateway")]
    fn a_rejected_login_is_explained_as_upstream_does(
        #[case] status: u16,
        #[case] body: &str,
        #[case] expected: &str,
    ) {
        let status = reqwest::StatusCode::from_u16(status).unwrap();
        assert_eq!(rejected_login_message(status, body), expected);
    }

    /// A mocked OpenSubtitles API. Each route (a path substring) answers its
    /// queued `(status, body)` replies in order and then repeats the last;
    /// every request line (`METHOD /path`) is recorded.
    struct ApiMock {
        base_url: String,
        requests: Arc<std::sync::Mutex<Vec<String>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for ApiMock {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    impl ApiMock {
        /// Starts the mock; `routes` gets its base URL, for download links.
        async fn start(
            routes: impl FnOnce(&str) -> Vec<(&'static str, Vec<(u16, String)>)>,
        ) -> Self {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let routes = Arc::new(std::sync::Mutex::new(
                routes(&base_url)
                    .into_iter()
                    .map(|(path, replies)| (path, std::collections::VecDeque::from(replies)))
                    .collect::<Vec<_>>(),
            ));
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            let handle = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let routes = Arc::clone(&routes);
                    let seen = Arc::clone(&seen);
                    tokio::spawn(async move {
                        let mut buffer = vec![0u8; 16 * 1024];
                        let n = socket.read(&mut buffer).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..n]).into_owned();
                        let line = request.lines().next().unwrap_or("").to_owned();
                        let mut parts = line.split_whitespace();
                        let (method, path) = (
                            parts.next().unwrap_or("").to_owned(),
                            parts.next().unwrap_or("").to_owned(),
                        );
                        seen.lock().unwrap().push(format!("{method} {path}"));
                        let (status, body) = {
                            let mut routes = routes.lock().unwrap();
                            routes
                                .iter_mut()
                                .find(|(route, _)| path.contains(route))
                                .map_or_else(
                                    || (404, "{}".to_owned()),
                                    |(_, replies)| {
                                        if replies.len() > 1 {
                                            replies.pop_front().unwrap()
                                        } else {
                                            replies.front().cloned().unwrap()
                                        }
                                    },
                                )
                        };
                        let response = format!(
                            "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
            });
            Self {
                base_url,
                requests,
                handle,
            }
        }

        /// How many requests started with `prefix` (`"POST /login"`).
        fn count(&self, prefix: &str) -> usize {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.starts_with(prefix))
                .count()
        }
    }

    /// A JWT whose payload is `{"exp": exp}` (the signature is not checked).
    fn token(payload: &str) -> String {
        format!("eyJhbGciOiJIUzI1NiJ9.{payload}.c2ln")
    }

    /// `{"exp":4102444800}`: 2100-01-01.
    const FUTURE_PAYLOAD: &str = "eyJleHAiOjQxMDI0NDQ4MDB9";
    /// `{"exp":1000000000}`: 2001-09-09.
    const PAST_PAYLOAD: &str = "eyJleHAiOjEwMDAwMDAwMDB9";

    fn login_reply(payload: &str) -> (u16, String) {
        (200, format!(r#"{{"token":"{}"}}"#, token(payload)))
    }

    fn user_reply(remaining: i32, reset: &str) -> (u16, String) {
        (
            200,
            format!(
                r#"{{"data":{{"allowed_downloads":20,"remaining_downloads":{remaining},"reset_time_utc":"{reset}"}}}}"#
            ),
        )
    }

    fn download_reply(base: &str, remaining: i32) -> (u16, String) {
        (
            200,
            format!(
                r#"{{"link":"{base}/file/1.srt","remaining":{remaining},"reset_time_utc":"2100-01-01T00:00:00Z"}}"#
            ),
        )
    }

    const SRT: &str = "1\n00:00:01,000 --> 00:00:02,000\nhi\n";

    /// A provider over `mock` with an account configured.
    fn account_provider(
        mock: &ApiMock,
        username: &str,
    ) -> (OpenSubtitlesProvider, Arc<dyn PluginManager>) {
        let config = format!(r#"{{"Username":"{username}","Password":"secret"}}"#);
        let plugins =
            crate::plugin_config::tests_support::manager_with(PLUGIN_ID, config.into_bytes());
        let provider =
            OpenSubtitlesProvider::new(Arc::clone(&plugins)).with_base_url(&mock.base_url);
        (provider, plugins)
    }

    /// `OpenSubtitleDownloader.Login`: the token is kept until it expires
    /// (`DateTime.UtcNow < _login.ExpirationDate`), so downloads do not log in
    /// again; the user's quota is read once, after the login.
    #[tokio::test]
    async fn downloads_reuse_one_login_until_it_expires() {
        let mock = ApiMock::start(|base| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(20, "2100-01-01T00:00:00Z")]),
                (
                    "/download",
                    vec![download_reply(base, 19), download_reply(base, 18)],
                ),
                ("/file/", vec![(200, SRT.to_owned())]),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        for _ in 0..2 {
            let response = provider.get_subtitles("srt-en-1").await.expect("download");
            assert_eq!(response.content, SRT.as_bytes());
        }

        assert_eq!(mock.count("POST /login"), 1);
        assert_eq!(mock.count("GET /infos/user"), 1);
        assert_eq!(mock.count("POST /download"), 2);
    }

    #[tokio::test]
    async fn an_expired_token_logs_in_again() {
        let mock = ApiMock::start(|base| {
            vec![
                ("/login", vec![login_reply(PAST_PAYLOAD)]),
                ("/infos/user", vec![user_reply(20, "2100-01-01T00:00:00Z")]),
                ("/download", vec![download_reply(base, 19)]),
                ("/file/", vec![(200, SRT.to_owned())]),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        for _ in 0..2 {
            provider.get_subtitles("srt-en-1").await.expect("download");
        }

        assert_eq!(mock.count("POST /login"), 2);
    }

    /// `HttpStatusCode.Unauthorized` on `/download`: the token is dropped and
    /// the download tried again with a fresh login.
    #[tokio::test]
    async fn a_rejected_token_logs_in_again_and_retries() {
        let mock = ApiMock::start(|base| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(20, "2100-01-01T00:00:00Z")]),
                (
                    "/download",
                    vec![
                        (401, r#"{"message":"expired"}"#.to_owned()),
                        download_reply(base, 19),
                    ],
                ),
                ("/file/", vec![(200, SRT.to_owned())]),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        provider.get_subtitles("srt-en-1").await.expect("download");

        assert_eq!(mock.count("POST /login"), 2);
        assert_eq!(mock.count("POST /download"), 2);
    }

    /// `NotAcceptable` with no downloads left: the quota is spent, and until
    /// its reset time no download is attempted at all.
    #[tokio::test]
    async fn a_spent_quota_stops_downloads_until_it_resets() {
        let mock = ApiMock::start(|_| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(1, "2100-01-01T00:00:00Z")]),
                (
                    "/download",
                    vec![(
                        406,
                        r#"{"remaining":0,"reset_time_utc":"2100-01-01T00:00:00Z","message":"limit"}"#
                            .to_owned(),
                    )],
                ),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        for _ in 0..2 {
            let error = provider.get_subtitles("srt-en-1").await.expect_err("limit");
            assert!(
                error.to_string().contains("download limit reached"),
                "{error}"
            );
        }

        assert_eq!(
            mock.count("POST /download"),
            1,
            "the second try never asked"
        );
    }

    /// Past the quota's reset time, the user's quota is read again
    /// (`UpdateUserInfo`) and downloads resume.
    #[tokio::test]
    async fn a_passed_reset_time_reads_the_quota_again() {
        let mock = ApiMock::start(|base| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(5, "2100-01-01T00:00:00Z")]),
                (
                    "/download",
                    vec![
                        (
                            406,
                            r#"{"remaining":0,"reset_time_utc":"2001-01-01T00:00:00Z"}"#.to_owned(),
                        ),
                        download_reply(base, 4),
                    ],
                ),
                ("/file/", vec![(200, SRT.to_owned())]),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        let error = provider.get_subtitles("srt-en-1").await.expect_err("spent");
        assert!(
            error.to_string().contains("download limit reached"),
            "{error}"
        );
        provider
            .get_subtitles("srt-en-1")
            .await
            .expect("the reset passed: downloads resume");

        assert_eq!(mock.count("GET /infos/user"), 2);
        assert_eq!(mock.count("POST /download"), 2);
        assert_eq!(mock.count("POST /login"), 1);
    }

    /// Past the reset time, a spent session whose token has expired is not
    /// read again with that token: the download logs in afresh, which reads
    /// the quota.
    #[tokio::test]
    async fn an_expired_spent_session_logs_in_again_past_its_reset() {
        let mock = ApiMock::start(|base| {
            vec![
                (
                    "/login",
                    vec![login_reply(PAST_PAYLOAD), login_reply(FUTURE_PAYLOAD)],
                ),
                (
                    "/infos/user",
                    vec![
                        user_reply(0, "2001-01-01T00:00:00Z"),
                        user_reply(5, "2100-01-01T00:00:00Z"),
                    ],
                ),
                (
                    "/download",
                    vec![
                        (
                            406,
                            r#"{"remaining":0,"reset_time_utc":"2001-01-01T00:00:00Z"}"#.to_owned(),
                        ),
                        download_reply(base, 4),
                    ],
                ),
                ("/file/", vec![(200, SRT.to_owned())]),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        provider.get_subtitles("srt-en-1").await.expect_err("spent");
        provider
            .get_subtitles("srt-en-1")
            .await
            .expect("a fresh login");

        assert_eq!(mock.count("POST /login"), 2);
        assert_eq!(
            mock.count("GET /infos/user"),
            2,
            "no read with the expired token"
        );
    }

    /// Past the reset time, a refused quota read drops the session instead of
    /// leaving the quota spent for good.
    #[tokio::test]
    async fn a_refused_quota_read_past_the_reset_logs_in_again() {
        let mock = ApiMock::start(|base| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                (
                    "/infos/user",
                    vec![
                        user_reply(0, "2001-01-01T00:00:00Z"),
                        (401, r#"{"message":"revoked"}"#.to_owned()),
                        user_reply(5, "2100-01-01T00:00:00Z"),
                    ],
                ),
                (
                    "/download",
                    vec![
                        (
                            406,
                            r#"{"remaining":0,"reset_time_utc":"2001-01-01T00:00:00Z"}"#.to_owned(),
                        ),
                        download_reply(base, 4),
                    ],
                ),
                ("/file/", vec![(200, SRT.to_owned())]),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        provider.get_subtitles("srt-en-1").await.expect_err("spent");
        provider
            .get_subtitles("srt-en-1")
            .await
            .expect("a fresh login");

        assert_eq!(mock.count("POST /login"), 2);
        assert_eq!(mock.count("GET /infos/user"), 3);
    }

    /// The retry after a rejected token is bounded: a second rejection fails
    /// the download, and its token is not kept.
    #[tokio::test]
    async fn a_second_rejection_fails_the_download_and_drops_the_token() {
        let mock = ApiMock::start(|_| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(20, "2100-01-01T00:00:00Z")]),
                ("/download", vec![(401, r#"{"message":"no"}"#.to_owned())]),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        let error = provider.get_subtitles("srt-en-1").await.expect_err("401");
        assert!(
            error
                .to_string()
                .contains("Invalid response for file 1: 401"),
            "{error}"
        );
        assert_eq!(mock.count("POST /login"), 2);
        assert_eq!(mock.count("POST /download"), 2);

        provider.get_subtitles("srt-en-1").await.expect_err("401");
        // The next download starts with a login of its own (kept, it would
        // have tried the rejected token first: 3 logins), then retries once.
        assert_eq!(
            mock.count("POST /login"),
            4,
            "the rejected token was dropped"
        );
    }

    /// `Login`'s account check: a download without a username or password
    /// fails before any request.
    #[tokio::test]
    async fn downloads_need_an_account() {
        let mock = ApiMock::start(|_| Vec::new()).await;
        let (provider, _) = account_provider(&mock, " ");

        let error = provider
            .get_subtitles("srt-en-1")
            .await
            .expect_err("no account");

        assert!(matches!(error, ServiceError::Unauthorized(_)), "{error:?}");
        assert_eq!(mock.count(""), 0);
    }

    /// A refused download names the API's reply, except an HTML page.
    #[tokio::test]
    async fn a_refused_download_names_the_reply_unless_it_is_html() {
        let mock = ApiMock::start(|_| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(20, "2100-01-01T00:00:00Z")]),
                (
                    "/download",
                    vec![
                        (403, r#"{"message":"forbidden"}"#.to_owned()),
                        (503, "<HTML><body>down</body></HTML>".to_owned()),
                    ],
                ),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        let error = provider.get_subtitles("srt-en-1").await.expect_err("403");
        assert!(
            error.to_string().contains(
                r#"403 Forbidden

{"message":"forbidden"}"#
            ),
            "{error}"
        );
        let error = provider.get_subtitles("srt-en-1").await.expect_err("503");
        assert!(
            error
                .to_string()
                .ends_with("503 Service Unavailable\n\n[html]"),
            "{error}"
        );
    }

    /// The quota only informs downloads: a reply missing its reset time, or
    /// carrying an unreadable one, does not fail a download.
    #[tokio::test]
    async fn a_partial_quota_does_not_fail_a_download() {
        let mock = ApiMock::start(|base| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![(200, r#"{"data":{"remaining_downloads":20}}"#.to_owned())]),
                (
                    "/download",
                    vec![(
                        200,
                        format!(r#"{{"link":"{base}/file/1.srt","remaining":19,"reset_time_utc":"soon"}}"#),
                    )],
                ),
                ("/file/", vec![(200, SRT.to_owned())]),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        let response = provider.get_subtitles("srt-en-1").await.expect("download");

        assert_eq!(response.content, SRT.as_bytes());
    }

    /// `UpdateResetTime`: a reset time within two seconds of the known one is
    /// the same time.
    #[test]
    fn reset_times_within_two_seconds_are_the_same() {
        let known = chrono::DateTime::from_timestamp(4_102_444_800, 0).unwrap();
        let mut state = DownloadState {
            limit_reset: Some(known),
            ..DownloadState::default()
        };

        update_reset_time(&mut state, known + chrono::Duration::milliseconds(1_999));
        assert_eq!(state.limit_reset, Some(known));
        update_reset_time(&mut state, known - chrono::Duration::milliseconds(2_500));
        assert_eq!(
            state.limit_reset,
            Some(known - chrono::Duration::milliseconds(2_500))
        );
    }

    /// Downloads running at once share one login: the second waits for the
    /// first's login instead of making its own.
    #[tokio::test]
    async fn concurrent_downloads_share_one_login() {
        let mock = ApiMock::start(|base| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(20, "2100-01-01T00:00:00Z")]),
                ("/download", vec![download_reply(base, 19)]),
                ("/file/", vec![(200, SRT.to_owned())]),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        let (first, second) = tokio::join!(
            provider.get_subtitles("srt-en-1"),
            provider.get_subtitles("srt-en-2")
        );
        first.expect("first download");
        second.expect("second download");

        assert_eq!(mock.count("POST /login"), 1);
        assert_eq!(mock.count("GET /infos/user"), 1);
        assert_eq!(mock.count("POST /download"), 2);
    }

    /// A download that used up the quota is the last one: the next is
    /// refused until the reset time without asking the API.
    #[tokio::test]
    async fn a_download_that_spends_the_quota_is_the_last() {
        let mock = ApiMock::start(|base| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(1, "2100-01-01T00:00:00Z")]),
                ("/download", vec![download_reply(base, 0)]),
                ("/file/", vec![(200, SRT.to_owned())]),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        provider
            .get_subtitles("srt-en-1")
            .await
            .expect("the last download");
        let error = provider.get_subtitles("srt-en-1").await.expect_err("spent");

        assert!(
            error.to_string().contains("download limit reached"),
            "{error}"
        );
        assert_eq!(mock.count("POST /download"), 1);
    }

    /// A refused token is forgotten only while the session still holds it: a
    /// session another download has already renewed is kept.
    #[test]
    fn only_the_rejected_token_is_forgotten() {
        let session = |token: &str| Session {
            username: "user".to_owned(),
            password: SecretString::from("secret".to_owned()),
            api_key: SecretString::from(DEFAULT_API_KEY.to_owned()),
            token: SecretString::from(token.to_owned()),
            expires_at: chrono::DateTime::from_timestamp(4_102_444_800, 0).unwrap(),
            quota: None,
        };
        let rejected = SecretString::from("first".to_owned());
        let mut state = DownloadState {
            session: Some(session("second")),
            ..DownloadState::default()
        };

        state.forget_rejected(&rejected);
        assert!(
            state
                .session
                .as_ref()
                .is_some_and(|s| s.holds(&SecretString::from("second".to_owned()))),
            "a session renewed by another download is kept"
        );

        state.session = Some(session("first"));
        state.forget_rejected(&rejected);
        assert!(
            state.session.is_none(),
            "the session holding the rejected token is dropped"
        );
    }

    /// `ConfigurationChanged` forces a login: a token belongs to the account
    /// it was issued for.
    #[tokio::test]
    async fn changed_credentials_log_in_again() {
        let mock = ApiMock::start(|base| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(20, "2100-01-01T00:00:00Z")]),
                ("/download", vec![download_reply(base, 19)]),
                ("/file/", vec![(200, SRT.to_owned())]),
            ]
        })
        .await;
        let (provider, plugins) = account_provider(&mock, "first");
        provider.get_subtitles("srt-en-1").await.expect("download");

        plugins
            .set_plugin_configuration(
                PLUGIN_ID,
                br#"{"Username":"second","Password":"secret"}"#.to_vec(),
            )
            .await
            .unwrap();
        provider.get_subtitles("srt-en-1").await.expect("download");

        assert_eq!(mock.count("POST /login"), 2);
    }

    /// `LoginInfo.ExpirationDate`: the JWT payload's `exp`, in seconds; a
    /// token without one counts as expired.
    #[rstest::rstest]
    #[case::year_2100(token(FUTURE_PAYLOAD), 4_102_444_800)]
    #[case::year_2001(token(PAST_PAYLOAD), 1_000_000_000)]
    #[case::no_exp(token("e30"), 0)]
    #[case::not_a_jwt("opaque".to_owned(), 0)]
    #[case::bad_base64(token("!!!"), 0)]
    fn the_token_expires_at_its_exp(#[case] token: String, #[case] expected: i64) {
        assert_eq!(token_expiry(&token).timestamp(), expected);
    }

    #[test]
    fn base64url_decodes_with_and_without_padding() {
        assert_eq!(decode_base64url("aGk").as_deref(), Some(&b"hi"[..]));
        assert_eq!(decode_base64url("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(decode_base64url("_-8").as_deref(), Some(&[0xff, 0xef][..]));
        assert_eq!(decode_base64url("a"), None);
    }

    #[test]
    fn search_response_maps_to_namespaced_candidates() {
        let json = r#"{
            "data": [
              {"attributes": {
                 "language": "en",
                 "release": "Movie.2021.1080p",
                 "download_count": 42,
                 "hearing_impaired": false,
                 "uploader": {"name": "alice"},
                 "files": [{"file_id": 998877, "file_name": "movie.en.srt"}]
              }}
            ]
        }"#;
        let parsed: SearchResponse = serde_json::from_str(json).unwrap();
        let mapped = map_search(&parsed, "eng");
        assert_eq!(mapped.len(), 1);
        let c = &mapped[0];
        assert_eq!(c.id.as_deref(), Some("srt-eng-998877"));
        assert_eq!(c.hearing_impaired, Some(false));
        assert_eq!(c.forced, None);
        assert_eq!(c.provider_name.as_deref(), Some("opensubtitles"));
        assert_eq!(c.three_letter_iso_language_name.as_deref(), Some("eng"));
        assert_eq!(c.author.as_deref(), Some("alice"));
        assert_eq!(c.download_count, Some(42));
    }

    #[test]
    fn candidate_language_is_the_requested_code_not_the_api_tag() {
        // The API tags pt-BR/zh-CN carry a '-'; the plugin stamps request.Language instead.
        let json = r#"{"data": [{"attributes": {"language": "pt-BR",
            "files": [{"file_id": 5}]}}]}"#;
        let parsed: SearchResponse = serde_json::from_str(json).unwrap();
        let c = &map_search(&parsed, "por")[0];
        assert_eq!(c.id.as_deref(), Some("srt-por-5"));
        assert_eq!(c.three_letter_iso_language_name.as_deref(), Some("por"));
        let p = parse_subtitle_id("srt-por-5").unwrap();
        assert_eq!((p.language.as_str(), p.file_id), ("por", 5));
    }

    #[test]
    fn subtitle_id_round_trips_like_the_plugin() {
        let attrs = SearchAttributes {
            hearing_impaired: Some(true),
            foreign_parts_only: Some(true),
            ..Default::default()
        };
        let id = build_subtitle_id("eng", &attrs, 42);
        assert_eq!(id, "srt-eng-42-sdh-forced");
        let p = parse_subtitle_id(&id).unwrap();
        assert_eq!(
            (p.format.as_str(), p.language.as_str(), p.file_id),
            ("srt", "eng", 42)
        );
        assert!(p.is_hearing_impaired && p.is_forced);
        let plain = parse_subtitle_id("srt-fre-7").unwrap();
        assert!(!plain.is_hearing_impaired && !plain.is_forced);
        assert!(parse_subtitle_id("998877").is_none());
        assert!(parse_subtitle_id("srt-eng-x").is_none());
    }

    #[test]
    fn language_code_three_to_two_common_cases() {
        for (three, two) in [("eng", "en"), ("spa", "es"), ("fra", "fr"), ("jpn", "ja")] {
            assert_eq!(three_to_two_letter(three), two);
        }
        // French alternate 3-letter code also maps.
        assert_eq!(three_to_two_letter("fre"), "fr");
        // Portuguese and Chinese use the regioned tags the API actually lists.
        assert_eq!(three_to_two_letter("por"), "pt-PT");
        assert_eq!(three_to_two_letter("chi"), "zh-CN");
        // Unknown passes through (truncated to 2 for the search param).
        assert_eq!(three_to_two_letter("xyz"), "xy");
    }
}
