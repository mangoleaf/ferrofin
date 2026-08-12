
> **Living document.** Mandatory rules for distributed tracing in the `hermit-*`
> workspace. The implementation these rules assume is specified in
> the OTLP bootstrap (`apps/hermit-server/src/bootstrap.rs`). Unlike metrics, tracing has **no
> Jellyfin parity constraint** — upstream has no tracing; this is purely additive.

# Tracing conventions

Hermit exports its existing `tracing` spans as OpenTelemetry traces over **OTLP
gRPC** (Alloy `:4317` → Tempo), enabled purely by environment. A slow request can
then be opened as a per-request waterfall in Grafana.

## 1. One pipeline per signal

Traces are the **only** signal carried on OTLP. Metrics stay on the Prometheus
scrape ([METRICS.md](METRICS.md)); logs stay on stdout + the rotating
file. **Never** enable OTLP metric or log export.

## 2. Off by default

No `OTEL_EXPORTER_OTLP_ENDPOINT` ⇒ no provider, no overhead beyond the existing
fmt subscriber. Export must never gate or alter server behaviour; exporter init
failure logs a `warn!` and the server runs on (same posture as metrics init).

## 3. Sampling is the storage knob

`ParentBased(TraceIdRatioBased(ratio))`, `ratio` from `OTEL_TRACES_SAMPLER_ARG`
(parsed f64, clamped `0.0..=1.0`), **default 0.25** — matches the fleet. Never
hardcode `1.0` in a deploy config without a reason.

## 4. `#[tracing::instrument]` conventions

- Always `skip_all`; name 1–3 meaningful fields explicitly.
- `tracing::field::Empty` + `span.record()` for values born inside the function.
- `tracing::field::display()` for `Uuid`s.
- **Never** record tokens, passwords, API keys, or `Authorization` header
  contents — anywhere, in spans or events.
- Span names are static strings; dynamic values go in fields.

## 5. Span granularity

Spans for I/O boundaries and units of work (request, DB transaction, ffmpeg
spawn, provider fetch) — not for pure functions or one-line delegations. sqlx
query logs already land as events inside the request span; don't wrap every query
in a hand-made span. Add spans opportunistically when debugging — no bulk
`#[instrument]` sweeps.

## 6. Flush on shutdown

Whoever owns the `SdkTracerProvider` calls `shutdown()` after the server drains
(`shutdown_tracing` in `bootstrap.rs`, called from `run()` after `axum::serve`
returns). A restart that loses the last spans is a bug.

## 7. Logs correlate to spans

The sampled request span carries a `trace_id` field, and every `tracing` event
inside the request inherits it — **never** stamp trace ids per-callsite by hand.

- Stdout is structured **JSON by default**; `HERMIT_LOG_FORMAT=text` restores the
  legacy human-readable tee for interactive dev.
- The rotating log **file** stays plain text always — `GET /System/Logs` feeds the
  Jellyfin dashboard log viewer, which renders raw lines (this IS a parity
  surface; JSON there would break it).
- Unsampled requests get **no** `trace_id` field — stamping one would create dead
  Tempo links in Grafana. Gate the stamp on `is_valid() && is_sampled()`.

## 8. Out of scope (deliberate — do not add)

- Inbound W3C `traceparent` extraction (no Jellyfin client sends it).
- Outbound context injection into provider HTTP calls (TMDB/OMDb) — YAGNI until
  remote providers are on by default.
- Any Hermit config-file / `ServerConfiguration` field for tracing — this is
  deployment config (env only), not user config, so `/System/Configuration` stays
  byte-identical to Jellyfin.
