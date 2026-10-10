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
    SubtitleMediaType, SubtitleProvider, SubtitleResponse, SubtitleSearchRequest,
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

#[derive(Deserialize)]
struct DownloadResponse {
    link: String,
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
            .map_err(request_err)?;
        if !resp.status().is_success() {
            return Err(ServiceError::unauthorized(format!(
                "OpenSubtitles login failed: HTTP {}",
                resp.status()
            )));
        }
        let body: LoginResponse = resp.counted_json().await.map_err(net_err)?;
        Ok(body.token)
    }

    /// Validates the supplied credentials by attempting a login. Returns `Ok(())`
    /// on success.
    ///
    /// # Errors
    /// Returns [`ServiceError::Unauthorized`] if the credentials are rejected or
    /// [`ServiceError::Backend`] on a transport error.
    pub async fn validate_config(&self, cfg: &OpenSubtitlesConfig) -> Result<(), ServiceError> {
        if !cfg.is_configured() {
            return Err(ServiceError::invalid_input(
                "OpenSubtitles account or API key is not configured",
            ));
        }
        self.login(cfg).await.map(|_| ())
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

    async fn validate_login(&self, config_json: &[u8]) -> Result<(), ServiceError> {
        let cfg: OpenSubtitlesConfig = serde_json::from_slice(config_json)
            .map_err(|_| ServiceError::invalid_input("invalid OpenSubtitles configuration"))?;
        self.validate_config(&cfg).await
    }

    async fn get_subtitles(
        &self,
        provider_local_id: &str,
    ) -> Result<SubtitleResponse, ServiceError> {
        let cfg = self.config().await?;
        if !cfg.is_configured() {
            return Err(ServiceError::invalid_input(
                "OpenSubtitles account or API key is not configured",
            ));
        }
        let parsed = parse_subtitle_id(provider_local_id)
            .ok_or_else(|| ServiceError::invalid_input("invalid OpenSubtitles subtitle id"))?;
        let file_id = parsed.file_id;

        let token = self.login(&cfg).await?;

        // Turn the file id into a one-time download link.
        let dl: DownloadResponse = self
            .http
            .post(format!("{}/download", self.base_url))
            .header("Api-Key", cfg.effective_api_key())
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
            .bearer_auth(&token)
            .json(&DownloadRequest {
                file_id,
                sub_format: parsed.format.clone(),
            })
            .send_limited(&self.limiter)
            .await
            .map_err(request_err)?
            .error_for_status()
            .map_err(net_err)?
            .counted_json()
            .await
            .map_err(net_err)?;

        // Fetch the subtitle bytes from the link.
        let content = self
            .http
            .get(&dl.link)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send_limited(&crate::image_download::limiter_for(
                &dl.link,
                "opensubtitles",
            ))
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
