# Benchmarks — three-run results

Measured on **2026-09-17** from server source **`9c17abad`**, against **Jellyfin 12.0.0**
and **Jellyfin 10.11.8**, using three complete repetitions. The root [README](../README.md#benchmarks)
withholds flagged cells; this detailed report retains flagged raw measurements as
**diagnostics, not performance claims**, and omits their speed ratios. The
[methodology](../bench/README.md) defines the measurements and checks.

## Setup and acceptance

An AMD Ryzen 9 9950X3D ran each server alone on logical CPUs 8–15, with clients on
16–19, an 8 GiB container limit and no swap. The fixture contains 3,001 movies,
250 series and 7,490 episodes. Every run used the same images and fixture hashes,
600 serial validation slots, 30-second warm-ups and 120-second load windows at
1, 5 and 25 screens/second. Browsing and state-changing playback use separate accounts;
background scans/schedules and plugins are controlled as described in the methodology.

| Check | Run 1 | Run 2 | Run 3 |
|---|---:|---:|---:|
| Preflight idle (90% required) | 92.0% | 95.6% | 95.6% |
| Elapsed server phases | 40.67 min | 40.64 min | 40.67 min |
| Phase, CPU coverage and isolation checks | Pass | Pass | Pass |
| Dropped load iterations | 0 | 0 | 0 |
| Measured request failures | 0 | 0 | 0 |

Passing those checks establishes a complete measurement, not response equivalence.
Only the movie screen passes Ferrofin's whole-screen comparison against Jellyfin 12.
Home, detail, series, search and playback retain response/selection differences;
HLS retains different transcode parameters. Inventory totals remain diagnostic-only.
Twenty-two comparable loaded endpoint measurements are faster on Ferrofin; flagged
endpoints are excluded from that count.

Latency, memory and startup cells report the median across runs, with observed ranges
where the displayed values differ. Latency is **p50 / p95 / p99 in milliseconds** for
scripted HTTP transactions, not browser rendering. Ferrofin's per-run cold-start medians
span **240–1087 ms**; the three-run median is **244 ms**. The host passed preflight but
was not interference-free: the memory table records sampled interference, including
kernel work as well as other processes, without attributing its cause.

The older 2026-09-04 results used Jellyfin 12.0-rc7 and a different workload policy.
They are not pooled with these runs, and differences from them are not regression claims.

## Loaded latency (5 screens/s)

| screen | Jellyfin 12.0.0 p50 / p95 / p99 ms (err) | Jellyfin 10.11.8 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|---|
| home | 45 / 48 (48–49) / 52 (51–56) (0.00%) | 63 (62–63) / 71 (69–74) / 76 (75–82) (0.00%) ⚠[1] | 13 (12–14) / 17 (16–17) / 20 (19–21) (0.00%) ⚠[2] |
| movies | 37 / 41 (40–42) / 44 (42–44) (0.00%) | 23 / 27 (26–27) / 29 (29–31) (0.00%) — 1.6× faster | 13 (12–13) / 16 (15–16) / 17 (16–17) (0.00%) — 2.8× faster |
| detail | 39 / 42 / 44 (43–45) (0.00%) | 17 (16–17) / 23 (23–31) / 27 (24–33) (0.00%) ⚠[3] | 6 / 8 / 10 (9–10) (0.00%) ⚠[4] |
| series | 30 / 32 (32–33) / 34 (34–35) (0.00%) | 14 (14–15) / 17 (16–19) / 20 (18–20) (0.00%) ⚠[5] | 6 / 7 (6–8) / 8 (7–11) (0.00%) ⚠[6] |
| search | 58 / 65 (63–66) / 82 (81–83) (0.00%) | 96 / 119 (116–119) / 179 (178–183) (0.00%) ⚠[7] | 23 (22–23) / 28 (27–31) / 36 (33–38) (0.00%) ⚠[7] |
| playback | 31 (30–31) / 34 (33–34) / 45 (41–47) (0.00%) | 5 / 7 (7–9) / 18 (15–18) (0.00%) ⚠[8] | 9 (9–10) / 11 (10–12) / 37 (31–40) (0.00%) ⚠[8] |

| endpoint | Jellyfin 12.0.0 p50 / p95 / p99 ms (err) | Jellyfin 10.11.8 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|---|
| home:image | 4.85 (4.80–4.87) / 6.60 (6.45–6.60) / 7.15 (6.91–7.39) (0.00%) | 0.44 (0.43–0.44) / 0.60 (0.58–0.60) / 2.90 (2.52–3.15) (0.00%) — 11.0× faster | 0.29 (0.28–0.31) / 0.42 (0.41–0.48) / 1.66 (1.58–2.23) (0.00%) — 16.7× faster |
| home:latest-movies | 11.6 (11.2–11.6) / 13.9 (13.9–14.0) / 15.0 (14.7–15.0) (0.00%) | 15.7 (15.5–15.8) / 18.6 (18.4–21.2) / 21.5 (20.7–24.4) (0.00%) — 1.4× slower | 7.53 (7.45–7.96) / 9.79 (9.47–10.3) / 10.6 (10.6–11.7) (0.00%) — 1.5× faster |
| home:latest-music | 8.02 (7.89–8.11) / 10.7 (10.6–10.8) / 12.5 (11.8–12.6) (0.00%) | 28.6 (28.3–28.8) / 33.4 (32.9–38.7) / 38.3 (37.7–48.6) (0.00%) ⚠[1] | 2.81 (2.63–3.42) / 5.38 (5.27–6.75) / 7.33 (6.36–7.37) (0.00%) — 2.9× faster |
| home:latest-shows | 21.1 (21.1–21.5) / 26.1 (24.6–26.5) / 28.4 (26.7–28.5) (0.00%) | 27.2 (27.2–27.4) / 32.4 (31.4–33.3) / 35.7 (32.9–40.6) (0.00%) ⚠[2] | 9.42 (9.36–10.2) / 12.9 (11.7–13.5) / 13.9 (13.4–15.2) (0.00%) ⚠[2] |
| home:nextup | 25.2 (24.9–25.4) / 27.6 (27.6–27.8) / 28.6 (28.1–30.4) (0.00%) | 60.1 (59.5–60.5) / 67.6 (66.6–70.2) / 72.5 (70.4–79.7) (0.00%) ⚠[9] | 9.60 (9.49–9.74) / 10.9 (10.8–11.7) / 13.2 (12.9–13.6) (0.00%) — 2.6× faster |
| home:resume-audio | 20.1 (19.6–20.2) / 22.7 (22.7–23.1) / 23.8 (23.8–24.1) (0.00%) | 9.35 (9.31–9.45) / 11.8 (11.3–13.6) / 15.6 (12.9–15.7) (0.00%) — 2.1× faster | 5.48 (5.47–5.54) / 6.27 (6.08–6.54) / 7.19 (7.18–8.14) (0.00%) — 3.7× faster |
| home:resume-book | 20.0 (19.8–20.3) / 22.9 (22.8–23.0) / 24.0 (23.4–24.3) (0.00%) | 3.39 (3.33–3.43) / 4.74 (4.70–5.57) / 7.31 (5.41–7.48) (0.00%) — 5.9× faster | 0.90 (0.90–0.93) / 1.32 (1.17–1.34) / 3.28 (2.52–3.63) (0.00%) — 22.2× faster |
| home:resume-video | 21.6 (21.4–21.8) / 24.5 (24.1–24.8) / 25.5 (25.1–26.5) (0.00%) | 12.0 (11.9–12.2) / 14.9 (14.2–16.9) / 18.9 (16.1–20.4) (0.00%) ⚠[10] | 6.81 (6.80–7.09) / 8.33 (7.90–9.05) / 9.18 (9.15–9.75) (0.00%) — 3.2× faster |
| home:views | 16.8 (16.5–16.8) / 19.4 (19.4–19.5) / 20.4 (20.1–20.8) (0.00%) | 2.66 (2.63–2.71) / 3.73 (3.73–4.36) / 6.42 (3.89–6.81) (0.00%) ⚠[11] | 1.52 (1.47–1.80) / 4.12 (4.02–4.20) / 4.69 (4.39–4.87) (0.00%) ⚠[11] |
| movies:image | 4.94 (4.94–5.03) / 6.79 (6.76–6.84) / 7.85 (7.08–8.14) (0.00%) | 0.44 (0.44–0.45) / 0.63 (0.60–0.76) / 1.90 (1.71–2.66) (0.00%) — 11.2× faster | 0.34 (0.32–0.34) / 0.49 (0.47–0.50) / 1.73 (1.16–2.41) (0.00%) — 14.5× faster |
| movies:items | 25.2 (25.0–25.3) / 28.5 (28.5–28.9) / 30.4 (30.0–30.4) (0.00%) | 20.7 (20.7–20.9) / 24.3 (24.0–24.4) / 26.4 (26.3–27.6) (0.00%) — 1.2× faster | 10.7 (10.4–10.8) / 11.9 (11.7–12.3) / 13.4 (13.1–13.9) (0.00%) — 2.4× faster |
| detail:image | 4.78 (4.76–4.80) / 6.55 (6.50–6.59) / 7.73 (7.70–7.84) (0.00%) | 0.47 (0.45–0.51) / 4.56 (4.50–4.64) / 6.12 (5.73–13.3) (0.00%) ⚠[3] | 0.27 (0.27–0.28) / 0.40 (0.38–0.42) / 2.05 (0.59–2.12) (0.00%) ⚠[4] |
| detail:item | 11.2 (11.1–11.3) / 13.2 (13.1–13.6) / 14.0 (13.5–14.2) (0.00%) | 6.67 (6.60–6.76) / 8.90 (8.18–9.20) / 10.9 (9.87–11.2) (0.00%) ⚠[12] | 1.49 (1.48–1.52) / 1.70 (1.67–1.79) / 2.17 (1.84–2.29) (0.00%) ⚠[13] |
| detail:local-trailers | 7.27 (7.27–7.46) / 9.31 (8.96–9.50) / 10.0 (9.72–10.2) (0.00%) | 0.58 (0.58–0.60) / 2.37 (2.31–2.37) / 2.58 (2.45–5.24) (0.00%) ⚠[14] | 0.67 (0.66–0.68) / 0.80 (0.79–0.82) / 1.05 (0.90–1.23) (0.00%) — 10.9× faster |
| detail:similar | 27.6 (27.5–27.7) / 29.6 (29.4–30.2) / 30.2 (30.1–32.8) (0.00%) | 11.9 (11.8–12.7) / 16.0 (15.8–16.1) / 18.1 (16.8–18.2) (0.00%) ⚠[15] | 4.75 (4.68–4.81) / 5.79 (5.55–5.79) / 6.33 (5.86–6.34) (0.00%) ⚠[16] |
| detail:special-features | 7.33 (7.32–7.40) / 9.09 (8.85–9.38) / 10.0 (9.45–10.1) (0.00%) | 0.59 (0.58–0.62) / 2.36 (2.32–2.38) / 2.59 (2.49–5.25) (0.00%) ⚠[14] | 0.68 (0.66–0.69) / 0.84 (0.82–0.84) / 0.94 (0.89–1.35) (0.00%) — 10.8× faster |
| series:episodes | 6.12 (6.05–6.16) / 7.44 (7.07–7.74) / 7.73 (7.48–8.54) (0.00%) | 1.37 (1.37–1.53) / 2.24 (2.10–2.28) / 3.10 (2.31–4.20) (0.00%) ⚠[5] | 0.93 (0.87–0.96) / 1.18 (1.02–1.19) / 1.25 (1.08–1.29) (0.00%) — 6.6× faster |
| series:image | 3.78 (3.73–3.85) / 5.27 (4.93–5.70) / 5.41 (5.32–6.08) (0.00%) | 0.34 (0.33–0.38) / 0.42 (0.40–0.45) / 0.65 (0.47–2.70) (0.00%) — 11.1× faster | 0.26 (0.25–0.26) / 0.34 (0.29–0.41) / 0.40 (0.39–2.57) (0.00%) — 14.5× faster |
| series:item | 10.4 (10.4–10.6) / 12.1 (11.9–12.6) / 12.6 (12.6–13.3) (0.00%) | 6.73 (6.64–6.95) / 9.46 (9.39–10.2) / 9.76 (9.76–11.3) (0.00%) ⚠[17] | 1.39 (1.39–1.43) / 1.59 (1.55–1.97) / 1.97 (1.58–2.25) (0.00%) — 7.5× faster |
| series:seasons | 7.81 (7.74–7.88) / 9.23 (9.22–9.65) / 9.72 (9.60–9.82) (0.00%) | 4.01 (3.74–4.14) / 7.48 (7.37–8.47) / 9.33 (8.81–10.7) (0.00%) ⚠[18] | 1.34 (1.34–1.35) / 1.51 (1.51–1.53) / 1.62 (1.54–1.98) (0.00%) — 5.8× faster |
| series:similar | 18.9 (18.8–19.1) / 20.8 (20.2–21.1) / 22.1 (20.7–22.5) (0.00%) | 12.0 (11.4–12.2) / 14.5 (13.7–15.6) / 16.4 (14.7–16.9) (0.00%) ⚠[6] | 3.81 (3.79–3.85) / 4.32 (4.19–4.53) / 4.51 (4.32–4.78) (0.00%) ⚠[6] |
| search:artists | 13.6 (13.5–13.8) / 15.7 (15.6–15.9) / 16.5 (16.3–16.8) (0.00%) | 46.7 (46.6–46.9) / 49.3 (47.9–50.3) / 52.0 (48.5–57.9) (0.00%) ⚠[7] | 1.92 (1.88–1.99) / 2.95 (2.66–3.73) / 3.91 (3.67–4.55) (0.00%) ⚠[7] |
| search:image | 5.04 (5.03–5.04) / 6.90 (6.87–6.95) / 7.46 (7.20–7.93) (0.00%) | 0.47 (0.47–0.48) / 2.11 (2.06–2.29) / 2.54 (2.45–2.70) (0.00%) — 10.7× faster | 0.33 (0.32–0.35) / 0.54 (0.50–0.96) / 1.62 (1.29–2.36) (0.00%) — 15.3× faster |
| search:items | 44.1 (44.0–44.9) / 49.6 (49.1–51.2) / 65.3 (65.0–65.4) (0.00%) | 91.5 (90.7–92.1) / 113 (109–115) / 173 (172–178) (0.00%) ⚠[19] | 18.6 (18.6–19.1) / 23.8 (21.0–25.5) / 29.2 (27.7–29.4) (0.00%) ⚠[20] |
| search:persons | 17.2 (17.1–17.2) / 19.2 (18.9–19.8) / 20.4 (19.6–20.9) (0.00%) | 4.78 (4.75–4.79) / 5.22 (5.17–6.26) / 7.07 (5.29–7.74) (0.00%) ⚠[21] | 1.81 (1.80–1.83) / 2.87 (2.11–3.35) / 3.60 (2.94–3.86) (0.00%) ⚠[21] |
| search:programs | 20.2 (20.1–20.5) / 22.2 (22.0–22.9) / 22.7 (22.4–23.4) (0.00%) | 3.71 (3.64–3.71) / 4.03 (3.91–5.20) / 5.86 (4.21–6.75) (0.00%) — 5.4× faster | 1.07 (1.03–1.07) / 2.86 (2.05–3.61) / 3.44 (2.81–3.98) (0.00%) — 18.9× faster |
| search:videos | 37.6 (37.3–37.6) / 39.7 (39.7–39.8) / 40.3 (40.2–43.4) (0.00%) | 9.20 (9.03–9.23) / 10.6 (9.52–12.1) / 12.3 (10.0–15.1) (0.00%) — 4.1× faster | 5.66 (5.65–5.74) / 6.73 (6.59–6.89) / 7.51 (7.36–8.25) (0.00%) — 6.6× faster |
| playback:intros | 5.17 (5.04–5.20) / 6.15 (5.39–6.76) / 7.10 (6.87–7.86) (0.00%) | 0.28 (0.28–0.30) / 0.38 (0.33–0.45) / 0.41 (0.36–0.74) (0.00%) — 18.5× faster | 2.53 (2.45–2.55) / 2.92 (2.78–3.92) / 3.83 (3.63–4.46) (0.00%) — 2.0× faster |
| playback:playbackinfo | 8.40 (8.15–8.45) / 9.49 (9.47–9.76) / 10.7 (10.0–12.1) (0.00%) | 1.83 (1.78–2.13) / 2.74 (2.53–3.31) / 3.20 (2.89–4.11) (0.00%) ⚠[8] | 1.15 (1.14–1.16) / 1.28 (1.27–1.29) / 3.30 (3.12–3.45) (0.00%) ⚠[8] |
| playback:playing | 9.02 (8.91–9.04) / 10.9 (10.4–11.0) / 20.5 (11.2–20.6) (0.00%) | 1.75 (1.75–1.76) / 2.24 (2.12–3.07) / 9.86 (8.75–14.4) (0.00%) — 5.2× faster | 3.84 (3.70–3.84) / 4.37 (4.16–4.69) / 31.2 (25.1–33.8) (0.00%) — 2.3× faster |
| playback:segments | 5.24 (5.06–5.24) / 6.56 (5.45–6.77) / 7.47 (7.10–8.02) (0.00%) | 0.31 (0.31–0.32) / 0.38 (0.36–0.50) / 0.40 (0.38–0.71) (0.00%) — 16.9× faster | 3.06 (3.04–3.09) / 3.48 (3.36–4.38) / 4.35 (4.13–5.05) (0.00%) — 1.7× faster |
| playback:stopped | 6.92 (6.74–7.01) / 8.59 (7.14–8.96) / 11.6 (11.5–16.3) (0.00%) | 0.77 (0.76–0.77) / 1.00 (0.93–1.06) / 3.17 (0.97–3.36) (0.00%) — 9.0× faster | 0.74 (0.73–0.78) / 0.88 (0.88–1.00) / 1.16 (1.03–1.28) (0.00%) — 9.4× faster |
| image | 4.86 (4.81–4.86) / 6.67 (6.62–6.70) / 7.62 (7.21–7.76) (0.00%) | 0.44 (0.44–0.45) / 2.09 (2.01–2.24) / 4.80 (4.65–4.88) (0.00%) ⚠[22] | 0.28 (0.28–0.30) / 0.46 (0.45–0.49) / 1.73 (1.46–2.11) (0.00%) ⚠[23] |



- **[2] `detail:item`, [5] `playback:playbackinfo`** — `MediaStreams[].IsOriginal` and
  `LocalizedOriginal` are absent. These are 12.0 additions: Jellyfin **10.11.8 is missing
  them on the same rows**, and Ferrofin targeted 10.11.8 when these numbers were taken.
  Ferrofin now serves both fields (and `LocalizedLanguage`) as Jellyfin 12 does; the
  numbers above are historical.
- **[1], [3], [4], [6]–[12]** — 10.11.8 only, for the same reason (fields and counts that
  changed in 12.0).

## CPU observations

Each cell is the median of the three per-run observations, followed by its minimum–maximum range. These are diagnostic measurements: whole-workload CPU savings percentages are withheld because response and request-selection validation differs. Failed screens would remain in the completed-screen denominator; none failed in these runs.

CPU-seconds measure server-container CPU work, including its children. Average cores busy is CPU-seconds divided by the bracketing wall-clock interval; 1.0 is one fully occupied logical CPU. Capacity busy divides that by the eight allocated CPUs. CPU ms/screen divides whole-window CPU by completed mixed-screen iterations; it does not attribute CPU to individual endpoints.

| Window / metric | Jellyfin 12.0.0 | Jellyfin 10.11.8 | Ferrofin |
|---|---|---|---|
| unloaded · CPU-seconds | 16.103 (16.060–16.113) | 11.253 (11.125–11.342) | 2.867 (2.805–2.955) |
| unloaded · Average cores busy | 0.134 (0.133–0.134) | 0.093 (0.092–0.094) | 0.024 (0.023–0.025) |
| unloaded · Allocated capacity busy (%) | 1.67 (1.67–1.67) | 1.17 (1.15–1.18) | 0.30 (0.29–0.31) |
| unloaded · CPU ms/screen | 133.162 (133.079–133.837) | 93.000 (91.941–94.517) | 23.895 (23.182–24.420) |
| loaded · CPU-seconds | 71.157 (71.114–71.214) | 48.185 (47.910–48.576) | 13.982 (13.921–14.414) |
| loaded · Average cores busy | 0.591 (0.591–0.591) | 0.401 (0.398–0.404) | 0.116 (0.116–0.120) |
| loaded · Allocated capacity busy (%) | 7.39 (7.38–7.39) | 5.01 (4.97–5.05) | 1.45 (1.45–1.50) |
| loaded · CPU ms/screen | 118.492 (118.326–118.594) | 80.308 (79.850–80.960) | 23.265 (23.201–23.984) |
| stress · CPU-seconds | 344.717 (344.228–352.939) | 225.642 (225.322–229.723) | 66.938 (65.898–70.252) |
| stress · Average cores busy | 2.863 (2.859–2.931) | 1.874 (1.871–1.908) | 0.556 (0.547–0.583) |
| stress · Allocated capacity busy (%) | 35.79 (35.74–36.64) | 23.43 (23.39–23.85) | 6.95 (6.84–7.29) |
| stress · CPU ms/screen | 114.906 (114.705–117.607) | 75.189 (75.107–76.549) | 22.305 (21.959–23.409) |
| steady · CPU-seconds | 0.045 (0.044–0.046) | 0.047 (0.046–0.049) | 0.001 (0.001–0.001) |
| steady · Average cores busy | 0.001 (0.001–0.001) | 0.001 (0.001–0.001) | 0.000 (0.000–0.000) |
| steady · Allocated capacity busy (%) | 0.01 (0.01–0.01) | 0.01 (0.01–0.01) | 0.00 (0.00–0.00) |

## Startup and streaming

| | Jellyfin 12.0.0 | Jellyfin 10.11.8 | Ferrofin |
|---|---|---|---|
| cold start (restart → authenticated views; host caches retained) | 2143 ms (2128 ms–2164 ms) | 2079 ms (2074 ms–2093 ms) — about the same | 244 ms (240 ms–1087 ms) — 8.8× faster |
| HLS first segment (forced transcode) | 333 ms (332 ms–333 ms) | 317 ms (316 ms–318 ms) ⚠[24] | 181 ms (176 ms–186 ms) ⚠[24] |
| direct-play TTFB (1 MiB range) | 4.31 ms (3.98 ms–4.67 ms) | 1.07 ms (1.05 ms–1.12 ms) — 4.0× faster | 0.52 ms (0.48 ms–0.65 ms) — 8.3× faster |

## Memory (anon, cache excluded, 100 ms samples)

| | Jellyfin 12.0.0 | Jellyfin 10.11.8 | Ferrofin |
|---|---|---|---|
| peak under load | 571 MiB (560 MiB–585 MiB) | 672 MiB (656 MiB–686 MiB) — 1.2× heavier | 202 MiB (178 MiB–209 MiB) — 2.8× lighter |
| steady idle | 423 MiB (392 MiB–439 MiB) | 481 MiB (475 MiB–484 MiB) — 1.1× heavier | 152 MiB (123 MiB–157 MiB) — 2.8× lighter |
| interference on the server's cores, p95 | 14% (13%–14%) | 13% (12%–14%) | 14% (13%–15%) |
| interference, max single sample | 32% (30%–34%) | 35% (33%–42%) | 40% (38%–84%) |
| max swap | 0.00 MiB | 0.00 MiB | 0.00 MiB |

## Response shape vs Jellyfin 12.0.0 (supporting evidence, not the parity number)

**jellyfin**: 20 divergence(s) across 33 compared requests

- count latest: 9 vs 3
- count persons: 5000 vs 5158
- count resume: 60 vs 195
- detail:image: request picks differ
- detail:item: per-item fields/types differ for a request pick
- detail:similar: items [2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16] vs [12]
- home:latest-music: ids differs for a request pick
- home:latest-shows: ids differs for a request pick
- home:nextup: ids differs for a request pick
- home:resume-video: per-item fields/types differ for a request pick
- home:views: per-item fields/types differ for a request pick
- image: request picks differ
- playback:playbackinfo: per-item fields/types differ for a request pick
- search:artists: ids differs for a request pick
- search:items: per-item fields/types differ for a request pick
- search:persons: items [0] vs [0, 1, 2, 3, 4, 5, 7]
- series:episodes: per-item fields/types differ for a request pick
- series:item: per-item fields/types differ for a request pick
- series:seasons: per-item fields/types differ for a request pick
- series:similar: ids differs for a request pick

**ferrofin**: 13 divergence(s) across 33 compared requests

- count latest: 9 vs 3
- count persons: 5000 vs 5158
- detail:image: request picks differ
- detail:item: per-item fields/types differ for a request pick
- detail:similar: ids differs for a request pick
- home:latest-shows: ids differs for a request pick
- home:views: per-item fields/types differ for a request pick
- image: request picks differ
- playback:playbackinfo: per-item fields/types differ for a request pick
- search:artists: ids differs for a request pick
- search:items: ids differs for a request pick
- search:persons: items [0] vs [0, 1, 2, 3, 4, 5, 7]
- series:similar: ids differs for a request pick


## Notes

1. home:latest-music: ids differs for a request pick
2. home:latest-shows: ids differs for a request pick
3. detail:image: request picks differ; measured request selections differ from validated slot
4. detail:image: request picks differ
5. series:episodes: per-item fields/types differ for a request pick
6. series:similar: ids differs for a request pick
7. search:artists: ids differs for a request pick
8. playback:playbackinfo: per-item fields/types differ for a request pick
9. home:nextup: ids differs for a request pick
10. home:resume-video: per-item fields/types differ for a request pick
11. home:views: per-item fields/types differ for a request pick
12. detail:item: per-item fields/types differ for a request pick; measured request selections differ from validated slot
13. detail:item: per-item fields/types differ for a request pick
14. measured request selections differ from validated slot
15. detail:similar: items [2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16] vs [12]; measured request selections differ from validated slot
16. detail:similar: ids differs for a request pick
17. series:item: per-item fields/types differ for a request pick
18. series:seasons: per-item fields/types differ for a request pick
19. search:items: per-item fields/types differ for a request pick
20. search:items: ids differs for a request pick
21. search:persons: items [0] vs [0, 1, 2, 3, 4, 5, 7]
22. image: request picks differ; measured request selections differ from validated slot
23. image: request picks differ
24. HLS transcode parameters differ from Jellyfin 12.0.0 in all five probes of each run; these timings are diagnostic and do not support a speed comparison.

## Reproduce the reports

Raw evidence stays under ignored `bench/runs/` directories. CPU samples are CSV;
CPU presentation is tabular. Role credentials must not be published.

```sh
python3 bench/report.py \
  bench/runs/quiet-cpu-comparison-current \
  bench/runs/quiet-cpu-comparison-run2 \
  bench/runs/quiet-cpu-comparison-run3
```

The combined local Markdown/HTML report and README preview are in
`bench/runs/quiet-cpu-comparison-three-runs/`; the full report includes unloaded and
stress latency tables plus every individual CPU observation and sampling boundary.
The CPU table above computes medians/ranges from those same per-run counters and
completed-screen counts. No raw CPU JSON is embedded in this document.
