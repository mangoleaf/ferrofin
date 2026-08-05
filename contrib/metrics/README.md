# Hermit metrics — Prometheus `/metrics` + Grafana

Hermit exposes Prometheus text-exposition metrics on `GET /metrics`, instrumented
through the **OpenTelemetry** API. It is a **parity port** of Jellyfin's
prometheus-net surface: every metric whose concept exists in Rust keeps Jellyfin's
exact name, labels, and buckets, so existing Jellyfin Grafana dashboards work
against Hermit unchanged.

Rules for adding/changing a metric live in `brain/rules/RULES_METRICS.md`.

## Enable it

Metrics are **off by default** and gated on the existing
`ServerConfiguration.EnableMetrics` toggle (restart required — Jellyfin semantics):

- Edit `{config_dir}/system.json`: set `"EnableMetrics": true`, or
- `POST /System/Configuration` with the flag set, or
- set the bootstrap override `HERMIT_ENABLE_METRICS=true` (env) / `enable_metrics = true`
  (in `config.toml`) — for declarative/GitOps/container deploys where editing
  `system.json` or calling the API is impractical. It wins over the persisted flag
  when set (`false` force-disables); unset defers to `system.json`. Like the sampler
  interval, it is a bootstrap knob only — NOT part of the API `ServerConfiguration`,
  so `/System/Configuration` stays byte-identical to Jellyfin.

then **restart** Hermit. Disabled ⇒ `/metrics` returns `404`, no recording
overhead, no background sampler. The endpoint is unauthenticated when enabled

Optionally align the gauge sampler with a non-default Prometheus scrape interval
via the bootstrap knob `HERMIT_METRICS_SAMPLE_INTERVAL` (env) or
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

## Scrape + dashboard

```bash
prometheus --config.file=contrib/metrics/prometheus.yml     # scrapes localhost:8096, 15s
# then import contrib/metrics/grafana-dashboard.json into Grafana (uid: hermit-overview)
```

The dashboard's `http_*` / `process_*` panels also light up when pointed at a
Jellyfin instance with metrics enabled (benchmark leg, port 18097) — that is the
parity proof.

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

### Hermit-specific (`hermit_*` — no Jellyfin equivalent)

| Metric | Type | Labels | Source |
|---|---|---|---|
| `hermit_sessions_active` | gauge | — | session snapshot |
| `hermit_playback_streams_active` | gauge | — | sessions with a now-playing item |
| `hermit_playback_streams` | gauge | `method` (`Transcode`/`DirectStream`/`DirectPlay`) | playing sessions by play method |
| `hermit_transcode_jobs_active` | gauge | — | sessions with active transcode info |
| `hermit_db_pool_connections` | gauge | `pool` (`read`/`write`) | sqlx pool size |
| `hermit_db_pool_idle_connections` | gauge | `pool` (`read`/`write`) | sqlx idle count |
| `hermit_library_items` | gauge | `type` (`BaseItemKind`) | `BaseItems` grouped by type (sampled ~60s) |
| `hermit_uptime_seconds` | gauge | — | since sampler start |
| `hermit_tokio_workers` | gauge | — | tokio runtime worker count |
| `hermit_tokio_alive_tasks` | gauge | — | tokio runtime alive-task count |

`hermit_tokio_*` is the honest analogue of `dotnet_threadpool_*`.

## Divergences — .NET metrics deliberately NOT ported (never faked)

These are .NET-runtime internals with no Rust equivalent. Jellyfin emits them from
`prometheus-net.DotNetRuntime`; Hermit does **not** stub them with zeros — their
absence is documented, honest divergence:

- `dotnet_total_memory_bytes`, `dotnet_collection_count_total`
- `dotnet_gc_*` (GC pauses, heap sizes, allocation rates)
- `dotnet_jit_*` (JIT compilation)
- `dotnet_threadpool_*` (→ use `hermit_tokio_*` instead)
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
`hermit-metrics` (and `RULES_METRICS.md`) to it.
