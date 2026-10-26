# Benchmarks — three-run results

Measured on **2026-10-26** from server source **`5112240e`** (`release/v1.4.0` with the gates' publishing
changes; the server code is `release/v1.4.0` at `4bedc9df`), against **Jellyfin 12.2.0** (pinned by digest in
`gates/bench/testdata/jellyfin-image.txt`), using three complete repetitions: the benchmark of one full gates
session and two benchmark sessions, all of one image. The root [README](../README.md#results) graphs every
comparable cell and lists the withheld ones; this detailed report keeps flagged raw measurements as
**diagnostics, not performance claims**. The [methodology](../gates/bench/README.md) defines the measurements
and checks.

Every run passed the start-of-run idle check (95.8 %, 92.2 % and 92.3 %). During measurement the server cores
saw 16–20 % p95 interference; the ranges below include it.

Everything between the two rules is the unedited output of `gates/bench/report.py` for the three runs.

---

## Ferrofin vs Jellyfin — 3 run(s), commit 5112240e, 2026-10-26
Host AMD Ryzen 9 9950X3D 16-Core Processor · server on cpus 8-15 · 8g limit · test data 3001 movies, 250 series, 7490 episodes, 50 hdr · windows 120 s · unloaded 1 screens/s · loaded 5 screens/s · stress 25 screens/s
Cells are the median across runs; where the runs disagreed, the range they spanned follows in brackets. `⚠[n]` means the server did different work than Jellyfin 12.2.0 (status / record count / missing fields), so the number is not comparable: it is kept for the work list, not for publication, and note `n` says why. `X.Y× faster` compares the cell with Jellyfin 12.2.0 on the same row. It is shown on flagged cells too — the note says the two servers did not do identical work, so read it as an indication rather than a like-for-like result.

### Latency — unloaded (1 screens/s)

| screen | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home | 46 / 51 (51–52) / 57 (52–58) (0.00%) | 16 (15–16) / 22 (22–25) / 26 (25–31) (0.00%) — 2.9× faster |
| movies | 39 (38–39) / 44 (43–44) / 50 (48–51) (0.00%) | 16 (16–17) / 19 (17–20) / 19 (19–21) (0.00%) — 2.4× faster |
| detail | 34 / 37 / 39 (37–40) (0.00%) | 8 / 10 / 10 (0.00%) — 4.2× faster |
| series | 24 / 27 (27–30) / 28 (28–33) (0.00%) | 7 (7–8) / 8 (8–9) / 8 (8–10) (0.00%) — 3.4× faster |
| search | 72 (71–78) / 270 (198–333) / 436 (303–569) (0.00%) | 29 (28–29) / 33 (30–33) / 33 (30–35) (0.00%) — 2.5× faster ⚠[1] |
| playback | 31 (30–31) / 35 (35–37) / 36 (35–39) (0.00%) | 9 (8–9) / 10 (9–3280) / 11 (9–6478) (0.00%) — 3.4× faster |

| endpoint | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home:image | 4.80 (4.65–4.84) / 6.70 (5.99–6.75) / 8.31 (6.73–12.5) (0.00%) | 0.78 (0.67–0.79) / 1.10 (1.04–1.20) / 2.06 (1.64–2.27) (0.00%) — 6.2× faster |
| home:latest-movies | 11.5 (11.4–11.5) / 14.9 (14.8–15.4) / 19.3 (17.2–20.6) (0.00%) | 5.11 (4.71–5.21) / 6.73 (6.58–6.92) / 7.64 (7.45–14.0) (0.00%) — 2.3× faster |
| home:latest-music | 7.11 (7.08–7.14) / 10.3 (9.96–10.7) / 13.2 (10.8–13.5) (0.00%) | 3.05 (2.86–3.08) / 4.56 (3.80–5.09) / 5.24 (4.73–5.50) (0.00%) — 2.3× faster |
| home:latest-shows | 21.8 (21.6–21.9) / 26.8 (24.7–26.8) / 28.4 (26.7–28.6) (0.00%) | 10.4 (9.79–10.4) / 12.0 (11.8–12.4) / 12.9 (12.1–13.1) (0.00%) — 2.1× faster |
| home:nextup | 25.6 (25.5–26.0) / 29.3 (28.6–29.7) / 33.4 (30.8–33.6) (0.00%) | 11.3 (10.8–11.5) / 12.6 (12.6–13.1) / 13.3 (12.9–13.8) (0.00%) — 2.3× faster |
| home:resume-audio | 19.4 (19.3–19.7) / 22.7 (22.0–23.8) / 25.3 (22.9–26.3) (0.00%) | 6.05 (5.84–6.32) / 8.34 (7.36–8.49) / 9.32 (8.60–17.0) (0.00%) — 3.2× faster |
| home:resume-book | 19.4 (19.0–19.9) / 23.2 (22.5–24.6) / 25.3 (23.6–27.2) (0.00%) | 1.80 (1.65–1.87) / 4.26 (2.01–4.58) / 4.64 (3.21–11.1) (0.00%) — 10.8× faster |
| home:resume-video | 21.0 (21.0–21.2) / 25.0 (24.1–25.3) / 26.7 (25.9–27.1) (0.00%) | 7.42 (6.92–7.72) / 9.55 (9.37–9.56) / 10.8 (9.78–17.1) (0.00%) — 2.8× faster |
| home:views | 20.3 (20.2–20.4) / 23.9 (22.6–25.4) / 26.3 (23.9–28.7) (0.00%) | 4.14 (3.61–4.83) / 7.55 (5.72–7.93) / 12.0 (6.93–14.0) (0.00%) — 4.9× faster |
| movies:image | 5.62 (5.51–5.67) / 8.10 (8.02–8.14) / 8.93 (8.81–12.2) (0.00%) | 0.80 (0.77–0.85) / 1.13 (1.06–1.13) / 1.36 (1.33–2.69) (0.00%) — 7.0× faster |
| movies:items | 25.2 (25.1–25.3) / 29.0 (28.2–29.4) / 35.9 (35.8–36.3) (0.00%) | 13.4 (12.4–13.4) / 15.2 (13.8–16.0) / 15.5 (14.0–17.2) (0.00%) — 1.9× faster |
| detail:image | 4.83 (4.72–4.83) / 7.71 (7.68–7.81) / 9.20 (8.04–13.5) (0.00%) | 0.69 (0.65–0.69) / 1.11 (0.96–1.11) / 2.07 (1.43–2.49) (0.00%) — 7.0× faster |
| detail:item | 12.0 (11.6–12.2) / 13.5 (13.4–14.5) / 14.4 (13.6–14.8) (0.00%) | 2.51 (2.46–2.55) / 2.79 (2.66–2.90) / 2.92 (2.70–2.96) (0.00%) — 4.8× faster |
| detail:local-trailers | 8.01 (7.91–8.06) / 9.00 (8.77–10.8) / 10.2 (10.0–10.9) (0.00%) | 1.35 (1.32–1.40) / 1.57 (1.49–1.62) / 1.61 (1.59–1.78) (0.00%) — 5.9× faster |
| detail:similar | 21.8 (21.6–22.0) / 23.5 (23.3–24.3) / 23.9 (23.7–24.7) (0.00%) | 5.34 (5.12–5.41) / 6.09 (5.72–6.36) / 6.20 (6.19–6.54) (0.00%) — 4.1× faster |
| detail:special-features | 8.02 (7.93–8.08) / 9.05 (8.79–10.8) / 10.2 (10.0–10.9) (0.00%) | 1.38 (1.37–1.42) / 1.59 (1.51–1.67) / 1.64 (1.64–1.69) (0.00%) — 5.8× faster |
| series:episodes | 5.84 (5.68–5.92) / 6.36 (6.36–6.52) / 6.42 (6.39–6.76) (0.00%) | 1.60 (1.47–1.64) / 1.81 (1.66–1.87) / 1.84 (1.68–1.94) (0.00%) — 3.6× faster |
| series:image | 3.48 (3.46–3.52) / 4.64 (3.74–5.81) / 4.76 (3.79–5.82) (0.00%) | 0.62 (0.55–0.64) / 0.91 (0.59–2.26) / 0.93 (0.64–2.66) (0.00%) — 5.6× faster |
| series:item | 10.7 (10.6–10.9) / 13.8 (11.7–15.6) / 14.0 (12.3–19.2) (0.00%) | 3.84 (3.80–3.85) / 4.19 (4.01–4.31) / 4.34 (4.02–4.55) (0.00%) — 2.8× faster |
| series:seasons | 7.98 (7.93–8.07) / 9.31 (9.22–12.6) / 10.5 (9.44–15.9) (0.00%) | 2.12 (2.02–2.14) / 2.37 (2.30–2.55) / 2.43 (2.37–2.72) (0.00%) — 3.8× faster |
| series:similar | 14.0 (13.8–14.0) / 17.1 (14.8–18.6) / 17.7 (15.4–22.1) (0.00%) | 4.23 (4.09–4.25) / 4.49 (4.45–4.59) / 4.58 (4.58–4.61) (0.00%) — 3.3× faster |
| search:artists | 11.5 (11.3–11.7) / 158 (25.8–229) / 280 (36.8–415) (0.00%) | 1.98 (1.85–2.09) / 2.66 (2.46–3.07) / 2.99 (2.49–3.09) (0.00%) — 5.8× faster ⚠[1] |
| search:image | 5.64 (5.36–5.73) / 8.53 (8.51–8.86) / 10.2 (9.12–14.4) (0.00%) | 0.84 (0.83–0.95) / 1.22 (1.14–1.38) / 1.78 (1.25–2.06) (0.00%) — 6.7× faster ⚠[2] |
| search:items | 55.7 (55.0–57.6) / 130 (128–203) / 183 (182–319) (0.00%) | 23.4 (22.8–23.7) / 26.3 (25.1–27.6) / 26.8 (25.4–28.4) (0.00%) — 2.4× faster ⚠[3] |
| search:persons | 17.6 (17.4–17.6) / 156 (88.7–221) / 284 (150–418) (0.00%) | 2.29 (2.24–2.30) / 2.44 (2.41–3.52) / 2.51 (2.42–4.00) (0.00%) — 7.7× faster |
| search:programs | 15.2 (15.0–15.3) / 23.1 (20.0–287) / 24.3 (22.3–549) (0.00%) | 1.54 (1.48–1.65) / 2.53 (2.00–2.56) / 2.68 (2.09–2.71) (0.00%) — 9.9× faster |
| search:videos | 33.2 (33.2–34.4) / 38.5 (37.9–170) / 41.4 (40.2–301) (0.00%) | 6.06 (5.92–6.24) / 6.69 (6.35–6.94) / 6.74 (6.53–7.33) (0.00%) — 5.5× faster |
| playback:intros | 4.94 (4.90–5.09) / 6.36 (5.22–6.45) / 6.66 (5.29–7.59) (0.00%) | 4.10 (3.92–4.20) / 4.29 (4.20–4.36) / 4.32 (4.22–4.36) (0.00%) — 1.2× faster |
| playback:playbackinfo | 8.69 (8.63–8.69) / 10.6 (10.4–12.7) / 12.0 (11.5–13.7) (0.00%) | 2.04 (2.04–2.05) / 2.29 (2.21–2.35) / 2.44 (2.23–2.44) (0.00%) — 4.3× faster |
| playback:playing | 9.23 (9.02–9.27) / 12.5 (10.8–15.3) / 12.6 (12.0–18.3) (0.00%) | 0.91 (0.88–0.92) / 1.85 (1.12–3272) / 2.67 (1.14–6470) (0.00%) — 10.1× faster |
| playback:segments | 4.90 (4.86–5.07) / 6.29 (5.21–6.50) / 6.62 (5.30–7.54) (0.00%) | 4.11 (4.11–4.25) / 4.30 (4.28–4.39) / 4.33 (4.30–4.42) (0.00%) — 1.2× faster |
| playback:stopped | 6.82 (6.81–6.94) / 7.27 (7.05–9.24) / 7.27 (7.13–9.35) (0.00%) | 0.79 (0.79–0.84) / 0.92 (0.91–2.73) / 0.94 (0.92–4.55) (0.00%) — 8.6× faster |
| image | 4.88 (4.84–4.93) / 7.71 (7.64–7.92) / 9.28 (9.16–9.92) (0.00%) | 0.74 (0.67–0.75) / 1.15 (1.09–1.15) / 1.90 (1.66–2.33) (0.00%) — 6.6× faster ⚠[4] |

### Image response bytes — unloaded (diagnostic)

| Run | Server | Bytes |
|---|---|---|
| 1 | Jellyfin 12.2.0 | 8218263 |
| 1 | Ferrofin | 7772572 |
| 2 | Jellyfin 12.2.0 | 8218263 |
| 2 | Ferrofin | 7772572 |
| 3 | Jellyfin 12.2.0 | 8218263 |
| 3 | Ferrofin | 7772572 |

### Latency — loaded (5 screens/s)

| screen | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home | 45 (45–46) / 49 (48–51) / 52 (51–53) (0.00%) | 15 / 18 (18–19) / 21 (19–22) (0.00%) — 3.0× faster |
| movies | 36 / 41 (40–42) / 43 (43–44) (0.00%) | 16 / 18 (17–19) / 19 (19–21) (0.00%) — 2.2× faster |
| detail | 33 / 36 (35–37) / 38 (36–40) (0.00%) | 8 / 10 (9–10) / 12 (11–13) (0.00%) — 4.1× faster |
| series | 25 / 28 (27–29) / 30 (30–33) (0.00%) | 7 (7–8) / 8 (8–9) / 8 (8–9) (0.00%) — 3.6× faster |
| search | 56 (55–56) / 63 (62–65) / 77 (0.00%) | 28 (28–30) / 34 (32–36) / 43 (41–43) (0.00%) — 2.0× faster ⚠[1] |
| playback | 29 / 31 (31–32) / 49 (48–49) (0.00%) | 8 (8–9) / 9 / 33 (23–33) (0.00%) — 3.6× faster |

| endpoint | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home:image | 4.75 (4.74–4.79) / 6.62 (6.25–6.67) / 7.16 (6.88–7.31) (0.00%) | 0.70 (0.68–0.71) / 1.03 (0.98–1.03) / 1.35 (1.35–1.43) (0.00%) — 6.8× faster |
| home:latest-movies | 11.4 (11.3–11.5) / 13.9 (13.8–14.0) / 15.3 (14.7–15.6) (0.00%) | 5.10 (5.03–5.18) / 7.09 (6.92–7.26) / 7.97 (7.58–8.18) (0.00%) — 2.2× faster |
| home:latest-music | 7.18 (7.07–7.26) / 10.1 (9.58–10.3) / 11.1 (10.4–12.5) (0.00%) | 2.95 (2.95–3.03) / 4.41 (4.06–5.05) / 5.70 (5.68–6.04) (0.00%) — 2.4× faster |
| home:latest-shows | 22.1 (21.9–22.2) / 25.6 (24.4–26.1) / 27.4 (27.0–28.9) (0.00%) | 10.0 (9.93–10.1) / 12.0 (11.9–12.1) / 13.3 (12.9–14.1) (0.00%) — 2.2× faster |
| home:nextup | 25.5 (25.4–25.6) / 28.1 (28.0–28.6) / 29.4 (29.3–31.4) (0.00%) | 11.1 / 12.9 (12.7–12.9) / 14.1 (14.0–14.6) (0.00%) — 2.3× faster |
| home:resume-audio | 19.3 (19.1–19.5) / 22.8 (22.1–23.1) / 24.0 (22.7–24.6) (0.00%) | 6.06 (6.05–6.11) / 7.51 (6.69–7.68) / 8.65 (8.23–9.03) (0.00%) — 3.2× faster |
| home:resume-book | 19.3 (19.2–19.4) / 22.2 (21.9–22.7) / 23.5 (22.4–23.8) (0.00%) | 1.72 (1.71–1.77) / 3.44 (2.88–4.02) / 4.51 (4.23–4.75) (0.00%) — 11.2× faster |
| home:resume-video | 21.3 (21.3–21.5) / 24.4 (23.9–24.6) / 25.2 (24.4–25.6) (0.00%) | 7.40 (7.39–7.46) / 9.67 (8.63–9.70) / 10.7 (9.88–11.1) (0.00%) — 2.9× faster |
| home:views | 20.4 (20.2–20.5) / 23.2 (22.8–23.4) / 24.5 (24.1–25.1) (0.00%) | 4.16 (4.11–4.56) / 7.64 (7.34–8.52) / 8.64 (8.54–9.63) (0.00%) — 4.9× faster |
| movies:image | 4.87 (4.78–4.89) / 6.70 (6.41–7.06) / 7.48 (7.03–7.57) (0.00%) | 0.75 (0.74–0.77) / 1.06 (1.00–1.07) / 1.61 (1.35–1.65) (0.00%) — 6.5× faster |
| movies:items | 24.8 (24.6–24.9) / 28.3 (28.0–28.4) / 29.2 (29.1–31.0) (0.00%) | 12.9 (12.5–12.9) / 15.3 (14.2–15.6) / 15.8 (14.5–16.6) (0.00%) — 1.9× faster |
| detail:image | 4.66 (4.63–4.70) / 6.60 (6.54–6.77) / 7.54 (7.40–7.79) (0.00%) | 0.66 (0.65–0.66) / 1.00 (0.98–1.03) / 1.30 (1.30–2.56) (0.00%) — 7.1× faster |
| detail:item | 11.4 (11.0–11.6) / 13.4 (13.1–13.5) / 14.0 (13.8–14.1) (0.00%) | 2.47 (2.47–2.55) / 2.87 (2.86–3.00) / 3.99 (3.42–4.71) (0.00%) — 4.6× faster |
| detail:local-trailers | 7.55 (7.15–7.63) / 9.29 (9.11–9.46) / 9.79 (9.59–9.96) (0.00%) | 1.34 (1.32–1.40) / 1.65 (1.63–1.67) / 1.78 (1.75–2.52) (0.00%) — 5.6× faster |
| detail:similar | 21.6 (21.3–21.9) / 23.6 (23.5–24.2) / 25.4 (24.7–27.5) (0.00%) | 5.37 (5.34–5.59) / 6.54 (6.02–6.71) / 7.18 (7.03–7.75) (0.00%) — 4.0× faster |
| detail:special-features | 7.58 (7.18–7.73) / 9.35 (9.26–9.55) / 10.1 (9.83–10.5) (0.00%) | 1.34 (1.33–1.41) / 1.69 (1.57–1.70) / 1.90 (1.79–2.51) (0.00%) — 5.7× faster |
| series:episodes | 6.16 (6.16–6.17) / 7.48 (7.12–7.89) / 8.32 (7.81–8.43) (0.00%) | 1.65 (1.50–1.68) / 2.03 (1.83–2.03) / 2.15 (2.11–2.20) (0.00%) — 3.7× faster |
| series:image | 3.72 (3.67–3.77) / 5.29 (4.12–5.52) / 5.77 (5.24–5.82) (0.00%) | 0.63 (0.55–0.63) / 0.81 (0.80–0.81) / 0.97 (0.91–1.08) (0.00%) — 5.9× faster |
| series:item | 10.9 (10.8–11.0) / 13.0 (11.8–13.2) / 16.1 (13.2–16.3) (0.00%) | 3.93 (3.70–4.20) / 4.39 (4.12–4.91) / 4.62 (4.26–5.11) (0.00%) — 2.8× faster |
| series:seasons | 7.95 (7.94–7.98) / 9.64 (8.88–9.80) / 10.9 (10.5–12.0) (0.00%) | 2.10 (2.06–2.27) / 2.39 (2.21–2.65) / 2.57 (2.40–2.90) (0.00%) — 3.8× faster |
| series:similar | 14.0 (14.0–14.2) / 15.7 (15.7–15.8) / 19.6 (18.8–21.0) (0.00%) | 4.24 (4.05–4.44) / 4.78 (4.36–5.09) / 4.99 (4.58–5.29) (0.00%) — 3.3× faster |
| search:artists | 10.8 (10.8–10.9) / 13.0 (12.6–13.0) / 13.2 (13.0–13.4) (0.00%) | 1.92 (1.90–2.02) / 3.10 (2.61–3.52) / 3.78 (3.08–4.39) (0.00%) — 5.6× faster ⚠[1] |
| search:image | 4.89 (4.87–4.95) / 6.60 (6.46–6.80) / 7.13 (6.78–7.80) (0.00%) | 0.79 (0.79–0.87) / 1.24 (1.17–1.25) / 1.57 (1.49–1.93) (0.00%) — 6.2× faster ⚠[2] |
| search:items | 42.5 (42.4–42.5) / 49.6 (48.1–49.8) / 61.7 (61.0–62.4) (0.00%) | 23.2 (23.0–24.5) / 28.6 (27.8–29.3) / 35.3 (34.7–36.1) (0.00%) — 1.8× faster ⚠[3] |
| search:persons | 17.1 (17.1–17.3) / 19.3 (19.2–19.4) / 20.1 (19.7–22.1) (0.00%) | 2.28 (2.27–2.38) / 2.87 (2.62–3.77) / 3.22 (3.07–4.28) (0.00%) — 7.5× faster |
| search:programs | 14.6 (14.6–14.7) / 16.8 (16.7–16.9) / 17.1 (17.1–17.6) (0.00%) | 1.51 (1.46–1.63) / 2.49 (2.43–2.65) / 2.91 (2.80–3.68) (0.00%) — 9.7× faster |
| search:videos | 32.7 (32.4–32.9) / 35.1 (34.5–35.3) / 36.2 (35.3–37.9) (0.00%) | 5.98 (5.92–6.12) / 6.63 (6.43–6.71) / 7.00 (6.98–7.38) (0.00%) — 5.5× faster |
| playback:intros | 4.86 (4.85–4.87) / 5.38 (5.24–6.34) / 7.30 (6.42–7.40) (0.00%) | 4.06 (4.02–4.14) / 4.27 (4.20–4.35) / 4.42 (4.32–4.42) (0.00%) — 1.2× faster |
| playback:playbackinfo | 7.82 (7.74–7.84) / 8.61 (8.41–9.37) / 9.15 (9.00–10.7) (0.00%) | 1.99 (1.95–2.06) / 2.23 (2.20–2.24) / 3.83 (3.72–4.06) (0.00%) — 3.9× faster |
| playback:playing | 8.55 (8.50–8.63) / 10.3 (10.1–10.3) / 18.5 (16.7–27.8) (0.00%) | 0.83 (0.81–0.83) / 1.01 (1.01–1.03) / 25.1 (14.7–25.2) (0.00%) — 10.3× faster |
| playback:segments | 4.90 (4.88–4.94) / 5.63 (5.33–6.29) / 6.68 (6.62–7.22) (0.00%) | 4.12 (4.08–4.18) / 4.35 (4.30–4.43) / 4.46 (4.39–4.49) (0.00%) — 1.2× faster |
| playback:stopped | 6.58 (6.57–6.62) / 8.06 (7.30–8.52) / 16.2 (12.9–16.6) (0.00%) | 0.75 (0.74–0.77) / 0.88 (0.87–0.89) / 1.01 (1.00–1.02) (0.00%) — 8.8× faster |
| image | 4.76 (4.73–4.78) / 6.64 (6.40–6.75) / 7.52 (7.10–7.57) (0.00%) | 0.69 / 1.08 (1.06–1.08) / 1.46 (1.38–1.66) (0.00%) — 6.9× faster ⚠[4] |

### Image response bytes — loaded (diagnostic)

| Run | Server | Bytes |
|---|---|---|
| 1 | Jellyfin 12.2.0 | 40942827 |
| 1 | Ferrofin | 38328356 |
| 2 | Jellyfin 12.2.0 | 40942827 |
| 2 | Ferrofin | 38328356 |
| 3 | Jellyfin 12.2.0 | 40942827 |
| 3 | Ferrofin | 38328356 |

### Latency — stress (25 screens/s)

| screen | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home | 45 / 51 (51–52) / 55 (54–57) (0.00%) | 15 / 19 (18–20) / 21 (20–23) (0.00%) — 3.0× faster |
| movies | 37 / 43 / 47 (47–51) (0.00%) | 15 / 18 (18–19) / 20 (19–22) (0.00%) — 2.5× faster |
| detail | 32 (32–33) / 37 (36–37) / 40 (40–41) (0.00%) | 7 / 9 / 10 (10–12) (0.00%) — 4.6× faster |
| series | 26 / 31 (29–31) / 32 (31–34) (0.00%) | 7 (6–7) / 8 / 9 (8–10) (0.00%) — 3.7× faster |
| search | 57 / 69 (67–76) / 85 (85–88) (0.00%) | 28 / 35 (34–39) / 46 (42–48) (0.00%) — 2.0× faster ⚠[1] |
| playback | 32 / 38 (37–38) / 54 (52–58) (0.00%) | 7 (7–8) / 9 (9–10) / 25 (25–26) (0.00%) — 4.6× faster |

| endpoint | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home:image | 4.89 (4.82–4.90) / 7.24 (7.09–7.45) / 8.22 (8.21–8.80) (0.00%) | 0.67 (0.67–0.69) / 1.06 (1.06–1.14) / 1.81 (1.75–2.08) (0.00%) — 7.3× faster |
| home:latest-movies | 10.8 (10.7–10.8) / 14.1 (13.6–14.3) / 18.1 (15.6–18.2) (0.00%) | 5.08 (5.07–5.15) / 7.26 (7.22–7.54) / 8.28 (8.04–9.09) (0.00%) — 2.1× faster |
| home:latest-music | 7.37 (7.30–7.40) / 10.2 (10.2–10.5) / 12.4 (12.0–12.6) (0.00%) | 2.96 (2.92–3.00) / 4.75 (4.27–4.93) / 5.90 (5.28–6.06) (0.00%) — 2.5× faster |
| home:latest-shows | 22.3 (22.1–22.3) / 26.0 (25.8–26.0) / 28.4 (28.2–28.5) (0.00%) | 10.0 (9.98–10.1) / 12.4 (12.4–13.0) / 13.9 (13.5–14.6) (0.00%) — 2.2× faster |
| home:nextup | 24.9 (24.8–25.0) / 28.4 (28.4–28.7) / 31.3 (30.7–31.3) (0.00%) | 11.0 / 13.1 (13.1–13.9) / 14.4 (14.2–16.2) (0.00%) — 2.3× faster |
| home:resume-audio | 18.5 (18.3–18.6) / 22.3 (22.2–22.4) / 24.5 (24.0–25.0) (0.00%) | 5.88 (5.87–5.89) / 7.66 (7.33–8.02) / 9.28 (8.80–9.96) (0.00%) — 3.1× faster |
| home:resume-book | 18.5 (18.3–18.6) / 22.2 (22.1–22.5) / 24.3 (24.1–25.5) (0.00%) | 1.63 (1.62–1.64) / 4.05 (3.98–4.07) / 4.98 (4.73–5.34) (0.00%) — 11.3× faster |
| home:resume-video | 20.5 (20.2–20.6) / 24.1 (24.0–24.2) / 26.2 (26.1–26.3) (0.00%) | 7.22 (7.22–7.27) / 9.56 (9.49–9.84) / 11.1 (10.9–12.3) (0.00%) — 2.8× faster |
| home:views | 19.5 (19.4–19.6) / 22.8 (22.7–23.4) / 25.1 (25.0–25.2) (0.00%) | 4.62 (4.62–4.72) / 8.58 (8.41–9.26) / 10.1 (9.73–11.7) (0.00%) — 4.2× faster |
| movies:image | 4.81 (4.76–4.83) / 6.92 (6.87–6.96) / 8.18 (7.81–8.54) (0.00%) | 0.67 (0.67–0.68) / 1.07 (1.04–1.09) / 1.65 (1.61–1.96) (0.00%) — 7.2× faster |
| movies:items | 25.4 (25.1–25.6) / 30.1 (29.9–30.1) / 31.8 (31.7–34.0) (0.00%) | 12.0 (12.0–12.1) / 15.1 (14.8–15.3) / 16.7 (16.3–18.2) (0.00%) — 2.1× faster |
| detail:image | 4.73 (4.72–4.77) / 7.06 (6.83–7.08) / 8.32 (7.99–8.44) (0.00%) | 0.60 / 0.93 (0.92–0.96) / 1.37 (1.20–2.08) (0.00%) — 7.9× faster |
| detail:item | 10.5 (10.4–10.5) / 13.2 (13.2–13.3) / 15.6 (15.0–15.7) (0.00%) | 2.20 (2.08–2.21) / 2.72 (2.72–3.07) / 3.34 (3.26–4.88) (0.00%) — 4.8× faster |
| detail:local-trailers | 6.35 (6.34–6.36) / 8.76 (8.74–9.06) / 10.5 (10.5–11.1) (0.00%) | 1.10 (1.03–1.11) / 1.48 (1.45–1.60) / 1.69 (1.64–2.50) (0.00%) — 5.8× faster |
| detail:similar | 21.0 (21.0–21.1) / 24.2 (24.0–24.3) / 26.3 (26.2–26.7) (0.00%) | 4.95 (4.86–4.98) / 6.18 (6.10–6.30) / 6.66 (6.51–7.36) (0.00%) — 4.2× faster |
| detail:special-features | 6.34 (6.32–6.34) / 8.95 (8.47–9.01) / 10.5 (9.64–10.7) (0.00%) | 1.12 (1.05–1.12) / 1.50 (1.46–1.59) / 1.72 (1.71–2.35) (0.00%) — 5.7× faster |
| series:episodes | 6.16 (6.09–6.16) / 7.26 (7.21–7.51) / 8.12 (8.10–9.17) (0.00%) | 1.51 (1.51–1.52) / 1.85 (1.84–1.90) / 2.04 (2.02–2.30) (0.00%) — 4.1× faster |
| series:image | 3.67 (3.65–3.70) / 5.08 (4.90–5.35) / 6.49 (5.82–6.49) (0.00%) | 0.56 (0.55–0.56) / 0.73 (0.71–0.74) / 0.91 (0.90–0.96) (0.00%) — 6.6× faster |
| series:item | 11.8 (11.5–12.0) / 15.3 (14.5–15.8) / 17.0 (16.5–17.6) (0.00%) | 3.48 (3.44–3.50) / 4.18 (4.06–4.43) / 4.81 (4.52–5.73) (0.00%) — 3.4× faster |
| series:seasons | 8.84 (8.55–9.00) / 11.8 (11.5–12.3) / 13.4 (13.4–14.3) (0.00%) | 1.73 (1.73–1.78) / 2.30 (2.22–2.43) / 2.75 (2.36–3.36) (0.00%) — 5.1× faster |
| series:similar | 15.3 (14.9–15.4) / 18.7 (17.7–19.1) / 20.0 (19.8–21.3) (0.00%) | 3.85 (3.81–3.88) / 4.56 (4.47–4.69) / 5.20 (4.79–6.11) (0.00%) — 4.0× faster |
| search:artists | 10.6 (10.5–10.7) / 13.1 (12.6–13.3) / 14.3 (14.2–15.0) (0.00%) | 1.74 (1.74–1.78) / 4.07 (3.92–4.16) / 4.90 (4.76–5.00) (0.00%) — 6.1× faster ⚠[1] |
| search:image | 5.14 (5.11–5.16) / 8.02 (7.87–8.07) / 9.37 (8.95–9.74) (0.00%) | 0.79 (0.78–0.84) / 1.41 (1.27–1.51) / 2.27 (2.24–4.36) (0.00%) — 6.5× faster ⚠[2] |
| search:items | 42.7 (42.7–42.9) / 55.8 (52.4–61.3) / 67.0 (66.9–67.0) (0.00%) | 23.4 (23.0–23.6) / 29.0 (27.9–31.3) / 35.4 (34.6–38.5) (0.00%) — 1.8× faster ⚠[3] |
| search:persons | 17.0 (17.0–17.2) / 19.9 (19.6–20.0) / 21.4 (21.0–22.1) (0.00%) | 2.13 (2.11–2.14) / 4.08 (3.37–4.13) / 4.84 (4.69–4.90) (0.00%) — 8.0× faster |
| search:programs | 14.6 (14.5–14.7) / 17.4 (16.8–17.6) / 18.5 (17.9–19.0) (0.00%) | 1.36 (1.35–1.38) / 3.54 (2.93–3.82) / 4.54 (4.45–4.55) (0.00%) — 10.7× faster |
| search:videos | 33.1 (33.0–33.3) / 36.3 (35.8–36.5) / 38.1 (37.1–38.3) (0.00%) | 5.85 (5.82–5.86) / 6.66 (6.61–6.86) / 7.21 (7.07–8.39) (0.00%) — 5.7× faster |
| playback:intros | 6.09 (6.01–6.20) / 9.15 (9.11–9.54) / 10.7 (10.3–10.9) (0.00%) | 4.04 (3.95–4.06) / 4.50 (4.39–4.58) / 4.90 (4.48–4.92) (0.00%) — 1.5× faster |
| playback:playbackinfo | 8.12 (8.05–8.13) / 12.0 (11.4–12.2) / 13.7 (12.4–13.8) (0.00%) | 1.36 (1.35–1.44) / 1.90 (1.81–1.97) / 2.08 (1.94–2.19) (0.00%) — 6.0× faster |
| playback:playing | 8.95 (8.88–9.01) / 12.2 (12.1–12.3) / 25.3 (24.9–26.7) (0.00%) | 0.78 (0.78–0.79) / 0.95 (0.92–1.03) / 17.8 (13.6–17.9) (0.00%) — 11.5× faster |
| playback:segments | 6.18 (6.02–6.21) / 9.43 (9.37–9.87) / 10.9 (10.6–11.1) (0.00%) | 4.12 (4.04–4.13) / 4.58 (4.47–4.71) / 4.96 (4.64–5.50) (0.00%) — 1.5× faster |
| playback:stopped | 6.72 (6.70–6.73) / 8.86 (8.81–9.24) / 19.9 (17.7–20.8) (0.00%) | 0.75 (0.73–0.75) / 0.95 (0.87–0.99) / 5.07 (2.92–17.6) (0.00%) — 9.0× faster |
| image | 4.85 (4.80–4.86) / 7.22 (7.06–7.32) / 8.53 (8.24–8.72) (0.00%) | 0.65 (0.65–0.66) / 1.09 (1.06–1.15) / 1.84 (1.62–2.22) (0.00%) — 7.5× faster ⚠[4] |

### Image response bytes — stress (diagnostic)

| Run | Server | Bytes |
|---|---|---|
| 1 | Jellyfin 12.2.0 | 204402707 |
| 1 | Ferrofin | 191713725 |
| 2 | Jellyfin 12.2.0 | 204402707 |
| 2 | Ferrofin | 191713725 |
| 3 | Jellyfin 12.2.0 | 204324850 |
| 3 | Ferrofin | 191713725 |

### Shape coverage

| Run | Server | Slots | Requests | Distinct requests | Duration (s) | Distinct picks | Evidence |
|---|---|---|---|---|---|---|---|
| 1 | Jellyfin 12.2.0 | 600 | 8732 | 2926 | 31.384354115999997 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 1 | Ferrofin | 600 | 8732 | 2949 | 15.468153794 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 2 | Jellyfin 12.2.0 | 600 | 8732 | 2926 | 31.063594690000002 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 2 | Ferrofin | 600 | 8732 | 2949 | 15.715168018 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 3 | Jellyfin 12.2.0 | 600 | 8732 | 2926 | 31.222604452 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 3 | Ferrofin | 600 | 8732 | 2949 | 15.595770055000001 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |

### CPU and sample coverage — invocation and idle windows

CPU seconds use cumulative counters at the reported bracketing samples; load includes setup/teardown and harness overhead, and steady is post-load idle. Missing historical counters remain unavailable.

Average cores busy is CPU-seconds divided by observed wall seconds; 1.0 means one core fully occupied. Capacity busy divides that by the allocated logical CPU count. CPU milliseconds per completed screen divides total window CPU by completed mixed-screen iterations, including failed screens; it is not per-endpoint attribution. Lower values mean less CPU work only when completed work and validation agree. Each row retains its individual run; flagged workloads cannot support CPU savings claims. See the latency/work flags for differences in executed work.

#### CPU consumption (CPU-seconds)

| Window / metric | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 15.575 | 3.750 |
| loaded · run 1 | 68.926 | 17.956 |
| stress · run 1 | 335.734 | 87.343 |
| steady · run 1 | 0.046 | 0.006 |
| unloaded · run 2 | 15.454 | 3.766 |
| loaded · run 2 | 68.330 | 17.976 |
| stress · run 2 | 332.766 | 86.738 |
| steady · run 2 | 0.055 | 0.006 |
| unloaded · run 3 | 15.391 | 3.625 |
| loaded · run 3 | 68.409 | 18.100 |
| stress · run 3 | 335.516 | 86.622 |
| steady · run 3 | 0.043 | 0.006 |

#### Average CPU cores busy

| Window / metric | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 0.129 | 0.031 |
| loaded · run 1 | 0.572 | 0.149 |
| stress · run 1 | 2.786 | 0.725 |
| steady · run 1 | 0.001 | 0.000 |
| unloaded · run 2 | 0.128 | 0.031 |
| loaded · run 2 | 0.568 | 0.149 |
| stress · run 2 | 2.764 | 0.720 |
| steady · run 2 | 0.001 | 0.000 |
| unloaded · run 3 | 0.128 | 0.030 |
| loaded · run 3 | 0.568 | 0.150 |
| stress · run 3 | 2.787 | 0.719 |
| steady · run 3 | 0.001 | 0.000 |

#### Allocated CPU capacity busy (%)

| Window / metric | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 1.62% | 0.39% |
| loaded · run 1 | 7.16% | 1.86% |
| stress · run 1 | 34.83% | 9.07% |
| steady · run 1 | 0.01% | 0.00% |
| unloaded · run 2 | 1.60% | 0.39% |
| loaded · run 2 | 7.09% | 1.87% |
| stress · run 2 | 34.55% | 9.01% |
| steady · run 2 | 0.01% | 0.00% |
| unloaded · run 3 | 1.60% | 0.38% |
| loaded · run 3 | 7.10% | 1.88% |
| stress · run 3 | 34.83% | 8.99% |
| steady · run 3 | 0.01% | 0.00% |

#### CPU milliseconds per completed screen

| Window / metric | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 128.721 | 30.990 ⚠ workload validation failed |
| loaded · run 1 | 114.686 | 29.927 ⚠ workload validation failed |
| stress · run 1 | 111.874 | 29.105 ⚠ workload validation failed |
| unloaded · run 2 | 127.715 | 31.121 ⚠ workload validation failed |
| loaded · run 2 | 113.694 | 29.961 ⚠ workload validation failed |
| stress · run 2 | 110.885 | 28.903 ⚠ workload validation failed |
| unloaded · run 3 | 127.196 | 29.958 ⚠ workload validation failed |
| loaded · run 3 | 113.825 | 30.167 ⚠ workload validation failed |
| stress · run 3 | 111.839 | 28.864 ⚠ workload validation failed |

#### CPU per screen comparison (validated workloads only)

| Window / metric | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 · vs Jellyfin 12.2.0 | reference | withheld — validation flags |
| loaded · run 1 · vs Jellyfin 12.2.0 | reference | withheld — validation flags |
| stress · run 1 · vs Jellyfin 12.2.0 | reference | withheld — validation flags |
| unloaded · run 2 · vs Jellyfin 12.2.0 | reference | withheld — validation flags |
| loaded · run 2 · vs Jellyfin 12.2.0 | reference | withheld — validation flags |
| stress · run 2 · vs Jellyfin 12.2.0 | reference | withheld — validation flags |
| unloaded · run 3 · vs Jellyfin 12.2.0 | reference | withheld — validation flags |
| loaded · run 3 · vs Jellyfin 12.2.0 | reference | withheld — validation flags |
| stress · run 3 · vs Jellyfin 12.2.0 | reference | withheld — validation flags |

#### CPU sample coverage

| Window / metric | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 · Observed duration (s) | 120.400 | 120.400 |
| unloaded · run 1 · Samples | 1205 | 1205 |
| unloaded · run 1 · Largest gap (ms) | 100.493 | 101.527 |
| unloaded · run 1 · Observed start (UTC) | 2026-10-26T07:26:28.151+00:00 | 2026-10-26T07:40:09.806+00:00 |
| unloaded · run 1 · Observed end (UTC) | 2026-10-26T07:28:28.551+00:00 | 2026-10-26T07:42:10.206+00:00 |
| unloaded · run 1 · CPU evidence | available | available |
| loaded · run 1 · Observed duration (s) | 120.400 | 120.400 |
| loaded · run 1 · Samples | 1205 | 1205 |
| loaded · run 1 · Largest gap (ms) | 101.344 | 100.407 |
| loaded · run 1 · Observed start (UTC) | 2026-10-26T07:29:30.451+00:00 | 2026-10-26T07:43:11.606+00:00 |
| loaded · run 1 · Observed end (UTC) | 2026-10-26T07:31:30.851+00:00 | 2026-10-26T07:45:12.006+00:00 |
| loaded · run 1 · CPU evidence | available | available |
| stress · run 1 · Observed duration (s) | 120.500 | 120.400 |
| stress · run 1 · Samples | 1206 | 1205 |
| stress · run 1 · Largest gap (ms) | 102.883 | 102.621 |
| stress · run 1 · Observed start (UTC) | 2026-10-26T07:32:32.651+00:00 | 2026-10-26T07:46:13.306+00:00 |
| stress · run 1 · Observed end (UTC) | 2026-10-26T07:34:33.151+00:00 | 2026-10-26T07:48:13.706+00:00 |
| stress · run 1 · CPU evidence | available | available |
| steady · run 1 · Observed duration (s) | 60.100 | 60.100 |
| steady · run 1 · Samples | 602 | 602 |
| steady · run 1 · Largest gap (ms) | 100.249 | 100.174 |
| steady · run 1 · Observed start (UTC) | 2026-10-26T07:35:03.651+00:00 | 2026-10-26T07:48:44.106+00:00 |
| steady · run 1 · Observed end (UTC) | 2026-10-26T07:36:03.751+00:00 | 2026-10-26T07:49:44.206+00:00 |
| steady · run 1 · CPU evidence | available | available |
| unloaded · run 2 · Observed duration (s) | 120.400 | 120.400 |
| unloaded · run 2 · Samples | 1205 | 1205 |
| unloaded · run 2 · Largest gap (ms) | 100.224 | 101.156 |
| unloaded · run 2 · Observed start (UTC) | 2026-10-26T07:54:41.054+00:00 | 2026-10-26T08:08:21.652+00:00 |
| unloaded · run 2 · Observed end (UTC) | 2026-10-26T07:56:41.454+00:00 | 2026-10-26T08:10:22.052+00:00 |
| unloaded · run 2 · CPU evidence | available | available |
| loaded · run 2 · Observed duration (s) | 120.400 | 120.400 |
| loaded · run 2 · Samples | 1205 | 1205 |
| loaded · run 2 · Largest gap (ms) | 101.327 | 100.270 |
| loaded · run 2 · Observed start (UTC) | 2026-10-26T07:57:43.354+00:00 | 2026-10-26T08:11:23.352+00:00 |
| loaded · run 2 · Observed end (UTC) | 2026-10-26T07:59:43.754+00:00 | 2026-10-26T08:13:23.752+00:00 |
| loaded · run 2 · CPU evidence | available | available |
| stress · run 2 · Observed duration (s) | 120.400 | 120.400 |
| stress · run 2 · Samples | 1205 | 1205 |
| stress · run 2 · Largest gap (ms) | 100.886 | 101.812 |
| stress · run 2 · Observed start (UTC) | 2026-10-26T08:00:45.654+00:00 | 2026-10-26T08:14:25.152+00:00 |
| stress · run 2 · Observed end (UTC) | 2026-10-26T08:02:46.054+00:00 | 2026-10-26T08:16:25.552+00:00 |
| stress · run 2 · CPU evidence | available | available |
| steady · run 2 · Observed duration (s) | 60.100 | 60.100 |
| steady · run 2 · Samples | 602 | 602 |
| steady · run 2 · Largest gap (ms) | 100.233 | 100.230 |
| steady · run 2 · Observed start (UTC) | 2026-10-26T08:03:16.554+00:00 | 2026-10-26T08:16:55.952+00:00 |
| steady · run 2 · Observed end (UTC) | 2026-10-26T08:04:16.654+00:00 | 2026-10-26T08:17:56.052+00:00 |
| steady · run 2 · CPU evidence | available | available |
| unloaded · run 3 · Observed duration (s) | 120.400 | 120.400 |
| unloaded · run 3 · Samples | 1205 | 1205 |
| unloaded · run 3 · Largest gap (ms) | 100.226 | 101.188 |
| unloaded · run 3 · Observed start (UTC) | 2026-10-26T08:22:32.046+00:00 | 2026-10-26T08:36:14.355+00:00 |
| unloaded · run 3 · Observed end (UTC) | 2026-10-26T08:24:32.446+00:00 | 2026-10-26T08:38:14.755+00:00 |
| unloaded · run 3 · CPU evidence | available | available |
| loaded · run 3 · Observed duration (s) | 120.400 | 120.300 |
| loaded · run 3 · Samples | 1205 | 1204 |
| loaded · run 3 · Largest gap (ms) | 100.931 | 100.255 |
| loaded · run 3 · Observed start (UTC) | 2026-10-26T08:25:34.346+00:00 | 2026-10-26T08:39:16.155+00:00 |
| loaded · run 3 · Observed end (UTC) | 2026-10-26T08:27:34.746+00:00 | 2026-10-26T08:41:16.455+00:00 |
| loaded · run 3 · CPU evidence | available | available |
| stress · run 3 · Observed duration (s) | 120.400 | 120.400 |
| stress · run 3 · Samples | 1205 | 1205 |
| stress · run 3 · Largest gap (ms) | 101.706 | 102.692 |
| stress · run 3 · Observed start (UTC) | 2026-10-26T08:28:36.646+00:00 | 2026-10-26T08:42:17.855+00:00 |
| stress · run 3 · Observed end (UTC) | 2026-10-26T08:30:37.046+00:00 | 2026-10-26T08:44:18.255+00:00 |
| stress · run 3 · CPU evidence | available | available |
| steady · run 3 · Observed duration (s) | 60.100 | 60.100 |
| steady · run 3 · Samples | 602 | 602 |
| steady · run 3 · Largest gap (ms) | 100.224 | 100.286 |
| steady · run 3 · Observed start (UTC) | 2026-10-26T08:31:07.546+00:00 | 2026-10-26T08:44:48.655+00:00 |
| steady · run 3 · Observed end (UTC) | 2026-10-26T08:32:07.646+00:00 | 2026-10-26T08:45:48.755+00:00 |
| steady · run 3 · CPU evidence | available | available |

### Phase outcomes

| Run | Server | Phase | Status | Duration (s) | Start (UTC) | End (UTC) | Reason |
|---|---|---|---|---|---|---|---|
| 1 | Jellyfin 12.2.0 | startup | completed | 7.909 | 2026-10-26T07:23:00.642+00:00 | 2026-10-26T07:23:08.551+00:00 | — |
| 1 | Jellyfin 12.2.0 | startup-drain | completed | 30.068 | 2026-10-26T07:23:08.559+00:00 | 2026-10-26T07:23:38.627+00:00 | — |
| 1 | Jellyfin 12.2.0 | cleanup | completed | 0.426 | 2026-10-26T07:36:53.839+00:00 | 2026-10-26T07:36:54.265+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-setup | completed | 1.591 | 2026-10-26T07:23:38.645+00:00 | 2026-10-26T07:23:40.235+00:00 | — |
| 1 | Jellyfin 12.2.0 | counts | completed | 2.060 | 2026-10-26T07:23:40.246+00:00 | 2026-10-26T07:23:42.306+00:00 | — |
| 1 | Jellyfin 12.2.0 | shape | completed | 33.320 | 2026-10-26T07:23:42.653+00:00 | 2026-10-26T07:24:15.973+00:00 | — |
| 1 | Jellyfin 12.2.0 | coldstart | completed | 69.409 | 2026-10-26T07:24:16.549+00:00 | 2026-10-26T07:25:25.958+00:00 | — |
| 1 | Jellyfin 12.2.0 | ttfs | completed | 49.130 | 2026-10-26T07:36:04.388+00:00 | 2026-10-26T07:36:53.518+00:00 | — |
| 1 | Jellyfin 12.2.0 | unloaded | completed | 120.322 | 2026-10-26T07:26:28.218+00:00 | 2026-10-26T07:28:28.540+00:00 | — |
| 1 | Jellyfin 12.2.0 | warmup-unloaded | completed | 30.252 | 2026-10-26T07:25:57.139+00:00 | 2026-10-26T07:26:27.391+00:00 | — |
| 1 | Jellyfin 12.2.0 | drain-unloaded | completed | 30.062 | 2026-10-26T07:25:26.733+00:00 | 2026-10-26T07:25:56.795+00:00 | — |
| 1 | Jellyfin 12.2.0 | sampler | completed | 637.453 | 2026-10-26T07:25:26.623+00:00 | 2026-10-26T07:36:04.075+00:00 | — |
| 1 | Jellyfin 12.2.0 | steady | completed | 60.011 | 2026-10-26T07:35:03.657+00:00 | 2026-10-26T07:36:03.668+00:00 | — |
| 1 | Jellyfin 12.2.0 | drain-steady | completed | 30.064 | 2026-10-26T07:34:33.581+00:00 | 2026-10-26T07:35:03.646+00:00 | — |
| 1 | Jellyfin 12.2.0 | loaded | completed | 120.311 | 2026-10-26T07:29:30.486+00:00 | 2026-10-26T07:31:30.797+00:00 | — |
| 1 | Jellyfin 12.2.0 | warmup-loaded | completed | 30.254 | 2026-10-26T07:28:59.424+00:00 | 2026-10-26T07:29:29.678+00:00 | — |
| 1 | Jellyfin 12.2.0 | drain-loaded | completed | 30.064 | 2026-10-26T07:28:29.064+00:00 | 2026-10-26T07:28:59.128+00:00 | — |
| 1 | Jellyfin 12.2.0 | stress | completed | 120.324 | 2026-10-26T07:32:32.744+00:00 | 2026-10-26T07:34:33.068+00:00 | — |
| 1 | Jellyfin 12.2.0 | warmup-stress | completed | 30.271 | 2026-10-26T07:32:01.673+00:00 | 2026-10-26T07:32:31.945+00:00 | — |
| 1 | Jellyfin 12.2.0 | drain-stress | completed | 30.057 | 2026-10-26T07:31:31.314+00:00 | 2026-10-26T07:32:01.372+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-shape-before | completed | 0.328 | 2026-10-26T07:23:42.315+00:00 | 2026-10-26T07:23:42.643+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-shape-after | completed | 0.271 | 2026-10-26T07:24:15.984+00:00 | 2026-10-26T07:24:16.255+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-coldstart-before | completed | 0.274 | 2026-10-26T07:24:16.265+00:00 | 2026-10-26T07:24:16.538+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-coldstart-after | completed | 0.644 | 2026-10-26T07:25:25.968+00:00 | 2026-10-26T07:25:26.611+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-unloaded-before | completed | 0.323 | 2026-10-26T07:25:56.806+00:00 | 2026-10-26T07:25:57.129+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-unloaded-after | completed | 0.293 | 2026-10-26T07:26:27.402+00:00 | 2026-10-26T07:26:27.695+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-unloaded-before | completed | 0.295 | 2026-10-26T07:26:27.705+00:00 | 2026-10-26T07:26:28.000+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-unloaded-after | completed | 0.286 | 2026-10-26T07:28:28.768+00:00 | 2026-10-26T07:28:29.054+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-loaded-before | completed | 0.274 | 2026-10-26T07:28:59.139+00:00 | 2026-10-26T07:28:59.413+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-loaded-after | completed | 0.296 | 2026-10-26T07:29:29.688+00:00 | 2026-10-26T07:29:29.984+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-loaded-before | completed | 0.274 | 2026-10-26T07:29:29.993+00:00 | 2026-10-26T07:29:30.267+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-loaded-after | completed | 0.279 | 2026-10-26T07:31:31.025+00:00 | 2026-10-26T07:31:31.304+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-stress-before | completed | 0.279 | 2026-10-26T07:32:01.383+00:00 | 2026-10-26T07:32:01.662+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-stress-after | completed | 0.281 | 2026-10-26T07:32:31.956+00:00 | 2026-10-26T07:32:32.237+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-stress-before | completed | 0.280 | 2026-10-26T07:32:32.247+00:00 | 2026-10-26T07:32:32.528+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-stress-after | completed | 0.277 | 2026-10-26T07:34:33.294+00:00 | 2026-10-26T07:34:33.571+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-ttfs-before | completed | 0.291 | 2026-10-26T07:36:04.086+00:00 | 2026-10-26T07:36:04.377+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-ttfs-after | completed | 0.301 | 2026-10-26T07:36:53.529+00:00 | 2026-10-26T07:36:53.830+00:00 | — |
| 1 | Ferrofin | startup | completed | 31.935 | 2026-10-26T07:36:54.501+00:00 | 2026-10-26T07:37:26.436+00:00 | — |
| 1 | Ferrofin | startup-drain | completed | 30.052 | 2026-10-26T07:37:26.445+00:00 | 2026-10-26T07:37:56.497+00:00 | — |
| 1 | Ferrofin | cleanup | completed | 0.400 | 2026-10-26T07:50:33.875+00:00 | 2026-10-26T07:50:34.275+00:00 | — |
| 1 | Ferrofin | isolation-setup | completed | 0.304 | 2026-10-26T07:37:56.514+00:00 | 2026-10-26T07:37:56.818+00:00 | — |
| 1 | Ferrofin | counts | completed | 0.277 | 2026-10-26T07:37:56.828+00:00 | 2026-10-26T07:37:57.105+00:00 | — |
| 1 | Ferrofin | shape | completed | 17.414 | 2026-10-26T07:37:57.275+00:00 | 2026-10-26T07:38:14.689+00:00 | — |
| 1 | Ferrofin | coldstart | completed | 53.448 | 2026-10-26T07:38:15.047+00:00 | 2026-10-26T07:39:08.495+00:00 | — |
| 1 | Ferrofin | ttfs | completed | 48.926 | 2026-10-26T07:49:44.759+00:00 | 2026-10-26T07:50:33.685+00:00 | — |
| 1 | Ferrofin | unloaded | completed | 120.276 | 2026-10-26T07:40:09.864+00:00 | 2026-10-26T07:42:10.141+00:00 | — |
| 1 | Ferrofin | warmup-unloaded | completed | 30.271 | 2026-10-26T07:39:39.027+00:00 | 2026-10-26T07:40:09.298+00:00 | — |
| 1 | Ferrofin | drain-unloaded | completed | 30.061 | 2026-10-26T07:39:08.788+00:00 | 2026-10-26T07:39:38.849+00:00 | — |
| 1 | Ferrofin | sampler | completed | 635.888 | 2026-10-26T07:39:08.677+00:00 | 2026-10-26T07:49:44.565+00:00 | — |
| 1 | Ferrofin | steady | completed | 60.010 | 2026-10-26T07:48:44.151+00:00 | 2026-10-26T07:49:44.161+00:00 | — |
| 1 | Ferrofin | drain-steady | completed | 30.054 | 2026-10-26T07:48:14.084+00:00 | 2026-10-26T07:48:44.139+00:00 | — |
| 1 | Ferrofin | loaded | completed | 120.292 | 2026-10-26T07:43:11.641+00:00 | 2026-10-26T07:45:11.933+00:00 | — |
| 1 | Ferrofin | warmup-loaded | completed | 30.280 | 2026-10-26T07:42:40.791+00:00 | 2026-10-26T07:43:11.071+00:00 | — |
| 1 | Ferrofin | drain-loaded | completed | 30.055 | 2026-10-26T07:42:10.541+00:00 | 2026-10-26T07:42:40.596+00:00 | — |
| 1 | Ferrofin | stress | completed | 120.286 | 2026-10-26T07:46:13.405+00:00 | 2026-10-26T07:48:13.691+00:00 | — |
| 1 | Ferrofin | warmup-stress | completed | 30.259 | 2026-10-26T07:45:42.575+00:00 | 2026-10-26T07:46:12.834+00:00 | — |
| 1 | Ferrofin | drain-stress | completed | 30.057 | 2026-10-26T07:45:12.332+00:00 | 2026-10-26T07:45:42.389+00:00 | — |
| 1 | Ferrofin | isolation-shape-before | completed | 0.151 | 2026-10-26T07:37:57.114+00:00 | 2026-10-26T07:37:57.264+00:00 | — |
| 1 | Ferrofin | isolation-shape-after | completed | 0.170 | 2026-10-26T07:38:14.699+00:00 | 2026-10-26T07:38:14.870+00:00 | — |
| 1 | Ferrofin | isolation-coldstart-before | completed | 0.158 | 2026-10-26T07:38:14.878+00:00 | 2026-10-26T07:38:15.036+00:00 | — |
| 1 | Ferrofin | isolation-coldstart-after | completed | 0.163 | 2026-10-26T07:39:08.503+00:00 | 2026-10-26T07:39:08.666+00:00 | — |
| 1 | Ferrofin | isolation-warmup-unloaded-before | completed | 0.160 | 2026-10-26T07:39:38.859+00:00 | 2026-10-26T07:39:39.019+00:00 | — |
| 1 | Ferrofin | isolation-warmup-unloaded-after | completed | 0.162 | 2026-10-26T07:40:09.309+00:00 | 2026-10-26T07:40:09.471+00:00 | — |
| 1 | Ferrofin | isolation-unloaded-before | completed | 0.163 | 2026-10-26T07:40:09.482+00:00 | 2026-10-26T07:40:09.645+00:00 | — |
| 1 | Ferrofin | isolation-unloaded-after | completed | 0.163 | 2026-10-26T07:42:10.368+00:00 | 2026-10-26T07:42:10.531+00:00 | — |
| 1 | Ferrofin | isolation-warmup-loaded-before | completed | 0.173 | 2026-10-26T07:42:40.607+00:00 | 2026-10-26T07:42:40.779+00:00 | — |
| 1 | Ferrofin | isolation-warmup-loaded-after | completed | 0.175 | 2026-10-26T07:43:11.082+00:00 | 2026-10-26T07:43:11.256+00:00 | — |
| 1 | Ferrofin | isolation-loaded-before | completed | 0.157 | 2026-10-26T07:43:11.266+00:00 | 2026-10-26T07:43:11.424+00:00 | — |
| 1 | Ferrofin | isolation-loaded-after | completed | 0.161 | 2026-10-26T07:45:12.162+00:00 | 2026-10-26T07:45:12.323+00:00 | — |
| 1 | Ferrofin | isolation-warmup-stress-before | completed | 0.165 | 2026-10-26T07:45:42.399+00:00 | 2026-10-26T07:45:42.564+00:00 | — |
| 1 | Ferrofin | isolation-warmup-stress-after | completed | 0.168 | 2026-10-26T07:46:12.844+00:00 | 2026-10-26T07:46:13.013+00:00 | — |
| 1 | Ferrofin | isolation-stress-before | completed | 0.165 | 2026-10-26T07:46:13.022+00:00 | 2026-10-26T07:46:13.187+00:00 | — |
| 1 | Ferrofin | isolation-stress-after | completed | 0.158 | 2026-10-26T07:48:13.916+00:00 | 2026-10-26T07:48:14.075+00:00 | — |
| 1 | Ferrofin | isolation-ttfs-before | completed | 0.171 | 2026-10-26T07:49:44.575+00:00 | 2026-10-26T07:49:44.747+00:00 | — |
| 1 | Ferrofin | isolation-ttfs-after | completed | 0.171 | 2026-10-26T07:50:33.695+00:00 | 2026-10-26T07:50:33.866+00:00 | — |
| 2 | Jellyfin 12.2.0 | startup | completed | 7.802 | 2026-10-26T07:51:13.752+00:00 | 2026-10-26T07:51:21.554+00:00 | — |
| 2 | Jellyfin 12.2.0 | startup-drain | completed | 30.074 | 2026-10-26T07:51:21.563+00:00 | 2026-10-26T07:51:51.638+00:00 | — |
| 2 | Jellyfin 12.2.0 | cleanup | completed | 0.399 | 2026-10-26T08:05:05.722+00:00 | 2026-10-26T08:05:06.121+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-setup | completed | 1.540 | 2026-10-26T07:51:51.655+00:00 | 2026-10-26T07:51:53.195+00:00 | — |
| 2 | Jellyfin 12.2.0 | counts | completed | 2.023 | 2026-10-26T07:51:53.206+00:00 | 2026-10-26T07:51:55.229+00:00 | — |
| 2 | Jellyfin 12.2.0 | shape | completed | 32.975 | 2026-10-26T07:51:55.573+00:00 | 2026-10-26T07:52:28.549+00:00 | — |
| 2 | Jellyfin 12.2.0 | coldstart | completed | 69.581 | 2026-10-26T07:52:29.161+00:00 | 2026-10-26T07:53:38.742+00:00 | — |
| 2 | Jellyfin 12.2.0 | ttfs | completed | 48.122 | 2026-10-26T08:04:17.289+00:00 | 2026-10-26T08:05:05.411+00:00 | — |
| 2 | Jellyfin 12.2.0 | unloaded | completed | 120.321 | 2026-10-26T07:54:41.096+00:00 | 2026-10-26T07:56:41.417+00:00 | — |
| 2 | Jellyfin 12.2.0 | warmup-unloaded | completed | 30.316 | 2026-10-26T07:54:09.956+00:00 | 2026-10-26T07:54:40.273+00:00 | — |
| 2 | Jellyfin 12.2.0 | drain-unloaded | completed | 30.067 | 2026-10-26T07:53:39.535+00:00 | 2026-10-26T07:54:09.602+00:00 | — |
| 2 | Jellyfin 12.2.0 | sampler | completed | 637.552 | 2026-10-26T07:53:39.425+00:00 | 2026-10-26T08:04:16.977+00:00 | — |
| 2 | Jellyfin 12.2.0 | steady | completed | 60.011 | 2026-10-26T08:03:16.559+00:00 | 2026-10-26T08:04:16.570+00:00 | — |
| 2 | Jellyfin 12.2.0 | drain-steady | completed | 30.062 | 2026-10-26T08:02:46.485+00:00 | 2026-10-26T08:03:16.547+00:00 | — |
| 2 | Jellyfin 12.2.0 | loaded | completed | 120.315 | 2026-10-26T07:57:43.389+00:00 | 2026-10-26T07:59:43.703+00:00 | — |
| 2 | Jellyfin 12.2.0 | warmup-loaded | completed | 30.262 | 2026-10-26T07:57:12.302+00:00 | 2026-10-26T07:57:42.564+00:00 | — |
| 2 | Jellyfin 12.2.0 | drain-loaded | completed | 30.062 | 2026-10-26T07:56:41.947+00:00 | 2026-10-26T07:57:12.010+00:00 | — |
| 2 | Jellyfin 12.2.0 | stress | completed | 120.311 | 2026-10-26T08:00:45.657+00:00 | 2026-10-26T08:02:45.967+00:00 | — |
| 2 | Jellyfin 12.2.0 | warmup-stress | completed | 30.280 | 2026-10-26T08:00:14.580+00:00 | 2026-10-26T08:00:44.860+00:00 | — |
| 2 | Jellyfin 12.2.0 | drain-stress | completed | 30.062 | 2026-10-26T07:59:44.220+00:00 | 2026-10-26T08:00:14.283+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-shape-before | completed | 0.327 | 2026-10-26T07:51:55.238+00:00 | 2026-10-26T07:51:55.564+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-shape-after | completed | 0.301 | 2026-10-26T07:52:28.559+00:00 | 2026-10-26T07:52:28.860+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-coldstart-before | completed | 0.282 | 2026-10-26T07:52:28.869+00:00 | 2026-10-26T07:52:29.151+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-coldstart-after | completed | 0.664 | 2026-10-26T07:53:38.751+00:00 | 2026-10-26T07:53:39.415+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-unloaded-before | completed | 0.332 | 2026-10-26T07:54:09.613+00:00 | 2026-10-26T07:54:09.945+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-unloaded-after | completed | 0.281 | 2026-10-26T07:54:40.283+00:00 | 2026-10-26T07:54:40.565+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-unloaded-before | completed | 0.301 | 2026-10-26T07:54:40.575+00:00 | 2026-10-26T07:54:40.877+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-unloaded-after | completed | 0.295 | 2026-10-26T07:56:41.643+00:00 | 2026-10-26T07:56:41.938+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-loaded-before | completed | 0.271 | 2026-10-26T07:57:12.021+00:00 | 2026-10-26T07:57:12.292+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-loaded-after | completed | 0.296 | 2026-10-26T07:57:42.575+00:00 | 2026-10-26T07:57:42.871+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-loaded-before | completed | 0.286 | 2026-10-26T07:57:42.883+00:00 | 2026-10-26T07:57:43.169+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-loaded-after | completed | 0.277 | 2026-10-26T07:59:43.933+00:00 | 2026-10-26T07:59:44.210+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-stress-before | completed | 0.276 | 2026-10-26T08:00:14.294+00:00 | 2026-10-26T08:00:14.569+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-stress-after | completed | 0.281 | 2026-10-26T08:00:44.871+00:00 | 2026-10-26T08:00:45.152+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-stress-before | completed | 0.276 | 2026-10-26T08:00:45.162+00:00 | 2026-10-26T08:00:45.438+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-stress-after | completed | 0.279 | 2026-10-26T08:02:46.195+00:00 | 2026-10-26T08:02:46.474+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-ttfs-before | completed | 0.292 | 2026-10-26T08:04:16.987+00:00 | 2026-10-26T08:04:17.279+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-ttfs-after | completed | 0.289 | 2026-10-26T08:05:05.421+00:00 | 2026-10-26T08:05:05.710+00:00 | — |
| 2 | Ferrofin | startup | completed | 31.535 | 2026-10-26T08:05:06.361+00:00 | 2026-10-26T08:05:37.896+00:00 | — |
| 2 | Ferrofin | startup-drain | completed | 30.052 | 2026-10-26T08:05:37.905+00:00 | 2026-10-26T08:06:07.957+00:00 | — |
| 2 | Ferrofin | cleanup | completed | 0.402 | 2026-10-26T08:18:44.628+00:00 | 2026-10-26T08:18:45.029+00:00 | — |
| 2 | Ferrofin | isolation-setup | completed | 0.294 | 2026-10-26T08:06:07.974+00:00 | 2026-10-26T08:06:08.268+00:00 | — |
| 2 | Ferrofin | counts | completed | 0.267 | 2026-10-26T08:06:08.280+00:00 | 2026-10-26T08:06:08.546+00:00 | — |
| 2 | Ferrofin | shape | completed | 17.684 | 2026-10-26T08:06:08.729+00:00 | 2026-10-26T08:06:26.413+00:00 | — |
| 2 | Ferrofin | coldstart | completed | 53.560 | 2026-10-26T08:06:26.790+00:00 | 2026-10-26T08:07:20.350+00:00 | — |
| 2 | Ferrofin | ttfs | completed | 47.898 | 2026-10-26T08:17:56.547+00:00 | 2026-10-26T08:18:44.445+00:00 | — |
| 2 | Ferrofin | unloaded | completed | 120.270 | 2026-10-26T08:08:21.706+00:00 | 2026-10-26T08:10:21.975+00:00 | — |
| 2 | Ferrofin | warmup-unloaded | completed | 30.280 | 2026-10-26T08:07:50.872+00:00 | 2026-10-26T08:08:21.152+00:00 | — |
| 2 | Ferrofin | drain-unloaded | completed | 30.055 | 2026-10-26T08:07:20.635+00:00 | 2026-10-26T08:07:50.690+00:00 | — |
| 2 | Ferrofin | sampler | completed | 635.845 | 2026-10-26T08:07:20.524+00:00 | 2026-10-26T08:17:56.369+00:00 | — |
| 2 | Ferrofin | steady | completed | 60.011 | 2026-10-26T08:16:55.955+00:00 | 2026-10-26T08:17:55.966+00:00 | — |
| 2 | Ferrofin | drain-steady | completed | 30.059 | 2026-10-26T08:16:25.884+00:00 | 2026-10-26T08:16:55.943+00:00 | — |
| 2 | Ferrofin | loaded | completed | 120.256 | 2026-10-26T08:11:23.438+00:00 | 2026-10-26T08:13:23.695+00:00 | — |
| 2 | Ferrofin | warmup-loaded | completed | 30.276 | 2026-10-26T08:10:52.606+00:00 | 2026-10-26T08:11:22.882+00:00 | — |
| 2 | Ferrofin | drain-loaded | completed | 30.055 | 2026-10-26T08:10:22.372+00:00 | 2026-10-26T08:10:52.427+00:00 | — |
| 2 | Ferrofin | stress | completed | 120.282 | 2026-10-26T08:14:25.190+00:00 | 2026-10-26T08:16:25.472+00:00 | — |
| 2 | Ferrofin | warmup-stress | completed | 30.272 | 2026-10-26T08:13:54.340+00:00 | 2026-10-26T08:14:24.612+00:00 | — |
| 2 | Ferrofin | drain-stress | completed | 30.056 | 2026-10-26T08:13:24.096+00:00 | 2026-10-26T08:13:54.152+00:00 | — |
| 2 | Ferrofin | isolation-shape-before | completed | 0.165 | 2026-10-26T08:06:08.555+00:00 | 2026-10-26T08:06:08.721+00:00 | — |
| 2 | Ferrofin | isolation-shape-after | completed | 0.168 | 2026-10-26T08:06:26.423+00:00 | 2026-10-26T08:06:26.591+00:00 | — |
| 2 | Ferrofin | isolation-coldstart-before | completed | 0.178 | 2026-10-26T08:06:26.601+00:00 | 2026-10-26T08:06:26.779+00:00 | — |
| 2 | Ferrofin | isolation-coldstart-after | completed | 0.153 | 2026-10-26T08:07:20.361+00:00 | 2026-10-26T08:07:20.514+00:00 | — |
| 2 | Ferrofin | isolation-warmup-unloaded-before | completed | 0.160 | 2026-10-26T08:07:50.701+00:00 | 2026-10-26T08:07:50.861+00:00 | — |
| 2 | Ferrofin | isolation-warmup-unloaded-after | completed | 0.162 | 2026-10-26T08:08:21.162+00:00 | 2026-10-26T08:08:21.324+00:00 | — |
| 2 | Ferrofin | isolation-unloaded-before | completed | 0.152 | 2026-10-26T08:08:21.335+00:00 | 2026-10-26T08:08:21.487+00:00 | — |
| 2 | Ferrofin | isolation-unloaded-after | completed | 0.160 | 2026-10-26T08:10:22.202+00:00 | 2026-10-26T08:10:22.362+00:00 | — |
| 2 | Ferrofin | isolation-warmup-loaded-before | completed | 0.158 | 2026-10-26T08:10:52.439+00:00 | 2026-10-26T08:10:52.597+00:00 | — |
| 2 | Ferrofin | isolation-warmup-loaded-after | completed | 0.161 | 2026-10-26T08:11:22.894+00:00 | 2026-10-26T08:11:23.055+00:00 | — |
| 2 | Ferrofin | isolation-loaded-before | completed | 0.155 | 2026-10-26T08:11:23.064+00:00 | 2026-10-26T08:11:23.220+00:00 | — |
| 2 | Ferrofin | isolation-loaded-after | completed | 0.163 | 2026-10-26T08:13:23.922+00:00 | 2026-10-26T08:13:24.085+00:00 | — |
| 2 | Ferrofin | isolation-warmup-stress-before | completed | 0.167 | 2026-10-26T08:13:54.162+00:00 | 2026-10-26T08:13:54.329+00:00 | — |
| 2 | Ferrofin | isolation-warmup-stress-after | completed | 0.178 | 2026-10-26T08:14:24.623+00:00 | 2026-10-26T08:14:24.801+00:00 | — |
| 2 | Ferrofin | isolation-stress-before | completed | 0.164 | 2026-10-26T08:14:24.810+00:00 | 2026-10-26T08:14:24.973+00:00 | — |
| 2 | Ferrofin | isolation-stress-after | completed | 0.175 | 2026-10-26T08:16:25.699+00:00 | 2026-10-26T08:16:25.874+00:00 | — |
| 2 | Ferrofin | isolation-ttfs-before | completed | 0.158 | 2026-10-26T08:17:56.379+00:00 | 2026-10-26T08:17:56.538+00:00 | — |
| 2 | Ferrofin | isolation-ttfs-after | completed | 0.161 | 2026-10-26T08:18:44.456+00:00 | 2026-10-26T08:18:44.617+00:00 | — |
| 3 | Jellyfin 12.2.0 | startup | completed | 7.879 | 2026-10-26T08:19:04.576+00:00 | 2026-10-26T08:19:12.455+00:00 | — |
| 3 | Jellyfin 12.2.0 | startup-drain | completed | 30.070 | 2026-10-26T08:19:12.463+00:00 | 2026-10-26T08:19:42.533+00:00 | — |
| 3 | Jellyfin 12.2.0 | cleanup | completed | 0.426 | 2026-10-26T08:32:58.757+00:00 | 2026-10-26T08:32:59.182+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-setup | completed | 1.585 | 2026-10-26T08:19:42.550+00:00 | 2026-10-26T08:19:44.135+00:00 | — |
| 3 | Jellyfin 12.2.0 | counts | completed | 2.106 | 2026-10-26T08:19:44.146+00:00 | 2026-10-26T08:19:46.252+00:00 | — |
| 3 | Jellyfin 12.2.0 | shape | completed | 33.166 | 2026-10-26T08:19:46.606+00:00 | 2026-10-26T08:20:19.772+00:00 | — |
| 3 | Jellyfin 12.2.0 | coldstart | completed | 69.365 | 2026-10-26T08:20:20.361+00:00 | 2026-10-26T08:21:29.726+00:00 | — |
| 3 | Jellyfin 12.2.0 | ttfs | completed | 50.129 | 2026-10-26T08:32:08.313+00:00 | 2026-10-26T08:32:58.442+00:00 | — |
| 3 | Jellyfin 12.2.0 | unloaded | completed | 120.301 | 2026-10-26T08:22:32.082+00:00 | 2026-10-26T08:24:32.383+00:00 | — |
| 3 | Jellyfin 12.2.0 | warmup-unloaded | completed | 30.304 | 2026-10-26T08:22:00.951+00:00 | 2026-10-26T08:22:31.254+00:00 | — |
| 3 | Jellyfin 12.2.0 | drain-unloaded | completed | 30.064 | 2026-10-26T08:21:30.527+00:00 | 2026-10-26T08:22:00.591+00:00 | — |
| 3 | Jellyfin 12.2.0 | sampler | completed | 637.555 | 2026-10-26T08:21:30.417+00:00 | 2026-10-26T08:32:07.972+00:00 | — |
| 3 | Jellyfin 12.2.0 | steady | completed | 60.010 | 2026-10-26T08:31:07.549+00:00 | 2026-10-26T08:32:07.560+00:00 | — |
| 3 | Jellyfin 12.2.0 | drain-steady | completed | 30.067 | 2026-10-26T08:30:37.470+00:00 | 2026-10-26T08:31:07.537+00:00 | — |
| 3 | Jellyfin 12.2.0 | loaded | completed | 120.306 | 2026-10-26T08:25:34.410+00:00 | 2026-10-26T08:27:34.716+00:00 | — |
| 3 | Jellyfin 12.2.0 | warmup-loaded | completed | 30.306 | 2026-10-26T08:25:03.290+00:00 | 2026-10-26T08:25:33.595+00:00 | — |
| 3 | Jellyfin 12.2.0 | drain-loaded | completed | 30.063 | 2026-10-26T08:24:32.926+00:00 | 2026-10-26T08:25:02.989+00:00 | — |
| 3 | Jellyfin 12.2.0 | stress | completed | 120.288 | 2026-10-26T08:28:36.654+00:00 | 2026-10-26T08:30:36.943+00:00 | — |
| 3 | Jellyfin 12.2.0 | warmup-stress | completed | 30.266 | 2026-10-26T08:28:05.587+00:00 | 2026-10-26T08:28:35.853+00:00 | — |
| 3 | Jellyfin 12.2.0 | drain-stress | completed | 30.063 | 2026-10-26T08:27:35.230+00:00 | 2026-10-26T08:28:05.294+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-shape-before | completed | 0.335 | 2026-10-26T08:19:46.261+00:00 | 2026-10-26T08:19:46.597+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-shape-after | completed | 0.285 | 2026-10-26T08:20:19.782+00:00 | 2026-10-26T08:20:20.067+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-coldstart-before | completed | 0.275 | 2026-10-26T08:20:20.077+00:00 | 2026-10-26T08:20:20.352+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-coldstart-after | completed | 0.671 | 2026-10-26T08:21:29.736+00:00 | 2026-10-26T08:21:30.407+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-unloaded-before | completed | 0.339 | 2026-10-26T08:22:00.601+00:00 | 2026-10-26T08:22:00.940+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-unloaded-after | completed | 0.284 | 2026-10-26T08:22:31.265+00:00 | 2026-10-26T08:22:31.549+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-unloaded-before | completed | 0.302 | 2026-10-26T08:22:31.559+00:00 | 2026-10-26T08:22:31.862+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-unloaded-after | completed | 0.305 | 2026-10-26T08:24:32.610+00:00 | 2026-10-26T08:24:32.915+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-loaded-before | completed | 0.280 | 2026-10-26T08:25:03.000+00:00 | 2026-10-26T08:25:03.279+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-loaded-after | completed | 0.295 | 2026-10-26T08:25:33.606+00:00 | 2026-10-26T08:25:33.901+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-loaded-before | completed | 0.279 | 2026-10-26T08:25:33.912+00:00 | 2026-10-26T08:25:34.191+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-loaded-after | completed | 0.278 | 2026-10-26T08:27:34.942+00:00 | 2026-10-26T08:27:35.220+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-stress-before | completed | 0.272 | 2026-10-26T08:28:05.304+00:00 | 2026-10-26T08:28:05.577+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-stress-after | completed | 0.286 | 2026-10-26T08:28:35.864+00:00 | 2026-10-26T08:28:36.150+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-stress-before | completed | 0.277 | 2026-10-26T08:28:36.160+00:00 | 2026-10-26T08:28:36.437+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-stress-after | completed | 0.288 | 2026-10-26T08:30:37.170+00:00 | 2026-10-26T08:30:37.458+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-ttfs-before | completed | 0.320 | 2026-10-26T08:32:07.983+00:00 | 2026-10-26T08:32:08.303+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-ttfs-after | completed | 0.293 | 2026-10-26T08:32:58.452+00:00 | 2026-10-26T08:32:58.745+00:00 | — |
| 3 | Ferrofin | startup | completed | 31.336 | 2026-10-26T08:32:59.422+00:00 | 2026-10-26T08:33:30.758+00:00 | — |
| 3 | Ferrofin | startup-drain | completed | 30.053 | 2026-10-26T08:33:30.766+00:00 | 2026-10-26T08:34:00.820+00:00 | — |
| 3 | Ferrofin | cleanup | completed | 0.385 | 2026-10-26T08:46:37.367+00:00 | 2026-10-26T08:46:37.752+00:00 | — |
| 3 | Ferrofin | isolation-setup | completed | 0.304 | 2026-10-26T08:34:00.836+00:00 | 2026-10-26T08:34:01.140+00:00 | — |
| 3 | Ferrofin | counts | completed | 0.265 | 2026-10-26T08:34:01.150+00:00 | 2026-10-26T08:34:01.415+00:00 | — |
| 3 | Ferrofin | shape | completed | 17.574 | 2026-10-26T08:34:01.594+00:00 | 2026-10-26T08:34:19.168+00:00 | — |
| 3 | Ferrofin | coldstart | completed | 53.522 | 2026-10-26T08:34:19.527+00:00 | 2026-10-26T08:35:13.049+00:00 | — |
| 3 | Ferrofin | ttfs | completed | 47.906 | 2026-10-26T08:45:49.268+00:00 | 2026-10-26T08:46:37.174+00:00 | — |
| 3 | Ferrofin | unloaded | completed | 120.274 | 2026-10-26T08:36:14.408+00:00 | 2026-10-26T08:38:14.681+00:00 | — |
| 3 | Ferrofin | warmup-unloaded | completed | 30.270 | 2026-10-26T08:35:43.571+00:00 | 2026-10-26T08:36:13.841+00:00 | — |
| 3 | Ferrofin | drain-unloaded | completed | 30.053 | 2026-10-26T08:35:13.338+00:00 | 2026-10-26T08:35:43.391+00:00 | — |
| 3 | Ferrofin | sampler | completed | 635.859 | 2026-10-26T08:35:13.228+00:00 | 2026-10-26T08:45:49.088+00:00 | — |
| 3 | Ferrofin | steady | completed | 60.010 | 2026-10-26T08:44:48.676+00:00 | 2026-10-26T08:45:48.686+00:00 | — |
| 3 | Ferrofin | drain-steady | completed | 30.054 | 2026-10-26T08:44:18.611+00:00 | 2026-10-26T08:44:48.665+00:00 | — |
| 3 | Ferrofin | loaded | completed | 120.273 | 2026-10-26T08:39:16.167+00:00 | 2026-10-26T08:41:16.440+00:00 | — |
| 3 | Ferrofin | warmup-loaded | completed | 30.274 | 2026-10-26T08:38:45.310+00:00 | 2026-10-26T08:39:15.584+00:00 | — |
| 3 | Ferrofin | drain-loaded | completed | 30.053 | 2026-10-26T08:38:15.081+00:00 | 2026-10-26T08:38:45.134+00:00 | — |
| 3 | Ferrofin | stress | completed | 120.285 | 2026-10-26T08:42:17.930+00:00 | 2026-10-26T08:44:18.214+00:00 | — |
| 3 | Ferrofin | warmup-stress | completed | 30.272 | 2026-10-26T08:41:47.086+00:00 | 2026-10-26T08:42:17.358+00:00 | — |
| 3 | Ferrofin | drain-stress | completed | 30.054 | 2026-10-26T08:41:16.855+00:00 | 2026-10-26T08:41:46.909+00:00 | — |
| 3 | Ferrofin | isolation-shape-before | completed | 0.159 | 2026-10-26T08:34:01.424+00:00 | 2026-10-26T08:34:01.583+00:00 | — |
| 3 | Ferrofin | isolation-shape-after | completed | 0.166 | 2026-10-26T08:34:19.179+00:00 | 2026-10-26T08:34:19.345+00:00 | — |
| 3 | Ferrofin | isolation-coldstart-before | completed | 0.161 | 2026-10-26T08:34:19.355+00:00 | 2026-10-26T08:34:19.516+00:00 | — |
| 3 | Ferrofin | isolation-coldstart-after | completed | 0.158 | 2026-10-26T08:35:13.060+00:00 | 2026-10-26T08:35:13.218+00:00 | — |
| 3 | Ferrofin | isolation-warmup-unloaded-before | completed | 0.159 | 2026-10-26T08:35:43.402+00:00 | 2026-10-26T08:35:43.561+00:00 | — |
| 3 | Ferrofin | isolation-warmup-unloaded-after | completed | 0.173 | 2026-10-26T08:36:13.852+00:00 | 2026-10-26T08:36:14.024+00:00 | — |
| 3 | Ferrofin | isolation-unloaded-before | completed | 0.155 | 2026-10-26T08:36:14.034+00:00 | 2026-10-26T08:36:14.189+00:00 | — |
| 3 | Ferrofin | isolation-unloaded-after | completed | 0.163 | 2026-10-26T08:38:14.909+00:00 | 2026-10-26T08:38:15.072+00:00 | — |
| 3 | Ferrofin | isolation-warmup-loaded-before | completed | 0.156 | 2026-10-26T08:38:45.144+00:00 | 2026-10-26T08:38:45.300+00:00 | — |
| 3 | Ferrofin | isolation-warmup-loaded-after | completed | 0.178 | 2026-10-26T08:39:15.594+00:00 | 2026-10-26T08:39:15.772+00:00 | — |
| 3 | Ferrofin | isolation-loaded-before | completed | 0.166 | 2026-10-26T08:39:15.782+00:00 | 2026-10-26T08:39:15.948+00:00 | — |
| 3 | Ferrofin | isolation-loaded-after | completed | 0.179 | 2026-10-26T08:41:16.667+00:00 | 2026-10-26T08:41:16.846+00:00 | — |
| 3 | Ferrofin | isolation-warmup-stress-before | completed | 0.157 | 2026-10-26T08:41:46.919+00:00 | 2026-10-26T08:41:47.076+00:00 | — |
| 3 | Ferrofin | isolation-warmup-stress-after | completed | 0.161 | 2026-10-26T08:42:17.369+00:00 | 2026-10-26T08:42:17.530+00:00 | — |
| 3 | Ferrofin | isolation-stress-before | completed | 0.170 | 2026-10-26T08:42:17.541+00:00 | 2026-10-26T08:42:17.711+00:00 | — |
| 3 | Ferrofin | isolation-stress-after | completed | 0.160 | 2026-10-26T08:44:18.441+00:00 | 2026-10-26T08:44:18.601+00:00 | — |
| 3 | Ferrofin | isolation-ttfs-before | completed | 0.160 | 2026-10-26T08:45:49.099+00:00 | 2026-10-26T08:45:49.258+00:00 | — |
| 3 | Ferrofin | isolation-ttfs-after | completed | 0.171 | 2026-10-26T08:46:37.186+00:00 | 2026-10-26T08:46:37.356+00:00 | — |

### Time to first screen

| | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| cold start (restart → authenticated views; host caches retained) | 3333 ms (3324 ms–3343 ms) | 275 ms (269 ms–284 ms) — 12.1× faster |
| HLS first segment (forced transcode) | 431 ms (430 ms–530 ms) | 184 ms (178 ms–397 ms) — 2.3× faster |
| direct-play TTFB (1 MiB range) | 4.12 ms (3.96 ms–4.21 ms) | 0.64 ms (0.58 ms–0.68 ms) — 6.4× faster |

### Memory (anon, cache excluded, 100 ms samples)

| | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| peak under load | 544 MiB (531 MiB–567 MiB) | 206 MiB (204 MiB–209 MiB) — 2.6× lighter |
| steady idle | 393 MiB (369 MiB–405 MiB) | 154 MiB (150 MiB–161 MiB) — 2.6× lighter |
| interference on the server's cores, p95 | 17% (16%–17%) | 18% (16%–20%) |
| interference, max single sample | 51% (42%–52%) | 40% (39%–43%) |
| max swap | 0.00 MiB | 0.00 MiB |

### Response shape vs Jellyfin 12.2.0 (supporting evidence, not the parity number)

**ferrofin**: 4 divergence(s) across 33 compared requests
- image: request picks differ
- search:artists: ids differs for a request pick
- search:image: request picks differ
- search:items: ids differs for a request pick


### Notes

1. search:artists: ids differs for a request pick
   — Ferrofin · search (unloaded screens); Ferrofin · search:artists (unloaded endpoints); Ferrofin · search (loaded screens); Ferrofin · search:artists (loaded endpoints); Ferrofin · search (stress screens); Ferrofin · search:artists (stress endpoints)
2. search:image: request picks differ
   — Ferrofin · search:image (unloaded endpoints); Ferrofin · search:image (loaded endpoints); Ferrofin · search:image (stress endpoints)
3. search:items: ids differs for a request pick
   — Ferrofin · search:items (unloaded endpoints); Ferrofin · search:items (loaded endpoints); Ferrofin · search:items (stress endpoints)
4. image: request picks differ
   — Ferrofin · image (unloaded endpoints); Ferrofin · image (loaded endpoints); Ferrofin · image (stress endpoints)

---

## Reproduce the reports

Raw evidence stays outside the repository, under the gates' output root (`$FERROFIN_GATES_DIR/runs/`).
The README's numbers come from the same three runs, through `docs/results/v1.4.0/summary.json`.
Role credentials must not be published.

```sh
python3 gates/bench/report.py \
  "$FERROFIN_GATES_DIR"/runs/detached-5112240e1923-20261026-0101/bench \
  "$FERROFIN_GATES_DIR"/runs/detached-5112240e1923-20261026-0151/bench \
  "$FERROFIN_GATES_DIR"/runs/detached-5112240e1923-20261026-0219/bench
```
