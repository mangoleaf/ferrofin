
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
`FERROFIN_LOG_FORMAT=text` (`log_format = "text"` in `config.toml`) switches stdout to the
human-readable tee for interactive dev. Neither of these may change.

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
  paths — that double-logs. A handler panic is logged once by the panic boundary
  (`ferrofin-api/src/panic_boundary.rs`) that answers it `500`, beside the panic hook's
  own line (rule 7). Both mark their responses `FailureLogged`, as do the IP filter's
  `503` and `RequiredRouteString`'s `500` (`ferrofin-api/src/extract.rs`), and the
  request span (`record_status` in `ferrofin-api/src/router.rs`) logs `response failed`
  only for a 5xx without that mark: one passed through from a relayed origin, a plugin
  or a health check. A new 5xx path that logs its own failure marks its response the
  same way.
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
| `tuner` | Live TV tuner host id |
| `listings_provider` | Live TV listings provider id |
| `channel` | Live TV channel name |
| `job_id` | transcode/other job id |
| `path` | filesystem path |
| `config_file` | the bootstrap `config.toml` in use, as context (the startup banner, configuration warnings), when `path` would name the wrong subject |
| `trigger` | `api` \| `schedule` \| `startup` \| `watcher` \| `webhook` (library scans: `ScanTrigger`) |
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

A URL an operator configured is not fine. An IPTV tuner or guide URL can carry the account
in its userinfo, its query or its path, and no rule tells a credential in a path from an
ordinary one, so log the id of what it belongs to and only its scheme and host
(`ferrofin_util::source_url::redact_url`). The same goes for error text, which ends up in
a log or a `4xx` body: `reqwest::Error` prints the URL in its `Display` and `Debug`, so
take it off with `without_url()`. ffmpeg needs the real URL, and it prints that URL and
every segment URL itself, so a job reading one has its `FFmpeg.*.log`, its debug argument
line and the stderr tail in its errors filtered (`transcoding::ffmpeg_log`). The
`-user_agent` and `-referer` values go too. An ordinary file transcode's log stays byte
for byte. A `.strm` item's media path is its target URL, so whatever names that path does
the same: subtitle extraction, a failed probe and the `.strm` reader log it through
`redact_url`, and an error that quotes the input (attachment extraction) or ffmpeg's stderr
(subtitle extraction) passes it through `redact_urls_in`. A process runner's error that
quotes ffmpeg's or ffprobe's arguments (a failed spawn or run, unbalanced quoting) also hides
the `-user_agent`, `-referer` and `-headers` values (`transcoding::shown_argument_line`).

## 6. The sinks are fixed

The sinks are fixed in kind: the file sink stays plain text; stdout stays
JSON-default. Unchanged from TRACING.md; nothing in a log change may alter either.

Within the plain text, no record can be forged: the text sinks write through
`RecordLines` (`apps/ferrofin-server/src/log_lines.rs`), which turns a line feed
inside an event into an indented continuation line and a carriage return into `\r`,
so every line that starts at the left margin is a real record (owner decision
2026-10-24). Free text a client or a plugin supplies is still logged as a field,
with `?` (Debug escapes it), never as the message.

## 7. Panics must be visible

The panic hook installed in `init_tracing` logs every panic through `tracing` before
delegating to the previous hook, and records where it happened
(`ferrofin_util::panic::record_location`) for a scheduled task's `LongErrorMessage`.
**Never remove it "temporarily."** Spawned tasks that
can panic must surface it — check the `JoinHandle` or wrap the future body so a panic
logs `error!` with the `task` field instead of vanishing into a dead handle.
