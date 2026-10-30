//! Composition-root wiring for the optional Prometheus `/metrics` endpoint.
//!
//! This module is the ONLY place that knows about `ferrofin-metrics`: nothing in
//! `ferrofin-api`/`ferrofin-core` changes. [`mount`] adds the `/metrics` route + the
//! HTTP tracking layer; [`register_gauges`] registers the async-sourced gauges
//! (once per process — OTel instruments are process-global) and
//! [`spawn_sampler`] drives their background mirror updates for one server
//! lifetime; [`install_subsystem_instruments`] creates the instruments the
//! library scan and the metadata providers record into themselves. All run only
//! when `EnableMetrics` is set (the endpoint is 404, nothing is recorded and no
//! sampler exists otherwise).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use ferrofin_metrics::MetricsHandle;
use ferrofin_model::session::PlayMethod;
use ferrofin_traits::session::{SessionManager, SessionSummary};

use ferrofin_db::Database;

/// Default sampler cadence when `MetricsSampleIntervalSeconds` is unset/≤0 —
/// matches the assumed Prometheus scrape interval (`contrib/metrics/prometheus.yml`,
/// 15 s). Per RULES_METRICS rule 10, keep the two aligned.
const DEFAULT_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Run the expensive library-count query only every Nth tick (≈60 s at the 15 s
/// cadence); item counts change slowly relative to the scrape rate.
const LIBRARY_COUNT_EVERY_N_TICKS: u64 = 4;

/// Creates the instruments the library crates record into themselves
/// (`docs/conventions/METRICS.md` rule 8): the library-scan family
/// (`ferrofin_core::scan_metrics`) and the metadata-provider request family
/// (`ferrofin_providers::metrics`). Called once per process, right after
/// [`ferrofin_metrics::init`] installed the meter provider — an instrument
/// created before the provider exists would stay a noop. Until it runs (and
/// for good when metrics are disabled) their recording is a no-op.
///
/// `scan_duration_buckets` is the configured boundary list of the scan
/// duration histograms (`Config::metrics_scan_duration_buckets`; `None` = the
/// default).
pub fn install_subsystem_instruments(scan_duration_buckets: Option<&[f64]>) {
    ferrofin_core::scan_metrics::install(scan_duration_buckets);
    ferrofin_providers::metrics::install();
}

/// The sampler-fed gauges' names ([`register_gauges`]), for the dashboard
/// lint test.
pub const GAUGE_NAMES: &[&str] = &[
    SESSIONS_ACTIVE,
    PLAYBACK_STREAMS_ACTIVE,
    PLAYBACK_STREAMS,
    TRANSCODE_JOBS_ACTIVE,
    DB_POOL_CONNECTIONS,
    DB_POOL_IDLE_CONNECTIONS,
    LIBRARY_ITEMS,
    UPTIME_SECONDS,
];

const SESSIONS_ACTIVE: &str = "ferrofin_sessions_active";
const PLAYBACK_STREAMS_ACTIVE: &str = "ferrofin_playback_streams_active";
const PLAYBACK_STREAMS: &str = "ferrofin_playback_streams";
const TRANSCODE_JOBS_ACTIVE: &str = "ferrofin_transcode_jobs_active";
const DB_POOL_CONNECTIONS: &str = "ferrofin_db_pool_connections";
const DB_POOL_IDLE_CONNECTIONS: &str = "ferrofin_db_pool_idle_connections";
const LIBRARY_ITEMS: &str = "ferrofin_library_items";
const UPTIME_SECONDS: &str = "ferrofin_uptime_seconds";

/// Adds the `/metrics` route and the post-routing HTTP tracking layer.
///
/// The layer is added via [`Router::layer`] so it runs AFTER routing (it sees
/// `MatchedPath` + the final status). It sits inside the outermost
/// `canonicalize_path_case` wrap the caller applies later.
pub fn mount(router: Router, metrics: &MetricsHandle) -> Router {
    router
        .merge(metrics.router())
        .layer(axum::middleware::from_fn(ferrofin_metrics::track_http))
}

/// The sampler-fed gauges' mirror cells/maps. Registered ONCE per process by
/// [`register_gauges`]: an OTel observable instrument is process-global, so an
/// in-process restart re-spawns the sampler over the same cells rather than
/// registering duplicates.
#[derive(Clone)]
pub struct SamplerGauges {
    sessions_active: ferrofin_metrics::GaugeCell,
    streams_active: ferrofin_metrics::GaugeCell,
    streams_by_method: ferrofin_metrics::GaugeMap,
    transcode_active: ferrofin_metrics::GaugeCell,
    pool_connections: ferrofin_metrics::GaugeMap,
    pool_idle: ferrofin_metrics::GaugeMap,
    library_items: ferrofin_metrics::GaugeMap,
    uptime: ferrofin_metrics::GaugeCell,
}

/// Registers the async-/DB-sourced gauges on `metrics` and returns their mirrors.
#[must_use]
pub fn register_gauges(metrics: &MetricsHandle) -> SamplerGauges {
    SamplerGauges {
        sessions_active: metrics.gauge_cell(
            SESSIONS_ACTIVE,
            "Number of active client sessions.",
            vec![],
        ),
        streams_active: metrics.gauge_cell(
            PLAYBACK_STREAMS_ACTIVE,
            "Number of sessions currently playing an item.",
            vec![],
        ),
        streams_by_method: metrics.gauge_map(
            PLAYBACK_STREAMS,
            "Active playback streams by play method.",
            "method",
        ),
        transcode_active: metrics.gauge_cell(
            TRANSCODE_JOBS_ACTIVE,
            "Number of sessions with an active transcode.",
            vec![],
        ),
        pool_connections: metrics.gauge_map(
            DB_POOL_CONNECTIONS,
            "Total connections in each database pool.",
            "pool",
        ),
        pool_idle: metrics.gauge_map(
            DB_POOL_IDLE_CONNECTIONS,
            "Idle connections in each database pool.",
            "pool",
        ),
        library_items: metrics.gauge_map(LIBRARY_ITEMS, "Library item count by item type.", "type"),
        uptime: metrics.gauge_cell(
            UPTIME_SECONDS,
            "Seconds since the metrics sampler started.",
            vec![],
        ),
    }
}

/// Spawns the background sampler that writes the gauges' mirror cells/maps every
/// `interval_seconds` (≤0 → the 15 s default) from THIS server lifetime's session
/// manager and database. The sampler swallows and logs its own errors — a metrics
/// failure never disrupts the server. The returned handle lets the composition
/// root abort it when the server drains (an in-process restart spawns a fresh one).
pub fn spawn_sampler(
    gauges: SamplerGauges,
    sessions: Arc<dyn SessionManager>,
    db: Database,
    interval_seconds: u32,
) -> tokio::task::JoinHandle<()> {
    let sample_interval = if interval_seconds > 0 {
        std::time::Duration::from_secs(u64::from(interval_seconds))
    } else {
        DEFAULT_SAMPLE_INTERVAL
    };
    let SamplerGauges {
        sessions_active,
        streams_active,
        streams_by_method,
        transcode_active,
        pool_connections,
        pool_idle,
        library_items,
        uptime,
    } = gauges;

    let started = Instant::now();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(sample_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut tick: u64 = 0;
        loop {
            interval.tick().await;
            tick = tick.wrapping_add(1);

            // Uptime + DB pool gauges: cheap, every tick.
            uptime.set(i64::try_from(started.elapsed().as_secs()).unwrap_or(i64::MAX));
            pool_connections.set(HashMap::from([
                ("read".to_owned(), i64::from(db.pool().size())),
                ("write".to_owned(), i64::from(db.writer().size())),
            ]));
            pool_idle.set(HashMap::from([
                ("read".to_owned(), idle(&db, false)),
                ("write".to_owned(), idle(&db, true)),
            ]));

            // Session-derived gauges: one summary of every session per tick.
            match sessions.get_session_summaries().await {
                Ok(list) => {
                    let gauges = SessionGauges::of(&list);
                    sessions_active.set(gauges.sessions);
                    streams_active.set(gauges.streams);
                    transcode_active.set(gauges.transcodes);
                    streams_by_method.set(gauges.streams_by_method);
                }
                Err(e) => {
                    tracing::debug!(error = %e, "metrics sampler: get_session_summaries failed");
                }
            }

            // Library counts: expensive full-table group-by, only every Nth tick.
            if tick % LIBRARY_COUNT_EVERY_N_TICKS == 1 {
                match db.item_counts_by_type().await {
                    Ok(rows) => library_items.set(
                        rows.into_iter()
                            // Label = the last `.`-segment of the stored C# type name.
                            .map(|(ty, n)| (ty.rsplit('.').next().unwrap_or(&ty).to_owned(), n))
                            .collect(),
                    ),
                    Err(e) => tracing::debug!(error = %e, "metrics sampler: item counts failed"),
                }
            }
        }
    })
}

/// Idle-connection count for one pool as an `i64` (sqlx reports `usize`).
fn idle(db: &Database, writer: bool) -> i64 {
    let pool = if writer { db.writer() } else { db.pool() };
    i64::try_from(pool.num_idle()).unwrap_or(i64::MAX)
}

/// The session-derived gauge values of one sampler tick.
#[derive(Debug, PartialEq, Eq)]
struct SessionGauges {
    /// Every session.
    sessions: i64,
    /// The sessions showing a now-playing item.
    streams: i64,
    /// The sessions carrying transcoding info.
    transcodes: i64,
    /// The playing sessions by `PlayMethod`, a bounded label→count map.
    streams_by_method: HashMap<String, i64>,
}

impl SessionGauges {
    fn of(sessions: &[SessionSummary]) -> Self {
        let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        let playing = || sessions.iter().filter(|s| s.now_playing_item_id.is_some());
        let mut streams_by_method: HashMap<String, i64> = HashMap::new();
        for method in playing().filter_map(|s| s.play_method) {
            *streams_by_method
                .entry(play_method_label(method).to_owned())
                .or_default() += 1;
        }
        Self {
            sessions: count(sessions.len()),
            streams: count(playing().count()),
            transcodes: count(sessions.iter().filter(|s| s.is_transcoding).count()),
            streams_by_method,
        }
    }
}

/// The bounded `method` label value for a [`PlayMethod`] (Jellyfin's names).
fn play_method_label(method: PlayMethod) -> &'static str {
    match method {
        PlayMethod::Transcode => "Transcode",
        PlayMethod::DirectStream => "DirectStream",
        PlayMethod::DirectPlay => "DirectPlay",
        PlayMethod::Unrecognized(_) => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn session(
        now_playing: Option<u128>,
        play_method: Option<PlayMethod>,
        is_transcoding: bool,
    ) -> SessionSummary {
        SessionSummary {
            user_id: Uuid::from_u128(1),
            now_playing_item_id: now_playing.map(Uuid::from_u128),
            play_method,
            is_transcoding,
        }
    }

    /// Streams and their methods count only the sessions showing an item;
    /// transcodes count every session carrying transcoding info.
    #[test]
    fn session_gauges_count_sessions_streams_transcodes_and_methods() {
        let gauges = SessionGauges::of(&[
            session(Some(10), Some(PlayMethod::Transcode), true),
            session(Some(11), Some(PlayMethod::DirectPlay), false),
            session(Some(12), None, false),
            session(None, Some(PlayMethod::DirectStream), true),
            session(None, None, false),
        ]);
        assert_eq!(
            gauges,
            SessionGauges {
                sessions: 5,
                streams: 3,
                transcodes: 2,
                streams_by_method: HashMap::from([
                    ("Transcode".to_owned(), 1),
                    ("DirectPlay".to_owned(), 1),
                ]),
            }
        );
        assert_eq!(SessionGauges::of(&[]).streams_by_method, HashMap::new());
    }
}
