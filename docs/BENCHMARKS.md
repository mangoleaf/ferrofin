# Benchmarks — full results

The root README carries the headline table. This is the rest of the same measurement:
the per-endpoint breakdown, how steady the numbers were across runs, and every place the
servers did not do identical work. The harness and its methodology are in
[`bench/README.md`](../bench/README.md).

## Setup

Measured on 2026-09-04 at commit `c599f65`, against **Jellyfin 12.0-rc7** and **Jellyfin
10.11.8** on the same machine, the same library and the same cgroup limits: an AMD Ryzen 9
9950X3D, each server alone in a container pinned to 8 dedicated cores with an 8 GiB limit
and swap disabled, over 3,001 movies / 250 series / 7,490 episodes. Every latency, memory
and startup figure is the **median of three full runs**; the response-shape findings below
come from one run, and the other two agree with it exactly. No request failed on any server
at any load level (0.00 % errors throughout).

Screen and endpoint figures are the "loaded" level — five simulated clients per second,
each walking a real screen's request sequence, which is the closest of the three levels to
ordinary use. Latency cells read **p50 / p95 / p99** in milliseconds. The `×` comparison is
**p50 against Jellyfin 12.0-rc7** on the same row.

## How steady are these numbers?

Taking a min–max spread of more than 15 % *of the median* across the three runs as the
test: **3 of the 102 p50s** moved more than that, against **68 of the 204 p95/p99 values**,
a third of the tails. Four of the twelve single-value rows moved too, three of them memory
figures (the widest, 10.11.8 steady idle, ranged 353–445 MiB). So read the p50s as the
numbers, the tails as an indication of shape, and the memory figures as approximate: tail
latency and allocator high-water marks both wander on a machine that is also running an
operating system. Each cell's run-to-run range is recorded alongside the number in the run
records, which are local rather than in this repo; `python3 bench/report.py --serve`
renders them.

## Per screen

| screen | Jellyfin 12.0-rc7 | Jellyfin 10.11.8 | Ferrofin |
|---|---|---|---|
| **home** | 44 / 51 / 57 | 64 / 73 / 77 — 1.5× slower ⚠[1] | 13 / 16 / 19 — 3.4× faster |
| **movies** | 38 / 43 / 47 | 24 / 28 / 31 — 1.6× faster | 25 / 30 / 32 — 1.5× faster |
| **detail** | 98 / 161 / 165 | 20 / 29 / 33 — 4.9× faster ⚠[2] | 9 / 12 / 13 — 10.9× faster ⚠[2] |
| **series** | 97 / 101 / 224 | 18 / 21 / 27 — 5.4× faster ⚠[3] | 8 / 10 / 11 — 12.1× faster |
| **search** | 70 / 234 / 255 | 98 / 121 / 184 — 1.4× slower ⚠[4] | 26 / 36 / 42 — 2.7× faster |
| **playback** | 31 / 37 / 55 | 5 / 20 / 33 — 6.2× faster ⚠[5] | 10 / 13 / 45 — 3.1× faster ⚠[5] |
| cold start (restart → home screen) | 2746 ms | 2162 ms — 1.3× faster | 174 ms — 15.8× faster |
| direct-play TTFB (1 MiB range) | 4.61 ms | 1.01 ms — 4.6× faster | 0.48 ms — 9.6× faster |
| peak memory under load | 588 MiB | 548 MiB — 1.1× lighter | 155 MiB — 3.8× lighter |
| steady idle memory | 409 MiB | 356 MiB — 1.1× lighter | 61.4 MiB — 6.7× lighter |

## Per endpoint

Same run, same level, broken out by request.

| endpoint | Jellyfin 12.0-rc7 | Jellyfin 10.11.8 | Ferrofin |
|---|---|---|---|
| `home:latest-movies` | 11.3 / 14.7 / 15.6 | 16.2 / 19.8 / 23.6 — 1.4× slower | 7.75 / 9.85 / 11.1 — 1.5× faster |
| `home:latest-music` | 8.42 / 11.6 / 13.8 | 29.8 / 35.6 / 42.5 — 3.5× slower | 2.96 / 6.42 / 7.92 — 2.8× faster |
| `home:latest-shows` | 19.8 / 24.6 / 26.6 | 28.5 / 33.4 / 39.6 — 1.4× slower ⚠[1] | 9.67 / 12.5 / 14.5 — 2.0× faster |
| `home:nextup` | 27.0 / 30.9 / 35.2 | 62.2 / 69.4 / 74.2 — 2.3× slower ⚠[6] | 10.4 / 12.4 / 14.4 — 2.6× faster |
| `home:resume-audio` | 25.4 / 29.3 / 31.2 | 9.99 / 12.9 / 15.4 — 2.5× faster | 6.70 / 8.10 / 9.08 — 3.8× faster |
| `home:resume-book` | 19.5 / 22.8 / 24.0 | 3.55 / 4.89 / 7.10 — 5.5× faster | 0.94 / 1.46 / 2.57 — 20.7× faster |
| `home:resume-video` | 27.3 / 31.2 / 34.2 | 12.5 / 15.8 / 18.3 — 2.2× faster ⚠[7] | 7.22 / 9.05 / 11.0 — 3.8× faster |
| `home:views` | 17.5 / 21.1 / 24.1 | 2.89 / 4.00 / 6.58 — 6.1× faster ⚠[8] | 1.10 / 1.84 / 3.06 — 15.9× faster |
| `movies:items` | 26.0 / 30.1 / 31.6 | 21.7 / 25.8 / 27.5 — 1.2× faster | 23.1 / 28.0 / 30.4 — 1.1× faster |
| `detail:item` | 12.3 / 15.4 / 16.6 | 6.95 / 8.68 / 10.3 — 1.8× faster ⚠[2] | 1.44 / 1.74 / 2.05 — 8.5× faster ⚠[2] |
| `detail:local-trailers` | 7.92 / 10.3 / 11.1 | 0.62 / 2.42 / 2.79 — 12.8× faster | 0.70 / 0.85 / 0.91 — 11.3× faster |
| `detail:similar` | 21.4 / 24.3 / 25.6 | 13.5 / 16.5 / 18.0 — 1.6× faster ⚠[9] | 4.95 / 5.68 / 6.10 — 4.3× faster |
| `detail:special-features` | 7.84 / 10.4 / 11.1 | 0.63 / 2.42 / 3.46 — 12.4× faster | 0.71 / 0.84 / 0.92 — 11.0× faster |
| `series:episodes` | 7.18 / 9.14 / 10.7 | 1.48 / 2.22 / 2.80 — 4.9× faster ⚠[3] | 0.94 / 1.11 / 1.16 — 7.6× faster |
| `series:item` | 11.2 / 13.4 / 15.1 | 6.86 / 8.37 / 10.1 — 1.6× faster ⚠[10] | 1.38 / 1.72 / 2.04 — 8.1× faster |
| `series:seasons` | 8.31 / 9.80 / 71.1 | 4.28 / 7.36 / 7.90 — 1.9× faster ⚠[11] | 1.35 / 1.62 / 2.07 — 6.2× faster |
| `series:similar` | 20.6 / 22.5 / 28.2 | 12.1 / 14.4 / 15.4 — 1.7× faster ⚠[12] | 3.71 / 4.12 / 4.54 — 5.6× faster |
| `search:artists` | 14.4 / 17.3 / 19.2 | 47.3 / 49.6 / 50.2 — 3.3× slower | 1.81 / 2.70 / 4.24 — 8.0× faster |
| `search:items` | 54.8 / 62.0 / 75.7 | 93.2 / 109 / 162 — 1.7× slower ⚠[4] | 21.5 / 24.6 / 29.0 — 2.5× faster |
| `search:persons` | 16.8 / 19.3 / 22.0 | 4.88 / 5.30 / 6.21 — 3.4× faster | 1.65 / 1.97 / 2.61 — 10.2× faster |
| `search:programs` | 21.5 / 23.5 / 26.2 | 3.79 / 4.12 / 5.02 — 5.7× faster | 1.06 / 1.91 / 3.46 — 20.3× faster |
| `search:videos` | 41.2 / 45.1 / 48.0 | 9.51 / 10.1 / 12.0 — 4.3× faster | 5.88 / 6.71 / 7.16 — 7.0× faster |
| `playback:intros` | 5.26 / 6.64 / 7.21 | 0.29 / 0.37 / 0.44 — 18.1× faster | 2.94 / 3.91 / 4.65 — 1.8× faster |
| `playback:playbackinfo` | 8.48 / 9.75 / 11.0 | 1.81 / 2.70 / 4.70 — 4.7× faster ⚠[5] | 1.20 / 1.35 / 1.38 — 7.1× faster ⚠[5] |
| `playback:playing` | 9.04 / 12.4 / 28.3 | 1.84 / 2.57 / 16.2 — 4.9× faster | 4.25 / 5.37 / 38.8 — 2.1× faster |
| `playback:segments` | 5.27 / 6.82 / 7.19 | 0.32 / 0.40 / 0.43 — 16.5× faster | 3.52 / 4.74 / 5.43 — 1.5× faster |
| `playback:stopped` | 6.97 / 9.22 / 17.2 | 0.77 / 1.10 / 28.9 — 9.1× faster | 0.89 / 1.18 / 1.40 — 7.8× faster |
| `image` | 5.19 / 69.4 / 73.4 | 0.49 / 5.45 / 6.76 — 10.6× faster | 0.33 / 3.36 / 4.33 — 15.7× faster |

## Where the servers did different work

⚠ marks a row where the servers did not do identical work, so the multiplier is an
indication rather than a like-for-like result:

- **[2] `detail:item`, [5] `playback:playbackinfo`** — `MediaStreams[].IsOriginal` and
  `LocalizedOriginal` are absent. These are 12.0 additions: Jellyfin **10.11.8 is missing
  them on the same rows**, and Ferrofin targets 10.11.8.
- **[1], [3], [4], [6]–[12]** — 10.11.8 only, for the same reason (fields and counts that
  changed in 12.0).

Three results are called out here rather than tabulated. **HLS first segment** is excluded
because all three servers pick different transcode parameters, so no two of them are
comparable. **`count latest`** returns 9 items where 12.0-rc7 returns 3, again matching
10.11.8, not a Ferrofin behaviour. **`count resume`** returns 60 where 12.0-rc7 returns 195
— 10.11.8 only, and the fourteenth of its divergences counted below, which is otherwise
invisible since only twelve carry a footnote.

Taken together, over the 28 compared requests plus the item-count probes: Ferrofin
differs from Jellyfin 12.0-rc7 in **three** ways, Jellyfin 10.11.8 differs in **fourteen**,
and all three of Ferrofin's are also on 10.11.8's list. They are places where 12.0 changed
and the release Ferrofin targets did not follow.

One divergence sits outside that comparison: on the excluded HLS row, Ferrofin's transcode
URL is **missing `subtitlemethod=encode`**, which 12.0-rc7 sends. 10.11.8 does not share
that one; its own transcode divergence runs the other way, an extra
`breakonnonkeyframes=true`. So within what this benchmark compares, Ferrofin has one
difference from current Jellyfin that its target release does not also have.

## What this does and does not show

The comparison covers 28 requests and only the fields those requests exercise, so it is
evidence for those, not for the 412 operations in the contract. Per-operation verification
means comparing an implementation against the upstream Jellyfin C#, and that record,
`handlers::VERIFIED`, is early. Known, deliberate deviations are listed in
[`FEATURES.md`](FEATURES.md).

Every benchmark run also exercises database adoption: each server boots a disposable copy
of a library scanned by a real Jellyfin 10.11.8, and Ferrofin migrates it in place.
