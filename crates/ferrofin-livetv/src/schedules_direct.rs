//! The Schedules Direct (SD) listings provider.
//!
//! Port of `Jellyfin.LiveTv/Listings/SchedulesDirect.cs` at v12.2, the second
//! listings backend beside XMLTV:
//!
//! - **Account:** [`SchedulesDirect::validate`] (the save-time check
//!   `ListingsManager.SaveListingProvider` runs), token login with the
//!   password's lowercase SHA1 hex, the account's lineups and adding one to it.
//! - **Lineups and channels:** [`SchedulesDirect::get_lineups`] (a postal
//!   code's headends) and [`SchedulesDirect::get_channels`] (a lineup's
//!   station map).
//! - **Guide:** [`SchedulesDirect::get_programs`] (schedules, program
//!   details, artwork in 500-id batches).
//! - **Errors:** the per-code table of `Request<T>` ([`SdErrorCode`]):
//!   - permanent account errors disable SD until the provider is saved again;
//!   - transient login errors back off for [`LOGIN_BACKOFF_MINUTES`];
//!   - the image and metadata daily limits are persisted under the cache dir
//!     and end at 00:00 UTC;
//!   - an expired token (or a bare `403`) is refreshed once.
//! - **Countries:** [`SchedulesDirect::get_available_countries`] backs
//!   `GET /LiveTv/ListingProviders/SchedulesDirect/Countries`, which the
//!   dashboard's SD page calls before any credentials exist. The document is
//!   passed through byte-for-byte behind upstream's two-level cache: memory,
//!   then `{cache}/sd-countries.json` while it is younger than
//!   [`COUNTRY_CACHE_DAYS`], then a fresh fetch that rewrites both.
//!
//! Credentials go only to [`API_URL`]. The password, its hash and the token
//! are never logged (upstream logs the token at info; that is not copied).

mod dto;
mod guide;

#[cfg(test)]
mod client_tests;

pub use dto::SdErrorCode;
pub use guide::SdProgram;

use ferrofin_util::directory_path::DirectoryPath;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, NaiveDate, Utc};
use serde::de::DeserializeOwned;

use ferrofin_model::dto::NameIdPair;
use ferrofin_model::live_tv::ListingsProviderInfo;
use ferrofin_traits::error::ServiceError;

use crate::error::LiveTvError;
use crate::fetch::{RequestMethod, SourceFetcher, SourceRequest};
use crate::mapping::eq_ordinal_ignore_case;

/// The Schedules Direct JSON API root (`SchedulesDirect.ApiUrl`,
/// `Jellyfin.LiveTv/Listings/SchedulesDirect.cs`).
pub const API_URL: &str = "https://json.schedulesdirect.org/20141201";

/// The provider's `Type` (`nameof(SchedulesDirect)`), matched ignoring case.
pub const PROVIDER_TYPE: &str = "SchedulesDirect";

/// The provider's display `Name` (`SchedulesDirect.Name`).
pub const PROVIDER_NAME: &str = "Schedules Direct";

/// How long a transient login error (SD codes 3000, 3001, 4004, or an
/// unexplained `4xx` from `/token`) keeps SD from being asked again.
///
/// Upstream hard-codes 30 minutes (`GetToken`, `Request<T>`). It is not part
/// of SD's API contract, so it is a candidate setting for the owner; it stays
/// upstream's value until decided.
pub const LOGIN_BACKOFF_MINUTES: i64 = 30;

/// How long a token is reused before the next login.
///
/// Upstream hard-codes 20 hours (`GetToken`) against SD tokens that last 24.
/// Like [`LOGIN_BACKOFF_MINUTES`], a candidate setting that keeps upstream's
/// value until the owner decides.
pub const TOKEN_REUSE_HOURS: i64 = 20;

/// The image daily-limit marker under the cache dir (`sd-image-limit.txt`).
const IMAGE_LIMIT_FILE: &str = "sd-image-limit.txt";

/// The metadata daily-limit marker under the cache dir (`sd-metadata-limit.txt`).
const METADATA_LIMIT_FILE: &str = "sd-metadata-limit.txt";

/// How long the on-disk country list stays valid before it is re-fetched.
///
/// Jellyfin's own constant (`SchedulesDirect.CountryCacheDays = 7`,
/// `Jellyfin.LiveTv/Listings/SchedulesDirect.cs`): the country set changes
/// rarely, so a weekly refresh keeps SD traffic negligible while never serving
/// a stale list for long.
pub const COUNTRY_CACHE_DAYS: u64 = 7;

/// The cache file name under the cache directory (`sd-countries.json` upstream).
const COUNTRIES_CACHE_FILE: &str = "sd-countries.json";

/// The Schedules Direct client.
///
/// Cloning shares the in-memory country cache, the token cache and the error
/// state, so every clone (the manager is cloned into each request's state)
/// behaves as upstream's one DI singleton does.
#[derive(Clone)]
pub struct SchedulesDirect {
    fetcher: Arc<dyn SourceFetcher>,
    cache_dir: DirectoryPath,
    /// `SchedulesDirect._countriesCache`: the bytes last read or fetched.
    countries: Arc<RwLock<Option<Vec<u8>>>>,
    state: Arc<SdState>,
}

/// The state upstream keeps in the singleton's fields.
#[derive(Default)]
struct SdState {
    account: Mutex<AccountState>,
    /// `_tokenLock`: one login at a time.
    token_lock: tokio::sync::Mutex<()>,
    /// `_imageLimitHitDate`/`_metadataLimitHitDate`, read from their files the
    /// first time they are needed (upstream reads them in its constructor).
    limits: Mutex<DailyLimits>,
}

/// The account half of [`SdState`].
#[derive(Default)]
struct AccountState {
    /// `_tokens`: username (ordinal) → the token and when it was issued.
    tokens: HashMap<String, (String, DateTime<Utc>)>,
    /// `_lastErrorResponseTicks`: when the last transient login error came.
    last_error: Option<DateTime<Utc>>,
    /// `_accountError`: a permanent account error disables SD until the
    /// provider is saved again (or the server restarts).
    account_error: bool,
}

/// The two daily limits, as the UTC day each was hit.
#[derive(Default)]
struct DailyLimits {
    /// Whether the marker files have been read yet.
    loaded: bool,
    image: Option<NaiveDate>,
    metadata: Option<NaiveDate>,
}

/// One station of a lineup: the `ChannelInfo` fields `GetChannels` fills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdChannel {
    /// `Id`: the station id.
    pub id: Option<String>,
    /// `CallSign`.
    pub callsign: Option<String>,
    /// `Number`, from the lineup map (`GetChannelNumber`).
    pub number: String,
    /// `Name`: the station's name, or the number when it has none.
    pub name: String,
    /// `ImageUrl`: the station logo.
    pub image_url: Option<String>,
}

/// How a Schedules Direct call failed, as the exception upstream throws.
#[derive(Debug)]
enum SdError {
    /// `HttpRequestException` carrying the response's status (`Request<T>`).
    Status { status: u16, url: String },
    /// A transport failure or an unreadable response.
    Backend(ServiceError),
    /// `AuthenticationException` (HTTP `401`).
    Auth(String),
    /// `ArgumentException` (HTTP `400`).
    Argument(String),
}

impl From<SdError> for ServiceError {
    fn from(error: SdError) -> Self {
        match error {
            SdError::Status { status, url } => {
                ServiceError::backend(format!("Schedules Direct {url}: HTTP {status}"))
            }
            SdError::Backend(error) => error,
            SdError::Auth(message) => ServiceError::Unauthorized(message),
            SdError::Argument(message) => ServiceError::InvalidInput(message),
        }
    }
}

/// `ArgumentException.ThrowIfNullOrEmpty`.
fn require(value: Option<&str>, name: &str) -> Result<(), SdError> {
    if value.is_some_and(|v| !v.is_empty()) {
        Ok(())
    } else {
        Err(SdError::Argument(format!("{name} cannot be null or empty")))
    }
}

/// `Encoding.ASCII.GetBytes`: each non-ASCII character becomes one `?`. The
/// replacement fallback takes a surrogate pair as one character, so an emoji
/// is one `?`, not two.
fn ascii_bytes(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| u8::try_from(c).ok().filter(u8::is_ascii).unwrap_or(b'?'))
        .collect()
}

/// The SD error code a failed response's body names, when the table defines it.
fn sd_error_code(body: &[u8]) -> Option<SdErrorCode> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("code")?
        .as_i64()
        .and_then(SdErrorCode::from_code)
}

impl std::fmt::Debug for SchedulesDirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulesDirect")
            .field("cache_dir", &self.cache_dir)
            .finish_non_exhaustive()
    }
}

impl SchedulesDirect {
    /// Creates the client over `fetcher`, caching on disk under `cache_dir`
    /// (the application cache path — `IApplicationPaths.CachePath` upstream).
    #[must_use]
    pub fn new(fetcher: Arc<dyn SourceFetcher>, cache_dir: impl Into<DirectoryPath>) -> Self {
        Self {
            fetcher,
            cache_dir: cache_dir.into(),
            countries: Arc::new(RwLock::new(None)),
            state: Arc::new(SdState::default()),
        }
    }

    /// The on-disk cache file for the country list.
    #[must_use]
    pub fn countries_cache_path(&self) -> PathBuf {
        self.cache_dir.join(COUNTRIES_CACHE_FILE)
    }

    /// The SD "available countries" document as raw JSON bytes.
    ///
    /// Port of `SchedulesDirect.GetAvailableCountries`: memory → disk (within
    /// [`COUNTRY_CACHE_DAYS`]; an unreadable file is deleted and treated as
    /// absent) → `GET {API_URL}/available/countries`, with a successful fetch
    /// written to disk and kept in memory.
    ///
    /// # Errors
    ///
    /// A transport/status failure on the fetch, or a failure creating the cache
    /// directory or writing the cache file, is a backend error (HTTP `500`) —
    /// as the corresponding exceptions are upstream.
    pub async fn get_available_countries(&self) -> Result<Vec<u8>, ServiceError> {
        if let Some(cached) = self.memory_cached() {
            return Ok(cached);
        }

        let cache_dir = self.cache_dir.resolve();
        let cache_path = cache_dir.join(COUNTRIES_CACHE_FILE);
        if cache_is_fresh(
            &cache_path,
            Duration::from_secs(COUNTRY_CACHE_DAYS * 24 * 60 * 60),
        )
        .await
        {
            match tokio::fs::read(&cache_path).await {
                Ok(bytes) => {
                    self.remember(bytes.clone());
                    return Ok(bytes);
                }
                Err(err) => {
                    // Corrupt or unreadable — delete and re-fetch.
                    tracing::debug!(
                        path = %cache_path.display(),
                        error = %err,
                        "unreadable Schedules Direct country cache; refetching"
                    );
                    let _ = tokio::fs::remove_file(&cache_path).await;
                }
            }
        }

        let url = format!("{API_URL}/available/countries");
        let bytes = self.fetcher.fetch_bytes(&url).await?;

        tokio::fs::create_dir_all(&cache_dir)
            .await
            .map_err(|e| LiveTvError::io(format!("create {}", cache_path.display()), e))?;
        tokio::fs::write(&cache_path, &bytes)
            .await
            .map_err(|e| LiveTvError::io(format!("write {}", cache_path.display()), e))?;

        self.remember(bytes.clone());
        Ok(bytes)
    }

    /// The in-memory copy, when one has been read or fetched this process.
    fn memory_cached(&self) -> Option<Vec<u8>> {
        self.countries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stores `bytes` as the in-memory copy.
    fn remember(&self, bytes: Vec<u8>) {
        *self
            .countries
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(bytes);
    }
}

/// A request to the SD API, rebuilt for the token-refresh retry.
#[derive(Clone)]
struct SdRequest {
    method: RequestMethod,
    url: String,
    token: Option<String>,
    json: Option<Vec<u8>>,
}

impl std::fmt::Debug for SdRequest {
    /// The token and the body (the `/token` body carries the password hash)
    /// are never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SdRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("json_bytes", &self.json.as_ref().map(Vec::len))
            .finish()
    }
}

impl SdRequest {
    fn new(method: RequestMethod, path: &str, token: Option<&str>) -> Self {
        Self {
            method,
            url: format!("{API_URL}{path}"),
            token: token.map(str::to_owned),
            json: None,
        }
    }

    fn json(mut self, body: Vec<u8>) -> Self {
        self.json = Some(body);
        self
    }

    fn to_source(&self) -> SourceRequest {
        let mut request = SourceRequest::new(self.method, self.url.clone());
        if let Some(token) = &self.token {
            request = request.header("token", token);
        }
        if let Some(json) = &self.json {
            request = request.json(json.clone());
        }
        request
    }
}

/// Serializes an SD request body.
fn to_json(body: &impl serde::Serialize) -> Result<Vec<u8>, SdError> {
    serde_json::to_vec(body).map_err(|e| {
        SdError::Backend(LiveTvError::serialize("serialize a Schedules Direct request", e).into())
    })
}

impl SchedulesDirect {
    /// Port of `SchedulesDirect.Validate`, which `ListingsManager.SaveListingProvider`
    /// runs before it stores the provider.
    ///
    /// Saving is the user's explicit retry, so the error state is reset first
    /// (`ResetErrorState`): the account error, the login back-off and this
    /// user's cached token. Then `validate_login` logs in, and
    /// `validate_listings` adds the chosen lineup to the account when it is not
    /// already on it.
    ///
    /// # Errors
    ///
    /// A missing username, password or listings id is [`ServiceError::InvalidInput`]
    /// (`ArgumentException`, `400`); a login that yields no token is
    /// [`ServiceError::Unauthorized`] (`AuthenticationException`, `401`); a
    /// failed SD request is a backend error (`500`).
    pub async fn validate(
        &self,
        info: &ListingsProviderInfo,
        validate_login: bool,
        validate_listings: bool,
    ) -> Result<(), ServiceError> {
        self.reset_error_state(info);

        if validate_login {
            require(info.username.as_deref(), "Username")?;
            require(
                info.password
                    .as_ref()
                    .map(ferrofin_model::secret::Secret::expose),
                "Password",
            )?;
            if self
                .get_token(info)
                .await?
                .is_none_or(|token| token.trim().is_empty())
            {
                return Err(SdError::Auth(
                    "Could not authenticate with Schedules Direct".to_owned(),
                )
                .into());
            }
        }

        if validate_listings {
            require(info.listings_id.as_deref(), "ListingsId")?;
            if !self.has_lineup(info).await? {
                self.add_lineup_to_account(info).await?;
            }
        }
        Ok(())
    }

    /// The lineups SD offers for a postal code: port of `GetLineups` →
    /// `GetHeadends`.
    ///
    /// Each headend's lineups become `NameIdPair`s named by the lineup's name
    /// (or its id when it has none) and keyed by its `lineup` id, falling back
    /// to the last segment of its `uri` (jellyfin/jellyfin#17935).
    ///
    /// # Errors
    ///
    /// A login that yields no token is [`ServiceError::Unauthorized`], since an
    /// empty list would read as "no lineups here" (upstream issue 17928); a
    /// failed request is a backend error.
    pub async fn get_lineups(
        &self,
        info: &ListingsProviderInfo,
        country: Option<&str>,
        location: Option<&str>,
    ) -> Result<Vec<NameIdPair>, ServiceError> {
        let Some(token) = self
            .get_token(info)
            .await?
            .filter(|token| !token.trim().is_empty())
        else {
            return Err(
                SdError::Auth("Could not authenticate with Schedules Direct".to_owned()).into(),
            );
        };

        let country = country.unwrap_or_default();
        let location = location.unwrap_or_default();
        let path = format!(
            "/headends?country={}&postalcode={}",
            encode_query(country),
            encode_query(location)
        );
        let headends: Vec<dto::HeadendsDto> = self
            .request(
                SdRequest::new(RequestMethod::Get, &path, Some(&token)),
                false,
                Some(info),
            )
            .await?
            .unwrap_or_default();

        let lineups: Vec<NameIdPair> = headends
            .into_iter()
            .flat_map(|headend| headend.lineups)
            .map(|lineup| {
                let blank = |v: &Option<String>| v.as_deref().is_none_or(|s| s.trim().is_empty());
                NameIdPair {
                    name: if blank(&lineup.name) {
                        lineup.lineup.clone()
                    } else {
                        lineup.name.clone()
                    },
                    id: if blank(&lineup.lineup) {
                        lineup
                            .uri
                            .as_deref()
                            .map(|uri| uri.rsplit('/').next().unwrap_or(uri).to_owned())
                    } else {
                        lineup.lineup
                    },
                }
            })
            .collect();

        if lineups.is_empty() {
            tracing::warn!(
                country,
                postal_code = location,
                "Schedules Direct has no lineups for this country and postal code"
            );
        }
        Ok(lineups)
    }

    /// A lineup's stations: port of `GetChannels`.
    ///
    /// Each lineup-map entry becomes a channel numbered by its logical channel
    /// number, else its channel, else `atscMajor.atscMinor`, with leading zeros
    /// trimmed (`GetChannelNumber`), and named by its station (or its number).
    ///
    /// `Ok(None)` is "SD gave no token": an earlier account error, the login
    /// back-off, or no credentials. Upstream answers `[]` there; the caller
    /// decides whether that is "no channels" (the mapping dialog) or "SD is
    /// unavailable" (the guide refresh, which then keeps the stored guide). A
    /// provider without a lineup id has no channels (`Ok(Some([]))`).
    ///
    /// # Errors
    ///
    /// A failed SD request.
    pub async fn get_channels(
        &self,
        info: &ListingsProviderInfo,
    ) -> Result<Option<Vec<SdChannel>>, ServiceError> {
        let Some(listings_id) = info.listings_id.as_deref().filter(|id| !id.is_empty()) else {
            return Ok(Some(Vec::new()));
        };
        let Some(token) = self.get_token(info).await?.filter(|t| !t.is_empty()) else {
            return Ok(None);
        };

        let Some(root) = self
            .request::<dto::ChannelDto>(
                SdRequest::new(
                    RequestMethod::Get,
                    &format!("/lineups/{listings_id}"),
                    Some(&token),
                ),
                true,
                Some(info),
            )
            .await?
        else {
            return Ok(Some(Vec::new()));
        };
        tracing::debug!(
            channels = root.map.len(),
            "found the lineup's channels on Schedules Direct"
        );

        Ok(Some(
            root.map
                .iter()
                .map(|map| {
                    let number = channel_number(map);
                    let station = root.stations.iter().find(|station| {
                        match (station.station_id.as_deref(), map.station_id.as_deref()) {
                            (Some(a), Some(b)) => eq_ordinal_ignore_case(a, b),
                            (None, None) => true,
                            _ => false,
                        }
                    });
                    let name = station
                        .and_then(|s| s.name.clone())
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or_else(|| number.clone());
                    SdChannel {
                        id: station
                            .map_or_else(|| map.station_id.clone(), |s| s.station_id.clone()),
                        callsign: station.and_then(|s| s.callsign.clone()),
                        number,
                        name,
                        image_url: station
                            .and_then(|s| s.logo.as_ref())
                            .and_then(|logo| logo.url.clone()),
                    }
                })
                .collect(),
        ))
    }

    /// Whether the image daily limit is in force (`IsImageDailyLimitActive`).
    ///
    /// A limit hit on an earlier UTC day has expired: it is cleared and its
    /// marker file deleted. Guide image pre-caching consults this to skip SD
    /// artwork until SD resets (`GuideManager.PreCacheImages`).
    #[must_use]
    pub fn is_image_daily_limit_active(&self) -> bool {
        let mut limits = self.limits();
        Self::limit_active(&mut limits.image, &self.cache_dir.join(IMAGE_LIMIT_FILE))
    }

    /// Whether the metadata daily limit is in force (`IsMetadataLimitActive`).
    fn is_metadata_limit_active(&self) -> bool {
        let mut limits = self.limits();
        Self::limit_active(
            &mut limits.metadata,
            &self.cache_dir.join(METADATA_LIMIT_FILE),
        )
    }

    fn limit_active(hit: &mut Option<NaiveDate>, path: &Path) -> bool {
        match *hit {
            None => false,
            Some(day) if day < Utc::now().date_naive() => {
                *hit = None;
                let _ = std::fs::remove_file(path);
                false
            }
            Some(_) => true,
        }
    }

    /// `SetImageLimitHit`: in force until SD resets at 00:00 UTC.
    fn set_image_limit_hit(&self) {
        self.limits().image = Some(Utc::now().date_naive());
        self.persist_limit(IMAGE_LIMIT_FILE);
    }

    /// `SetMetadataLimitHit`: in force until SD resets at 00:00 UTC.
    fn set_metadata_limit_hit(&self) {
        self.limits().metadata = Some(Utc::now().date_naive());
        self.persist_limit(METADATA_LIMIT_FILE);
    }

    /// The daily limits, read from their marker files on first use
    /// (`LoadDailyLimitDate`, which upstream runs in its constructor).
    ///
    /// The files are a few bytes each and read once per process, so the
    /// synchronous read upstream does is kept.
    fn limits(&self) -> MutexGuard<'_, DailyLimits> {
        let mut limits = self
            .state
            .limits
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !limits.loaded {
            *limits = DailyLimits {
                loaded: true,
                image: load_daily_limit_date(&self.cache_dir.join(IMAGE_LIMIT_FILE)),
                metadata: load_daily_limit_date(&self.cache_dir.join(METADATA_LIMIT_FILE)),
            };
        }
        limits
    }

    /// `PersistDailyLimitFile`: the hit instant, round-trip format.
    fn persist_limit(&self, file: &str) {
        let path = self.cache_dir.join(file);
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, round_trip(Utc::now())));
        if let Err(error) = written {
            tracing::warn!(path = %path.display(), %error, "could not persist the Schedules Direct daily limit");
        }
    }

    /// `ResetErrorState`: the account error, the login back-off, and the
    /// cached token of the account being saved (the other accounts' tokens stay
    /// valid).
    fn reset_error_state(&self, info: &ListingsProviderInfo) {
        let mut account = self.account();
        account.account_error = false;
        account.last_error = None;
        if let Some(username) = info.username.as_deref().filter(|u| !u.trim().is_empty()) {
            account.tokens.remove(username);
        }
    }

    fn account(&self) -> MutexGuard<'_, AccountState> {
        self.state
            .account
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Port of `GetToken`: a cached token under [`TOKEN_REUSE_HOURS`] old, or a
    /// fresh login.
    ///
    /// `Ok(None)` when there are no credentials, after a permanent account
    /// error, or within [`LOGIN_BACKOFF_MINUTES`] of a transient login error.
    /// A login rejected with a `4xx` that no SD code already explained starts
    /// the back-off too.
    async fn get_token(&self, info: &ListingsProviderInfo) -> Result<Option<String>, SdError> {
        let Some(username) = info.username.as_deref().filter(|u| !u.trim().is_empty()) else {
            return Ok(None);
        };
        let Some(password) = info
            .password
            .as_ref()
            .map(ferrofin_model::secret::Secret::expose)
            .filter(|p| !p.is_empty())
        else {
            return Ok(None);
        };

        let now = Utc::now();
        {
            let account = self.account();
            if account.account_error {
                tracing::debug!(
                    "skipping a Schedules Direct request after an account error; save the listings provider again to retry"
                );
                return Ok(None);
            }
            if account
                .last_error
                .is_some_and(|at| now - at < chrono::Duration::minutes(LOGIN_BACKOFF_MINUTES))
            {
                tracing::debug!("skipping a Schedules Direct request during the login back-off");
                return Ok(None);
            }
            if let Some((token, issued)) = account.tokens.get(username)
                && now - *issued < chrono::Duration::hours(TOKEN_REUSE_HOURS)
            {
                return Ok(Some(token.clone()));
            }
        }

        let _login = self.state.token_lock.lock().await;
        // Another caller may have logged in while this one waited for the
        // lock. Upstream logs in again; reusing that token spares SD a login,
        // and repeated logins are what SD's 4009 ban counts.
        if let Some((token, issued)) = self.account().tokens.get(username)
            && Utc::now() - *issued < chrono::Duration::hours(TOKEN_REUSE_HOURS)
        {
            return Ok(Some(token.clone()));
        }
        match self.get_token_internal(username, password).await {
            Ok(token) => {
                if let Some(token) = &token {
                    self.account()
                        .tokens
                        .insert(username.to_owned(), (token.clone(), Utc::now()));
                }
                Ok(token)
            }
            Err(error) => {
                if let SdError::Status { status, .. } = &error
                    && (400..500).contains(status)
                {
                    let mut account = self.account();
                    if !account.account_error {
                        account.tokens.clear();
                        account.last_error = Some(Utc::now());
                    }
                }
                Err(error)
            }
        }
    }

    /// Port of `GetTokenInternal`: `POST /token` with the password's lowercase
    /// SHA1 hex (jellyfin/jellyfin#17174).
    async fn get_token_internal(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<String>, SdError> {
        #[derive(serde::Serialize)]
        struct TokenRequest<'a> {
            username: &'a str,
            password: &'a str,
        }
        let hashed = ferrofin_common::extensions::sha1_hex(&ascii_bytes(password));
        let body = to_json(&TokenRequest {
            username,
            password: &hashed,
        })?;
        let root: Option<dto::TokenDto> = self
            .request(
                SdRequest::new(RequestMethod::Post, "/token", None).json(body),
                false,
                None,
            )
            .await?;
        match root {
            Some(root) if root.message.as_deref() == Some("OK") => {
                tracing::info!("authenticated with Schedules Direct");
                Ok(root.token)
            }
            root => Err(SdError::Auth(format!(
                "Could not authenticate with Schedules Direct Error: {}",
                root.and_then(|r| r.message)
                    .unwrap_or_else(|| "empty response".to_owned())
            ))),
        }
    }

    /// Port of `Request<T>`: sends `request` and reads a `2xx` body as `T`
    /// (`null` is `None`).
    ///
    /// A failed response's SD code drives the error table: permanent account
    /// errors disable SD, transient login errors start the back-off, the
    /// login-attempt bans set both daily limits, the image and schedule limits
    /// set theirs. Otherwise, with `enable_retry`, an expired token (or a `403`
    /// with no SD code) is refreshed and the request sent once more.
    async fn request<T: DeserializeOwned>(
        &self,
        mut request: SdRequest,
        mut enable_retry: bool,
        info: Option<&ListingsProviderInfo>,
    ) -> Result<Option<T>, SdError> {
        use SdErrorCode as C;
        loop {
            let response = self
                .fetcher
                .send(request.to_source())
                .await
                .map_err(SdError::Backend)?;
            if response.is_success() {
                return serde_json::from_slice::<Option<T>>(&response.body).map_err(|e| {
                    SdError::Backend(ServiceError::backend(format!(
                        "Schedules Direct {}: unreadable response: {e}",
                        request.url
                    )))
                });
            }

            let status = response.status;
            let code = sd_error_code(&response.body);
            tracing::debug!(
                url = %request.url,
                status,
                sd_code = code.map(SdErrorCode::code),
                "Schedules Direct request failed"
            );

            match code {
                Some(
                    C::AccountExpired
                    | C::InvalidHash
                    | C::InvalidUser
                    | C::AccountLocked
                    | C::AppLocked
                    | C::AccountInactive,
                ) => {
                    tracing::warn!(
                        sd_code = code.map(SdErrorCode::code),
                        "Schedules Direct account error: disabled until the listings provider is saved again"
                    );
                    let mut account = self.account();
                    account.tokens.clear();
                    account.account_error = true;
                }
                Some(C::ServiceOffline | C::ServiceBusy | C::AccountTempLock) => {
                    tracing::warn!(
                        sd_code = code.map(SdErrorCode::code),
                        minutes = LOGIN_BACKOFF_MINUTES,
                        "Schedules Direct transient error: backing off"
                    );
                    let mut account = self.account();
                    account.tokens.clear();
                    account.last_error = Some(Utc::now());
                }
                Some(C::MaxLoginAttempts | C::MaxIpAttempts) => {
                    tracing::warn!(
                        sd_code = code.map(SdErrorCode::code),
                        "Schedules Direct login limit: no images or metadata until SD resets at 00:00 UTC"
                    );
                    self.set_image_limit_hit();
                    self.set_metadata_limit_hit();
                }
                Some(C::MaxImageDownloads | C::MaxImageDownloadsTrial) => {
                    tracing::warn!(
                        sd_code = code.map(SdErrorCode::code),
                        "Schedules Direct image download limit: no images until SD resets at 00:00 UTC"
                    );
                    self.set_image_limit_hit();
                }
                Some(C::MaxScheduleRequests) => {
                    tracing::warn!(
                        sd_code = code.map(SdErrorCode::code),
                        "Schedules Direct metadata limit: no guide data until SD resets at 00:00 UTC"
                    );
                    self.set_metadata_limit_hit();
                }
                _ if enable_retry
                    && status < 500
                    && (code == Some(C::TokenExpired) || (status == 403 && code.is_none())) =>
                {
                    self.account().tokens.clear();
                    request.token = match info {
                        // Boxed: `get_token` logs in through this function.
                        Some(info) => Box::pin(self.get_token(info)).await?,
                        None => None,
                    };
                    enable_retry = false;
                    continue;
                }
                _ => {}
            }
            return Err(SdError::Status {
                status,
                url: request.url,
            });
        }
    }

    /// Port of `HasLineup`: whether the account already has `ListingsId`.
    ///
    /// SD answers `400` when the account has no lineups at all, which is "no".
    async fn has_lineup(&self, info: &ListingsProviderInfo) -> Result<bool, SdError> {
        require(info.listings_id.as_deref(), "ListingsId")?;
        let token = self.get_token(info).await?;
        require(token.as_deref(), "token")?;
        match self
            .request::<dto::LineupsDto>(
                SdRequest::new(RequestMethod::Get, "/lineups", token.as_deref()),
                false,
                None,
            )
            .await
        {
            Ok(root) => Ok(root.is_some_and(|root| {
                root.lineups.iter().any(|lineup| {
                    match (info.listings_id.as_deref(), lineup.lineup.as_deref()) {
                        (Some(wanted), Some(id)) => eq_ordinal_ignore_case(wanted, id),
                        _ => false,
                    }
                })
            })),
            Err(SdError::Status { status: 400, .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Port of `AddLineupToAccount`: `PUT /lineups/{ListingsId}`.
    ///
    /// A refusal is logged, not raised, as upstream does: the provider is
    /// still saved, and the account page on schedulesdirect.org is where the
    /// lineup can be added by hand.
    async fn add_lineup_to_account(&self, info: &ListingsProviderInfo) -> Result<(), SdError> {
        let token = self.get_token(info).await?;
        require(token.as_deref(), "token")?;
        require(info.listings_id.as_deref(), "ListingsId")?;
        let listings_id = info.listings_id.as_deref().unwrap_or_default();
        tracing::info!(
            lineup = listings_id,
            "adding the lineup to the Schedules Direct account"
        );

        // `ResponseHeadersRead`: a success ends at its headers; only a
        // refusal's body is read, to be logged.
        let response = self
            .fetcher
            .send(
                SdRequest::new(
                    RequestMethod::Put,
                    &format!("/lineups/{listings_id}"),
                    token.as_deref(),
                )
                .to_source()
                .headers_only(),
            )
            .await
            .map_err(SdError::Backend)?;
        if !response.is_success() {
            tracing::warn!(
                lineup = listings_id,
                status = response.status,
                response = %String::from_utf8_lossy(&response.body),
                "Schedules Direct refused to add the lineup to the account"
            );
        }
        Ok(())
    }
}

/// `GetChannelNumber`: the logical channel number, else the channel, else
/// `atscMajor.atscMinor`, with leading zeros trimmed.
fn channel_number(map: &dto::MapDto) -> String {
    let blank = |v: &Option<String>| v.as_deref().is_none_or(|s| s.trim().is_empty());
    let number = if !blank(&map.logical_channel_number) {
        map.logical_channel_number.clone().unwrap_or_default()
    } else if !blank(&map.channel) {
        map.channel.clone().unwrap_or_default()
    } else {
        format!("{}.{}", map.atsc_major, map.atsc_minor)
    };
    number.trim_start_matches('0').to_owned()
}

/// `DateTime.ToString("O")` of a UTC instant: seven fraction digits and `Z`.
pub(super) fn round_trip(at: DateTime<Utc>) -> String {
    format!(
        "{}.{:07}Z",
        at.format("%Y-%m-%dT%H:%M:%S"),
        at.timestamp_subsec_nanos() / 100
    )
}

/// Percent-encodes a query value (upstream concatenates it raw; `HttpClient`
/// escapes what a URI cannot carry, such as a postal code's space).
fn encode_query(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// `LoadDailyLimitDate`: the UTC day a limit was hit, unless that day is past
/// (the expired file is deleted). An unreadable or unparsable file is deleted
/// and read as no limit.
fn load_daily_limit_date(path: &Path) -> Option<NaiveDate> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => {
            let _ = std::fs::remove_file(path);
            return None;
        }
    };
    let day = dto::parse_datetime(&text)?.date_naive();
    if day < Utc::now().date_naive() {
        let _ = std::fs::remove_file(path);
        return None;
    }
    Some(day)
}

/// Whether the on-disk cache file at `path` exists and was written less than
/// `max_age` ago (`File.Exists && UtcNow - LastWriteTimeUtc < maxAge`, as the
/// Schedules Direct country cache and `XmlTvListingsProvider.GetXml` check it).
/// A missing file, an unreadable mtime, or an mtime that cannot be compared all
/// read as "not fresh".
pub(crate) async fn cache_is_fresh(path: &Path, max_age: Duration) -> bool {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    // A future mtime (clock skew) has a negative age upstream, which is "fresh".
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    age < max_age
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    use ferrofin_traits::error::ServiceError;

    use super::{API_URL, COUNTRY_CACHE_DAYS, SchedulesDirect};
    use crate::fetch::SourceFetcher;

    const COUNTRIES: &[u8] =
        br#"{"North America":[{"fullName":"United States","shortName":"USA"}]}"#;

    /// A fetcher that records how often it was hit and what it was asked for,
    /// answering with `body` (or a backend failure when `body` is `None`).
    struct CountingFetcher {
        body: Option<Vec<u8>>,
        calls: AtomicUsize,
        last_url: std::sync::Mutex<Option<String>>,
    }

    impl CountingFetcher {
        fn serving(body: &[u8]) -> Arc<Self> {
            Arc::new(Self {
                body: Some(body.to_vec()),
                calls: AtomicUsize::new(0),
                last_url: std::sync::Mutex::new(None),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                body: None,
                calls: AtomicUsize::new(0),
                last_url: std::sync::Mutex::new(None),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl SourceFetcher for CountingFetcher {
        async fn fetch(&self, _url: &str) -> Result<String, ServiceError> {
            unreachable!("the country list is fetched as bytes")
        }

        async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, ServiceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_url.lock().unwrap() = Some(url.to_owned());
            self.body
                .clone()
                .ok_or_else(|| ServiceError::backend("schedulesdirect.org: 503"))
        }
    }

    fn sd(fetcher: &Arc<CountingFetcher>, dir: &tempfile::TempDir) -> SchedulesDirect {
        let fetcher: Arc<dyn SourceFetcher> = fetcher.clone();
        SchedulesDirect::new(fetcher, dir.path())
    }

    /// Backdates the cache file's mtime by `days`.
    fn age_cache(sd: &SchedulesDirect, days: u64) {
        let file = std::fs::File::options()
            .write(true)
            .open(sd.countries_cache_path())
            .expect("open cache");
        file.set_modified(SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60))
            .expect("set mtime");
    }

    #[tokio::test]
    async fn fresh_fetch_hits_sd_and_writes_the_cache_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fetcher = CountingFetcher::serving(COUNTRIES);
        // The cache directory need not pre-exist (upstream `CreateDirectory`).
        let nested = dir.path().join("cache");
        let fetcher_dyn: Arc<dyn SourceFetcher> = fetcher.clone();
        let sd = SchedulesDirect::new(fetcher_dyn, &nested);

        let bytes = sd.get_available_countries().await.expect("countries");
        assert_eq!(bytes, COUNTRIES);
        assert_eq!(fetcher.calls(), 1);
        assert_eq!(
            fetcher.last_url.lock().unwrap().as_deref(),
            Some(format!("{API_URL}/available/countries").as_str())
        );
        assert_eq!(
            std::fs::read(nested.join("sd-countries.json")).expect("cache file"),
            COUNTRIES
        );
    }

    #[tokio::test]
    async fn second_call_is_served_from_memory_without_refetching() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fetcher = CountingFetcher::serving(COUNTRIES);
        let sd = sd(&fetcher, &dir);

        sd.get_available_countries().await.expect("first");
        // Clones share the memory cache.
        let again = sd.clone().get_available_countries().await.expect("second");
        assert_eq!(again, COUNTRIES);
        assert_eq!(fetcher.calls(), 1);
    }

    #[tokio::test]
    async fn a_new_process_reads_the_disk_cache_within_the_ttl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = CountingFetcher::serving(COUNTRIES);
        sd(&first, &dir)
            .get_available_countries()
            .await
            .expect("seed the disk cache");

        // A fresh instance (no memory copy) must not hit SD while the file is
        // within its TTL — even when SD would answer differently now.
        let second = CountingFetcher::serving(b"{}");
        let sd = sd(&second, &dir);
        age_cache(&sd, COUNTRY_CACHE_DAYS - 1);
        let bytes = sd.get_available_countries().await.expect("cached");
        assert_eq!(bytes, COUNTRIES);
        assert_eq!(second.calls(), 0);
    }

    #[tokio::test]
    async fn an_expired_disk_cache_is_refetched_and_rewritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = CountingFetcher::serving(COUNTRIES);
        sd(&first, &dir)
            .get_available_countries()
            .await
            .expect("seed the disk cache");

        let second = CountingFetcher::serving(b"{\"updated\":true}");
        let sd = sd(&second, &dir);
        age_cache(&sd, COUNTRY_CACHE_DAYS);
        let bytes = sd.get_available_countries().await.expect("refetched");
        assert_eq!(bytes, b"{\"updated\":true}");
        assert_eq!(second.calls(), 1);
        assert_eq!(
            std::fs::read(sd.countries_cache_path()).expect("cache file"),
            b"{\"updated\":true}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_cache_file_is_removed_and_refetched() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let fetcher = CountingFetcher::serving(COUNTRIES);
        let sd = sd(&fetcher, &dir);
        // A fresh cache file the process cannot read — upstream's `IOException`
        // branch: delete it and fetch anew.
        let path = sd.countries_cache_path();
        std::fs::write(&path, b"stale").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        if std::fs::read(&path).is_ok() {
            // Running as root: permissions do not bite, so the branch cannot be
            // provoked this way.
            return;
        }

        let bytes = sd.get_available_countries().await.expect("refetched");
        assert_eq!(bytes, COUNTRIES);
        assert_eq!(fetcher.calls(), 1);
        // The unreadable file was replaced by the fetched document.
        assert_eq!(std::fs::read(&path).expect("rewritten"), COUNTRIES);
    }

    #[tokio::test]
    async fn upstream_failure_surfaces_a_backend_error_and_caches_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fetcher = CountingFetcher::failing();
        let sd = sd(&fetcher, &dir);

        let err = sd.get_available_countries().await.expect_err("must fail");
        assert!(matches!(err, ServiceError::Backend(_)), "{err:?}");
        assert!(!sd.countries_cache_path().exists());

        // Nothing was memoised: the next call tries SD again.
        let _ = sd.get_available_countries().await;
        assert_eq!(fetcher.calls(), 2);
    }

    #[tokio::test]
    async fn an_unwritable_cache_dir_is_a_backend_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Put a *file* where the cache directory must be created.
        let blocked = dir.path().join("cache");
        std::fs::write(&blocked, b"not a dir").expect("block");
        let fetcher = CountingFetcher::serving(COUNTRIES);
        let fetcher_dyn: Arc<dyn SourceFetcher> = fetcher.clone();
        let sd = SchedulesDirect::new(fetcher_dyn, &blocked);

        let err = sd.get_available_countries().await.expect_err("must fail");
        assert!(matches!(err, ServiceError::BackendSource(_)), "{err:?}");
        assert!(err.to_string().starts_with("create "), "{err}");
    }

    #[test]
    fn debug_names_the_cache_dir_only() {
        let fetcher = CountingFetcher::serving(COUNTRIES);
        let fetcher_dyn: Arc<dyn SourceFetcher> = fetcher;
        let sd = SchedulesDirect::new(fetcher_dyn, "/var/cache/ferrofin");
        let dbg = format!("{sd:?}");
        assert!(dbg.contains("/var/cache/ferrofin"), "{dbg}");
        assert!(!dbg.contains("countries"), "{dbg}");
    }
}
