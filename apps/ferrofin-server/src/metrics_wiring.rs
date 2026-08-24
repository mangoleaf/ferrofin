//! Composition-root wiring for the optional Prometheus `/metrics` endpoint.
//!
//! This module is the ONLY place that knows about `ferrofin-metrics`: nothing in
//! `ferrofin-api`/`ferrofin-core` changes. [`mount`] adds the `/metrics` route + the
//! HTTP tracking layer; [`register_gauges`] registers the async-sourced gauges
//! (once per process — OTel instruments are process-global) and
//! [`spawn_sampler`] drives their background mirror updates for one server
//! lifetime. All run only when `EnableMetrics` is set (the endpoint is 404 and
//! no sampler exists otherwise).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use ferrofin_metrics::MetricsHandle;
use ferrofin_model::session::PlayMethod;
use ferrofin_traits::session::SessionManager;
use uuid::Uuid;

use ferrofin_db::Database;

/// Default sampler cadence when `MetricsSampleIntervalSeconds` is unset/≤0 —
/// matches the assumed Prometheus scrape interval (`contrib/metrics/prometheus.yml`,
/// 15 s). Per RULES_METRICS rule 10, keep the two aligned.
const DEFAULT_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Run the expensive library-count query only every Nth tick (≈60 s at the 15 s
/// cadence); item counts change slowly relative to the scrape rate.
const LIBRARY_COUNT_EVERY_N_TICKS: u64 = 4;

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
            "ferrofin_sessions_active",
            "Number of active client sessions.",
            vec![],
        ),
        streams_active: metrics.gauge_cell(
            "ferrofin_playback_streams_active",
            "Number of sessions currently playing an item.",
            vec![],
        ),
        streams_by_method: metrics.gauge_map(
            "ferrofin_playback_streams",
            "Active playback streams by play method.",
            "method",
        ),
        transcode_active: metrics.gauge_cell(
            "ferrofin_transcode_jobs_active",
            "Number of sessions with an active transcode.",
            vec![],
        ),
        pool_connections: metrics.gauge_map(
            "ferrofin_db_pool_connections",
            "Total connections in each database pool.",
            "pool",
        ),
        pool_idle: metrics.gauge_map(
            "ferrofin_db_pool_idle_connections",
            "Idle connections in each database pool.",
            "pool",
        ),
        library_items: metrics.gauge_map(
            "ferrofin_library_items",
            "Library item count by item type.",
            "type",
        ),
        uptime: metrics.gauge_cell(
            "ferrofin_uptime_seconds",
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

            // Session-derived gauges: one all-sessions admin snapshot per tick.
            match sessions
                .get_sessions(Uuid::nil(), None, None, None, true)
                .await
            {
                Ok(list) => {
                    let playing = list.iter().filter(|s| s.now_playing_item.is_some()).count();
                    sessions_active.set(i64::try_from(list.len()).unwrap_or(i64::MAX));
                    streams_active.set(i64::try_from(playing).unwrap_or(i64::MAX));
                    transcode_active.set(
                        i64::try_from(list.iter().filter(|s| s.transcoding_info.is_some()).count())
                            .unwrap_or(i64::MAX),
                    );
                    streams_by_method.set(streams_by_method_counts(&list));
                }
                Err(e) => tracing::debug!(error = %e, "metrics sampler: get_sessions failed"),
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

/// Groups the playing sessions by `PlayMethod` into a bounded label→count map.
fn streams_by_method_counts(
    sessions: &[ferrofin_model::dto::SessionInfoDto],
) -> HashMap<String, i64> {
    let mut counts: HashMap<String, i64> = HashMap::new();
    for session in sessions {
        if session.now_playing_item.is_none() {
            continue;
        }
        if let Some(method) = session.play_state.as_ref().and_then(|p| p.play_method) {
            *counts
                .entry(play_method_label(method).to_owned())
                .or_default() += 1;
        }
    }
    counts
}

/// The bounded `method` label value for a [`PlayMethod`] (Jellyfin's names).
fn play_method_label(method: PlayMethod) -> &'static str {
    match method {
        PlayMethod::Transcode => "Transcode",
        PlayMethod::DirectStream => "DirectStream",
        PlayMethod::DirectPlay => "DirectPlay",
    }
}
