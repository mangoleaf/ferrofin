//! Library-scan metrics: the `ferrofin_library_scan*` family and
//! `ferrofin_media_probe_total`, rendered on `/metrics` when metrics are enabled
//! (`docs/conventions/METRICS.md`; every metric is documented in
//! `contrib/metrics/README.md` and charted by `grafana-library-scans.json`).
//!
//! Deep-internal recording (METRICS.md rule 8): the scan's outcome, its closing
//! passes and its ffprobe runs are only known inside this crate, so it records
//! them itself through the OpenTelemetry API — never through `ferrofin-metrics`
//! or the `prometheus` crate. The instruments are created exactly once, by
//! [`install`], which the composition root calls right after it installs the
//! meter provider, and only when metrics are enabled. An instrument created from
//! the global meter *before* the provider exists would stay a noop forever,
//! which is why creation waits for `install` rather than happening on first use.
//!
//! Until [`install`] runs — for the whole process when metrics are disabled —
//! every recording function below is one atomic load that finds nothing
//! installed and returns: no allocation, no label set, no clock read (rule 6).
//! Recording is per scan pass, per closing pass and per ffprobe run; nothing is
//! recorded per scanned item, so an unchanged 60,000-item rescan costs the same
//! few counter adds as a one-item one.
//!
//! Labels are bounded (rule 5): `trigger` is [`ScanTrigger`] (5 values),
//! `outcome` the four [`ScanOutcome`] counts, `result` 3 values, `pass` the 9
//! closing passes.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ferrofin_traits::library::ScanTrigger;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::library_scan::ScanOutcome;

/// Scan passes run, by `trigger` and `result`.
pub const SCANS_TOTAL: &str = "ferrofin_library_scans_total";
/// Items a scan processed, by `trigger` and `outcome`.
pub const SCAN_ITEMS_TOTAL: &str = "ferrofin_library_scan_items_total";
/// How long a completed scan pass took, by `trigger`.
pub const SCAN_DURATION_SECONDS: &str = "ferrofin_library_scan_duration_seconds";
/// Whether a scan is running (0/1).
pub const SCAN_IN_PROGRESS: &str = "ferrofin_library_scan_in_progress";
/// When the last scan pass of each `trigger` completed (unix seconds).
pub const SCAN_LAST_COMPLETED_TIMESTAMP_SECONDS: &str =
    "ferrofin_library_scan_last_completed_timestamp_seconds";
/// How long the last completed scan pass of each `trigger` took.
pub const SCAN_LAST_DURATION_SECONDS: &str = "ferrofin_library_scan_last_duration_seconds";
/// How long each closing pass of a library validation took, by `pass`.
pub const SCAN_PASS_DURATION_SECONDS: &str = "ferrofin_library_scan_pass_duration_seconds";
/// Item refreshes a running scan served from its priority lane, by `result`.
pub const SCAN_LANE_REFRESHES_TOTAL: &str = "ferrofin_library_scan_lane_refreshes_total";
/// ffprobe runs of the scan's media probe, by `result`.
pub const MEDIA_PROBE_TOTAL: &str = "ferrofin_media_probe_total";

/// Every metric name this module exposes — what the dashboard lint test checks
/// the Grafana dashboards against.
pub const METRIC_NAMES: &[&str] = &[
    SCANS_TOTAL,
    SCAN_ITEMS_TOTAL,
    SCAN_DURATION_SECONDS,
    SCAN_IN_PROGRESS,
    SCAN_LAST_COMPLETED_TIMESTAMP_SECONDS,
    SCAN_LAST_DURATION_SECONDS,
    SCAN_PASS_DURATION_SECONDS,
    SCAN_LANE_REFRESHES_TOTAL,
    MEDIA_PROBE_TOTAL,
];

/// The closing passes of a library validation, in the order they run — the
/// `pass` label set of [`SCAN_PASS_DURATION_SECONDS`] (the fields of the
/// "post-scan passes complete" log line, without their `_ms`).
pub const SCAN_PASSES: [&str; 9] = [
    "music",
    "album_covers",
    "years",
    "artists",
    "aggregates",
    "by_name_paths",
    "studios",
    "library_images",
    "dynamic_images",
];

/// The default boundaries (seconds) of both duration histograms: 1–2.5–5 per
/// decade from 1 ms to 5,000 s (83 min), 21 buckets. The bootstrap setting
/// `FERROFIN_METRICS_SCAN_DURATION_BUCKETS` / `metrics_scan_duration_buckets`
/// replaces them ([`install`]).
///
/// The range is what scans measurably take: a closing pass with nothing to do
/// is well under 10 ms, a watcher/webhook scan of one new episode 20–60 ms, an
/// unchanged rescan of the 20k-item bench corpus ~1.7 s, an unchanged prod
/// rescan over NFS 15–100 s, and the first scan of a large library (or a first
/// music pass paced at MusicBrainz's 1 request/s) tens of minutes. Three
/// buckets per decade keeps `histogram_quantile` within a factor of ~2.5
/// everywhere in that range at a bounded cost (24 series per label value).
/// Anything past 5,000 s lands in `+Inf`; the exact value of the latest one is
/// still on [`SCAN_LAST_DURATION_SECONDS`].
pub const DEFAULT_DURATION_BUCKETS: [f64; 21] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0,
    100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0,
];

/// Checks a configured bucket list: non-empty, every boundary finite and
/// greater than zero, strictly increasing.
///
/// # Errors
/// Says what is wrong with `buckets`.
pub fn validate_duration_buckets(buckets: &[f64]) -> Result<(), String> {
    if buckets.is_empty() {
        return Err("the list is empty".to_owned());
    }
    if let Some(bad) = buckets.iter().find(|b| !b.is_finite() || **b <= 0.0) {
        return Err(format!(
            "every boundary must be a finite number of seconds above zero, found {bad}"
        ));
    }
    if let Some([earlier, later, ..]) = buckets.windows(2).find(|w| w[0] >= w[1]) {
        return Err(format!(
            "the boundaries must be strictly increasing, found {earlier} then {later}"
        ));
    }
    Ok(())
}

/// The bucket boundaries to install: the configured list when it is valid,
/// else [`DEFAULT_DURATION_BUCKETS`] — with why the configured one was
/// refused. A bad value never fails startup (METRICS.md rule 6).
fn resolve_duration_buckets(configured: Option<&[f64]>) -> (Vec<f64>, Option<String>) {
    match configured {
        None => (DEFAULT_DURATION_BUCKETS.to_vec(), None),
        Some(buckets) => match validate_duration_buckets(buckets) {
            Ok(()) => (buckets.to_vec(), None),
            Err(why) => (DEFAULT_DURATION_BUCKETS.to_vec(), Some(why)),
        },
    }
}

/// How a scan pass (or a lane refresh) ended — the `result` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanEnd {
    /// It ran to its end.
    Completed,
    /// It was cancelled and stopped early.
    Stopped,
    /// It failed or panicked.
    Failed,
}

impl ScanEnd {
    /// Every result, in label order.
    const ALL: [Self; 3] = [Self::Completed, Self::Stopped, Self::Failed];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// How one ffprobe run of the scan ended — the `result` label of
/// [`MEDIA_PROBE_TOTAL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeResult {
    /// ffprobe answered.
    Ok,
    /// ffprobe failed (missing binary, unreadable or corrupt file, timeout).
    Failed,
    /// The scan stopped (or moved past the item) while ffprobe ran; the child
    /// was killed.
    Cancelled,
}

impl ProbeResult {
    const ALL: [Self; 3] = [Self::Ok, Self::Failed, Self::Cancelled];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The four item counts of a [`ScanOutcome`], in label order.
fn outcome_counts(outcome: &ScanOutcome) -> [(&'static str, usize); 4] {
    [
        ("created", outcome.created),
        ("updated", outcome.updated),
        ("unchanged", outcome.unchanged),
        ("removed", outcome.removed),
    ]
}

/// The state the observable gauges read on each scrape (rule 7: atomics only).
#[derive(Default)]
struct GaugeState {
    /// Scans running right now.
    in_progress: AtomicI64,
    /// Per trigger ([`ScanTrigger::ALL`] order): when its last pass completed,
    /// as `f64` unix-second bits; 0 means never.
    last_completed: [AtomicU64; ScanTrigger::ALL.len()],
    /// Per trigger: how long its last completed pass took, as `f64` bits.
    last_duration: [AtomicU64; ScanTrigger::ALL.len()],
}

/// The index of `trigger` in [`ScanTrigger::ALL`] (and in the gauge arrays).
fn trigger_index(trigger: ScanTrigger) -> usize {
    ScanTrigger::ALL
        .iter()
        .position(|t| *t == trigger)
        .unwrap_or_default()
}

/// The library-scan instruments. Created once per process by [`install`] (and
/// on a private meter by the unit tests).
pub(crate) struct ScanInstruments {
    scans: Counter<u64>,
    items: Counter<u64>,
    duration: Histogram<f64>,
    passes: Histogram<f64>,
    lane: Counter<u64>,
    probes: Counter<u64>,
    gauges: Arc<GaugeState>,
}

impl ScanInstruments {
    /// Creates every instrument on `meter`, both duration histograms with the
    /// `buckets` boundaries, and seeds each counter's bounded label
    /// combinations at zero, so the first scan after a start already shows as
    /// an `increase()` (a series born at its final value would not).
    pub(crate) fn new(meter: &Meter, buckets: &[f64]) -> Self {
        let gauges = Arc::new(GaugeState::default());
        let scans = meter
            .u64_counter(SCANS_TOTAL)
            .with_description("Library scan passes run, by trigger and result.")
            .build();
        let items = meter
            .u64_counter(SCAN_ITEMS_TOTAL)
            .with_description(
                "Items library scans processed, by trigger and outcome \
                 (created, updated, unchanged, removed).",
            )
            .build();
        let duration = meter
            .f64_histogram(SCAN_DURATION_SECONDS)
            .with_description("Duration of completed library scan passes, by trigger.")
            .with_boundaries(buckets.to_vec())
            .build();
        let passes = meter
            .f64_histogram(SCAN_PASS_DURATION_SECONDS)
            .with_description("Duration of each closing pass of a library validation.")
            .with_boundaries(buckets.to_vec())
            .build();
        let lane = meter
            .u64_counter(SCAN_LANE_REFRESHES_TOTAL)
            .with_description(
                "Item refreshes (refresh, Identify) a running library scan served \
                 between its own items, by result.",
            )
            .build();
        let probes = meter
            .u64_counter(MEDIA_PROBE_TOTAL)
            .with_description("ffprobe runs of the library scan's media probe, by result.")
            .build();

        let read = Arc::clone(&gauges);
        meter
            .i64_observable_gauge(SCAN_IN_PROGRESS)
            .with_description("Whether a library scan is running (1) or not (0).")
            .with_callback(move |obs| obs.observe(read.in_progress.load(Ordering::Relaxed), &[]))
            .build();
        let read = Arc::clone(&gauges);
        meter
            .f64_observable_gauge(SCAN_LAST_COMPLETED_TIMESTAMP_SECONDS)
            .with_description(
                "Unix time the last library scan pass of each trigger completed \
                 (absent until one has).",
            )
            .with_callback(move |obs| observe_per_trigger(obs, &read.last_completed))
            .build();
        let read = Arc::clone(&gauges);
        meter
            .f64_observable_gauge(SCAN_LAST_DURATION_SECONDS)
            .with_description(
                "Duration of the last completed library scan pass of each trigger \
                 (absent until one has).",
            )
            .with_callback(move |obs| observe_per_trigger(obs, &read.last_duration))
            .build();

        for trigger in ScanTrigger::ALL {
            for end in ScanEnd::ALL {
                scans.add(0, &scan_labels(trigger, end));
            }
            for (outcome, _) in outcome_counts(&ScanOutcome::default()) {
                items.add(0, &item_labels(trigger, outcome));
            }
        }
        for end in ScanEnd::ALL {
            lane.add(0, &[KeyValue::new("result", end.as_str())]);
        }
        for result in ProbeResult::ALL {
            probes.add(0, &[KeyValue::new("result", result.as_str())]);
        }

        Self {
            scans,
            items,
            duration,
            passes,
            lane,
            probes,
            gauges,
        }
    }

    /// Records one scan pass the queue's worker ran: its result, the items it
    /// processed (a stopped pass's partial counts included; a failed one has
    /// none) and, for a completed pass, its duration and completion time.
    fn scan_finished(
        &self,
        trigger: ScanTrigger,
        end: ScanEnd,
        outcome: Option<&ScanOutcome>,
        elapsed: Duration,
    ) {
        self.scans.add(1, &scan_labels(trigger, end));
        if let Some(outcome) = outcome {
            self.add_items(trigger, outcome);
        }
        if end == ScanEnd::Completed {
            let secs = elapsed.as_secs_f64();
            self.duration
                .record(secs, &[KeyValue::new("trigger", trigger.as_str())]);
            let index = trigger_index(trigger);
            self.gauges.last_duration[index].store(secs.to_bits(), Ordering::Relaxed);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            self.gauges.last_completed[index].store(now.to_bits(), Ordering::Relaxed);
        }
    }

    /// Records one item refresh a running scan served from its lane.
    fn lane_refresh_finished(
        &self,
        trigger: ScanTrigger,
        end: ScanEnd,
        outcome: Option<&ScanOutcome>,
    ) {
        self.lane.add(1, &[KeyValue::new("result", end.as_str())]);
        if let Some(outcome) = outcome {
            self.add_items(trigger, outcome);
        }
    }

    fn add_items(&self, trigger: ScanTrigger, outcome: &ScanOutcome) {
        for (label, count) in outcome_counts(outcome) {
            self.items.add(
                u64::try_from(count).unwrap_or(u64::MAX),
                &item_labels(trigger, label),
            );
        }
    }

    /// Records the closing passes of one completed library validation.
    fn passes_finished(&self, timings: &[(&'static str, Duration); 9]) {
        for (pass, elapsed) in timings {
            self.passes
                .record(elapsed.as_secs_f64(), &[KeyValue::new("pass", *pass)]);
        }
    }

    fn probe_finished(&self, result: ProbeResult) {
        self.probes
            .add(1, &[KeyValue::new("result", result.as_str())]);
    }
}

fn scan_labels(trigger: ScanTrigger, end: ScanEnd) -> [KeyValue; 2] {
    [
        KeyValue::new("trigger", trigger.as_str()),
        KeyValue::new("result", end.as_str()),
    ]
}

fn item_labels(trigger: ScanTrigger, outcome: &'static str) -> [KeyValue; 2] {
    [
        KeyValue::new("trigger", trigger.as_str()),
        KeyValue::new("outcome", outcome),
    ]
}

/// Observes one series per trigger whose slot holds a value (non-zero bits).
fn observe_per_trigger(
    obs: &dyn opentelemetry::metrics::AsyncInstrument<f64>,
    slots: &[AtomicU64],
) {
    for (trigger, slot) in ScanTrigger::ALL.iter().zip(slots) {
        let bits = slot.load(Ordering::Relaxed);
        if bits != 0 {
            obs.observe(
                f64::from_bits(bits),
                &[KeyValue::new("trigger", trigger.as_str())],
            );
        }
    }
}

/// The process-wide instruments, set once by [`install`].
static INSTRUMENTS: OnceLock<ScanInstruments> = OnceLock::new();

/// Creates the library-scan instruments on the global meter provider. The
/// composition root calls it once, right after it installs the provider — only
/// when metrics are enabled. A second call is a no-op.
///
/// `duration_buckets` is the configured boundary list of both duration
/// histograms (`None` = [`DEFAULT_DURATION_BUCKETS`]). An invalid list
/// ([`validate_duration_buckets`]) is refused with a warning and the default
/// is used — a metrics setting never stops the server.
pub fn install(duration_buckets: Option<&[f64]>) {
    INSTRUMENTS.get_or_init(|| {
        let (buckets, refused) = resolve_duration_buckets(duration_buckets);
        if let Some(why) = refused {
            tracing::warn!(
                reason = %why,
                "ignoring the configured scan duration buckets \
                 (FERROFIN_METRICS_SCAN_DURATION_BUCKETS / metrics_scan_duration_buckets); \
                 using the default"
            );
        }
        ScanInstruments::new(&opentelemetry::global::meter("ferrofin"), &buckets)
    });
}

/// Records one scan pass the queue's worker ran (see
/// [`ScanInstruments::scan_finished`]). A no-op until [`install`].
pub(crate) fn scan_finished(
    trigger: ScanTrigger,
    end: ScanEnd,
    outcome: Option<&ScanOutcome>,
    elapsed: Duration,
) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.scan_finished(trigger, end, outcome, elapsed);
    }
}

/// Records one item refresh a running scan served from its lane. A no-op
/// until [`install`].
pub(crate) fn lane_refresh_finished(
    trigger: ScanTrigger,
    end: ScanEnd,
    outcome: Option<&ScanOutcome>,
) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.lane_refresh_finished(trigger, end, outcome);
    }
}

/// Records the closing passes of one completed library validation, in
/// [`SCAN_PASSES`] order. A no-op until [`install`].
pub(crate) fn passes_finished(timings: &[(&'static str, Duration); 9]) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.passes_finished(timings);
    }
}

/// Marks a scan as running for as long as it lives
/// ([`SCAN_IN_PROGRESS`]). Taken only while the instruments are installed, so
/// a scan that began before [`install`] never drives the gauge below zero.
pub(crate) struct ScanInProgress(Option<&'static ScanInstruments>);

impl ScanInProgress {
    /// Counts one running scan until the guard drops.
    pub(crate) fn enter() -> Self {
        Self::enter_on(INSTRUMENTS.get())
    }

    fn enter_on(installed: Option<&'static ScanInstruments>) -> Self {
        if let Some(instruments) = installed {
            instruments
                .gauges
                .in_progress
                .fetch_add(1, Ordering::Relaxed);
        }
        Self(installed)
    }
}

impl Drop for ScanInProgress {
    fn drop(&mut self) {
        if let Some(instruments) = self.0 {
            instruments
                .gauges
                .in_progress
                .fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// One ffprobe run of the scan's media probe. Dropped without
/// [`finish`](Self::finish) — the probe task aborted while ffprobe ran — it
/// counts as [`ProbeResult::Cancelled`].
pub(crate) struct ProbeRun(Option<&'static ScanInstruments>);

impl ProbeRun {
    /// Starts counting one ffprobe run. A no-op until [`install`].
    pub(crate) fn start() -> Self {
        Self(INSTRUMENTS.get())
    }

    #[cfg(test)]
    fn start_on(instruments: &'static ScanInstruments) -> Self {
        Self(Some(instruments))
    }

    /// Records how the run ended.
    pub(crate) fn finish(mut self, result: ProbeResult) {
        if let Some(instruments) = self.0.take() {
            instruments.probe_finished(result);
        }
    }
}

impl Drop for ProbeRun {
    fn drop(&mut self) {
        if let Some(instruments) = self.0.take() {
            instruments.probe_finished(ProbeResult::Cancelled);
        }
    }
}

/// Whether [`install`] has run (the disabled-metrics tests assert it has not).
#[cfg(test)]
pub(crate) fn installed() -> bool {
    INSTRUMENTS.get().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    /// A private meter provider (no process global touched) whose exports
    /// land in the returned exporter on `force_flush`.
    fn provider() -> (SdkMeterProvider, InMemoryMetricExporter) {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        (provider, exporter)
    }

    /// One exported data point: its metric name, sorted labels and value
    /// (a histogram's count).
    #[derive(Debug, PartialEq)]
    struct Point {
        name: String,
        labels: Vec<(String, String)>,
        value: f64,
    }

    fn labels<'a>(attrs: impl Iterator<Item = &'a KeyValue>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = attrs
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect();
        out.sort();
        out
    }

    /// Flushes `provider` and flattens the last export into points.
    #[allow(clippy::cast_precision_loss)]
    fn collect(provider: &SdkMeterProvider, exporter: &InMemoryMetricExporter) -> Vec<Point> {
        provider.force_flush().expect("flush");
        let finished: Vec<ResourceMetrics> = exporter.get_finished_metrics().expect("metrics");
        let last = finished.last().expect("one export");
        let mut points = Vec::new();
        for scope in last.scope_metrics() {
            for metric in scope.metrics() {
                let name = metric.name().to_owned();
                match metric.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                        for p in sum.data_points() {
                            points.push(Point {
                                name: name.clone(),
                                labels: labels(p.attributes()),
                                value: p.value() as f64,
                            });
                        }
                    }
                    AggregatedMetrics::I64(MetricData::Gauge(gauge)) => {
                        for p in gauge.data_points() {
                            points.push(Point {
                                name: name.clone(),
                                labels: labels(p.attributes()),
                                value: p.value() as f64,
                            });
                        }
                    }
                    AggregatedMetrics::F64(MetricData::Gauge(gauge)) => {
                        for p in gauge.data_points() {
                            points.push(Point {
                                name: name.clone(),
                                labels: labels(p.attributes()),
                                value: p.value(),
                            });
                        }
                    }
                    AggregatedMetrics::F64(MetricData::Histogram(hist)) => {
                        for p in hist.data_points() {
                            points.push(Point {
                                name: name.clone(),
                                labels: labels(p.attributes()),
                                value: p.count() as f64,
                            });
                        }
                    }
                    other => panic!("unexpected aggregation for {name}: {other:?}"),
                }
            }
        }
        points
    }

    fn value(points: &[Point], name: &str, want: &[(&str, &str)]) -> Option<f64> {
        let mut want: Vec<(String, String)> = want
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        want.sort();
        points
            .iter()
            .find(|p| p.name == name && p.labels == want)
            .map(|p| p.value)
    }

    #[test]
    fn every_counter_series_starts_at_zero() {
        let (provider, exporter) = provider();
        let _instruments = ScanInstruments::new(&provider.meter("test"), &DEFAULT_DURATION_BUCKETS);
        let points = collect(&provider, &exporter);
        for trigger in ScanTrigger::ALL {
            for result in ["completed", "stopped", "failed"] {
                assert_eq!(
                    value(
                        &points,
                        SCANS_TOTAL,
                        &[("trigger", trigger.as_str()), ("result", result)]
                    ),
                    Some(0.0),
                    "{trigger} {result}"
                );
            }
            for outcome in ["created", "updated", "unchanged", "removed"] {
                assert_eq!(
                    value(
                        &points,
                        SCAN_ITEMS_TOTAL,
                        &[("trigger", trigger.as_str()), ("outcome", outcome)]
                    ),
                    Some(0.0),
                    "{trigger} {outcome}"
                );
            }
        }
        for result in ["ok", "failed", "cancelled"] {
            assert_eq!(
                value(&points, MEDIA_PROBE_TOTAL, &[("result", result)]),
                Some(0.0)
            );
        }
        assert_eq!(value(&points, SCAN_IN_PROGRESS, &[]), Some(0.0));
        // Nothing completed yet: no last-scan series at all.
        assert!(
            !points
                .iter()
                .any(|p| p.name == SCAN_LAST_COMPLETED_TIMESTAMP_SECONDS
                    || p.name == SCAN_LAST_DURATION_SECONDS)
        );
    }

    #[test]
    fn a_completed_scan_records_its_items_duration_and_completion() {
        let (provider, exporter) = provider();
        let instruments = ScanInstruments::new(&provider.meter("test"), &DEFAULT_DURATION_BUCKETS);
        let outcome = ScanOutcome {
            created: 3,
            updated: 1,
            unchanged: 40,
            removed: 2,
            stopped: false,
        };
        instruments.scan_finished(
            ScanTrigger::Schedule,
            ScanEnd::Completed,
            Some(&outcome),
            Duration::from_millis(1500),
        );
        let points = collect(&provider, &exporter);
        let schedule = ("trigger", "schedule");
        assert_eq!(
            value(&points, SCANS_TOTAL, &[schedule, ("result", "completed")]),
            Some(1.0)
        );
        for (outcome, count) in [
            ("created", 3.0),
            ("updated", 1.0),
            ("unchanged", 40.0),
            ("removed", 2.0),
        ] {
            assert_eq!(
                value(&points, SCAN_ITEMS_TOTAL, &[schedule, ("outcome", outcome)]),
                Some(count),
                "{outcome}"
            );
        }
        assert_eq!(
            value(&points, SCAN_DURATION_SECONDS, &[schedule]),
            Some(1.0)
        );
        assert_eq!(
            value(&points, SCAN_LAST_DURATION_SECONDS, &[schedule]),
            Some(1.5)
        );
        let completed = value(&points, SCAN_LAST_COMPLETED_TIMESTAMP_SECONDS, &[schedule])
            .expect("completion time");
        assert!(completed > 1.7e9, "unix seconds, got {completed}");
        // Other triggers never completed: no series.
        assert_eq!(
            value(&points, SCAN_LAST_DURATION_SECONDS, &[("trigger", "api")]),
            None
        );
    }

    #[test]
    fn stopped_and_failed_scans_are_counted_but_not_timed() {
        let (provider, exporter) = provider();
        let instruments = ScanInstruments::new(&provider.meter("test"), &DEFAULT_DURATION_BUCKETS);
        let partial = ScanOutcome {
            created: 2,
            stopped: true,
            ..ScanOutcome::default()
        };
        instruments.scan_finished(
            ScanTrigger::Api,
            ScanEnd::Stopped,
            Some(&partial),
            Duration::from_secs(3),
        );
        instruments.scan_finished(
            ScanTrigger::Webhook,
            ScanEnd::Failed,
            None,
            Duration::from_secs(1),
        );
        let points = collect(&provider, &exporter);
        assert_eq!(
            value(
                &points,
                SCANS_TOTAL,
                &[("trigger", "api"), ("result", "stopped")]
            ),
            Some(1.0)
        );
        assert_eq!(
            value(
                &points,
                SCANS_TOTAL,
                &[("trigger", "webhook"), ("result", "failed")]
            ),
            Some(1.0)
        );
        assert_eq!(
            value(
                &points,
                SCAN_ITEMS_TOTAL,
                &[("trigger", "api"), ("outcome", "created")]
            ),
            Some(2.0)
        );
        assert!(!points.iter().any(|p| p.name == SCAN_DURATION_SECONDS));
        assert!(!points.iter().any(|p| p.name == SCAN_LAST_DURATION_SECONDS));
    }

    #[test]
    fn lane_refreshes_count_their_items_under_their_trigger() {
        let (provider, exporter) = provider();
        let instruments = ScanInstruments::new(&provider.meter("test"), &DEFAULT_DURATION_BUCKETS);
        let outcome = ScanOutcome {
            updated: 1,
            ..ScanOutcome::default()
        };
        instruments.lane_refresh_finished(ScanTrigger::Api, ScanEnd::Completed, Some(&outcome));
        instruments.lane_refresh_finished(ScanTrigger::Api, ScanEnd::Failed, None);
        let points = collect(&provider, &exporter);
        assert_eq!(
            value(
                &points,
                SCAN_LANE_REFRESHES_TOTAL,
                &[("result", "completed")]
            ),
            Some(1.0)
        );
        assert_eq!(
            value(&points, SCAN_LANE_REFRESHES_TOTAL, &[("result", "failed")]),
            Some(1.0)
        );
        assert_eq!(
            value(
                &points,
                SCAN_ITEMS_TOTAL,
                &[("trigger", "api"), ("outcome", "updated")]
            ),
            Some(1.0)
        );
        // A lane refresh is not a scan pass.
        assert!(
            points
                .iter()
                .filter(|p| p.name == SCANS_TOTAL)
                .all(|p| p.value == 0.0)
        );
    }

    #[test]
    fn closing_passes_and_probes_record_their_labels() {
        let (provider, exporter) = provider();
        let instruments = ScanInstruments::new(&provider.meter("test"), &DEFAULT_DURATION_BUCKETS);
        let timings = SCAN_PASSES.map(|pass| (pass, Duration::from_millis(5)));
        instruments.passes_finished(&timings);
        instruments.probe_finished(ProbeResult::Ok);
        instruments.probe_finished(ProbeResult::Ok);
        instruments.probe_finished(ProbeResult::Failed);
        instruments.probe_finished(ProbeResult::Cancelled);
        let points = collect(&provider, &exporter);
        for pass in SCAN_PASSES {
            assert_eq!(
                value(&points, SCAN_PASS_DURATION_SECONDS, &[("pass", pass)]),
                Some(1.0),
                "{pass}"
            );
        }
        assert_eq!(
            value(&points, MEDIA_PROBE_TOTAL, &[("result", "ok")]),
            Some(2.0)
        );
        assert_eq!(
            value(&points, MEDIA_PROBE_TOTAL, &[("result", "failed")]),
            Some(1.0)
        );
        assert_eq!(
            value(&points, MEDIA_PROBE_TOTAL, &[("result", "cancelled")]),
            Some(1.0)
        );
    }

    #[test]
    fn the_exported_names_are_exactly_metric_names() {
        let (provider, exporter) = provider();
        let instruments = ScanInstruments::new(&provider.meter("test"), &DEFAULT_DURATION_BUCKETS);
        instruments.scan_finished(
            ScanTrigger::Watcher,
            ScanEnd::Completed,
            Some(&ScanOutcome::default()),
            Duration::from_millis(40),
        );
        instruments.passes_finished(&SCAN_PASSES.map(|pass| (pass, Duration::ZERO)));
        let mut names: Vec<String> = collect(&provider, &exporter)
            .into_iter()
            .map(|p| p.name)
            .collect();
        names.sort();
        names.dedup();
        let mut declared: Vec<String> = METRIC_NAMES.iter().map(|n| (*n).to_owned()).collect();
        declared.sort();
        assert_eq!(names, declared);
    }

    #[test]
    fn nothing_is_recorded_until_install() {
        // This binary never calls `install`: every entry point is inert, and
        // the guards neither count nor underflow.
        assert!(!installed());
        scan_finished(
            ScanTrigger::Api,
            ScanEnd::Completed,
            Some(&ScanOutcome::default()),
            Duration::from_secs(1),
        );
        lane_refresh_finished(ScanTrigger::Api, ScanEnd::Completed, None);
        passes_finished(&SCAN_PASSES.map(|pass| (pass, Duration::ZERO)));
        drop(ScanInProgress::enter());
        ProbeRun::start().finish(ProbeResult::Ok);
        drop(ProbeRun::start());
        assert!(!installed());
    }

    #[test]
    fn the_guards_drive_the_in_progress_gauge_and_count_a_dropped_probe_as_cancelled() {
        let (provider, exporter) = provider();
        let instruments: &'static ScanInstruments = Box::leak(Box::new(ScanInstruments::new(
            &provider.meter("test"),
            &DEFAULT_DURATION_BUCKETS,
        )));
        let running = ScanInProgress::enter_on(Some(instruments));
        let points = collect(&provider, &exporter);
        assert_eq!(value(&points, SCAN_IN_PROGRESS, &[]), Some(1.0));
        drop(running);
        ProbeRun::start_on(instruments).finish(ProbeResult::Ok);
        drop(ProbeRun::start_on(instruments));
        let points = collect(&provider, &exporter);
        assert_eq!(value(&points, SCAN_IN_PROGRESS, &[]), Some(0.0));
        assert_eq!(
            value(&points, MEDIA_PROBE_TOTAL, &[("result", "ok")]),
            Some(1.0)
        );
        assert_eq!(
            value(&points, MEDIA_PROBE_TOTAL, &[("result", "cancelled")]),
            Some(1.0)
        );
    }

    #[test]
    fn the_default_buckets_are_valid() {
        assert_eq!(validate_duration_buckets(&DEFAULT_DURATION_BUCKETS), Ok(()));
    }

    #[test]
    fn a_configured_bucket_list_is_validated_and_a_bad_one_falls_back() {
        assert_eq!(
            resolve_duration_buckets(None),
            (DEFAULT_DURATION_BUCKETS.to_vec(), None)
        );
        assert_eq!(
            resolve_duration_buckets(Some(&[0.5, 30.0, 3600.0])),
            (vec![0.5, 30.0, 3600.0], None)
        );
        for (bad, why) in [
            (vec![], "empty"),
            (vec![1.0, f64::NAN], "finite"),
            (vec![1.0, f64::INFINITY], "finite"),
            (vec![0.0, 1.0], "above zero"),
            (vec![-1.0, 1.0], "above zero"),
            (vec![1.0, 1.0], "strictly increasing"),
            (vec![5.0, 1.0], "strictly increasing"),
        ] {
            let (buckets, refused) = resolve_duration_buckets(Some(&bad));
            assert_eq!(buckets, DEFAULT_DURATION_BUCKETS.to_vec(), "{bad:?}");
            let refused = refused.unwrap_or_else(|| panic!("{bad:?} accepted"));
            assert!(refused.contains(why), "{bad:?}: {refused}");
        }
    }

    #[test]
    fn the_configured_buckets_are_the_histograms_boundaries() {
        let (provider, exporter) = provider();
        let instruments = ScanInstruments::new(&provider.meter("test"), &[0.5, 30.0]);
        instruments.scan_finished(
            ScanTrigger::Api,
            ScanEnd::Completed,
            None,
            Duration::from_secs(1),
        );
        instruments.passes_finished(&SCAN_PASSES.map(|pass| (pass, Duration::ZERO)));
        provider.force_flush().expect("flush");
        let finished = exporter.get_finished_metrics().expect("metrics");
        let mut histograms = 0;
        for scope in finished.last().expect("one export").scope_metrics() {
            for metric in scope.metrics() {
                if let AggregatedMetrics::F64(MetricData::Histogram(hist)) = metric.data() {
                    for point in hist.data_points() {
                        histograms += 1;
                        assert_eq!(point.bounds().collect::<Vec<_>>(), vec![0.5, 30.0]);
                    }
                }
            }
        }
        assert_eq!(histograms, 1 + SCAN_PASSES.len());
    }
}
