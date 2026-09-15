# MusicBrainz request handling

MusicBrainz can return HTTP 503 for application, source-IP, or global overload.
Even a client below the published one-request-per-second limit can be rejected.
See [MusicBrainz's rate-limiting policy](https://musicbrainz.org/doc/MusicBrainz_API/Rate_Limiting).

MusicBrainz uses the [shared provider rate limiter](PROVIDER_RATE_LIMITING.md)
for request pacing, quota headers, retries and cooldowns. Its adapter keeps
MusicBrainz-specific rules:

- The configured `RateLimit` is the minimum spacing. Official MusicBrainz hosts
  always have a one-second floor, including HTTP, mixed-case and explicit-port
  URLs. Mirrors can use a shorter configured interval.
- Requests include `fmt=json` and a User-Agent containing Ferrofin's version and
  project URL. No API key is required.
- Album, artist and interactive lookups share the same client and cooldown.

A completed library scan can still have incomplete remote metadata if attempts
are exhausted or MusicBrainz remains unavailable. Retry the metadata refresh
once service recovers. Restarting Ferrofin discards the in-memory cooldown and
is not a remedy for upstream overload.
