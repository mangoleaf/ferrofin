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
