# Shared metadata-provider rate limiting

`ferrofin_providers::rate_limit::RateLimiter` handles request pacing, response
quota headers, and transient failures independently of any provider schema.
MusicBrainz and OMDb use it for their JSON metadata requests.

Create one limiter per quota (normally a provider host/account) and keep it on
the provider client. Clone that limiter for callers sharing the quota: clones
share cooldown state. Separate limiters are independent, so an outage at one
provider does not block another. Creating a limiter for each request would
lose pacing and cooldown state.

```rust,no_run
use std::time::Duration;
use ferrofin_providers::rate_limit::RateLimiter;

# async fn example() {
let http = reqwest::Client::new();
let limiter = RateLimiter::new("example-provider");
let request = http.get("https://metadata.example/releases")
    .query(&[("title", "Example")]);
let metadata: Option<serde_json::Value> = limiter
    .get_json(request, Duration::from_secs(1))
    .await;
# }
```

`send_get` returns a successful `reqwest::Response` for providers that need
text, XML, bytes or custom decoding. `get_json` adds JSON deserialization.
Callers retain control of request construction, authentication and minimum
spacing. The limiter accepts replayable GET requests only; it rejects POSTs
and other methods to avoid replaying writes. Request headers and query
parameters survive retries. Explicit request timeouts are preserved; the
fallback timeout is 20 seconds per attempt.

## Retry and cooldown policy

- Requests are serialized through receipt of response headers. The supplied
  minimum interval applies to initial requests and retries.
- HTTP 408, 429, 500, 502, 503 and 504, and transient connection/request failures,
  receive at most four attempts per lookup.
- Failures increase a shared pacing penalty to 2, 4, 8, 16, 32, then 60 seconds,
  with up to 255 milliseconds of jitter on retries. Successful calls retain
  that minimum spacing. After a minute without failure and at least five
  consecutive successes, the penalty drops one level. Each further five
  successes drops another level; recovery is logged only when it reaches zero.
  A single successful call or permanent HTTP error does not reset congestion.
- `Retry-After` (seconds or HTTP date) is a minimum delay. Missing, zero or
  malformed values do not disable backoff.
- `X-RateLimit-Remaining` and the Unix timestamp in `X-RateLimit-Reset` are
  retained as a quota window. Requests are spread across its remaining budget;
  each dispatch consumes one slot even if its response omits quota headers.
  An exhausted budget blocks requests until reset. Response `Date` provides
  the clock reference, with one second of reset-boundary slack because these
  headers have whole-second precision. Successful responses update quota too.
  Shared global quotas can still be consumed by other clients after a response;
  these headers cannot guarantee that the next request will be accepted.
- A pending wait longer than one minute returns `None` immediately, retaining
  the cooldown. Exhausted retries and cancellation also retain pacing state.
- Permanent errors, exhausted retries and JSON parse failures return `None`.
  A caller must not treat that as proof that the metadata does not exist.

Backoff entry is logged at WARN, recovery at INFO and individual attempts at
DEBUG, all tagged with `provider`. Transport errors omit URLs to protect API
keys. The gate coordinates callers within a process, not separate processes
sharing an external IP. Upstream overload can still reject compliant requests.
