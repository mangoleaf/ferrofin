//! ListenBrainz Labs client — port of
//! `MediaBrowser.Providers/Plugins/ListenBrainz`.
//!
//! The Labs API answers "which artists are similar to this one" from aggregated
//! listening sessions, keyed by MusicBrainz artist id. Requests use server quota
//! headers when available, with a one-second minimum for the public Labs server.
//! The settings are the ListenBrainz plugin's
//! ([`builtin_plugins::LISTENBRAINZ`](crate::builtin_plugins::LISTENBRAINZ)),
//! read per call.

use crate::rate_limit::CountedBody as _;
use crate::rate_limit::RateLimiter;
use std::time::Duration;

use serde::Deserialize;

/// The default Labs API server (C# `PluginConfiguration.DefaultLabsServer`).
pub const DEFAULT_LABS_SERVER: &str = "https://labs.api.listenbrainz.org";

/// The default number of days a similar-artist result is cached
/// (C# `PluginConfiguration.SimilarItemsCacheDays`).
pub const DEFAULT_CACHE_DAYS: i32 = 14;

/// The similarity algorithms the Labs API exposes — port of
/// `Configuration.SimilarityAlgorithm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SimilarityAlgorithm {
    /// Session-based over ~5 years of listening data.
    #[default]
    SessionBased1825Days,
    /// Session-based over ~5 years (alternate).
    SessionBased1800Days,
    /// Session-based over ~20 years.
    SessionBased7500Days,
    /// Session-based over ~20 years with a higher contribution threshold.
    SessionBased7500DaysHighContribution,
    /// Session-based over ~25 years.
    SessionBased9000Days,
    /// Session-based over ~75 days of recent listening.
    SessionBased75Days,
}

impl SimilarityAlgorithm {
    /// The `algorithm=` query value — port of `SimilarityAlgorithmExtensions`.
    #[must_use]
    pub fn as_api_string(self) -> &'static str {
        match self {
            Self::SessionBased1825Days => {
                "session_based_days_1825_session_300_contribution_3_threshold_10_limit_100_filter_True_skip_30"
            }
            Self::SessionBased1800Days => {
                "session_based_days_1800_session_300_contribution_3_threshold_10_limit_100_filter_True_skip_30"
            }
            Self::SessionBased7500Days => {
                "session_based_days_7500_session_300_contribution_3_threshold_10_limit_100_filter_True_skip_30"
            }
            Self::SessionBased7500DaysHighContribution => {
                "session_based_days_7500_session_300_contribution_5_threshold_10_limit_100_filter_True_skip_30"
            }
            Self::SessionBased9000Days => {
                "session_based_days_9000_session_300_contribution_5_threshold_15_limit_50_skip_30"
            }
            Self::SessionBased75Days => {
                "session_based_days_75_session_300_contribution_5_threshold_10_limit_100_filter_True_skip_30"
            }
        }
    }

    /// The algorithms in declaration order, by name: the C# enum's values are
    /// their indexes.
    const NAMED: [(&'static str, Self); 6] = [
        ("SessionBased1825Days", Self::SessionBased1825Days),
        ("SessionBased1800Days", Self::SessionBased1800Days),
        ("SessionBased7500Days", Self::SessionBased7500Days),
        (
            "SessionBased7500DaysHighContribution",
            Self::SessionBased7500DaysHighContribution,
        ),
        ("SessionBased9000Days", Self::SessionBased9000Days),
        ("SessionBased75Days", Self::SessionBased75Days),
    ];

    /// A stored `Algorithm`, as `JsonStringEnumConverter` reads one: its name
    /// in any case, or its value, as a number or a string. Anything else is the
    /// default, as `ToApiString`'s `_` arm makes an undefined value.
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Self {
        let index = match value {
            serde_json::Value::String(text) => {
                if let Some((_, algorithm)) = Self::NAMED
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(text))
                {
                    return *algorithm;
                }
                text.parse::<u64>().ok()
            }
            serde_json::Value::Number(n) => n.as_u64(),
            _ => None,
        };
        index
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| Self::NAMED.get(i))
            .map_or_else(Self::default, |(_, algorithm)| *algorithm)
    }
}

/// The ListenBrainz plugin's settings — port of its `PluginConfiguration`.
#[derive(Debug, Clone, PartialEq)]
pub struct ListenBrainzConfig {
    /// The Labs API root (`LabsServer`). Blank falls back to
    /// [`DEFAULT_LABS_SERVER`].
    pub labs_server: String,
    /// The similarity algorithm to request (`Algorithm`).
    pub algorithm: SimilarityAlgorithm,
    /// Minimum request spacing in seconds (`RateLimit`); defaults to one. The
    /// public Labs server always retains at least one second, even without
    /// quota headers.
    pub rate_limit_seconds: f64,
    /// How many days a result may be cached (`SimilarItemsCacheDays`); `0`
    /// disables caching.
    pub cache_days: i32,
}

impl Default for ListenBrainzConfig {
    fn default() -> Self {
        Self {
            labs_server: DEFAULT_LABS_SERVER.to_owned(),
            algorithm: SimilarityAlgorithm::default(),
            rate_limit_seconds: 1.0,
            cache_days: DEFAULT_CACHE_DAYS,
        }
    }
}

impl ListenBrainzConfig {
    /// The settings a stored configuration document holds. A member that is
    /// missing or of another kind keeps its default, as a C# property keeps
    /// its initializer, so one bad value never resets the others.
    #[must_use]
    pub fn from_document(document: &serde_json::Map<String, serde_json::Value>) -> Self {
        let defaults = Self::default();
        Self {
            labs_server: document
                .get("LabsServer")
                .and_then(serde_json::Value::as_str)
                .map_or(defaults.labs_server, str::to_owned),
            algorithm: document
                .get("Algorithm")
                .map_or(defaults.algorithm, SimilarityAlgorithm::from_json),
            rate_limit_seconds: document
                .get("RateLimit")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(defaults.rate_limit_seconds),
            cache_days: document
                .get("SimilarItemsCacheDays")
                .and_then(serde_json::Value::as_i64)
                .and_then(|days| i32::try_from(days).ok())
                .unwrap_or(defaults.cache_days),
        }
    }

    /// The server root with any trailing slash trimmed, falling back to the
    /// default when unset (the `LabsServer` setter).
    #[must_use]
    pub fn server(&self) -> &str {
        let server = self.labs_server.trim().trim_end_matches('/');
        if server.is_empty() {
            DEFAULT_LABS_SERVER
        } else {
            server
        }
    }

    fn request_interval(&self) -> Duration {
        let interval = Duration::try_from_secs_f64(self.rate_limit_seconds)
            .unwrap_or(Duration::from_secs(1))
            .min(Duration::from_secs(u64::from(u32::MAX)));
        if reqwest::Url::parse(self.server()).is_ok_and(|url| {
            url.host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case("labs.api.listenbrainz.org"))
        }) {
            interval.max(Duration::from_secs(1))
        } else {
            interval
        }
    }
}

/// Adds `AlgorithmString`, the configuration's get-only member, to a read of
/// it: the `Algorithm`'s API string (`PluginConfiguration.AlgorithmString`).
pub fn add_algorithm_string(config: &mut serde_json::Map<String, serde_json::Value>) {
    let algorithm = config
        .get("Algorithm")
        .map_or_else(SimilarityAlgorithm::default, SimilarityAlgorithm::from_json);
    config.insert(
        "AlgorithmString".to_owned(),
        algorithm.as_api_string().into(),
    );
}

/// A ListenBrainz Labs client, reading the ListenBrainz plugin's settings per
/// call.
#[derive(Debug)]
pub struct ListenBrainzClient {
    http: reqwest::Client,
    limiter: RateLimiter,
    plugin: crate::plugin_config::ConfigSource,
}

impl Default for ListenBrainzClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ListenBrainzClient {
    /// A client on the plugin's defaults until
    /// [`attach_plugin_manager`](Self::attach_plugin_manager) is called.
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: crate::http_client::client(),
            limiter: RateLimiter::new("listenbrainz"),
            plugin: crate::plugin_config::ConfigSource::new(),
        }
    }

    /// Binds the plugin manager, making the ListenBrainz settings page live.
    /// Called once by the composition root.
    pub fn attach_plugin_manager(
        &self,
        plugins: std::sync::Arc<dyn ferrofin_traits::plugins::PluginManager>,
    ) {
        self.plugin.attach(plugins);
    }

    /// The plugin's settings for this call, read the way
    /// `ListenBrainzPlugin.Instance.Configuration` is: an admin's save changes
    /// the next lookup, with no restart.
    pub(crate) async fn settings(&self) -> ListenBrainzConfig {
        let document: serde_json::Map<String, serde_json::Value> = self
            .plugin
            .load(crate::builtin_plugins::LISTENBRAINZ.id)
            .await;
        ListenBrainzConfig::from_document(&document)
    }

    /// The MusicBrainz artist ids similar to `artist_mbid`, most similar first.
    ///
    /// Port of `ListenBrainzLabsClient.GetSimilarArtistsAsync`: the seed artist
    /// is dropped from its own results and the rest are ordered by descending
    /// score. Empty on any failure — a similarity lookup never fails a request.
    pub async fn similar_artists(&self, artist_mbid: &str) -> Vec<String> {
        let mbid = artist_mbid.trim();
        if mbid.is_empty() {
            return Vec::new();
        }
        let config = self.settings().await;
        let url = format!("{}/similar-artists/json", config.server());
        let request = self.http.get(&url).query(&[
            ("artist_mbids", mbid),
            ("algorithm", config.algorithm.as_api_string()),
        ]);
        let resp = match self
            .limiter
            .send_request(request, config.request_interval())
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                tracing::debug!(provider = "listenbrainz", %err, "similar-artist request failed");
                return Vec::new();
            }
        };
        if !resp.status().is_success() {
            tracing::debug!(
                provider = "listenbrainz",
                status = %resp.status(),
                "similar-artist request returned non-success"
            );
            return Vec::new();
        }
        let Ok(mut artists) = resp.counted_json::<Vec<SimilarArtist>>().await else {
            return Vec::new();
        };
        artists.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        artists
            .into_iter()
            .filter_map(|a| a.artist_mbid)
            .filter(|id| !id.eq_ignore_ascii_case(mbid))
            .collect()
    }
}

/// One entry of the Labs `similar-artists` response.
#[derive(Debug, Deserialize)]
struct SimilarArtist {
    #[serde(default)]
    artist_mbid: Option<String>,
    #[serde(default)]
    score: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_http::MockServer;

    /// A client reading `config` as the ListenBrainz plugin's settings.
    fn client_with(config: &serde_json::Value) -> ListenBrainzClient {
        let client = ListenBrainzClient::new();
        client.attach_plugin_manager(crate::plugin_config::tests_support::manager_with(
            crate::builtin_plugins::LISTENBRAINZ.id,
            config.to_string().into_bytes(),
        ));
        client
    }

    #[test]
    fn an_empty_server_falls_back_to_the_default() {
        let config = ListenBrainzConfig {
            labs_server: "   ".to_owned(),
            ..ListenBrainzConfig::default()
        };
        assert_eq!(config.server(), DEFAULT_LABS_SERVER);
    }

    /// The stored members as a stock 12.4 binds them (each measured with a
    /// `POST /Plugins/{id}/Configuration`): the algorithm by name in any case
    /// or by value, a number or a string. One unusable member keeps its
    /// default and leaves the others alone.
    #[test]
    fn a_stored_document_is_read_member_by_member() {
        let read = |json: serde_json::Value| {
            ListenBrainzConfig::from_document(json.as_object().expect("object"))
        };
        assert_eq!(read(serde_json::json!({})), ListenBrainzConfig::default());
        let config = read(serde_json::json!({
            "LabsServer": "https://labs.example.org",
            "Algorithm": "SessionBased75Days",
            "RateLimit": 0.25,
            "SimilarItemsCacheDays": 30,
            "AlgorithmString": "ignored"
        }));
        assert_eq!(
            config,
            ListenBrainzConfig {
                labs_server: "https://labs.example.org".to_owned(),
                algorithm: SimilarityAlgorithm::SessionBased75Days,
                rate_limit_seconds: 0.25,
                cache_days: 30,
            }
        );
        for (stored, algorithm) in [
            (
                serde_json::json!(2),
                SimilarityAlgorithm::SessionBased7500Days,
            ),
            (
                serde_json::json!("3"),
                SimilarityAlgorithm::SessionBased7500DaysHighContribution,
            ),
            (
                serde_json::json!("sessionbased9000days"),
                SimilarityAlgorithm::SessionBased9000Days,
            ),
            // Undefined: `ToApiString`'s `_` arm.
            (
                serde_json::json!(9),
                SimilarityAlgorithm::SessionBased1825Days,
            ),
            (
                serde_json::json!("Bogus"),
                SimilarityAlgorithm::SessionBased1825Days,
            ),
            (
                serde_json::json!(null),
                SimilarityAlgorithm::SessionBased1825Days,
            ),
        ] {
            assert_eq!(
                read(serde_json::json!({"Algorithm": stored, "SimilarItemsCacheDays": 3}))
                    .algorithm,
                algorithm,
                "{stored}"
            );
        }
        let config = read(serde_json::json!({
            "LabsServer": null,
            "RateLimit": "fast",
            "SimilarItemsCacheDays": 1.5,
            "Algorithm": "SessionBased9000Days"
        }));
        assert_eq!(config.server(), DEFAULT_LABS_SERVER);
        assert!((config.rate_limit_seconds - 1.0).abs() < f64::EPSILON);
        assert_eq!(config.cache_days, DEFAULT_CACHE_DAYS);
        assert_eq!(config.algorithm, SimilarityAlgorithm::SessionBased9000Days);
    }

    #[test]
    fn the_view_adds_the_chosen_algorithms_api_string() {
        let mut config =
            serde_json::json!({"Algorithm": "SessionBased75Days", "AlgorithmString": "zzz"});
        add_algorithm_string(config.as_object_mut().expect("object"));
        assert_eq!(
            config["AlgorithmString"],
            SimilarityAlgorithm::SessionBased75Days.as_api_string()
        );
        let mut config = serde_json::json!({"Algorithm": 9});
        add_algorithm_string(config.as_object_mut().expect("object"));
        assert_eq!(
            config["AlgorithmString"],
            SimilarityAlgorithm::SessionBased1825Days.as_api_string()
        );
    }

    #[test]
    fn every_algorithm_has_a_distinct_api_string() {
        let strings: std::collections::HashSet<_> = SimilarityAlgorithm::NAMED
            .iter()
            .map(|(_, a)| a.as_api_string())
            .collect();
        assert_eq!(strings.len(), SimilarityAlgorithm::NAMED.len());
        assert!(
            SimilarityAlgorithm::default()
                .as_api_string()
                .contains("1825")
        );
    }

    /// The settings are read per call: the server, the algorithm and the
    /// spacing the plugin holds now.
    #[tokio::test]
    async fn similar_artists_drops_the_seed_and_orders_by_score() {
        let body = r#"[
            {"artist_mbid":"aaaaaaaa-0000-0000-0000-000000000001","score":10.0},
            {"artist_mbid":"SEED","score":99.0},
            {"artist_mbid":"bbbbbbbb-0000-0000-0000-000000000002","score":50.0}
        ]"#;
        let server = MockServer::start(vec![(
            "/similar-artists/json?artist_mbids=seed&algorithm=session_based_days_75_",
            body.to_owned(),
        )])
        .await;
        let client = client_with(&serde_json::json!({
            "LabsServer": format!("{}/", server.base_url),
            "Algorithm": "SessionBased75Days"
        }));
        let similar = client.similar_artists("seed").await;
        assert_eq!(
            similar,
            [
                "bbbbbbbb-0000-0000-0000-000000000002",
                "aaaaaaaa-0000-0000-0000-000000000001"
            ],
            "the seed is dropped and the rest are ordered by descending score"
        );
    }

    #[tokio::test]
    async fn labs_retains_default_spacing_without_headers_and_honors_quota() {
        let body = r#"[{"artist_mbid":"other","score":1}]"#;
        let open = MockServer::always(body).await;
        let client = client_with(&serde_json::json!({"LabsServer": open.base_url}));
        let started = tokio::time::Instant::now();
        assert_eq!(client.similar_artists("seed").await, ["other"]);
        assert_eq!(client.similar_artists("seed").await, ["other"]);
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "the one-second pacing gate holds without response headers"
        );
        let limited = MockServer::with_headers(
            vec![("/", body.to_owned())],
            "X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset-In: 120\r\n",
        )
        .await;
        let client = client_with(&serde_json::json!({"LabsServer": limited.base_url}));
        assert_eq!(client.similar_artists("seed").await, ["other"]);
        let similar_artists = client.similar_artists("seed").await;
        assert!(similar_artists.is_empty(), "{similar_artists:?}");
    }

    #[test]
    fn public_labs_minimum_cannot_be_disabled() {
        for value in [0.0, 0.1, -1.0, f64::NAN, f64::INFINITY] {
            let config = ListenBrainzConfig {
                rate_limit_seconds: value,
                ..Default::default()
            };
            assert_eq!(config.request_interval(), Duration::from_secs(1));
        }
        let mirror = ListenBrainzConfig {
            labs_server: "https://labs.example.org".to_owned(),
            rate_limit_seconds: 0.0,
            ..Default::default()
        };
        assert_eq!(mirror.request_interval(), Duration::ZERO);
    }

    #[tokio::test]
    async fn an_empty_mbid_is_never_looked_up() {
        let client = client_with(&serde_json::json!({"LabsServer": "http://127.0.0.1:1"}));
        let similar_artists = client.similar_artists("  ").await;
        assert!(similar_artists.is_empty(), "{similar_artists:?}");
    }
}
