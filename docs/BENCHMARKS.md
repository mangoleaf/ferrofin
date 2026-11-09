# Benchmarks — three-run results

Measured on **2026-11-08** from commit **`961c0b412`** (the `v1.5.0` tag, `release/v1.5.0`), against **Jellyfin 12.4.0** (pinned by digest in
`gates/bench/testdata/jellyfin-image.txt`), using three complete repetitions: the benchmark of one full gates
session and two benchmark sessions, all of one image. The root [README](../README.md#results) summarises them,
and the release's page, [`docs/results/v1.5.0/README.md`](results/v1.5.0/README.md), graphs every comparable cell
and lists the withheld ones; this detailed report keeps flagged raw measurements as
**diagnostics, not performance claims**. The [methodology](../gates/bench/README.md) defines the measurements
and checks.

Every run passed the start-of-run idle check (99.3 %, 99.7 % and 99.7 %). During measurement the server cores
saw 6–9 % p95 interference; the ranges below include it.

Everything between the two rules is the unedited output of `gates/bench/report.py` for the three runs.

---

## Ferrofin vs Jellyfin — 3 run(s), commit 961c0b412, 2026-11-08
Host AMD Ryzen 9 9950X3D 16-Core Processor · server on cpus 8-15 · 8g limit · test data 3001 movies, 250 series, 7490 episodes, 50 hdr · windows 120 s · unloaded 1 screens/s · loaded 5 screens/s · stress 25 screens/s
Cells are the median across runs; where the runs disagreed, the range they spanned follows in brackets. `⚠[n]` means the server did different work than Jellyfin 12.4.0 (status / record count / missing fields), so the number is not comparable: it is kept for the work list, not for publication, and note `n` says why. `X.Y× faster` compares the cell with Jellyfin 12.4.0 on the same row. It is shown on flagged cells too — the note says the two servers did not do identical work, so read it as an indication rather than a like-for-like result.

### Latency — unloaded (1 screens/s)

| screen | Jellyfin 12.4.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home | 45 (45–46) / 50 (49–51) / 53 (53–54) (0.00%) | 13 / 20 (16–20) / 22 (18–23) (0.00%) — 3.5× faster |
| movies | 38 (37–38) / 43 (42–48) / 50 (49–53) (0.00%) | 14 (13–14) / 15 (15–24) / 15 (15–26) (0.00%) — 2.7× faster |
| detail | 34 (33–34) / 37 (36–43) / 37 (36–46) (0.00%) | 6 (6–7) / 7 (7–17) / 10 (9–18) (0.00%) — 5.7× faster |
| series | 24 / 161 (27–167) / 294 (27–295) (0.00%) | 5 (5–6) / 6 (6–8) / 6 (6–9) (0.00%) — 4.8× faster |
| search | 68 (68–76) / 91 (86–94) / 97 (88–97) (0.00%) | 22 (21–22) / 24 (22–25) / 25 (23–27) (0.00%) — 3.1× faster ⚠[1] |
| playback | 31 (31–32) / 34 (34–39) / 35 (35–41) (0.00%) | 7 / 8 (7–8) / 8 (0.00%) — 4.4× faster |

| endpoint | Jellyfin 12.4.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home:image | 4.75 (4.74–4.76) / 6.35 (6.26–6.84) / 7.09 (6.84–7.71) (0.00%) | 0.41 (0.40–0.41) / 0.59 (0.54–0.62) / 0.80 (0.59–1.02) (0.00%) — 11.6× faster |
| home:latest-movies | 11.5 (11.3–11.5) / 13.9 (13.8–14.6) / 17.8 (17.7–18.2) (0.00%) | 4.67 (4.62–4.70) / 5.50 (5.48–6.03) / 6.30 (5.72–11.2) (0.00%) — 2.5× faster |
| home:latest-music | 7.00 (6.97–7.01) / 9.24 (9.18–9.58) / 10.2 (9.65–10.4) (0.00%) | 2.77 (2.76–2.80) / 3.85 (3.84–4.24) / 4.34 (4.20–4.68) (0.00%) — 2.5× faster |
| home:latest-shows | 21.6 (21.4–21.6) / 24.6 (24.1–26.0) / 27.0 (24.4–29.2) (0.00%) | 8.86 (8.85–8.89) / 9.85 (9.42–10.5) / 10.6 (10.1–11.4) (0.00%) — 2.4× faster |
| home:nextup | 25.4 (24.9–25.6) / 28.0 (27.9–28.5) / 31.3 (30.8–31.6) (0.00%) | 9.29 (9.22–9.30) / 10.2 (10.1–10.3) / 10.9 (10.5–11.3) (0.00%) — 2.7× faster |
| home:resume-audio | 19.1 (19.1–19.9) / 22.5 (22.1–24.0) / 26.0 (25.1–26.6) (0.00%) | 5.39 (5.34–5.47) / 6.16 (6.15–7.02) / 9.07 (6.31–12.5) (0.00%) — 3.5× faster |
| home:resume-book | 19.0 (18.9–19.3) / 22.3 (22.3–22.5) / 25.5 (24.0–26.0) (0.00%) | 1.27 (1.21–1.29) / 2.54 (1.79–3.11) / 3.61 (2.19–13.0) (0.00%) — 15.0× faster |
| home:resume-video | 21.1 (21.0–21.2) / 23.6 (23.5–23.9) / 27.2 (26.4–28.3) (0.00%) | 6.68 (6.60–6.68) / 7.75 (7.65–8.40) / 8.67 (8.02–9.51) (0.00%) — 3.2× faster |
| home:views | 20.1 (19.9–20.3) / 23.1 (22.5–23.5) / 26.7 (25.9–27.0) (0.00%) | 1.88 (1.79–1.91) / 3.49 (3.46–4.17) / 3.89 (3.77–10.4) (0.00%) — 10.7× faster |
| movies:image | 5.36 (5.24–5.41) / 7.61 (7.56–7.80) / 8.24 (8.14–8.41) (0.00%) | 0.46 (0.45–0.48) / 0.62 (0.62–2.14) / 0.70 (0.66–11.8) (0.00%) — 11.7× faster |
| movies:items | 24.4 (24.3–24.8) / 27.3 (26.7–29.5) / 35.4 (35.2–36.0) (0.00%) | 11.3 (11.2–11.3) / 12.2 (12.0–12.3) / 12.4 (12.2–12.4) (0.00%) — 2.2× faster |
| detail:image | 4.70 (4.65–4.73) / 7.56 (7.53–8.38) / 8.99 (8.48–15.6) (0.00%) | 0.39 (0.37–0.42) / 0.52 (0.50–11.2) / 1.15 (0.55–11.6) (0.00%) — 12.1× faster |
| detail:item | 11.8 (11.5–11.9) / 13.1 (13.0–13.8) / 14.0 (13.3–14.1) (0.00%) | 1.72 (1.70–1.72) / 1.86 (1.82–2.17) / 2.29 (1.90–2.46) (0.00%) — 6.9× faster |
| detail:local-trailers | 8.05 (7.92–8.07) / 8.89 (8.57–9.89) / 9.09 (8.77–10.1) (0.00%) | 0.94 (0.94–1.00) / 1.08 (1.06–1.08) / 1.10 (1.09–1.13) (0.00%) — 8.6× faster |
| detail:similar | 21.7 / 23.0 (23.0–23.7) / 23.7 (23.1–24.0) (0.00%) | 4.15 (4.01–4.20) / 4.73 (4.70–4.77) / 4.78 (4.74–4.90) (0.00%) — 5.2× faster |
| detail:special-features | 8.06 (7.92–8.08) / 8.90 (8.58–9.89) / 9.12 (8.73–10.1) (0.00%) | 0.98 (0.97–1.02) / 1.14 (1.07–1.17) / 1.22 (1.12–1.72) (0.00%) — 8.2× faster |
| series:episodes | 5.80 (5.76–5.91) / 7.44 (6.33–8.30) / 8.10 (6.61–8.90) (0.00%) | 1.05 (1.03–1.05) / 1.22 (1.17–1.31) / 1.29 (1.18–1.36) (0.00%) — 5.5× faster |
| series:image | 3.52 (3.47–3.54) / 4.67 (4.43–4.83) / 4.88 (4.73–12.2) (0.00%) | 0.36 (0.36–0.39) / 0.43 (0.40–1.99) / 0.44 (0.41–2.07) (0.00%) — 9.8× faster |
| series:item | 10.7 (10.7–10.8) / 12.0 (10.9–147) / 12.8 (11.0–279) (0.00%) | 1.87 (1.86–1.93) / 2.19 (2.15–2.22) / 2.21 (2.16–2.43) (0.00%) — 5.7× faster |
| series:seasons | 7.94 (7.90–7.95) / 76.5 (8.13–77.2) / 142 (8.20–143) (0.00%) | 1.51 (1.47–1.53) / 1.80 (1.65–1.90) / 1.90 (1.65–2.12) (0.00%) — 5.3× faster |
| series:similar | 13.9 (13.8–14.0) / 14.8 (14.7–151) / 15.6 (14.8–282) (0.00%) | 3.16 (3.10–3.17) / 3.33 (3.31–3.39) / 3.42 (3.34–3.43) (0.00%) — 4.4× faster |
| search:artists | 11.1 (11.0–11.1) / 21.3 (20.5–21.4) / 28.5 (28.3–29.6) (0.00%) | 1.42 (1.39–1.43) / 2.02 (1.82–2.60) / 2.21 (2.05–2.91) (0.00%) — 7.8× faster |
| search:image | 5.25 (5.25–5.69) / 7.30 (6.67–14.5) / 8.29 (7.93–18.0) (0.00%) | 0.48 (0.45–0.54) / 0.67 (0.64–2.00) / 0.86 (0.74–2.28) (0.00%) — 10.9× faster ⚠[1] |
| search:items | 53.3 (52.9–55.4) / 71.2 (71.2–72.4) / 74.9 (74.4–75.9) (0.00%) | 17.3 (17.0–17.6) / 18.5 (18.5–18.8) / 18.9 (18.5–19.2) (0.00%) — 3.1× faster ⚠[2] |
| search:persons | 17.2 (17.1–17.5) / 19.1 (18.2–19.5) / 19.2 (18.5–19.9) (0.00%) | 2.17 (2.05–2.20) / 2.56 (2.33–2.75) / 2.56 (2.33–3.03) (0.00%) — 7.9× faster |
| search:programs | 14.9 (14.8–14.9) / 16.7 / 16.8 (16.8–17.2) (0.00%) | 0.98 (0.94–1.04) / 1.11 (1.07–1.38) / 1.12 (1.08–1.53) (0.00%) — 15.2× faster |
| search:videos | 32.5 (32.5–33.0) / 34.3 (34.2–35.6) / 34.9 (34.3–36.1) (0.00%) | 5.29 (5.21–5.30) / 5.72 (5.61–6.03) / 5.98 (5.66–6.22) (0.00%) — 6.1× faster |
| playback:intros | 4.83 (4.79–4.84) / 5.91 (5.68–6.01) / 6.56 (6.21–6.82) (0.00%) | 1.50 (1.49–1.56) / 2.79 (2.66–3.80) / 3.85 (3.65–3.89) (0.00%) — 3.2× faster |
| playback:playbackinfo | 8.66 (8.65–8.73) / 11.2 (11.0–11.5) / 11.8 (11.5–13.0) (0.00%) | 1.51 (1.50–1.55) / 1.72 (1.72–1.86) / 1.76 (1.74–1.93) (0.00%) — 5.7× faster |
| playback:playing | 9.02 (8.99–9.84) / 11.2 (11.0–16.6) / 11.4 (11.2–19.9) (0.00%) | 1.72 (1.66–1.81) / 1.89 (1.89–1.90) / 1.91 (1.89–1.92) (0.00%) — 5.2× faster |
| playback:segments | 4.90 (4.83–4.93) / 5.86 (5.68–6.20) / 6.65 (6.26–7.02) (0.00%) | 1.87 (1.86–1.93) / 3.00 (2.83–3.84) / 3.89 (3.64–3.90) (0.00%) — 2.6× faster |
| playback:stopped | 6.77 (6.74–6.86) / 7.65 (7.56–8.80) / 8.17 (8.01–10.3) (0.00%) | 1.28 (1.28–1.30) / 1.48 (1.44–1.53) / 1.52 (1.45–1.58) (0.00%) — 5.3× faster |
| image | 4.82 (4.78–4.83) / 6.92 (6.86–7.74) / 8.12 (7.97–14.9) (0.00%) | 0.40 (0.40–0.43) / 0.62 (0.60–1.91) / 0.95 (0.70–11.6) (0.00%) — 12.1× faster ⚠[3] |

### Image response bytes — unloaded (diagnostic)

| Run | Server | Bytes |
|---|---|---|
| 1 | Jellyfin 12.4.0 | 8218263 |
| 1 | Ferrofin | 7772572 |
| 2 | Jellyfin 12.4.0 | 8218263 |
| 2 | Ferrofin | 7700627 |
| 3 | Jellyfin 12.4.0 | 8140406 |
| 3 | Ferrofin | 7772572 |

### Latency — loaded (5 screens/s)

| screen | Jellyfin 12.4.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home | 44 (44–45) / 47 / 49 (0.00%) | 12 / 14 (14–15) / 17 (16–18) (0.00%) — 3.7× faster |
| movies | 34 (34–35) / 37 (37–38) / 39 (39–40) (0.00%) | 13 / 15 (14–15) / 16 (15–16) (0.00%) — 2.6× faster |
| detail | 31 (31–32) / 34 (33–37) / 44 (34–44) (0.00%) | 6 (5–6) / 6 (6–7) / 9 (7–9) (0.00%) — 5.2× faster |
| series | 24 / 26 (26–28) / 27 (26–36) (0.00%) | 5 / 6 / 6 (0.00%) — 4.8× faster |
| search | 55 / 62 (61–69) / 77 (0.00%) | 21 / 25 (25–27) / 32 (31–33) (0.00%) — 2.6× faster ⚠[1] |
| playback | 28 (28–29) / 31 (30–32) / 49 (45–49) (0.00%) | 6 (6–7) / 7 (7–8) / 9 (9–10) (0.00%) — 4.7× faster |

| endpoint | Jellyfin 12.4.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home:image | 4.66 (4.65–4.71) / 6.30 (6.26–6.40) / 6.71 (6.46–6.85) (0.00%) | 0.41 (0.41–0.42) / 0.53 (0.53–0.54) / 0.59 (0.58–0.59) (0.00%) — 11.4× faster |
| home:latest-movies | 11.1 (10.8–11.3) / 13.3 (13.2–13.6) / 14.3 (14.1–14.3) (0.00%) | 4.49 (4.43–4.50) / 5.75 (5.56–5.77) / 6.45 (6.37–6.55) (0.00%) — 2.5× faster |
| home:latest-music | 6.96 (6.95–7.08) / 9.32 (9.14–9.43) / 10.3 (10.2–10.3) (0.00%) | 2.75 (2.72–2.82) / 3.89 (3.68–3.90) / 4.41 (4.21–4.52) (0.00%) — 2.5× faster |
| home:latest-shows | 21.4 (21.3–21.7) / 24.0 (23.2–24.7) / 25.7 (25.6–26.8) (0.00%) | 8.79 (8.77–8.86) / 9.77 (9.58–10.0) / 10.5 (10.3–10.6) (0.00%) — 2.4× faster |
| home:nextup | 24.6 (24.5–25.2) / 27.1 (26.9–27.6) / 27.8 (27.7–28.3) (0.00%) | 9.19 (9.18–9.20) / 9.95 (9.75–10.0) / 10.7 (10.5–10.9) (0.00%) — 2.7× faster |
| home:resume-audio | 18.6 (18.4–19.0) / 21.6 (20.9–22.0) / 22.8 (22.7–23.2) (0.00%) | 5.28 (5.27–5.32) / 5.82 (5.75–5.92) / 7.20 (6.80–7.71) (0.00%) — 3.5× faster |
| home:resume-book | 18.6 (18.6–19.2) / 21.7 (20.8–21.7) / 22.8 (22.7–23.0) (0.00%) | 1.15 (1.13–1.16) / 1.93 (1.72–1.93) / 2.84 (2.80–2.87) (0.00%) — 16.2× faster |
| home:resume-video | 20.5 (20.4–20.8) / 23.0 (23.0–23.3) / 24.4 (24.4–24.6) (0.00%) | 6.66 (6.64–6.68) / 7.41 (7.39–7.62) / 8.67 (7.90–8.89) (0.00%) — 3.1× faster |
| home:views | 19.5 (19.4–20.1) / 22.3 (21.5–22.5) / 23.1 (22.9–23.3) (0.00%) | 1.78 (1.74–1.79) / 3.32 (3.30–3.47) / 3.70 (3.68–4.06) (0.00%) — 11.0× faster |
| movies:image | 4.71 (4.65–4.76) / 6.30 (6.16–6.30) / 6.61 (6.60–6.96) (0.00%) | 0.45 (0.44–0.45) / 0.61 (0.61–0.62) / 0.67 (0.66–0.68) (0.00%) — 10.5× faster |
| movies:items | 23.6 (23.2–24.0) / 25.7 (25.6–26.5) / 27.1 (26.8–27.4) (0.00%) | 10.9 (10.8–10.9) / 11.8 (11.6–12.0) / 12.3 (12.1–12.3) (0.00%) — 2.2× faster |
| detail:image | 4.49 (4.41–4.55) / 6.05 (6.03–6.22) / 6.98 (6.82–7.32) (0.00%) | 0.37 / 0.49 (0.48–0.49) / 0.56 (0.55–0.57) (0.00%) — 12.1× faster |
| detail:item | 10.9 (10.8–10.9) / 13.2 (12.9–13.3) / 13.9 (13.5–14.5) (0.00%) | 1.50 (1.46–1.52) / 1.89 (1.80–2.46) / 3.05 (2.46–3.30) (0.00%) — 7.3× faster |
| detail:local-trailers | 7.04 (6.94–7.14) / 9.06 (8.81–9.44) / 9.55 (9.45–9.88) (0.00%) | 0.75 (0.74–0.79) / 1.03 (1.00–1.05) / 1.10 (1.06–1.26) (0.00%) — 9.4× faster |
| detail:similar | 20.4 (20.4–21.1) / 22.5 (22.5–23.5) / 23.5 (23.2–25.8) (0.00%) | 3.87 (3.82–3.95) / 4.58 (4.45–4.62) / 4.76 (4.69–4.82) (0.00%) — 5.3× faster |
| detail:special-features | 7.06 (6.92–7.20) / 9.06 (8.70–9.55) / 9.61 (9.57–9.94) (0.00%) | 0.76 (0.76–0.80) / 1.04 (1.01–1.04) / 1.11 (1.08–1.86) (0.00%) — 9.3× faster |
| series:episodes | 5.88 (5.81–5.89) / 7.00 (6.73–7.28) / 7.53 (6.82–7.54) (0.00%) | 0.95 (0.95–0.96) / 1.16 (1.13–1.22) / 1.28 (1.21–1.34) (0.00%) — 6.2× faster |
| series:image | 3.50 (3.47–3.61) / 5.14 (3.90–5.26) / 5.33 (3.96–14.8) (0.00%) | 0.35 (0.34–0.35) / 0.42 (0.40–0.43) / 0.43 (0.41–0.97) (0.00%) — 10.0× faster |
| series:item | 10.4 (10.1–10.5) / 11.9 (11.8–12.5) / 12.6 (12.1–12.6) (0.00%) | 1.56 (1.52–1.57) / 1.91 (1.79–1.92) / 1.98 (1.96–2.32) (0.00%) — 6.7× faster |
| series:seasons | 7.67 (7.38–7.74) / 9.12 (8.79–9.47) / 9.62 (9.43–10.2) (0.00%) | 1.25 (1.23–1.35) / 1.63 (1.54–1.71) / 1.74 (1.60–1.84) (0.00%) — 6.1× faster |
| series:similar | 13.6 (13.2–13.6) / 15.2 (15.1–15.4) / 15.5 (15.4–15.7) (0.00%) | 2.96 (2.86–2.98) / 3.27 (3.26–3.28) / 3.39 (3.37–3.41) (0.00%) — 4.6× faster |
| search:artists | 10.6 (10.6–10.8) / 12.5 (12.4–12.6) / 12.6 (12.5–13.0) (0.00%) | 1.26 (1.21–1.31) / 2.16 (1.52–2.31) / 2.37 (1.93–2.77) (0.00%) — 8.4× faster |
| search:image | 4.85 (4.82–4.92) / 6.45 (6.40–6.50) / 7.32 (6.78–7.62) (0.00%) | 0.49 (0.47–0.50) / 0.66 / 0.70 (0.70–0.75) (0.00%) — 9.9× faster ⚠[1] |
| search:items | 42.0 (41.5–42.3) / 48.1 (47.5–48.1) / 61.5 (61.3–62.8) (0.00%) | 17.1 (17.1–17.2) / 19.5 (19.1–20.7) / 25.8 (25.6–26.0) (0.00%) — 2.5× faster ⚠[2] |
| search:persons | 16.8 (16.7–17.0) / 18.7 (18.7–19.2) / 19.7 (19.4–20.0) (0.00%) | 2.14 (2.09–2.16) / 2.56 (2.36–2.64) / 2.85 (2.57–3.50) (0.00%) — 7.9× faster |
| search:programs | 14.3 (14.3–14.6) / 16.3 (16.2–16.4) / 16.8 (16.4–16.8) (0.00%) | 0.93 (0.84–0.94) / 1.20 (1.12–1.43) / 2.04 (1.36–2.38) (0.00%) — 15.4× faster |
| search:videos | 31.9 (31.9–32.4) / 34.3 (34.2–34.3) / 34.7 (34.7–35.0) (0.00%) | 5.22 (5.18–5.30) / 5.66 (5.46–5.73) / 6.07 (5.64–6.22) (0.00%) — 6.1× faster |
| playback:intros | 4.81 (4.77–4.82) / 6.30 (6.27–6.34) / 6.48 (6.38–6.76) (0.00%) | 1.47 (1.44–1.56) / 1.61 (1.60–1.74) / 2.86 (2.77–3.56) (0.00%) — 3.3× faster |
| playback:playbackinfo | 7.47 (7.44–7.69) / 8.39 (8.23–8.83) / 9.45 (8.91–9.88) (0.00%) | 1.17 (1.08–1.19) / 1.51 (1.50–1.60) / 2.77 (2.69–2.78) (0.00%) — 6.4× faster |
| playback:playing | 8.47 (8.46–8.55) / 10.0 (9.55–10.1) / 16.7 (15.2–20.6) (0.00%) | 1.67 (1.67–1.80) / 2.00 (1.98–2.08) / 4.55 (4.25–4.65) (0.00%) — 5.1× faster |
| playback:segments | 4.85 (4.84–4.89) / 6.39 (6.34–6.49) / 6.80 (6.46–6.91) (0.00%) | 1.82 (1.80–1.98) / 2.03 (2.01–2.16) / 3.39 (3.15–3.79) (0.00%) — 2.7× faster |
| playback:stopped | 6.61 (6.51–6.65) / 7.71 (7.30–8.34) / 15.5 (15.0–28.7) (0.00%) | 1.24 (1.24–1.32) / 1.47 (1.45–1.56) / 1.59 (1.59–1.60) (0.00%) — 5.3× faster |
| image | 4.63 (4.56–4.67) / 6.26 (6.20–6.34) / 6.76 (6.62–7.06) (0.00%) | 0.40 (0.39–0.40) / 0.60 (0.59–0.60) / 0.67 (0.66–0.67) (0.00%) — 11.6× faster ⚠[3] |

### Image response bytes — loaded (diagnostic)

| Run | Server | Bytes |
|---|---|---|
| 1 | Jellyfin 12.4.0 | 40942827 |
| 1 | Ferrofin | 38400301 |
| 2 | Jellyfin 12.4.0 | 40864970 |
| 2 | Ferrofin | 38400301 |
| 3 | Jellyfin 12.4.0 | 40942827 |
| 3 | Ferrofin | 38328356 |

### Latency — stress (25 screens/s)

| screen | Jellyfin 12.4.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home | 44 (43–44) / 47 (47–48) / 50 (49–50) (0.00%) | 12 / 14 / 16 (0.00%) — 3.7× faster |
| movies | 35 / 39 (38–39) / 40 (39–40) (0.00%) | 12 / 13 / 15 (14–15) (0.00%) — 2.9× faster |
| detail | 31 / 34 (33–34) / 35 (35–36) (0.00%) | 5 / 6 / 7 (6–7) (0.00%) — 6.2× faster |
| series | 25 (24–25) / 27 / 29 (28–29) (0.00%) | 4 / 5 / 5 (5–6) (0.00%) — 6.2× faster |
| search | 55 (55–56) / 64 (63–71) / 81 (80–82) (0.00%) | 20 / 24 (24–26) / 32 (31–33) (0.00%) — 2.8× faster ⚠[1] |
| playback | 31 / 34 (34–35) / 54 (53–54) (0.00%) | 6 / 7 / 9 (0.00%) — 5.2× faster |

| endpoint | Jellyfin 12.4.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home:image | 4.76 (4.73–4.79) / 6.64 (6.47–6.64) / 7.61 (7.43–7.96) (0.00%) | 0.39 (0.38–0.39) / 0.51 / 0.57 (0.57–0.58) (0.00%) — 12.2× faster |
| home:latest-movies | 10.1 (10.1–10.2) / 12.8 (12.7–13.0) / 13.7 (13.6–13.8) (0.00%) | 4.14 (4.12–4.16) / 5.79 (5.60–5.82) / 6.50 (6.34–6.74) (0.00%) — 2.4× faster |
| home:latest-music | 7.11 (7.09–7.16) / 9.40 (9.20–9.62) / 10.7 (10.3–10.7) (0.00%) | 2.66 (2.65–2.74) / 4.18 (4.06–4.21) / 4.60 (4.55–4.77) (0.00%) — 2.7× faster |
| home:latest-shows | 21.6 (21.5–21.7) / 24.0 (24.0–24.2) / 25.9 (25.8–26.2) (0.00%) | 8.75 (8.75–8.80) / 10.2 (10.1–10.4) / 11.0 (10.8–11.1) (0.00%) — 2.5× faster |
| home:nextup | 24.1 (23.9–24.1) / 26.6 (26.2–26.6) / 28.2 (27.0–28.5) (0.00%) | 9.04 (9.01–9.04) / 10.0 (9.94–10.2) / 11.0 (10.8–11.0) (0.00%) — 2.7× faster |
| home:resume-audio | 17.7 (17.5–17.8) / 21.0 (20.7–21.3) / 22.6 (22.0–23.0) (0.00%) | 5.10 / 5.82 (5.73–5.97) / 7.17 (7.00–7.18) (0.00%) — 3.5× faster |
| home:resume-book | 17.7 (17.6–17.8) / 21.0 (20.7–21.2) / 22.3 (22.0–23.1) (0.00%) | 0.97 (0.97–0.98) / 2.56 (2.52–2.57) / 2.82 (2.78–2.82) (0.00%) — 18.2× faster |
| home:resume-video | 19.6 (19.4–19.7) / 22.8 (22.4–22.9) / 24.0 (23.5–24.5) (0.00%) | 6.38 (6.36–6.38) / 7.39 (7.33–7.61) / 8.33 (8.04–8.39) (0.00%) — 3.1× faster |
| home:views | 18.7 (18.5–18.7) / 21.4 (21.2–21.7) / 22.7 (22.0–23.1) (0.00%) | 1.69 (1.68–1.72) / 3.65 (3.54–3.66) / 4.95 (4.91–4.99) (0.00%) — 11.1× faster |
| movies:image | 4.60 (4.59–4.63) / 6.37 (6.27–6.41) / 7.30 (7.19–7.34) (0.00%) | 0.38 (0.37–0.38) / 0.51 / 0.62 (0.60–0.62) (0.00%) — 12.1× faster |
| movies:items | 24.1 (24.0–24.3) / 27.2 (26.7–27.2) / 28.3 (27.8–28.9) (0.00%) | 9.88 (9.80–9.91) / 10.8 (10.8–10.9) / 12.2 (11.9–12.2) (0.00%) — 2.4× faster |
| detail:image | 4.56 (4.53–4.56) / 6.45 (6.26–6.48) / 7.59 (7.35–7.72) (0.00%) | 0.32 / 0.43 / 0.50 (0.50–0.53) (0.00%) — 14.2× faster |
| detail:item | 10.1 (10.0–10.1) / 12.3 (12.3–12.4) / 13.1 (13.1–13.3) (0.00%) | 1.22 / 1.97 (1.96–2.07) / 2.70 (2.61–3.09) (0.00%) — 8.3× faster |
| detail:local-trailers | 6.14 (6.12–6.16) / 8.02 (8.01–8.03) / 8.95 (8.80–9.16) (0.00%) | 0.56 (0.55–0.56) / 0.73 (0.73–0.83) / 1.23 (1.02–1.46) (0.00%) — 11.0× faster |
| detail:similar | 20.2 (20.1–20.3) / 22.6 (22.6–22.7) / 23.6 (23.2–23.8) (0.00%) | 3.24 (3.23–3.27) / 3.98 (3.92–4.03) / 4.41 (4.33–4.43) (0.00%) — 6.2× faster |
| detail:special-features | 6.12 (6.11–6.14) / 8.01 (8.00–8.02) / 8.80 (8.79–9.28) (0.00%) | 0.57 (0.56–0.57) / 0.73 (0.72–0.79) / 1.06 (0.94–1.09) (0.00%) — 10.7× faster |
| series:episodes | 5.92 (5.90–5.98) / 7.05 (6.84–7.26) / 7.41 (7.20–8.69) (0.00%) | 0.89 (0.88–0.89) / 1.07 (1.06–1.09) / 1.15 (1.14–1.16) (0.00%) — 6.7× faster |
| series:image | 3.48 (3.47–3.51) / 4.76 (4.59–4.81) / 5.50 (5.28–5.54) (0.00%) | 0.30 / 0.37 (0.36–0.37) / 0.40 (0.39–0.41) (0.00%) — 11.6× faster |
| series:item | 10.9 (10.8–11.0) / 13.2 (13.2–13.6) / 14.4 (14.3–14.7) (0.00%) | 1.26 / 1.52 (1.48–1.55) / 1.81 (1.72–1.93) (0.00%) — 8.7× faster |
| series:seasons | 8.04 (7.93–8.15) / 10.3 (10.1–10.6) / 11.4 (11.4–11.8) (0.00%) | 0.94 (0.94–0.96) / 1.20 (1.18–1.24) / 1.48 (1.37–1.49) (0.00%) — 8.6× faster |
| series:similar | 14.5 (14.2–14.6) / 16.5 (16.3–16.7) / 17.6 (17.4–17.8) (0.00%) | 2.51 (2.45–2.52) / 2.87 (2.83–2.92) / 3.06 (3.04–3.19) (0.00%) — 5.8× faster |
| search:artists | 9.75 (9.66–9.76) / 12.1 (11.6–12.2) / 12.9 (12.4–13.4) (0.00%) | 1.00 / 2.14 (1.93–2.36) / 2.83 (2.56–2.84) (0.00%) — 9.8× faster |
| search:image | 5.07 (5.05–5.09) / 7.51 (7.28–7.57) / 8.84 (8.47–8.91) (0.00%) | 0.46 (0.45–0.46) / 0.63 (0.61–0.63) / 0.71 (0.68–0.73) (0.00%) — 11.0× faster ⚠[1] |
| search:items | 41.5 (41.3–41.7) / 49.4 (48.3–51.1) / 64.9 (64.7–65.4) (0.00%) | 16.6 (16.6–16.7) / 19.4 (19.2–22.3) / 25.7 (25.4–25.9) (0.00%) — 2.5× faster ⚠[2] |
| search:persons | 16.3 (16.3–16.4) / 18.9 (18.6–18.9) / 19.9 (19.4–20.1) (0.00%) | 1.91 (1.91–1.93) / 2.39 (2.37–2.62) / 2.90 (2.84–2.96) (0.00%) — 8.5× faster |
| search:programs | 13.8 (13.7–13.8) / 16.0 (15.8–16.3) / 16.7 (16.5–17.5) (0.00%) | 0.65 (0.65–0.66) / 1.64 (1.30–1.66) / 2.51 (1.96–2.53) (0.00%) — 21.2× faster |
| search:videos | 31.5 (31.4–31.9) / 34.3 (33.6–34.4) / 35.3 (34.4–36.1) (0.00%) | 4.76 (4.76–4.77) / 5.37 (5.35–5.40) / 5.61 (5.61–5.80) (0.00%) — 6.6× faster |
| playback:intros | 5.89 (5.82–6.09) / 8.53 (8.26–8.70) / 9.73 (9.53–10.6) (0.00%) | 1.47 (1.39–1.49) / 1.64 (1.53–1.78) / 1.74 (1.61–2.03) (0.00%) — 4.0× faster |
| playback:playbackinfo | 8.29 (8.15–8.44) / 11.4 (11.0–11.4) / 12.2 (12.2–12.3) (0.00%) | 0.84 (0.84–0.85) / 0.96 (0.95–0.97) / 1.15 (1.10–1.29) (0.00%) — 9.9× faster |
| playback:playing | 8.66 (8.55–8.70) / 10.9 / 24.9 (15.0–25.6) (0.00%) | 1.75 (1.67–1.78) / 2.07 (1.87–2.07) / 4.60 (4.49–4.85) (0.00%) — 4.9× faster |
| playback:segments | 5.90 (5.88–6.07) / 8.50 (8.22–8.56) / 9.75 (8.99–10.4) (0.00%) | 1.89 (1.76–1.89) / 2.07 (1.92–2.20) / 2.17 (2.02–2.52) (0.00%) — 3.1× faster |
| playback:stopped | 6.63 (6.63–6.65) / 9.05 (8.64–9.67) / 22.9 (18.6–29.2) (0.00%) | 1.28 (1.23–1.29) / 1.44 (1.37–1.48) / 1.50 (1.42–1.58) (0.00%) — 5.2× faster |
| image | 4.71 (4.68–4.71) / 6.69 (6.49–6.71) / 7.76 (7.51–7.96) (0.00%) | 0.35 / 0.54 (0.53–0.54) / 0.63 (0.62–0.63) (0.00%) — 13.5× faster ⚠[3] |

### Image response bytes — stress (diagnostic)

| Run | Server | Bytes |
|---|---|---|
| 1 | Jellyfin 12.4.0 | 204324850 |
| 1 | Ferrofin | 191713725 |
| 2 | Jellyfin 12.4.0 | 204402707 |
| 2 | Ferrofin | 191713725 |
| 3 | Jellyfin 12.4.0 | 204324850 |
| 3 | Ferrofin | 191713725 |

### Shape coverage

| Run | Server | Slots | Requests | Distinct requests | Duration (s) | Distinct picks | Evidence |
|---|---|---|---|---|---|---|---|
| 1 | Jellyfin 12.4.0 | 600 | 8732 | 2926 | 30.080630245000002 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 1 | Ferrofin | 600 | 8732 | 2949 | 13.582577207 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 2 | Jellyfin 12.4.0 | 600 | 8732 | 2926 | 30.508812656 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 2 | Ferrofin | 600 | 8732 | 2949 | 13.560734547 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 3 | Jellyfin 12.4.0 | 600 | 8732 | 2926 | 30.386445917 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 3 | Ferrofin | 600 | 8732 | 2949 | 13.821395233999999 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |

### CPU and sample coverage — invocation and idle windows

CPU seconds use cumulative counters at the reported bracketing samples; load includes setup/teardown and harness overhead, and steady is post-load idle. Missing historical counters remain unavailable.

Average cores busy is CPU-seconds divided by observed wall seconds; 1.0 means one core fully occupied. Capacity busy divides that by the allocated logical CPU count. CPU milliseconds per completed screen divides total window CPU by completed mixed-screen iterations, including failed screens; it is not per-endpoint attribution. Lower values mean less CPU work only when completed work and validation agree. Each row retains its individual run; flagged workloads cannot support CPU savings claims. See the latency/work flags for differences in executed work.

#### CPU consumption (CPU-seconds)

| Window / metric | Jellyfin 12.4.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 15.139 | 2.912 |
| loaded · run 1 | 65.913 | 13.813 |
| stress · run 1 | 322.068 | 65.263 |
| steady · run 1 | 0.045 | 0.005 |
| unloaded · run 2 | 15.196 | 2.852 |
| loaded · run 2 | 66.012 | 13.905 |
| stress · run 2 | 323.723 | 65.004 |
| steady · run 2 | 0.044 | 0.005 |
| unloaded · run 3 | 15.006 | 2.902 |
| loaded · run 3 | 66.909 | 13.842 |
| stress · run 3 | 323.966 | 65.097 |
| steady · run 3 | 0.045 | 0.005 |

#### Average CPU cores busy

| Window / metric | Jellyfin 12.4.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 0.126 | 0.024 |
| loaded · run 1 | 0.547 | 0.115 |
| stress · run 1 | 2.673 | 0.542 |
| steady · run 1 | 0.001 | 0.000 |
| unloaded · run 2 | 0.126 | 0.024 |
| loaded · run 2 | 0.548 | 0.115 |
| stress · run 2 | 2.689 | 0.540 |
| steady · run 2 | 0.001 | 0.000 |
| unloaded · run 3 | 0.125 | 0.024 |
| loaded · run 3 | 0.556 | 0.115 |
| stress · run 3 | 2.691 | 0.541 |
| steady · run 3 | 0.001 | 0.000 |

#### Allocated CPU capacity busy (%)

| Window / metric | Jellyfin 12.4.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 1.57% | 0.30% |
| loaded · run 1 | 6.84% | 1.43% |
| stress · run 1 | 33.41% | 6.78% |
| steady · run 1 | 0.01% | 0.00% |
| unloaded · run 2 | 1.58% | 0.30% |
| loaded · run 2 | 6.85% | 1.44% |
| stress · run 2 | 33.61% | 6.75% |
| steady · run 2 | 0.01% | 0.00% |
| unloaded · run 3 | 1.56% | 0.30% |
| loaded · run 3 | 6.95% | 1.44% |
| stress · run 3 | 33.63% | 6.76% |
| steady · run 3 | 0.01% | 0.00% |

#### CPU milliseconds per completed screen

| Window / metric | Jellyfin 12.4.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 125.114 | 24.065 ⚠ workload validation failed |
| loaded · run 1 | 109.672 | 22.984 ⚠ workload validation failed |
| stress · run 1 | 107.356 | 21.747 ⚠ workload validation failed |
| unloaded · run 2 | 125.588 | 23.767 ⚠ workload validation failed |
| loaded · run 2 | 110.020 | 23.136 ⚠ workload validation failed |
| stress · run 2 | 107.872 | 21.661 ⚠ workload validation failed |
| unloaded · run 3 | 125.046 | 23.982 ⚠ workload validation failed |
| loaded · run 3 | 111.329 | 23.070 ⚠ workload validation failed |
| stress · run 3 | 107.989 | 21.692 ⚠ workload validation failed |

#### CPU per screen comparison (validated workloads only)

| Window / metric | Jellyfin 12.4.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 · vs Jellyfin 12.4.0 | reference | withheld — validation flags |
| loaded · run 1 · vs Jellyfin 12.4.0 | reference | withheld — validation flags |
| stress · run 1 · vs Jellyfin 12.4.0 | reference | withheld — validation flags |
| unloaded · run 2 · vs Jellyfin 12.4.0 | reference | withheld — validation flags |
| loaded · run 2 · vs Jellyfin 12.4.0 | reference | withheld — validation flags |
| stress · run 2 · vs Jellyfin 12.4.0 | reference | withheld — validation flags |
| unloaded · run 3 · vs Jellyfin 12.4.0 | reference | withheld — validation flags |
| loaded · run 3 · vs Jellyfin 12.4.0 | reference | withheld — validation flags |
| stress · run 3 · vs Jellyfin 12.4.0 | reference | withheld — validation flags |

#### CPU sample coverage

| Window / metric | Jellyfin 12.4.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 · Observed duration (s) | 120.500 | 120.400 |
| unloaded · run 1 · Samples | 1206 | 1205 |
| unloaded · run 1 · Largest gap (ms) | 100.307 | 100.299 |
| unloaded · run 1 · Observed start (UTC) | 2026-11-08T05:47:05.339+00:00 | 2026-11-08T06:00:32.175+00:00 |
| unloaded · run 1 · Observed end (UTC) | 2026-11-08T05:49:05.839+00:00 | 2026-11-08T06:02:32.575+00:00 |
| unloaded · run 1 · CPU evidence | available | available |
| loaded · run 1 · Observed duration (s) | 120.500 | 120.500 |
| loaded · run 1 · Samples | 1206 | 1206 |
| loaded · run 1 · Largest gap (ms) | 100.287 | 100.361 |
| loaded · run 1 · Observed start (UTC) | 2026-11-08T05:50:08.039+00:00 | 2026-11-08T06:03:34.275+00:00 |
| loaded · run 1 · Observed end (UTC) | 2026-11-08T05:52:08.539+00:00 | 2026-11-08T06:05:34.775+00:00 |
| loaded · run 1 · CPU evidence | available | available |
| stress · run 1 · Observed duration (s) | 120.500 | 120.400 |
| stress · run 1 · Samples | 1206 | 1205 |
| stress · run 1 · Largest gap (ms) | 102.013 | 100.214 |
| stress · run 1 · Observed start (UTC) | 2026-11-08T05:53:10.739+00:00 | 2026-11-08T06:06:36.475+00:00 |
| stress · run 1 · Observed end (UTC) | 2026-11-08T05:55:11.239+00:00 | 2026-11-08T06:08:36.875+00:00 |
| stress · run 1 · CPU evidence | available | available |
| steady · run 1 · Observed duration (s) | 60.200 | 60.200 |
| steady · run 1 · Samples | 603 | 603 |
| steady · run 1 · Largest gap (ms) | 100.299 | 100.306 |
| steady · run 1 · Observed start (UTC) | 2026-11-08T05:55:41.839+00:00 | 2026-11-08T06:09:07.475+00:00 |
| steady · run 1 · Observed end (UTC) | 2026-11-08T05:56:42.039+00:00 | 2026-11-08T06:10:07.675+00:00 |
| steady · run 1 · CPU evidence | available | available |
| unloaded · run 2 · Observed duration (s) | 120.500 | 120.400 |
| unloaded · run 2 · Samples | 1206 | 1205 |
| unloaded · run 2 · Largest gap (ms) | 101.075 | 101.043 |
| unloaded · run 2 · Observed start (UTC) | 2026-11-08T06:14:35.725+00:00 | 2026-11-08T06:28:02.384+00:00 |
| unloaded · run 2 · Observed end (UTC) | 2026-11-08T06:16:36.225+00:00 | 2026-11-08T06:30:02.784+00:00 |
| unloaded · run 2 · CPU evidence | available | available |
| loaded · run 2 · Observed duration (s) | 120.400 | 120.500 |
| loaded · run 2 · Samples | 1205 | 1206 |
| loaded · run 2 · Largest gap (ms) | 101.160 | 100.265 |
| loaded · run 2 · Observed start (UTC) | 2026-11-08T06:17:38.425+00:00 | 2026-11-08T06:31:04.484+00:00 |
| loaded · run 2 · Observed end (UTC) | 2026-11-08T06:19:38.825+00:00 | 2026-11-08T06:33:04.984+00:00 |
| loaded · run 2 · CPU evidence | available | available |
| stress · run 2 · Observed duration (s) | 120.400 | 120.400 |
| stress · run 2 · Samples | 1205 | 1205 |
| stress · run 2 · Largest gap (ms) | 100.214 | 100.298 |
| stress · run 2 · Observed start (UTC) | 2026-11-08T06:20:41.125+00:00 | 2026-11-08T06:34:06.684+00:00 |
| stress · run 2 · Observed end (UTC) | 2026-11-08T06:22:41.525+00:00 | 2026-11-08T06:36:07.084+00:00 |
| stress · run 2 · CPU evidence | available | available |
| steady · run 2 · Observed duration (s) | 60.200 | 60.100 |
| steady · run 2 · Samples | 603 | 602 |
| steady · run 2 · Largest gap (ms) | 100.308 | 100.580 |
| steady · run 2 · Observed start (UTC) | 2026-11-08T06:23:12.225+00:00 | 2026-11-08T06:36:37.684+00:00 |
| steady · run 2 · Observed end (UTC) | 2026-11-08T06:24:12.425+00:00 | 2026-11-08T06:37:37.784+00:00 |
| steady · run 2 · CPU evidence | available | available |
| unloaded · run 3 · Observed duration (s) | 120.400 | 120.400 |
| unloaded · run 3 · Samples | 1205 | 1205 |
| unloaded · run 3 · Largest gap (ms) | 100.284 | 100.324 |
| unloaded · run 3 · Observed start (UTC) | 2026-11-08T06:42:05.308+00:00 | 2026-11-08T06:55:34.565+00:00 |
| unloaded · run 3 · Observed end (UTC) | 2026-11-08T06:44:05.708+00:00 | 2026-11-08T06:57:34.965+00:00 |
| unloaded · run 3 · CPU evidence | available | available |
| loaded · run 3 · Observed duration (s) | 120.400 | 120.500 |
| loaded · run 3 · Samples | 1205 | 1206 |
| loaded · run 3 · Largest gap (ms) | 100.317 | 100.311 |
| loaded · run 3 · Observed start (UTC) | 2026-11-08T06:45:08.008+00:00 | 2026-11-08T06:58:36.665+00:00 |
| loaded · run 3 · Observed end (UTC) | 2026-11-08T06:47:08.408+00:00 | 2026-11-08T07:00:37.165+00:00 |
| loaded · run 3 · CPU evidence | available | available |
| stress · run 3 · Observed duration (s) | 120.400 | 120.400 |
| stress · run 3 · Samples | 1205 | 1205 |
| stress · run 3 · Largest gap (ms) | 100.163 | 100.219 |
| stress · run 3 · Observed start (UTC) | 2026-11-08T06:48:10.708+00:00 | 2026-11-08T07:01:38.865+00:00 |
| stress · run 3 · Observed end (UTC) | 2026-11-08T06:50:11.108+00:00 | 2026-11-08T07:03:39.265+00:00 |
| stress · run 3 · CPU evidence | available | available |
| steady · run 3 · Observed duration (s) | 60.200 | 60.200 |
| steady · run 3 · Samples | 603 | 603 |
| steady · run 3 · Largest gap (ms) | 100.301 | 100.298 |
| steady · run 3 · Observed start (UTC) | 2026-11-08T06:50:41.808+00:00 | 2026-11-08T07:04:09.865+00:00 |
| steady · run 3 · Observed end (UTC) | 2026-11-08T06:51:42.008+00:00 | 2026-11-08T07:05:10.065+00:00 |
| steady · run 3 · CPU evidence | available | available |

### Phase outcomes

| Run | Server | Phase | Status | Duration (s) | Start (UTC) | End (UTC) | Reason |
|---|---|---|---|---|---|---|---|
| 1 | Jellyfin 12.4.0 | startup | completed | 8.127 | 2026-11-08T05:43:38.682+00:00 | 2026-11-08T05:43:46.809+00:00 | — |
| 1 | Jellyfin 12.4.0 | startup-drain | completed | 30.187 | 2026-11-08T05:43:46.841+00:00 | 2026-11-08T05:44:17.028+00:00 | — |
| 1 | Jellyfin 12.4.0 | cleanup | completed | 0.386 | 2026-11-08T05:57:31.386+00:00 | 2026-11-08T05:57:31.772+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-setup | completed | 1.562 | 2026-11-08T05:44:17.071+00:00 | 2026-11-08T05:44:18.633+00:00 | — |
| 1 | Jellyfin 12.4.0 | counts | completed | 2.185 | 2026-11-08T05:44:18.667+00:00 | 2026-11-08T05:44:20.852+00:00 | — |
| 1 | Jellyfin 12.4.0 | shape | completed | 32.019 | 2026-11-08T05:44:21.267+00:00 | 2026-11-08T05:44:53.286+00:00 | — |
| 1 | Jellyfin 12.4.0 | coldstart | completed | 68.738 | 2026-11-08T05:44:53.987+00:00 | 2026-11-08T05:46:02.725+00:00 | — |
| 1 | Jellyfin 12.4.0 | ttfs | completed | 48.214 | 2026-11-08T05:56:42.803+00:00 | 2026-11-08T05:57:31.017+00:00 | — |
| 1 | Jellyfin 12.4.0 | unloaded | completed | 120.318 | 2026-11-08T05:47:05.429+00:00 | 2026-11-08T05:49:05.748+00:00 | — |
| 1 | Jellyfin 12.4.0 | warmup-unloaded | completed | 30.319 | 2026-11-08T05:46:34.201+00:00 | 2026-11-08T05:47:04.520+00:00 | — |
| 1 | Jellyfin 12.4.0 | drain-unloaded | completed | 30.176 | 2026-11-08T05:46:03.619+00:00 | 2026-11-08T05:46:33.795+00:00 | — |
| 1 | Jellyfin 12.4.0 | sampler | completed | 638.928 | 2026-11-08T05:46:03.485+00:00 | 2026-11-08T05:56:42.413+00:00 | — |
| 1 | Jellyfin 12.4.0 | steady | completed | 60.035 | 2026-11-08T05:55:41.931+00:00 | 2026-11-08T05:56:41.966+00:00 | — |
| 1 | Jellyfin 12.4.0 | drain-steady | completed | 30.182 | 2026-11-08T05:55:11.713+00:00 | 2026-11-08T05:55:41.895+00:00 | — |
| 1 | Jellyfin 12.4.0 | loaded | completed | 120.322 | 2026-11-08T05:50:08.111+00:00 | 2026-11-08T05:52:08.433+00:00 | — |
| 1 | Jellyfin 12.4.0 | warmup-loaded | completed | 30.314 | 2026-11-08T05:49:36.893+00:00 | 2026-11-08T05:50:07.207+00:00 | — |
| 1 | Jellyfin 12.4.0 | drain-loaded | completed | 30.178 | 2026-11-08T05:49:06.350+00:00 | 2026-11-08T05:49:36.528+00:00 | — |
| 1 | Jellyfin 12.4.0 | stress | completed | 120.297 | 2026-11-08T05:53:10.809+00:00 | 2026-11-08T05:55:11.106+00:00 | — |
| 1 | Jellyfin 12.4.0 | warmup-stress | completed | 30.322 | 2026-11-08T05:52:39.579+00:00 | 2026-11-08T05:53:09.901+00:00 | — |
| 1 | Jellyfin 12.4.0 | drain-stress | completed | 30.177 | 2026-11-08T05:52:09.037+00:00 | 2026-11-08T05:52:39.214+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-shape-before | completed | 0.348 | 2026-11-08T05:44:20.884+00:00 | 2026-11-08T05:44:21.232+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-shape-after | completed | 0.297 | 2026-11-08T05:44:53.323+00:00 | 2026-11-08T05:44:53.620+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-coldstart-before | completed | 0.299 | 2026-11-08T05:44:53.653+00:00 | 2026-11-08T05:44:53.952+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-coldstart-after | completed | 0.689 | 2026-11-08T05:46:02.760+00:00 | 2026-11-08T05:46:03.449+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-warmup-unloaded-before | completed | 0.337 | 2026-11-08T05:46:33.831+00:00 | 2026-11-08T05:46:34.168+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-warmup-unloaded-after | completed | 0.302 | 2026-11-08T05:47:04.555+00:00 | 2026-11-08T05:47:04.857+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-unloaded-before | completed | 0.295 | 2026-11-08T05:47:04.892+00:00 | 2026-11-08T05:47:05.187+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-unloaded-after | completed | 0.295 | 2026-11-08T05:49:06.021+00:00 | 2026-11-08T05:49:06.316+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-warmup-loaded-before | completed | 0.295 | 2026-11-08T05:49:36.562+00:00 | 2026-11-08T05:49:36.857+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-warmup-loaded-after | completed | 0.297 | 2026-11-08T05:50:07.242+00:00 | 2026-11-08T05:50:07.540+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-loaded-before | completed | 0.296 | 2026-11-08T05:50:07.573+00:00 | 2026-11-08T05:50:07.870+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-loaded-after | completed | 0.298 | 2026-11-08T05:52:08.706+00:00 | 2026-11-08T05:52:09.004+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-warmup-stress-before | completed | 0.295 | 2026-11-08T05:52:39.250+00:00 | 2026-11-08T05:52:39.544+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-warmup-stress-after | completed | 0.302 | 2026-11-08T05:53:09.935+00:00 | 2026-11-08T05:53:10.237+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-stress-before | completed | 0.297 | 2026-11-08T05:53:10.271+00:00 | 2026-11-08T05:53:10.568+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-stress-after | completed | 0.298 | 2026-11-08T05:55:11.380+00:00 | 2026-11-08T05:55:11.678+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-ttfs-before | completed | 0.320 | 2026-11-08T05:56:42.449+00:00 | 2026-11-08T05:56:42.769+00:00 | — |
| 1 | Jellyfin 12.4.0 | isolation-ttfs-after | completed | 0.299 | 2026-11-08T05:57:31.052+00:00 | 2026-11-08T05:57:31.350+00:00 | — |
| 1 | Ferrofin | startup | completed | 17.059 | 2026-11-08T05:57:32.642+00:00 | 2026-11-08T05:57:49.702+00:00 | — |
| 1 | Ferrofin | startup-drain | completed | 30.172 | 2026-11-08T05:57:49.735+00:00 | 2026-11-08T05:58:19.907+00:00 | — |
| 1 | Ferrofin | cleanup | completed | 0.386 | 2026-11-08T06:10:56.540+00:00 | 2026-11-08T06:10:56.926+00:00 | — |
| 1 | Ferrofin | isolation-setup | completed | 0.322 | 2026-11-08T05:58:19.952+00:00 | 2026-11-08T05:58:20.274+00:00 | — |
| 1 | Ferrofin | counts | completed | 0.551 | 2026-11-08T05:58:20.308+00:00 | 2026-11-08T05:58:20.859+00:00 | — |
| 1 | Ferrofin | shape | completed | 15.514 | 2026-11-08T05:58:21.097+00:00 | 2026-11-08T05:58:36.610+00:00 | — |
| 1 | Ferrofin | coldstart | completed | 53.411 | 2026-11-08T05:58:37.067+00:00 | 2026-11-08T05:59:30.477+00:00 | — |
| 1 | Ferrofin | ttfs | completed | 48.027 | 2026-11-08T06:10:08.262+00:00 | 2026-11-08T06:10:56.289+00:00 | — |
| 1 | Ferrofin | unloaded | completed | 120.289 | 2026-11-08T06:00:32.236+00:00 | 2026-11-08T06:02:32.524+00:00 | — |
| 1 | Ferrofin | warmup-unloaded | completed | 30.304 | 2026-11-08T06:00:01.269+00:00 | 2026-11-08T06:00:31.573+00:00 | — |
| 1 | Ferrofin | drain-unloaded | completed | 30.170 | 2026-11-08T05:59:30.856+00:00 | 2026-11-08T06:00:01.026+00:00 | — |
| 1 | Ferrofin | sampler | completed | 637.293 | 2026-11-08T05:59:30.723+00:00 | 2026-11-08T06:10:08.016+00:00 | — |
| 1 | Ferrofin | steady | completed | 60.033 | 2026-11-08T06:09:07.531+00:00 | 2026-11-08T06:10:07.564+00:00 | — |
| 1 | Ferrofin | drain-steady | completed | 30.171 | 2026-11-08T06:08:37.326+00:00 | 2026-11-08T06:09:07.497+00:00 | — |
| 1 | Ferrofin | loaded | completed | 120.315 | 2026-11-08T06:03:34.361+00:00 | 2026-11-08T06:05:34.676+00:00 | — |
| 1 | Ferrofin | warmup-loaded | completed | 30.276 | 2026-11-08T06:03:03.424+00:00 | 2026-11-08T06:03:33.700+00:00 | — |
| 1 | Ferrofin | drain-loaded | completed | 30.166 | 2026-11-08T06:02:33.014+00:00 | 2026-11-08T06:03:03.179+00:00 | — |
| 1 | Ferrofin | stress | completed | 120.302 | 2026-11-08T06:06:36.530+00:00 | 2026-11-08T06:08:36.833+00:00 | — |
| 1 | Ferrofin | warmup-stress | completed | 30.275 | 2026-11-08T06:06:05.578+00:00 | 2026-11-08T06:06:35.853+00:00 | — |
| 1 | Ferrofin | drain-stress | completed | 30.170 | 2026-11-08T06:05:35.165+00:00 | 2026-11-08T06:06:05.335+00:00 | — |
| 1 | Ferrofin | isolation-shape-before | completed | 0.171 | 2026-11-08T05:58:20.893+00:00 | 2026-11-08T05:58:21.064+00:00 | — |
| 1 | Ferrofin | isolation-shape-after | completed | 0.179 | 2026-11-08T05:58:36.643+00:00 | 2026-11-08T05:58:36.823+00:00 | — |
| 1 | Ferrofin | isolation-coldstart-before | completed | 0.175 | 2026-11-08T05:58:36.857+00:00 | 2026-11-08T05:58:37.032+00:00 | — |
| 1 | Ferrofin | isolation-coldstart-after | completed | 0.177 | 2026-11-08T05:59:30.512+00:00 | 2026-11-08T05:59:30.689+00:00 | — |
| 1 | Ferrofin | isolation-warmup-unloaded-before | completed | 0.176 | 2026-11-08T06:00:01.060+00:00 | 2026-11-08T06:00:01.237+00:00 | — |
| 1 | Ferrofin | isolation-warmup-unloaded-after | completed | 0.175 | 2026-11-08T06:00:31.608+00:00 | 2026-11-08T06:00:31.783+00:00 | — |
| 1 | Ferrofin | isolation-unloaded-before | completed | 0.174 | 2026-11-08T06:00:31.818+00:00 | 2026-11-08T06:00:31.992+00:00 | — |
| 1 | Ferrofin | isolation-unloaded-after | completed | 0.180 | 2026-11-08T06:02:32.800+00:00 | 2026-11-08T06:02:32.980+00:00 | — |
| 1 | Ferrofin | isolation-warmup-loaded-before | completed | 0.177 | 2026-11-08T06:03:03.214+00:00 | 2026-11-08T06:03:03.391+00:00 | — |
| 1 | Ferrofin | isolation-warmup-loaded-after | completed | 0.176 | 2026-11-08T06:03:33.736+00:00 | 2026-11-08T06:03:33.912+00:00 | — |
| 1 | Ferrofin | isolation-loaded-before | completed | 0.174 | 2026-11-08T06:03:33.945+00:00 | 2026-11-08T06:03:34.119+00:00 | — |
| 1 | Ferrofin | isolation-loaded-after | completed | 0.180 | 2026-11-08T06:05:34.951+00:00 | 2026-11-08T06:05:35.131+00:00 | — |
| 1 | Ferrofin | isolation-warmup-stress-before | completed | 0.173 | 2026-11-08T06:06:05.372+00:00 | 2026-11-08T06:06:05.545+00:00 | — |
| 1 | Ferrofin | isolation-warmup-stress-after | completed | 0.185 | 2026-11-08T06:06:35.887+00:00 | 2026-11-08T06:06:36.072+00:00 | — |
| 1 | Ferrofin | isolation-stress-before | completed | 0.181 | 2026-11-08T06:06:36.108+00:00 | 2026-11-08T06:06:36.289+00:00 | — |
| 1 | Ferrofin | isolation-stress-after | completed | 0.185 | 2026-11-08T06:08:37.108+00:00 | 2026-11-08T06:08:37.293+00:00 | — |
| 1 | Ferrofin | isolation-ttfs-before | completed | 0.179 | 2026-11-08T06:10:08.049+00:00 | 2026-11-08T06:10:08.229+00:00 | — |
| 1 | Ferrofin | isolation-ttfs-after | completed | 0.183 | 2026-11-08T06:10:56.323+00:00 | 2026-11-08T06:10:56.505+00:00 | — |
| 2 | Jellyfin 12.4.0 | startup | completed | 8.134 | 2026-11-08T06:11:08.370+00:00 | 2026-11-08T06:11:16.504+00:00 | — |
| 2 | Jellyfin 12.4.0 | startup-drain | completed | 30.183 | 2026-11-08T06:11:16.538+00:00 | 2026-11-08T06:11:46.721+00:00 | — |
| 2 | Jellyfin 12.4.0 | cleanup | completed | 0.369 | 2026-11-08T06:25:01.743+00:00 | 2026-11-08T06:25:02.112+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-setup | completed | 1.574 | 2026-11-08T06:11:46.766+00:00 | 2026-11-08T06:11:48.340+00:00 | — |
| 2 | Jellyfin 12.4.0 | counts | completed | 2.182 | 2026-11-08T06:11:48.374+00:00 | 2026-11-08T06:11:50.556+00:00 | — |
| 2 | Jellyfin 12.4.0 | shape | completed | 32.446 | 2026-11-08T06:11:50.970+00:00 | 2026-11-08T06:12:23.416+00:00 | — |
| 2 | Jellyfin 12.4.0 | coldstart | completed | 69.020 | 2026-11-08T06:12:24.118+00:00 | 2026-11-08T06:13:33.138+00:00 | — |
| 2 | Jellyfin 12.4.0 | ttfs | completed | 48.191 | 2026-11-08T06:24:13.184+00:00 | 2026-11-08T06:25:01.375+00:00 | — |
| 2 | Jellyfin 12.4.0 | unloaded | completed | 120.337 | 2026-11-08T06:14:35.809+00:00 | 2026-11-08T06:16:36.146+00:00 | — |
| 2 | Jellyfin 12.4.0 | warmup-unloaded | completed | 30.315 | 2026-11-08T06:14:04.589+00:00 | 2026-11-08T06:14:34.904+00:00 | — |
| 2 | Jellyfin 12.4.0 | drain-unloaded | completed | 30.180 | 2026-11-08T06:13:34.005+00:00 | 2026-11-08T06:14:04.185+00:00 | — |
| 2 | Jellyfin 12.4.0 | sampler | completed | 638.923 | 2026-11-08T06:13:33.872+00:00 | 2026-11-08T06:24:12.795+00:00 | — |
| 2 | Jellyfin 12.4.0 | steady | completed | 60.033 | 2026-11-08T06:23:12.311+00:00 | 2026-11-08T06:24:12.344+00:00 | — |
| 2 | Jellyfin 12.4.0 | drain-steady | completed | 30.180 | 2026-11-08T06:22:42.096+00:00 | 2026-11-08T06:23:12.275+00:00 | — |
| 2 | Jellyfin 12.4.0 | loaded | completed | 120.281 | 2026-11-08T06:17:38.477+00:00 | 2026-11-08T06:19:38.757+00:00 | — |
| 2 | Jellyfin 12.4.0 | warmup-loaded | completed | 30.270 | 2026-11-08T06:17:07.297+00:00 | 2026-11-08T06:17:37.566+00:00 | — |
| 2 | Jellyfin 12.4.0 | drain-loaded | completed | 30.176 | 2026-11-08T06:16:36.755+00:00 | 2026-11-08T06:17:06.931+00:00 | — |
| 2 | Jellyfin 12.4.0 | stress | completed | 120.330 | 2026-11-08T06:20:41.144+00:00 | 2026-11-08T06:22:41.474+00:00 | — |
| 2 | Jellyfin 12.4.0 | warmup-stress | completed | 30.324 | 2026-11-08T06:20:09.915+00:00 | 2026-11-08T06:20:40.238+00:00 | — |
| 2 | Jellyfin 12.4.0 | drain-stress | completed | 30.174 | 2026-11-08T06:19:39.375+00:00 | 2026-11-08T06:20:09.549+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-shape-before | completed | 0.348 | 2026-11-08T06:11:50.589+00:00 | 2026-11-08T06:11:50.937+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-shape-after | completed | 0.299 | 2026-11-08T06:12:23.451+00:00 | 2026-11-08T06:12:23.751+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-coldstart-before | completed | 0.300 | 2026-11-08T06:12:23.784+00:00 | 2026-11-08T06:12:24.084+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-coldstart-after | completed | 0.663 | 2026-11-08T06:13:33.172+00:00 | 2026-11-08T06:13:33.835+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-warmup-unloaded-before | completed | 0.337 | 2026-11-08T06:14:04.220+00:00 | 2026-11-08T06:14:04.557+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-warmup-unloaded-after | completed | 0.302 | 2026-11-08T06:14:34.939+00:00 | 2026-11-08T06:14:35.241+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-unloaded-before | completed | 0.295 | 2026-11-08T06:14:35.274+00:00 | 2026-11-08T06:14:35.569+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-unloaded-after | completed | 0.299 | 2026-11-08T06:16:36.421+00:00 | 2026-11-08T06:16:36.720+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-warmup-loaded-before | completed | 0.297 | 2026-11-08T06:17:06.966+00:00 | 2026-11-08T06:17:07.263+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-warmup-loaded-after | completed | 0.301 | 2026-11-08T06:17:37.603+00:00 | 2026-11-08T06:17:37.904+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-loaded-before | completed | 0.295 | 2026-11-08T06:17:37.940+00:00 | 2026-11-08T06:17:38.235+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-loaded-after | completed | 0.311 | 2026-11-08T06:19:39.030+00:00 | 2026-11-08T06:19:39.341+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-warmup-stress-before | completed | 0.298 | 2026-11-08T06:20:09.584+00:00 | 2026-11-08T06:20:09.882+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-warmup-stress-after | completed | 0.301 | 2026-11-08T06:20:40.272+00:00 | 2026-11-08T06:20:40.573+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-stress-before | completed | 0.296 | 2026-11-08T06:20:40.609+00:00 | 2026-11-08T06:20:40.905+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-stress-after | completed | 0.311 | 2026-11-08T06:22:41.752+00:00 | 2026-11-08T06:22:42.063+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-ttfs-before | completed | 0.318 | 2026-11-08T06:24:12.830+00:00 | 2026-11-08T06:24:13.148+00:00 | — |
| 2 | Jellyfin 12.4.0 | isolation-ttfs-after | completed | 0.301 | 2026-11-08T06:25:01.410+00:00 | 2026-11-08T06:25:01.710+00:00 | — |
| 2 | Ferrofin | startup | completed | 17.068 | 2026-11-08T06:25:02.985+00:00 | 2026-11-08T06:25:20.053+00:00 | — |
| 2 | Ferrofin | startup-drain | completed | 30.172 | 2026-11-08T06:25:20.085+00:00 | 2026-11-08T06:25:50.258+00:00 | — |
| 2 | Ferrofin | cleanup | completed | 0.367 | 2026-11-08T06:38:26.698+00:00 | 2026-11-08T06:38:27.065+00:00 | — |
| 2 | Ferrofin | isolation-setup | completed | 0.321 | 2026-11-08T06:25:50.299+00:00 | 2026-11-08T06:25:50.620+00:00 | — |
| 2 | Ferrofin | counts | completed | 0.551 | 2026-11-08T06:25:50.653+00:00 | 2026-11-08T06:25:51.204+00:00 | — |
| 2 | Ferrofin | shape | completed | 15.497 | 2026-11-08T06:25:51.442+00:00 | 2026-11-08T06:26:06.939+00:00 | — |
| 2 | Ferrofin | coldstart | completed | 53.293 | 2026-11-08T06:26:07.393+00:00 | 2026-11-08T06:27:00.687+00:00 | — |
| 2 | Ferrofin | ttfs | completed | 48.025 | 2026-11-08T06:37:38.424+00:00 | 2026-11-08T06:38:26.449+00:00 | — |
| 2 | Ferrofin | unloaded | completed | 120.264 | 2026-11-08T06:28:02.451+00:00 | 2026-11-08T06:30:02.715+00:00 | — |
| 2 | Ferrofin | warmup-unloaded | completed | 30.316 | 2026-11-08T06:27:31.482+00:00 | 2026-11-08T06:28:01.798+00:00 | — |
| 2 | Ferrofin | drain-unloaded | completed | 30.176 | 2026-11-08T06:27:01.065+00:00 | 2026-11-08T06:27:31.241+00:00 | — |
| 2 | Ferrofin | sampler | completed | 637.249 | 2026-11-08T06:27:00.931+00:00 | 2026-11-08T06:37:38.179+00:00 | — |
| 2 | Ferrofin | steady | completed | 60.034 | 2026-11-08T06:36:37.692+00:00 | 2026-11-08T06:37:37.725+00:00 | — |
| 2 | Ferrofin | drain-steady | completed | 30.165 | 2026-11-08T06:36:07.492+00:00 | 2026-11-08T06:36:37.658+00:00 | — |
| 2 | Ferrofin | loaded | completed | 120.308 | 2026-11-08T06:31:04.556+00:00 | 2026-11-08T06:33:04.864+00:00 | — |
| 2 | Ferrofin | warmup-loaded | completed | 30.271 | 2026-11-08T06:30:33.621+00:00 | 2026-11-08T06:31:03.892+00:00 | — |
| 2 | Ferrofin | drain-loaded | completed | 30.173 | 2026-11-08T06:30:03.202+00:00 | 2026-11-08T06:30:33.376+00:00 | — |
| 2 | Ferrofin | stress | completed | 120.284 | 2026-11-08T06:34:06.718+00:00 | 2026-11-08T06:36:07.002+00:00 | — |
| 2 | Ferrofin | warmup-stress | completed | 30.292 | 2026-11-08T06:33:35.767+00:00 | 2026-11-08T06:34:06.058+00:00 | — |
| 2 | Ferrofin | drain-stress | completed | 30.168 | 2026-11-08T06:33:05.351+00:00 | 2026-11-08T06:33:35.519+00:00 | — |
| 2 | Ferrofin | isolation-shape-before | completed | 0.171 | 2026-11-08T06:25:51.237+00:00 | 2026-11-08T06:25:51.408+00:00 | — |
| 2 | Ferrofin | isolation-shape-after | completed | 0.177 | 2026-11-08T06:26:06.975+00:00 | 2026-11-08T06:26:07.152+00:00 | — |
| 2 | Ferrofin | isolation-coldstart-before | completed | 0.172 | 2026-11-08T06:26:07.188+00:00 | 2026-11-08T06:26:07.360+00:00 | — |
| 2 | Ferrofin | isolation-coldstart-after | completed | 0.176 | 2026-11-08T06:27:00.722+00:00 | 2026-11-08T06:27:00.898+00:00 | — |
| 2 | Ferrofin | isolation-warmup-unloaded-before | completed | 0.172 | 2026-11-08T06:27:31.276+00:00 | 2026-11-08T06:27:31.447+00:00 | — |
| 2 | Ferrofin | isolation-warmup-unloaded-after | completed | 0.175 | 2026-11-08T06:28:01.832+00:00 | 2026-11-08T06:28:02.007+00:00 | — |
| 2 | Ferrofin | isolation-unloaded-before | completed | 0.171 | 2026-11-08T06:28:02.040+00:00 | 2026-11-08T06:28:02.211+00:00 | — |
| 2 | Ferrofin | isolation-unloaded-after | completed | 0.178 | 2026-11-08T06:30:02.990+00:00 | 2026-11-08T06:30:03.168+00:00 | — |
| 2 | Ferrofin | isolation-warmup-loaded-before | completed | 0.177 | 2026-11-08T06:30:33.410+00:00 | 2026-11-08T06:30:33.588+00:00 | — |
| 2 | Ferrofin | isolation-warmup-loaded-after | completed | 0.177 | 2026-11-08T06:31:03.927+00:00 | 2026-11-08T06:31:04.104+00:00 | — |
| 2 | Ferrofin | isolation-loaded-before | completed | 0.177 | 2026-11-08T06:31:04.136+00:00 | 2026-11-08T06:31:04.313+00:00 | — |
| 2 | Ferrofin | isolation-loaded-after | completed | 0.177 | 2026-11-08T06:33:05.139+00:00 | 2026-11-08T06:33:05.315+00:00 | — |
| 2 | Ferrofin | isolation-warmup-stress-before | completed | 0.176 | 2026-11-08T06:33:35.555+00:00 | 2026-11-08T06:33:35.731+00:00 | — |
| 2 | Ferrofin | isolation-warmup-stress-after | completed | 0.180 | 2026-11-08T06:34:06.093+00:00 | 2026-11-08T06:34:06.273+00:00 | — |
| 2 | Ferrofin | isolation-stress-before | completed | 0.173 | 2026-11-08T06:34:06.306+00:00 | 2026-11-08T06:34:06.480+00:00 | — |
| 2 | Ferrofin | isolation-stress-after | completed | 0.179 | 2026-11-08T06:36:07.278+00:00 | 2026-11-08T06:36:07.458+00:00 | — |
| 2 | Ferrofin | isolation-ttfs-before | completed | 0.176 | 2026-11-08T06:37:38.215+00:00 | 2026-11-08T06:37:38.391+00:00 | — |
| 2 | Ferrofin | isolation-ttfs-after | completed | 0.183 | 2026-11-08T06:38:26.481+00:00 | 2026-11-08T06:38:26.664+00:00 | — |
| 3 | Jellyfin 12.4.0 | startup | completed | 8.104 | 2026-11-08T06:38:38.468+00:00 | 2026-11-08T06:38:46.573+00:00 | — |
| 3 | Jellyfin 12.4.0 | startup-drain | completed | 30.184 | 2026-11-08T06:38:46.605+00:00 | 2026-11-08T06:39:16.789+00:00 | — |
| 3 | Jellyfin 12.4.0 | cleanup | completed | 0.380 | 2026-11-08T06:52:31.322+00:00 | 2026-11-08T06:52:31.702+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-setup | completed | 1.504 | 2026-11-08T06:39:16.831+00:00 | 2026-11-08T06:39:18.336+00:00 | — |
| 3 | Jellyfin 12.4.0 | counts | completed | 2.157 | 2026-11-08T06:39:18.371+00:00 | 2026-11-08T06:39:20.527+00:00 | — |
| 3 | Jellyfin 12.4.0 | shape | completed | 32.310 | 2026-11-08T06:39:20.941+00:00 | 2026-11-08T06:39:53.251+00:00 | — |
| 3 | Jellyfin 12.4.0 | coldstart | completed | 68.776 | 2026-11-08T06:39:53.949+00:00 | 2026-11-08T06:41:02.725+00:00 | — |
| 3 | Jellyfin 12.4.0 | ttfs | completed | 48.213 | 2026-11-08T06:51:42.731+00:00 | 2026-11-08T06:52:30.944+00:00 | — |
| 3 | Jellyfin 12.4.0 | unloaded | completed | 120.268 | 2026-11-08T06:42:05.360+00:00 | 2026-11-08T06:44:05.628+00:00 | — |
| 3 | Jellyfin 12.4.0 | warmup-unloaded | completed | 30.279 | 2026-11-08T06:41:34.173+00:00 | 2026-11-08T06:42:04.452+00:00 | — |
| 3 | Jellyfin 12.4.0 | drain-unloaded | completed | 30.180 | 2026-11-08T06:41:03.589+00:00 | 2026-11-08T06:41:33.769+00:00 | — |
| 3 | Jellyfin 12.4.0 | sampler | completed | 638.886 | 2026-11-08T06:41:03.455+00:00 | 2026-11-08T06:51:42.341+00:00 | — |
| 3 | Jellyfin 12.4.0 | steady | completed | 60.034 | 2026-11-08T06:50:41.854+00:00 | 2026-11-08T06:51:41.888+00:00 | — |
| 3 | Jellyfin 12.4.0 | drain-steady | completed | 30.177 | 2026-11-08T06:50:11.641+00:00 | 2026-11-08T06:50:41.818+00:00 | — |
| 3 | Jellyfin 12.4.0 | loaded | completed | 120.315 | 2026-11-08T06:45:08.016+00:00 | 2026-11-08T06:47:08.331+00:00 | — |
| 3 | Jellyfin 12.4.0 | warmup-loaded | completed | 30.331 | 2026-11-08T06:44:36.779+00:00 | 2026-11-08T06:45:07.110+00:00 | — |
| 3 | Jellyfin 12.4.0 | drain-loaded | completed | 30.183 | 2026-11-08T06:44:06.232+00:00 | 2026-11-08T06:44:36.415+00:00 | — |
| 3 | Jellyfin 12.4.0 | stress | completed | 120.304 | 2026-11-08T06:48:10.728+00:00 | 2026-11-08T06:50:11.032+00:00 | — |
| 3 | Jellyfin 12.4.0 | warmup-stress | completed | 30.342 | 2026-11-08T06:47:39.478+00:00 | 2026-11-08T06:48:09.820+00:00 | — |
| 3 | Jellyfin 12.4.0 | drain-stress | completed | 30.178 | 2026-11-08T06:47:08.938+00:00 | 2026-11-08T06:47:39.116+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-shape-before | completed | 0.348 | 2026-11-08T06:39:20.560+00:00 | 2026-11-08T06:39:20.908+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-shape-after | completed | 0.298 | 2026-11-08T06:39:53.285+00:00 | 2026-11-08T06:39:53.583+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-coldstart-before | completed | 0.300 | 2026-11-08T06:39:53.616+00:00 | 2026-11-08T06:39:53.916+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-coldstart-after | completed | 0.663 | 2026-11-08T06:41:02.759+00:00 | 2026-11-08T06:41:03.421+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-warmup-unloaded-before | completed | 0.334 | 2026-11-08T06:41:33.803+00:00 | 2026-11-08T06:41:34.137+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-warmup-unloaded-after | completed | 0.298 | 2026-11-08T06:42:04.485+00:00 | 2026-11-08T06:42:04.783+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-unloaded-before | completed | 0.301 | 2026-11-08T06:42:04.817+00:00 | 2026-11-08T06:42:05.119+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-unloaded-after | completed | 0.297 | 2026-11-08T06:44:05.902+00:00 | 2026-11-08T06:44:06.199+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-warmup-loaded-before | completed | 0.296 | 2026-11-08T06:44:36.449+00:00 | 2026-11-08T06:44:36.745+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-warmup-loaded-after | completed | 0.301 | 2026-11-08T06:45:07.144+00:00 | 2026-11-08T06:45:07.445+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-loaded-before | completed | 0.295 | 2026-11-08T06:45:07.479+00:00 | 2026-11-08T06:45:07.774+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-loaded-after | completed | 0.298 | 2026-11-08T06:47:08.605+00:00 | 2026-11-08T06:47:08.903+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-warmup-stress-before | completed | 0.293 | 2026-11-08T06:47:39.150+00:00 | 2026-11-08T06:47:39.443+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-warmup-stress-after | completed | 0.299 | 2026-11-08T06:48:09.855+00:00 | 2026-11-08T06:48:10.154+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-stress-before | completed | 0.297 | 2026-11-08T06:48:10.189+00:00 | 2026-11-08T06:48:10.485+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-stress-after | completed | 0.300 | 2026-11-08T06:50:11.305+00:00 | 2026-11-08T06:50:11.606+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-ttfs-before | completed | 0.321 | 2026-11-08T06:51:42.378+00:00 | 2026-11-08T06:51:42.699+00:00 | — |
| 3 | Jellyfin 12.4.0 | isolation-ttfs-after | completed | 0.308 | 2026-11-08T06:52:30.979+00:00 | 2026-11-08T06:52:31.287+00:00 | — |
| 3 | Ferrofin | startup | completed | 19.564 | 2026-11-08T06:52:32.548+00:00 | 2026-11-08T06:52:52.112+00:00 | — |
| 3 | Ferrofin | startup-drain | completed | 30.166 | 2026-11-08T06:52:52.143+00:00 | 2026-11-08T06:53:22.310+00:00 | — |
| 3 | Ferrofin | cleanup | completed | 0.394 | 2026-11-08T07:05:58.893+00:00 | 2026-11-08T07:05:59.287+00:00 | — |
| 3 | Ferrofin | isolation-setup | completed | 0.324 | 2026-11-08T06:53:22.353+00:00 | 2026-11-08T06:53:22.677+00:00 | — |
| 3 | Ferrofin | counts | completed | 0.550 | 2026-11-08T06:53:22.712+00:00 | 2026-11-08T06:53:23.262+00:00 | — |
| 3 | Ferrofin | shape | completed | 15.725 | 2026-11-08T06:53:23.498+00:00 | 2026-11-08T06:53:39.224+00:00 | — |
| 3 | Ferrofin | coldstart | completed | 53.192 | 2026-11-08T06:53:39.678+00:00 | 2026-11-08T06:54:32.870+00:00 | — |
| 3 | Ferrofin | ttfs | completed | 48.014 | 2026-11-08T07:05:10.633+00:00 | 2026-11-08T07:05:58.647+00:00 | — |
| 3 | Ferrofin | unloaded | completed | 120.296 | 2026-11-08T06:55:34.611+00:00 | 2026-11-08T06:57:34.907+00:00 | — |
| 3 | Ferrofin | warmup-unloaded | completed | 30.295 | 2026-11-08T06:55:03.653+00:00 | 2026-11-08T06:55:33.948+00:00 | — |
| 3 | Ferrofin | drain-unloaded | completed | 30.166 | 2026-11-08T06:54:33.245+00:00 | 2026-11-08T06:55:03.412+00:00 | — |
| 3 | Ferrofin | sampler | completed | 637.277 | 2026-11-08T06:54:33.114+00:00 | 2026-11-08T07:05:10.391+00:00 | — |
| 3 | Ferrofin | steady | completed | 60.035 | 2026-11-08T07:04:09.909+00:00 | 2026-11-08T07:05:09.944+00:00 | — |
| 3 | Ferrofin | drain-steady | completed | 30.168 | 2026-11-08T07:03:39.706+00:00 | 2026-11-08T07:04:09.874+00:00 | — |
| 3 | Ferrofin | loaded | completed | 120.296 | 2026-11-08T06:58:36.764+00:00 | 2026-11-08T07:00:37.060+00:00 | — |
| 3 | Ferrofin | warmup-loaded | completed | 30.281 | 2026-11-08T06:58:05.817+00:00 | 2026-11-08T06:58:36.098+00:00 | — |
| 3 | Ferrofin | drain-loaded | completed | 30.172 | 2026-11-08T06:57:35.400+00:00 | 2026-11-08T06:58:05.573+00:00 | — |
| 3 | Ferrofin | stress | completed | 120.295 | 2026-11-08T07:01:38.921+00:00 | 2026-11-08T07:03:39.216+00:00 | — |
| 3 | Ferrofin | warmup-stress | completed | 30.288 | 2026-11-08T07:01:07.964+00:00 | 2026-11-08T07:01:38.252+00:00 | — |
| 3 | Ferrofin | drain-stress | completed | 30.171 | 2026-11-08T07:00:37.546+00:00 | 2026-11-08T07:01:07.717+00:00 | — |
| 3 | Ferrofin | isolation-shape-before | completed | 0.169 | 2026-11-08T06:53:23.295+00:00 | 2026-11-08T06:53:23.464+00:00 | — |
| 3 | Ferrofin | isolation-shape-after | completed | 0.178 | 2026-11-08T06:53:39.258+00:00 | 2026-11-08T06:53:39.436+00:00 | — |
| 3 | Ferrofin | isolation-coldstart-before | completed | 0.173 | 2026-11-08T06:53:39.471+00:00 | 2026-11-08T06:53:39.645+00:00 | — |
| 3 | Ferrofin | isolation-coldstart-after | completed | 0.174 | 2026-11-08T06:54:32.905+00:00 | 2026-11-08T06:54:33.079+00:00 | — |
| 3 | Ferrofin | isolation-warmup-unloaded-before | completed | 0.172 | 2026-11-08T06:55:03.448+00:00 | 2026-11-08T06:55:03.621+00:00 | — |
| 3 | Ferrofin | isolation-warmup-unloaded-after | completed | 0.179 | 2026-11-08T06:55:33.983+00:00 | 2026-11-08T06:55:34.162+00:00 | — |
| 3 | Ferrofin | isolation-unloaded-before | completed | 0.173 | 2026-11-08T06:55:34.196+00:00 | 2026-11-08T06:55:34.369+00:00 | — |
| 3 | Ferrofin | isolation-unloaded-after | completed | 0.183 | 2026-11-08T06:57:35.181+00:00 | 2026-11-08T06:57:35.365+00:00 | — |
| 3 | Ferrofin | isolation-warmup-loaded-before | completed | 0.175 | 2026-11-08T06:58:05.608+00:00 | 2026-11-08T06:58:05.783+00:00 | — |
| 3 | Ferrofin | isolation-warmup-loaded-after | completed | 0.180 | 2026-11-08T06:58:36.132+00:00 | 2026-11-08T06:58:36.312+00:00 | — |
| 3 | Ferrofin | isolation-loaded-before | completed | 0.177 | 2026-11-08T06:58:36.345+00:00 | 2026-11-08T06:58:36.522+00:00 | — |
| 3 | Ferrofin | isolation-loaded-after | completed | 0.180 | 2026-11-08T07:00:37.334+00:00 | 2026-11-08T07:00:37.514+00:00 | — |
| 3 | Ferrofin | isolation-warmup-stress-before | completed | 0.177 | 2026-11-08T07:01:07.753+00:00 | 2026-11-08T07:01:07.930+00:00 | — |
| 3 | Ferrofin | isolation-warmup-stress-after | completed | 0.186 | 2026-11-08T07:01:38.287+00:00 | 2026-11-08T07:01:38.473+00:00 | — |
| 3 | Ferrofin | isolation-stress-before | completed | 0.175 | 2026-11-08T07:01:38.507+00:00 | 2026-11-08T07:01:38.682+00:00 | — |
| 3 | Ferrofin | isolation-stress-after | completed | 0.183 | 2026-11-08T07:03:39.489+00:00 | 2026-11-08T07:03:39.672+00:00 | — |
| 3 | Ferrofin | isolation-ttfs-before | completed | 0.175 | 2026-11-08T07:05:10.423+00:00 | 2026-11-08T07:05:10.597+00:00 | — |
| 3 | Ferrofin | isolation-ttfs-after | completed | 0.178 | 2026-11-08T07:05:58.682+00:00 | 2026-11-08T07:05:58.860+00:00 | — |

### Time to first screen

| | Jellyfin 12.4.0 | Ferrofin |
|---|---|---|
| cold start (restart → authenticated views; host caches retained) | 3257 ms (3249 ms–3260 ms) | 250 ms (242 ms–254 ms) — 13.0× faster |
| HLS first segment (forced transcode) | 329 ms (328 ms–331 ms) | 168 ms (165 ms–169 ms) — 2.0× faster |
| direct-play TTFB (1 MiB range) | 3.91 ms (3.77 ms–4.08 ms) | 0.71 ms (0.69 ms–0.73 ms) — 5.5× faster |

### Memory (anon, cache excluded, 100 ms samples)

| | Jellyfin 12.4.0 | Ferrofin |
|---|---|---|
| peak under load | 598 MiB (576 MiB–609 MiB) | 245 MiB (238 MiB–254 MiB) — 2.4× lighter |
| steady idle | 404 MiB (383 MiB–415 MiB) | 197 MiB (194 MiB–200 MiB) — 2.1× lighter |
| interference on the server's cores, p95 | 6% (6%–9%) | 6% |
| interference, max single sample | 28% (27%–29%) | 28% (26%–32%) |
| max swap | 0.00 MiB | 0.00 MiB |

### Response shape vs Jellyfin 12.4.0 (supporting evidence, not the parity number)

**ferrofin**: 3 divergence(s) across 33 compared requests
- image: request picks differ
- search:image: request picks differ
- search:items: ids differs for a request pick


### Notes

1. search:image: request picks differ
   — Ferrofin · search (unloaded screens); Ferrofin · search:image (unloaded endpoints); Ferrofin · search (loaded screens); Ferrofin · search:image (loaded endpoints); Ferrofin · search (stress screens); Ferrofin · search:image (stress endpoints)
2. search:items: ids differs for a request pick
   — Ferrofin · search:items (unloaded endpoints); Ferrofin · search:items (loaded endpoints); Ferrofin · search:items (stress endpoints)
3. image: request picks differ
   — Ferrofin · image (unloaded endpoints); Ferrofin · image (loaded endpoints); Ferrofin · image (stress endpoints)

---

## Reproduce the reports

Raw evidence stays outside the repository, under the gates' output root (`$FERROFIN_GATES_DIR/runs/`).
The README's numbers come from the same three runs, through `docs/results/v1.5.0/summary.json`.
Role credentials must not be published.

```sh
python3 gates/bench/report.py \
  "$FERROFIN_GATES_DIR"/runs/detached-961c0b412329-20261107-2316/bench \
  "$FERROFIN_GATES_DIR"/runs/detached-961c0b412329-20261108-0011/bench \
  "$FERROFIN_GATES_DIR"/runs/detached-961c0b412329-20261108-0038/bench
```
