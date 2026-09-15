# MusicBrainz request handling

MusicBrainz can return HTTP 503 for application, source-IP, or global overload.
Even a client below the published one-request-per-second limit can be rejected.
See [MusicBrainz's rate-limiting policy](https://musicbrainz.org/doc/MusicBrainz_API/Rate_Limiting).

Ferrofin uses one shared MusicBrainz client for its metadata lookups:

- Requests are serialized through response handling. The configured `RateLimit`
  is the minimum spacing; official MusicBrainz hosts always have a one-second
  floor, including HTTP, mixed-case and explicit-port URLs. Mirrors can use a
  shorter configured interval.
- HTTP 408, 429, 500, 502, 503 and 504, and transient connection/request failures,
  get at most four attempts per lookup. Each HTTP attempt has a 20-second timeout.
- Consecutive failures back off for 2, 4, 8, 16, 32, then 60 seconds, plus up to
  255 milliseconds of jitter. The failure count persists across lookups and
  resets on a non-retryable HTTP response.
- `Retry-After` (seconds or an HTTP date) is a minimum delay, even if longer than
  the normal backoff. Zero, missing or malformed values never disable backoff.
- When `X-RateLimit-Remaining` is zero, the Unix timestamp in `X-RateLimit-Reset`
  also sets a minimum delay. The response `Date` supplies the clock reference
  when available. These headers apply to successful responses too.
- A pending wait longer than one minute returns no metadata immediately,
  preserving the shared cooldown for later calls. Four exhausted attempts also
  preserve cooldown. This keeps a long upstream cooldown from blocking a scan.
- Entry into backoff is logged at WARN, recovery at INFO, and individual attempts
  and skipped lookups at DEBUG. Rejection logs include the rate-limit zone when
  supplied. Query strings are excluded from transport-error logs.

A completed library scan can still have incomplete remote metadata if attempts
are exhausted or MusicBrainz remains unavailable. Retry the metadata refresh
once service recovers. Restarting Ferrofin discards the in-memory cooldown and
is not a remedy for upstream overload. Separate processes sharing a public IP
also share MusicBrainz's IP quota; their traffic is outside this client's gate.

The client sends the Ferrofin version and project URL in its User-Agent.
No API key or deployment setting is needed to enable retries.
