
> **Living document.** Mandatory rules for logging across the `ferrofin-*` workspace.
> These govern every `tracing` statement and span, now and later. The
> implementation these assume is specified in
> the tracing-subscriber setup (`apps/ferrofin-server/src/bootstrap.rs`). Logs share the pipeline with traces
> ([TRACING.md](TRACING.md)): a log event inside a span inherits its
> fields, so good spans make good logs.

# Logging conventions

Ferrofin logs through `tracing` to two sinks: **structured JSON on stdout** (default;
Alloy → Loki) and a **plain-text daily-rotating file** under `{data_dir}/log`
(always text — `GET /System/Logs` renders it in the dashboard, a parity surface).
`FERROFIN_LOG_FORMAT=text` switches stdout to the human-readable tee for interactive
dev. Neither of these may change.

## 1. Levels mean things

- `error!` — an **actionable failure**: something is broken and someone should look.
- `warn!` — **degraded but handled**: a fallback was taken, a retry scheduled, or bad
  client input worth noticing.
- `info!` — **lifecycle / state changes**, bounded volume at steady state.
- `debug!` — **per-item detail**.
- `trace!` — **hot-loop internals**.

If a level's output volume scales with library size or request rate, it is too high:
per-item detail goes to `debug!`, progress goes to a periodic `info!` (the
intro-skipper per-season pattern, commit `aab8815`, is the precedent). **No level
whose volume is O(items) may sit above `debug`.**

## 2. Errors are logged exactly once

At the **outermost layer that still has context** — never also at the origin.

- **Request paths**: the existing `ApiError` boundary
  (`ferrofin-api/src/error.rs`) logs 5xx once. Do **not** add origin logging on request
  paths — that double-logs.
- **Background paths** (spawned tasks, samplers, schedulers): at the task's own top
  level, with the span active.
- `let _ =` / `.ok()` on a fallible call is allowed **only** when failure is truly
  meaningless. Otherwise `if let Err(e) = … { tracing::debug!(error = %e, …) }` at
  minimum — pick the level by rule 1.

## 3. Spans for units of work, fields over prose

Every long-running or failure-prone unit of work gets **one** span
(`#[instrument(skip_all, fields(…))]` or manual) with identifying fields; events
inside inherit them. Follow the TRACING.md `skip_all` conventions. No span on pure
functions or one-line delegations; no per-query DB spans (sqlx events already attach).

**Standard field vocabulary — use these exact keys everywhere** so Loki/Tempo queries
work across subsystems:

| Key | Meaning |
|---|---|
| `item_id` | a media item `Uuid` |
| `user_id` | a user `Uuid` |
| `device_id` | client device id |
| `session_id` | session id |
| `play_session_id` | playback session id |
| `library` | library name/id |
| `task` | scheduled-task key |
| `provider` | metadata provider name |
| `job_id` | transcode/other job id |
| `path` | filesystem path |
| `trigger` | `api` \| `schedule` \| `startup` \| `watcher` |
| `trigger_trace_id` | originating sampled request's trace id (hex), when known |

`Uuid`s via `tracing::field::display()`. Values born inside a function:
`tracing::field::Empty` + `span.record()`.

## 4. Background work starts a new root trace, tagged with its trigger

Do **not** parent a 45-minute scan under an HTTP request span — the trace is unusable.
A `tokio::spawn`ed future MUST be wrapped with `.instrument(span)`
(`tracing::Instrument`); a span created before `spawn` does not follow the task by
itself. Record `trigger` and, when kicked off by a sampled request,
`trigger_trace_id` as a plain field (greppable join; OTel span links are optional
polish, not required).

## 5. Never log secrets

No `.expose()` in any log statement; tokens, passwords, API keys, and `Authorization`
header contents stay out — spans and events alike. Secret hygiene
(`ferrofin-model/src/secret.rs`, `secrecy`-backed) is load-bearing; keep it. Raw
filesystem paths and entity IDs **are** fine in logs (unlike metric labels — different
rules; cardinality doesn't bind logs).

## 6. The sinks are fixed

File sink stays plain text; stdout stays JSON-default. Unchanged from TRACING.md;
nothing in a log change may alter either.

## 7. Panics must be visible

The panic hook installed in `init_tracing` logs every panic through `tracing` before
delegating to the previous hook. **Never remove it "temporarily."** Spawned tasks that
can panic must surface it — check the `JoinHandle` or wrap the future body so a panic
logs `error!` with the `task` field instead of vanishing into a dead handle.
