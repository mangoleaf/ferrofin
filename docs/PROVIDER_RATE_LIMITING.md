# Shared metadata-provider rate limiting

`ferrofin_providers::rate_limit::RateLimiter` handles request pacing, response
quota headers, and transient failures independently of any provider schema.
MusicBrainz, OMDb, TMDb, TVDB, TheAudioDB, fanart.tv, ListenBrainz Labs,
LRCLIB, OpenSubtitles, and Studio Images use it. Each provider client owns an
independent instance; cloning a client shares only that provider's state.
Artwork and subtitle-file downloads share separate gates by URL origin
(scheme, host, port), so one image host's cooldown does not block another host or metadata API.

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
    .get_json(request, Duration::ZERO)
    .await;
# }
```

`send_get` returns a successful `reqwest::Response` for providers that need
text, XML, bytes or custom decoding. `get_json` adds JSON deserialization.
`send_request` returns HTTP responses, including unsuccessful statuses, for
callers that need provider-specific error handling. GETs can be retried; POSTs
(such as TVDB login and OpenSubtitles download-link creation) are sent once,
while still observing/updating the same provider's quota and cooldown. The
GET-only convenience methods continue to reject non-GET requests.
Callers retain control of request construction, authentication and minimum
spacing. Request headers and query parameters survive retries. Explicit request
timeouts are preserved; the
fallback timeout is 20 seconds per attempt.

## Header-driven pacing

Providers other than MusicBrainz and ListenBrainz Labs use zero fixed spacing. A successful response
without quota headers introduces no timed throttle. An unexpired quota learned
from an earlier response still applies if subsequent responses omit headers.
Failures still activate bounded retries and adaptive backoff, even without
headers; absence of headers does not mean an unavailable service should be
hammered.

MusicBrainz retains its documented public-server minimum of one request per
second (and its configurable interval). This is an upstream per-IP requirement,
not a Jellyfin heuristic; global quota headers are not a substitute for it.
A live successful MusicBrainz response checked during this change included
`X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset`.

A successful request to the actual ListenBrainz Labs `/similar-artists/json`
endpoint returned no quota headers. Labs therefore retains Jellyfin's conservative
one-second default/minimum for the public server, in addition to header-aware
pacing and failure backoff. Its request interval is configurable for mirrors;
the public server cannot be configured below one second. A missing quota header
does not disable this provider-specific minimum. The separate main ListenBrainz
API documents both quota headers and a one-call-per-second policy.

Sources: [MusicBrainz policy](https://musicbrainz.org/doc/MusicBrainz_API/Rate_Limiting)
and [main ListenBrainz API policy](https://listenbrainz.readthedocs.io/en/latest/users/api/index.html).

## Retry and cooldown policy

- Requests are serialized through receipt of response headers. The supplied
  minimum interval applies to initial requests and retries.
- HTTP 408, 429, 500, 502, 503 and 504, and transient request/timeout failures,
  receive at most four attempts per GET lookup; other methods get one attempt.
- A connection failure makes one attempt and opens the circuit immediately.
  Later lookups skip without sleeping until its backoff deadline permits a
  probe. Repeated failures increase the cooldown instead of stalling each item.
- Six accumulated transient failures also explicitly open the circuit until
  its deadline. This does not depend on jitter exceeding the maximum wait.
- Failures increase a shared pacing penalty to 2, 4, 8, 16, 32, then 60 seconds,
  with up to 255 milliseconds of jitter on retries. Successful calls retain
  that minimum spacing. After a minute without failure and at least five
  consecutive successes, the penalty drops one level. Each further five
  successes drops another level; recovery is logged only when it reaches zero.
  A single successful call or permanent HTTP error does not reset congestion.
- `Retry-After` (seconds or HTTP date) is a minimum delay. Missing, zero or
  malformed values do not disable backoff.
- `X-RateLimit-Remaining` and the Unix timestamp in `X-RateLimit-Reset` are
  retained as a quota window. `X-RateLimit-Reset-In` is supported as relative
  seconds and preferred over the epoch value. Integer `RateLimit-Remaining`
  and relative `RateLimit-Reset` headers are also supported. Requests are spread
  across the remaining budget;
  each dispatch consumes one slot even if its response omits quota headers.
  An exhausted budget blocks requests until reset. Response `Date` provides
  the clock reference, with one second of reset-boundary slack because these
  headers have whole-second precision. Successful responses update quota too.
  Shared global quotas can still be consumed by other clients after a response;
  these headers cannot guarantee that the next request will be accepted.
- A pending wait longer than the configured maximum (default one minute) skips
  the call immediately, retaining
  the cooldown. Exhausted retries and cancellation also retain pacing state.
- The GET/JSON convenience methods return `None` for permanent errors,
  exhausted retries and JSON parse failures. `send_request` preserves the last
  HTTP response or returns a transport/cooldown error.
  A caller must not treat that as proof that the metadata does not exist.

Backoff entry is logged once per outage at WARN. Permanent HTTP rejections and
JSON parse failures remain WARN, with the MusicBrainz path or OMDb IMDb id.
The first quota-only skip is also WARN; when lookups resume, INFO includes the
number skipped during cooldown. Gradual outage recovery is logged separately.
Download limiter labels contain the URL origin, never signed paths or queries.
Transport errors omit URLs to protect API keys. The gate coordinates callers
within one process, not separate processes sharing an external IP. Upstream
overload can still reject compliant requests.

## Settings

Read when each limiter is constructed (restart to apply):

| Environment variable | Default | Range | Meaning |
| --- | --- | --- | --- |
| `FERROFIN_PROVIDER_TIMEOUT_SECONDS` | 20 | 10–60 | Default timeout per HTTP attempt; an explicitly set request timeout takes precedence. |
| `FERROFIN_PROVIDER_MAX_WAIT_SECONDS` | 60 | 30–300 | Maximum pending pacing/quota wait before skipping a lookup. Open circuits always skip until their probe deadline. |

Malformed values use the default; numeric values are clamped to the range.
Attempts, the adaptive backoff cap, recovery streak/quiet period, and jitter
remain internal policy rather than user settings. The gate deliberately holds
its lock through response headers; supporting concurrent in-flight requests
would require quota reservations and coordinated response updates.
