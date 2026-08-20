# suite/micro — the fast loop

The full suite (`suite/perf/perf-gate.sh`) rebuilds a Docker image and rescans the
library on every invocation: ~15 minutes to measure one thing. That is the right
tool for a release record and the wrong tool for "did this change help?".

This is the fast loop. One long-lived server over an already-scanned database,
driven one endpoint at a time. **~10 seconds per measurement**, and the same
server can be run under a profiler.

```
./serve.sh start                 # boot the profiling binary over /tmp/ff-fast (once)
./hit.sh persons 608             # drive ONE endpoint at ONE rate -> p50/p95/p99/ok%
./hit.sh nextup 2000
./serve.sh stop
```

## Why it is allowed to be less rigorous

It is **not** a substitute for the suite:

- one server, no Jellyfin leg, so it answers "did Ferrofin get faster?", never
  "is Ferrofin faster than Jellyfin";
- no two-stage warmup, no rate calibration, no sample-count-based windows;
- absolute numbers are host-sensitive — this box runs clickhouse/llama/k3s in the
  background, so treat a single number as noise and a *delta measured back to back*
  as signal.

Its job is to make the measure→fix→re-measure cycle cheap enough to actually do.
Land a change here, then confirm with the real gate before claiming anything.

## Data

`/tmp/ff-fast` is a copy of the `ferrofin-benchv2_ferrofin-data` volume
(9,862 items / 7,505 people). Recreate with:

```sh
mkdir -p /tmp/ff-fast
docker run --rm -v ferrofin-benchv2_ferrofin-data:/d -v /tmp/ff-fast:/out \
  alpine sh -c 'cp -a /d/. /out/'
```

Media paths inside that DB point at container paths (`/media/...`), which is
irrelevant for query/DTO endpoints — they never touch the file. Endpoints that do
read files (images, streaming) will misreport; do not use this harness for those.

## Profiling the collapse

```sh
./serve.sh profile               # runs the server under samply
./hit.sh playlist_items 2000     # drive it while the profiler samples
                                 # ctrl-c serve.sh -> opens the Firefox Profiler UI
```

## Write rows

`hit.sh` drives **any** method, with or without a JSON body. A non-GET row (or any row
with a `body`) is sent through a vegeta *targets file* — vegeta reads a body only from an
`@file` line in that form — with `Content-Type: application/json` attached; GET rows keep
the original one-line target. The expected status comes from the row's `ok` field, and a
window that returns something else prints `<-- expected 204, also saw {...}` so a write
that silently 200s or 400s can't masquerade as a measurement.

```sh
./hit.sh list                      # every drivable row, with its method
./hit.sh playstate_progress 600    # POST + body, 204 expected
```

Rows come from `suite/perf/endpoints.py` (the gate's table) **plus** `./write_endpoints.py`,
a fast-loop-only table of extra write rows — playstate start/stop/ping, favourite/played/
rating, playlist add/move, display preferences. They live here rather than in
`endpoints.py` because that file is what `perf-gate.sh` measures and what
`suite/perf-baseline.json` must carry. Every row there is state-preserving under
repetition, so a 10s window leaves the database where it found it. A name collision with
the gate's table is a hard error.

Write rows need the ids `benchlib.enrich_context` resolves (`writeItemId`, `playlistId`,
…); `hit.sh` calls it on demand the first time a row asks for one and extends the cached
context.

## Running two servers at once

`FF_MICRO_PORT` now selects the pid/log files too (`/tmp/ff-micro-<port>.pid`), so a
second agent measuring a second binary cannot stop yours. `FF_MICRO_CTX` picks the fixture
cache — use a private one per server, since the playlist fixture only exists in the
database that server was pointed at:

```sh
FF_MICRO_PORT=18331 FF_MICRO_DATA=/tmp/my-db ./serve.sh start
FF_MICRO_PORT=18331 FF_MICRO_CTX=/tmp/my-ctx.json ./hit.sh playstate_progress 600
```

## What this harness cannot measure (and will report as a failure)

These are gaps in the harness, not server bugs. Check here before chasing a 0% row.

- **Endpoints needing media files** — `image_primary`, `item_image_indexed` and the other
  image/streaming routes return 404 because `/tmp/ffdb2` holds only the database. The
  media, metadata and image directories are deliberately not copied (they are ~1.3 GB and
  irrelevant to query/DTO work). Use the real suite for those.
- **Endpoints needing fixtures the server cannot supply** — a row whose path or body names
  a `{…}` field neither `pick_items` nor `enrich_context` resolves fails loudly rather
  than measuring the wrong thing. (`hit.sh` calls `enrich_context` on demand, so the rows
  templating on `{seriesId}`, `{playlistId}`, `{genreName}`, `{studioName}` and
  `{personName}` do resolve here — a `KeyError` means the *library* lacks that shape.)

A row reading `0.0%` ok for one of the above means "not exercised", not "broken". Those
three image rows are the ONLY ones: every other row in `endpoints.py` was verified to
answer its expected status against this harness (2026-08).
