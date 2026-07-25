---
name: api-status
description: >-
  Generate Hermit's API implementation-status report against the vendored
  Jellyfin OpenAPI contract: every operation classified REAL / PARTIAL / HOLLOW /
  STUB(501), a per-controller table, the hollow-endpoint "landmine" list, and a
  summary. Use when asked for "API status", "implementation status", "what
  endpoints actually work", "contract coverage", "audit the API surface", "how
  much of Jellyfin is done", or "/api-status".
---

# API implementation-status report

Produce an honest status of Hermit's API vs. the Jellyfin contract. Run from the
repo root (`/home/mango/dev/hermit`).

## The one thing that makes this report honest

`handlers::REAL_ROUTES` is a **hand-maintained list**: a route is in it because
someone wrote a handler and returns 200. That does **not** mean the handler
returns real data. A registered route can be **HOLLOW** in two ways:

1. **Hollow handler** — returns a constant regardless of input
   (`Json(QueryResult::default())`, `Json(Vec::new())`, echoes the request body).
2. **Hollow manager** — the handler calls `state.x.method()`, but the *injected
   impl* is a stub (returns empty/`None`/no-op).

A report that only counts `REAL_ROUTES` reads ~81% "done"; the real
data-backed number is ~62%. **Do not skip the depth audit (Step 2)** — the whole
value of this skill is separating "returns 200" from "returns real data".

Four states: **REAL** (data-backed, varies with input) · **PARTIAL** (real work,
documented gap) · **HOLLOW** (200 but constant/empty, or stub manager) ·
**STUB** (501).

## Step 1 — deterministic baseline (REAL vs STUB)

```bash
python3 .claude/skills/api-status/scan.py
```

This enumerates all ~412 contract ops, joins with `REAL_ROUTES`, and prints a
per-controller REAL-vs-STUB table + totals. STUB = returns 501. This is the
floor; Step 2 reclassifies some REAL rows down to HOLLOW/PARTIAL.

## Step 2 — depth audit (find the HOLLOW/PARTIAL handlers)

Fan out subagents (Agent tool, `general-purpose`) over the handler files. Batch
the ~64 files in `crates/hermit-api/src/handlers/` into ~8 groups of similar size
(see `wc -l crates/hermit-api/src/handlers/*.rs`); group by domain
(items/library, media/streaming, images/subs, users/sessions, system/admin,
by-name/metadata, playlists/tv, plugins/livetv/misc). Give each agent this exact
rubric:

> You are auditing a Rust axum API (Hermit, a Jellyfin-compatible server) for
> HOLLOW route handlers — registered, returns 200, but returns constant/empty
> data regardless of the request, so a client sees "success" but no real data.
> Audit ONLY these files: `<list>`. For each `.route(...)` in the file's
> `register(...)` fn, read the handler body and classify:
> - **REAL** = response derived from a manager/repo/DB/ffmpeg call that depends
>   on request input; response varies with data.
> - **HOLLOW** = returns a constant unconditionally (`Json(X::default())`,
>   `Vec::new()`, empty body, fixed 204 with no persisted side effect, echoes the
>   request without storing), OR calls a manager whose impl is clearly disabled/
>   stub (`Disabled*`, `Null*`, empty provider list, feature-gated-off provider).
> - **PARTIAL** = real work with an obvious gap (ignores a documented param that
>   should change results, a sub-case always empty, a posted profile dropped).
> RECORD the exact `state.<field>.<method>()` each handler calls. Be skeptical:
> do not mark REAL just because the fn is long or calls something — verify the
> returned data actually depends on a lookup. Output ONE line per route, nothing
> else: `METHOD /Path | REAL|HOLLOW|PARTIAL | <=15-word evidence | state.field.method or -`

## Step 3 — confirm the manager layer (catch hollow-at-impl)

A handler that calls a manager is only REAL if the *injected* impl is real. Check
the composition root `apps/hermit-server/src/state.rs` for the tells:

- **Underscore-bound = never wired into `AppState`** (e.g. `let _live_tv = ...`):
  the handlers for it can't be using it — they return constants. HOLLOW.
- **Empty-constructed providers**: `LocalProviderManager::new(Vec::new())` →
  remote metadata/image/subtitle search all HOLLOW.
- **`Null*` / stub structs**: `NullImageEncoder` (image resize/format = no-op),
  `HermitLyricManager` (stub — all methods return `None`/empty), `Disabled*`.
- Grep to be sure:
  `grep -rn "struct .*Manager" crates/hermit-core/src | grep -i "stub\|disabled\|null"`
  and read the impl (`Ok(None)`, `Ok(Vec::new())`, `todo!`, "deferred", "stub").

Fold any manager-layer stubs into the classifications from Step 2 (a REAL-looking
handler over a stub manager is HOLLOW).

## Step 4 — assemble and render

Write the Step 2/3 findings to a TSV (`STATE<TAB>method<TAB>/Path`, only the
HOLLOW and PARTIAL rows — everything else stays REAL or STUB), then overlay it:

```bash
python3 .claude/skills/api-status/scan.py /tmp/api-classify.tsv --list
```

That prints the final 4-state per-controller table, totals, the data-backed %,
and the STUB + HOLLOW op lists. Build the report from it:

1. **Headline** — 4-state totals + "data-backed %" vs "returns-no-real-data %".
2. **Per-controller table** — REAL / PARTIAL / HOLLOW / STUB / total.
3. **Landmine list** — split the HOLLOW ops into:
   - **Faithful-empty** (no data source configured; real Jellyfin also returns
     empty — Live TV w/o tuner, Channels w/o providers, remote providers gated
     off). Not bugs; the UI hides these.
   - **Functional gaps** (client/dashboard expects action, gets a silent no-op —
     e.g. plugin config pages, QuickConnect token, dropped toggles, image resize
     ignored). These are the ones that bite. Say *what breaks* for each.
4. **STUB (501) list** — group by: deferred core subsystems (SyncPlay, Live TV
   DVR), third-party plugins (not core Jellyfin; only in the spec because the
   reference server had them installed — IntroSkipper, OpenSubtitles, etc.), and
   scattered core gaps.
5. **Summary** — what works end-to-end, why the dashboard breaks (it's the HOLLOW
   functional gaps, not the honest 501s), and a priority order by dashboard impact.

## Notes

- `scan.py` normalizes paths param-agnostically and folds the `.{container}` /
  `.m3u8` / `/stream` suffix equivalences Hermit's router relies on, so
  `REAL_ROUTES` matches the spec paths. HEAD ops (image variants) are included.
- The classification TSV is a **snapshot** — regenerate it via Steps 2–3 each run;
  do not cache stale findings. The deterministic REAL-vs-STUB split (Step 1) is
  always current from source.
- To spot-check any single route live: run the server and `curl` it (see
  `CLAUDE.md`); a hollow route returns `200` with an empty/default body.
