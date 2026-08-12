
> **Living document.** Mandatory rules for metric collection in the `ferrofin-*`
> workspace. These govern every instrument added to the codebase, now and later.
> The implementation lives in `crates/ferrofin-metrics`; this file is the
> design contract the implementation must satisfy.

# Metrics conventions

Ferrofin exposes Prometheus text-exposition metrics on `GET /metrics`, gated by the
existing `ServerConfiguration.enable_metrics` (`EnableMetrics` in
`{config_dir}/system.json`, restart required — Jellyfin semantics). Instrumentation
is written against the **OpenTelemetry API**; the prometheus crate appears only as
an encode-time detail inside `ferrofin-metrics`.

## 1. Parity first — Jellyfin's metric surface is a contract

Jellyfin's `/metrics` (prometheus-net) is to metrics what the vendored OpenAPI spec
is to routes.

- A metric whose concept exists in Jellyfin's output keeps Jellyfin's **exact
  name, labels, and buckets**: the `http_*` family
  (`http_requests_received_total`, `http_requests_in_progress`,
  `http_request_duration_seconds` with
  `code`/`method`/`controller`/`action`/`page`/`endpoint`) and the `process_*`
  family. Existing community Jellyfin Grafana dashboards must work against Ferrofin
  unchanged. Details the live fixture pinned (do not "fix" these): `page` is
  always `""` (a Razor-Pages artifact on every series); `endpoint` is the route
  template with **no leading slash** and the spec's own param names
  (`Users/{userId}`); the duration histogram uses prometheus-net.AspNetCore's
  **exponential** buckets `0.001 × 2ⁿ` (`0.001 … 32.768`), not a round-number set.
- The oracle is empirical: `contrib/metrics/jellyfin-metrics-fixture.txt`, captured
  from a live Jellyfin with `EnableMetrics: true`. Where a table in a plan or doc
  disagrees with the fixture, **the fixture wins**.
- **Never fake a .NET metric.** `dotnet_gc_*`, `dotnet_jit_*`, `dotnet_exceptions_*`,
  `dotnet_contention_*` have no Rust equivalent — they are documented divergences
  (listed in `contrib/metrics/README.md`), not stubs emitting zeros. The honest
  analogue of `dotnet_threadpool_*` is `ferrofin_tokio_*` from
  `tokio::runtime::RuntimeMetrics`.
- The parity name-set test (metrics analogue of `contract_superset.rs`) asserts
  every portable Jellyfin metric name renders in Ferrofin's exposition. Keep it green.

## 2. The stack — what may be depended on

- Instrumentation API: `opentelemetry` / `opentelemetry_sdk` /
  `opentelemetry-prometheus` (workspace-pinned, currently 0.32).
- The `prometheus` crate is a **direct dep of `ferrofin-metrics` only**,
  `default-features = false`, used solely for `Registry` + `TextEncoder` (the
  0.32 exporter exposes no render helper). No other crate may name a
  `prometheus::` type.
- There is exactly **one recording pipeline**: OTel global meter → prometheus
  exporter → registry → `/metrics`. Never add a second path (no OTLP metric push,
  no parallel prometheus-registry helpers, no `metrics`-rs facade). A metric
  recorded outside the meter provider is invisible on the scrape — the rest
  workspace learned this the hard way; Ferrofin avoids the split by construction.
- Ferrofin is a standalone workspace: metrics are built on the `prometheus`
  crate directly, never on external in-house telemetry crates.

## 3. Naming

- Parity metrics: Jellyfin's names verbatim (rule 1). Everything Ferrofin-specific:
  `ferrofin_` prefix, snake_case, base units in the name (`_seconds`, `_bytes`,
  never `_ms`), `_total` suffix on counters.
- The instrument name **is** the final Prometheus name: the exporter is built with
  `without_units().without_counter_suffixes()`, so no auto-suffixing exists to
  rely on — and none may be reintroduced, or every parity name breaks.
- One metric name = one instrument, created in exactly one place (a static/OnceLock
  or a registration function called once at init). Creating the same name twice
  produces duplicate/undefined streams; the fix is always "hoist to the single
  definition site", never "create on demand".

## 4. Instrument types

- **Counter** for monotonic event counts. **Histogram** for durations/sizes
  (HTTP uses the fixture's bucket boundaries; other histograms justify their
  boundaries in a comment). **Gauge (observable)** for point-in-time state.
  **UpDownCounter** only for in-flight style ± tracking (`http_requests_in_progress`).
- Never encode state in a counter or events in a gauge. If a value can go down,
  it is not a counter.

## 5. Labels — bounded sets only

Cardinality is the way `/metrics` dies. Every label value set must be finite and
enumerable at review time:

- Allowed: route template, HTTP method, status code, controller/action derived
  from the vendored spec, `PlayMethod`, `BaseItemKind`, pool name (`read`/`write`).
- **Forbidden**: user IDs, item IDs, device IDs, session/play-session IDs, raw URL
  paths, filenames, client-supplied strings of any kind, tokens.
- The HTTP `endpoint` label comes from axum's `MatchedPath` (else `""`,
  prometheus-net's convention) — **never the raw request path**.
- Adding a label to any instrument requires naming its bounded value set in the
  PR/commit description.

## 6. Recording must never affect behavior

- All recording goes through the OTel **global meter**, which is a noop when
  metrics are disabled. No `if metrics_enabled` branches in business logic —
  the toggle's only effects are: mount `/metrics` + the HTTP layer, install the
  provider, spawn the sampler.
- The toggle is resolved once at startup in the composition root: the bootstrap
  override `FERROFIN_ENABLE_METRICS` (env) / `enable_metrics` (`config.toml`) wins
  when set (`false` force-disables), else the persisted
  `ServerConfiguration.EnableMetrics` (dashboard/API). The override exists for
  declarative/GitOps deploys; it stays a **bootstrap** knob and is **not** added
  to the API `ServerConfiguration`, so `/System/Configuration` stays byte-identical
  to Jellyfin. This is the one intentional divergence from "the toggle is only the
  persisted flag" — the noop-when-disabled guarantee is unchanged.
- No `unwrap`/`expect` on any recording path. Instrument-creation failures and
  sampler errors log at `debug`/`warn` and continue. A metrics bug must never
  fail a request, and metrics init failure must never prevent server startup.
- Disabled means: `/metrics` → 404 (route not mounted), zero recording overhead,
  no background sampler task.

## 7. Observable callbacks are sync — respect it

- Callbacks run on the scrape path. They may read atomics, locked maps, and cheap
  `/proc/self` files. They may **not** block, query the DB, take a tokio runtime
  handle, or `block_on` anything.
- Any async- or DB-sourced value goes through the mirror pattern:
  `GaugeCell`/`GaugeMap` written by the background sampler task (15 s interval;
  expensive queries like library counts on a slower multiple), read by the
  callback. The sampler swallows and logs its own errors.

## 8. Placement — the architecture seam

- Instruments live in `crates/ferrofin-metrics` (HTTP + process families) or the
  composition root `apps/ferrofin-server` (sampler-fed gauges). That is the default
  and covers everything shipped so far.
- `ferrofin-api` handlers and library crates must **not** depend on
  `ferrofin-metrics` — same DI principle as `ferrofin-traits` (handlers see traits,
  not impls). A subsystem that later genuinely needs deep-internal recording uses
  `opentelemetry::global::meter("ferrofin")` directly; that dep is `opentelemetry`
  only, and the noop-when-disabled guarantee (rule 6) still holds.
- Do not confuse this system with the `PlaybackMetrics` trait
  (`ferrofin-traits/src/metrics.rs`) — that is the DB-backed playback-decision log,
  a different track entirely. They stay separate.

## 9. Every metric is registered, documented, tested

- New metric ⇒ a row in the table in `contrib/metrics/README.md`, marked
  **parity** or **ferrofin-specific**; consider whether the Grafana dashboard
  (`contrib/metrics/grafana-dashboard.json`) needs a panel.
- New metric ⇒ a test asserting its name renders in the exposition output.
  Tests that touch the global meter provider or the instruments `OnceLock` go in
  their own integration binary (process-globals are set-once); everything else
  tests against a fresh `Registry`/provider. Metric names in tests are unique per
  test binary — the registry outlives each test.
- Metrics are **aggregates**. Per-request detail (which item, which user, why)
  belongs to `tracing`, not to labels.

## 10. Endpoint & exposure

- `/metrics` is served on the **app port**, like Jellyfin — not a dedicated
  metrics port. This deliberately diverges from the rest-workspace model (which
  isolates `/metrics` on `:9090` because Istio routes its app port publicly):
  Ferrofin is a LAN media server whose exposure model is Jellyfin's, and matching
  Jellyfin's behavior is the product requirement. If Ferrofin ever grows a
  public-ingress deployment story, revisit this rule before shipping it.
- The endpoint is unauthenticated when enabled (Jellyfin parity). The cardinality
  rules (rule 5) are what keep that acceptable — nothing user-identifying may
  appear in the exposition. Content-Type is
  `text/plain; version=0.0.4; charset=utf-8`.
- Scrape cadence assumption is 15 s (`contrib/metrics/prometheus.yml`); the
  sampler interval matches it (default 15 s), and is tunable via the bootstrap knob
  `FERROFIN_METRICS_SAMPLE_INTERVAL` (env) / `metrics_sample_interval` (`config.toml`)
  to align with a non-default scrape interval. Don't tighten one without the other.
  Like the enable override, this is a bootstrap knob, never a `ServerConfiguration`
  field.

## Quick checklist for adding a metric

1. Does Jellyfin expose this concept? → use its exact name/labels/buckets (check
   the fixture). Otherwise → `ferrofin_*` naming rules.
2. Right instrument type (rule 4)? Labels all bounded (rule 5)?
3. Defined exactly once; recorded via the global meter; no behavior change when
   disabled (rules 3, 6).
4. Async source? → sampler + `GaugeCell`/`GaugeMap`, never in the callback (rule 7).
5. Row in `contrib/metrics/README.md` + render test + dashboard consideration
   (rule 9).
