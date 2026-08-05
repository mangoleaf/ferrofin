//! Composition-root wiring for the optional Prometheus `/metrics` endpoint.
//!
//! This module is the ONLY place that knows about `hermit-metrics`: nothing in
//! `hermit-api`/`hermit-core` changes. [`mount`] adds the `/metrics` route + the
//! HTTP tracking layer; [`spawn_sampler`] registers the async-sourced gauges and
//! drives their background mirror updates. Both run only when
//! `EnableMetrics` is set (the endpoint is 404 and no sampler exists otherwise).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use hermit_metrics::MetricsHandle;
use hermit_model::session::PlayMethod;
use hermit_traits::session::SessionManager;
use uuid::Uuid;

use hermit_db::Database;

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
        .layer(axum::middleware::from_fn(hermit_metrics::track_http))
}

/// Registers the async-/DB-sourced gauges and spawns the background sampler that
/// writes their mirror cells/maps every `interval_seconds` (≤0 → the 15 s
/// default). The sampler swallows and logs its own errors — a metrics failure
/// never disrupts the server.
pub fn spawn_sampler(
    metrics: &MetricsHandle,
    sessions: Arc<dyn SessionManager>,
    db: Database,
    interval_seconds: u32,
) {
    let sample_interval = if interval_seconds > 0 {
        std::time::Duration::from_secs(u64::from(interval_seconds))
    } else {
        DEFAULT_SAMPLE_INTERVAL
    };
    let sessions_active = metrics.gauge_cell(
        "hermit_sessions_active",
        "Number of active client sessions.",
        vec![],
    );
    let streams_active = metrics.gauge_cell(
        "hermit_playback_streams_active",
        "Number of sessions currently playing an item.",
        vec![],
    );
    let streams_by_method = metrics.gauge_map(
        "hermit_playback_streams",
        "Active playback streams by play method.",
        "method",
    );
    let transcode_active = metrics.gauge_cell(
        "hermit_transcode_jobs_active",
        "Number of sessions with an active transcode.",
        vec![],
    );
    let pool_connections = metrics.gauge_map(
        "hermit_db_pool_connections",
        "Total connections in each database pool.",
        "pool",
    );
    let pool_idle = metrics.gauge_map(
        "hermit_db_pool_idle_connections",
        "Idle connections in each database pool.",
        "pool",
    );
    let library_items = metrics.gauge_map(
        "hermit_library_items",
        "Library item count by item type.",
        "type",
    );
    let uptime = metrics.gauge_cell(
        "hermit_uptime_seconds",
        "Seconds since the metrics sampler started.",
        vec![],
    );

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
    });
}

/// Idle-connection count for one pool as an `i64` (sqlx reports `usize`).
fn idle(db: &Database, writer: bool) -> i64 {
    let pool = if writer { db.writer() } else { db.pool() };
    i64::try_from(pool.num_idle()).unwrap_or(i64::MAX)
}

/// Groups the playing sessions by `PlayMethod` into a bounded label→count map.
fn streams_by_method_counts(
    sessions: &[hermit_model::dto::SessionInfoDto],
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
