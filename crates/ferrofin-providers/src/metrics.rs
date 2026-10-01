//! Metadata-provider request metrics: `ferrofin_metadata_provider_requests_total`
//! and `ferrofin_metadata_provider_retries_total`, recorded at the one chokepoint
//! every remote provider call passes — the [`RateLimiter`](crate::rate_limit::RateLimiter)
//! (`docs/conventions/METRICS.md`; documented in `contrib/metrics/README.md`).
//!
//! Deep-internal recording (METRICS.md rule 8): this crate names only the
//! OpenTelemetry API. The instruments are created exactly once, by [`install`],
//! which the composition root calls right after it installs the meter provider
//! (only when metrics are enabled); until then every recording function is one
//! atomic load that finds nothing installed and returns.
//!
//! Labels are bounded (rule 5): `provider` is one of [`PROVIDERS`] — a limiter
//! named anything else (a test's, a future provider's before it is added here)
//! is `other`, and every image-CDN origin is `image` — and `result` one of
//! four values.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Meter};

/// Logical provider requests, by `provider` and `result`.
pub const PROVIDER_REQUESTS_TOTAL: &str = "ferrofin_metadata_provider_requests_total";
/// Retry attempts beyond each request's first, by `provider`.
pub const PROVIDER_RETRIES_TOTAL: &str = "ferrofin_metadata_provider_retries_total";

/// Every metric name this module exposes — what the dashboard lint test checks
/// the Grafana dashboards against.
pub const METRIC_NAMES: &[&str] = &[PROVIDER_REQUESTS_TOTAL, PROVIDER_RETRIES_TOTAL];

/// The built-in providers' limiter names — the `provider` label values, with
/// `image` (artwork downloads, one limiter per CDN origin) and `other`.
pub const PROVIDERS: [&str; 12] = [
    "tmdb",
    "tvdb",
    "musicbrainz",
    "audiodb",
    "fanart",
    "omdb",
    "studios",
    "opensubtitles",
    "lrclib",
    "listenbrainz",
    IMAGE,
    OTHER,
];

/// The `provider` label of every artwork download.
pub(crate) const IMAGE: &str = "image";
/// The `provider` label of a limiter no built-in provider owns.
const OTHER: &str = "other";

/// The bounded `provider` label for a limiter named `name`.
pub(crate) fn provider_label(name: &str) -> &'static str {
    PROVIDERS
        .iter()
        .find(|known| **known == name)
        .copied()
        .unwrap_or(OTHER)
}

/// How one logical provider request ended — the `result` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestResult {
    /// A success status.
    Ok,
    /// A 404: the provider answered "nothing here".
    NotFound,
    /// A transport error, an exhausted retry, or any other non-success status.
    Failed,
    /// Never sent: the provider's circuit is open or its cooldown is longer
    /// than the caller may wait.
    Skipped,
}

impl RequestResult {
    const ALL: [Self; 4] = [Self::Ok, Self::NotFound, Self::Failed, Self::Skipped];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotFound => "not_found",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// The provider-request instruments.
pub(crate) struct ProviderInstruments {
    requests: Counter<u64>,
    retries: Counter<u64>,
}

impl ProviderInstruments {
    /// Creates both counters on `meter`, every bounded label combination
    /// seeded at zero (so a provider's first request after a start shows as an
    /// `increase()`).
    pub(crate) fn new(meter: &Meter) -> Self {
        let requests = meter
            .u64_counter(PROVIDER_REQUESTS_TOTAL)
            .with_description(
                "Metadata provider requests (one per logical request, retries \
                 excluded), by provider and result.",
            )
            .build();
        let retries = meter
            .u64_counter(PROVIDER_RETRIES_TOTAL)
            .with_description("Metadata provider retry attempts, by provider.")
            .build();
        for provider in PROVIDERS {
            for result in RequestResult::ALL {
                requests.add(0, &request_labels(provider, result));
            }
            retries.add(0, &[KeyValue::new("provider", provider)]);
        }
        Self { requests, retries }
    }

    fn request_finished(&self, provider: &'static str, result: RequestResult) {
        self.requests.add(1, &request_labels(provider, result));
    }

    fn retried(&self, provider: &'static str) {
        self.retries.add(1, &[KeyValue::new("provider", provider)]);
    }
}

fn request_labels(provider: &'static str, result: RequestResult) -> [KeyValue; 2] {
    [
        KeyValue::new("provider", provider),
        KeyValue::new("result", result.as_str()),
    ]
}

/// The process-wide instruments, set once by [`install`].
static INSTRUMENTS: OnceLock<ProviderInstruments> = OnceLock::new();

/// Creates the provider-request instruments on the global meter provider. The
/// composition root calls it once, right after it installs the provider — only
/// when metrics are enabled. A second call is a no-op.
pub fn install() {
    INSTRUMENTS.get_or_init(|| ProviderInstruments::new(&opentelemetry::global::meter("ferrofin")));
}

/// Records one logical provider request. A no-op until [`install`].
pub(crate) fn request_finished(provider: &'static str, result: RequestResult) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.request_finished(provider, result);
    }
}

/// Records one retry attempt. A no-op until [`install`].
pub(crate) fn retried(provider: &'static str) {
    if let Some(instruments) = INSTRUMENTS.get() {
        instruments.retried(provider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    /// One exported counter point: `(name, sorted labels, value)`.
    type Point = (String, Vec<(String, String)>, u64);

    /// Flushes a private provider and returns every exported counter point.
    fn points(provider: &SdkMeterProvider, exporter: &InMemoryMetricExporter) -> Vec<Point> {
        provider.force_flush().expect("flush");
        let finished = exporter.get_finished_metrics().expect("metrics");
        let mut out = Vec::new();
        for scope in finished.last().expect("one export").scope_metrics() {
            for metric in scope.metrics() {
                let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                    panic!("{} is not a u64 counter", metric.name());
                };
                for p in sum.data_points() {
                    let mut labels: Vec<(String, String)> = p
                        .attributes()
                        .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                        .collect();
                    labels.sort();
                    out.push((metric.name().to_owned(), labels, p.value()));
                }
            }
        }
        out
    }

    fn setup() -> (
        SdkMeterProvider,
        InMemoryMetricExporter,
        ProviderInstruments,
    ) {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let instruments = ProviderInstruments::new(&provider.meter("test"));
        (provider, exporter, instruments)
    }

    fn find(points: &[Point], name: &str, labels: &[(&str, &str)]) -> Option<u64> {
        let mut want: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        want.sort();
        points
            .iter()
            .find(|(n, l, _)| n == name && *l == want)
            .map(|(_, _, v)| *v)
    }

    #[test]
    fn provider_labels_are_bounded() {
        assert_eq!(provider_label("tmdb"), "tmdb");
        assert_eq!(provider_label("musicbrainz"), "musicbrainz");
        assert_eq!(provider_label("https://image.tmdb.org"), "other");
        assert_eq!(provider_label("quota-test"), "other");
        assert_eq!(provider_label(""), "other");
    }

    #[test]
    fn requests_and_retries_record_their_labels() {
        let (provider, exporter, instruments) = setup();
        instruments.request_finished("tmdb", RequestResult::Ok);
        instruments.request_finished("tmdb", RequestResult::Ok);
        instruments.request_finished("tvdb", RequestResult::NotFound);
        instruments.request_finished("musicbrainz", RequestResult::Failed);
        instruments.request_finished(IMAGE, RequestResult::Skipped);
        instruments.retried("musicbrainz");
        let points = points(&provider, &exporter);
        let req = |p: &str, r: &str| {
            find(
                &points,
                PROVIDER_REQUESTS_TOTAL,
                &[("provider", p), ("result", r)],
            )
        };
        assert_eq!(req("tmdb", "ok"), Some(2));
        assert_eq!(req("tvdb", "not_found"), Some(1));
        assert_eq!(req("musicbrainz", "failed"), Some(1));
        assert_eq!(req("image", "skipped"), Some(1));
        // Seeded at zero, never touched.
        assert_eq!(req("fanart", "ok"), Some(0));
        assert_eq!(
            find(
                &points,
                PROVIDER_RETRIES_TOTAL,
                &[("provider", "musicbrainz")]
            ),
            Some(1)
        );
        // 12 providers × 4 results + 12 retry series, nothing else.
        assert_eq!(points.len(), PROVIDERS.len() * 5);
        let mut names: Vec<&str> = points.iter().map(|(n, _, _)| n.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names, METRIC_NAMES);
    }

    #[test]
    fn nothing_is_recorded_until_install() {
        // This binary never calls `install`.
        assert!(INSTRUMENTS.get().is_none());
        request_finished("tmdb", RequestResult::Ok);
        retried("tmdb");
        assert!(INSTRUMENTS.get().is_none());
    }
}
