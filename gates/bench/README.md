# Benchmark methodology

This directory produces the comparison table in the root README: Ferrofin against
the Jellyfin release the branch pins (`UPSTREAM_TAG`), on identical test data, on one host.
It also holds the [release gate](#release-gate), which compares Ferrofin with its previous release. Archived runs retain
their recorded labels and raw measurements. Re-rendering archived results applies the current
validation rules: previously published cells can become flagged (including archived
Ferrofin detail/playback responses missing `IsOriginal`/`LocalizedOriginal`). Review
an unpublished preview before replacing any published table. Every number has a definition below. New published results must pass the
comparability, phase and resource checks described here:

- **comparable** — the server returned the same status and record count as Jellyfin,
  matching content type, counts, ordered item IDs and per-item field types for
  every validated request key; extra object fields are allowed. Required image requests
  must have matching keys/types and successful, non-empty responses. Image byte sizes
  are recorded as diagnostics and never compared for equivalence.
  A server that returns fewer records, fewer fields, or an error would be "faster" for
  free; such a cell is marked `⚠[n]` — the number stays for the work list rather than
  being published, and note `n`, printed once at the end with every cell that points at
  it, says what differed. A window in which k6 could not hold the arrival rate, a
  Jellyfin-side failure, or a transcode with different parameters is flagged the same way.

Inventory totals (including person counts) are diagnostics in the work list, not latency
gates. Missing inventory evidence is also reported there. Actual request/response counts,
shapes, selections and failures still determine the affected latency cells' comparability.

There is deliberately **no** second rule about run-to-run spread. Where the runs
disagreed, the cell reports the range they spanned — in brackets in the markdown, beneath
the number in the viewer's headline tiles, and on the cell's hover text in its tables —
and the reader judges it. Where they agreed at the precision published, nothing is
printed: agreement is the absence of a range, not a claim about one.

The owner withdrew the fixed 15% spread gate on 2026-09-04: tail variation was not a
reliable pass/fail test. Reports retain ranges for review instead.

The benchmark reference is the Jellyfin release named by the C# source-parity pin
(`UPSTREAM_TAG` in `handlers.rs`), pinned by digest in `testdata/jellyfin-image.txt`.
The pin is the latest Jellyfin release when a release branch is cut, and the branch keeps it:
a release is measured against the Jellyfin release its branch pins, and the next release
branch moves to the newest one (owner decision 2026-10-25). `oracle.py check` compares the tag
with the latest Jellyfin release tag (release candidates excluded). When the pin is behind or
the lookup fails, it notes so ("measured against Jellyfin v12.2, the release this branch pins;
v12.3 is out") and nothing is refused: neither `run.sh` nor a release-gate session
(`gate.py aa|ab|scan|ramp|soak|endpoints`). Only a pin it cannot read is refused. Each run records
the oracle, note included, in `run.json`, and its `report.html` names the tag, digest and note;
a gate session records them in `gate.json` and its verdict JSON, and names them in its report.
A run that boots Jellyfin also stops unless the pinned image reports the `UPSTREAM_TAG` release
(`oracle.py matches`), so the digest cannot drift from the tag. Moving the pin means moving
`UPSTREAM_TAG`, pinning that release's image and preparing the corpus again.
The owner requested plugin checks and background scan suppression; the isolation
policy below implements that request on disposable configurations.

## What is measured

**Screens.** Load is expressed as *screens a user opens*, each being exactly the request
set jellyfin-web issues for it (sources cited in `screens.js`), fired concurrently
as a browser does (six connections per host), then the first twelve poster images the
cards would load:

| screen | requests |
|---|---|
| home | `/Users/{u}/Views`, `/Users/{u}/Items/Resume` ×3 (video, audio, book), `/Shows/NextUp`, `/Users/{u}/Items/Latest` ×3 (one per library) |
| movies | `/Users/{u}/Items` — a random page of 100, sorted by name (the library view) |
| detail | `/Users/{u}/Items/{id}`, `/Items/{id}/Similar`, `SpecialFeatures`, `LocalTrailers` — a random movie from the first 500 by name |
| series | `/Users/{u}/Items/{id}`, `/Shows/{id}/Seasons`, `/Shows/{id}/Episodes` (first season), `/Items/{id}/Similar` |
| search | the global search set: `/Items` (all types, limit 800), `/Items` (videos), `/Persons`, `/Artists`, `/Items` (programs) |
| playback | `POST /Items/{id}/PlaybackInfo` (direct-play profile), `Intros`, `MediaSegments`, `POST /Sessions/Playing`, `POST /Sessions/Playing/Stopped` |

Screens are opened **open-loop** (k6 `constant-arrival-rate`): the next user never waits
for the previous one, so slow responses do not throttle arrivals and the tails are real.
The mix is home 3 : movies 2 : detail 2 : series 1 : search 1 : playback 1, in a fixed
order with shared primary picks. Response-dependent image and episode picks are
recorded and checked; differing selections flag the affected screen.
Three levels are published: **unloaded** (1 screen/s), **loaded** (5 screens/s — about
24 API requests/s plus up to ~55 poster requests/s) and **stress** (25 screens/s, five
times that), which exists to push both servers past a comfortable browse. All three are
fixed arrival rates, because comparing two servers doing the same work is a different
question from finding one server's knee; the knee has its own opt-in, Ferrofin-only phase,
the [saturation ramp](#throughput-ceiling-the-saturation-ramp), and `RATE_STRESS` is the
knob if neither server bends at the stress rate. Each window is 120 s after a 30 s
warm-up at the same rate (different picks) that is discarded, and a window in which k6 could not hold its rate is flagged, not published.

## Definitions (one sentence each)

- **Screen latency** — time from issuing a screen's first request to receiving its last
  response, p50 / p95 / p99 over the window; **endpoint latency** — `http_req_duration`
  per request name inside the screens; **err** — share of non-2xx/transport failures,
  invalid image responses, or missing playback/episode dependencies (legacy runs
  checked non-2xx/3xx and transport failures only).
- **Cold start** — milliseconds from the container process start (`docker inspect
  .State.StartedAt`) to the first authenticated `200` on `GET /UserViews` (the home-screen
  query), polled every 10 ms with each attempt's connect abandoned after
  `COLDSTART_ATTEMPT_TIMEOUT_MS` (the response is not cut short), on a *restart* of a
  server that had already booted this data, with host caches retained; stop completes
  before the poller starts, and polling begins before `docker start`; stop duration and
  first provisioning/adoption are excluded, and the median of 5 restarts is the run's
  number, not full-screen rendering.
- **HLS first segment** — milliseconds from `POST PlaybackInfo` (a device profile that
  cannot direct-play the file: vp9/webm only, 2 Mbps cap on an 8 Mbps h264 source; no
  subtitle) through `master.m3u8` and the variant playlist to the last byte of the first
  segment; the median of 5 (fresh play session each, encoding killed afterwards) is the
  run's number; `ffprobe` checks the received segment after the timer stops, recording
  codecs, dimensions, audio properties and duration; every repetition's selected
  parameters and output properties are compared, with differences flagged.
- **Direct-play TTFB** — milliseconds to the first byte of `GET /Videos/{id}/stream?static=true`
  with `Range: bytes=0-1048575`; the median of 5 is the run's number, eligible only
  with HTTP 206, the expected Content-Range and exactly 1,048,576 received bytes;
  EOF before the first byte is failure.
- **Peak memory** — the maximum of cgroup v2 `memory.stat anon` (heap and stacks of the
  server and its ffmpeg children; page cache excluded) sampled every 100 ms across the
  loaded and stress windows — not the unloaded one, which is the control rather than
  load; **steady memory** — the median of the same over the 60 s idle after them, which
  the run drains into.
  Every server runs under an 8 GiB cgroup limit with swap disabled (`--memory-swap` =
  `--memory`; the sampler records `memory.swap.current` to prove it stayed 0), which is
  part of the definition (.NET sizes its GC heap from it).
- **CPU seconds** — change in the raw cgroup `usage_usec` counter between the
  bracketing samples, divided by one million, for the server and its children; load
  windows include k6 setup/teardown and harness overhead, and steady is post-load idle.
- **Throughput ceiling** — the highest loaded-mix arrival rate, in screens per second,
  whose worst screen p95 stayed within 100 ms with no failed request and no dropped k6
  iteration, found by the `ramp` phase.
- **Parity** — `N / 412 operations deep-verified against <UPSTREAM_TAG>, plus M compared
  against an earlier release`: the number of contract operations whose Ferrofin
  implementation was compared against the upstream Jellyfin C# (`UPSTREAM_TAG`, the release
  this branch pins; M counts rows in `VERIFIED_AGAINST_EARLIER_PIN`, compared against an
  earlier pin and owed a re-check) for behavioral equivalence, recorded as rows of
  `handlers::VERIFIED` in
  `crates/ferrofin-api/src/handlers.rs` and printed by
  `cargo test -p ferrofin-api --test contract_superset verified_rows -- --nocapture`.
  Runtime response comparison is supporting evidence for that work, never the number.

## Test data

Built once by `testdata/build.sh` (see `testdata/gen.py` for every constant):
a seeded generator writes ~3,000 movies, 250 series (~7,500 episodes) and 800 albums
(8,000 tracks) with Kodi-style NFO metadata (5,000 people, 30 genres, 200 studios,
tmdb/imdb ids), locally drawn posters/fanart/logos, and five ffmpeg-generated template
clips cloned under every name (real streams, no disk cost). It includes 40 multi-version
movies, 50 HDR10 4K files, 300 multi-track files and one 3-minute 8 Mbps movie for the
streaming numbers. **Jellyfin 10.11.8 itself scans it** with every remote fetcher off and
is seeded over its own API (two users; 30 % of movies and 60 % of forty series played, 5 %
favorites, 60 resume positions, 200 ratings), then drained and stopped. The resulting
config directory is what Ferrofin boots a fresh copy of. Jellyfin boots a copy of the
separately prepared config described below. Ferrofin adoption stays inside the run: after
its isolation restart, each run asks Ferrofin for one full library scan, waits for it to
complete (`scan.json`) and restarts the server, outside every measured phase. That is the
equivalent of the scan the Jellyfin preparation gave its config, so album-artist people
and library view images exist on both sides, and no scan heap is left in the memory
numbers. `scan_policy: 1` in `run.json` keeps these runs from pooling with earlier ones.
Media is mounted read-only for every server.

## Shared picks and bounded validation

The seeder writes `pools` into `ids.json`: ordered movie/series IDs, search terms, the
movie count, and a fixed NextUp cutoff. The cutoff is fixed fixture input (not the current date), recorded in `ids.json` and
covered by its hash; regenerating it changes the workload. The export also records credentials for the existing seeded `viewer` playback account.
Existing fixtures can export these without regenerating media or reseeding users:

```bash
gates/bench/testdata/build.sh --export-pools
# Move an existing testdata/jellyfin preparation aside, then:
gates/bench/testdata/build.sh --prepare-jellyfin
```

Export boots a disposable 10.11.8 source copy and atomically updates only `ids.json`.
Per-run role tokens live in mode-0600 `ids-private.json`, never in report metadata;
missing or wrong role credentials fail provisioning instead of falling back to the admin.
The changed ID-file hash invalidates an earlier Jellyfin preparation. Runs refuse
missing pools; measured servers never supply their own fallback picks. The final partial
movie page is included using `ceil(movieCount / 100)`.

`SLOTS` defaults to 600 and must be a positive multiple of ten. Each iteration uses
`slot = iterationInTest % SLOTS`, the fixed ten-screen mix, and `lcg(slot + 1 + SEED)`.
All measured levels share `SEED` (default 0); warm-up uses `SEED + 900000`. The one
untimed shape pass uses `SHAPE_VUS` (default **1**) and exactly `SLOTS` shared iterations, with scenario-wide
indices so each slot runs once. Serial validation avoids concurrent cold-cache poster
encodes across screens; the server race is tracked in [IMAGE_CACHE_RACE.md](IMAGE_CACHE_RACE.md).
Setting `SHAPE_VUS` above one is an explicit concurrency experiment, not the default.
Iteration 601 wraps to slot 0. This is a warm-cache
workload, identified as revision 4 in the raw data; it cannot be pooled with the old
per-level-seed workload.

Shape records identify each request by slot, scoped name, occurrence, method, path/query
and stable request body. Image cache tags and runtime authentication/session IDs are
excluded from keys. Counts, ordered IDs (including duplicates), nested array lengths and
per-item field types are checked per response; no cross-item field union or arbitrary
value diff is used. Each image belongs to its screen, and binary byte lengths detect
empty bodies. Different nonzero JPEG sizes remain valid, unflagged diagnostics.
`/Items/{id}/Similar` (`detail:similar`, `series:similar`) is the one exception: Jellyfin
breaks equal similarity scores in arbitrary order (series in random order), and the
response carries no score, so which items fill the limit is the server's free pick. Those
responses are checked for status, counts, totals, content type and their own top-level
fields, not item IDs or per-item fields, and the posters of their items are checked by
count, status and type per slot and screen rather than by item (which assumes every
candidate has a Primary poster, as every fixture item does). A Similar response that repeats an item or returns the item
it was asked about still fails. Which items score as similar is covered by the code
comparison recorded in `handlers::VERIFIED`, not by this check.

Timed iterations emit one compact selection record after recording screen latency,
without a full structural comparison. The reporter checks observed keys and request
counts against that server's validated slot, including every dependent image and episode
request. Missing, extra or changed requests fail eligibility. Logging still consumes client
resources; a dropped iteration remains a failure. Raw logs are kept per phase for review.

`run.json` records the non-secret pools, IDs/script SHA-256 hashes, seeds, slot count and
shape VU count. Reports show measured shape duration and distinct-pick coverage, plus
image response bytes per measured level as diagnostics. Missing historical evidence is
not upgraded to bounded coverage. The 600-slot validation uses a sample of the fixture;
it is not all-endpoint coverage or a deep-parity score.

## Accuracy controls

- Each server runs alone, in a container pinned to dedicated cores (`SERVER_CPUS`,
  default 8–15) with the load generator on other cores (`CLIENT_CPUS`, 16–19); the run
  rejects overlap or shared physical cores, checks their SMT siblings too, and refuses
  to start unless the expanded set is ≥ 90 % idle. Checked CPU IDs and the observed
  idle fraction are recorded. Affinity does not reserve those CPUs. The existing
  "interference" row is the difference between observed busy time and container CPU
  time on the server CPUs; it does not establish what caused a slowdown.
- Automatic scheduled-task triggers are cleared on each disposable server configuration,
  including library scans and startup/maintenance triggers. Real-time library monitoring
  is set off before boot; automatic metadata refresh and metadata/image fetchers must
  already be off in every fixture library or the run fails. The runner restarts once
  outside timing to apply plugin unloads and verify persisted settings. Initial provisioning
  work can run before these settings take effect; it is outside measured phases.
- Before each drain, the runner verifies that triggers remain empty, library monitoring/
  fetchers remain off, and plugin states still satisfy the policy. Tasks drain to idle
  plus a 30 s settle before measurements. Fixture creation and the explicit Jellyfin 12
  preparation scan remain enabled outside benchmark runs, and Ferrofin's one explicit
  library scan runs during provisioning, followed by a restart, before every measured phase.
- Ferrofin runs as **core Ferrofin**, with every plugin/compiled-in extension disabled;
  WASM files in the disposable config are rejected. Installed Jellyfin plugins are also
  disabled. Jellyfin's non-uninstallable bundled providers (AudioDB, MusicBrainz, OMDb,
  Studio Images, TMDb, and ListenBrainz Similarity Provider) remain loaded and report `Active`: their disable endpoint fails
  when it tries to write a manifest inside `MediaBrowser.Providers.dll`. Their library
  fetchers are disabled instead; this is not a claim that those assemblies are unloaded.
  `plugins-before.json`, `plugins.json`, `tasks-before.json`, `tasks.json` and `libraries.json`
  record the policy evidence. `background_policy: 1` prevents aggregation with older runs
  that left automatic schedules and stock Jellyfin plugin settings unchanged.
- Browsing uses the seeded `bench` account; the entire playback sequence uses the seeded
  `viewer` account with its own authenticated token and device/session identity. Start/stop
  events remain in the workload. Each role still shares one session across its VUs; this
  is not a per-VU session benchmark. Additional session users are forbidden.
- Before and after shape, warm-up and load, bounded observations verify that the browse
  account's playback fields (500 movie-pool IDs) and home lists/poster selections match
  the pre-workload baseline. Playback-user changes are retained as diagnostics. No state
  is reset to conceal drift. Observations run outside measured resource/time windows;
  `state-*.json`, `isolation-*.json` and `user-roles.json` retain evidence. Selection logs
  also record the actor/user on every iteration and still check exact home request keys.
  Metadata `user_isolation: 1` prevents combining these runs with the old shared-user work.
- `phases.json` records selected phases and prerequisites as pending, running, completed,
  failed or skipped, with timestamps and reasons. Failed startup stops that server;
  failed drain or warm-up skips its dependent load window. Independent selected phases
  can still produce diagnostics, and any failed required work makes the runner exit
  nonzero. Interruption leaves unfinished work identifiable and stops owned clients,
  sampler and container. Results are retained; an existing run directory is refused.
- The scripts refuse the paths of the owner's real media and server config outright
  (`/mnt/mangonas`, `/mnt/nvme0/k3s`); every server boots a fresh copy of the test data's
  `config`, and `media` is bind-mounted read-only, always.

## Timeouts and resource evidence

The limits are explicit environment tunables at the top of `run.sh`:

| setting | default | scope |
|---|---:|---|
| `READY_TIMEOUT_S` | 300 | server readiness / restart start command |
| `DRAIN_TIMEOUT_S` | 300 | scheduled-task drain |
| `HTTP_TIMEOUT_S` | 10 | runner API requests, Docker inspection and ffprobe |
| `STREAM_HTTP_TIMEOUT_S` | 120 | each streaming HTTP request |
| `PHASE_TIMEOUT_S` | 600 | each k6 or Python measurement client invocation |
| `CLEANUP_TIMEOUT_S` | 30 | client/sampler termination and owned container removal |
| `COLDSTART_ATTEMPT_TIMEOUT_MS` | 100 | the connect of each cold-start readiness attempt; a connect that stalls while the port comes up is abandoned instead of waiting out TCP's 1 s SYN retransmit, and the response keeps `HTTP_TIMEOUT_S` (runs before this setting bounded the whole attempt by 1 s and do not pool with newer ones) |
| `SAMPLE_BRACKET_INTERVALS` | 2 | maximum distance from a window boundary to its bracketing sample |
| `SAMPLE_GAP_INTERVALS` | 5 | maximum gap within that sample span |

These settings are recorded and checked before aggregation. Client process groups receive
TERM, then KILL if needed; container cleanup uses the ID created by this run. k6 retains
its bounded default HTTP request timeout, with the outer client limit above. A drain can
finish its last bounded HTTP call before reporting timeout. Fixture task drain/export
uses `PREPARE_TIMEOUT_S` (1800 by default), and so does Ferrofin's in-run library
scan. No global cleanup or host changes occur.

The sampler saves `cpu_usec` beside its interval utilization fields. Sample timestamps
retain sub-millisecond precision so catch-up observations do not collide through rounding.
Reports compare CPU-seconds, average cores busy, percentage of allocated CPU capacity busy, and CPU milliseconds per completed mixed screen side by side for each server and repetition. Average cores busy is CPU-seconds divided by observed wall seconds; capacity busy divides that by the allocated logical CPU count. CPU per screen includes failed screens and window overhead, so savings claims require matching completed work and passing validation. CPU measurements render as tables, including the runner log, rather than JSON dumps. A separate
coverage table shows observed duration, UTC boundaries, sample count, largest gap and
CPU evidence validity; missing historical counters remain unavailable. CPU seconds use the two bracketing
raw counters; they are not reconstructed from rounded utilization for archived runs.
Unbracketed windows, excessive gaps, counter resets or failed sampler exit flag resource
results. These checks do not erase otherwise valid latency. No number represents memory
allocated by an individual request. A successful first-segment probe is limited output
validation, not full decoding or sustained playback evidence.

## Instrument validations (done once, against known answers)

| instrument | known answer | measured |
|---|---|---|
| memory sampler (`mem_sample.py`) | a container that allocates and touches exactly 512 MiB | 515.2 MiB anon (interpreter overhead ≈ 3 MiB), identical across 80 samples |
| restart timer (`coldstart.py`) | a container sleeps 3.000 s before serving authenticated-view probes | 3,144–3,190 ms over 3 restarts, re-measured with the 100 ms connect bound; polling was active 42–57 ms before process start; Python startup accounts for the additional time |
| cumulative CPU (`mem_sample.py`) | a process burns 2.000 CPU seconds | 2.021 cgroup CPU seconds across 23 bracketing samples, including container command overhead |
| load client (`screens.js`) | a stub server answering every request after exactly 20 ms, at 5 screens/s | every endpoint p50 = 20 ms, p99 = 20–21 ms, max 21 ms — the client adds ≤ 1 ms |

## Throughput ceiling: the saturation ramp

`run.sh --servers ferrofin --only ramp` (opt-in: no default run includes it, and it
refuses Jellyfin) steps the loaded mix upward and reports where Ferrofin stops keeping up.
It drains the server, discards one `WARMUP_S` warm-up at the start rate, then measures
`RAMP_STEP_S` (30 s) at each rate from `RAMP_START` (25 screens/s, the stress rate Ferrofin
is known to pass) in steps of `RAMP_STEP_RATE` (10), the last step being the highest
start-plus-a-multiple-of-the-step not above `RAMP_CAP` (400; so 395). Each step is
the `screens.js` workload at that rate, with the same picks as every other window.

A step ends the ramp when, in this order of precedence, a screen's request failed
(`errors`), k6 dropped an iteration so the client rather than the server was the limit
(`client`), or the **worst screen's p95** went over `RAMP_BOUND_MS` (100 ms; `p95`). The
ceiling is the last rate before that step. The mix has no single p95 in k6's per-screen
summary, and the slowest screen is the one a client notices first, so that is the number
bounded. If no step fails before the cap the ceiling is a lower bound (`cap`); if the first
step fails there is no ceiling and the phase fails. The phase writes `ramp-params.json`,
one `k6-ramp-<rate>.json` per step and `ramp.json` (the ceiling, what ended the ramp and
every step), and the run's `report.html` gets a "Throughput ramp" section with the step
table. The decisions live in `ramp.py` and are unit-tested in `test_ramp.py`.

The release gate compares the candidate with the previous release in one window,
interleaved, exactly as the latency gate does:

```bash
python3 gates/bench/gate.py ramp ghcr.io/mangoleaf/ferrofin:vPREV ferrofin:rc --pairs 3
```

The ceiling is a higher-is-better cell, `ramp/ceiling`, judged by the `[ramp]` row of
[`gate.toml`](gate.toml) (15 % and 5 screens/s) and the same rule: the median of the
candidate's ceilings is below the previous release's by more than both thresholds, in at
least two of three pairs. A ramp that ended on the client or has no passing step cannot be
judged, and an unjudgeable gated cell blocks like a regression. These are starting values,
to be re-derived after the first two ramps. The ramp pushes the shared host harder than any
other phase: `run.sh` still refuses to start unless the server and client cores are idle,
and a run that fails stops the session.

## Memory soak

`run.sh --servers ferrofin --only soak` (opt-in: no default run includes it, and it refuses
Jellyfin) holds the loaded mix at `RATE_SOAK` (5 screens/s) for `SOAK_S` (3600 s, one hour)
while the cgroup sampler records anon memory every `MEM_SAMPLE_MS`. It drains, discards the
usual `WARMUP_S` warm-up, checks isolation before and after like the other levels, and
then runs the usual 60 s steady window. A two-minute window cannot see a leak of a few MiB
an hour, and steady anon memory has drifted by that much between releases without any gate
noticing.

The **soak slope** is the least-squares line through the anon samples of the hour after its
first `SOAK_SKIP_S` seconds (600), in MiB per hour: heap growth in the first minutes is
warm-up, not a leak. The phase writes `soak-params.json` and, once the sampler has stopped,
`soak.json` (the slope, the samples it was fitted on, the fit window and the peak); the
run's `report.html` gets a "Memory soak" section. The client timeout is extended by
`SOAK_S` for this level only. Like every window, the soak's sampler coverage is checked
(a gap of more than five sample intervals fails the run), and over an hour that is a
stricter test of the sampler than the two-minute windows are; a run that fails is reported
and never retried. The decisions live in `soak.py` and are unit-tested in
`test_soak.py`.

The release gate soaks the previous release and then the candidate back to back (one pair
by default; the cost is two hours of a quiet host):

```bash
python3 gates/bench/gate.py soak ghcr.io/mangoleaf/ferrofin:vPREV ferrofin:vX.Y.Z-rc
```

The cell is `soak/slope`, judged by the `[soak]` row of [`gate.toml`](gate.toml): the
candidate **regressed** when its slope exceeds `max_slope_mib_per_hour` (20) and the previous
release's does not, **improved** when it is the other way round, and *within noise* in every
other case, so a candidate that leaks faster than a previous release that already leaked
is not a new problem (both numbers are in the table). With more than one pair each side's
median slope is used. A soak that left no slope cannot be judged and blocks like a
regression. The 20 MiB/h limit and the 600 s skip are starting values, to be re-derived
after the first two soaks. Memory is compared only between runs in one window: transparent
huge pages shift anon memory by about 50 MiB between time windows, which is why the pair runs
back to back.

## Per-endpoint latency

The screens time 28 endpoints, all inside six jellyfin-web screens. `run.sh --only endpoints`
(opt-in: no default run includes it; either server, or both) times **every GET of the parity
request catalogue** on its own ([`gates/parity/README.md`](../parity/README.md#requests)): the sweep of
the vendored contract in `gates/parity/requests/sweep/`, which holds the `gates/bench/screens.js` request
shapes (`screens.jsonl`), and the edge cases in `gates/parity/requests/edge/`. `endpoints.py` reads them
with the parity harness's own parser, so the two catalogues cannot drift apart. HEAD requests,
writes and a write's read-backs are left out. Templates are filled from the run's `ids.json`, and
the `{found.*}` names (`gates/parity/discover.toml`) are read from the server under test; a name it does
not give skips only the endpoints that use it, with the reason. `python3 gates/bench/endpoints.py plan`
prints the schedule without a server.

After discovery comes **one shared warm-up**, every endpoint once in catalogue order, discarded.
Then each endpoint runs alone: `ENDPOINT_COUNT` sequential requests (200) or `ENDPOINT_MAX_S`
seconds (10), whichever comes first. The request that crosses the time limit still counts, and
nothing is ever topped up (the scrapped v2 suite's per-endpoint top-ups took longer than its
measurements). The pass uses one keep-alive connection, as the browse role of `screens.js` (its
token and device). A latency is the time from sending the request to having read the whole body;
each endpoint records its p50, p95 and p99, every status it was answered with, how many
answers were not the expected one, and how often the server had closed the keep-alive connection.
`HTTP_TIMEOUT_S` bounds each socket operation of a request (not the whole request), and a request
that runs past it counts as an answer with no status. The phase drains, checks isolation before and after like the
other phases, and writes `endpoints.json`; the run's `report.html` gets a "Per-endpoint latency"
section with each server's p50 / p95 / p99 and `X.Y× faster` against Jellyfin. An endpoint the
two servers answered with different statuses says so instead. `ENDPOINT_SETS` (comma-separated
paths, relative to the repository) narrows the request sets: for example
`gates/parity/requests/sweep/screens.jsonl,gates/parity/requests/edge` is the gated set alone.

Not measured, each with its reason in `SKIP` (`endpoints.py`; `plan` and the report list them):
- media streams and downloads (`/Videos/{id}/stream`, `/Audio/{id}/universal`, `/Items/{id}/Download`,
  …): they send the file, so 200 reads time the disk, and starting playback is the
  time-to-first-screen phase's number;
- the HLS routes (playlists and segments): each prepares or runs an ffmpeg transcode that outlives
  the request and would load the cores the next endpoints are timed on;
- the remote lookups (remote images, subtitles and lyrics searches): their time is the internet
  provider's, and 200 requests in a row would hammer it.

The release gate runs it interleaved, Ferrofin only, three pairs:

```bash
python3 gates/bench/gate.py endpoints ghcr.io/mangoleaf/ferrofin:vPREV ferrofin:vX.Y.Z-rc
```

Each endpoint is the cell `endpoints/<id>`, judged on each run's p50 by the `[endpoints]` row of
[`gate.toml`](gate.toml) (more than 15 % **and** more than 0.5 ms, D2) and the D4 rule: the
candidate slower in at least 2 of the 3 pairs. Only the `gates/bench/screens.js` shapes and the edge-case
GETs gate (decision DE); every other endpoint is reported, and a regression there is filed as an
issue rather than blocking. An endpoint whose answers differ between runs (another status, a status
that changed within a run, or a request filled differently) cannot be judged, and a gated one then
blocks like a regression. The session's `report.html` lists every endpoint, the gated ones first,
with both sides' p50, p95 and p99 (each the median across that side's runs).

**Wall time.** One run of the whole catalogue, measured on the shared host (2026-10-24): Ferrofin
took 77 minutes (50 s warm-up, 4,551 s measuring) and Jellyfin 115 minutes (75 s, 6,840 s). The
1,836 parameter combinations of `combos.jsonl` are nearly all of it (Ferrofin 4,302 s, Jellyfin
6,377 s): at 200 requests an endpoint, a 20 ms answer already costs 4 s, and 309 Jellyfin and 77
Ferrofin endpoints ran into the 10 s limit. The other 241 endpoints took about 4 minutes on Ferrofin
and 8 on Jellyfin. A three-pair gate session of the whole catalogue therefore takes about eight
hours; `ENDPOINT_SETS` without `gates/parity/requests/sweep/combos.jsonl` brings it to about an hour,
nearly half of it the library scan every Ferrofin boot does.

## Running it

The reporter has a fast correctness check, also run in CI:

```sh
python3 -m unittest discover -s gates/bench -p test_report.py
python3 -m unittest discover -s gates/bench -p test_ramp.py
python3 -m unittest discover -s gates/bench -p test_soak.py
python3 -m unittest discover -s gates/bench -p test_endpoints.py
```

It uses self-contained fixtures and does not start servers or measure performance.
The reporter checks every selected repetition and refuses to combine recorded workloads
or builds that differ. Missing observations and failed repetitions retain their diagnostic
numbers with a flag in full reports; flagged cells and their ratios are withheld from
the generated root README. Historical logs retain their original, limited shape evidence.
Their timing repetition count defaults to the original five unless recorded explicitly.

Needs `docker`, `k6`, `jq`, `taskset`, `python3`, `curl`, `sha256sum`, `timeout` and
`setsid` on PATH, plus `ffprobe` when selecting TTFS; `run.sh` checks before starting.
Building the test data additionally needs `ffmpeg` (with libx264 and libx265), `ffprobe`
and Python `Pillow`.

```bash
export FERROFIN_GATES_DIR=/srv/gates                # the gates' output root: absolute, a local disk, outside the repository
docker build -t ferrofin:bench .                  # the commit under test
docker pull jellyfin/jellyfin:10.11.8              # source fixture builder only
docker pull "$(cat gates/bench/testdata/jellyfin-image.txt)"   # the pinned Jellyfin reference
gates/bench/testdata/build.sh                           # source fixture, once, ~20 min → $FERROFIN_GATES_DIR/fixtures/bench-testdata
gates/bench/testdata/build.sh --prepare-jellyfin          # once per source fixture / pinned image
gates/bench/run.sh                                      # one full run, ~40 min → $FERROFIN_GATES_DIR/runs/bench-<version>/report.md
python3 gates/bench/report.py "$FERROFIN_GATES_DIR"/runs/bench-{A,B,C}   # the full tables: medians, markers, notes
make publish-results VERSION=vX.Y.Z BENCH="$FERROFIN_GATES_DIR/runs/bench-A …"
                                                  # the release's summary, the README Results block and its graphs (gates/README.md)
python3 gates/bench/report.py --serve                   # the comparison viewer at http://127.0.0.1:8097/ (every gate's runs)
```

Every location comes from the output root (the layout is in [`gates/lib.sh`](../lib.sh)); `--testdata`
and `--out` override it, and with neither the scripts stop and name `FERROFIN_GATES_DIR`.

Jellyfin preparation copies the stopped 10.11.8 config into `testdata/jellyfin/`,
boots the pinned image, explicitly requests a full library scan, and waits for that
scan's last-execution end time to advance with a successful result. An initial Idle
state does not count as completion. It verifies movie, series, episode, album and
track counts, all 40 multi-version groups, and the fixture's selected item IDs,
then drains tasks and stops the server. `PREPARE_TIMEOUT_S` (default 1800) bounds each
task-drain/scan wait; preparation is outside measured windows. The output directory
must not already exist, so a failed preparation cannot silently become a ready fixture.

`preparation.json` records the image ID, source database and ID-file hashes, server
version, scan result, counts and preparation elapsed time. Runs reject a preparation
whose source, IDs or image no longer match, and copy its config for each run. The
source 10.11.8 config and prepared Jellyfin config remain stopped and unchanged. Each
run also records `/System/Info/Public` in `system-info.json`; all report labels use that
version, falling back to `image.txt` for archived runs. Different Jellyfin versions
cannot be pooled. Preparation logs and validation evidence stay beside the prepared config.

A run is named `bench-` and the code it measured: `v0.42.1` on a tag, `v0.42.1-3-7e80268` when
the branch is that many commits past the tag, and `-dirty` appended when the working
tree had uncommitted changes at the start of the run, so the sha does not identify it
(what actually ran is recorded in each server's `image.txt` and in `run.json`). Repeats of the same code are counted as `v0.42.1-run2`,
`v0.42.1-3-7e80268-run2` — a word, so the count can never be misread as part of the
version the way a bare `-2` could.
With no tags, or outside git, the name falls back to `20260903-1412-7e80268`. The
resolved name and the dirty flag are also written into `run.json`, so they survive a
renamed directory. `--out` overrides the whole thing.

`python3 gates/bench/report.py --serve [PORT] [RUNS_DIR]` is the one local viewer
(127.0.0.1:8097, loopback only) for every harness's runs, with four tabs:

| Tab | Runs come from |
|---|---|
| Benchmark | `RUNS_DIR` (default `$FERROFIN_GATES_DIR/runs`, else `gates/bench/runs`, where runs went before the output root): every directory with a `run.json`, and the runs inside a `gate.py aa`, `ab`, `ramp`, `soak` or `endpoints` session (a directory with a `gate.json`), listed as `<session>/<run>`. A session `gate.py render` has written a `report.html` for is also listed as `<session>`, opening the gate's own report: the only page a `gate.py scan` session has, since its runs hold `scan.json` and are not listed |
| Adoption | `--adoption-dir DIR` or `FERROFIN_ADOPTION_RUNS`, else both `$FERROFIN_GATES_DIR/work` (standalone runs) and `$FERROFIN_GATES_DIR/runs` (each session's `<session>/adoption`): every directory under them holding a `report.html` (or `DIR` itself) |
| Parity | `--parity-dir DIR` or `FERROFIN_PARITY_RUNS`, else `$FERROFIN_GATES_DIR/runs`: every directory holding a `report.html` or a `result.json`; a run without the page shows its overview from `result.json` |
| Verify | `--verify-dir DIR` or `FERROFIN_VERIFY_RUNS`, else `$FERROFIN_GATES_DIR/runs`: every directory holding a `report.html` or a `result.json`; a run without the page is rendered from its `result.json` |

The output root's `runs/` holds every gate's runs, so each tab lists only the directories named for
its gate (`bench-*`, `parity-*`, `verify-*`) and those with no gate prefix, which come from before the root and so
show in every tab: rename a run moved under the root to `bench-<name>` or `parity-<name>`.

Each run is one self-contained `report.html` (no external assets, so it opens from disk
and works as a CI artifact). `run.sh` writes the benchmark's beside `report.md`
(`report.py --html RUN_DIR` rewrites it), and with it `result.json`, from the same model: the
benchmark's result in the envelope every gate shares ([`gates/result.py`](../result.py)). It
holds, per server, the median, range and each run's value of every screen and endpoint cell
at each level (with p95 and p99), time to first screen, peak and idle memory and CPU
milliseconds per completed screen, Ferrofin's ratio to Jellyfin where the cells are
comparable, and the reason a cell is withheld where they are not (the servers did different
work, or the evidence is incomplete); its verdict and counts come from the phase states. Every
`gate.py` session writes one too, holding its verdict cells (`a_median`, `b_median`, `pct`,
`verdict`, `gated`) and the blocking ones. The gates' session page reads the benchmark only
through it, and the viewer labels a run from it. The viewer renders a benchmark run that has no
`report.html` on demand: from its `result.json` when it has one (the headline tables only),
else from the run. The adoption and parity tabs list whatever `report.html` files those harnesses leave in
their directory, and a tab with no runs says so and names the flag. Pick an A and a B of the same harness to see their reports side by side;
runs of different harnesses are never compared. If a benchmark run directory holds an
`ab-verdicts.json` (`{"cells": [{"cell", "verdict", "baseline", "candidate", "unit",
"delta_pct", "reason"}]}`, `verdict` one of `regressed`, `not comparable`, `within noise`,
`improved`, and `reason` saying why a cell was not comparable), its
`report.html` gets an "A/B gate" section after the headline numbers, listing those cells,
the blocking ones first; a malformed file is an error, not an omitted section. When several runs are rendered together the
file of the first run that has one is used. `gate.py ab`, `ramp`, `soak` and `endpoints` (and `gate.py
render` of such a session) write the session's gated cells into every candidate run and
rewrite that run's `report.html`, since a `report.html` written before the file existed does
not show it. A soak cell's rule is a slope limit rather than a percentage, so its Change
column shows `—`.

On the Benchmark tab the form below the list is the earlier median compare: tick the runs to render (several =
median + ranges) and optionally one or more baseline runs. Cells stay numeric in both renderers: the
median, the range its runs spanned (in brackets in the markdown; under the number in the
viewer's tiles and on the cell's hover text in its tables), and `⚠[n]` pointing at a
numbered note. Both also print each cell's speed against the recorded Jellyfin reference as
`X.Y× faster` (memory says lighter), shown only where both numbers stand; with a
baseline the viewer adds each server's percentage change against the median of those
earlier runs of itself, and the page says how many runs each side had. A change is
coloured (green = faster/smaller) only when it crosses both the relative and the
absolute threshold for its row in [`gate.toml`](gate.toml), so run-to-run noise stays
uncoloured. It turns amber only when either side's own evidence is flagged or the server
answered differently than in the baseline; a run without Jellyfin caveats only the
Jellyfin comparison. Localhost only,
stdlib only, no JavaScript.

Tunables are the variables at the top of `run.sh` (`WINDOW_S`, `RATE_LOADED`, …); pass
`--only loaded` for a before/after on two builds, or a comma list such as
`--only counts,shape,unloaded,loaded`. Unknown or empty list entries are rejected.
The default still selects all phases (except the opt-in `ramp`, `soak` and `endpoints`) and both servers. A shared host must be quiet: stop
anything that would compete for the chosen cores before a release run.

For repeated comparisons, rotate order explicitly, for example
`--servers jellyfin,ferrofin` followed by `--servers ferrofin,jellyfin`.
Use the same phase selection and settings; affinity alone does not make a busy host quiet.

## Release gate

What blocks a release is the interleaved A/B gate in [`gate.py`](gate.py), not the
Jellyfin comparison above. [`RELEASE.md`](../../RELEASE.md) has the gate's command, wall
time, threshold table and blocking rules. `gate.py ab PREVIOUS CANDIDATE` runs Ferrofin
only: three adjacent pairs of `run.sh --servers ferrofin --only shape,unloaded,loaded`. It
judges each cell regressed, within noise or improved against the thresholds in
[`gate.toml`](gate.toml). `gate.py aa IMAGE` measures the noise floor those thresholds are
re-derived from. `gate.py scan` and `gate.py ramp` are the same interleaved pairs for the
[scan and migration](#scan-and-migration) and [throughput ceiling](#throughput-ceiling-the-saturation-ramp)
phases; RELEASE.md says which of them block. The three-server runs with both Jellyfins are for publication: they produce
the root README and `docs/BENCHMARKS.md` tables and never decide a release.

Cold start is not part of the gate and keeps the definition above: a restart until the
first authenticated `GET /UserViews` succeeds, measured in full runs and never replaced by an
unauthenticated probe such as `/System/Ping`.

## Scan and migration

`gate.py scan PREVIOUS CANDIDATE` measures library-scan and adoption-migration cost, A/B
against the previous release in the same window: PREVIOUS CANDIDATE … as interleaved pairs
(`--pairs`, default 3), each run being `scan.py` on that image ID. It needs the same test data
as `run.sh` (`--testdata`, default `$FERROFIN_GATES_DIR/fixtures/bench-testdata`), and the server gets `run.sh`'s cores and
memory limit after the same idle-core check.

- **Scan wall time**: seconds from `POST /Library/Refresh` to the RefreshLibrary task
  reporting a newer successful run (polled every 0.5 s). The server starts on an empty data
  dir: startup wizard, the three bench libraries set up as `testdata/seed.py` builds them
  (no remote fetchers, trickplay or chapter images), tasks drained plus `SETTLE_S` idle,
  then the scan.
- **Migration wall time**: seconds from the container's start to its first authenticated
  `200` on `GET /UserViews`, polled every 10 ms, on a fresh copy of the test data's config
  (its libraries' realtime monitor turned off, as `run.sh` provisions every copy).
  That is the bench corpus's own Jellyfin 10.11.8 database (about 26,000 items), so the
  boot runs the whole adoption and migration chain. The synthetic fixtures under
  `gates/adoption/` hold a few dozen items each: they check correctness, not cost.
- **Peak anon**: the largest cgroup `anon` sample (`mem_sample.py`, every 100 ms) inside
  each timed span. For migration, sampling starts once `docker start` returns, so the first
  moments of the boot can be missed.

After each measurement the movie, series, episode, album and track counts are recorded in
`scan.json` and must be at least `media/summary.json`'s, so a server that finds or keeps less
is never faster; a run that fails stops the session. More is allowed and the two images'
counts are not compared: v1.3.2's fresh scan lists the 40 alternate versions as movies of
their own (3,041), and it must stay measurable as a previous release. The test data must
hold at least 10,000 items. Every cell gates: wall time by the `[scan]` row of
[`gate.toml`](gate.toml), peak anon by the `[memory]` row, with the A/B verdict rule. The
session directory holds `report.md`, `report.html`, `verdict.json` and `result.json`; each run directory
holds `scan.json`, and each measurement's `server.log`, `mem.csv` and data dir.
