# Benchmarks — three-run results

Measured on **2026-10-20** from server source **`d2bb9306`** (`release/v1.4.0`), against
**Jellyfin 12.2.0** (pinned by digest in `bench/testdata/jellyfin-image.txt`), using three
complete repetitions. The root [README](../README.md#benchmarks) withholds flagged cells;
this detailed report keeps flagged raw measurements as **diagnostics, not performance
claims**. The [methodology](../bench/README.md) defines the measurements and checks.

Every run passed the start-of-run idle check (92.8 %, 98.0 % and 98.1 %). The first run
then saw 42–49 % p95 interference on the server cores during measurement, against 13–14 %
in the other two; the ranges below include it.

Everything between the two rules is the unedited output of `bench/report.py` for the three runs.

---

## Ferrofin vs Jellyfin — 3 run(s), commit d2bb9306, 2026-10-20
Host AMD Ryzen 9 9950X3D 16-Core Processor · server on cpus 8-15 · 8g limit · test data 3001 movies, 250 series, 7490 episodes, 50 hdr · windows 120 s · unloaded 1 screens/s · loaded 5 screens/s · stress 25 screens/s
Cells are the median across runs; where the runs disagreed, the range they spanned follows in brackets. `⚠[n]` means the server did different work than Jellyfin 12.2.0 (status / record count / missing fields), so the number is not comparable: it is kept for the work list, not for publication, and note `n` says why. `X.Y× faster` compares the cell with Jellyfin 12.2.0 on the same row. It is shown on flagged cells too — the note says the two servers did not do identical work, so read it as an indication rather than a like-for-like result.

### Latency — unloaded (1 screens/s)

| screen | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home | 47 (46–52) / 53 (49–61) / 54 (54–66) (0.00%) | 20 (19–20) / 26 (25–26) / 29 (27–29) (0.00%) — 2.4× faster ⚠[1] |
| movies | 41 (38–45) / 48 (44–57) / 53 (51–61) (0.00%) | 16 (16–17) / 18 (18–19) / 21 (19–21) (0.00%) — 2.6× faster |
| detail | 36 (34–43) / 39 (36–50) / 41 (37–55) (0.00%) | 9 / 11 (10–13) / 12 (10–15) (0.00%) — 4.0× faster ⚠[2] |
| series | 26 (25–30) / 29 (28–164) / 30 (28–294) (0.00%) | 24 (24–28) / 28 (26–29) / 28 (27–30) (0.00%) — 1.1× faster ⚠[3] |
| search | 73 (72–79) / 199 (107–202) / 300 (109–301) (0.00%) | 27 (27–28) / 30 (28–31) / 30 (29–32) (0.00%) — 2.7× faster ⚠[4] |
| playback | 32 (31–34) / 37 (34–43) / 38 (36–45) (0.00%) | 11 (11–12) / 12 / 12 (12–13) (0.00%) — 2.9× faster |

| endpoint | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home:image | 4.88 (4.78–4.98) / 7.52 (7.27–7.90) / 8.57 (8.16–8.81) (0.00%) | 1.02 (1.01–1.04) / 1.23 (1.21–1.37) / 1.99 (1.58–2.21) (0.00%) — 4.8× faster |
| home:latest-movies | 11.6 (11.5–12.8) / 16.3 (15.7–18.5) / 19.9 (18.2–20.4) (0.00%) | 11.5 (11.5–12.0) / 13.6 (13.2–14.3) / 14.1 (13.3–14.8) (0.00%) — about the same |
| home:latest-music | 7.46 (7.08–8.78) / 9.94 (8.27–13.8) / 12.0 (10.0–17.7) (0.00%) | 5.34 (4.64–5.41) / 8.68 (6.84–10.5) / 12.1 (8.85–13.4) (0.00%) — 1.4× faster |
| home:latest-shows | 22.7 (21.9–25.1) / 24.6 (23.7–30.6) / 26.3 (24.5–35.1) (0.00%) | 11.2 (10.9–12.0) / 13.5 (13.0–15.2) / 17.9 (16.3–18.7) (0.00%) — 2.0× faster ⚠[1] |
| home:nextup | 26.6 (25.6–29.0) / 33.0 (27.5–34.7) / 37.8 (30.2–41.1) (0.00%) | 15.4 (15.2–16.1) / 17.7 (16.6–18.0) / 18.3 (17.3–18.5) (0.00%) — 1.7× faster |
| home:resume-audio | 19.5 (19.2–20.5) / 26.0 (22.4–27.0) / 28.1 (25.4–30.0) (0.00%) | 7.38 (7.33–7.55) / 8.19 (7.98–9.14) / 8.47 (8.14–11.6) (0.00%) — 2.6× faster |
| home:resume-book | 19.0 (19.0–19.9) / 24.6 (22.9–26.5) / 26.9 (25.6–27.1) (0.00%) | 2.91 (2.91–2.93) / 4.15 (3.31–5.67) / 5.87 (4.21–14.6) (0.00%) — 6.5× faster |
| home:resume-video | 21.8 (21.4–23.7) / 26.8 (25.1–29.6) / 29.5 (27.5–32.5) (0.00%) | 9.63 (9.29–9.96) / 11.3 (11.1–11.3) / 11.7 (11.6–12.7) (0.00%) — 2.3× faster |
| home:views | 20.7 (20.3–23.2) / 25.6 (24.0–30.5) / 28.8 (26.6–31.9) (0.00%) | 2.78 (2.77–2.79) / 3.86 (3.58–4.06) / 8.91 (4.14–9.21) (0.00%) — 7.4× faster ⚠[5] |
| movies:image | 6.19 (5.48–6.53) / 8.01 (7.70–16.7) / 8.43 (8.36–18.7) (0.00%) | 1.08 (1.05–1.08) / 1.29 (1.27–1.38) / 1.33 (1.32–1.52) (0.00%) — 5.7× faster |
| movies:items | 26.9 (25.0–27.2) / 32.2 (29.8–32.5) / 37.9 (37.0–38.1) (0.00%) | 12.4 (12.4–13.5) / 14.5 (14.2–15.6) / 17.4 (15.2–17.7) (0.00%) — 2.2× faster |
| detail:image | 5.00 (4.67–5.61) / 7.89 (7.29–10.6) / 9.15 (8.13–15.0) (0.00%) | 0.97 (0.96–0.98) / 1.18 (1.15–1.26) / 2.75 (1.56–3.57) (0.00%) — 5.2× faster ⚠[2] |
| detail:item | 11.9 (11.7–15.0) / 14.6 (14.0–24.2) / 14.7 (14.4–24.5) (0.00%) | 2.50 (2.42–2.54) / 2.89 (2.77–2.99) / 3.06 (2.83–3.29) (0.00%) — 4.8× faster ⚠[6] |
| detail:local-trailers | 8.16 (7.95–8.35) / 10.3 (10.2–10.4) / 11.0 (10.5–11.0) (0.00%) | 1.67 (1.66–1.69) / 1.99 (1.93–2.04) / 2.13 (2.11–2.37) (0.00%) — 4.9× faster |
| detail:similar | 22.7 (22.0–26.2) / 25.8 (24.3–34.9) / 26.6 (24.4–37.0) (0.00%) | 5.69 (5.65–5.89) / 6.85 (6.64–7.43) / 7.02 (6.96–8.00) (0.00%) — 4.0× faster ⚠[7] |
| detail:special-features | 8.17 (7.96–8.21) / 10.2 (9.89–10.5) / 10.5 (10.1–10.8) (0.00%) | 1.68 (1.66–1.71) / 2.04 (1.99–2.05) / 2.33 (2.13–2.82) (0.00%) — 4.9× faster |
| series:episodes | 6.04 (5.88–6.57) / 6.96 (6.71–9.21) / 7.42 (6.89–9.45) (0.00%) | 2.14 (2.07–2.45) / 2.48 (2.19–2.72) / 2.57 (2.21–2.85) (0.00%) — 2.8× faster |
| series:image | 3.62 (3.46–4.30) / 5.31 (3.65–7.21) / 5.77 (3.69–7.22) (0.00%) | 0.70 (0.67–0.80) / 0.79 (0.79–0.92) / 0.80 (0.79–0.94) (0.00%) — 5.2× faster |
| series:item | 11.4 (10.9–13.5) / 13.8 (13.6–152) / 14.3 (13.7–279) (0.00%) | 20.7 (20.4–23.9) / 23.0 (23.0–25.0) / 24.0 (23.9–25.2) (0.00%) — 1.8× slower |
| series:seasons | 8.62 (8.15–8.64) / 11.0 (10.5–15.4) / 11.5 (10.5–16.9) (0.00%) | 2.33 (2.23–2.35) / 2.50 (2.48–2.53) / 2.50 (2.49–2.63) (0.00%) — 3.7× faster |
| series:similar | 14.8 (14.0–14.9) / 18.1 (16.7–83.4) / 18.3 (16.7–149) (0.00%) | 4.51 (4.48–4.79) / 5.00 (4.73–5.13) / 5.12 (4.76–5.21) (0.00%) — 3.3× faster ⚠[3] |
| search:artists | 11.4 (11.0–12.8) / 161 (24.9–161) / 281 (31.4–281) (0.00%) | 2.89 (2.73–2.94) / 3.41 (3.15–4.13) / 3.55 (3.18–5.12) (0.00%) — 3.9× faster ⚠[4] |
| search:image | 5.72 (5.58–5.87) / 8.06 (7.62–9.82) / 9.11 (8.44–10.8) (0.00%) | 1.12 (1.08–1.22) / 1.32 (1.30–1.42) / 1.36 (1.35–1.58) (0.00%) — 5.1× faster |
| search:items | 55.9 (55.5–60.3) / 87.6 (81.1–134) / 88.8 (85.8–184) (0.00%) | 22.0 (21.2–23.3) / 24.9 (23.0–26.2) / 25.1 (23.2–26.2) (0.00%) — 2.5× faster ⚠[8] |
| search:persons | 17.7 (17.1–18.4) / 21.8 (21.4–24.9) / 25.0 (24.8–26.5) (0.00%) | 2.25 (2.16–2.26) / 2.44 (2.35–2.52) / 2.44 (2.38–2.59) (0.00%) — 7.9× faster ⚠[9] |
| search:programs | 15.4 (14.8–15.5) / 20.1 (19.1–87.3) / 23.2 (20.2–149) (0.00%) | 2.04 (2.01–2.05) / 2.27 (2.10–2.27) / 2.30 (2.13–2.34) (0.00%) — 7.5× faster |
| search:videos | 34.2 (33.5–36.4) / 39.8 (38.8–40.2) / 42.2 (40.5–42.6) (0.00%) | 6.64 (6.51–6.65) / 6.94 (6.89–7.06) / 6.95 (6.91–7.08) (0.00%) — 5.2× faster |
| playback:intros | 4.96 (4.93–5.03) / 6.92 (6.04–8.07) / 7.02 (6.99–8.13) (0.00%) | 6.58 (6.50–6.81) / 6.85 (6.71–7.07) / 6.85 (6.71–7.13) (0.00%) — 1.3× slower |
| playback:playbackinfo | 9.27 (8.58–9.28) / 11.2 (10.5–12.0) / 12.4 (11.4–12.4) (0.00%) | 2.00 (1.94–2.01) / 2.30 (2.17–2.46) / 2.36 (2.19–2.65) (0.00%) — 4.6× faster |
| playback:playing | 9.95 (9.68–10.2) / 13.0 (11.2–17.3) / 13.0 (11.6–22.4) (0.00%) | 0.90 (0.88–0.95) / 1.06 (0.99–1.13) / 1.13 (1.00–1.14) (0.00%) — 11.1× faster |
| playback:segments | 4.98 (4.91–5.25) / 6.96 (6.05–8.25) / 7.02 (7.00–8.25) (0.00%) | 7.03 (6.93–7.39) / 7.36 (7.23–7.59) / 7.44 (7.26–7.64) (0.00%) — 1.4× slower |
| playback:stopped | 7.10 (6.84–7.23) / 7.54 (7.11–10.3) / 7.54 (7.13–10.9) (0.00%) | 0.72 (0.71–0.77) / 0.80 (0.78–0.88) / 0.83 (0.80–0.94) (0.00%) — 9.9× faster |
| image | 5.06 (4.85–5.47) / 7.92 (7.52–14.5) / 8.83 (8.25–16.8) (0.00%) | 1.01 / 1.28 (1.26–1.38) / 1.80 (1.67–1.99) (0.00%) — 5.0× faster ⚠[10] |

### Image response bytes — unloaded (diagnostic)

| Run | Server | Bytes |
|---|---|---|
| 1 | Jellyfin 12.2.0 | 8233749 |
| 1 | Ferrofin | 7656791 |
| 2 | Jellyfin 12.2.0 | 8155892 |
| 2 | Ferrofin | 7584846 |
| 3 | Jellyfin 12.2.0 | 8233749 |
| 3 | Ferrofin | 7584846 |

### Latency — loaded (5 screens/s)

| screen | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home | 46 (45–48) / 51 (50–59) / 60 (54–68) (0.00%) | 19 (19–25) / 23 (22–92) / 27 (24–120) (0.00%) — 2.4× faster ⚠[1] |
| movies | 36 (36–38) / 41 (39–43) / 47 (40–48) (0.00%) | 16 (16–19) / 18 (18–42) / 20 (20–59) (0.00%) — 2.2× faster |
| detail | 32 (32–36) / 37 (34–44) / 41 (37–49) (0.00%) | 9 (9–10) / 10 (10–32) / 12 (12–62) (0.00%) — 3.6× faster ⚠[2] |
| series | 24 (24–26) / 27 (26–33) / 28 (27–37) (0.00%) | 24 (24–29) / 29 (26–75) / 31 (27–86) (0.00%) — about the same ⚠[3] |
| search | 59 (58–61) / 68 (65–80) / 81 (78–93) (0.00%) | 27 (26–32) / 37 (34–96) / 40 (40–113) (0.00%) — 2.2× faster ⚠[4] |
| playback | 29 (29–31) / 33 (31–47) / 57 (55–61) (0.00%) | 11 (11–13) / 13 (12–52) / 33 (29–61) (0.00%) — 2.6× faster |

| endpoint | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home:image | 4.78 (4.74–4.91) / 6.52 (6.46–7.25) / 7.50 (7.43–9.01) (0.00%) | 1.02 (1.00–1.19) / 1.26 (1.19–3.92) / 2.17 (1.85–6.50) (0.00%) — 4.7× faster |
| home:latest-movies | 11.3 (11.2–12.1) / 14.1 (13.9–17.7) / 16.2 (14.9–21.0) (0.00%) | 12.1 (12.0–14.7) / 14.2 (14.1–60.8) / 15.0 (14.9–73.7) (0.00%) — 1.1× slower |
| home:latest-music | 7.09 (7.00–8.25) / 9.87 (9.49–12.4) / 11.1 (11.0–14.0) (0.00%) | 4.68 (4.63–9.05) / 9.66 (9.59–26.9) / 11.1 (11.0–49.7) (0.00%) — 1.5× faster |
| home:latest-shows | 22.3 (21.8–23.6) / 26.1 (24.7–30.4) / 34.5 (28.5–35.9) (0.00%) | 11.1 (11.1–15.9) / 15.0 (14.1–57.8) / 17.5 (16.9–80.6) (0.00%) — 2.0× faster ⚠[1] |
| home:nextup | 25.4 (25.3–27.3) / 28.7 (27.7–36.1) / 32.0 (28.9–38.5) (0.00%) | 15.7 (15.7–18.7) / 17.8 (17.2–76.3) / 19.1 (18.2–99.2) (0.00%) — 1.6× faster |
| home:resume-audio | 19.2 (19.0–19.7) / 22.5 (21.7–25.5) / 26.2 (23.6–30.3) (0.00%) | 7.46 (7.34–8.50) / 8.37 (8.07–34.6) / 9.20 (8.53–44.9) (0.00%) — 2.6× faster |
| home:resume-book | 19.2 (19.2–19.5) / 22.5 (21.5–26.5) / 24.3 (22.6–28.9) (0.00%) | 2.95 (2.91–3.41) / 3.87 (3.76–11.2) / 5.72 (4.69–18.0) (0.00%) — 6.5× faster |
| home:resume-video | 21.3 (21.1–21.8) / 24.5 (23.4–28.5) / 25.4 (24.8–34.0) (0.00%) | 9.53 (9.52–11.6) / 11.1 (11.0–41.0) / 12.3 (11.9–65.4) (0.00%) — 2.2× faster |
| home:views | 20.6 (20.2–21.4) / 23.7 (22.8–28.6) / 25.8 (23.6–32.8) (0.00%) | 2.81 (2.78–3.85) / 5.54 (4.44–21.2) / 6.36 (5.71–36.7) (0.00%) — 7.3× faster ⚠[5] |
| movies:image | 4.89 (4.83–5.02) / 6.60 (6.51–7.49) / 8.10 (7.08–9.29) (0.00%) | 1.07 (1.07–1.28) / 1.35 (1.33–3.55) / 2.36 (2.30–6.87) (0.00%) — 4.6× faster |
| movies:items | 24.4 (24.4–25.7) / 28.7 (26.7–29.6) / 30.5 (27.4–31.2) (0.00%) | 12.4 (12.3–14.6) / 13.8 (13.5–32.1) / 14.1 (14.0–44.4) (0.00%) — 2.0× faster |
| detail:image | 4.56 (4.54–4.79) / 6.62 (6.54–7.42) / 7.81 (7.48–8.64) (0.00%) | 0.97 (0.97–1.16) / 1.17 (1.16–5.07) / 1.68 (1.47–10.5) (0.00%) — 4.7× faster ⚠[2] |
| detail:item | 11.0 (11.0–12.2) / 13.7 (12.9–16.2) / 16.3 (13.5–23.4) (0.00%) | 2.39 (2.38–2.79) / 2.85 (2.68–10.1) / 3.41 (2.88–11.8) (0.00%) — 4.6× faster ⚠[6] |
| detail:local-trailers | 7.14 (7.01–7.74) / 8.81 (8.78–9.94) / 9.78 (9.39–11.0) (0.00%) | 1.59 (1.54–1.91) / 1.97 (1.87–6.42) / 2.10 (2.07–8.76) (0.00%) — 4.5× faster |
| detail:similar | 21.4 (21.2–23.1) / 23.7 (23.1–27.4) / 25.9 (23.6–32.9) (0.00%) | 5.77 (5.71–6.81) / 6.85 (6.61–20.6) / 8.19 (6.98–34.6) (0.00%) — 3.7× faster ⚠[7] |
| detail:special-features | 7.15 (6.99–7.72) / 8.82 (8.70–9.97) / 9.78 (9.06–10.6) (0.00%) | 1.59 (1.58–1.93) / 1.96 (1.91–7.82) / 2.11 (2.00–8.96) (0.00%) — 4.5× faster |
| series:episodes | 5.90 (5.81–6.22) / 7.13 (6.72–7.94) / 8.03 (7.25–9.63) (0.00%) | 2.07 (2.06–2.76) / 2.70 (2.49–8.69) / 3.02 (2.61–12.8) (0.00%) — 2.9× faster |
| series:image | 3.52 (3.52–4.11) / 4.08 (3.99–5.98) / 5.16 (4.44–7.22) (0.00%) | 0.70 (0.70–0.98) / 0.98 (0.87–2.77) / 1.51 (1.07–5.40) (0.00%) — 5.0× faster |
| series:item | 10.6 (10.5–11.4) / 12.4 (11.6–17.4) / 13.7 (12.3–22.4) (0.00%) | 20.6 (20.5–25.1) / 25.0 (22.7–63.6) / 26.0 (23.7–67.9) (0.00%) — 1.9× slower |
| series:seasons | 7.71 (7.67–8.09) / 9.19 (8.86–10.4) / 9.96 (9.12–11.1) (0.00%) | 2.26 (2.22–2.57) / 2.54 (2.54–8.80) / 3.06 (2.61–10.3) (0.00%) — 3.4× faster |
| series:similar | 13.6 (13.6–14.3) / 15.3 (14.7–18.5) / 16.7 (15.1–21.1) (0.00%) | 4.49 (4.47–5.20) / 5.22 (4.82–21.7) / 6.51 (5.09–25.9) (0.00%) — 3.0× faster ⚠[3] |
| search:artists | 11.0 (10.8–12.1) / 13.1 (12.9–17.6) / 14.5 (13.1–23.5) (0.00%) | 2.83 (2.80–3.47) / 4.82 (3.52–11.2) / 5.39 (4.26–17.4) (0.00%) — 3.9× faster ⚠[4] |
| search:image | 5.05 (5.03–5.09) / 7.35 (7.12–8.24) / 8.63 (8.44–9.54) (0.00%) | 1.09 (1.07–1.39) / 1.51 (1.47–3.29) / 2.62 (1.65–5.66) (0.00%) — 4.6× faster |
| search:items | 44.7 (43.4–45.9) / 51.0 (50.6–60.7) / 63.5 (63.4–73.6) (0.00%) | 21.7 (21.2–25.6) / 29.4 (24.9–80.0) / 32.3 (31.6–94.7) (0.00%) — 2.1× faster ⚠[8] |
| search:persons | 18.0 (17.4–18.6) / 20.8 (19.0–27.7) / 23.6 (19.8–31.5) (0.00%) | 2.16 (2.13–2.44) / 2.87 (2.38–6.24) / 4.12 (2.72–10.3) (0.00%) — 8.3× faster ⚠[9] |
| search:programs | 15.2 (14.6–15.7) / 17.6 (16.6–22.8) / 18.5 (16.9–23.6) (0.00%) | 1.97 (1.96–2.30) / 2.47 (2.41–6.70) / 3.98 (2.97–15.9) (0.00%) — 7.7× faster |
| search:videos | 34.9 (33.1–35.9) / 37.8 (35.2–47.4) / 40.2 (35.9–52.6) (0.00%) | 6.50 (6.39–7.38) / 7.20 (7.07–22.5) / 9.79 (8.56–30.5) (0.00%) — 5.4× faster |
| playback:intros | 4.92 (4.85–5.05) / 6.47 (5.14–8.04) / 6.91 (5.71–8.84) (0.00%) | 6.62 (6.52–7.36) / 7.03 (6.85–21.3) / 7.99 (7.12–27.7) (0.00%) — 1.3× slower |
| playback:playbackinfo | 7.95 (7.90–8.21) / 9.38 (8.56–11.0) / 10.3 (9.05–13.2) (0.00%) | 1.97 (1.91–2.11) / 2.29 (2.15–6.65) / 4.92 (4.84–9.23) (0.00%) — 4.0× faster |
| playback:playing | 8.84 (8.63–9.30) / 10.1 (9.85–13.1) / 11.6 (10.3–14.6) (0.00%) | 0.85 (0.85–1.11) / 1.45 (1.00–6.48) / 21.6 (18.9–23.9) (0.00%) — 10.4× faster |
| playback:segments | 4.95 (4.90–5.13) / 5.41 (5.18–7.18) / 7.00 (6.40–8.24) (0.00%) | 7.10 (6.98–7.93) / 7.47 (7.40–23.1) / 8.50 (7.79–36.7) (0.00%) — 1.4× slower |
| playback:stopped | 6.61 (6.57–6.83) / 8.83 (7.89–11.0) / 34.3 (33.5–39.0) (0.00%) | 0.71 (0.70–0.91) / 0.91 (0.85–5.61) / 1.80 (0.98–10.7) (0.00%) — 9.3× faster |
| image | 4.77 (4.71–4.93) / 6.64 (6.57–7.44) / 8.07 (7.62–9.22) (0.00%) | 1.01 (0.99–1.21) / 1.32 (1.31–4.11) / 2.31 (1.65–7.63) (0.00%) — 4.7× faster ⚠[10] |

### Image response bytes — loaded (diagnostic)

| Run | Server | Bytes |
|---|---|---|
| 1 | Jellyfin 12.2.0 | 40870671 |
| 1 | Ferrofin | 38031704 |
| 2 | Jellyfin 12.2.0 | 40948528 |
| 2 | Ferrofin | 38031704 |
| 3 | Jellyfin 12.2.0 | 40870671 |
| 3 | Ferrofin | 38031704 |

### Latency — stress (25 screens/s)

| screen | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home | 45 (44–46) / 49 (49–62) / 51 (51–78) (0.00%) | 19 (19–20) / 22 (22–26) / 25 (25–30) (0.00%) — 2.4× faster ⚠[1] |
| movies | 36 (36–37) / 40 (40–55) / 43 (42–66) (0.00%) | 14 (14–15) / 18 (18–19) / 20 (19–23) (0.00%) — 2.6× faster |
| detail | 32 (32–33) / 35 (34–47) / 38 (37–70) (0.00%) | 8 / 9 (9–11) / 10 (10–18) (0.00%) — 4.0× faster ⚠[2] |
| series | 25 (25–26) / 29 (28–39) / 30 (29–55) (0.00%) | 23 (23–24) / 29 (26–29) / 31 (28–32) (0.00%) — 1.1× faster ⚠[3] |
| search | 57 (57–59) / 73 (69–81) / 85 (83–108) (0.00%) | 26 (25–27) / 33 (31–38) / 41 (39–53) (0.00%) — 2.2× faster ⚠[4] |
| playback | 32 (32–33) / 38 (37–53) / 64 (63–74) (0.00%) | 10 (10–11) / 15 (13–19) / 33 (31–39) (0.00%) — 3.2× faster |

| endpoint | Jellyfin 12.2.0 p50 / p95 / p99 ms (err) | Ferrofin p50 / p95 / p99 ms (err) |
|---|---|---|
| home:image | 4.83 (4.82–4.91) / 6.91 (6.64–7.49) / 7.74 (7.31–10.7) (0.00%) | 1.00 (1.00–1.03) / 1.23 (1.22–1.37) / 2.09 (1.99–2.13) (0.00%) — 4.8× faster |
| home:latest-movies | 10.4 (10.3–10.7) / 13.2 (13.0–16.8) / 14.4 (13.9–24.5) (0.00%) | 11.2 (11.1–12.0) / 13.7 (13.5–15.4) / 15.0 (14.6–17.5) (0.00%) — 1.1× slower |
| home:latest-music | 7.23 (7.20–7.46) / 9.92 (9.49–11.1) / 11.1 (10.8–15.4) (0.00%) | 4.71 (4.59–5.94) / 9.90 (9.84–11.6) / 11.7 (11.4–13.5) (0.00%) — 1.5× faster |
| home:latest-shows | 22.0 (22.0–22.6) / 24.8 (24.4–29.3) / 26.8 (26.4–39.3) (0.00%) | 10.9 (10.9–12.1) / 14.8 (14.4–18.4) / 17.8 (17.1–21.4) (0.00%) — 2.0× faster ⚠[1] |
| home:nextup | 24.6 (24.5–25.4) / 27.4 (27.3–34.4) / 29.4 (28.9–41.8) (0.00%) | 15.1 (15.1–16.2) / 17.5 (17.1–19.0) / 18.6 (18.1–21.8) (0.00%) — 1.6× faster |
| home:resume-audio | 17.9 (17.7–18.2) / 21.4 (21.0–23.9) / 22.9 (22.6–34.3) (0.00%) | 6.97 (6.92–7.23) / 8.12 (7.79–9.18) / 9.16 (8.74–10.8) (0.00%) — 2.6× faster |
| home:resume-book | 17.8 (17.7–18.2) / 21.2 (21.1–24.2) / 22.9 (22.7–31.8) (0.00%) | 2.55 (2.52–2.68) / 3.68 (3.36–4.20) / 4.79 (4.57–5.56) (0.00%) — 7.0× faster |
| home:resume-video | 19.9 (19.7–20.4) / 23.4 (23.1–26.9) / 24.6 (24.5–35.7) (0.00%) | 8.98 (8.97–9.55) / 11.1 (10.6–12.2) / 12.1 (11.7–13.8) (0.00%) — 2.2× faster |
| home:views | 18.9 (18.9–19.8) / 22.2 (22.0–27.2) / 23.8 (23.5–36.2) (0.00%) | 2.42 (2.39–2.63) / 4.75 (4.61–6.14) / 6.98 (6.65–8.07) (0.00%) — 7.8× faster ⚠[5] |
| movies:image | 4.73 (4.72–4.91) / 6.63 (6.51–7.52) / 7.47 (7.08–12.6) (0.00%) | 1.00 (0.99–1.03) / 1.30 (1.29–1.41) / 2.20 (1.64–2.21) (0.00%) — 4.7× faster |
| movies:items | 25.1 (24.9–26.0) / 28.4 (28.2–34.4) / 30.1 (29.7–45.5) (0.00%) | 11.2 (11.2–11.9) / 14.1 (13.9–14.5) / 15.5 (15.1–16.9) (0.00%) — 2.2× faster |
| detail:image | 4.63 (4.63–4.78) / 6.74 (6.38–7.77) / 7.93 (7.43–15.1) (0.00%) | 0.94 (0.93–0.96) / 1.13 (1.11–1.28) / 1.31 (1.29–2.10) (0.00%) — 4.9× faster ⚠[2] |
| detail:item | 10.2 (10.1–10.5) / 12.5 (12.1–17.0) / 13.2 (13.0–24.3) (0.00%) | 1.92 (1.92–2.07) / 2.48 (2.47–2.83) / 2.91 (2.85–4.04) (0.00%) — 5.3× faster ⚠[6] |
| detail:local-trailers | 6.16 (6.14–6.35) / 8.14 (8.00–11.7) / 8.86 (8.84–17.6) (0.00%) | 1.19 (1.18–1.28) / 1.63 (1.60–1.82) / 1.87 (1.83–2.28) (0.00%) — 5.2× faster |
| detail:similar | 20.9 (20.7–21.7) / 23.1 (23.0–29.6) / 24.8 (24.2–38.1) (0.00%) | 5.01 (4.96–5.40) / 6.29 (6.15–6.87) / 6.97 (6.57–7.70) (0.00%) — 4.2× faster ⚠[7] |
| detail:special-features | 6.15 (6.13–6.35) / 8.14 (7.91–10.9) / 8.82 (8.71–17.7) (0.00%) | 1.19 (1.18–1.29) / 1.62 (1.59–1.85) / 1.93 (1.84–2.34) (0.00%) — 5.2× faster |
| series:episodes | 5.99 (5.97–6.13) / 7.21 (7.00–9.40) / 8.05 (7.80–12.0) (0.00%) | 2.05 (1.99–2.12) / 2.73 (2.47–2.80) / 3.04 (2.57–3.09) (0.00%) — 2.9× faster |
| series:image | 3.56 (3.54–3.68) / 4.93 (4.88–5.73) / 5.97 (5.93–7.86) (0.00%) | 0.71 (0.69–0.75) / 0.94 (0.87–1.00) / 1.08 (1.00–1.33) (0.00%) — 5.0× faster |
| series:item | 11.4 (11.2–11.9) / 14.1 (13.2–19.0) / 15.9 (14.5–33.7) (0.00%) | 19.8 (19.8–20.9) / 24.5 (22.9–25.0) / 25.7 (24.2–27.8) (0.00%) — 1.7× slower |
| series:seasons | 8.52 (8.23–8.91) / 11.3 (10.3–14.0) / 12.1 (11.5–24.1) (0.00%) | 1.67 (1.64–1.71) / 2.39 (2.11–2.44) / 2.64 (2.38–2.97) (0.00%) — 5.1× faster |
| series:similar | 15.1 (14.6–15.5) / 17.8 (17.0–23.5) / 18.3 (17.7–38.7) (0.00%) | 3.85 (3.79–4.06) / 4.78 (4.68–5.13) / 6.47 (6.09–7.60) (0.00%) — 3.9× faster ⚠[3] |
| search:artists | 10.1 (10.1–10.6) / 12.6 (12.5–15.1) / 13.9 (13.7–26.6) (0.00%) | 2.34 (2.30–2.52) / 3.32 (3.09–4.40) / 4.71 (4.33–5.87) (0.00%) — 4.3× faster ⚠[4] |
| search:image | 5.11 (5.07–5.17) / 7.58 (7.39–8.53) / 9.03 (8.45–13.3) (0.00%) | 1.08 (1.07–1.11) / 1.49 (1.40–1.69) / 2.19 (2.13–3.89) (0.00%) — 4.7× faster |
| search:items | 42.8 (42.7–44.3) / 54.4 (54.1–65.1) / 67.6 (65.6–74.5) (0.00%) | 20.8 (20.3–21.6) / 26.5 (26.1–30.9) / 31.7 (31.4–35.1) (0.00%) — 2.1× faster ⚠[8] |
| search:persons | 16.8 (16.7–17.3) / 19.5 (18.8–23.0) / 20.7 (20.5–33.2) (0.00%) | 1.80 (1.77–1.87) / 2.24 (2.13–2.68) / 4.22 (2.55–4.54) (0.00%) — 9.3× faster ⚠[9] |
| search:programs | 14.1 (14.1–14.7) / 16.7 (16.7–19.7) / 18.6 (18.0–31.1) (0.00%) | 1.55 (1.51–1.61) / 2.10 (1.93–2.59) / 2.79 (2.47–4.50) (0.00%) — 9.1× faster |
| search:videos | 32.9 (32.8–34.3) / 36.0 (36.0–42.1) / 37.8 (37.7–55.1) (0.00%) | 5.67 (5.47–6.03) / 6.81 (6.49–7.52) / 7.60 (7.03–8.66) (0.00%) — 5.8× faster |
| playback:intros | 6.04 (6.04–6.27) / 8.75 (8.69–9.74) / 10.5 (9.79–11.7) (0.00%) | 6.42 (6.38–6.51) / 7.04 (6.77–7.53) / 8.22 (7.23–8.37) (0.00%) — 1.1× slower |
| playback:playbackinfo | 8.20 (8.13–8.32) / 11.1 (10.9–12.2) / 12.5 (11.9–14.1) (0.00%) | 1.30 (1.27–1.30) / 1.84 (1.59–1.89) / 2.04 (1.80–2.76) (0.00%) — 6.3× faster |
| playback:playing | 9.05 (8.88–9.56) / 12.4 (11.6–16.0) / 32.8 (31.5–33.4) (0.00%) | 0.80 (0.79–0.85) / 1.24 (0.99–1.52) / 20.8 (20.4–23.4) (0.00%) — 11.3× faster |
| playback:segments | 6.09 (6.04–6.34) / 9.00 (8.77–9.73) / 10.7 (10.2–12.3) (0.00%) | 6.88 (6.85–7.02) / 7.62 (7.24–8.13) / 8.87 (7.77–8.92) (0.00%) — 1.1× slower |
| playback:stopped | 6.74 (6.69–6.84) / 9.95 (8.92–14.5) / 23.3 (19.3–29.6) (0.00%) | 0.68 (0.67–0.72) / 0.96 (0.82–1.32) / 10.4 (1.41–22.7) (0.00%) — 9.9× faster |
| image | 4.78 (4.78–4.89) / 6.90 (6.65–7.68) / 7.99 (7.57–12.4) (0.00%) | 0.97 (0.97–1.00) / 1.29 (1.28–1.42) / 1.98 (1.80–2.29) (0.00%) — 4.9× faster ⚠[10] |

### Image response bytes — stress (diagnostic)

| Run | Server | Bytes |
|---|---|---|
| 1 | Jellyfin 12.2.0 | 204431212 |
| 1 | Ferrofin | 189870740 |
| 2 | Jellyfin 12.2.0 | 204431212 |
| 2 | Ferrofin | 189870740 |
| 3 | Jellyfin 12.2.0 | 204353355 |
| 3 | Ferrofin | 189870740 |

### Shape coverage

| Run | Server | Slots | Requests | Distinct requests | Duration (s) | Distinct picks | Evidence |
|---|---|---|---|---|---|---|---|
| 1 | Jellyfin 12.2.0 | 600 | 8732 | 2913 | 40.128375136 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 1 | Ferrofin | 600 | 8732 | 2875 | 14.627211545 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 2 | Jellyfin 12.2.0 | 600 | 8732 | 2913 | 31.595709259 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 2 | Ferrofin | 600 | 8732 | 2875 | 14.416083819999999 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 3 | Jellyfin 12.2.0 | 600 | 8732 | 2913 | 31.255376644 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |
| 3 | Ferrofin | 600 | 8732 | 2875 | 14.368297839 | detail:item: 120; movies:items: 31; playback:playbackinfo: 60; search:items: 40; series:item: 59 | available |

### CPU and sample coverage — invocation and idle windows

CPU seconds use cumulative counters at the reported bracketing samples; load includes setup/teardown and harness overhead, and steady is post-load idle. Missing historical counters remain unavailable.

Average cores busy is CPU-seconds divided by observed wall seconds; 1.0 means one core fully occupied. Capacity busy divides that by the allocated logical CPU count. CPU milliseconds per completed screen divides total window CPU by completed mixed-screen iterations, including failed screens; it is not per-endpoint attribution. Lower values mean less CPU work only when completed work and validation agree. Each row retains its individual run; flagged workloads cannot support CPU savings claims. See the latency/work flags for differences in executed work.

#### CPU consumption (CPU-seconds)

| Window / metric | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 16.773 | 5.029 |
| loaded · run 1 | 72.282 | 30.190 |
| stress · run 1 | 345.764 | 122.515 |
| steady · run 1 | 0.043 | 0.006 |
| unloaded · run 2 | 15.731 | 5.050 |
| loaded · run 2 | 67.903 | 24.670 |
| stress · run 2 | 329.723 | 118.388 |
| steady · run 2 | 0.049 | 0.005 |
| unloaded · run 3 | 15.407 | 4.910 |
| loaded · run 3 | 68.771 | 24.984 |
| stress · run 3 | 330.324 | 118.816 |
| steady · run 3 | 0.044 | 0.006 |

#### Average CPU cores busy

| Window / metric | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 0.139 | 0.042 |
| loaded · run 1 | 0.601 | 0.250 |
| stress · run 1 | 2.872 | 1.017 |
| steady · run 1 | 0.001 | 0.000 |
| unloaded · run 2 | 0.131 | 0.042 |
| loaded · run 2 | 0.564 | 0.205 |
| stress · run 2 | 2.739 | 0.983 |
| steady · run 2 | 0.001 | 0.000 |
| unloaded · run 3 | 0.128 | 0.041 |
| loaded · run 3 | 0.572 | 0.208 |
| stress · run 3 | 2.746 | 0.987 |
| steady · run 3 | 0.001 | 0.000 |

#### Allocated CPU capacity busy (%)

| Window / metric | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 1.74% | 0.52% |
| loaded · run 1 | 7.51% | 3.13% |
| stress · run 1 | 35.90% | 12.71% |
| steady · run 1 | 0.01% | 0.00% |
| unloaded · run 2 | 1.63% | 0.52% |
| loaded · run 2 | 7.05% | 2.56% |
| stress · run 2 | 34.23% | 12.29% |
| steady · run 2 | 0.01% | 0.00% |
| unloaded · run 3 | 1.60% | 0.51% |
| loaded · run 3 | 7.15% | 2.59% |
| stress · run 3 | 34.32% | 12.34% |
| steady · run 3 | 0.01% | 0.00% |

#### CPU milliseconds per completed screen

| Window / metric | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| unloaded · run 1 | 138.618 | 41.565 ⚠ workload validation failed |
| loaded · run 1 | 120.470 | 50.233 ⚠ workload validation failed |
| stress · run 1 | 115.216 | 40.825 ⚠ workload validation failed |
| unloaded · run 2 | 131.093 | 42.086 ⚠ workload validation failed |
| loaded · run 2 | 112.983 | 41.048 ⚠ workload validation failed |
| stress · run 2 | 109.871 | 39.449 ⚠ workload validation failed |
| unloaded · run 3 | 127.332 | 40.918 ⚠ workload validation failed |
| loaded · run 3 | 114.619 | 41.570 ⚠ workload validation failed |
| stress · run 3 | 110.108 | 39.592 ⚠ workload validation failed |

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
| unloaded · run 1 · Observed duration (s) | 120.500 | 120.300 |
| unloaded · run 1 · Samples | 1206 | 1204 |
| unloaded · run 1 · Largest gap (ms) | 101.534 | 100.397 |
| unloaded · run 1 · Observed start (UTC) | 2026-10-20T00:46:31.612+00:00 | 2026-10-20T01:00:00.327+00:00 |
| unloaded · run 1 · Observed end (UTC) | 2026-10-20T00:48:32.112+00:00 | 2026-10-20T01:02:00.627+00:00 |
| unloaded · run 1 · CPU evidence | available | available |
| loaded · run 1 · Observed duration (s) | 120.300 | 120.600 |
| loaded · run 1 · Samples | 1204 | 1207 |
| loaded · run 1 · Largest gap (ms) | 103.624 | 109.576 |
| loaded · run 1 · Observed start (UTC) | 2026-10-20T00:49:34.012+00:00 | 2026-10-20T01:03:02.227+00:00 |
| loaded · run 1 · Observed end (UTC) | 2026-10-20T00:51:34.312+00:00 | 2026-10-20T01:05:02.827+00:00 |
| loaded · run 1 · CPU evidence | available | available |
| stress · run 1 · Observed duration (s) | 120.400 | 120.500 |
| stress · run 1 · Samples | 1205 | 1206 |
| stress · run 1 · Largest gap (ms) | 103.699 | 102.555 |
| stress · run 1 · Observed start (UTC) | 2026-10-20T00:52:36.612+00:00 | 2026-10-20T01:06:04.327+00:00 |
| stress · run 1 · Observed end (UTC) | 2026-10-20T00:54:37.012+00:00 | 2026-10-20T01:08:04.827+00:00 |
| stress · run 1 · CPU evidence | available | available |
| steady · run 1 · Observed duration (s) | 60.100 | 60.100 |
| steady · run 1 · Samples | 602 | 602 |
| steady · run 1 · Largest gap (ms) | 126.199 | 102.580 |
| steady · run 1 · Observed start (UTC) | 2026-10-20T00:55:07.512+00:00 | 2026-10-20T01:08:35.227+00:00 |
| steady · run 1 · Observed end (UTC) | 2026-10-20T00:56:07.612+00:00 | 2026-10-20T01:09:35.327+00:00 |
| steady · run 1 · CPU evidence | available | available |
| unloaded · run 2 · Observed duration (s) | 120.400 | 120.300 |
| unloaded · run 2 · Samples | 1205 | 1204 |
| unloaded · run 2 · Largest gap (ms) | 100.294 | 100.290 |
| unloaded · run 2 · Observed start (UTC) | 2026-10-20T01:15:58.564+00:00 | 2026-10-20T01:29:20.543+00:00 |
| unloaded · run 2 · Observed end (UTC) | 2026-10-20T01:17:58.964+00:00 | 2026-10-20T01:31:20.843+00:00 |
| unloaded · run 2 · CPU evidence | available | available |
| loaded · run 2 · Observed duration (s) | 120.400 | 120.300 |
| loaded · run 2 · Samples | 1205 | 1204 |
| loaded · run 2 · Largest gap (ms) | 102.277 | 100.610 |
| loaded · run 2 · Observed start (UTC) | 2026-10-20T01:19:00.864+00:00 | 2026-10-20T01:32:22.243+00:00 |
| loaded · run 2 · Observed end (UTC) | 2026-10-20T01:21:01.264+00:00 | 2026-10-20T01:34:22.543+00:00 |
| loaded · run 2 · CPU evidence | available | available |
| stress · run 2 · Observed duration (s) | 120.400 | 120.400 |
| stress · run 2 · Samples | 1205 | 1205 |
| stress · run 2 · Largest gap (ms) | 100.968 | 101.376 |
| stress · run 2 · Observed start (UTC) | 2026-10-20T01:22:03.164+00:00 | 2026-10-20T01:35:23.843+00:00 |
| stress · run 2 · Observed end (UTC) | 2026-10-20T01:24:03.564+00:00 | 2026-10-20T01:37:24.243+00:00 |
| stress · run 2 · CPU evidence | available | available |
| steady · run 2 · Observed duration (s) | 60.100 | 60.200 |
| steady · run 2 · Samples | 602 | 603 |
| steady · run 2 · Largest gap (ms) | 100.219 | 100.167 |
| steady · run 2 · Observed start (UTC) | 2026-10-20T01:24:34.064+00:00 | 2026-10-20T01:37:54.543+00:00 |
| steady · run 2 · Observed end (UTC) | 2026-10-20T01:25:34.164+00:00 | 2026-10-20T01:38:54.743+00:00 |
| steady · run 2 · CPU evidence | available | available |
| unloaded · run 3 · Observed duration (s) | 120.400 | 120.400 |
| unloaded · run 3 · Samples | 1205 | 1205 |
| unloaded · run 3 · Largest gap (ms) | 100.230 | 100.209 |
| unloaded · run 3 · Observed start (UTC) | 2026-10-20T01:43:14.630+00:00 | 2026-10-20T01:56:46.123+00:00 |
| unloaded · run 3 · Observed end (UTC) | 2026-10-20T01:45:15.030+00:00 | 2026-10-20T01:58:46.523+00:00 |
| unloaded · run 3 · CPU evidence | available | available |
| loaded · run 3 · Observed duration (s) | 120.300 | 120.400 |
| loaded · run 3 · Samples | 1204 | 1205 |
| loaded · run 3 · Largest gap (ms) | 102.787 | 101.700 |
| loaded · run 3 · Observed start (UTC) | 2026-10-20T01:46:16.930+00:00 | 2026-10-20T01:59:47.823+00:00 |
| loaded · run 3 · Observed end (UTC) | 2026-10-20T01:48:17.230+00:00 | 2026-10-20T02:01:48.223+00:00 |
| loaded · run 3 · CPU evidence | available | available |
| stress · run 3 · Observed duration (s) | 120.300 | 120.400 |
| stress · run 3 · Samples | 1204 | 1205 |
| stress · run 3 · Largest gap (ms) | 100.256 | 101.560 |
| stress · run 3 · Observed start (UTC) | 2026-10-20T01:49:19.030+00:00 | 2026-10-20T02:02:49.523+00:00 |
| stress · run 3 · Observed end (UTC) | 2026-10-20T01:51:19.330+00:00 | 2026-10-20T02:04:49.923+00:00 |
| stress · run 3 · CPU evidence | available | available |
| steady · run 3 · Observed duration (s) | 60.100 | 60.200 |
| steady · run 3 · Samples | 602 | 603 |
| steady · run 3 · Largest gap (ms) | 102.482 | 100.206 |
| steady · run 3 · Observed start (UTC) | 2026-10-20T01:51:49.830+00:00 | 2026-10-20T02:05:20.223+00:00 |
| steady · run 3 · Observed end (UTC) | 2026-10-20T01:52:49.930+00:00 | 2026-10-20T02:06:20.423+00:00 |
| steady · run 3 · CPU evidence | available | available |

### Phase outcomes

| Run | Server | Phase | Status | Duration (s) | Start (UTC) | End (UTC) | Reason |
|---|---|---|---|---|---|---|---|
| 1 | Jellyfin 12.2.0 | startup | completed | 9.467 | 2026-10-20T00:42:45.563+00:00 | 2026-10-20T00:42:55.029+00:00 | — |
| 1 | Jellyfin 12.2.0 | startup-drain | completed | 30.216 | 2026-10-20T00:42:55.050+00:00 | 2026-10-20T00:43:25.267+00:00 | — |
| 1 | Jellyfin 12.2.0 | cleanup | completed | 0.467 | 2026-10-20T00:56:56.687+00:00 | 2026-10-20T00:56:57.154+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-setup | completed | 1.727 | 2026-10-20T00:43:25.284+00:00 | 2026-10-20T00:43:27.011+00:00 | — |
| 1 | Jellyfin 12.2.0 | counts | completed | 2.286 | 2026-10-20T00:43:27.021+00:00 | 2026-10-20T00:43:29.308+00:00 | — |
| 1 | Jellyfin 12.2.0 | shape | completed | 42.137 | 2026-10-20T00:43:29.712+00:00 | 2026-10-20T00:44:11.849+00:00 | — |
| 1 | Jellyfin 12.2.0 | coldstart | completed | 76.116 | 2026-10-20T00:44:12.515+00:00 | 2026-10-20T00:45:28.632+00:00 | — |
| 1 | Jellyfin 12.2.0 | ttfs | completed | 48.135 | 2026-10-20T00:56:08.249+00:00 | 2026-10-20T00:56:56.384+00:00 | — |
| 1 | Jellyfin 12.2.0 | unloaded | completed | 120.356 | 2026-10-20T00:46:31.650+00:00 | 2026-10-20T00:48:32.006+00:00 | — |
| 1 | Jellyfin 12.2.0 | warmup-unloaded | completed | 30.341 | 2026-10-20T00:46:00.468+00:00 | 2026-10-20T00:46:30.809+00:00 | — |
| 1 | Jellyfin 12.2.0 | drain-unloaded | completed | 30.081 | 2026-10-20T00:45:29.591+00:00 | 2026-10-20T00:45:59.672+00:00 | — |
| 1 | Jellyfin 12.2.0 | sampler | completed | 638.452 | 2026-10-20T00:45:29.480+00:00 | 2026-10-20T00:56:07.932+00:00 | — |
| 1 | Jellyfin 12.2.0 | steady | completed | 60.009 | 2026-10-20T00:55:07.544+00:00 | 2026-10-20T00:56:07.553+00:00 | — |
| 1 | Jellyfin 12.2.0 | drain-steady | completed | 30.061 | 2026-10-20T00:54:37.472+00:00 | 2026-10-20T00:55:07.532+00:00 | — |
| 1 | Jellyfin 12.2.0 | loaded | completed | 120.252 | 2026-10-20T00:49:34.027+00:00 | 2026-10-20T00:51:34.279+00:00 | — |
| 1 | Jellyfin 12.2.0 | warmup-loaded | completed | 30.319 | 2026-10-20T00:49:02.895+00:00 | 2026-10-20T00:49:33.215+00:00 | — |
| 1 | Jellyfin 12.2.0 | drain-loaded | completed | 30.065 | 2026-10-20T00:48:32.522+00:00 | 2026-10-20T00:49:02.587+00:00 | — |
| 1 | Jellyfin 12.2.0 | stress | completed | 120.308 | 2026-10-20T00:52:36.637+00:00 | 2026-10-20T00:54:36.945+00:00 | — |
| 1 | Jellyfin 12.2.0 | warmup-stress | completed | 30.452 | 2026-10-20T00:52:05.213+00:00 | 2026-10-20T00:52:35.665+00:00 | — |
| 1 | Jellyfin 12.2.0 | drain-stress | completed | 30.058 | 2026-10-20T00:51:34.829+00:00 | 2026-10-20T00:52:04.887+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-shape-before | completed | 0.385 | 2026-10-20T00:43:29.317+00:00 | 2026-10-20T00:43:29.701+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-shape-after | completed | 0.319 | 2026-10-20T00:44:11.861+00:00 | 2026-10-20T00:44:12.180+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-coldstart-before | completed | 0.313 | 2026-10-20T00:44:12.191+00:00 | 2026-10-20T00:44:12.504+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-coldstart-after | completed | 0.824 | 2026-10-20T00:45:28.645+00:00 | 2026-10-20T00:45:29.469+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-unloaded-before | completed | 0.752 | 2026-10-20T00:45:59.697+00:00 | 2026-10-20T00:46:00.449+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-unloaded-after | completed | 0.291 | 2026-10-20T00:46:30.821+00:00 | 2026-10-20T00:46:31.112+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-unloaded-before | completed | 0.308 | 2026-10-20T00:46:31.123+00:00 | 2026-10-20T00:46:31.431+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-unloaded-after | completed | 0.280 | 2026-10-20T00:48:32.232+00:00 | 2026-10-20T00:48:32.512+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-loaded-before | completed | 0.289 | 2026-10-20T00:49:02.597+00:00 | 2026-10-20T00:49:02.886+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-loaded-after | completed | 0.281 | 2026-10-20T00:49:33.226+00:00 | 2026-10-20T00:49:33.507+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-loaded-before | completed | 0.293 | 2026-10-20T00:49:33.517+00:00 | 2026-10-20T00:49:33.810+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-loaded-after | completed | 0.312 | 2026-10-20T00:51:34.507+00:00 | 2026-10-20T00:51:34.819+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-stress-before | completed | 0.304 | 2026-10-20T00:52:04.898+00:00 | 2026-10-20T00:52:05.202+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-warmup-stress-after | completed | 0.383 | 2026-10-20T00:52:35.678+00:00 | 2026-10-20T00:52:36.061+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-stress-before | completed | 0.343 | 2026-10-20T00:52:36.073+00:00 | 2026-10-20T00:52:36.416+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-stress-after | completed | 0.293 | 2026-10-20T00:54:37.169+00:00 | 2026-10-20T00:54:37.462+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-ttfs-before | completed | 0.297 | 2026-10-20T00:56:07.942+00:00 | 2026-10-20T00:56:08.239+00:00 | — |
| 1 | Jellyfin 12.2.0 | isolation-ttfs-after | completed | 0.283 | 2026-10-20T00:56:56.394+00:00 | 2026-10-20T00:56:56.677+00:00 | — |
| 1 | Ferrofin | startup | completed | 17.113 | 2026-10-20T00:56:57.383+00:00 | 2026-10-20T00:57:14.496+00:00 | — |
| 1 | Ferrofin | startup-drain | completed | 30.050 | 2026-10-20T00:57:14.505+00:00 | 2026-10-20T00:57:44.555+00:00 | — |
| 1 | Ferrofin | cleanup | completed | 0.431 | 2026-10-20T01:10:23.952+00:00 | 2026-10-20T01:10:24.383+00:00 | — |
| 1 | Ferrofin | isolation-setup | completed | 0.296 | 2026-10-20T00:57:44.572+00:00 | 2026-10-20T00:57:44.868+00:00 | — |
| 1 | Ferrofin | counts | completed | 0.291 | 2026-10-20T00:57:44.878+00:00 | 2026-10-20T00:57:45.169+00:00 | — |
| 1 | Ferrofin | shape | completed | 16.422 | 2026-10-20T00:57:45.339+00:00 | 2026-10-20T00:58:01.760+00:00 | — |
| 1 | Ferrofin | coldstart | completed | 56.795 | 2026-10-20T00:58:02.112+00:00 | 2026-10-20T00:58:58.908+00:00 | — |
| 1 | Ferrofin | ttfs | completed | 47.924 | 2026-10-20T01:09:35.822+00:00 | 2026-10-20T01:10:23.746+00:00 | — |
| 1 | Ferrofin | unloaded | completed | 120.249 | 2026-10-20T01:00:00.350+00:00 | 2026-10-20T01:02:00.600+00:00 | — |
| 1 | Ferrofin | warmup-unloaded | completed | 30.361 | 2026-10-20T00:59:29.430+00:00 | 2026-10-20T00:59:59.791+00:00 | — |
| 1 | Ferrofin | drain-unloaded | completed | 30.051 | 2026-10-20T00:58:59.205+00:00 | 2026-10-20T00:59:29.256+00:00 | — |
| 1 | Ferrofin | sampler | completed | 636.537 | 2026-10-20T00:58:59.094+00:00 | 2026-10-20T01:09:35.631+00:00 | — |
| 1 | Ferrofin | steady | completed | 60.011 | 2026-10-20T01:08:35.230+00:00 | 2026-10-20T01:09:35.241+00:00 | — |
| 1 | Ferrofin | drain-steady | completed | 30.052 | 2026-10-20T01:08:05.167+00:00 | 2026-10-20T01:08:35.219+00:00 | — |
| 1 | Ferrofin | loaded | completed | 120.502 | 2026-10-20T01:03:02.270+00:00 | 2026-10-20T01:05:02.772+00:00 | — |
| 1 | Ferrofin | warmup-loaded | completed | 30.390 | 2026-10-20T01:02:31.270+00:00 | 2026-10-20T01:03:01.660+00:00 | — |
| 1 | Ferrofin | drain-loaded | completed | 30.050 | 2026-10-20T01:02:01.004+00:00 | 2026-10-20T01:02:31.054+00:00 | — |
| 1 | Ferrofin | stress | completed | 120.359 | 2026-10-20T01:06:04.412+00:00 | 2026-10-20T01:08:04.771+00:00 | — |
| 1 | Ferrofin | warmup-stress | completed | 30.267 | 2026-10-20T01:05:33.568+00:00 | 2026-10-20T01:06:03.835+00:00 | — |
| 1 | Ferrofin | drain-stress | completed | 30.061 | 2026-10-20T01:05:03.262+00:00 | 2026-10-20T01:05:33.323+00:00 | — |
| 1 | Ferrofin | isolation-shape-before | completed | 0.151 | 2026-10-20T00:57:45.179+00:00 | 2026-10-20T00:57:45.330+00:00 | — |
| 1 | Ferrofin | isolation-shape-after | completed | 0.164 | 2026-10-20T00:58:01.771+00:00 | 2026-10-20T00:58:01.935+00:00 | — |
| 1 | Ferrofin | isolation-coldstart-before | completed | 0.157 | 2026-10-20T00:58:01.945+00:00 | 2026-10-20T00:58:02.102+00:00 | — |
| 1 | Ferrofin | isolation-coldstart-after | completed | 0.165 | 2026-10-20T00:58:58.918+00:00 | 2026-10-20T00:58:59.083+00:00 | — |
| 1 | Ferrofin | isolation-warmup-unloaded-before | completed | 0.153 | 2026-10-20T00:59:29.267+00:00 | 2026-10-20T00:59:29.420+00:00 | — |
| 1 | Ferrofin | isolation-warmup-unloaded-after | completed | 0.166 | 2026-10-20T00:59:59.801+00:00 | 2026-10-20T00:59:59.967+00:00 | — |
| 1 | Ferrofin | isolation-unloaded-before | completed | 0.155 | 2026-10-20T00:59:59.977+00:00 | 2026-10-20T01:00:00.132+00:00 | — |
| 1 | Ferrofin | isolation-unloaded-after | completed | 0.168 | 2026-10-20T01:02:00.826+00:00 | 2026-10-20T01:02:00.993+00:00 | — |
| 1 | Ferrofin | isolation-warmup-loaded-before | completed | 0.194 | 2026-10-20T01:02:31.065+00:00 | 2026-10-20T01:02:31.259+00:00 | — |
| 1 | Ferrofin | isolation-warmup-loaded-after | completed | 0.192 | 2026-10-20T01:03:01.670+00:00 | 2026-10-20T01:03:01.862+00:00 | — |
| 1 | Ferrofin | isolation-loaded-before | completed | 0.178 | 2026-10-20T01:03:01.873+00:00 | 2026-10-20T01:03:02.051+00:00 | — |
| 1 | Ferrofin | isolation-loaded-after | completed | 0.225 | 2026-10-20T01:05:03.027+00:00 | 2026-10-20T01:05:03.252+00:00 | — |
| 1 | Ferrofin | isolation-warmup-stress-before | completed | 0.182 | 2026-10-20T01:05:33.376+00:00 | 2026-10-20T01:05:33.558+00:00 | — |
| 1 | Ferrofin | isolation-warmup-stress-after | completed | 0.177 | 2026-10-20T01:06:03.845+00:00 | 2026-10-20T01:06:04.023+00:00 | — |
| 1 | Ferrofin | isolation-stress-before | completed | 0.161 | 2026-10-20T01:06:04.033+00:00 | 2026-10-20T01:06:04.194+00:00 | — |
| 1 | Ferrofin | isolation-stress-after | completed | 0.161 | 2026-10-20T01:08:04.996+00:00 | 2026-10-20T01:08:05.158+00:00 | — |
| 1 | Ferrofin | isolation-ttfs-before | completed | 0.172 | 2026-10-20T01:09:35.641+00:00 | 2026-10-20T01:09:35.813+00:00 | — |
| 1 | Ferrofin | isolation-ttfs-after | completed | 0.184 | 2026-10-20T01:10:23.757+00:00 | 2026-10-20T01:10:23.941+00:00 | — |
| 2 | Jellyfin 12.2.0 | startup | completed | 8.915 | 2026-10-20T01:12:29.854+00:00 | 2026-10-20T01:12:38.769+00:00 | — |
| 2 | Jellyfin 12.2.0 | startup-drain | completed | 30.065 | 2026-10-20T01:12:38.778+00:00 | 2026-10-20T01:13:08.843+00:00 | — |
| 2 | Jellyfin 12.2.0 | cleanup | completed | 0.386 | 2026-10-20T01:26:23.202+00:00 | 2026-10-20T01:26:23.588+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-setup | completed | 1.635 | 2026-10-20T01:13:08.859+00:00 | 2026-10-20T01:13:10.494+00:00 | — |
| 2 | Jellyfin 12.2.0 | counts | completed | 2.127 | 2026-10-20T01:13:10.505+00:00 | 2026-10-20T01:13:12.631+00:00 | — |
| 2 | Jellyfin 12.2.0 | shape | completed | 33.386 | 2026-10-20T01:13:12.989+00:00 | 2026-10-20T01:13:46.375+00:00 | — |
| 2 | Jellyfin 12.2.0 | coldstart | completed | 69.380 | 2026-10-20T01:13:46.966+00:00 | 2026-10-20T01:14:56.346+00:00 | — |
| 2 | Jellyfin 12.2.0 | ttfs | completed | 48.104 | 2026-10-20T01:25:34.801+00:00 | 2026-10-20T01:26:22.905+00:00 | — |
| 2 | Jellyfin 12.2.0 | unloaded | completed | 120.322 | 2026-10-20T01:15:58.604+00:00 | 2026-10-20T01:17:58.926+00:00 | — |
| 2 | Jellyfin 12.2.0 | warmup-unloaded | completed | 30.252 | 2026-10-20T01:15:27.544+00:00 | 2026-10-20T01:15:57.796+00:00 | — |
| 2 | Jellyfin 12.2.0 | drain-unloaded | completed | 30.062 | 2026-10-20T01:14:57.146+00:00 | 2026-10-20T01:15:27.207+00:00 | — |
| 2 | Jellyfin 12.2.0 | sampler | completed | 637.446 | 2026-10-20T01:14:57.036+00:00 | 2026-10-20T01:25:34.482+00:00 | — |
| 2 | Jellyfin 12.2.0 | steady | completed | 60.010 | 2026-10-20T01:24:34.094+00:00 | 2026-10-20T01:25:34.103+00:00 | — |
| 2 | Jellyfin 12.2.0 | drain-steady | completed | 30.060 | 2026-10-20T01:24:04.023+00:00 | 2026-10-20T01:24:34.083+00:00 | — |
| 2 | Jellyfin 12.2.0 | loaded | completed | 120.288 | 2026-10-20T01:19:00.903+00:00 | 2026-10-20T01:21:01.191+00:00 | — |
| 2 | Jellyfin 12.2.0 | warmup-loaded | completed | 30.275 | 2026-10-20T01:18:29.811+00:00 | 2026-10-20T01:19:00.086+00:00 | — |
| 2 | Jellyfin 12.2.0 | drain-loaded | completed | 30.059 | 2026-10-20T01:17:59.449+00:00 | 2026-10-20T01:18:29.508+00:00 | — |
| 2 | Jellyfin 12.2.0 | stress | completed | 120.263 | 2026-10-20T01:22:03.247+00:00 | 2026-10-20T01:24:03.510+00:00 | — |
| 2 | Jellyfin 12.2.0 | warmup-stress | completed | 30.384 | 2026-10-20T01:21:32.069+00:00 | 2026-10-20T01:22:02.452+00:00 | — |
| 2 | Jellyfin 12.2.0 | drain-stress | completed | 30.060 | 2026-10-20T01:21:01.713+00:00 | 2026-10-20T01:21:31.773+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-shape-before | completed | 0.338 | 2026-10-20T01:13:12.641+00:00 | 2026-10-20T01:13:12.979+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-shape-after | completed | 0.280 | 2026-10-20T01:13:46.385+00:00 | 2026-10-20T01:13:46.665+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-coldstart-before | completed | 0.282 | 2026-10-20T01:13:46.674+00:00 | 2026-10-20T01:13:46.957+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-coldstart-after | completed | 0.669 | 2026-10-20T01:14:56.357+00:00 | 2026-10-20T01:14:57.026+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-unloaded-before | completed | 0.316 | 2026-10-20T01:15:27.218+00:00 | 2026-10-20T01:15:27.534+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-unloaded-after | completed | 0.278 | 2026-10-20T01:15:57.806+00:00 | 2026-10-20T01:15:58.084+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-unloaded-before | completed | 0.292 | 2026-10-20T01:15:58.095+00:00 | 2026-10-20T01:15:58.387+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-unloaded-after | completed | 0.287 | 2026-10-20T01:17:59.152+00:00 | 2026-10-20T01:17:59.439+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-loaded-before | completed | 0.282 | 2026-10-20T01:18:29.519+00:00 | 2026-10-20T01:18:29.801+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-loaded-after | completed | 0.286 | 2026-10-20T01:19:00.096+00:00 | 2026-10-20T01:19:00.381+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-loaded-before | completed | 0.292 | 2026-10-20T01:19:00.392+00:00 | 2026-10-20T01:19:00.684+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-loaded-after | completed | 0.284 | 2026-10-20T01:21:01.419+00:00 | 2026-10-20T01:21:01.702+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-stress-before | completed | 0.276 | 2026-10-20T01:21:31.783+00:00 | 2026-10-20T01:21:32.059+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-warmup-stress-after | completed | 0.282 | 2026-10-20T01:22:02.463+00:00 | 2026-10-20T01:22:02.744+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-stress-before | completed | 0.275 | 2026-10-20T01:22:02.754+00:00 | 2026-10-20T01:22:03.029+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-stress-after | completed | 0.276 | 2026-10-20T01:24:03.737+00:00 | 2026-10-20T01:24:04.013+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-ttfs-before | completed | 0.298 | 2026-10-20T01:25:34.492+00:00 | 2026-10-20T01:25:34.791+00:00 | — |
| 2 | Jellyfin 12.2.0 | isolation-ttfs-after | completed | 0.276 | 2026-10-20T01:26:22.916+00:00 | 2026-10-20T01:26:23.192+00:00 | — |
| 2 | Ferrofin | startup | completed | 11.142 | 2026-10-20T01:26:23.816+00:00 | 2026-10-20T01:26:34.958+00:00 | — |
| 2 | Ferrofin | startup-drain | completed | 30.052 | 2026-10-20T01:26:34.967+00:00 | 2026-10-20T01:27:05.019+00:00 | — |
| 2 | Ferrofin | cleanup | completed | 0.389 | 2026-10-20T01:39:43.276+00:00 | 2026-10-20T01:39:43.665+00:00 | — |
| 2 | Ferrofin | isolation-setup | completed | 0.292 | 2026-10-20T01:27:05.036+00:00 | 2026-10-20T01:27:05.328+00:00 | — |
| 2 | Ferrofin | counts | completed | 0.292 | 2026-10-20T01:27:05.338+00:00 | 2026-10-20T01:27:05.629+00:00 | — |
| 2 | Ferrofin | shape | completed | 16.247 | 2026-10-20T01:27:05.803+00:00 | 2026-10-20T01:27:22.050+00:00 | — |
| 2 | Ferrofin | coldstart | completed | 56.844 | 2026-10-20T01:27:22.392+00:00 | 2026-10-20T01:28:19.236+00:00 | — |
| 2 | Ferrofin | ttfs | completed | 47.889 | 2026-10-20T01:38:55.195+00:00 | 2026-10-20T01:39:43.083+00:00 | — |
| 2 | Ferrofin | unloaded | completed | 120.250 | 2026-10-20T01:29:20.545+00:00 | 2026-10-20T01:31:20.795+00:00 | — |
| 2 | Ferrofin | warmup-unloaded | completed | 30.237 | 2026-10-20T01:28:49.752+00:00 | 2026-10-20T01:29:19.989+00:00 | — |
| 2 | Ferrofin | drain-unloaded | completed | 30.050 | 2026-10-20T01:28:19.525+00:00 | 2026-10-20T01:28:49.574+00:00 | — |
| 2 | Ferrofin | sampler | completed | 635.603 | 2026-10-20T01:28:19.415+00:00 | 2026-10-20T01:38:55.018+00:00 | — |
| 2 | Ferrofin | steady | completed | 60.010 | 2026-10-20T01:37:54.630+00:00 | 2026-10-20T01:38:54.640+00:00 | — |
| 2 | Ferrofin | drain-steady | completed | 30.053 | 2026-10-20T01:37:24.565+00:00 | 2026-10-20T01:37:54.618+00:00 | — |
| 2 | Ferrofin | loaded | completed | 120.240 | 2026-10-20T01:32:22.266+00:00 | 2026-10-20T01:34:22.506+00:00 | — |
| 2 | Ferrofin | warmup-loaded | completed | 30.264 | 2026-10-20T01:31:51.430+00:00 | 2026-10-20T01:32:21.694+00:00 | — |
| 2 | Ferrofin | drain-loaded | completed | 30.053 | 2026-10-20T01:31:21.198+00:00 | 2026-10-20T01:31:51.251+00:00 | — |
| 2 | Ferrofin | stress | completed | 120.251 | 2026-10-20T01:35:23.917+00:00 | 2026-10-20T01:37:24.168+00:00 | — |
| 2 | Ferrofin | warmup-stress | completed | 30.222 | 2026-10-20T01:34:53.127+00:00 | 2026-10-20T01:35:23.348+00:00 | — |
| 2 | Ferrofin | drain-stress | completed | 30.049 | 2026-10-20T01:34:22.899+00:00 | 2026-10-20T01:34:52.948+00:00 | — |
| 2 | Ferrofin | isolation-shape-before | completed | 0.155 | 2026-10-20T01:27:05.639+00:00 | 2026-10-20T01:27:05.794+00:00 | — |
| 2 | Ferrofin | isolation-shape-after | completed | 0.161 | 2026-10-20T01:27:22.061+00:00 | 2026-10-20T01:27:22.222+00:00 | — |
| 2 | Ferrofin | isolation-coldstart-before | completed | 0.152 | 2026-10-20T01:27:22.230+00:00 | 2026-10-20T01:27:22.382+00:00 | — |
| 2 | Ferrofin | isolation-coldstart-after | completed | 0.159 | 2026-10-20T01:28:19.246+00:00 | 2026-10-20T01:28:19.405+00:00 | — |
| 2 | Ferrofin | isolation-warmup-unloaded-before | completed | 0.158 | 2026-10-20T01:28:49.585+00:00 | 2026-10-20T01:28:49.744+00:00 | — |
| 2 | Ferrofin | isolation-warmup-unloaded-after | completed | 0.165 | 2026-10-20T01:29:19.999+00:00 | 2026-10-20T01:29:20.164+00:00 | — |
| 2 | Ferrofin | isolation-unloaded-before | completed | 0.154 | 2026-10-20T01:29:20.174+00:00 | 2026-10-20T01:29:20.328+00:00 | — |
| 2 | Ferrofin | isolation-unloaded-after | completed | 0.167 | 2026-10-20T01:31:21.022+00:00 | 2026-10-20T01:31:21.189+00:00 | — |
| 2 | Ferrofin | isolation-warmup-loaded-before | completed | 0.160 | 2026-10-20T01:31:51.261+00:00 | 2026-10-20T01:31:51.421+00:00 | — |
| 2 | Ferrofin | isolation-warmup-loaded-after | completed | 0.173 | 2026-10-20T01:32:21.704+00:00 | 2026-10-20T01:32:21.876+00:00 | — |
| 2 | Ferrofin | isolation-loaded-before | completed | 0.164 | 2026-10-20T01:32:21.886+00:00 | 2026-10-20T01:32:22.050+00:00 | — |
| 2 | Ferrofin | isolation-loaded-after | completed | 0.157 | 2026-10-20T01:34:22.733+00:00 | 2026-10-20T01:34:22.889+00:00 | — |
| 2 | Ferrofin | isolation-warmup-stress-before | completed | 0.159 | 2026-10-20T01:34:52.958+00:00 | 2026-10-20T01:34:53.117+00:00 | — |
| 2 | Ferrofin | isolation-warmup-stress-after | completed | 0.163 | 2026-10-20T01:35:23.358+00:00 | 2026-10-20T01:35:23.522+00:00 | — |
| 2 | Ferrofin | isolation-stress-before | completed | 0.166 | 2026-10-20T01:35:23.532+00:00 | 2026-10-20T01:35:23.699+00:00 | — |
| 2 | Ferrofin | isolation-stress-after | completed | 0.160 | 2026-10-20T01:37:24.395+00:00 | 2026-10-20T01:37:24.555+00:00 | — |
| 2 | Ferrofin | isolation-ttfs-before | completed | 0.157 | 2026-10-20T01:38:55.028+00:00 | 2026-10-20T01:38:55.185+00:00 | — |
| 2 | Ferrofin | isolation-ttfs-after | completed | 0.172 | 2026-10-20T01:39:43.094+00:00 | 2026-10-20T01:39:43.266+00:00 | — |
| 3 | Jellyfin 12.2.0 | startup | completed | 7.980 | 2026-10-20T01:39:47.663+00:00 | 2026-10-20T01:39:55.642+00:00 | — |
| 3 | Jellyfin 12.2.0 | startup-drain | completed | 30.065 | 2026-10-20T01:39:55.652+00:00 | 2026-10-20T01:40:25.717+00:00 | — |
| 3 | Jellyfin 12.2.0 | cleanup | completed | 0.399 | 2026-10-20T01:53:39.072+00:00 | 2026-10-20T01:53:39.471+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-setup | completed | 1.691 | 2026-10-20T01:40:25.734+00:00 | 2026-10-20T01:40:27.424+00:00 | — |
| 3 | Jellyfin 12.2.0 | counts | completed | 2.138 | 2026-10-20T01:40:27.435+00:00 | 2026-10-20T01:40:29.573+00:00 | — |
| 3 | Jellyfin 12.2.0 | shape | completed | 33.047 | 2026-10-20T01:40:29.928+00:00 | 2026-10-20T01:41:02.975+00:00 | — |
| 3 | Jellyfin 12.2.0 | coldstart | completed | 68.980 | 2026-10-20T01:41:03.556+00:00 | 2026-10-20T01:42:12.536+00:00 | — |
| 3 | Jellyfin 12.2.0 | ttfs | completed | 48.072 | 2026-10-20T01:52:50.703+00:00 | 2026-10-20T01:53:38.775+00:00 | — |
| 3 | Jellyfin 12.2.0 | unloaded | completed | 120.277 | 2026-10-20T01:43:14.729+00:00 | 2026-10-20T01:45:15.006+00:00 | — |
| 3 | Jellyfin 12.2.0 | warmup-unloaded | completed | 30.225 | 2026-10-20T01:42:43.710+00:00 | 2026-10-20T01:43:13.935+00:00 | — |
| 3 | Jellyfin 12.2.0 | drain-unloaded | completed | 30.059 | 2026-10-20T01:42:13.312+00:00 | 2026-10-20T01:42:43.371+00:00 | — |
| 3 | Jellyfin 12.2.0 | sampler | completed | 637.183 | 2026-10-20T01:42:13.202+00:00 | 2026-10-20T01:52:50.385+00:00 | — |
| 3 | Jellyfin 12.2.0 | steady | completed | 60.010 | 2026-10-20T01:51:49.895+00:00 | 2026-10-20T01:52:49.905+00:00 | — |
| 3 | Jellyfin 12.2.0 | drain-steady | completed | 30.060 | 2026-10-20T01:51:19.824+00:00 | 2026-10-20T01:51:49.884+00:00 | — |
| 3 | Jellyfin 12.2.0 | loaded | completed | 120.251 | 2026-10-20T01:46:16.934+00:00 | 2026-10-20T01:48:17.185+00:00 | — |
| 3 | Jellyfin 12.2.0 | warmup-loaded | completed | 30.263 | 2026-10-20T01:45:45.878+00:00 | 2026-10-20T01:46:16.141+00:00 | — |
| 3 | Jellyfin 12.2.0 | drain-loaded | completed | 30.060 | 2026-10-20T01:45:15.518+00:00 | 2026-10-20T01:45:45.577+00:00 | — |
| 3 | Jellyfin 12.2.0 | stress | completed | 120.230 | 2026-10-20T01:49:19.080+00:00 | 2026-10-20T01:51:19.310+00:00 | — |
| 3 | Jellyfin 12.2.0 | warmup-stress | completed | 30.222 | 2026-10-20T01:48:48.066+00:00 | 2026-10-20T01:49:18.288+00:00 | — |
| 3 | Jellyfin 12.2.0 | drain-stress | completed | 30.057 | 2026-10-20T01:48:17.707+00:00 | 2026-10-20T01:48:47.764+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-shape-before | completed | 0.337 | 2026-10-20T01:40:29.583+00:00 | 2026-10-20T01:40:29.919+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-shape-after | completed | 0.276 | 2026-10-20T01:41:02.986+00:00 | 2026-10-20T01:41:03.262+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-coldstart-before | completed | 0.275 | 2026-10-20T01:41:03.272+00:00 | 2026-10-20T01:41:03.547+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-coldstart-after | completed | 0.645 | 2026-10-20T01:42:12.546+00:00 | 2026-10-20T01:42:13.192+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-unloaded-before | completed | 0.318 | 2026-10-20T01:42:43.382+00:00 | 2026-10-20T01:42:43.700+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-unloaded-after | completed | 0.280 | 2026-10-20T01:43:13.945+00:00 | 2026-10-20T01:43:14.225+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-unloaded-before | completed | 0.276 | 2026-10-20T01:43:14.235+00:00 | 2026-10-20T01:43:14.511+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-unloaded-after | completed | 0.276 | 2026-10-20T01:45:15.233+00:00 | 2026-10-20T01:45:15.509+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-loaded-before | completed | 0.280 | 2026-10-20T01:45:45.588+00:00 | 2026-10-20T01:45:45.868+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-loaded-after | completed | 0.274 | 2026-10-20T01:46:16.151+00:00 | 2026-10-20T01:46:16.425+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-loaded-before | completed | 0.282 | 2026-10-20T01:46:16.435+00:00 | 2026-10-20T01:46:16.717+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-loaded-after | completed | 0.285 | 2026-10-20T01:48:17.412+00:00 | 2026-10-20T01:48:17.697+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-stress-before | completed | 0.282 | 2026-10-20T01:48:47.775+00:00 | 2026-10-20T01:48:48.057+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-warmup-stress-after | completed | 0.277 | 2026-10-20T01:49:18.298+00:00 | 2026-10-20T01:49:18.574+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-stress-before | completed | 0.279 | 2026-10-20T01:49:18.585+00:00 | 2026-10-20T01:49:18.863+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-stress-after | completed | 0.277 | 2026-10-20T01:51:19.536+00:00 | 2026-10-20T01:51:19.813+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-ttfs-before | completed | 0.299 | 2026-10-20T01:52:50.394+00:00 | 2026-10-20T01:52:50.693+00:00 | — |
| 3 | Jellyfin 12.2.0 | isolation-ttfs-after | completed | 0.276 | 2026-10-20T01:53:38.786+00:00 | 2026-10-20T01:53:39.062+00:00 | — |
| 3 | Ferrofin | startup | completed | 21.315 | 2026-10-20T01:53:39.703+00:00 | 2026-10-20T01:54:01.019+00:00 | — |
| 3 | Ferrofin | startup-drain | completed | 30.051 | 2026-10-20T01:54:01.028+00:00 | 2026-10-20T01:54:31.079+00:00 | — |
| 3 | Ferrofin | cleanup | completed | 0.423 | 2026-10-20T02:07:08.992+00:00 | 2026-10-20T02:07:09.414+00:00 | — |
| 3 | Ferrofin | isolation-setup | completed | 0.294 | 2026-10-20T01:54:31.097+00:00 | 2026-10-20T01:54:31.391+00:00 | — |
| 3 | Ferrofin | counts | completed | 0.299 | 2026-10-20T01:54:31.401+00:00 | 2026-10-20T01:54:31.700+00:00 | — |
| 3 | Ferrofin | shape | completed | 16.161 | 2026-10-20T01:54:31.870+00:00 | 2026-10-20T01:54:48.031+00:00 | — |
| 3 | Ferrofin | coldstart | completed | 56.530 | 2026-10-20T01:54:48.390+00:00 | 2026-10-20T01:55:44.919+00:00 | — |
| 3 | Ferrofin | ttfs | completed | 47.920 | 2026-10-20T02:06:20.888+00:00 | 2026-10-20T02:07:08.808+00:00 | — |
| 3 | Ferrofin | unloaded | completed | 120.250 | 2026-10-20T01:56:46.221+00:00 | 2026-10-20T01:58:46.472+00:00 | — |
| 3 | Ferrofin | warmup-unloaded | completed | 30.239 | 2026-10-20T01:56:15.429+00:00 | 2026-10-20T01:56:45.668+00:00 | — |
| 3 | Ferrofin | drain-unloaded | completed | 30.052 | 2026-10-20T01:55:45.205+00:00 | 2026-10-20T01:56:15.256+00:00 | — |
| 3 | Ferrofin | sampler | completed | 635.613 | 2026-10-20T01:55:45.096+00:00 | 2026-10-20T02:06:20.708+00:00 | — |
| 3 | Ferrofin | steady | completed | 60.010 | 2026-10-20T02:05:20.317+00:00 | 2026-10-20T02:06:20.327+00:00 | — |
| 3 | Ferrofin | drain-steady | completed | 30.051 | 2026-10-20T02:04:50.255+00:00 | 2026-10-20T02:05:20.305+00:00 | — |
| 3 | Ferrofin | loaded | completed | 120.250 | 2026-10-20T01:59:47.908+00:00 | 2026-10-20T02:01:48.158+00:00 | — |
| 3 | Ferrofin | warmup-loaded | completed | 30.229 | 2026-10-20T01:59:17.100+00:00 | 2026-10-20T01:59:47.328+00:00 | — |
| 3 | Ferrofin | drain-loaded | completed | 30.050 | 2026-10-20T01:58:46.873+00:00 | 2026-10-20T01:59:16.923+00:00 | — |
| 3 | Ferrofin | stress | completed | 120.256 | 2026-10-20T02:02:49.604+00:00 | 2026-10-20T02:04:49.860+00:00 | — |
| 3 | Ferrofin | warmup-stress | completed | 30.252 | 2026-10-20T02:02:18.789+00:00 | 2026-10-20T02:02:49.041+00:00 | — |
| 3 | Ferrofin | drain-stress | completed | 30.052 | 2026-10-20T02:01:48.555+00:00 | 2026-10-20T02:02:18.608+00:00 | — |
| 3 | Ferrofin | isolation-shape-before | completed | 0.151 | 2026-10-20T01:54:31.709+00:00 | 2026-10-20T01:54:31.860+00:00 | — |
| 3 | Ferrofin | isolation-shape-after | completed | 0.172 | 2026-10-20T01:54:48.042+00:00 | 2026-10-20T01:54:48.214+00:00 | — |
| 3 | Ferrofin | isolation-coldstart-before | completed | 0.156 | 2026-10-20T01:54:48.224+00:00 | 2026-10-20T01:54:48.381+00:00 | — |
| 3 | Ferrofin | isolation-coldstart-after | completed | 0.156 | 2026-10-20T01:55:44.930+00:00 | 2026-10-20T01:55:45.086+00:00 | — |
| 3 | Ferrofin | isolation-warmup-unloaded-before | completed | 0.152 | 2026-10-20T01:56:15.267+00:00 | 2026-10-20T01:56:15.419+00:00 | — |
| 3 | Ferrofin | isolation-warmup-unloaded-after | completed | 0.161 | 2026-10-20T01:56:45.678+00:00 | 2026-10-20T01:56:45.838+00:00 | — |
| 3 | Ferrofin | isolation-unloaded-before | completed | 0.156 | 2026-10-20T01:56:45.848+00:00 | 2026-10-20T01:56:46.004+00:00 | — |
| 3 | Ferrofin | isolation-unloaded-after | completed | 0.165 | 2026-10-20T01:58:46.698+00:00 | 2026-10-20T01:58:46.863+00:00 | — |
| 3 | Ferrofin | isolation-warmup-loaded-before | completed | 0.157 | 2026-10-20T01:59:16.934+00:00 | 2026-10-20T01:59:17.090+00:00 | — |
| 3 | Ferrofin | isolation-warmup-loaded-after | completed | 0.161 | 2026-10-20T01:59:47.339+00:00 | 2026-10-20T01:59:47.500+00:00 | — |
| 3 | Ferrofin | isolation-loaded-before | completed | 0.179 | 2026-10-20T01:59:47.510+00:00 | 2026-10-20T01:59:47.689+00:00 | — |
| 3 | Ferrofin | isolation-loaded-after | completed | 0.161 | 2026-10-20T02:01:48.385+00:00 | 2026-10-20T02:01:48.545+00:00 | — |
| 3 | Ferrofin | isolation-warmup-stress-before | completed | 0.163 | 2026-10-20T02:02:18.618+00:00 | 2026-10-20T02:02:18.780+00:00 | — |
| 3 | Ferrofin | isolation-warmup-stress-after | completed | 0.167 | 2026-10-20T02:02:49.051+00:00 | 2026-10-20T02:02:49.218+00:00 | — |
| 3 | Ferrofin | isolation-stress-before | completed | 0.158 | 2026-10-20T02:02:49.229+00:00 | 2026-10-20T02:02:49.387+00:00 | — |
| 3 | Ferrofin | isolation-stress-after | completed | 0.157 | 2026-10-20T02:04:50.087+00:00 | 2026-10-20T02:04:50.244+00:00 | — |
| 3 | Ferrofin | isolation-ttfs-before | completed | 0.159 | 2026-10-20T02:06:20.720+00:00 | 2026-10-20T02:06:20.879+00:00 | — |
| 3 | Ferrofin | isolation-ttfs-after | completed | 0.164 | 2026-10-20T02:07:08.818+00:00 | 2026-10-20T02:07:08.981+00:00 | — |

### Time to first screen

| | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| cold start (restart → authenticated views; host caches retained) | 3301 ms (3273 ms–3957 ms) | 1088 ms (739 ms–1092 ms) — 3.0× faster |
| HLS first segment (forced transcode) | 330 ms | 186 ms (180 ms–189 ms) — 1.8× faster |
| direct-play TTFB (1 MiB range) | 3.93 ms (3.86 ms–4.11 ms) | 0.52 ms (0.50 ms–0.58 ms) — 7.6× faster |

### Memory (anon, cache excluded, 100 ms samples)

| | Jellyfin 12.2.0 | Ferrofin |
|---|---|---|
| peak under load | 590 MiB (551 MiB–609 MiB) | 214 MiB (211 MiB–227 MiB) — 2.8× lighter |
| steady idle | 422 MiB (372 MiB–432 MiB) | 168 MiB (166 MiB–168 MiB) — 2.5× lighter |
| interference on the server's cores, p95 | 14% (13%–49%) | 13% (13%–42%) |
| interference, max single sample | 41% (35%–100%) | 45% (37%–100%) |
| max swap | 0.00 MiB | 0.00 MiB |

### Response shape vs Jellyfin 12.2.0 (supporting evidence, not the parity number)

**ferrofin**: 12 divergence(s) across 33 compared requests
- count latest: 9 vs 3
- count persons: 5000 vs 5158
- detail:image: request picks differ
- detail:item: per-item fields/types differ for a request pick
- detail:similar: ids differs for a request pick
- home:latest-shows: ids differs for a request pick
- home:views: per-item fields/types differ for a request pick
- image: request picks differ
- search:artists: ids differs for a request pick
- search:items: ids differs for a request pick
- search:persons: items [0] vs [0, 1, 2, 3, 4, 5, 7]
- series:similar: ids differs for a request pick


### Notes

1. home:latest-shows: ids differs for a request pick
   — Ferrofin · home (unloaded screens); Ferrofin · home:latest-shows (unloaded endpoints); Ferrofin · home (loaded screens); Ferrofin · home:latest-shows (loaded endpoints); Ferrofin · home (stress screens); Ferrofin · home:latest-shows (stress endpoints)
2. detail:image: request picks differ
   — Ferrofin · detail (unloaded screens); Ferrofin · detail:image (unloaded endpoints); Ferrofin · detail (loaded screens); Ferrofin · detail:image (loaded endpoints); Ferrofin · detail (stress screens); Ferrofin · detail:image (stress endpoints)
3. series:similar: ids differs for a request pick
   — Ferrofin · series (unloaded screens); Ferrofin · series:similar (unloaded endpoints); Ferrofin · series (loaded screens); Ferrofin · series:similar (loaded endpoints); Ferrofin · series (stress screens); Ferrofin · series:similar (stress endpoints)
4. search:artists: ids differs for a request pick
   — Ferrofin · search (unloaded screens); Ferrofin · search:artists (unloaded endpoints); Ferrofin · search (loaded screens); Ferrofin · search:artists (loaded endpoints); Ferrofin · search (stress screens); Ferrofin · search:artists (stress endpoints)
5. home:views: per-item fields/types differ for a request pick
   — Ferrofin · home:views (unloaded endpoints); Ferrofin · home:views (loaded endpoints); Ferrofin · home:views (stress endpoints)
6. detail:item: per-item fields/types differ for a request pick
   — Ferrofin · detail:item (unloaded endpoints); Ferrofin · detail:item (loaded endpoints); Ferrofin · detail:item (stress endpoints)
7. detail:similar: ids differs for a request pick
   — Ferrofin · detail:similar (unloaded endpoints); Ferrofin · detail:similar (loaded endpoints); Ferrofin · detail:similar (stress endpoints)
8. search:items: ids differs for a request pick
   — Ferrofin · search:items (unloaded endpoints); Ferrofin · search:items (loaded endpoints); Ferrofin · search:items (stress endpoints)
9. search:persons: items [0] vs [0, 1, 2, 3, 4, 5, 7]
   — Ferrofin · search:persons (unloaded endpoints); Ferrofin · search:persons (loaded endpoints); Ferrofin · search:persons (stress endpoints)
10. image: request picks differ
   — Ferrofin · image (unloaded endpoints); Ferrofin · image (loaded endpoints); Ferrofin · image (stress endpoints)

---

## Reproduce the reports

Raw evidence stays under ignored `bench/runs/` directories. Role credentials must not be
published.

```sh
python3 bench/report.py \
  bench/runs/v1.3.2-603-d2bb9306 \
  bench/runs/v1.3.2-603-d2bb9306-run2 \
  bench/runs/v1.3.2-603-d2bb9306-run3
```
