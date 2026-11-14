//! OpenSubtitles subtitle provider — a [`SubtitleProvider`] over the
//! opensubtitles.com REST API v1.
//!
//! Faithful to Jellyfin's OpenSubtitles **plugin**: the provider is a
//! compiled-in Ferrofin plugin ([`PLUGIN_ID`] / [`PLUGIN_NAME`]) whose credentials
//! ([`OpenSubtitlesConfig`] — account + optional API key override) are set through
//! the dashboard plugin-configuration page (`POST /Plugins/{id}/Configuration`)
//! and read back via the [`PluginManager`]. Jellyfin's shared application key is
//! used by default. Downloads and automated searches still require an
//! OpenSubtitles account; with no account or explicit key configured, the
//! provider remains inactive.
//!
//! Searches and downloads share one session, as the plugin's downloader does:
//! an account logs in before its first search or download, the login's token
//! is reused until it expires or the configured account changes, and the
//! account's daily quota is tracked, so a spent quota stops downloads until its
//! reset time, and automated searches until the session is renewed, instead
//! of logging in again for every file.
//!
//! API shape (<https://opensubtitles.stoplight.io>): every call carries the
//! `Api-Key` header and a descriptive `User-Agent`; `POST /login` exchanges the
//! account for a bearer token, `GET /subtitles` searches, and `POST /download`
//! turns a `file_id` into a one-time download link whose body is the subtitle.
//!
//! That link is a destination OpenSubtitles chose, so it is fetched through
//! the guard image downloads use (`GuardedHttp`): a private address is
//! refused on the link and on every redirect from it, unless the server's
//! `subtitle_private_http_allow` lists it
//! ([`with_link_destinations`](OpenSubtitlesProvider::with_link_destinations)).
//! The plugin downloads it with a default `HttpClient`; the guard is a
//! divergence (owner decision 2026-11-03, gl-1160).

use crate::rate_limit::CountedBody as _;
use crate::rate_limit::{BodyError, LimitedRequest as _, RateLimiter, RequestError};
use std::collections::HashSet;
use std::sync::{Arc, PoisonError};

use crate::destination::PrivateAllowList;
use crate::error::ProvidersError;
use crate::guarded_http::{FetchKind, GuardedHttp, SendError};
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
const USER_AGENT: &str = ferrofin_health::USER_AGENT;

/// How often an automated search says the daily quota is spent: the
/// plugin's `_lastRatelimitLog` interval.
const LIMIT_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// The download link's fetch, for its limiter and log lines.
static LINK_FETCH: FetchKind = FetchKind::new(
    "opensubtitles",
    "subtitle",
    "subtitle downloads",
    "subtitle_private_http_allow (FERROFIN_SUBTITLE_PRIVATE_HTTP_ALLOW) lists it",
    Some(USER_AGENT),
);

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
    /// the shared application key; an explicit key alone still permits manual
    /// searches.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.api_key.expose_secret().trim().is_empty()
            || (!self.username.trim().is_empty()
                && !self.password.expose_secret().trim().is_empty())
    }

    /// Whether an account is configured: both a username and a password
    /// (`Login`'s `IsNullOrWhiteSpace` checks). Downloads and automated
    /// searches need one.
    #[must_use]
    pub fn has_account(&self) -> bool {
        !self.username.trim().is_empty() && !self.password.expose_secret().trim().is_empty()
    }

    /// Whether `other` names the same account and key.
    fn same_account(&self, other: &Self) -> bool {
        self.username == other.username
            && self.password.expose_secret() == other.password.expose_secret()
            && self.effective_api_key() == other.effective_api_key()
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
    /// Whether the API refused the account itself, which `Login` marks as
    /// `CredentialsInvalid`: a 401, or a 400 for an e-mail address given as
    /// the username.
    fn refuses_account(&self, cfg: &OpenSubtitlesConfig) -> bool {
        match self {
            Self::Rejected { status, .. } => {
                *status == reqwest::StatusCode::UNAUTHORIZED
                    || (*status == reqwest::StatusCode::BAD_REQUEST && cfg.username.contains('@'))
            }
            Self::Request(_) => false,
        }
    }

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

/// One page of `GET /subtitles` (`SearchResult`).
#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    total_pages: i32,
    #[serde(default)]
    page: i32,
    #[serde(default)]
    data: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    #[serde(default)]
    attributes: SearchAttributes,
}

/// One result's `Attributes`. The plugin's non-nullable numbers
/// (`DownloadCount`, `Ratings`, `Fps`) read an absent one as zero; an absent
/// `upload_date` leaves the candidate's `DateCreated` out, where the plugin's
/// non-nullable `DateTime` would send 0001-01-01.
#[derive(Deserialize, Default)]
struct SearchAttributes {
    #[serde(default)]
    hearing_impaired: Option<bool>,
    #[serde(default)]
    moviehash_match: Option<bool>,
    #[serde(default)]
    foreign_parts_only: Option<bool>,
    #[serde(default)]
    machine_translated: Option<bool>,
    #[serde(default)]
    ai_translated: Option<bool>,
    #[serde(default)]
    from_trusted: Option<bool>,
    #[serde(default)]
    release: Option<String>,
    #[serde(default)]
    comments: Option<String>,
    #[serde(default)]
    download_count: Option<i32>,
    #[serde(default)]
    ratings: Option<f32>,
    #[serde(default)]
    fps: Option<f32>,
    #[serde(default, deserialize_with = "lenient_time")]
    upload_date: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    uploader: Option<Uploader>,
    #[serde(default)]
    feature_details: Option<FeatureDetails>,
    #[serde(default)]
    files: Vec<SubFile>,
}

/// What a result is for (`FeatureDetails`).
#[derive(Deserialize, Default)]
struct FeatureDetails {
    #[serde(default)]
    feature_type: Option<String>,
    #[serde(default)]
    imdb_id: Option<i64>,
    #[serde(default)]
    season_number: Option<i32>,
    #[serde(default)]
    episode_number: Option<i32>,
}

#[derive(Deserialize, Default)]
struct Uploader {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize, Default)]
struct SubFile {
    #[serde(default)]
    file_id: Option<i64>,
}

/// `GET /infos/languages` (`EncapsulatedLanguageList`).
#[derive(Deserialize)]
struct LanguageList {
    #[serde(default)]
    data: Option<Vec<LanguageCode>>,
}

/// One language the API searches (`LanguageInfo`).
#[derive(Deserialize)]
struct LanguageCode {
    #[serde(default)]
    language_code: Option<String>,
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

/// What searches and downloads share: the session, the quota's reset time,
/// when an automated search last said the quota is spent, and the account
/// the API refused (the plugin's `_login`, `_limitReset`, `_lastRatelimitLog`
/// and `CredentialsInvalid`, which the plugin saves and this keeps for the
/// process).
#[derive(Default)]
struct DownloadState {
    session: Option<Session>,
    limit_reset: Option<chrono::DateTime<chrono::Utc>>,
    limit_logged: Option<std::time::Instant>,
    rejected: Option<OpenSubtitlesConfig>,
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

/// `Search`'s filters, order and mapping over every page's results.
///
/// A result is kept when its first file has an id that an automated search
/// has not found empty (`BadSubtitleFilter`), when it is for the item's own
/// feature (`MediaFilter`: the content type, an episode's season and number,
/// a movie's IMDb id when it has one), and, for a perfect-match search, when
/// it matches the file's hash (`MatchFilter`). The results are ordered by hash
/// match, download count, rating and trusted uploader, and each becomes one
/// candidate.
///
/// Each candidate's `id` is the provider-local id ([`build_subtitle_id`]) of
/// the result's first file; the subtitle manager namespaces it with the
/// provider's MD5 id, as upstream `SubtitleManager.Normalize` does. As in the
/// plugin, the language stamped on the candidate and into its id is the
/// CALLER's 3-letter code (`request.Language`), never the API's own tag: the
/// search was already filtered by it, and the API's tags (`pt-BR`, `zh-CN`)
/// carry a `-` that would break the id.
fn map_search(
    items: Vec<SearchItem>,
    request: &SubtitleSearchRequest,
    imdb_id: i64,
    bad_ids: &HashSet<i64>,
) -> Vec<RemoteSubtitleInfo> {
    let episode = request.content_type == SubtitleMediaType::Episode;
    let feature_type = if episode { "Episode" } else { "Movie" };
    let mut kept: Vec<(i64, SearchAttributes)> = items
        .into_iter()
        .filter_map(|item| {
            let attrs = item.attributes;
            let file_id = attrs.files.first().and_then(|file| file.file_id)?;
            if request.is_automated && bad_ids.contains(&file_id) {
                return None;
            }
            let details = attrs.feature_details.as_ref()?;
            let own_feature = details.feature_type.as_deref() == Some(feature_type)
                && if episode {
                    details.season_number == request.parent_index_number
                        && details.episode_number == request.index_number
                } else {
                    imdb_id == 0 || details.imdb_id == Some(imdb_id)
                };
            let matched =
                request.is_perfect_match != Some(true) || attrs.moviehash_match.unwrap_or(false);
            (own_feature && matched).then_some((file_id, attrs))
        })
        .collect();
    kept.sort_by(|(_, a), (_, b)| {
        b.moviehash_match
            .unwrap_or(false)
            .cmp(&a.moviehash_match.unwrap_or(false))
            .then(
                b.download_count
                    .unwrap_or(0)
                    .cmp(&a.download_count.unwrap_or(0)),
            )
            .then(
                b.ratings
                    .unwrap_or(0.0)
                    .total_cmp(&a.ratings.unwrap_or(0.0)),
            )
            .then(b.from_trusted.cmp(&a.from_trusted))
    });
    kept.into_iter()
        .map(|(file_id, attrs)| RemoteSubtitleInfo {
            id: Some(build_subtitle_id(&request.language, &attrs, file_id)),
            provider_name: Some(PLUGIN_NAME.to_owned()),
            three_letter_iso_language_name: Some(request.language.clone()),
            name: attrs.release,
            format: Some("srt".to_owned()),
            author: attrs.uploader.and_then(|uploader| uploader.name),
            comment: attrs.comments,
            date_created: attrs.upload_date,
            community_rating: Some(attrs.ratings.unwrap_or(0.0)),
            frame_rate: Some(attrs.fps.unwrap_or(0.0)),
            download_count: Some(attrs.download_count.unwrap_or(0)),
            is_hash_match: attrs.moviehash_match,
            ai_translated: attrs.ai_translated,
            machine_translated: attrs.machine_translated,
            forced: attrs.foreign_parts_only,
            hearing_impaired: attrs.hearing_impaired,
        })
        .collect()
}

/// The item's IMDb number, as the plugin reads it (`long.TryParse` of the id
/// without its leading `t`s), or `0` when it has none.
fn imdb_number(imdb_id: Option<&str>) -> i64 {
    imdb_id
        .and_then(|id| id.trim_start_matches('t').trim().parse().ok())
        .unwrap_or(0)
}

/// `Search`'s parameters: the language, the content `type`, the file's hash
/// (and `moviehash_match=only` for a perfect match), then the IMDb id when
/// the item has one, or else the file's name and an episode's numbers.
fn search_options(
    request: &SubtitleSearchRequest,
    language: String,
    hash: Option<String>,
    imdb_id: i64,
    media_path: &str,
) -> Vec<(&'static str, String)> {
    let episode = request.content_type == SubtitleMediaType::Episode;
    let content_type = if episode { "episode" } else { "movie" };
    let mut options = vec![("languages", language), ("type", content_type.to_owned())];
    if let Some(hash) = hash {
        options.push(("moviehash", hash));
        if request.is_perfect_match == Some(true) {
            options.push(("moviehash_match", "only".to_owned()));
        }
    }
    if imdb_id != 0 {
        options.push(("imdb_id", imdb_id.to_string()));
        return options;
    }
    // `Path.GetFileName`: after the platform's last separator.
    let file_name = media_path
        .rsplit(std::path::is_separator)
        .next()
        .unwrap_or(media_path);
    options.push(("query", file_name.to_owned()));
    if episode {
        options.extend(
            request
                .parent_index_number
                .map(|n| ("season_number", n.to_string())),
        );
        options.extend(
            request
                .index_number
                .map(|n| ("episode_number", n.to_string())),
        );
    }
    options
}

/// `RequestHandler.AddQueryString`: `path` with `params` sorted by name, each
/// name and value lowercased (`ToLowerInvariant`) and encoded as
/// `HttpUtility.UrlEncode` encodes it.
fn add_query_string(path: &str, params: &[(&str, String)]) -> String {
    use ferrofin_util::string_extensions::lower_invariant;
    if params.is_empty() {
        return path.to_owned();
    }
    let mut sorted: Vec<_> = params.iter().collect();
    sorted.sort_by_key(|(name, _)| *name);
    let pairs: Vec<String> = sorted
        .into_iter()
        .map(|(name, value)| {
            format!(
                "{}={}",
                url_encode(&lower_invariant(name)),
                url_encode(&lower_invariant(value)),
            )
        })
        .collect();
    format!("{path}?{}", pairs.join("&"))
}

/// `HttpUtility.UrlEncode`: the UTF-8 bytes, with a space as `+`, ASCII
/// letters, digits and `-_.!*()` as they are, and every other byte as `%xx`
/// in lowercase hex.
fn url_encode(text: &str) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b' ' => encoded.push('+'),
            b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'*'
            | b'('
            | b')' => encoded.push(char::from(byte)),
            _ => {
                let _ = write!(encoded, "%{byte:02x}");
            }
        }
    }
    encoded
}

/// The OpenSubtitles-backed subtitle provider.
pub struct OpenSubtitlesProvider {
    http: reqwest::Client,
    /// The download link's client: refuses the private destinations the
    /// server's `subtitle_private_http_allow` does not list.
    link_http: GuardedHttp,
    limiter: RateLimiter,
    plugins: Arc<dyn PluginManager>,
    base_url: String,
    /// Held across a login, so concurrent searches and downloads share one.
    state: tokio::sync::Mutex<DownloadState>,
    /// The language codes the API searches, read once (the plugin's
    /// `_languages`).
    languages: tokio::sync::Mutex<Vec<String>>,
    /// The files whose link served an empty subtitle, which automated
    /// searches skip (the plugin's `_badSubtitleIds`).
    bad_subtitle_ids: std::sync::Mutex<HashSet<i64>>,
}

impl std::fmt::Debug for OpenSubtitlesProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenSubtitlesProvider")
            .finish_non_exhaustive()
    }
}

impl OpenSubtitlesProvider {
    /// Builds the provider, reading its credentials from the plugin config
    /// store. Its download links may reach no private address until
    /// [`with_link_destinations`](Self::with_link_destinations) lists some.
    #[must_use]
    pub fn new(plugins: Arc<dyn PluginManager>) -> Self {
        Self {
            http: crate::http_client::client(),
            link_http: Self::link_http(PrivateAllowList::default()),
            limiter: RateLimiter::new("opensubtitles"),
            plugins,
            base_url: API_BASE.to_owned(),
            state: tokio::sync::Mutex::new(DownloadState::default()),
            languages: tokio::sync::Mutex::new(Vec::new()),
            bad_subtitle_ids: std::sync::Mutex::new(HashSet::new()),
        }
    }

    /// Lets the download links reach the private hosts and networks `allowed`
    /// lists (`subtitle_private_http_allow`); every other private address
    /// stays refused, on the link and on every redirect from it.
    #[must_use]
    pub fn with_link_destinations(mut self, allowed: PrivateAllowList) -> Self {
        self.link_http = Self::link_http(allowed);
        self
    }

    /// The download links' client under `allowed`, through the environment's
    /// proxy if it configures one.
    fn link_http(allowed: PrivateAllowList) -> GuardedHttp {
        GuardedHttp::with_environment_proxy(Arc::new(allowed), &LINK_FETCH)
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
            .map_err(|e| LoginFailure::Request(body_err(e)))
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
        let token = match self.login_reply(cfg).await {
            Ok(reply) => {
                state.rejected = None;
                reply.token
            }
            Err(failure) => {
                if failure.refuses_account(cfg) {
                    tracing::warn!(
                        "OpenSubtitles refused the account; until it logs in again or changes, \
                         manual searches go on without a login and automated ones find nothing"
                    );
                    state.rejected = Some(cfg.clone());
                }
                return Err(failure.into_service_error());
            }
        };
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
        let body = response.counted_bytes().await.map_err(body_err)?;
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
    /// refusal leaves spent until a later `Login` (a search's, once the token
    /// has expired) replaces the session.
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

    /// The start of `Search`: an account logs in, or reuses its session,
    /// before every search, and an automated search goes on only while the
    /// account has downloads left.
    ///
    /// Without a login an automated search finds nothing and a manual one
    /// searches on the key alone: upstream's `Login` skips an account it marked
    /// invalid, leaving `_login` null. With no account at all it throws, and a
    /// manual search on an API key alone still searches here (gl-808).
    async fn may_search(
        &self,
        request: &SubtitleSearchRequest,
        cfg: &OpenSubtitlesConfig,
    ) -> Result<bool, ServiceError> {
        let without_login = || {
            if request.is_automated {
                tracing::debug!(item_id = %request.item_id, "no OpenSubtitles login; automated search skipped");
            }
            Ok(!request.is_automated)
        };
        if !cfg.has_account() {
            return without_login();
        }
        let mut state = self.state.lock().await;
        if state
            .rejected
            .as_ref()
            .is_some_and(|rejected| rejected.same_account(cfg))
        {
            return without_login();
        }
        self.session_token(&mut state, cfg).await?;
        if !request.is_automated || !state.session.as_ref().is_some_and(Session::quota_spent) {
            return Ok(true);
        }
        if state
            .limit_logged
            .is_none_or(|logged| logged.elapsed() > LIMIT_LOG_INTERVAL)
        {
            tracing::info!(
                "OpenSubtitles daily download limit reached; automated searches find nothing"
            );
            state.limit_logged = Some(std::time::Instant::now());
        }
        Ok(false)
    }

    /// `GetLanguage`: the search's `languages` value. It is the request's
    /// two-letter code, with the regioned tags the plugin uses for bare `zh`
    /// and `pt`, spelled as the API's language list spells it; a regioned
    /// code the list lacks falls back to its language. `None` for a language
    /// with no two-letter code or one the list lacks, which upstream refuses
    /// with an exception.
    ///
    /// The list is read once and kept; a failed read fails the search, and
    /// the next search reads it again.
    async fn search_language(
        &self,
        request: &SubtitleSearchRequest,
        cfg: &OpenSubtitlesConfig,
    ) -> Result<Option<String>, ServiceError> {
        let mut code = match request.two_letter_iso_language_name.as_deref() {
            Some("zh") => "zh-CN",
            Some("pt") => "pt-PT",
            Some(code) if !code.is_empty() => code,
            _ => return Ok(None),
        };
        let mut languages = self.languages.lock().await;
        if languages.is_empty() {
            *languages = self.language_list(cfg).await?;
        }
        loop {
            if let Some(found) = languages
                .iter()
                .find(|listed| listed.eq_ignore_ascii_case(code))
            {
                return Ok(Some(found.clone()));
            }
            // Not rewritten again: upstream's recursion would turn `pt` back
            // into `pt-PT` forever when the list lacks it.
            let Some((language, _)) = code.split_once('-') else {
                return Ok(None);
            };
            code = language;
        }
    }

    /// `OpenSubtitlesApi.GetLanguageList`: the language codes the API
    /// searches.
    async fn language_list(&self, cfg: &OpenSubtitlesConfig) -> Result<Vec<String>, ServiceError> {
        let response = self
            .http
            .get(format!("{}/infos/languages", self.base_url))
            .header("Api-Key", cfg.effective_api_key())
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .send_limited(&self.limiter)
            .await
            .map_err(request_err)?;
        let status = response.status();
        let list = if status.is_success() {
            response
                .counted_json::<LanguageList>()
                .await
                .map_err(body_err)?
                .data
        } else {
            None
        };
        let list = list.ok_or_else(|| {
            ServiceError::backend(format!("Failed to get language list: {status}"))
        })?;
        Ok(list
            .into_iter()
            .filter_map(|language| language.language_code)
            .filter(|code| !code.trim().is_empty())
            .collect())
    }

    /// `OpenSubtitlesApi.SearchSubtitlesAsync`: every page's results, the
    /// pages after the first asked for by `page`.
    async fn search_pages(
        &self,
        cfg: &OpenSubtitlesConfig,
        options: &[(&str, String)],
    ) -> Result<Vec<SearchItem>, ServiceError> {
        let mut items = Vec::new();
        let mut current = 1;
        let mut last = None;
        loop {
            let mut params = options.to_vec();
            if current > 1 {
                params.push(("page", current.to_string()));
            }
            let response = self
                .http
                .get(format!(
                    "{}{}",
                    self.base_url,
                    add_query_string("/subtitles", &params)
                ))
                .header("Api-Key", cfg.effective_api_key())
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .header(reqwest::header::ACCEPT, "application/json")
                .send_limited(&self.limiter)
                .await
                .map_err(request_err)?;
            if !response.status().is_success() {
                return Err(ServiceError::backend(format!(
                    "OpenSubtitles search failed: HTTP {}",
                    response.status()
                )));
            }
            let page: SearchResponse = response.counted_json().await.map_err(body_err)?;
            if page.total_pages == 0 {
                break;
            }
            let last = *last.get_or_insert(page.total_pages);
            let next = page.page.saturating_add(1);
            // A page that does not move on ends the search; upstream would
            // ask for it again forever.
            if next <= current {
                break;
            }
            items.extend(page.data);
            if next > last {
                break;
            }
            current = next;
        }
        Ok(items)
    }

    /// The subtitle file at `link`, a destination OpenSubtitles chose: vetted
    /// on every hop (gl-1160). An empty file fails the download and is most
    /// likely broken at OpenSubtitles, so automated searches skip `file_id`
    /// from then on (`GetSubtitlesInternal`).
    async fn download_link(&self, link: &str, file_id: i64) -> Result<Vec<u8>, ServiceError> {
        let content = self
            .link_http
            .send(link)
            .await
            .map_err(link_err)?
            .response
            .error_for_status()
            // The link is a one-time capability URL: keep it out of the error text (LOGGING.md §5).
            .map_err(|error| net_err(error.without_url()))?
            .counted_bytes()
            .await
            .map_err(body_err)?;
        if String::from_utf8_lossy(&content).trim().is_empty() {
            self.bad_subtitle_ids
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(file_id);
            return Err(ServiceError::backend(format!(
                "Subtitle with Id {file_id} could not be downloaded: OK - this is most likely a \
                 broken subtitle, report at opensubtitles.com/contact and make sure to include \
                 the id"
            )));
        }
        Ok(content)
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
                // The settings page's validation clears `CredentialsInvalid`.
                self.state.lock().await.rejected = None;
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

/// Why the download link gave no response. A refusal names the setting but
/// not the host or its address, which the client has no use for: the log
/// line of the refusal carries them for the admin.
fn link_err(error: SendError) -> ServiceError {
    match error {
        SendError::Refused => ServiceError::backend(
            "OpenSubtitles download link refused: it is at a private address, which subtitle \
             downloads may not reach unless subtitle_private_http_allow lists it",
        ),
        SendError::Request(error) => request_err(error),
        SendError::Failed(reason) => {
            ServiceError::backend(format!("OpenSubtitles download link not fetched: {reason}"))
        }
    }
}

fn net_err(e: reqwest::Error) -> ServiceError {
    ProvidersError::http("OpenSubtitles request failed", e).into()
}

fn body_err(e: BodyError) -> ServiceError {
    match e {
        BodyError::Read(e) => net_err(e),
        BodyError::TooLarge => {
            ServiceError::backend("OpenSubtitles sent more than image_download_max_mb")
        }
        BodyError::Decode(e) => ServiceError::backend(format!("OpenSubtitles request failed: {e}")),
    }
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
        if !self.may_search(request, &cfg).await? {
            return Ok(Vec::new());
        }
        let imdb_id = imdb_number(request.imdb_id.as_deref());
        let episode = request.content_type == SubtitleMediaType::Episode;
        if episode
            && (request.index_number.is_none()
                || request.parent_index_number.is_none()
                || request.series_name.as_deref().is_none_or(str::is_empty))
        {
            tracing::debug!(item_id = %request.item_id, "episode information missing; not searched");
            return Ok(Vec::new());
        }
        let Some(media_path) = request
            .media_path
            .as_deref()
            .filter(|path| !path.is_empty())
        else {
            tracing::debug!(item_id = %request.item_id, "no media path; not searched");
            return Ok(Vec::new());
        };
        let Some(language) = self.search_language(request, &cfg).await? else {
            // Not searched, as upstream; at debug, since the scheduled task asks once per video.
            tracing::debug!(
                item_id = %request.item_id,
                language = ?request.language,
                "OpenSubtitles has no code for this language"
            );
            return Ok(Vec::new());
        };

        let hash = if media_path.to_ascii_lowercase().ends_with(".strm") {
            None
        } else {
            movie_hash(media_path)
                .await
                .map_err(|e| ServiceError::backend(format!("subtitle movie hash failed: {e}")))?
        };
        let options = search_options(request, language, hash, imdb_id, media_path);

        let items = self.search_pages(&cfg, &options).await?;
        let bad_ids = self
            .bad_subtitle_ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Ok(map_search(items, request, imdb_id, &bad_ids))
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
            let body = response.counted_bytes().await.map_err(body_err)?;
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

        let content = self.download_link(&link, file_id).await?;

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

    /// A provider over `mock` with an account configured, whose download
    /// links may reach `127.0.0.1`, where the mocks serve them.
    fn account_provider(
        mock: &ApiMock,
        username: &str,
    ) -> (OpenSubtitlesProvider, Arc<dyn PluginManager>) {
        let config = format!(r#"{{"Username":"{username}","Password":"secret"}}"#);
        let plugins =
            crate::plugin_config::tests_support::manager_with(PLUGIN_ID, config.into_bytes());
        let provider = OpenSubtitlesProvider::new(Arc::clone(&plugins))
            .with_base_url(&mock.base_url)
            .with_link_destinations(PrivateAllowList::parse("127.0.0.1").0);
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

    /// gl-1160: the link OpenSubtitles answers with is a destination a third
    /// party chose. With no `subtitle_private_http_allow`, a link at a
    /// private address (loopback here) is refused and never fetched, even
    /// while image downloads may reach loopback: the image allow-list does
    /// not apply to subtitles.
    #[tokio::test]
    async fn a_link_to_a_private_address_is_refused_unfetched() {
        crate::destination::test_support::allow_loopback_images();
        let mock = ApiMock::start(|base| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(20, "2100-01-01T00:00:00Z")]),
                ("/download", vec![download_reply(base, 19)]),
                ("/file/", vec![(200, SRT.to_owned())]),
            ]
        })
        .await;
        let plugins = crate::plugin_config::tests_support::manager_with(
            PLUGIN_ID,
            br#"{"Username":"user","Password":"secret"}"#.to_vec(),
        );
        let provider = OpenSubtitlesProvider::new(plugins).with_base_url(&mock.base_url);

        let error = provider
            .get_subtitles("srt-en-1")
            .await
            .expect_err("a private link is refused");

        assert!(
            error.to_string().contains("subtitle_private_http_allow"),
            "{error}"
        );
        assert_eq!(mock.count("POST /download"), 1);
        assert_eq!(mock.count("GET /file/"), 0, "the link was never fetched");
    }

    /// A link at an address `subtitle_private_http_allow` lists is fetched,
    /// with the `User-Agent` OpenSubtitles requires.
    #[tokio::test]
    async fn a_listed_link_is_fetched_with_the_user_agent() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let link = format!("http://{}/file/1.srt", listener.local_addr().unwrap());
        let file = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            let n = socket.read(&mut bytes).await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{SRT}",
                        SRT.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            String::from_utf8_lossy(&bytes[..n]).to_ascii_lowercase()
        });
        let mock = ApiMock::start(move |_| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(20, "2100-01-01T00:00:00Z")]),
                (
                    "/download",
                    vec![(200, format!(r#"{{"link":"{link}","remaining":19}}"#))],
                ),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        let response = provider.get_subtitles("srt-en-1").await.expect("download");

        assert_eq!(response.content, SRT.as_bytes());
        let request = file.await.unwrap();
        assert!(
            request.contains(&format!("user-agent: {}", USER_AGENT.to_ascii_lowercase())),
            "{request}"
        );
    }

    /// A link on a listed host that redirects to a private address the list
    /// does not name is refused at the hop, and nothing reaches that address.
    #[tokio::test]
    async fn a_redirect_from_a_listed_link_to_a_private_address_is_refused() {
        use crate::mock_http::MockServer;
        let secret = MockServer::start_raw(vec![MockServer::raw_ok("", SRT.as_bytes())]).await;
        let secret_port = secret.base_url.rsplit(':').next().unwrap().to_owned();
        let hop = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{secret_port}/secret\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let source = MockServer::start_raw(vec![hop.into_bytes()]).await;
        let source_port = source.base_url.rsplit(':').next().unwrap().to_owned();
        let mock = ApiMock::start(move |_| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(20, "2100-01-01T00:00:00Z")]),
                (
                    "/download",
                    vec![(
                        200,
                        format!(
                            r#"{{"link":"http://localhost:{source_port}/file/1.srt","remaining":19}}"#
                        ),
                    )],
                ),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");
        let provider = provider.with_link_destinations(PrivateAllowList::parse("localhost").0);

        let error = provider
            .get_subtitles("srt-en-1")
            .await
            .expect_err("the hop is refused");

        assert!(
            error.to_string().contains("subtitle_private_http_allow"),
            "{error}"
        );
        assert_eq!(source.requests(), 1, "the listed link was fetched");
        assert_eq!(secret.requests(), 0, "the hop was never sent");
    }

    /// A link that answers an error status fails the download without naming
    /// the link: it is a one-time capability URL, and the error reaches the
    /// server log (gl-178).
    #[tokio::test]
    async fn a_failed_link_fetch_keeps_the_link_out_of_the_error() {
        use crate::mock_http::MockServer;
        let source = MockServer::start_raw(vec![MockServer::raw_status("404 Not Found")]).await;
        let source_port = source.base_url.rsplit(':').next().unwrap().to_owned();
        let mock = ApiMock::start(move |_| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(20, "2100-01-01T00:00:00Z")]),
                (
                    "/download",
                    vec![(
                        200,
                        format!(
                            r#"{{"link":"http://localhost:{source_port}/file/one-time-token.srt","remaining":19}}"#
                        ),
                    )],
                ),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");
        let provider = provider.with_link_destinations(PrivateAllowList::parse("localhost").0);

        let error = provider
            .get_subtitles("srt-en-1")
            .await
            .expect_err("a 404 link fails the download");

        let text = format!("{error} {error:?}");
        assert!(text.contains("404"), "{text}");
        assert!(!text.contains("one-time-token"), "{text}");
        assert_eq!(source.requests(), 1, "the link was fetched");
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
    fn candidate_language_is_the_requested_code_not_the_api_tag() {
        // The API tags pt-BR/zh-CN carry a '-'; the plugin stamps request.Language instead.
        let json = r#"{"data": [{"attributes": {"language": "pt-BR",
            "feature_details": {"feature_type": "Movie"}, "files": [{"file_id": 5}]}}]}"#;
        let parsed: SearchResponse = serde_json::from_str(json).unwrap();
        let request = SubtitleSearchRequest {
            language: "por".to_owned(),
            ..Default::default()
        };
        let c = &map_search(parsed.data, &request, 0, &HashSet::new())[0];
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

    use serde_json::json;

    /// The API's `/infos/languages` reply, listing `codes`.
    fn languages_reply(codes: &[&str]) -> (u16, String) {
        let data: Vec<_> = codes
            .iter()
            .map(|code| json!({ "language_code": code }))
            .collect();
        (200, json!({ "data": data }).to_string())
    }

    /// One `/subtitles` page of `total_pages`, holding `items` (each one
    /// result's attributes).
    fn search_page(page: i32, total_pages: i32, items: &[serde_json::Value]) -> (u16, String) {
        let data: Vec<_> = items
            .iter()
            .map(|attributes| json!({ "attributes": attributes }))
            .collect();
        let body = json!({ "total_pages": total_pages, "page": page, "data": data });
        (200, body.to_string())
    }

    /// A `/subtitles` reply with no results.
    fn no_results() -> Vec<(u16, String)> {
        vec![(200, r#"{"data":[]}"#.to_owned())]
    }

    /// One search result's attributes: `attributes` with one file, `file_id`.
    fn result(file_id: i64, mut attributes: serde_json::Value) -> serde_json::Value {
        attributes["files"] = json!([{ "file_id": file_id }]);
        attributes
    }

    /// A result for a movie, with no IMDb id.
    fn movie_result(file_id: i64) -> serde_json::Value {
        result(
            file_id,
            json!({ "feature_details": { "feature_type": "Movie" } }),
        )
    }

    /// An API whose account logs in with `remaining` downloads left, which
    /// lists `languages` and answers searches with `pages`, in order.
    async fn search_api(remaining: i32, languages: &[&str], pages: Vec<(u16, String)>) -> ApiMock {
        let languages = languages_reply(languages);
        ApiMock::start(move |_| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                (
                    "/infos/user",
                    vec![user_reply(remaining, "2100-01-01T00:00:00Z")],
                ),
                ("/infos/languages", vec![languages]),
                ("/subtitles", pages),
            ]
        })
        .await
    }

    /// A manual movie search in `two_letter`'s language (`eng` as the
    /// three-letter code) for the file at `path`.
    fn movie_request(two_letter: &str, path: &str) -> SubtitleSearchRequest {
        SubtitleSearchRequest {
            language: "eng".to_owned(),
            two_letter_iso_language_name: Some(two_letter.to_owned()),
            media_path: Some(path.to_owned()),
            ..Default::default()
        }
    }

    /// An episode search, S02E05 of "Show", for the file at `path`.
    fn episode_request(path: &str) -> SubtitleSearchRequest {
        SubtitleSearchRequest {
            content_type: SubtitleMediaType::Episode,
            series_name: Some("Show".to_owned()),
            parent_index_number: Some(2),
            index_number: Some(5),
            ..movie_request("en", path)
        }
    }

    /// The `/subtitles` request lines `mock` saw.
    fn searches(mock: &ApiMock) -> Vec<String> {
        mock.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with("GET /subtitles"))
            .cloned()
            .collect()
    }

    /// The ids of `found`, in order.
    fn ids(found: &[RemoteSubtitleInfo]) -> Vec<&str> {
        found.iter().filter_map(|info| info.id.as_deref()).collect()
    }

    /// The plugin's `RequestHandlerTests.ComputeHash_Success` (which tests
    /// `AddQueryString`), plus `HttpUtility.UrlEncode`'s lowercase escapes of
    /// UTF-8 and of `/`, and the characters it keeps.
    #[rstest::rstest]
    #[case("/subtitles", &[("b", "c and d"), ("a", "1")], "/subtitles?a=1&b=c+and+d")]
    #[case("/subtitles", &[], "/subtitles")]
    #[case(
        "/subtitles",
        &[("Query", "Amélie/A_B-C.D!*(2001).MKV")],
        "/subtitles?query=am%c3%a9lie%2fa_b-c.d!*(2001).mkv"
    )]
    fn query_strings_are_built_as_the_plugin_builds_them(
        #[case] path: &str,
        #[case] params: &[(&str, &str)],
        #[case] expected: &str,
    ) {
        let params: Vec<_> = params
            .iter()
            .map(|&(name, value)| (name, value.to_owned()))
            .collect();
        assert_eq!(add_query_string(path, &params), expected);
    }

    /// gl-1462: `OpenSubtitleDownloader.GetLanguage` searches the request's
    /// two-letter code, with the regioned tags the API lists for bare `zh`
    /// and `pt`; `AddQueryString` lowercases it.
    #[rstest::rstest]
    #[case("et", "et")]
    #[case("cs", "cs")]
    #[case("sk", "sk")]
    #[case("bs", "bs")]
    #[case("en", "en")]
    #[case("zh", "zh-cn")]
    #[case("pt", "pt-pt")]
    #[tokio::test]
    async fn search_sends_the_two_letter_language(#[case] two_letter: &str, #[case] sent: &str) {
        let languages = ["et", "cs", "sk", "bs", "en", "zh-CN", "pt-PT"];
        let mock = search_api(10, &languages, no_results()).await;
        let (provider, _) = account_provider(&mock, "user");
        provider
            .search(&movie_request(two_letter, "/films/movie.strm"))
            .await
            .expect("search");
        assert_eq!(
            searches(&mock),
            [format!(
                "GET /subtitles?languages={sent}&query=movie.strm&type=movie"
            )]
        );
    }

    /// `GetLanguage` takes a code as the API's language list spells it, a
    /// regioned code falling back to its language. A code the list lacks,
    /// or a language with no two-letter code, is not searched. The list is
    /// read once; a refused read fails the search and is tried again next
    /// time.
    #[tokio::test]
    async fn search_checks_the_language_against_the_apis_list() {
        let mock = ApiMock::start(|_| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(10, "2100-01-01T00:00:00Z")]),
                (
                    "/infos/languages",
                    vec![
                        (403, "{}".to_owned()),
                        languages_reply(&["en", "pt", "zh-CN"]),
                    ],
                ),
                ("/subtitles", no_results()),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");
        provider
            .search(&movie_request("en", "/films/a.strm"))
            .await
            .expect_err("no language list");
        for two_letter in ["en", "pt", "zh", "et", ""] {
            let found = provider
                .search(&movie_request(two_letter, "/films/a.strm"))
                .await
                .expect("search");
            assert_eq!(found, [], "{two_letter}");
        }
        let unknown = SubtitleSearchRequest {
            two_letter_iso_language_name: None,
            ..movie_request("", "/films/a.strm")
        };
        assert_eq!(provider.search(&unknown).await.expect("search"), []);
        assert_eq!(
            searches(&mock),
            [
                "GET /subtitles?languages=en&query=a.strm&type=movie",
                "GET /subtitles?languages=pt&query=a.strm&type=movie",
                "GET /subtitles?languages=zh-cn&query=a.strm&type=movie",
            ]
        );
        assert_eq!(mock.count("GET /infos/languages"), 2);
    }

    /// `Search`'s parameters: the content `type`, then the IMDb id when the
    /// item has one, or else the media file's name and an episode's
    /// numbers, sorted by name and lowercased as `AddQueryString` builds
    /// them.
    #[rstest::rstest]
    #[case::movie_by_imdb_id(
        false,
        Some("tt0133093"),
        "GET /subtitles?imdb_id=133093&languages=en&type=movie"
    )]
    #[case::movie_by_file_name(
        false,
        None,
        "GET /subtitles?languages=en&query=the+matrix+(1999).strm&type=movie"
    )]
    #[case::episode_by_imdb_id(
        true,
        Some("tt0000042"),
        "GET /subtitles?imdb_id=42&languages=en&type=episode"
    )]
    #[case::episode_by_file_name(
        true,
        Some("not-a-number"),
        "GET /subtitles?episode_number=5&languages=en&query=the+matrix+(1999).strm&season_number=2&type=episode"
    )]
    #[tokio::test]
    async fn search_asks_for_the_imdb_id_or_the_file_name(
        #[case] episode: bool,
        #[case] imdb: Option<&str>,
        #[case] sent: &str,
    ) {
        let mock = search_api(10, &["en"], no_results()).await;
        let (provider, _) = account_provider(&mock, "user");
        let path = "/media/Films/The Matrix (1999).strm";
        let request = SubtitleSearchRequest {
            imdb_id: imdb.map(str::to_owned),
            ..if episode {
                episode_request(path)
            } else {
                movie_request("en", path)
            }
        };
        provider.search(&request).await.expect("search");
        assert_eq!(searches(&mock), [sent]);
    }

    /// A hashed file sends `moviehash`, and a perfect-match search adds
    /// `moviehash_match=only` and keeps only the hash matches
    /// (`MatchFilter`).
    #[tokio::test]
    async fn a_perfect_match_search_sends_the_hash_and_keeps_its_matches() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Movie.mkv");
        let mut bytes = vec![0u8; 131_072];
        bytes[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        bytes[131_064..].copy_from_slice(&2u64.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let page = search_page(
            1,
            1,
            &[
                movie_result(1),
                result(
                    2,
                    json!({ "feature_details": { "feature_type": "Movie" }, "moviehash_match": true }),
                ),
            ],
        );
        let mock = search_api(10, &["en"], vec![page]).await;
        let (provider, _) = account_provider(&mock, "user");
        let request = SubtitleSearchRequest {
            is_perfect_match: Some(true),
            ..movie_request("en", path.to_str().unwrap())
        };

        let found = provider.search(&request).await.expect("search");

        assert_eq!(ids(&found), ["srt-eng-2"]);
        assert_eq!(
            searches(&mock),
            [
                "GET /subtitles?languages=en&moviehash=0000000000020001&moviehash_match=only&query=movie.mkv&type=movie"
            ]
        );
    }

    /// `Search` reads every page and keeps a result only for a file of the
    /// item's own feature (`BadSubtitleFilter`, `MediaFilter`). It orders
    /// them by hash match, download count, rating and trusted uploader, and
    /// maps each result once, from its first file.
    #[tokio::test]
    async fn search_filters_orders_and_maps_every_page() {
        let movie = |imdb: i64| json!({ "feature_type": "Movie", "imdb_id": imdb });
        let pages = vec![
            search_page(
                1,
                2,
                &[
                    result(
                        1,
                        json!({ "feature_details": movie(133_093), "download_count": 5 }),
                    ),
                    result(
                        2,
                        json!({ "feature_details": { "feature_type": "Episode", "imdb_id": 133_093 } }),
                    ),
                    result(3, json!({ "feature_details": movie(42) })),
                    json!({ "feature_details": movie(133_093), "files": [] }),
                    json!({ "feature_details": movie(133_093), "files": [{ "file_name": "a.srt" }] }),
                    result(4, json!({ "download_count": 9 })),
                ],
            ),
            search_page(
                2,
                2,
                &[
                    result(
                        5,
                        json!({ "feature_details": movie(133_093), "moviehash_match": true, "download_count": 1 }),
                    ),
                    result(
                        6,
                        json!({ "feature_details": movie(133_093), "download_count": 5, "ratings": 8.0 }),
                    ),
                    json!({
                        "feature_details": movie(133_093),
                        "download_count": 5,
                        "ratings": 8.0,
                        "from_trusted": true,
                        "release": "The.Matrix.1999.1080p",
                        "comments": "synced",
                        "uploader": { "name": "alice" },
                        "upload_date": "2024-05-01T10:20:30Z",
                        "fps": 23.976,
                        "hearing_impaired": true,
                        "foreign_parts_only": false,
                        "machine_translated": false,
                        "ai_translated": true,
                        "files": [
                            { "file_id": 7, "file_name": "a.srt" },
                            { "file_id": 8, "file_name": "b.srt" }
                        ]
                    }),
                ],
            ),
        ];
        let mock = search_api(10, &["en"], pages).await;
        let (provider, _) = account_provider(&mock, "user");
        let request = SubtitleSearchRequest {
            imdb_id: Some("tt0133093".to_owned()),
            ..movie_request("en", "/films/movie.strm")
        };

        let found = provider.search(&request).await.expect("search");

        assert_eq!(
            ids(&found),
            ["srt-eng-5", "srt-eng-7-sdh", "srt-eng-6", "srt-eng-1"]
        );
        assert_eq!(
            found[1],
            RemoteSubtitleInfo {
                three_letter_iso_language_name: Some("eng".to_owned()),
                id: Some("srt-eng-7-sdh".to_owned()),
                provider_name: Some(PLUGIN_NAME.to_owned()),
                name: Some("The.Matrix.1999.1080p".to_owned()),
                format: Some("srt".to_owned()),
                author: Some("alice".to_owned()),
                comment: Some("synced".to_owned()),
                date_created: Some("2024-05-01T10:20:30Z".parse().unwrap()),
                community_rating: Some(8.0),
                frame_rate: Some(23.976),
                download_count: Some(5),
                is_hash_match: None,
                ai_translated: Some(true),
                machine_translated: Some(false),
                forced: Some(false),
                hearing_impaired: Some(true),
            }
        );
        // The plugin's non-nullable counts read an absent one as zero.
        assert_eq!(found[3].community_rating, Some(0.0));
        assert_eq!(found[3].frame_rate, Some(0.0));
        assert_eq!(
            searches(&mock),
            [
                "GET /subtitles?imdb_id=133093&languages=en&type=movie",
                "GET /subtitles?imdb_id=133093&languages=en&page=2&type=movie",
            ]
        );
    }

    /// A page that does not move on ends the search, where upstream would ask
    /// for it again forever. A page after the first that the API refuses fails
    /// the search, dropping the pages before it, as the plugin's failed
    /// `SearchSubtitlesAsync` does.
    #[tokio::test]
    async fn search_stops_at_a_page_that_does_not_move_on_or_fails() {
        let stuck = search_api(10, &["en"], vec![search_page(1, 3, &[movie_result(1)])]).await;
        let (provider, _) = account_provider(&stuck, "user");
        let found = provider
            .search(&movie_request("en", "/films/a.strm"))
            .await
            .expect("search");
        assert_eq!(ids(&found), ["srt-eng-1"]);
        assert_eq!(searches(&stuck).len(), 2);

        let pages = vec![
            search_page(1, 2, &[movie_result(1)]),
            (403, "{}".to_owned()),
        ];
        let failing = search_api(10, &["en"], pages).await;
        let (provider, _) = account_provider(&failing, "user");
        provider
            .search(&movie_request("en", "/films/a.strm"))
            .await
            .expect_err("a refused page");
        assert_eq!(searches(&failing).len(), 2);
    }

    /// An episode search keeps the results for its own season and episode.
    #[tokio::test]
    async fn an_episode_search_keeps_its_own_episode() {
        let episode = |season: i32, number: i32| json!({ "feature_type": "Episode", "season_number": season, "episode_number": number });
        let page = search_page(
            1,
            1,
            &[
                result(1, json!({ "feature_details": episode(2, 5) })),
                result(2, json!({ "feature_details": episode(2, 6) })),
                result(3, json!({ "feature_details": episode(1, 5) })),
                result(
                    4,
                    json!({ "feature_details": { "feature_type": "Movie", "season_number": 2, "episode_number": 5 } }),
                ),
            ],
        );
        let mock = search_api(10, &["en"], vec![page]).await;
        let (provider, _) = account_provider(&mock, "user");

        let found = provider
            .search(&episode_request("/tv/Show/e.strm"))
            .await
            .expect("search");

        assert_eq!(ids(&found), ["srt-eng-1"]);
    }

    /// `Search`'s early returns: an episode without its season, its number
    /// or its series name, or a video without a path, is not searched.
    #[tokio::test]
    async fn search_skips_an_episode_without_its_numbers_or_a_video_without_a_path() {
        let mock = search_api(10, &["en"], no_results()).await;
        let (provider, _) = account_provider(&mock, "user");
        let episode = episode_request("/tv/Show/e.strm");
        let skipped = [
            SubtitleSearchRequest {
                parent_index_number: None,
                ..episode.clone()
            },
            SubtitleSearchRequest {
                index_number: None,
                ..episode.clone()
            },
            SubtitleSearchRequest {
                series_name: Some(String::new()),
                ..episode.clone()
            },
            movie_request("en", ""),
            SubtitleSearchRequest {
                media_path: None,
                ..movie_request("en", "")
            },
        ];
        for request in &skipped {
            assert_eq!(provider.search(request).await.expect("search"), []);
        }
        assert_eq!(mock.count("GET /infos/languages"), 0);
        assert_eq!(mock.count("GET /subtitles"), 0);

        provider.search(&episode).await.expect("search");
        assert_eq!(
            mock.count("GET /subtitles"),
            1,
            "the whole episode is searched"
        );
    }

    /// `Search` logs the account in first, once for every search while the
    /// token lasts. An automated search finds nothing while the account's
    /// quota is spent, and nothing without an account, where upstream's
    /// `Login` throws; a manual search with an API key and no account
    /// still searches (gl-808).
    #[tokio::test]
    async fn automated_searches_stop_at_a_spent_quota_or_without_an_account() {
        let automated = SubtitleSearchRequest {
            is_automated: true,
            ..movie_request("en", "/films/a.strm")
        };
        let manual = movie_request("en", "/films/a.strm");
        let page = || vec![search_page(1, 1, &[movie_result(1)])];

        let spent = search_api(0, &["en"], page()).await;
        let (provider, _) = account_provider(&spent, "user");
        assert_eq!(provider.search(&automated).await.expect("search"), []);
        assert_eq!(spent.count("GET /subtitles"), 0);
        let logged = provider.state.lock().await.limit_logged;
        assert!(logged.is_some(), "the spent quota is logged");
        assert_eq!(provider.search(&automated).await.expect("search"), []);
        assert_eq!(
            provider.state.lock().await.limit_logged,
            logged,
            "at most once a minute"
        );
        let found = provider.search(&manual).await.expect("search");
        assert_eq!(ids(&found), ["srt-eng-1"], "a manual search still searches");
        assert_eq!(spent.count("POST /login"), 1);
        assert_eq!(spent.count("GET /infos/user"), 1);

        let keyed = search_api(10, &["en"], page()).await;
        let plugins = crate::plugin_config::tests_support::manager_with(
            PLUGIN_ID,
            br#"{"ApiKey":"key"}"#.to_vec(),
        );
        let provider = OpenSubtitlesProvider::new(plugins).with_base_url(&keyed.base_url);
        assert_eq!(provider.search(&automated).await.expect("search"), []);
        assert_eq!(keyed.count(""), 0);
        let found = provider.search(&manual).await.expect("search");
        assert_eq!(ids(&found), ["srt-eng-1"]);
        assert_eq!(keyed.count("POST /login"), 0);
    }

    /// `Login`: a login the API refuses fails the search. A refusal of the
    /// account itself (`CredentialsInvalid`: a 401, or a 400 for an e-mail
    /// username) stops the logins with that account: a manual search then goes
    /// on without one and an automated search finds nothing, until the
    /// settings page validates the account again (or the account changes).
    /// Any other refusal is tried again by the next search.
    #[rstest::rstest]
    #[case::unauthorized(401, "user", true)]
    #[case::email_username(400, "user@example.com", true)]
    #[case::bad_request(400, "user", false)]
    #[case::forbidden(403, "user", false)]
    #[tokio::test]
    async fn a_refused_login_fails_the_search(
        #[case] status: u16,
        #[case] username: &str,
        #[case] refuses_account: bool,
    ) {
        let mock = ApiMock::start(move |_| {
            vec![
                (
                    "/login",
                    vec![(status, "{}".to_owned()), login_reply(FUTURE_PAYLOAD)],
                ),
                ("/infos/languages", vec![languages_reply(&["en"])]),
                ("/subtitles", vec![search_page(1, 1, &[movie_result(1)])]),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, username);
        let manual = movie_request("en", "/films/a.strm");

        let error = provider.search(&manual).await.expect_err("refused login");
        assert!(matches!(error, ServiceError::Unauthorized(_)), "{error:?}");
        assert_eq!(mock.count("GET /subtitles"), 0);

        let found = provider.search(&manual).await.expect("search");
        assert_eq!(ids(&found), ["srt-eng-1"]);
        if !refuses_account {
            assert_eq!(mock.count("POST /login"), 2, "logged in again");
            return;
        }
        let automated = SubtitleSearchRequest {
            is_automated: true,
            ..manual.clone()
        };
        assert_eq!(provider.search(&automated).await.expect("search"), []);
        assert_eq!(
            mock.count("POST /login"),
            1,
            "no login with a refused account"
        );

        let config = format!(r#"{{"Username":"{username}","Password":"secret"}}"#);
        provider
            .validate_login(config.as_bytes())
            .await
            .expect("validated");
        provider.search(&automated).await.expect("search");
        assert_eq!(mock.count("POST /login"), 3, "validation and the search");
        assert_eq!(mock.count("GET /subtitles"), 2);
    }

    /// `GetSubtitlesInternal`: a link that answers with an empty file fails
    /// the download, and from then on automated searches skip that file
    /// (`BadSubtitleFilter`); a manual search still lists it.
    #[tokio::test]
    async fn an_empty_download_is_skipped_by_automated_searches() {
        let mock = ApiMock::start(|base| {
            vec![
                ("/login", vec![login_reply(FUTURE_PAYLOAD)]),
                ("/infos/user", vec![user_reply(10, "2100-01-01T00:00:00Z")]),
                ("/infos/languages", vec![languages_reply(&["en"])]),
                ("/download", vec![download_reply(base, 9)]),
                ("/file/", vec![(200, " \r\n".to_owned())]),
                (
                    "/subtitles",
                    vec![search_page(1, 1, &[movie_result(1), movie_result(2)])],
                ),
            ]
        })
        .await;
        let (provider, _) = account_provider(&mock, "user");

        let error = provider
            .get_subtitles("srt-eng-1")
            .await
            .expect_err("empty file");

        assert!(
            error
                .to_string()
                .contains("Subtitle with Id 1 could not be downloaded: OK - this is most likely a broken subtitle"),
            "{error}"
        );
        let manual = movie_request("en", "/films/a.strm");
        let found = provider.search(&manual).await.expect("search");
        assert_eq!(ids(&found), ["srt-eng-1", "srt-eng-2"]);
        let automated = SubtitleSearchRequest {
            is_automated: true,
            ..manual
        };
        let found = provider.search(&automated).await.expect("search");
        assert_eq!(ids(&found), ["srt-eng-2"]);
    }
}
