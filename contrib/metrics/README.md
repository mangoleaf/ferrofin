# Ferrofin metrics — Prometheus `/metrics` + Grafana

Ferrofin exposes Prometheus text-exposition metrics on `GET /metrics`, instrumented
through the **OpenTelemetry** API. It is a **parity port** of Jellyfin's
prometheus-net surface: every metric whose concept exists in Rust keeps Jellyfin's
exact name, labels, and buckets, so existing Jellyfin Grafana dashboards work
against Ferrofin unchanged.

Rules for adding/changing a metric live in `docs/conventions/METRICS.md`.

## Enable it

Metrics are **off by default** and gated on the existing
`ServerConfiguration.EnableMetrics` toggle (restart required — Jellyfin semantics):

- Edit `{config_dir}/system.json`: set `"EnableMetrics": true`, or
- `POST /System/Configuration` with the flag set, or
- set the bootstrap override `FERROFIN_ENABLE_METRICS=true` (env) / `enable_metrics = true`
  (in `config.toml`) — for declarative/GitOps/container deploys where editing
  `system.json` or calling the API is impractical. It wins over the persisted flag
  when set (`false` force-disables); unset defers to `system.json`. Like the sampler
  interval, it is a bootstrap knob only — NOT part of the API `ServerConfiguration`,
  so `/System/Configuration` stays byte-identical to Jellyfin.

then **restart** Ferrofin. Disabled ⇒ `/metrics` returns `404`, no recording
overhead, no background sampler. The endpoint is unauthenticated when enabled

Optionally align the gauge sampler with a non-default Prometheus scrape interval
via the bootstrap knob `FERROFIN_METRICS_SAMPLE_INTERVAL` (env) or
`metrics_sample_interval` (in `config.toml`), in seconds; unset keeps the 15 s
default. It lives in the bootstrap config, not the API `ServerConfiguration`, so
`/System/Configuration` stays byte-identical to Jellyfin. Keep it aligned with
`scrape_interval` in `prometheus.yml` (RULES_METRICS rule 10).

The endpoint is unauthenticated when enabled
(Jellyfin parity); the bounded-cardinality label rules are what keep that safe —
nothing user-identifying appears in the exposition.

```bash
curl http://localhost:8096/metrics
```

Content-Type: `text/plain; version=0.0.4; charset=utf-8`.

## Scrape + dashboards

```bash
prometheus --config.file=contrib/metrics/prometheus.yml     # scrapes localhost:8096, 15s
```

Then import into Grafana (each carries a `datasource` variable — pick your Prometheus),
or let the Helm chart provision all three for a Grafana dashboard sidecar
(`dashboards.enabled`, see `charts/ferrofin/README.md`):

- **`grafana-golden-signals.json`** (uid `ferrofin-golden-signals`) — the at-a-glance
  overview, organized by the four golden signals: **traffic** (request rate, in-flight,
  sessions/streams), **latency** (p50/p95/p99), **errors** (5xx ratio, 4xx/5xx rate),
  **saturation** (CPU %, memory, DB pool in-use). Start here.
- **`grafana-deep-dive.json`** (uid `ferrofin-deep-dive`) — drill-down with a `controller`
  filter variable: a latency heatmap, top endpoints by rate and by p95, status-code and
  error breakdowns, full DB-pool detail, process/tokio-runtime internals, and
  playback/library panels.
- **`grafana-library-scans.json`** (uid `ferrofin-library-scans`) — library scans,
  their ffprobe runs and metadata-provider traffic; see
  [Library scans dashboard](#library-scans-dashboard). Golden Signals carries a
  compact *Library scans* row (in progress, time since the last scheduled scan,
  scans by trigger, provider failures) that links to it.

All three carry the `ferrofin` tag, a dashboard-links dropdown to each other (carrying
the variables across) and a **`job`** variable that every query is scoped to, so a
Prometheus that also scrapes a Jellyfin (or several Ferrofins) never mixes them:

- Golden Signals and Deep Dive take `job` from `http_requests_received_total` — the
  prometheus-net parity metric, so the list holds the Ferrofin *and* Jellyfin jobs and
  nothing else (`process_start_time_seconds` would also list every Go exporter in the
  cluster). It defaults to the single job `ferrofin` (`prometheus.yml` below names it so,
  as does the chart's ServiceMonitor for a release named `ferrofin` — the job is the
  release's full name; another name falls back to the first job), is
  single-select, and has no *All*.
- Library Scans takes `job` (and `instance`) from `ferrofin_uptime_seconds`, a
  Ferrofin-only metric, multi-select with *All* — which expands to exactly the Ferrofin
  jobs found, never a wildcard.

`apps/ferrofin-server/tests/dashboards.rs` fails if a dashboard charts a metric or
label Ferrofin does not expose, selects a metric without `job=~"$job"`, or if the
chart's copies drift from these files.

The `http_*` / `process_*` panels also light up when Golden Signals or Deep Dive is
pointed at a Jellyfin instance with metrics enabled (benchmark leg, port 18097):
switch the `job` dropdown to its job, or open the dashboard twice (one tab per job) to
compare side by side. That is the parity proof. The dropdown is single-select because
no panel there splits by `job` — two jobs would merge into one line.

## Traces + log↔trace correlation (OTLP → Tempo)

Metrics are one signal; traces are a separate, opt-in one. Setting
`OTEL_EXPORTER_OTLP_ENDPOINT` turns on OTLP gRPC span export (off otherwise — no
overhead). Traces are the **only** signal on OTLP; metrics stay on this
Prometheus scrape and logs stay on stdout/file. Design rules live in
`docs/conventions/TRACING.md`.

```bash
# Export sampled request traces to Alloy → Tempo (default sample ratio 0.25).
OTEL_EXPORTER_OTLP_ENDPOINT=http://<alloy-host>:4317 \
OTEL_TRACES_SAMPLER_ARG=0.25 \
  ferrofin-server --data-dir ./data
```

With export on, stdout is structured JSON by default and every log line emitted
inside a **sampled** request carries the request's `trace_id` (unsampled requests
carry none — no dead links). `FERROFIN_LOG_FORMAT=text` restores the legacy
human-readable stdout for interactive dev; the rotating log **file** is always
plain text regardless (it feeds the `GET /System/Logs` dashboard viewer).

To click from a log line straight to its trace in Grafana, add a **Loki derived
field** on the Loki datasource:

- **Name:** `trace_id`
- **Regex:** `trace_id":"([0-9a-f]{32})`
- **Internal link → data source:** your Tempo datasource
- **URL/Query:** `${__value.raw}`

Grafana → Explore (Loki) then shows a "Tempo" button on each `trace_id`-bearing
line that opens the request waterfall.

## Metric table

### Parity (Jellyfin's exact names — prometheus-net)

| Metric | Type | Labels |
|---|---|---|
| `http_requests_received_total` | counter | `code`, `method`, `controller`, `action`, `page`, `endpoint` |
| `http_requests_in_progress` | gauge | `method`, `controller`, `action`, `page`, `endpoint` |
| `http_request_duration_seconds` | histogram | `code`, `method`, `controller`, `action`, `page`, `endpoint` |
| `process_cpu_seconds_total` | counter | — |
| `process_start_time_seconds` | gauge | — |
| `process_working_set_bytes` | gauge | — |
| `process_virtual_memory_bytes` | gauge | — |
| `process_private_memory_bytes` | gauge | — |
| `process_num_threads` | gauge | — |
| `process_open_handles` | gauge | — |
| `process_cpu_count` | gauge | — |

`controller`/`action` come from the vendored OpenAPI spec (each operation's first
tag + `operationId`), matching prometheus-net's ASP.NET routing labels. `endpoint`
is the route **template** with no leading slash (the spec path, e.g.
`Users/{userId}` — never a raw path; cardinality). `page` is always empty (a
Razor-Pages artifact prometheus-net emits on every series). Histogram buckets are
prometheus-net.AspNetCore's default exponential series `0.001 × 2ⁿ`
(`0.001, 0.002, … 32.768`). All four are copied verbatim from the live fixture.

### Ferrofin-specific (`ferrofin_*` — no Jellyfin equivalent)

| Metric | Type | Labels | Source |
|---|---|---|---|
| `ferrofin_sessions_active` | gauge | — | session snapshot |
| `ferrofin_playback_streams_active` | gauge | — | sessions with a now-playing item |
| `ferrofin_playback_streams` | gauge | `method` (`Transcode`/`DirectStream`/`DirectPlay`) | playing sessions by play method |
| `ferrofin_transcode_jobs_active` | gauge | — | sessions with active transcode info |
| `ferrofin_db_pool_connections` | gauge | `pool` (`read`/`write`) | sqlx pool size |
| `ferrofin_db_pool_idle_connections` | gauge | `pool` (`read`/`write`) | sqlx idle count |
| `ferrofin_library_items` | gauge | `type` (`BaseItemKind`) | `BaseItems` grouped by type (sampled ~60s) |
| `ferrofin_uptime_seconds` | gauge | — | since sampler start |
| `ferrofin_tokio_workers` | gauge | — | tokio runtime worker count |
| `ferrofin_tokio_alive_tasks` | gauge | — | tokio runtime alive-task count |

`ferrofin_tokio_*` is the honest analogue of `dotnet_threadpool_*`.

### Library scans, ffprobe and metadata providers (ferrofin-specific)

Recorded by the library crates themselves (METRICS.md rule 8): the scan in
`ferrofin_core::scan_metrics`, the providers in `ferrofin_providers::metrics`, both
created by the composition root right after the meter provider (nothing is recorded
while metrics are disabled). Every counter's bounded label combinations are seeded at
0 at startup, so the first scan after a restart already shows up in `increase()`.

The two duration histograms share one bucket set, a bootstrap setting:
`FERROFIN_METRICS_SCAN_DURATION_BUCKETS` (env, comma-separated seconds, e.g.
`0.01,0.1,1,10,60,300,1800,7200`) or `metrics_scan_duration_buckets` (in `config.toml`,
an array of numbers). Unset, it is 1–2.5–5 per decade from 0.001 to 5000 s (21
buckets). A list that is empty, has a non-numeric, non-finite or non-positive entry, or
is not strictly increasing is refused with a warning at startup and the default is
used — a metrics setting never stops the server. Like the sampler interval it is not a
`ServerConfiguration` field. Keep one bucket set across instances you aggregate: a
`histogram_quantile` over mixed bucket sets is meaningless.

| Metric | Type | Labels | What it counts |
|---|---|---|---|
| `ferrofin_library_scans_total` | counter | `trigger`, `result` (`completed`/`stopped`/`failed`) | Scan passes the scan queue ran. A pass is one queued request: a full or library scan, a folder or item refresh, a watcher/webhook batch. `stopped` = cancelled (shutdown, a replacing scan). |
| `ferrofin_library_scan_items_total` | counter | `trigger`, `outcome` (`created`/`updated`/`unchanged`/`removed`) | The items each pass processed (a stopped pass's partial counts included), plus those of item refreshes served inside a running scan. `unchanged` = a stored item the scan left untouched. |
| `ferrofin_library_scan_duration_seconds` | histogram | `trigger` | Duration of **completed** passes. Default buckets: 1–2.5–5 per decade, 0.001 … 5000 s (21 buckets; configurable, above): a watcher scan is 20–60 ms, an unchanged 20k-item rescan ~2 s, a first scan of a big library tens of minutes. |
| `ferrofin_library_scan_in_progress` | gauge | — | 1 while the scan queue is running a scan. |
| `ferrofin_library_scan_last_completed_timestamp_seconds` | gauge | `trigger` | Unix time the last pass of that trigger completed (absent until one has). |
| `ferrofin_library_scan_last_duration_seconds` | gauge | `trigger` | Duration of the last completed pass of that trigger. |
| `ferrofin_library_scan_pass_duration_seconds` | histogram | `pass` (`music`, `album_covers`, `years`, `artists`, `aggregates`, `by_name_paths`, `studios`, `library_images`, `dynamic_images`) | Each closing pass of a completed library validation — the fields of the `post-scan passes complete` log line. Same buckets. Watcher, webhook and item refreshes run only the touched items' passes, which are not recorded here. |
| `ferrofin_library_scan_lane_refreshes_total` | counter | `result` | Item refreshes (refresh, Identify → Apply) a running scan served between its own items instead of making the caller wait. Their items count in `…_items_total{trigger="api"}`; they are not passes. |
| `ferrofin_media_probe_total` | counter | `result` (`ok`/`failed`/`cancelled`) | ffprobe runs of the scan's media probe, one per new or changed media file (its sidecar subtitle/audio probes are not counted separately). `cancelled` = the scan stopped mid-probe and the child was killed. **~0 on an unchanged rescan.** |
| `ferrofin_metadata_provider_requests_total` | counter | `provider`, `result` (`ok`/`not_found`/`failed`/`skipped`) | One per logical request through a provider's rate limiter, retries excluded (the `metadata provider request` debug line). `not_found` = a 404 answer; `failed` = transport error, another error status, or a request that went out and then gave up (a 429 whose `Retry-After` outlasts the wait for its retry); `skipped` = never sent (cooldown or open circuit). |
| `ferrofin_metadata_provider_retries_total` | counter | `provider` | Retry attempts beyond each request's first (408/429/5xx, timeouts). |

Label sets:

- `trigger` — the **request that started the scan**, in `LOGGING.md`'s vocabulary:
  `schedule` (the *Scan Media Library* task's own trigger), `startup` (its startup
  trigger), `api` (*Scan All Libraries*, `POST /Library/Refresh`, an item or folder
  refresh, a library change — so `api` mixes whole-library scans with single-item
  refreshes; the `scope` field of the `library_scan_pass` log span tells them apart),
  `watcher` (real-time monitoring), `webhook` (`POST /Library/{Series,Movies}/{Added,Updated}`,
  `POST /Library/Media/Updated`). It is a label only and never changes what runs:
  watcher and webhook reports share one `LibraryMonitorDelay` window, whose batch is one
  scan labelled with the window's first reporter, and requests that coalesce in the scan
  queue run as one scan under the trigger of the request that scan runs as — the queued
  request the later ones joined, or, when a queued full scan takes over the pending
  requests it covers, the full scan's own.
- `provider` — `tmdb`, `tvdb`, `musicbrainz`, `audiodb`, `fanart`, `omdb`, `studios`,
  `opensubtitles` (its API and its subtitle-file downloads), `lrclib`, `listenbrainz`,
  `image` (artwork downloads, every CDN origin) and `other` (any limiter outside that
  set). Only requests through a provider's rate limiter are counted: a WASM plugin's
  `http-fetch` goes through the plugin host, not a rate limiter, so it is neither
  counted here nor able to add label values.

## Library scans dashboard

`grafana-library-scans.json` (uid `ferrofin-library-scans`, default range 24 h) has
`job`, `instance` and `trigger` variables. Panels:

| Row | Panel | Query basis |
|---|---|---|
| Overview | Scan in progress | `ferrofin_library_scan_in_progress` |
| | Time since last scheduled scan (`schedule`/`startup` only, one value per scraped instance; yellow > 13 h, red > 25 h; instant query) | `time() - max by (instance) (last_over_time(…_last_completed_timestamp_seconds[7d]))` over the instances `up` lists now; one with no scan of its own in 7 d takes the latest of its job's departed instances |
| | Last scan duration (per trigger; instant query) | the `…_last_duration_seconds` series whose `…_last_completed_timestamp_seconds` is newest in 7 d |
| | Unchanged (range) | `unchanged` ÷ all items over the range |
| | ffprobe runs (range) | `increase(ferrofin_media_probe_total[$__range])` |
| | How to read this dashboard | text |
| Scans | Scans by trigger and result (stacked bars; failed red, stopped orange) | `increase(ferrofin_library_scans_total)` |
| | Scan duration p50 / p95 / max by trigger | histogram quantiles over 30 min steps; max from the last-duration gauge |
| | Items per scan by outcome (stacked) | items ÷ passes per interval |
| | Unchanged ratio by trigger | `unchanged` ÷ all items per interval |
| Closing passes & ffprobe | Post-scan pass durations (stacked) | `…_pass_duration_seconds` sum ÷ count |
| | ffprobe runs by result | `increase(ferrofin_media_probe_total)` |
| Metadata providers | Requests by provider and result (failed/skipped red) | `rate(ferrofin_metadata_provider_requests_total)` |
| | Failed or skipped requests (range) | same, `result=~"failed\|skipped"` |
| | Provider retries | `increase(ferrofin_metadata_provider_retries_total)` |
| Library | Library items by type | `ferrofin_library_items` |
| | Item refreshes served mid-scan | `increase(…_lane_refreshes_total)` |

The per-interval panels take `increase()` over `$__rate_interval` (the step plus one
scrape), so a scan ending on a step boundary is never lost; the price is that one
landing in that overlap can show in two neighbouring bars. The "last scan" stats read
back 7 days with `last_over_time`: the gauges only exist once a scan has completed
since the last start, so without the lookback a restart would blank the panel (and hide
a scheduled scan that stopped running) instead of letting it turn red. Both are instant
queries, so each shows the current value only (a range query reduced to its last value
would keep a tile for a pod replaced earlier in the dashboard range) and the 7-day
lookbacks are evaluated once, not at every step. *Time since last scheduled scan* is per
instance, so one stale server is not hidden behind a healthy one, but only over the
instances Prometheus scrapes now (`up`): a rollout gives the new pod a new `instance`,
and the replaced pod must drop out rather than linger red for 7 days. The new pod, with
no scan of its own yet, shows the latest completion of the instances its job has
*departed* — never a live sibling's, so a server that has never finished a scan shows
no value rather than a healthy neighbour's. *Last scan duration* likewise takes the
duration whose completion is newest across the job's pods, not the largest one any pod
left behind.

**What healthy looks like.**

- A **scheduled rescan of an unchanged library**: *Items per scan* is all `unchanged`
  (≈ the library size, cf. *Library items by type*) with `created`/`updated`/`removed`
  ≈ 0; *ffprobe runs* ≈ 0; provider requests flat except for items still missing
  remote metadata (an overview, artwork) — the scan asks the providers again for those
  on every run until one answers, so a few `ok`/`not_found` requests per scan are
  normal; duration ≈ the time to walk the library.
- A **watcher or webhook scan**: `created` 1 (or `removed` 1), one ffprobe run, that
  item's provider requests, tens of milliseconds.
- **Red flags**: `failed` scans or probes; `skipped` provider requests (a provider is
  rate-limiting us or down — its circuit is open); `updated` ≈ the library size on a
  rescan (change detection is not holding); *Time since last scheduled scan* past the
  task interval (12 h by default).

To check a server on demand instead of waiting for its next scheduled scan, run
[`verify/scan-behaviour.sh`](../../verify/README.md): it drives scratch changes through one
library and asserts on these same counters.

## Divergences — .NET metrics deliberately NOT ported (never faked)

These are .NET-runtime internals with no Rust equivalent. Jellyfin emits them from
`prometheus-net.DotNetRuntime`; Ferrofin does **not** stub them with zeros — their
absence is documented, honest divergence:

- `dotnet_total_memory_bytes`, `dotnet_collection_count_total`
- `dotnet_gc_*` (GC pauses, heap sizes, allocation rates)
- `dotnet_jit_*` (JIT compilation)
- `dotnet_threadpool_*` (→ use `ferrofin_tokio_*` instead)
- `dotnet_contention_*` (lock contention)
- `dotnet_exceptions_*` (exception counts)
- `prometheus_net_*` (the .NET exporter's own internal metrics)

## Regenerating the parity fixture

`jellyfin-metrics-fixture.txt` is the empirical oracle for exact label sets and
histogram buckets. Capture it from a live Jellyfin (benchmark leg, port 18097,
`EnableMetrics: true`):

```bash
curl -s http://localhost:18097/metrics > contrib/metrics/jellyfin-metrics-fixture.txt
```

Where the fixture disagrees with a table above, **the fixture wins** — adjust
`ferrofin-metrics` (and `RULES_METRICS.md`) to it.
