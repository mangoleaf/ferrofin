# Benchmark methodology

This directory produces the comparison table in the root README: Ferrofin against
Jellyfin 12.0-rc7 (the source of truth — owner decision, 2026-09-02: Jellyfin 12 is
10.12 in all but name, and where 10.11.8 differs from it, 10.11.8 is the one that is
wrong) and Jellyfin 10.11.8 (the vendored API contract), on identical test data, on one host. Every number in that table has a one-sentence definition below, and every
published number passed the one check the run itself performs:

- **comparable** — the server returned the same status and record count as Jellyfin
  12.0-rc7 and a field set that is a superset of its, for every request behind the number.
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
order with fixed picks, so every server receives the identical request sequence.
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
  per request name inside the screens; **err** — share of non-2xx/3xx or transport failures.
- **Cold start** — milliseconds from the container process start (`docker inspect
  .State.StartedAt`) to the first authenticated `200` on `GET /UserViews` (the home-screen
  query), polled every 10 ms, on a *restart* of a server that had already booted this
  data (the first boot includes Ferrofin's one-time adoption and Jellyfin 12's migration,
  and is excluded); the median of 5 restarts is the run's number.
- **HLS first segment** — milliseconds from `POST PlaybackInfo` (a device profile that
  cannot direct-play the file: vp9/webm only, 2 Mbps cap on an 8 Mbps h264 source; no
  subtitle) through `master.m3u8` and the variant playlist to the last byte of the first
  segment; the median of 5 (fresh play session each, encoding killed afterwards) is the
  run's number; if the servers chose different transcode parameters the cell says so.
- **Direct-play TTFB** — milliseconds to the first byte of `GET /Videos/{id}/stream?static=true`
  with `Range: bytes=0-1048575`; the median of 5 is the run's number.
- **Peak memory** — the maximum of cgroup v2 `memory.stat anon` (heap and stacks of the
  server and its ffmpeg children; page cache excluded) sampled every 100 ms across the
  loaded and stress windows — not the unloaded one, which is the control rather than
  load; **steady memory** — the median of the same over the 60 s idle after them, which
  the run drains into.
  Every server runs under an 8 GiB cgroup limit with swap disabled (`--memory-swap` =
  `--memory`; the sampler records `memory.swap.current` to prove it stayed 0), which is
  part of the definition (.NET sizes its GC heap from it).
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
config directory is what every server boots a fresh copy of; media is mounted read-only.

## Accuracy controls

- Each server runs alone, in a container pinned to dedicated cores (`SERVER_CPUS`,
  default 8–15) with the load generator on other cores (`CLIENT_CPUS`, 16–19); the run
  refuses to start unless those cores are ≥ 90 % idle, and records per 100 ms sample how
  much of the server's cores was *not* spent by the container (the "interference" row —
  other processes, plus the kernel's own network work for the server's traffic, which is
  a few % under load).
- Scheduled tasks are drained to idle plus a 30 s settle before every window (after
  provisioning, after the cold-start restarts, before each load level, before TTFS), and
  item counts are read after the first drain (startup tasks mutate libraries on boot).
- Ferrofin runs as **core Ferrofin**: after its provisioning boot the run disables every
  plugin `GET /Plugins` lists (the compiled-in extensions and the remote-provider plugins,
  which the test data already disables per library) and refuses to proceed if a WASM
  plugin is present; the persisted flags survive the cold-start restarts and are recorded
  in `plugins.json`. Jellyfin runs stock.
- Every virtual user shares one device id, so all load collapses into one server session
  (a realism simplification, identical for every server; it does not affect comparability).
- Every phase writes its file when it ends; `report.py` renders whatever exists and names
  what is missing; any phase reruns alone (`--only`).
- The scripts refuse the paths of the owner's real media and server config outright
  (`/mnt/mangonas`, `/mnt/nvme0/k3s`); every server boots a fresh copy of the test data's
  `config`, and `media` is bind-mounted read-only, always.

## Instrument validations (done once, against known answers)

| instrument | known answer | measured |
|---|---|---|
| memory sampler (`mem_sample.py`) | a container that allocates and touches exactly 512 MiB | 515.2 MiB anon (interpreter overhead ≈ 3 MiB), identical across 80 samples |
| cold-start timer (`coldstart.py`) | a container that sleeps 3.000 s then starts `python -m http.server` | 3,158–3,188 ms over 5 restarts (≈ 160 ms is the interpreter's own startup); polling begins 143–172 ms after process start |
| load client (`screens.js`) | a stub server answering every request after exactly 20 ms, at 5 screens/s | every endpoint p50 = 20 ms, p99 = 20–21 ms, max 21 ms — the client adds ≤ 1 ms |

## Running it

Needs `docker`, `k6`, `jq`, `taskset` and `python3` on PATH — `run.sh` checks for all five
and refuses to start without them — plus `curl`, which it uses but does not check for.
Building the test data additionally needs `ffmpeg` (with libx264 and libx265), `ffprobe`
and Python `Pillow`.

```bash
docker build -t ferrofin:bench .                  # the commit under test
docker pull jellyfin/jellyfin:10.11.8
docker pull jellyfin/jellyfin:12.0-rc7
bench/testdata/build.sh                           # once, ~20 min
bench/run.sh                                      # one full run, ~40 min → bench/runs/<version>/report.md
python3 bench/report.py bench/runs/A bench/runs/B bench/runs/C   # the full tables: medians, markers, notes
python3 bench/report.py --readme README.md bench/runs/A bench/runs/B bench/runs/C
                                                  # rewrites the README "Benchmarks" block (headline table + prose)
python3 bench/report.py --serve                   # the comparison viewer at http://127.0.0.1:8097/
```

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
numbered note. Both also print each cell's speed against Jellyfin 12.0-rc7 as
`X.Y× faster` (memory says lighter), shown only where both numbers stand; with a
baseline the viewer adds each server's percentage change against an earlier run of
itself (green = faster/smaller). Localhost only,
stdlib only, no JavaScript.

Tunables are the variables at the top of `run.sh` (`WINDOW_S`, `RATE_LOADED`, …); pass
`--only loaded` for a before/after on two builds. A shared host must be quiet: stop
anything that would compete for the chosen cores before a release run.
