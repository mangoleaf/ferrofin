# Benchmark methodology

This directory produces the comparison table in the root README: Ferrofin against
Jellyfin 12.0.0 (the reference for future runs) and Jellyfin 10.11.8 (the vendored API
contract), on identical test data, on one host. Archived runs retain their recorded
12.0-rc7 labels and measurements. Every number has a definition below. New published results must pass the
comparability, phase and resource checks described here:

- **comparable** — the server returned the same status and record count as Jellyfin
  12, matching content type, counts, ordered item IDs and per-item field types for
  every validated request key; extra object fields are allowed. Required image requests
  must have matching keys/types and successful, non-empty responses. Image byte sizes
  are recorded as diagnostics and never compared for equivalence.
  A server that returns fewer records, fewer fields, or an error would be "faster" for
  free; such a cell is marked `⚠[n]` — the number stays for the work list rather than
  being published, and note `n`, printed once at the end with every cell that points at
  it, says what differed. A window in which k6 could not hold the arrival rate, a
  Jellyfin-side failure, or a transcode with different parameters is flagged the same way.

There is deliberately **no** second rule about run-to-run spread. Where the runs
disagreed, the cell reports the range they spanned — in brackets in the markdown, beneath
the number in the viewer's headline tiles, and on the cell's hover text in its tables —
and the reader judges it. Where they agreed at the precision published, nothing is
printed: agreement is the absence of a range, not a claim about one.

A fixed 15 % band was tried as a **reproducible** verdict and withdrawn (owner,
2026-09-04). It failed 57 of Ferrofin's 110 cells, and the failures sit almost entirely
in the tails. Counting each failing number rather than each cell — a latency cell prints
p50, p95 and p99 — those 57 cells hold 80 failures, of which **69 are p95 or p99** and
only 11 are a median. A percentage of the median cannot tell a tail that moved because
the host hiccuped from a tail that is honestly long, so the band was answering a question
nobody had asked.

It was not a busy-host artefact either. An idle trio (`quiet-1..3`) failed 41 of 76, and
over every cell both trios produced it failed *more* often than the working trio did —
54 % against 45 % (57 % against 46 % on the load levels alone). A wide range is still a
reason not to publish a number; that is now a judgement made with the range in front of
you rather than by a threshold.

## What is measured

**Screens.** Load is expressed as *screens a user opens*, each being exactly the request
set jellyfin-web 10.11.8 issues for it (sources cited in `screens.js`), fired concurrently
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
fixed arrival rates rather than a ramp: finding one server's knee is a different question
from comparing two servers doing the same work, and `RATE_STRESS` is the knob if neither
bends. Each window is 120 s after a 30 s warm-up at the same rate (different picks) that
is discarded, and a window in which k6 could not hold its rate is flagged, not published.

## Definitions (one sentence each)

- **Screen latency** — time from issuing a screen's first request to receiving its last
  response, p50 / p95 / p99 over the window; **endpoint latency** — `http_req_duration`
  per request name inside the screens; **err** — share of non-2xx/transport failures,
  invalid image responses, or missing playback/episode dependencies (legacy runs
  checked non-2xx/3xx and transport failures only).
- **Cold start** — milliseconds from the container process start (`docker inspect
  .State.StartedAt`) to the first authenticated `200` on `GET /UserViews` (the home-screen
  query), polled every 10 ms, on a *restart* of a server that had already booted this
  data, with host caches retained; stop completes before the poller starts, and polling
  begins before `docker start`; stop duration and first provisioning/adoption are excluded,
  and the median of 5 restarts is the run's number, not full-screen rendering.
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
- **Parity** — `N / 412 operations deep-verified`: the number of contract operations whose
  Ferrofin implementation was compared against the upstream Jellyfin C# (`v12.0-rc7`) for
  behavioral equivalence, recorded as rows of `handlers::VERIFIED` in
  `crates/ferrofin-api/src/handlers/mod.rs` and printed by
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
is seeded over its own API (two users; 30 % of movies and 60 % of forty series played,
5 % favorites, 60 resume positions, 200 ratings), then drained and stopped. The resulting
config directory is what Jellyfin 10.11.8 and Ferrofin boot a fresh copy of. Jellyfin
12 boots a copy of the separately prepared config described below. Ferrofin adoption
stays inside the run; media is mounted read-only for every server.

## Shared picks and bounded validation

The seeder writes `pools` into `ids.json`: ordered movie/series IDs, search terms, the
movie count, and a fixed NextUp cutoff. Existing fixtures can export these without
regenerating media or reseeding users:

```bash
bench/testdata/build.sh --export-pools
# Move an existing testdata/jellyfin12 preparation aside, then:
bench/testdata/build.sh --prepare-jellyfin12
```

Export boots a disposable 10.11.8 source copy and atomically updates only `ids.json`.
The changed ID-file hash invalidates an earlier Jellyfin 12 preparation. Runs refuse
missing pools; measured servers never supply their own fallback picks. The final partial
movie page is included using `ceil(movieCount / 100)`.

`SLOTS` defaults to 600 and must be a positive multiple of ten. Each iteration uses
`slot = iterationInTest % SLOTS`, the fixed ten-screen mix, and `lcg(slot + 1 + SEED)`.
All measured levels share `SEED` (default 0); warm-up uses `SEED + 900000`. The one
untimed shape pass uses ten VUs and exactly `SLOTS` shared iterations, with scenario-wide
indices so each slot runs once. Iteration 601 wraps to slot 0. This is a warm-cache
workload, identified as revision 4 in the raw data; it cannot be pooled with the old
per-level-seed workload.

Shape records identify each request by slot, scoped name, occurrence, method, path/query
and stable request body. Image cache tags and runtime authentication/session IDs are
excluded from keys. Counts, ordered IDs (including duplicates), nested array lengths and
per-item field types are checked per response; no cross-item field union or arbitrary
value diff is used. Each image belongs to its screen, and binary byte lengths detect
empty bodies. Different nonzero JPEG sizes remain valid, unflagged diagnostics.

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
- Scheduled tasks are drained to idle plus a 30 s settle before every window (after
  provisioning, after the cold-start restarts, before each load level, before TTFS), and
  item counts are read after the first drain (startup tasks mutate libraries on boot).
- Ferrofin runs as **core Ferrofin**: after its provisioning boot the run disables every
  plugin `GET /Plugins` lists (the compiled-in extensions and the remote-provider plugins,
  which the test data already disables per library) and refuses to proceed if a WASM
  plugin is present; the persisted flags survive the cold-start restarts and are recorded
  in `plugins.json`. Jellyfin runs stock.
- Every virtual user shares one device id, so all load collapses into one server session
  (a realism simplification). Playback runs during shape, warm-up and measured phases;
  it can change later home/resume/image selections. These changes are flagged, without
  an implicit state reset or a claim of value parity under concurrent mutations.
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
| `SAMPLE_BRACKET_INTERVALS` | 2 | maximum distance from a window boundary to its bracketing sample |
| `SAMPLE_GAP_INTERVALS` | 5 | maximum gap within that sample span |

These settings are recorded and checked before aggregation. Client process groups receive
TERM, then KILL if needed; container cleanup uses the ID created by this run. k6 retains
its bounded default HTTP request timeout, with the outer client limit above. A drain can
finish its last bounded HTTP call before reporting timeout. Fixture task drain/export
uses `PREPARE_TIMEOUT_S` (1800 by default). No global cleanup or host changes occur.

The sampler saves `cpu_usec` beside its interval utilization fields. Reports show actual
observation boundaries, sample count and largest gap. CPU seconds use the two bracketing
raw counters; they are not reconstructed from rounded utilization for archived runs.
Unbracketed windows, excessive gaps, counter resets or failed sampler exit flag resource
results. These checks do not erase otherwise valid latency. No number represents memory
allocated by an individual request. A successful first-segment probe is limited output
validation, not full decoding or sustained playback evidence.

## Instrument validations (done once, against known answers)

| instrument | known answer | measured |
|---|---|---|
| memory sampler (`mem_sample.py`) | a container that allocates and touches exactly 512 MiB | 515.2 MiB anon (interpreter overhead ≈ 3 MiB), identical across 80 samples |
| restart timer (`coldstart.py`) | a container sleeps 3.000 s before serving authenticated-view probes | 3,162–3,168 ms over 3 restarts; polling was active 55–58 ms before process start; Python startup accounts for the additional time |
| cumulative CPU (`mem_sample.py`) | a process burns 2.000 CPU seconds | 2.021 cgroup CPU seconds across 23 bracketing samples, including container command overhead |
| load client (`screens.js`) | a stub server answering every request after exactly 20 ms, at 5 screens/s | every endpoint p50 = 20 ms, p99 = 20–21 ms, max 21 ms — the client adds ≤ 1 ms |

## Running it

The reporter has a fast correctness check, also run in CI:

```sh
python3 -m unittest discover -s bench -p test_report.py
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
docker build -t ferrofin:bench .                  # the commit under test
docker pull jellyfin/jellyfin:10.11.8
docker pull "$(cat bench/testdata/jellyfin12-image.txt)"  # immutable Jellyfin 12.0 image
bench/testdata/build.sh                           # source fixture, once, ~20 min
bench/testdata/build.sh --prepare-jellyfin12        # once per source fixture / pinned image
bench/run.sh                                      # one full run, ~40 min → bench/runs/<version>/report.md
python3 bench/report.py bench/runs/A bench/runs/B bench/runs/C   # the full tables: medians, markers, notes
python3 bench/report.py --readme README.md bench/runs/A bench/runs/B bench/runs/C
                                                  # rewrites the README "Benchmarks" block (headline table + prose)
python3 bench/report.py --serve                   # the comparison viewer at http://127.0.0.1:8097/
```

Jellyfin preparation copies the stopped 10.11.8 config into `testdata/jellyfin12/`,
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
source 10.11.8 config and prepared 12 config remain stopped and unchanged. Each run
also records `/System/Info/Public` in `system-info.json`; all report labels use that
version, falling back to `image.txt` for archived runs. RC7 and stable runs cannot be
pooled. Preparation logs and validation evidence stay beside the prepared config.

A run is named for the code it measured: `v0.42.1` on a tag, `v0.42.1-3-7e80268` when
the branch is that many commits past the tag, and `-dirty` appended when the working
tree had uncommitted changes at the start of the run, so the sha does not identify it
(what actually ran is recorded in each server's `image.txt` and in `run.json`). Repeats of the same code are counted as `v0.42.1-run2`,
`v0.42.1-3-7e80268-run2` — a word, so the count can never be misread as part of the
version the way a bare `-2` could.
With no tags, or outside git, the name falls back to `20260903-1412-7e80268`. The
resolved name and the dirty flag are also written into `run.json`, so they survive a
renamed directory. `--out` overrides the whole thing.

The viewer lists every run under `bench/runs`; tick the runs to render (several =
median + ranges) and optionally a baseline. Cells stay numeric in both renderers: the
median, the range its runs spanned (in brackets in the markdown; under the number in the
viewer's tiles and on the cell's hover text in its tables), and `⚠[n]` pointing at a
numbered note. Both also print each cell's speed against the recorded Jellyfin 12 reference as
`X.Y× faster` (memory says lighter), shown only where both numbers stand; with a
baseline the viewer adds each server's percentage change against an earlier run of
itself (green = faster/smaller). Localhost only,
stdlib only, no JavaScript.

Tunables are the variables at the top of `run.sh` (`WINDOW_S`, `RATE_LOADED`, …); pass
`--only loaded` for a before/after on two builds, or a comma list such as
`--only counts,shape,unloaded,loaded`. Unknown or empty list entries are rejected.
The default still selects all phases and all three servers. A shared host must be quiet: stop
anything that would compete for the chosen cores before a release run.

For repeated comparisons, rotate order explicitly, for example
`--servers jellyfin12,ferrofin` followed by `--servers ferrofin,jellyfin12`.
Use the same phase selection and settings; affinity alone does not make a busy host quiet.
