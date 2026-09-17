//! Shared request pacing and retry handling for metadata providers.
//!
//! Create one limiter per provider quota (usually API host/account), and clone
//! it for callers sharing that quota. Clones share state; unrelated instances
//! are independent. Callers supply their own request headers, credentials and
//! minimum interval. Only replayable GET requests are retried; other methods
//! are sent once while sharing quota/cooldown tracking.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

const GET_ATTEMPTS: u32 = 4;
const BACKOFF_CAP: Duration = Duration::from_mins(1);
const OPEN_AFTER_FAILURES: u32 = 6;
const RECOVERY_SUCCESSES: u32 = 5;
const RECOVERY_QUIET: Duration = Duration::from_mins(1);

#[derive(Debug, Clone, Copy)]
struct Settings {
    timeout: Duration,
    max_wait: Duration,
}

impl Settings {
    fn from_env() -> Self {
        Self {
            timeout: seconds_setting(
                std::env::var("FERROFIN_PROVIDER_TIMEOUT_SECONDS")
                    .ok()
                    .as_deref(),
                20,
                10,
                60,
            ),
            max_wait: seconds_setting(
                std::env::var("FERROFIN_PROVIDER_MAX_WAIT_SECONDS")
                    .ok()
                    .as_deref(),
                60,
                30,
                300,
            ),
        }
    }
}

fn seconds_setting(value: Option<&str>, default: u64, min: u64, max: u64) -> Duration {
    Duration::from_secs(
        value
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default)
            .clamp(min, max),
    )
}

// Only allow known non-secret identifiers. Some providers put API keys or
// signed tokens in URL paths, so never log arbitrary paths or whole queries.
fn request_context(provider: &str, url: &reqwest::Url) -> String {
    match provider {
        "musicbrainz" => url.path().to_owned(),
        "omdb" => url
            .query_pairs()
            .find(|(key, _)| key == "i")
            .map_or_else(|| "search".to_owned(), |(_, id)| id.into_owned()),
        _ => "lookup".to_owned(),
    }
}

/// A shared gate for metadata GETs, including retries and server cooldowns.
///
/// Honors `Retry-After` and exhausted `X-RateLimit-Remaining`/epoch
/// `X-RateLimit-Reset` headers. Transient failures (408, 429, 500, 502, 503,
/// 504 and request errors) receive at most four GET attempts with exponential
/// backoff and jitter. Connection failures open the circuit after one attempt.
/// State persists between calls, including after cancellation.
/// This coordinates callers within one process, not other clients sharing its IP.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    provider: Arc<str>,
    settings: Settings,
    state: Arc<Mutex<RequestRate>>,
}

impl RateLimiter {
    /// Creates an independent quota gate. Clone it to share cooldowns.
    #[must_use]
    pub fn new(provider: impl Into<Arc<str>>) -> Self {
        Self {
            provider: provider.into(),
            settings: Settings::from_env(),
            state: Arc::default(),
        }
    }

    /// Executes a GET with the caller's minimum interval and bounded retries.
    ///
    /// Returns `None` for permanent errors, exhausted retries,
    /// non-GET/non-replayable requests, an open circuit, or a pending cooldown
    /// longer than the configured maximum wait. Long cooldowns are retained for later calls. Each HTTP attempt
    /// has a 20-second default timeout; explicit request timeouts are preserved.
    /// The shared gate covers response headers as well as
    /// dispatch, so concurrent callers cannot miss a newly received cooldown.
    /// Cooldowns survive exhausted retries and cancelled callers.
    pub async fn send_get(
        &self,
        request: reqwest::RequestBuilder,
        interval: Duration,
    ) -> Option<reqwest::Response> {
        let (http, request) = request.build_split();
        let request = request.ok()?;
        if request.method() != reqwest::Method::GET {
            return None;
        }
        self.execute(http, request, interval)
            .await
            .ok()
            .filter(|response| response.status().is_success())
    }

    /// Send a provider request. GETs receive bounded retries; other methods are
    /// dispatched only once, but still observe and respect the shared cooldown.
    /// HTTP errors are returned intact so callers can interpret API responses.
    ///
    /// # Errors
    /// Returns an error for transport/build failures, a non-replayable GET, or
    /// an open circuit, or a cooldown longer than the configured maximum wait.
    pub async fn send_request(
        &self,
        request: reqwest::RequestBuilder,
        interval: Duration,
    ) -> Result<reqwest::Response, RequestError> {
        let (http, request) = request.build_split();
        let request = request.map_err(|error| RequestError::Http(error.without_url()))?;
        self.execute(http, request, interval).await
    }

    async fn execute(
        &self,
        http: reqwest::Client,
        request: reqwest::Request,
        interval: Duration,
    ) -> Result<reqwest::Response, RequestError> {
        let attempts = if request.method() == reqwest::Method::GET {
            GET_ATTEMPTS
        } else {
            1
        };
        let context = request_context(&self.provider, request.url());
        let mut original = Some(request);
        let interval = interval.min(Duration::from_secs(u64::from(u32::MAX)));
        for attempt in 1..=attempts {
            let mut rate = self.state.lock().await;
            // ponytail: serialize through response headers so quota updates cannot
            // race. This limits each provider/origin to one request in flight;
            // revisit with quota reservations if scans become concurrent.
            rate.wait_ready(&self.provider, &context, self.settings.max_wait)
                .await?;
            // Reserve before awaiting I/O, including when this future is cancelled.
            rate.consume_quota();
            rate.next = Some(Instant::now() + interval.max(rate.adaptive_interval()));
            let mut attempt_request = if attempts == 1 {
                original.take().ok_or(RequestError::NotReplayable)?
            } else {
                original
                    .as_ref()
                    .and_then(reqwest::Request::try_clone)
                    .ok_or(RequestError::NotReplayable)?
            };
            attempt_request
                .timeout_mut()
                .get_or_insert(self.settings.timeout);
            let response = http.execute(attempt_request).await;
            match response {
                Ok(resp) => {
                    let status = resp.status();
                    let retryable = self.observe_response(&mut rate, &resp, &context, interval);
                    if status.is_success() || attempt == attempts {
                        return Ok(resp);
                    }
                    if !retryable {
                        return Ok(resp);
                    }
                }
                Err(error) => {
                    let error = error.without_url();
                    if !error.is_timeout() && !error.is_connect() && !error.is_request() {
                        tracing::warn!(provider = %self.provider, %context, "Metadata provider request failed");
                        return Err(RequestError::Http(error.without_url()));
                    }
                    rate.failed(Duration::ZERO);
                    if rate.failures == 1 {
                        tracing::warn!(
                            provider = %self.provider, %context, %error,
                            "Metadata provider connection failed; entering backoff"
                        );
                    }
                    tracing::debug!(
                        provider = %self.provider,
                        attempt,
                        "Metadata provider request failed; backing off"
                    );
                    if error.is_connect() {
                        // One failed connection attempt is enough. Subsequent
                        // lookups skip immediately until the next probe deadline.
                        rate.open_until = rate.next;
                        return Err(RequestError::Http(error.without_url()));
                    }
                    if attempt == attempts {
                        return Err(RequestError::Http(error.without_url()));
                    }
                }
            }
        }
        Err(RequestError::NotReplayable)
    }
    fn observe_response(
        &self,
        rate: &mut RequestRate,
        response: &reqwest::Response,
        context: &str,
        interval: Duration,
    ) -> bool {
        let status = response.status();
        let headers = response.headers();
        let retryable = matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504);
        rate.observe_quota(headers, chrono::Utc::now());
        let server_delay = server_delay(headers, chrono::Utc::now()).max(rate.quota_delay());
        if retryable {
            let delay = rate.failed(server_delay);
            if rate.failures == 1 {
                tracing::warn!(provider = %self.provider, %context, %status,
                    rate_limit_zone = ?headers.get("x-ratelimit-zone"),
                    remaining = ?headers.get("x-ratelimit-remaining"),
                    reset = ?headers.get("x-ratelimit-reset"),
                    retry_after = ?headers.get("retry-after"),
                    "Metadata provider unavailable; entering backoff");
            }
            tracing::debug!(provider = %self.provider, %context, %status,
                delay_ms = delay.as_millis(), "Metadata provider request rejected; backing off");
        } else {
            if !status.is_success() {
                tracing::warn!(provider = %self.provider, %context, %status,
                    "Metadata provider request rejected");
            } else if rate.succeeded() {
                tracing::info!(provider = %self.provider, "Metadata provider requests recovered");
            }
            rate.postpone(interval.max(server_delay).max(rate.adaptive_interval()));
        }
        retryable
    }

    /// Executes a paced GET and decodes its successful response as JSON.
    /// Parse failures return `None`; transport retries are handled by [`Self::send_get`].
    pub async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        interval: Duration,
    ) -> Option<T> {
        let response = self.send_get(request, interval).await?;
        let context = request_context(&self.provider, response.url());
        match response.json().await {
            Ok(body) => Some(body),
            Err(error) => {
                tracing::warn!(provider = %self.provider, %context, error = %error.without_url(), "Metadata provider response could not be parsed");
                None
            }
        }
    }
}

/// A request failed before a usable HTTP response was received.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// Transport/build errors never expose request URLs containing credentials.
    #[error("provider HTTP request failed: {0}")]
    Http(#[source] reqwest::Error),
    /// Preserve long cooldowns without blocking the scan indefinitely.
    #[error("provider cooldown is active")]
    Cooldown,
    /// The request body cannot be duplicated for a safe retry.
    #[error("provider request cannot be replayed")]
    NotReplayable,
}

/// Apply a provider's quota without imposing a fixed request interval.
pub(crate) trait LimitedRequest {
    async fn send_limited(self, limiter: &RateLimiter) -> Result<reqwest::Response, RequestError>;
}

impl LimitedRequest for reqwest::RequestBuilder {
    async fn send_limited(self, limiter: &RateLimiter) -> Result<reqwest::Response, RequestError> {
        limiter.send_request(self, Duration::ZERO).await
    }
}

/// Pacing and cooldown state for one provider quota.
#[derive(Debug, Default)]
struct RequestRate {
    next: Option<Instant>,
    open_until: Option<Instant>,
    skipped: u64,
    failures: u32,
    successes: u32,
    last_failure: Option<Instant>,
    quota: Option<Quota>,
}

impl RequestRate {
    async fn wait_ready(
        &mut self,
        provider: &str,
        context: &str,
        max_wait: Duration,
    ) -> Result<(), RequestError> {
        self.postpone(self.quota_delay());
        let now = Instant::now();
        let open = self.open_until.is_some_and(|until| until > now);
        let long_wait = self
            .next
            .is_some_and(|next| next.saturating_duration_since(now) > max_wait);
        if open || long_wait {
            if self.skipped == 0 && self.failures == 0 {
                tracing::warn!(
                    provider,
                    context,
                    "Metadata provider quota cooldown active; skipping lookups"
                );
            }
            self.skipped = self.skipped.saturating_add(1);
            return Err(RequestError::Cooldown);
        }
        if let Some(next) = self.next {
            tokio::time::sleep_until(next).await;
        }
        self.open_until = None;
        if self.skipped > 0 {
            let skipped = std::mem::take(&mut self.skipped);
            tracing::info!(
                provider,
                skipped,
                "Metadata provider cooldown ended; resuming lookups"
            );
        }
        Ok(())
    }

    fn adaptive_interval(&self) -> Duration {
        if self.failures == 0 {
            return Duration::ZERO;
        }
        Duration::from_secs(2_u64 << self.failures.min(6).saturating_sub(1)).min(BACKOFF_CAP)
    }

    // A single successful request does not prove a congested service recovered.
    // Require a healthy streak AND a quiet minute, then ease off one level.
    fn succeeded(&mut self) -> bool {
        self.successes = self.successes.saturating_add(1);
        if self.failures > 0
            && self.successes >= RECOVERY_SUCCESSES
            && self
                .last_failure
                .is_some_and(|last| last.elapsed() >= RECOVERY_QUIET)
        {
            self.failures -= 1;
            self.successes = 0;
            return self.failures == 0;
        }
        false
    }

    fn observe_quota(
        &mut self,
        headers: &reqwest::header::HeaderMap,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        let remaining = header_number(headers, "x-ratelimit-remaining")
            .or_else(|| header_number(headers, "ratelimit-remaining"));
        let relative = header_number(headers, "x-ratelimit-reset-in")
            .or_else(|| header_number(headers, "ratelimit-reset"))
            .map(Duration::from_secs);
        let absolute = header_number(headers, "x-ratelimit-reset")
            .and_then(|v| i64::try_from(v).ok())
            .and_then(|v| chrono::DateTime::from_timestamp(v, 0))
            .and_then(|reset| (reset - server_now(headers, now)).to_std().ok());
        if let (Some(remaining), Some(window)) = (remaining, relative.or(absolute)) {
            // A second of slack covers integer header precision at the boundary.
            self.quota = Some(Quota {
                remaining,
                reset: Instant::now()
                    + window.min(Duration::from_secs(u64::from(u32::MAX)))
                    + Duration::from_secs(1),
            });
        }
    }

    fn quota_delay(&self) -> Duration {
        self.quota.as_ref().map_or(Duration::ZERO, |quota| {
            let window = quota.reset.saturating_duration_since(Instant::now());
            // Spread the remaining budget across the rest of the window rather
            // than bursting until Remaining reaches zero. No advertised budget
            // means no calls until reset, even if later responses omit headers.
            window / u32::try_from(quota.remaining.max(1)).unwrap_or(u32::MAX)
        })
    }

    fn consume_quota(&mut self) {
        if let Some(quota) = &mut self.quota {
            if quota.reset <= Instant::now() {
                self.quota = None;
            } else {
                quota.remaining = quota.remaining.saturating_sub(1);
            }
        }
    }

    fn postpone(&mut self, delay: Duration) {
        // Bound untrusted numeric headers to a representable monotonic deadline.
        let deadline = Instant::now() + delay.min(Duration::from_secs(u64::from(u32::MAX)));
        self.next = Some(self.next.map_or(deadline, |next| next.max(deadline)));
    }

    fn failed(&mut self, server_delay: Duration) -> Duration {
        self.failures = self.failures.saturating_add(1);
        self.successes = 0;
        self.last_failure = Some(Instant::now());
        let backoff = self.adaptive_interval();
        // Positive jitter avoids synchronizing clients without shortening Retry-After.
        let jitter = Duration::from_millis(u64::from(uuid::Uuid::new_v4().as_bytes()[0]));
        let delay = backoff.max(server_delay).saturating_add(jitter);
        self.postpone(delay);
        if self.failures >= OPEN_AFTER_FAILURES {
            self.open_until = self.next;
        }
        delay
    }
}

#[derive(Debug)]
struct Quota {
    remaining: u64,
    reset: Instant,
}

fn header_number(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

fn server_now(
    headers: &reqwest::header::HeaderMap,
    fallback: chrono::DateTime<chrono::Utc>,
) -> chrono::DateTime<chrono::Utc> {
    headers
        .get("date")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| chrono::DateTime::parse_from_rfc2822(v).ok())
        .map_or(fallback, |date| date.with_timezone(&chrono::Utc))
}

/// Honor Retry-After (seconds or HTTP date), plus the epoch reset
/// when the advertised quota is exhausted. Date avoids client/server clock skew.
fn server_delay(
    headers: &reqwest::header::HeaderMap,
    now: chrono::DateTime<chrono::Utc>,
) -> Duration {
    let value = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let now = value("date")
        .and_then(|v| chrono::DateTime::parse_from_rfc2822(v).ok())
        .map_or(now, |date| date.with_timezone(&chrono::Utc));
    let until = |date: chrono::DateTime<chrono::Utc>| (date - now).to_std().unwrap_or_default();
    let retry = value("retry-after")
        .and_then(|v| {
            v.parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
                chrono::DateTime::parse_from_rfc2822(v)
                    .ok()
                    .map(|date| until(date.with_timezone(&chrono::Utc)))
            })
        })
        .unwrap_or_default();
    let reset = if value("x-ratelimit-remaining").and_then(|v| v.parse::<u64>().ok()) == Some(0) {
        value("x-ratelimit-reset")
            .and_then(|v| v.parse::<i64>().ok())
            .and_then(|v| chrono::DateTime::from_timestamp(v, 0))
            .map_or(Duration::ZERO, until)
    } else {
        Duration::ZERO
    };
    retry.max(reset)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Real sockets need executor turns before a paused clock advances to an
    // HTTP timeout. Keep the runtime runnable and advance virtual milliseconds
    // explicitly, giving loopback I/O ample turns between ticks. No real sleeps.
    struct TestClock(tokio::task::JoinHandle<()>);
    impl TestClock {
        fn start() -> Self {
            Self(tokio::spawn(async {
                loop {
                    for _ in 0..100 {
                        tokio::task::yield_now().await;
                    }
                    tokio::time::advance(Duration::from_millis(1)).await;
                }
            }))
        }
    }
    impl Drop for TestClock {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn lookup(client: &RateLimiter, url: &str, name: &str) -> Option<String> {
        let response: serde_json::Value = client
            .get_json(
                reqwest::Client::new()
                    .get(format!("{url}/ws/2/artist"))
                    .query(&[("query", name)]),
                Duration::from_secs(1),
            )
            .await?;
        Some(response["artists"][0]["id"].as_str()?.to_owned())
    }

    #[tokio::test(start_paused = true)]
    async fn quota_budget_is_paced_and_retained_without_headers() {
        let mut rate = RequestRate::default();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("date", "Tue, 15 Sep 2026 17:04:50 GMT".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "2".parse().unwrap());
        headers.insert("x-ratelimit-reset", "1789491900".parse().unwrap());
        rate.observe_quota(&headers, chrono::Utc::now());
        assert_eq!(rate.quota_delay(), Duration::from_millis(5500));
        tokio::time::advance(rate.quota_delay()).await;
        rate.consume_quota();
        rate.observe_quota(&reqwest::header::HeaderMap::new(), chrono::Utc::now());
        assert_eq!(rate.quota.as_ref().unwrap().remaining, 1);
        assert_eq!(rate.quota_delay(), Duration::from_millis(5500));
        rate.consume_quota();
        assert_eq!(rate.quota.as_ref().unwrap().remaining, 0);
        assert_eq!(rate.quota_delay(), Duration::from_millis(5500));
        tokio::time::advance(rate.quota_delay()).await;
        rate.consume_quota();
        assert!(rate.quota.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn intermittent_success_does_not_reset_congestion_penalty() {
        let mut rate = RequestRate::default();
        rate.failed(Duration::ZERO);
        assert!(!rate.succeeded());
        assert_eq!(rate.adaptive_interval(), Duration::from_secs(2));
        rate.failed(Duration::ZERO);
        assert!(!rate.succeeded());
        assert_eq!(rate.adaptive_interval(), Duration::from_secs(4));
        for _ in 0..10 {
            assert!(!rate.succeeded());
        }
        assert_eq!(
            rate.failures, 2,
            "success bursts cannot prematurely declare recovery"
        );
        tokio::time::advance(Duration::from_mins(1)).await;
        assert!(!rate.succeeded());
        assert_eq!(rate.failures, 1);
        for _ in 0..4 {
            assert!(!rate.succeeded());
        }
        assert!(rate.succeeded());
        assert_eq!(rate.adaptive_interval(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn successful_quota_exhaustion_holds_concurrent_callers_until_reset() {
        let _clock = TestClock::start();
        let (url, task) = scripted_server(vec![
            (200, "Date: Tue, 15 Sep 2026 17:04:50 GMT\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1789491891\r\n"),
            (200, ""),
        ]).await;
        let client = RateLimiter::new("quota-test");
        let (a, b) = tokio::join!(lookup(&client, &url, "a"), lookup(&client, &url, "b"));
        assert!(a.is_some() && b.is_some());
        let times = task.await.unwrap();
        assert!(times[1] - times[0] >= Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn headerless_success_has_no_fixed_pacing() {
        let _clock = TestClock::start();
        let (url, task) = scripted_server(vec![(200, ""), (200, "")]).await;
        let limiter = RateLimiter::new("headerless");
        for _ in 0..2 {
            assert!(
                reqwest::Client::new()
                    .get(&url)
                    .send_limited(&limiter)
                    .await
                    .is_ok()
            );
            let state = limiter.state.lock().await;
            assert!(state.next.unwrap() <= Instant::now());
            assert!(state.quota.is_none());
            assert_eq!(state.adaptive_interval(), Duration::ZERO);
        }
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn post_is_not_retried_but_retains_server_cooldown() {
        let _clock = TestClock::start();
        let (url, task) = scripted_server(vec![(429, "Retry-After: 120\r\n")]).await;
        let limiter = RateLimiter::new("post-test");
        let http = reqwest::Client::new();
        let response = limiter
            .send_request(http.post(&url).body("payload"), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(response.status(), 429);
        assert_eq!(task.await.unwrap().len(), 1);
        assert!(matches!(
            limiter.send_request(http.get(&url), Duration::ZERO).await,
            Err(RequestError::Cooldown)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn relative_quota_headers_and_invalid_values() {
        for (remaining, reset) in [
            ("x-ratelimit-remaining", "x-ratelimit-reset-in"),
            ("ratelimit-remaining", "ratelimit-reset"),
        ] {
            let mut rate = RequestRate::default();
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(remaining, "2".parse().unwrap());
            headers.insert(reset, "9".parse().unwrap());
            rate.observe_quota(&headers, chrono::Utc::now());
            assert_eq!(rate.quota_delay(), Duration::from_secs(5));
            headers.insert(reset, "invalid".parse().unwrap());
            rate.observe_quota(&headers, chrono::Utc::now());
            assert_eq!(rate.quota_delay(), Duration::from_secs(5));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn connection_failure_opens_immediately_and_allows_later_probe() {
        let _clock = TestClock::start();
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        drop(socket);
        let url = format!("http://{addr}");
        let limiter = RateLimiter::new("offline");
        let http = reqwest::Client::new();
        assert!(
            matches!(limiter.send_request(http.get(&url), Duration::ZERO).await,
            Err(RequestError::Http(error)) if error.is_connect())
        );
        assert_eq!(limiter.state.lock().await.failures, 1);
        for _ in 0..3 {
            assert!(matches!(
                limiter.send_request(http.get(&url), Duration::ZERO).await,
                Err(RequestError::Cooldown)
            ));
        }
        assert_eq!(limiter.state.lock().await.skipped, 3);
        let (healthy, server) = scripted_server(vec![(200, "")]).await;
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(
            limiter
                .send_request(http.get(healthy), Duration::ZERO)
                .await
                .is_ok()
        );
        assert_eq!(limiter.state.lock().await.skipped, 0);
        server.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn circuit_is_explicit_even_when_wait_threshold_exceeds_backoff() {
        let mut rate = RequestRate::default();
        for _ in 0..OPEN_AFTER_FAILURES {
            rate.failed(Duration::ZERO);
        }
        assert!(rate.open_until.is_some());
        // Remove jitter entirely: opening must not depend on it or max_wait.
        rate.next = Some(Instant::now() + BACKOFF_CAP);
        rate.open_until = rate.next;
        assert!(matches!(
            rate.wait_ready("test", "lookup", Duration::from_mins(5))
                .await,
            Err(RequestError::Cooldown)
        ));
        tokio::time::advance(BACKOFF_CAP).await;
        assert!(
            rate.wait_ready("test", "lookup", Duration::from_mins(5))
                .await
                .is_ok()
        );
        assert!(rate.open_until.is_none());
        assert_eq!(rate.skipped, 0);
    }

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn warnings_identify_failures_and_cooldown_summary_counts_skips() {
        use tracing::instrument::WithSubscriber as _;
        let _clock = TestClock::start();
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .finish();
        async {
            let (url, server) = scripted_server(vec![(404, ""), (200, ""), (200, "")]).await;
            let client = RateLimiter::new("omdb");
            let http = reqwest::Client::new();
            let request = || {
                http.get(&url)
                    .query(&[("i", "tt123"), ("apikey", "private-key")])
            };
            assert!(client.send_get(request(), Duration::ZERO).await.is_none());
            assert!(
                client
                    .get_json::<u64>(request(), Duration::ZERO)
                    .await
                    .is_none()
            );
            client.state.lock().await.postpone(Duration::from_mins(2));
            for _ in 0..2 {
                assert!(client.send_get(request(), Duration::ZERO).await.is_none());
            }
            tokio::time::advance(Duration::from_mins(2)).await;
            assert!(client.send_get(request(), Duration::ZERO).await.is_some());
            server.await.unwrap();
        }
        .with_subscriber(subscriber)
        .await;
        let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            logs.lines().any(|line| line.contains("WARN")
                && line.contains("tt123")
                && line.contains("404"))
        );
        assert!(logs.lines().any(|line| line.contains("WARN")
            && line.contains("tt123")
            && line.contains("could not be parsed")));
        assert!(logs.contains("skipped=2"));
        assert!(!logs.contains("private-key"));
    }

    #[test]
    fn settings_bounds_and_safe_request_identifiers() {
        assert_eq!(seconds_setting(None, 20, 10, 60), Duration::from_secs(20));
        assert_eq!(
            seconds_setting(Some("bad"), 20, 10, 60),
            Duration::from_secs(20)
        );
        assert_eq!(
            seconds_setting(Some("0"), 20, 10, 60),
            Duration::from_secs(10)
        );
        assert_eq!(
            seconds_setting(Some("1000"), 20, 10, 60),
            Duration::from_mins(1)
        );
        let url =
            reqwest::Url::parse("https://example.org/private-key?apikey=secret&i=tt123").unwrap();
        assert_eq!(request_context("omdb", &url), "tt123");
        assert_eq!(request_context("audiodb", &url), "lookup");
        let url =
            reqwest::Url::parse("https://musicbrainz.org/ws/2/release/id?query=private").unwrap();
        assert_eq!(request_context("musicbrainz", &url), "/ws/2/release/id");
    }

    #[test]
    fn cooldown_headers_and_backoff() {
        use reqwest::header::HeaderMap;
        let now = chrono::DateTime::from_timestamp(1_789_491_890, 0).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "0".parse().unwrap());
        let mut rate = RequestRate::default();
        assert!(rate.failed(server_delay(&headers, now)) >= Duration::from_secs(2));
        assert!(rate.failed(Duration::ZERO) >= Duration::from_secs(4));
        for _ in 0..20 {
            rate.failed(Duration::ZERO);
        }
        assert!(rate.failed(Duration::ZERO) < Duration::from_secs(61));
        headers.insert("retry-after", "120".parse().unwrap());
        assert_eq!(server_delay(&headers, now), Duration::from_mins(2));
        headers.insert(
            "retry-after",
            "Tue, 15 Sep 2026 17:05:00 GMT".parse().unwrap(),
        );
        headers.insert("date", "Tue, 15 Sep 2026 17:04:50 GMT".parse().unwrap());
        assert_eq!(server_delay(&headers, now), Duration::from_secs(10));
        headers.insert("retry-after", "invalid".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset", "1789491900".parse().unwrap());
        assert_eq!(server_delay(&headers, now), Duration::from_secs(10));
        headers.insert("x-ratelimit-remaining", "14".parse().unwrap());
        assert_eq!(server_delay(&headers, now), Duration::ZERO);
    }

    // Real HTTP exercises the entire request/retry path, including headers and
    // concurrent calls, without sending test traffic to MusicBrainz.
    async fn scripted_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<Instant>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut times = Vec::new();
            for (status, headers) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buf = [0; 1024];
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                times.push(Instant::now());
                let body = r#"{"artists":[{"id":"found"}]}"#;
                let response = format!(
                    "HTTP/1.1 {status} Test\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            times
        });
        (url, task)
    }

    #[tokio::test(start_paused = true)]
    async fn retries_and_concurrent_lookups_share_cooldown() {
        let _clock = TestClock::start();
        let (url, task) =
            scripted_server(vec![(503, "Retry-After: 3\r\n"), (200, ""), (200, "")]).await;
        let client = RateLimiter::new("test");
        let clone = client.clone();
        let (a, b) = tokio::join!(lookup(&client, &url, "a"), lookup(&clone, &url, "b"));
        assert_eq!(a.as_deref(), Some("found"));
        assert_eq!(b.as_deref(), Some("found"));
        let times = task.await.unwrap();
        assert!(times[1] - times[0] >= Duration::from_secs(3));
        assert!(times[2] - times[1] >= Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn unrelated_provider_is_not_blocked_and_non_gets_are_not_sent() {
        let _clock = TestClock::start();
        let blocked = RateLimiter::new("blocked");
        blocked.state.lock().await.postpone(Duration::from_mins(2));
        let (url, task) = scripted_server(vec![(200, "")]).await;
        let other = RateLimiter::new("other");
        let http = reqwest::Client::new();
        assert!(
            blocked
                .send_get(http.get(&url), Duration::ZERO)
                .await
                .is_none()
        );
        assert!(
            other
                .send_get(http.post(&url), Duration::ZERO)
                .await
                .is_none()
        );
        let response = other
            .send_get(http.get(&url), Duration::ZERO)
            .await
            .unwrap();
        assert!(response.text().await.unwrap().contains("found"));
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_500_retries_preserving_request_details() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let _clock = TestClock::start();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for status in [500, 200] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let request = String::from_utf8_lossy(&bytes);
                assert!(request.contains("/metadata?id=123"));
                assert!(request.contains("authorization: Bearer test-token"));
                stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").as_bytes()).await.unwrap();
            }
        });
        let limiter = RateLimiter::new("another-provider");
        let value: Option<serde_json::Value> = limiter
            .get_json(
                reqwest::Client::new()
                    .get(format!("{url}/metadata"))
                    .query(&[("id", "123")])
                    .bearer_auth("test-token"),
                Duration::ZERO,
            )
            .await;
        assert!(value.is_some());
        assert_eq!(limiter.state.lock().await.failures, 1);
        server.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_retries_preserve_backoff() {
        let _clock = TestClock::start();
        let (url, task) = scripted_server(vec![(503, "Retry-After: 0\r\n"); 4]).await;
        let client = RateLimiter::new("test");
        assert!(lookup(&client, &url, "a").await.is_none());
        let times = task.await.unwrap();
        for (i, pair) in times.windows(2).enumerate() {
            assert!(pair[1] - pair[0] >= Duration::from_secs(2 << i));
        }
        let rate = client.state.lock().await;
        assert_eq!(rate.failures, 4);
        assert!(
            rate.next.unwrap().saturating_duration_since(Instant::now()) > Duration::from_secs(15)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn long_cooldown_skips_following_items_and_permanent_errors_do_not_retry() {
        let _clock = TestClock::start();
        let (url, task) = scripted_server(vec![(429, "Retry-After: 120\r\n")]).await;
        let client = RateLimiter::new("test");
        assert!(lookup(&client, &url, "a").await.is_none());
        assert!(lookup(&client, &url, "b").await.is_none());
        assert_eq!(task.await.unwrap().len(), 1);
        let (url, task) = scripted_server(vec![(400, "")]).await;
        let client = RateLimiter::new("test");
        assert!(lookup(&client, &url, "a").await.is_none());
        assert_eq!(client.state.lock().await.failures, 0);
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn successful_response_with_exhausted_quota_delays_next_lookup() {
        let _clock = TestClock::start();
        let (url, task) = scripted_server(vec![(200, "Retry-After: 2\r\n"), (200, "")]).await;
        let client = RateLimiter::new("test");
        assert!(lookup(&client, &url, "a").await.is_some());
        assert!(lookup(&client, &url, "b").await.is_some());
        let times = task.await.unwrap();
        assert!(times[1] - times[0] >= Duration::from_secs(2));
    }
}
