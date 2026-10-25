# Configuration

Ferrofin is configured three ways, **highest precedence first**:

1. **CLI flags** — `--data-dir`, `--bind`, `--port`, `--published-url`, `--ffmpeg`, `--config`.
2. **`FERROFIN_*` environment variables** — the full surface, below.
3. **`config.toml`** — an optional file at `{data_dir}/config.toml` (or `--config`/
   `FERROFIN_CONFIG_FILE`). Keys are the env-var stems, lower-cased and prefix-stripped
   (`FERROFIN_BIND_ADDR` → `bind_addr`). The whole file is optional; a missing one is not an
   error.

Everything below has a working default except where noted; a bare `cargo run -p ferrofin-server`
boots.

## Paths

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_DATA_DIR` | `$XDG_DATA_HOME/ferrofin`, else `/var/lib/ferrofin` | Program-data root; derives the sub-dirs below and holds the database. |
| `FERROFIN_CONFIG_DIR` | `{data_dir}/config` | `system.json` + `branding.json`. |
| `FERROFIN_CACHE_DIR` | `{data_dir}/cache` | Cache root, including the transcode/segment cache. |
| `FERROFIN_WEB_DIR` | `{data_dir}/web` | Built jellyfin-web `dist/` served at `/web`. Absent → API-only. |
| `FERROFIN_CONFIG_FILE` | `{data_dir}/config.toml` | Path to the optional bootstrap `config.toml`. |

The data, configuration and cache directories must be different directories: startup checks each against its Jellyfin marker file (`.jellyfin-data`, `.jellyfin-config`, `.jellyfin-cache`) and stops when two of them are one directory (see `docs/UPGRADING.md`).

The database file is `{data_dir}/ferrofin.db`. For drop-in adoption Ferrofin also opens a
Jellyfin `jellyfin.db` (root or `data/jellyfin.db`) found in the data dir — see [Migrating from Jellyfin](../README.md#migrating-from-jellyfin).

## Network & identity

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_BIND_ADDR` | `0.0.0.0` | Address the HTTP listener binds to. |
| `FERROFIN_PORT` | `8096` | HTTP port. |
| `FERROFIN_HTTPS_PORT` | `8920` | HTTPS port (parity value). Served beside the HTTP port when the dashboard's Networking settings enable HTTPS and the PKCS#12 (`.pfx`) certificate at their certificate path loads; see [HTTPS](#https). |
| `FERROFIN_PUBLISHED_URL` | auto-detect | Public base URL advertised to clients. |
| `FERROFIN_BASE_URL` | none | URL path prefix the server is mounted under. |
| `FERROFIN_SERVER_NAME` | host name | Server name reported to clients. |
| `FERROFIN_WS_MIN_STREAM_PERIOD_MS` | `100` | Smallest period, in milliseconds, between pushes of a dashboard WebSocket stream (Sessions / ScheduledTasksInfo / ActivityLogEntry). A client names the period in its `*Start` subscription; Ferrofin drives these streams on a timer (each tick recomputes and resends the full snapshot), so the client's period is clamped up to this floor to stop a tiny period becoming back-to-back `get_sessions`/activity work. Well-behaved clients ask for 1500 ms and are unaffected. The `config.toml` key is `websocket_min_stream_period_ms`. |
| `FERROFIN_LOG` | `info` | Log filter, `RUST_LOG`/`EnvFilter` syntax. |
| `FERROFIN_QUICK_CONNECT_PENDING_PER_IP` | `10` | How many unauthorized Quick Connect requests one client IP may hold at once. `POST /QuickConnect/Initiate` needs no sign-in, so past this cap the IP's oldest pending request is dropped and its code stops working. Jellyfin has no cap. Behind a reverse proxy that is not in **Known proxies**, every client shares the proxy's IP, so the cap then counts all clients together. The cap is per address, so an IPv6 client can spread requests across its prefix. `0` means the default. The `config.toml` key is `quick_connect_pending_per_ip`. |

### HTTPS

Ferrofin serves HTTPS itself, as Jellyfin does, from the dashboard's **Networking** settings
(`{config}/users/named/network.json`):

- **Enable HTTPS** with a **Custom SSL certificate path** to a PKCS#12 (`.pfx`/`.p12`) file holding
  the certificate and its private key, plus its password if it has one. At startup the server
  listens with HTTPS on `FERROFIN_HTTPS_PORT` on every address it listens with HTTP on.
  If the certificate does not load, the server logs why and serves HTTP only.
  A certificate without a password must use the usual empty-password form, as OpenSSL writes
  it. The rarer NULL-password form, which .NET loads, does not load yet (gl-815). To rewrite
  a password-less file in the usual form:
  `openssl pkcs12 -in old.pfx -passin pass: -nodes -out all.pem`, then
  `openssl pkcs12 -export -in all.pem -passout pass: -out new.pfx` (delete `all.pem` after:
  it holds the private key unencrypted).
- HTTPS connections use HTTP/1.1; Jellyfin's HTTPS endpoints also offer HTTP/2 (gl-814).
- **Require HTTPS** answers every plain HTTP request with a `307` redirect to the same path on
  HTTPS, except `/health/live` and `/health/ready`. A request a known proxy forwarded with
  `X-Forwarded-Proto: https` counts as HTTPS.
- A new certificate path takes effect after a restart; the dashboard shows the restart as
  pending. A path to a missing file saves but is answered `404`.
- The ports are `FERROFIN_PORT` and `FERROFIN_HTTPS_PORT`; the port fields on the Networking
  page are not read yet (gl-554). When both name the same port, HTTPS is not served and the
  log says so.
- Adoption keeps Jellyfin's **Enable HTTPS** and **Require HTTPS**, but not its certificate
  path and password, which point into Jellyfin's filesystem.

A reverse proxy that terminates TLS needs none of this. List it under **Known proxies** instead.
Addresses published from each request (**Enable published server URI by request**) take the
proxy's `X-Forwarded-Proto`, but not yet its `X-Forwarded-Host` (gl-813).

## Admin seeding (fresh database only)

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_ADMIN_USER` | `admin` | Username of the admin seeded on first boot. |
| `FERROFIN_ADMIN_PASSWORD` | none (passwordless) | Admin password. Set it for a headless install. Unset, the admin is seeded **without a password** (as in Jellyfin) and signs in with an empty one until the setup wizard at `/web` sets it — finish the wizard before exposing the server. |

## ffmpeg

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_FFMPEG_PATH` | auto-discovered on `$PATH` | Path to `ffmpeg`. Absent ffmpeg only disables transcode. |
| `FERROFIN_FFPROBE_PATH` | auto-discovered on `$PATH` | Path to `ffprobe` (scan probes). |
| `FERROFIN_FFMPEG_IMG_EXTRACT_PERF_TRADEOFF` | `false` | Jellyfin's `FFmpeg:imgExtractPerfTradeoff`: trade frame quality for speed when extracting chapter images, video thumbnails and embedded pictures. On, the first attempt decodes keyframes only and takes the first one, instead of picking the most representative of 24 frames; extraction is faster, and the image may be less typical of the scene. The `config.toml` key is `ffmpeg_img_extract_perf_tradeoff`. |

## Library & database

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_LIBRARY_ROOTS` | none | Library root paths to seed on a fresh install. |
| `FERROFIN_DB_POOL` | `auto` | SQLite connection count, or `auto` (sizes to cores — the measured optimum). |
| `FERROFIN_SCAN_PROGRESS_EVERY` | built-in | Items between scan-progress log lines. |
| `FERROFIN_PLAYBACK_METRICS_QUEUE` | `1024` | Depth of the playback-metrics write queue, in events. Each PlaybackInfo decision and playstate start/stop is handed to this queue and written by one background task, so a SQLite WAL checkpoint can never stall a playback request. A full queue drops the event with a warning rather than blocking — raise it if `playback-metrics queue full` appears in the log under a load spike. |
| `FERROFIN_SCAN_PROBE_CONCURRENCY` | `8` (capped at cores) | ffprobe processes a library scan keeps in flight. The probe is the bulk of scan time, so this is the scan's main throughput knob: measured ~3.7× at `4` and ~10× at `16` over serial probing on a local SSD. Keep it low on a spinning disk or a network mount, where a wide window turns sequential reads into seek thrash. |
| `FERROFIN_SCAN_DIRECTORY_CACHE_RECORDS` | `100000` | How many directory-listing records one library scan keeps (a folder counts each entry plus one). A scan lists each folder once and every item in it reads that listing; once the scan holds this many records it drops them all and lists folders again as it reaches them. This bounds the memory a huge library's scan spends on listings. The default is Jellyfin's (`DirectoryService.MaxCachedRecords`). Must be a positive integer; `0` is an error. The `config.toml` key is `scan_directory_cache_records`. |
| `FERROFIN_SCAN_DIRECTORY_CACHE_IDLE_SECS` | `300` | Seconds a library scan's directory listings survive unread before they are dropped. The default is Jellyfin's (`DirectoryService.IdleTimeoutMs`). Must be a positive integer; `0` is an error. The `config.toml` key is `scan_directory_cache_idle_secs`. |
| `FERROFIN_TRICKPLAY_CONCURRENCY` | `auto` | How many trickplay images are generated at once (one item and width each: an ffmpeg frame extraction, then tile packing). A library scan generates several at once; the **Generate Trickplay Images** task goes one item at a time but shares this cap when it overlaps a scan. `auto` allows as many as the image-encoding limit: **Parallel image encoding limit** on the dashboard (`ParallelImageEncodingLimit`), else one per CPU. An integer sets a lower cap; `0`, a negative number, or a value above the auto value means `auto`. Each generation is an ffmpeg process with its own memory, and with hardware acceleration its own GPU memory (about 1 GiB for a 4K HEVC video on an NVIDIA GPU). Jellyfin generates one at a time; `1` does the same. The `config.toml` key `trickplay_concurrency` takes an integer or `"auto"`. |

## Observability

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_ENABLE_METRICS` | `false` | Serve Prometheus `/metrics`. Off → the route is absent. |
| `FERROFIN_METRICS_SAMPLE_INTERVAL` | built-in | Seconds between process-metric samples. |
| `FERROFIN_METRICS_SCAN_DURATION_BUCKETS` | 1–2.5–5 per decade, `0.001` … `5000` | Bucket boundaries, in seconds, of the library-scan duration histograms (`ferrofin_library_scan_duration_seconds`, `ferrofin_library_scan_pass_duration_seconds`). The env var is comma-separated (`0.01,0.1,1,10,60,300,1800,7200`); the `config.toml` key `metrics_scan_duration_buckets` is an array (`[0.01, 0.1, 1, 10]`). A list that is empty, non-numeric, non-finite, not above zero or not strictly increasing is refused with a warning at startup and the default is used. |
| `FERROFIN_SHUTDOWN_TIMEOUT_SECS` | `30` | Seconds a graceful shutdown or in-process restart (`POST /System/Restart`, backup restore) waits for in-flight requests before aborting the remaining connections — ASP.NET's `ShutdownTimeout` default. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | none | Enable OTLP trace export to this endpoint (off by default). |

## Provider request limits

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_PROVIDER_TIMEOUT_SECONDS` | `20` | Default per-attempt HTTP timeout, clamped to 10–60 seconds. Explicit request timeouts take precedence. |
| `FERROFIN_PROVIDER_MAX_WAIT_SECONDS` | `60` | Maximum pending pacing/quota wait before skipping a lookup, clamped to 30–300 seconds. Open circuits skip immediately until a probe is permitted. |

These are read when provider limiters are constructed; restart to apply changes.
See [provider rate limiting](PROVIDER_RATE_LIMITING.md) for retry and cooldown policy.

## Remote metadata provider keys

Every remote provider is compiled in and enabled per library through the fetcher
checkboxes, as in Jellyfin. TMDB, OMDb, TVDB, fanart.tv and TheAudioDB ship with
Jellyfin's shared keys. The settings below are optional overrides or additions.

| Variable | Purpose |
|---|---|
| `FERROFIN_OMDB_KEY` | Optional OMDb API key override; unset or blank uses Jellyfin's shared key. |
| `FERROFIN_TVDB_KEY` / `FERROFIN_TVDB_PIN` | Optional TheTVDB API key override and subscriber PIN. |
| `FERROFIN_TVDB_CACHE_HOURS` | How long TheTVDB lookups are reused, in whole hours (default `1`, the TVDB plugin's `CacheDurationInHours`): an episode's TVDB id, and the season list of a series a season's TVDB id is read from. A longer duration saves requests when a library is refreshed again within that time. The `config.toml` key is `tvdb_cache_hours`. A value that is not a whole number above zero is refused with a warning at startup and the default is used. |
| `FERROFIN_TVDB_TOKEN_DAYS` | How old TheTVDB's sign-in token may get before Ferrofin signs in again, in whole days (default `25`, the TVDB plugin's value). TheTVDB expires a token after a month, so the value must be from 1 to 29. The `config.toml` key is `tvdb_token_days`. Any other value is refused with a warning at startup and the default is used. |
| `FERROFIN_FANART_KEY` | Optional fanart.tv personal key, sent alongside the shared project key. |
| `FERROFIN_MUSICBRAINZ_URL` | MusicBrainz base URL override (self-hosted mirror). |
| `FERROFIN_STUDIOS_REPO_URL` | Studio-images repo URL override. |

TMDB's `TmdbApiKey` plugin setting overrides its shared key. OpenSubtitles uses
Jellyfin's shared application key with the username/password configured in its
plugin settings; its `ApiKey` field is an optional override. The shared key does
not replace the account needed for subtitle downloads.

## WASM plugins (Tier 1b)

Limits for sandboxed WASM plugins loaded from `{data_dir}/plugins/*.wasm`
(see `EXTENSIONS.md`). Zero is treated as unset; defaults apply.

| Variable | Purpose |
|---|---|
| `FERROFIN_WASM_CALL_TIMEOUT_SECS` | Per-guest-call deadline in seconds (default 30). A plugin call past the deadline is interrupted; repeated failures sideline the plugin until restart. Exception: time spent inside a host media extraction (`extract-audio`/`extract-frames`) is bounded by the extraction's own 1-minute wall-clock budget instead — the deadline clock does not tick during host calls, so a call using extraction can legitimately outlive this setting by up to that budget. |
| `FERROFIN_WASM_MEMORY_LIMIT_MB` | Per-plugin linear-memory cap in MiB (default 128). A `memory.grow` ceiling, never a reservation — small plugins use a few MiB. Also caps `http-fetch` response bodies. |
| `FERROFIN_WASM_EVENT_QUEUE_CAPACITY` | Per-plugin event queue depth (default 256). A full queue drops events for that plugin only. Requests to a plugin's own URL space (`/Plugins/{id}/web/…`) may hold at most half of it, queued or running; past that, or when the queue is full, they get `503` at once instead of waiting. |
| `FERROFIN_WASM_STATE_LIMIT_MB` | Per-plugin key/value state cap in MiB (default 8). Settings and cursors fit easily; raise for stats-heavy plugins (e.g. playback reporting). |
| `FERROFIN_WASM_IMAGE_DOWNLOAD_MB` | Cap on one plugin-artwork download in MiB (default 20). Posters/backdrops are single-digit MiB; anything larger is not artwork. |
| `FERROFIN_WASM_IMAGE_TIMEOUT_SECS` | Wall-clock bound on one plugin-artwork download in seconds (default 30). A CDN GET, not a transfer job. |
| `FERROFIN_WASM_WRITE_CONTENT_MB` | Cap on one plugin `write-lyrics`/`write-subtitles` payload in MiB (default 2). Settings-class writes, not media. |
| `FERROFIN_WASM_SUBTITLE_EXTRACT_MB` | Cap on one plugin-extracted subtitle track in MiB (default 10). Generous for SRT text. |
| `FERROFIN_WASM_ANALYSIS_CONCURRENCY` | Concurrent media-decode budget shared by all analysis plugins (`extract-audio`/`extract-frames`). Default: a quarter of the visible cores, at least one — analysis must never starve transcodes; a small NAS may want `1`, a big host more. |
| `FERROFIN_WASM_PRIVATE_HTTP_ALLOW` | Plugins allowed to `http-fetch` private/loopback/link-local destinations: comma-separated plugin UUIDs, or `*` for all. Default: denied for every plugin (public destinations are always allowed). Plugin UUIDs appear in `/Plugins` and the load log line. (Accepting plugin names here is a planned improvement.) |

## Build- and test-time only

Not runtime config, listed for completeness:

- `FERROFIN_REFRESH_PLUGIN_ASSETS=1` — re-fetch vendored extension settings pages during
  `cargo build -p ferrofin-extensions`.
- `FERROFIN_SCAN_BENCH=1` — run the opt-in library-scan measurement harness
  (`apps/ferrofin-server/tests/scan_bench.rs`); reports items/second for a generated
  fixture library. `FERROFIN_SCAN_BENCH_{MOVIES,SERIES,EPISODES}` size it,
  `FERROFIN_SCAN_BENCH_PROBE=1` turns on the real ffprobe, and
  `FERROFIN_SCAN_BENCH_{MEDIA,POSTER}` point it at real sample files.
- `FERROFIN_FFMPEG_TESTS=1` — run the env-gated real-ffmpeg integration tests.
- `FERROFIN_WASM_GUEST_TESTS=1` — build `examples/wasm-hello` from source (needs the
  wasm32-wasip2 target) and run the end-to-end WASM plugin tests.
