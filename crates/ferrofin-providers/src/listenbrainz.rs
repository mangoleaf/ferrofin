//! ListenBrainz Labs client — port of
//! `MediaBrowser.Providers/Plugins/ListenBrainz`.
//!
//! The Labs API answers "which artists are similar to this one" from aggregated
//! listening sessions, keyed by MusicBrainz artist id. Requests use server quota
//! headers when available, with a one-second minimum for the public Labs server.

use crate::rate_limit::RateLimiter;
use std::time::Duration;

use serde::Deserialize;

/// The default Labs API server (C# `PluginConfiguration.DefaultLabsServer`).
pub const DEFAULT_LABS_SERVER: &str = "https://labs.api.listenbrainz.org";

/// The default number of days a similar-artist result is cached
/// (C# `PluginConfiguration.SimilarItemsCacheDays`).
pub const DEFAULT_CACHE_DAYS: i64 = 14;

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
}

/// The ListenBrainz provider's settings — port of its `PluginConfiguration`.
#[derive(Debug, Clone)]
pub struct ListenBrainzConfig {
    /// The Labs API root. Empty falls back to [`DEFAULT_LABS_SERVER`].
    pub labs_server: String,
    /// The similarity algorithm to request.
    pub algorithm: SimilarityAlgorithm,
    /// Minimum request spacing in seconds; defaults to one. The public Labs
    /// server always retains at least one second, even without quota headers.
    pub rate_limit_seconds: f64,
    /// How many days a result may be cached; `0` disables caching.
    pub cache_days: i64,
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
    /// The server root with any trailing slash trimmed, falling back to the
    /// default when unset.
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

/// A ListenBrainz Labs client. Cheap to clone (shares the rate-limit gate).
#[derive(Debug, Clone)]
pub struct ListenBrainzClient {
    http: reqwest::Client,
    limiter: RateLimiter,
    config: ListenBrainzConfig,
}

impl Default for ListenBrainzClient {
    fn default() -> Self {
        Self::new(ListenBrainzConfig::default())
    }
}

impl ListenBrainzClient {
    /// A client over `config`.
    #[must_use]
    pub fn new(config: ListenBrainzConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            limiter: RateLimiter::new("listenbrainz"),
            config,
        }
    }

    /// How long a similar-artist result may be cached, or `None` when caching
    /// is disabled.
    #[must_use]
    pub fn cache_duration(&self) -> Option<Duration> {
        (self.config.cache_days > 0).then(|| {
            Duration::from_secs(u64::try_from(self.config.cache_days).unwrap_or(0) * 24 * 60 * 60)
        })
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
        let url = format!("{}/similar-artists/json", self.config.server());
        let request = self.http.get(&url).query(&[
            ("artist_mbids", mbid),
            ("algorithm", self.config.algorithm.as_api_string()),
        ]);
        let resp = match self
            .limiter
            .send_request(request, self.config.request_interval())
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
        let Ok(mut artists) = resp.json::<Vec<SimilarArtist>>().await else {
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

    #[test]
    fn an_empty_server_falls_back_to_the_default() {
        let config = ListenBrainzConfig {
            labs_server: "   ".to_owned(),
            ..ListenBrainzConfig::default()
        };
        assert_eq!(config.server(), DEFAULT_LABS_SERVER);
    }

    #[test]
    fn cache_duration_follows_the_configured_days() {
        assert_eq!(
            ListenBrainzClient::default()
                .cache_duration()
                .map(|d| d.as_secs()),
            Some(14 * 24 * 60 * 60)
        );
        let no_cache = ListenBrainzClient::new(ListenBrainzConfig {
            cache_days: 0,
            ..ListenBrainzConfig::default()
        });
        assert_eq!(no_cache.cache_duration(), None);
    }

    #[test]
    fn every_algorithm_has_a_distinct_api_string() {
        let all = [
            SimilarityAlgorithm::SessionBased1825Days,
            SimilarityAlgorithm::SessionBased1800Days,
            SimilarityAlgorithm::SessionBased7500Days,
            SimilarityAlgorithm::SessionBased7500DaysHighContribution,
            SimilarityAlgorithm::SessionBased9000Days,
            SimilarityAlgorithm::SessionBased75Days,
        ];
        let strings: std::collections::HashSet<_> = all.iter().map(|a| a.as_api_string()).collect();
        assert_eq!(strings.len(), all.len());
        assert!(
            SimilarityAlgorithm::default()
                .as_api_string()
                .contains("1825")
        );
    }

    #[tokio::test]
    async fn similar_artists_drops_the_seed_and_orders_by_score() {
        let body = r#"[
            {"artist_mbid":"aaaaaaaa-0000-0000-0000-000000000001","score":10.0},
            {"artist_mbid":"SEED","score":99.0},
            {"artist_mbid":"bbbbbbbb-0000-0000-0000-000000000002","score":50.0}
        ]"#;
        let server = MockServer::start(vec![("/similar-artists", body.to_owned())]).await;
        let client = ListenBrainzClient::new(ListenBrainzConfig {
            labs_server: server.base_url.clone(),
            ..ListenBrainzConfig::default()
        });
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
        let client = ListenBrainzClient::new(ListenBrainzConfig {
            labs_server: open.base_url.clone(),
            ..ListenBrainzConfig::default()
        });
        let started = tokio::time::Instant::now();
        assert_eq!(client.similar_artists("seed").await, ["other"]);
        assert_eq!(client.clone().similar_artists("seed").await, ["other"]);
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "clones must share the one-second pacing gate without response headers"
        );
        let limited = MockServer::with_headers(
            vec![("/", body.to_owned())],
            "X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset-In: 120\r\n",
        )
        .await;
        let client = ListenBrainzClient::new(ListenBrainzConfig {
            labs_server: limited.base_url.clone(),
            ..ListenBrainzConfig::default()
        });
        assert_eq!(client.similar_artists("seed").await, ["other"]);
        assert!(client.similar_artists("seed").await.is_empty());
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
        let client = ListenBrainzClient::new(ListenBrainzConfig {
            labs_server: "http://127.0.0.1:1".to_owned(),
            ..ListenBrainzConfig::default()
        });
        assert!(client.similar_artists("  ").await.is_empty());
    }
}
