//! ListenBrainz Labs client — port of
//! `MediaBrowser.Providers/Plugins/ListenBrainz`.
//!
//! The Labs API answers "which artists are similar to this one" from aggregated
//! listening sessions, keyed by MusicBrainz artist id. Keyless, but rate-limited
//! to one request a second against the public server — a limit the client
//! enforces itself, and which cannot be lowered while the default server is in
//! use (upstream's own clamp).

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// The default Labs API server (C# `PluginConfiguration.DefaultLabsServer`).
pub const DEFAULT_LABS_SERVER: &str = "https://labs.api.listenbrainz.org";

/// The default (and minimum, against the public server) seconds between
/// requests — C# `PluginConfiguration.DefaultRateLimit`.
pub const DEFAULT_RATE_LIMIT_SECONDS: f64 = 1.0;

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
                "session_based_days_1825_session_300_contribution_5_threshold_10_limit_100_filter_True_skip_30"
            }
            Self::SessionBased1800Days => {
                "session_based_days_1800_session_300_contribution_5_threshold_10_limit_100_skip_30"
            }
            Self::SessionBased7500Days => {
                "session_based_days_7500_session_300_contribution_5_threshold_10_limit_100_filter_True_skip_30"
            }
            Self::SessionBased7500DaysHighContribution => {
                "session_based_days_7500_session_300_contribution_10_threshold_15_limit_50_filter_True_skip_30"
            }
            Self::SessionBased9000Days => {
                "session_based_days_9000_session_300_contribution_5_threshold_15_limit_50_filter_True_skip_30"
            }
            Self::SessionBased75Days => {
                "session_based_days_75_session_300_contribution_10_threshold_10_limit_100_filter_True_skip_30"
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
    /// Seconds between requests. A value below [`DEFAULT_RATE_LIMIT_SECONDS`]
    /// is ignored while the default server is in use, exactly as upstream's
    /// setter clamps it.
    pub rate_limit_seconds: f64,
    /// How many days a result may be cached; `0` disables caching.
    pub cache_days: i64,
}

impl Default for ListenBrainzConfig {
    fn default() -> Self {
        Self {
            labs_server: DEFAULT_LABS_SERVER.to_owned(),
            algorithm: SimilarityAlgorithm::default(),
            rate_limit_seconds: DEFAULT_RATE_LIMIT_SECONDS,
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

    /// The effective rate limit. The public server's floor cannot be lowered.
    #[must_use]
    pub fn effective_rate_limit(&self) -> f64 {
        if self.server() == DEFAULT_LABS_SERVER {
            self.rate_limit_seconds.max(DEFAULT_RATE_LIMIT_SECONDS)
        } else {
            self.rate_limit_seconds.max(0.0)
        }
    }
}

/// A ListenBrainz Labs client. Cheap to clone (shares the rate-limit gate).
#[derive(Debug, Clone)]
pub struct ListenBrainzClient {
    http: reqwest::Client,
    config: ListenBrainzConfig,
    /// The last request's completion time, guarding the rate limit. C# uses a
    /// `SemaphoreSlim` for the same job.
    last_request: Arc<Mutex<Option<Instant>>>,
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
            config,
            last_request: Arc::new(Mutex::new(None)),
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
        self.enforce_rate_limit().await;
        let url = format!("{}/similar-artists/json", self.config.server());
        let resp = match self
            .http
            .get(&url)
            .query(&[
                ("artist_mbids", mbid),
                ("algorithm", self.config.algorithm.as_api_string()),
            ])
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                tracing::warn!(provider = "listenbrainz", %err, "similar-artist request failed");
                return Vec::new();
            }
        };
        if !resp.status().is_success() {
            tracing::warn!(
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

    /// Waits out the configured gap since the previous request.
    async fn enforce_rate_limit(&self) {
        let gap = Duration::from_secs_f64(self.config.effective_rate_limit());
        let mut last = self.last_request.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if let Some(remaining) = gap.checked_sub(elapsed) {
                tokio::time::sleep(remaining).await;
            }
        }
        *last = Some(Instant::now());
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
    fn the_public_servers_rate_limit_floor_cannot_be_lowered() {
        // Upstream's setter refuses a sub-second limit against its own server.
        let config = ListenBrainzConfig {
            rate_limit_seconds: 0.1,
            ..ListenBrainzConfig::default()
        };
        assert!((config.effective_rate_limit() - 1.0).abs() < f64::EPSILON);

        // A self-hosted mirror may be hit as fast as the operator likes.
        let mirror = ListenBrainzConfig {
            labs_server: "https://labs.example.org/".to_owned(),
            rate_limit_seconds: 0.1,
            ..ListenBrainzConfig::default()
        };
        assert_eq!(mirror.server(), "https://labs.example.org");
        assert!((mirror.effective_rate_limit() - 0.1).abs() < f64::EPSILON);
    }

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
            rate_limit_seconds: 0.0,
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
    async fn an_empty_mbid_is_never_looked_up() {
        let client = ListenBrainzClient::new(ListenBrainzConfig {
            labs_server: "http://127.0.0.1:1".to_owned(),
            rate_limit_seconds: 0.0,
            ..ListenBrainzConfig::default()
        });
        assert!(client.similar_artists("  ").await.is_empty());
    }
}
