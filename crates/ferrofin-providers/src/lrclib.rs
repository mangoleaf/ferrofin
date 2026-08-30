//! LrcLib lyric provider — a [`LyricProvider`] over the lrclib.net REST API.
//!
//! Faithful port of Jellyfin's `Jellyfin.Plugin.LrcLib` plugin
//! (`LrcLibProvider.cs` + `Models/LrcLibSearchResponse.cs`): the provider is
//! compiled in and needs no key or credentials — lrclib.net is a free, open
//! lyrics database. Like the plugin, search runs in one of two modes
//! ([`LrcLibConfig::use_strict_search`], default strict):
//!
//! - **strict** (`GET /api/get?track_name=…&artist_name=…&album_name=…&duration=…`)
//!   requires song + artist + album + duration and returns the single best
//!   signature match;
//! - **fuzzy** (`GET /api/search?track_name=…[&artist_name=…][&album_name=…]`)
//!   returns many candidates, synced results first.
//!
//! Each LrcLib record can yield up to two candidates — a synced (`.lrc`) and a
//! plain (`.txt`) variant — whose provider-local ids are `"{id}_synced"` /
//! `"{id}_plain"`; [`get_lyrics`](LrcLibProvider::get_lyrics) resolves either
//! via `GET /api/get/{id}`. Every request sends a descriptive `User-Agent`, as
//! lrclib.net asks of API consumers.

use async_trait::async_trait;
use ferrofin_model::lyrics::{LyricMetadata, LyricSearchRequest};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::stubs::{LyricProvider, LyricResponse, RemoteLyricInfo};
use serde::Deserialize;

/// The provider's display name. Port of `LrcLibPlugin.Name` — the lyric
/// manager derives the provider id from this, so it must match upstream for
/// id parity.
pub const PROVIDER_NAME: &str = "LrcLib Lyrics";

/// The lrclib.net API base URL.
const BASE_URL: &str = "https://lrclib.net";

/// The provider-local id suffix marking the synced variant of a record.
const SYNCED_SUFFIX: &str = "synced";
/// The provider-local id suffix marking the plain-text variant of a record.
const PLAIN_SUFFIX: &str = "plain";
/// The lyric format (and sidecar extension) of a synced candidate.
const SYNCED_FORMAT: &str = "lrc";
/// The lyric format (and sidecar extension) of a plain candidate.
const PLAIN_FORMAT: &str = "txt";

/// The descriptive `User-Agent` lrclib.net asks API consumers to send.
const USER_AGENT: &str = concat!(
    "Ferrofin/",
    env!("CARGO_PKG_VERSION"),
    " (Jellyfin-compatible media server)"
);

/// The number of .NET ticks (100 ns) per second.
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The provider's search behavior. Port of the plugin's `PluginConfiguration`
/// (upstream defaults: strict search on, artist/album included).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LrcLibConfig {
    /// Strict signature search (`/api/get`) vs. fuzzy search (`/api/search`).
    /// Port of `UseStrictSearch` (default `true`).
    pub use_strict_search: bool,
    /// Omit the artist name from a fuzzy search. Port of `ExcludeArtistName`
    /// (default `false`).
    pub exclude_artist_name: bool,
    /// Omit the album name from a fuzzy search. Port of `ExcludeAlbumName`
    /// (default `false`).
    pub exclude_album_name: bool,
}

impl Default for LrcLibConfig {
    fn default() -> Self {
        Self {
            use_strict_search: true,
            exclude_artist_name: false,
            exclude_album_name: false,
        }
    }
}

// ── wire DTO ────────────────────────────────────────────────────────────────

/// One lrclib.net record. Port of `LrcLibSearchResponse` (camelCase JSON).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct LrcLibItem {
    /// The lrclib record id.
    id: i64,
    /// The track name.
    track_name: Option<String>,
    /// The artist name.
    artist_name: Option<String>,
    /// The album name.
    album_name: Option<String>,
    /// The track duration in seconds.
    duration: Option<f64>,
    /// The plain (unsynced) lyrics, when available.
    plain_lyrics: Option<String>,
    /// The synced (LRC) lyrics, when available.
    synced_lyrics: Option<String>,
}

/// Converts an lrclib duration in seconds to .NET ticks. Port of
/// `TimeSpan.FromSeconds(d).Ticks`.
fn seconds_to_ticks(seconds: f64) -> i64 {
    // Durations are a few thousand seconds at most, far inside f64/i64 range.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    {
        (seconds * TICKS_PER_SECOND as f64) as i64
    }
}

/// Converts a duration in .NET ticks to whole+fractional seconds for the
/// strict-search `duration` parameter. Port of
/// `TimeSpan.FromTicks(t).TotalSeconds`.
fn ticks_to_seconds(ticks: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        ticks as f64 / TICKS_PER_SECOND as f64
    }
}

/// Maps one lrclib record into up to two provider-local candidates (synced
/// first, then plain). Port of `LrcLibProvider.GetRemoteLyrics`.
fn map_item(item: &LrcLibItem) -> Vec<RemoteLyricInfo> {
    let metadata = |synced: bool| LyricMetadata {
        album: item.album_name.clone(),
        artist: item.artist_name.clone(),
        title: item.track_name.clone(),
        length: Some(seconds_to_ticks(item.duration.unwrap_or(0.0))),
        is_synced: Some(synced),
        ..LyricMetadata::default()
    };

    let mut out = Vec::new();
    if let Some(synced) = item.synced_lyrics.as_deref().filter(|s| !s.is_empty()) {
        out.push(RemoteLyricInfo {
            id: format!("{}_{SYNCED_SUFFIX}", item.id),
            provider_name: PROVIDER_NAME.to_owned(),
            metadata: metadata(true),
            lyrics: LyricResponse {
                format: SYNCED_FORMAT.to_owned(),
                text: synced.to_owned(),
            },
        });
    }
    if let Some(plain) = item.plain_lyrics.as_deref().filter(|s| !s.is_empty()) {
        out.push(RemoteLyricInfo {
            id: format!("{}_{PLAIN_SUFFIX}", item.id),
            provider_name: PROVIDER_NAME.to_owned(),
            metadata: metadata(false),
            lyrics: LyricResponse {
                format: PLAIN_FORMAT.to_owned(),
                text: plain.to_owned(),
            },
        });
    }
    out
}

/// The strict-search query parameters, or `None` when a required field (song /
/// artist / album / duration) is missing. Port of `LrcLibProvider.GetExactMatch`
/// preconditions.
fn strict_query(request: &LyricSearchRequest) -> Option<Vec<(&'static str, String)>> {
    let song = request.song_name.as_deref().filter(|s| !s.is_empty())?;
    let artist = request
        .artist_names
        .as_ref()
        .and_then(|a| a.first())
        .filter(|s| !s.is_empty())?;
    let album = request.album_name.as_deref().filter(|s| !s.is_empty())?;
    let duration = request.duration?;
    Some(vec![
        ("track_name", song.to_owned()),
        ("artist_name", artist.clone()),
        ("album_name", album.to_owned()),
        ("duration", ticks_to_seconds(duration).to_string()),
    ])
}

/// The fuzzy-search query parameters, or `None` when a required field is
/// missing (song always; artist/album unless excluded by `config`). Port of
/// `LrcLibProvider.GetFuzzyMatch` preconditions.
fn fuzzy_query(
    request: &LyricSearchRequest,
    config: LrcLibConfig,
) -> Option<Vec<(&'static str, String)>> {
    let song = request.song_name.as_deref().filter(|s| !s.is_empty())?;
    let mut query = vec![("track_name", song.to_owned())];
    if !config.exclude_artist_name {
        let artist = request
            .artist_names
            .as_ref()
            .and_then(|a| a.first())
            .filter(|s| !s.is_empty())?;
        query.push(("artist_name", artist.clone()));
    }
    if !config.exclude_album_name {
        let album = request.album_name.as_deref().filter(|s| !s.is_empty())?;
        query.push(("album_name", album.to_owned()));
    }
    Some(query)
}

/// The lrclib.net-backed lyric provider.
pub struct LrcLibProvider {
    /// The shared HTTP client.
    http: reqwest::Client,
    /// The search behavior (strict vs. fuzzy, field exclusions).
    config: LrcLibConfig,
    /// The API base URL (overridable for tests).
    base_url: String,
}

impl std::fmt::Debug for LrcLibProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LrcLibProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Default for LrcLibProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LrcLibProvider {
    /// Builds the provider with the upstream default configuration (strict
    /// search). No key or credentials are needed.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(LrcLibConfig::default())
    }

    /// Builds the provider with an explicit search configuration.
    #[must_use]
    pub fn with_config(config: LrcLibConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
            base_url: BASE_URL.to_owned(),
        }
    }

    /// Overrides the API base URL (test seam).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Runs a GET returning `T`, or `None` on any transport/HTTP/decode failure
    /// (mirrors the plugin treating a failed search as "no results").
    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&'static str, String)],
    ) -> Option<T> {
        let result = self
            .http
            .get(format!("{}{path}", self.base_url))
            .query(query)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await;
        let response = match result {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, path, "lrclib request failed");
                return None;
            }
        };
        if !response.status().is_success() {
            tracing::debug!(status = %response.status(), path, "lrclib request rejected");
            return None;
        }
        match response.json::<T>().await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!(error = %e, path, "lrclib response decode failed");
                None
            }
        }
    }
}

#[async_trait]
impl LyricProvider for LrcLibProvider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn search(
        &self,
        request: &LyricSearchRequest,
    ) -> Result<Vec<RemoteLyricInfo>, ServiceError> {
        if self.config.use_strict_search {
            let Some(query) = strict_query(request) else {
                tracing::debug!("lrclib strict search skipped: missing song/artist/album/duration");
                return Ok(Vec::new());
            };
            let Some(item) = self.get_json::<LrcLibItem>("/api/get", &query).await else {
                return Ok(Vec::new());
            };
            Ok(map_item(&item))
        } else {
            let Some(query) = fuzzy_query(request, self.config) else {
                tracing::debug!("lrclib fuzzy search skipped: missing song/artist/album");
                return Ok(Vec::new());
            };
            let Some(items) = self
                .get_json::<Vec<LrcLibItem>>("/api/search", &query)
                .await
            else {
                return Ok(Vec::new());
            };
            let mut results: Vec<RemoteLyricInfo> = items.iter().flat_map(map_item).collect();
            // Synced candidates first (upstream `OrderByDescending(IsSynced)`).
            results.sort_by_key(|r| !r.metadata.is_synced.unwrap_or(false));
            Ok(results)
        }
    }

    async fn get_lyrics(
        &self,
        provider_local_id: &str,
    ) -> Result<Option<LyricResponse>, ServiceError> {
        // The provider-local id is `"{lrclib_id}_{synced|plain}"`.
        // An id this provider did not mint is a plain miss, not an error:
        // upstream `GetLyricsAsync` returning null is the controller's 404, and
        // the vendored contract declares no 400 on these routes.
        let Some((record_id, variant)) = provider_local_id.split_once('_') else {
            tracing::debug!(provider_local_id, "malformed lrclib lyric id");
            return Ok(None);
        };
        let Some(item) = self
            .get_json::<LrcLibItem>(&format!("/api/get/{record_id}"), &[])
            .await
        else {
            return Err(ServiceError::not_found(format!(
                "lrclib has no lyric record {record_id}"
            )));
        };
        if variant.eq_ignore_ascii_case(SYNCED_SUFFIX)
            && let Some(text) = item.synced_lyrics.filter(|s| !s.is_empty())
        {
            return Ok(Some(LyricResponse {
                format: SYNCED_FORMAT.to_owned(),
                text,
            }));
        }
        if variant.eq_ignore_ascii_case(PLAIN_SUFFIX)
            && let Some(text) = item.plain_lyrics.filter(|s| !s.is_empty())
        {
            return Ok(Some(LyricResponse {
                format: PLAIN_FORMAT.to_owned(),
                text,
            }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LrcLibConfig, LrcLibItem, LrcLibProvider, fuzzy_query, map_item, seconds_to_ticks,
        strict_query, ticks_to_seconds,
    };
    use ferrofin_model::lyrics::LyricSearchRequest;
    use ferrofin_traits::stubs::LyricProvider;

    /// Serves exactly one canned HTTP response on an ephemeral localhost port
    /// and reports the request line it received. No external network involved.
    fn spawn_stub_server(
        status_line: &'static str,
        body: String,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub server");
        let addr = listener.local_addr().expect("stub server addr");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Read until the end of the request headers.
                let mut raw = Vec::new();
                let mut buf = [0u8; 1024];
                while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&raw);
                let _ = tx.send(request.lines().next().unwrap_or_default().to_owned());
                let response = format!(
                    "{status_line}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// A canned lrclib record as returned by `GET /api/get/{id}` (abridged real
    /// response shape).
    const CANNED_ITEM: &str = r#"{
        "id": 3396226,
        "trackName": "I Want to Live",
        "artistName": "Borislav Slavov",
        "albumName": "Baldur's Gate 3 (Original Game Soundtrack)",
        "duration": 233.0,
        "instrumental": false,
        "plainLyrics": "I want to live\nSomewhere I belong",
        "syncedLyrics": "[00:17.12] I want to live\n[00:20.31] Somewhere I belong"
    }"#;

    fn request() -> LyricSearchRequest {
        LyricSearchRequest {
            song_name: Some("I Want to Live".to_owned()),
            artist_names: Some(vec!["Borislav Slavov".to_owned()]),
            album_name: Some("Baldur's Gate 3".to_owned()),
            duration: Some(233 * 10_000_000),
            ..LyricSearchRequest::default()
        }
    }

    #[test]
    fn parses_canned_json_and_maps_both_variants() {
        let item: LrcLibItem = serde_json::from_str(CANNED_ITEM).expect("canned JSON parses");
        let results = map_item(&item);
        assert_eq!(results.len(), 2);

        let synced = &results[0];
        assert_eq!(synced.id, "3396226_synced");
        assert_eq!(synced.provider_name, "LrcLib Lyrics");
        assert_eq!(synced.metadata.is_synced, Some(true));
        assert_eq!(synced.metadata.artist.as_deref(), Some("Borislav Slavov"));
        assert_eq!(synced.metadata.title.as_deref(), Some("I Want to Live"));
        assert_eq!(synced.metadata.length, Some(233 * 10_000_000));
        assert_eq!(synced.lyrics.format, "lrc");
        assert!(synced.lyrics.text.starts_with("[00:17.12]"));

        let plain = &results[1];
        assert_eq!(plain.id, "3396226_plain");
        assert_eq!(plain.metadata.is_synced, Some(false));
        assert_eq!(plain.lyrics.format, "txt");
        assert_eq!(plain.lyrics.text, "I want to live\nSomewhere I belong");
    }

    #[test]
    fn map_item_skips_missing_variants() {
        let item: LrcLibItem =
            serde_json::from_str(r#"{"id": 7, "plainLyrics": "Only plain here"}"#).expect("parses");
        let results = map_item(&item);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "7_plain");
        assert_eq!(results[0].lyrics.format, "txt");

        let empty: LrcLibItem =
            serde_json::from_str(r#"{"id": 8, "instrumental": true}"#).expect("parses");
        assert!(map_item(&empty).is_empty());
    }

    #[test]
    fn strict_query_requires_all_fields() {
        let query = strict_query(&request()).expect("complete request builds a query");
        assert_eq!(
            query,
            vec![
                ("track_name", "I Want to Live".to_owned()),
                ("artist_name", "Borislav Slavov".to_owned()),
                ("album_name", "Baldur's Gate 3".to_owned()),
                ("duration", "233".to_owned()),
            ]
        );

        for strip in [
            |r: &mut LyricSearchRequest| r.song_name = None,
            |r: &mut LyricSearchRequest| r.artist_names = None,
            |r: &mut LyricSearchRequest| r.album_name = None,
            |r: &mut LyricSearchRequest| r.duration = None,
        ] {
            let mut incomplete = request();
            strip(&mut incomplete);
            assert!(strict_query(&incomplete).is_none());
        }
    }

    #[test]
    fn fuzzy_query_honours_exclusions() {
        let full = fuzzy_query(&request(), LrcLibConfig::default()).expect("builds");
        assert_eq!(full.len(), 3);

        let track_only = fuzzy_query(
            &request(),
            LrcLibConfig {
                use_strict_search: false,
                exclude_artist_name: true,
                exclude_album_name: true,
            },
        )
        .expect("builds");
        assert_eq!(
            track_only,
            vec![("track_name", "I Want to Live".to_owned())]
        );

        // With artist required but absent, the query cannot be built.
        let mut no_artist = request();
        no_artist.artist_names = None;
        assert!(fuzzy_query(&no_artist, LrcLibConfig::default()).is_none());
    }

    #[test]
    fn tick_conversions_round_trip() {
        assert_eq!(seconds_to_ticks(233.0), 2_330_000_000);
        let seconds = ticks_to_seconds(2_330_000_000);
        assert!((seconds - 233.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn strict_search_calls_api_get_with_signature() {
        let (base_url, seen) = spawn_stub_server("HTTP/1.1 200 OK", CANNED_ITEM.to_owned());
        let provider = LrcLibProvider::new().with_base_url(base_url);

        let results = provider.search(&request()).await.expect("search");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "3396226_synced");

        let request_line = seen.recv().expect("stub saw the request");
        assert!(request_line.starts_with("GET /api/get?"));
        assert!(request_line.contains("track_name=I+Want+to+Live"));
        assert!(request_line.contains("duration=233"));
    }

    #[tokio::test]
    async fn fuzzy_search_calls_api_search_and_sorts_synced_first() {
        // Plain-only record first: the sort must still put the synced
        // candidate ahead of it.
        let body = format!(r#"[{{"id": 1, "plainLyrics": "plain only"}}, {CANNED_ITEM}]"#);
        let (base_url, seen) = spawn_stub_server("HTTP/1.1 200 OK", body);
        let provider = LrcLibProvider::with_config(LrcLibConfig {
            use_strict_search: false,
            ..LrcLibConfig::default()
        })
        .with_base_url(base_url);

        let results = provider.search(&request()).await.expect("search");
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].id, "3396226_synced");
        assert_eq!(results[0].metadata.is_synced, Some(true));

        let request_line = seen.recv().expect("stub saw the request");
        assert!(request_line.starts_with("GET /api/search?"));
    }

    #[tokio::test]
    async fn search_treats_http_error_as_no_results() {
        let (base_url, _seen) =
            spawn_stub_server("HTTP/1.1 500 Internal Server Error", "{}".to_owned());
        let provider = LrcLibProvider::new().with_base_url(base_url);
        assert!(
            provider
                .search(&request())
                .await
                .expect("search")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn search_skips_request_when_required_fields_missing() {
        // No stub server: the strict preconditions fail before any request.
        let mut incomplete = request();
        incomplete.duration = None;
        let provider = LrcLibProvider::new().with_base_url("http://127.0.0.1:1");
        assert!(
            provider
                .search(&incomplete)
                .await
                .expect("search")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn get_lyrics_fetches_record_and_selects_variant() {
        let (base_url, seen) = spawn_stub_server("HTTP/1.1 200 OK", CANNED_ITEM.to_owned());
        let provider = LrcLibProvider::new().with_base_url(base_url);

        let response = provider
            .get_lyrics("3396226_synced")
            .await
            .expect("fetch")
            .expect("synced variant exists");
        assert_eq!(response.format, "lrc");
        assert!(response.text.starts_with("[00:17.12]"));

        let request_line = seen.recv().expect("stub saw the request");
        assert!(request_line.starts_with("GET /api/get/3396226 "));
    }

    #[tokio::test]
    async fn get_lyrics_missing_variant_and_malformed_id_are_a_miss() {
        // The canned record has no synced lyrics → the synced variant is gone.
        let (base_url, _seen) = spawn_stub_server(
            "HTTP/1.1 200 OK",
            r#"{"id": 9, "plainLyrics": "words"}"#.to_owned(),
        );
        let provider = LrcLibProvider::new().with_base_url(base_url);
        assert!(
            provider
                .get_lyrics("9_synced")
                .await
                .expect("fetch")
                .is_none()
        );

        // An id without the variant suffix was never minted here: a miss
        // (the controller's 404), not an off-contract 400.
        let provider = LrcLibProvider::new().with_base_url("http://127.0.0.1:1");
        assert!(provider.get_lyrics("42").await.expect("runs").is_none());

        // An unreachable remote surfaces as not-found.
        assert!(provider.get_lyrics("42_synced").await.is_err());
    }
}
