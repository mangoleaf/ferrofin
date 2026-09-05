#!/usr/bin/env python3
"""Render a run (or several) as the README tables, or as an HTML page for comparing
(PLAN_BENCHMARK_V3 §7). stdlib only.

    report.py RUN_DIR [RUN_DIR ...]          markdown on stdout (the full tables, docs/BENCHMARKS.md style)
    report.py --readme [README.md] RUN_DIR... the README "Benchmarks" section (headline table + Mermaid
                                             speedup charts); with a README path, replaces the block between
                                             the BEGIN/END GENERATED BENCHMARKS markers in place
    report.py --serve [PORT] [RUNS_DIR]      the comparison viewer on http://127.0.0.1:PORT (default 8097, bench/runs)

One run dir: the numbers of that run. Several: each cell is the median across runs, and
where the runs disagreed at the precision published, the range they spanned; a cell that
did different work than the oracle is marked `⚠[n]` and the reason is printed once,
numbered, under Notes.
One rule is applied per cell, from files the run itself wrote:
  comparable   — same status + record count as the oracle (Jellyfin 12.0-rc7) and a field
                 set ⊇ the oracle's, for every request name behind the cell (shape.log).
Flagged cells keep their raw number (the work list) but are not publishable.
The range is reported, never judged. A fixed 15 % band was tried as a reproducibility
verdict and withdrawn (owner, 2026-09-04): it failed 57 of Ferrofin's 110 cells. Counting
each failing number separately — a latency cell prints three — those 57 cells hold 80
failures, and 69 of the 80 are p95 or p99, where a percentage of the median cannot tell
run-to-run noise from a tail that is genuinely long. Nor was it a busy host: an idle trio
(`quiet-1..3`) failed 41 of 76, and over every cell both trios produced it failed MORE
often than the working one — 54 % against 45 %. Read the range and decide.
Both renderers print, per cell, how it compares with the oracle as `X.Y× faster`
(memory says lighter), computed from the printed numbers so a reader can check it; the
viewer adds, with a baseline run chosen, each server's change against an earlier run of
itself (the before/after of a code change). A comparison drawn from a flagged number is
shown in amber italics rather than green. Missing phases are named.
"""

import csv
import html
import http.server
import json
import os
import statistics
import sys
import urllib.parse
from collections import defaultdict

DELTA_NOISE_PCT = 2  # a baseline change smaller than this is not coloured
ORACLE = "jellyfin12"  # the source of truth (owner, 2026-09-02): Jellyfin 12 is the newer code
ORACLE_LABEL = "Jellyfin 12.0-rc7"
SERVERS = [("jellyfin12", "Jellyfin 12.0-rc7"), ("jellyfin", "Jellyfin 10.11.8"), ("ferrofin", "Ferrofin")]
SCREENS = ["home", "movies", "detail", "series", "search", "playback"]
#: The load levels a run may contain, lightest first. A run that skipped one (`--only`)
#: has no entry for it in `windows.json`, and the renderers drop the level.
LOAD_LEVELS = ("unloaded", "loaded", "stress")
#: The levels "peak under load" is the peak *of*. `unloaded` is a screen a second — it is
#: a control, not load, and a peak attributed to it would not mean what the row says.
PEAK_LEVELS = ("loaded", "stress")


def run_levels(d):
    """The load levels this run dir actually executed, from its own `windows.json`.
    A level that ran and then failed to produce a k6 file must still be reported as a
    missing phase, so "did it run" cannot be inferred from the result file existing."""
    win = load(os.path.join(d, "windows.json"))
    if win is None:
        # An interrupted run has no record of itself; fall back to what it produced, so
        # a level whose result file is also missing is still named rather than dropped.
        return {lvl for lvl in LOAD_LEVELS if os.path.isfile(os.path.join(d, f"k6-{lvl}.json"))}
    return {lvl for lvl in LOAD_LEVELS if lvl in win}
MIB = 2 ** 20
TRANSCODE_NOISE = ("playsessionid", "apikey", "deviceid", "tag=", "api_key", "transcodereasons")


# ── loading ─────────────────────────────────────────────────────────────────
def load(path):
    try:
        return json.load(open(path))
    except (OSError, ValueError):
        return None


def load_shape(d):
    """shape.log → {request name: {status set, counts, fields}} merged over the responses."""
    out = defaultdict(lambda: {"status": set(), "count": set(), "total": set(), "fields": set(), "n": 0})
    try:
        lines = open(os.path.join(d, "shape.log")).read().splitlines()
    except OSError:
        return None
    for line in lines:
        try:
            rec = json.loads(json.loads(line)["msg"])
        except (ValueError, KeyError, TypeError):
            continue
        s = out[rec["shape"]]
        s["status"].add(rec.get("status"))
        s["n"] += 1
        if "count" in rec and rec["count"] is not None:
            s["count"].add(rec["count"])
        if "total" in rec and rec["total"] is not None:
            s["total"].add(rec["total"])
        s["fields"].update(rec.get("fields") or [])
    return out


def oracle_failed(shape_oracle, names):
    """The oracle's own verdict: a status >= 400 on any request behind the row."""
    if shape_oracle is None:
        return "no shape pass"  # the oracle ran but its shape phase did not
    for n in names:
        o = shape_oracle.get(n)
        if o and any(st >= 400 for st in o["status"]):
            return f"{ORACLE_LABEL} failed {n} ({sorted(o['status'])})"
    return None


def comparable(shape_srv, shape_oracle, names):
    """None if comparable, else the reason."""
    if shape_oracle is None:
        return f"no oracle run ({ORACLE_LABEL} not in this run)"
    if shape_srv is None:
        return "no shape pass"
    for n in names:
        o, s = shape_oracle.get(n), shape_srv.get(n)
        if o is None or s is None:
            return f"{n}: missing"
        if any(st >= 400 for st in o["status"]):
            return f"{n}: {ORACLE_LABEL} failed ({sorted(o['status'])})"
        if s["status"] != o["status"]:
            return f"{n}: status {sorted(s['status'])} vs {sorted(o['status'])}"
        if s["count"] != o["count"]:
            return f"{n}: items {sorted(s['count'])} vs {sorted(o['count'])}"
        if s["total"] != o["total"]:
            return f"{n}: TotalRecordCount {sorted(s['total'])} vs {sorted(o['total'])}"
        missing = sorted(o["fields"] - s["fields"])
        if missing:
            more = f" (+{len(missing) - 3} more)" if len(missing) > 3 else ""
            return f"{n}: missing {', '.join(m.lstrip('.') for m in missing[:3])}{more}"
    return None


def mem_numbers(d):
    win = load(os.path.join(d, "windows.json"))
    try:
        rows = list(csv.DictReader(open(os.path.join(d, "mem.csv"))))
    except OSError:
        return None
    if not win or not rows:
        return None

    def within(w):
        return [int(r["anon"]) for r in rows if w["start"] <= float(r["t"]) <= w["end"]]

    out = {}
    # "Peak under load" is the peak across the run's load windows — with a stress level
    # present that is where it lives, and naming one window would quietly report the
    # second-heaviest number. `unloaded` is excluded: it is the control, not load.
    under_load = [v for lvl in PEAK_LEVELS if lvl in win for v in within(win[lvl])]
    if under_load:
        out["peak"] = max(under_load) / MIB
    if "steady" in win and within(win["steady"]):
        out["steady"] = statistics.median(within(win["steady"])) / MIB
    if "interference" in rows[0]:
        inter = sorted(float(r["interference"]) for r in rows)
        out["interference"] = inter[-1]
        out["interference_p95"] = inter[int(0.95 * (len(inter) - 1))]
    if "swap" in rows[0]:
        out["swap_max"] = max(int(r["swap"]) for r in rows) / MIB
    return out


# ── the model: one Cell per (row, server) ───────────────────────────────────
class Cell:
    """A number across runs, the range they spanned, and the comparability verdict.
    `vals` are per-run values (None = missing); `flag` is the reason it is not
    publishable, if any."""

    def __init__(self, vals, fmt, flag=None, sub=None, unit="ms", context=False):
        vals = list(vals)
        #: how many runs were selected, whether or not each produced this number
        self.runs = len(vals)
        self.vals = [v for v in vals if v is not None]
        self.fmt = fmt
        self.flag = flag
        self.sub = sub or {}  # extra per-run series, e.g. p95/p99/err for latency
        self.unit = unit
        self.context = context  # describes the run's conditions, not the server: no ratio, no delta

    @property
    def median(self):
        return statistics.median(self.vals) if self.vals else None

    def value(self):
        """The published number — the median, and nothing else. A cell has to stay
        scannable, so the range and the comparability flag are added by the renderers."""
        return self.fmt(self.median) if self.vals else "—"

    def spread_text(self):
        """`min–max over N runs`, or None when there is no range to report — one run, or
        runs that agreed at the precision published, where `174 ms–174 ms` would read as
        a measurement rather than as agreement. Says `of M selected` when the cell is
        missing from some of them: a level added later exists in one run and not another,
        and one value is not the same evidence as several agreeing."""
        coverage = f"{len(self.vals)} of {self.runs} selected runs" if self.runs > len(self.vals) else None
        if len(self.vals) < 2:
            # Including no values at all: a tile prints "—" and this line says how many
            # of the selected runs produced nothing, which is the useful thing to know.
            return coverage
        lo, hi = self.fmt(min(self.vals)), self.fmt(max(self.vals))
        if lo == hi:
            return coverage
        return f"{lo}–{hi} over {coverage or f'{len(self.vals)} runs'}"

    def parts(self):
        """This cell and, for a latency cell, its p95 and p99 — every number it prints,
        each carrying its own range."""
        extra = [self.sub[k] for k in ("p95", "p99") if k in self.sub]
        return [self, *extra]

    def with_ranges(self):
        """The numbers, each with the range its runs spanned where they disagree at the
        precision printed — markdown has no hover text, so the range has to be in the
        cell or it is not available at all. A number identical on every run as published
        has nothing to add."""
        out = []
        for part in self.parts():
            txt = part.value()
            if len(part.vals) > 1 and part.fmt(min(part.vals)) != part.fmt(max(part.vals)):
                txt += f" ({part.fmt(min(part.vals))}–{part.fmt(max(part.vals))})"
            out.append(txt)
        txt = " / ".join(out)
        if 0 < len(self.vals) < self.runs:
            # A cell missing from some of the selected runs is not the same measurement
            # as one present in all of them — say which, rather than let it look agreed.
            txt += f" [{len(self.vals)}/{self.runs} runs]"
        return txt

    def spreads(self):
        """The per-number spreads, for the hover text."""
        labels = ("p50", "p95", "p99") if "p95" in self.sub else ("",)
        out = [f"{lbl} {p.spread_text()}".strip() for lbl, p in zip(labels, self.parts()) if p.spread_text()]
        return "; ".join(out)


def sig3(v):
    """Three significant figures without an exponent: `0.71`, `5.31`, `19.3`, `171`,
    `2960`. Fixed decimals cannot serve both ends of this report — most endpoint
    numbers are under 10 ms and a good many under 1 ms, where printing `0` or `1`
    lost the value and suppressed its speedup outright; while cold start is ~2,800 ms
    and moves by tens between runs, where two decimals would be precision the
    measurement does not have."""
    if abs(v) >= 100:
        return f"{v:.0f}"
    if abs(v) >= 10:
        return f"{v:.1f}"
    return f"{v:.2f}"


ms = lambda v: f"{sig3(v)} ms"
mib = lambda v: f"{sig3(v)} MiB"
#: A screen is timed with `Date.now()` in screens.js, so its *samples* are whole
#: milliseconds. k6 interpolates its Trend percentiles, so the run files do hold
#: fractional screen values — but that fraction comes from interpolating between
#: 1 ms-quantised samples, not from measuring finer, so printing it would dress up
#: the instrument's granularity. Screens need `performance.now()` and a re-run to
#: earn decimals; until then they stay whole.
n0 = lambda v: f"{v:.0f}"


class Notes:
    """Deduplicated, numbered explanations, each remembering every cell that points at
    it. A comparability reason is long and repeats across rows and load levels, so the
    cell carries a marker and the text is printed once at the end — with the list of
    server/row/table it came from, because a reason with no "where" cannot be acted on."""

    def __init__(self):
        #: reason -> its 1-based number, in first-seen order (dicts keep insertion order)
        self.index = {}
        #: reason -> the cells that point at it, as `server · row (table)` strings
        self.sources = {}

    def add(self, text, source=None):
        """The 1-based number for `text`, assigning one the first time it is seen and
        recording `source` (a `server · row (table)` string) among its references."""
        n = self.index.setdefault(text, len(self.index) + 1)
        if source:
            self.sources.setdefault(text, [])
            if source not in self.sources[text]:
                self.sources[text].append(source)
        return n

    @property
    def items(self):
        """`(number, reason, sources)` in numbered order."""
        return [(n, text, self.sources.get(text, [])) for text, n in self.index.items()]

    def __bool__(self):
        return bool(self.index)


def latency_cell(per_run, flag, fmt=sig3):
    """per_run: k6 summary entries (or None) for one screen/endpoint across runs.
    `fmt` is `n0` for screens, whose source data is whole milliseconds."""
    return Cell([x["p50"] if x else None for x in per_run], fmt, flag, {
        "p95": Cell([x["p95"] if x else None for x in per_run], fmt),
        "p99": Cell([x["p99"] if x else None for x in per_run], fmt),
        "err": max(((1 - x["ok"]) * 100 for x in per_run if x and x.get("ok") is not None), default=0.0),
    })


def md_cell(c, notes, source=None, oracle_cell=None):
    """`p50 / p95 / p99 (err) ⚠[n]` — the numbers, each with the range its runs spanned
    where they disagreed (markdown has no hover, so it goes in the cell), and `⚠[n]`
    pointing at the note saying why the cell is not comparable, hence not publishable."""
    if not c.vals:
        return "—"
    txt = c.with_ranges()
    if "p95" in c.sub:
        txt += f" ({c.sub['err']:.2f}%)"
    gain = speedup(c, oracle_cell)
    if gain:
        txt += f" — {gain}"
    if c.flag:
        txt += f" ⚠[{notes.add(c.flag, source)}]"
    return txt


def build(runs):
    """Everything both renderers need, computed once from the run dirs."""
    metas = [load(os.path.join(r, "run.json")) or {} for r in runs]
    meta = metas[0]
    servers = [(k, label) for k, label in SERVERS if any(os.path.isdir(os.path.join(r, k)) for r in runs)]
    per = {k: [os.path.join(r, k) for r in runs] for k, _ in servers}
    shapes = {k: [load_shape(d) for d in ds] for k, ds in per.items()}
    oracle_shape = shapes[ORACLE][0] if ORACLE in shapes else None
    missing = []
    m = {"meta": meta, "runs": runs, "servers": servers, "levels": {}, "missing": missing}

    for level in LOAD_LEVELS:
        data = {k: [load(os.path.join(d, f"k6-{level}.json")) for d in ds] for k, ds in per.items()}
        # `windows.json` says the level ran; a missing k6 file then means it failed,
        # which has to be reported. Only a level no run executed disappears silently.
        ran = any(level in run_levels(d) for ds in per.values() for d in ds)
        for k, ds in per.items():
            for d, x in zip(ds, data[k]):
                if x is None and level in run_levels(d):
                    missing.append(f"{k}: k6-{level}.json")
        names_of = defaultdict(set)
        for xs in data.values():
            for x in xs:
                if x:
                    for n in x["endpoints"]:
                        names_of[n.split(":")[0]].add(n)
        dropped = {k: sum((x or {}).get("dropped_iterations", 0) for x in data[k]) for k in data}

        def flag(k, names):
            if dropped.get(k):
                return f"invalid window: k6 dropped {dropped[k]} iterations"
            if k == ORACLE:
                return oracle_failed(oracle_shape, names)
            return comparable(shapes[k][0], oracle_shape, names)

        # a screen row is judged on its API requests; poster fetches ('image') show up in the
        # image endpoint row and in err%, and the list endpoints' ImageTags superset check
        # already proves the same posters exist on every server
        screens = [(scr, {k: latency_cell([x["screens"].get(scr) if x else None for x in data[k]], flag(k, sorted(names_of[scr])), n0)
                          for k, _ in servers}) for scr in SCREENS]
        names = [n for scr in SCREENS for n in sorted(names_of[scr])] + (["image"] if "image" in names_of else [])
        endpoints = [(n, {k: latency_cell([x["endpoints"].get(n) if x else None for x in data[k]], flag(k, [n]))
                          for k, _ in servers}) for n in names]
        # A level no run executed (an older run, or `--only`) is dropped rather than
        # rendered as an empty section by whichever renderer forgets to check. A level
        # that ran and failed keeps its (empty) section and its missing-phase entries.
        if ran or any(any(xs) for xs in data.values()):
            rates = {r["rate_" + level] for r in metas if r.get("rate_" + level) is not None}
            rate = rates.pop() if len(rates) == 1 else ("?" if not rates else
                                                        "/".join(str(x) for x in sorted(rates)))
            m["levels"][level] = {"rate": rate, "screens": screens,
                                  "endpoints": endpoints, "any_data": True}

    # time to first screen
    cold, hls, direct, turl = {}, {}, {}, {}
    for k, ds in per.items():
        cs = [load(os.path.join(d, "coldstart.json")) for d in ds]
        if any(c is None for c in cs):
            missing.append(f"{k}: coldstart.json")
        cold[k] = Cell([statistics.median([r["home_ms"] for r in c["runs"] if r["home_ms"]]) if c and any(r["home_ms"] for r in c["runs"]) else None for c in cs], ms)
        ts = [load(os.path.join(d, "ttfs.json")) for d in ds]
        if any(t is None for t in ts):
            missing.append(f"{k}: ttfs.json")
        errs = [h["error"] for t in ts if t for h in t["hls"] if "error" in h]
        # fewer successful reps than specified is different work, so failures flag the cell
        hls[k] = Cell([statistics.median([h["ttfs_ms"] for h in t["hls"] if "ttfs_ms" in h]) if t and any("ttfs_ms" in h for h in t["hls"]) else None for t in ts], ms,
                      f"{len(errs)} rep(s) failed: {errs[0][:60]}" if errs else None)
        direct[k] = Cell([statistics.median([h["ttfb_ms"] for h in t["direct"] if "ttfb_ms" in h]) if t and any("ttfb_ms" in h for h in t["direct"]) else None for t in ts], ms)
        for t in ts:
            for h in (t or {}).get("hls", []):
                if h.get("transcoding_url"):
                    # ffmpeg-relevant parameters only: session/auth/tag identify the request and
                    # TranscodeReasons is informational (it differs between versions without changing an ffmpeg argument)
                    turl[k] = sorted(p.lower() for p in h["transcoding_url"].split("?")[-1].split("&")
                                     if p and not p.lower().startswith(TRANSCODE_NOISE))
                    break
    if ORACLE in turl:
        for k in turl:
            if k != ORACLE and turl[k] != turl[ORACLE]:
                diff = sorted(set(turl[k]) ^ set(turl[ORACLE]))
                reason = f"transcode parameters differ: {', '.join(diff)[:200]}"
                hls[k].flag = f"{hls[k].flag}; {reason}" if hls[k].flag else reason
    m["ttfs"] = [("cold start (restart → home screen)", cold), ("HLS first segment (forced transcode)", hls), ("direct-play TTFB (1 MiB range)", direct)]

    # memory
    mems = {k: [mem_numbers(d) for d in ds] for k, ds in per.items()}
    for k, x in mems.items():
        if any(v is None for v in x):
            missing.append(f"{k}: mem.csv/windows.json")
    # The gate asks the run what it ran, so a window that vanished still invalidates the
    # peak computed from it — checking only the files that exist cannot fail.
    windows = {k: [load(os.path.join(d, f"k6-{lvl}.json"))
                   for d in ds for lvl in run_levels(d) if lvl in PEAK_LEVELS]
               for k, ds in per.items()}
    mixed = None if same_load_shape([d for ds in per.values() for d in ds]) else (
        "runs measured different load levels, so this row mixes two definitions")
    peak = {}
    for k, ds in per.items():
        bad = [x for x in windows[k] if x is None or x.get("dropped_iterations")]
        # the peak is only meaningful if the loaded window that produced it was the specified load
        peak[k] = Cell([(x or {}).get("peak") for x in mems[k]], mib,
                       mixed or ("a load window is missing or dropped iterations" if bad else None),
                       unit="MiB")
    pct = lambda v: f"{v * 100:.0f}%"
    m["memory"] = [
        ("peak under load", peak),
        ("steady idle", {k: Cell([(x or {}).get("steady") for x in mems[k]], mib, mixed, unit="MiB")
                         for k, _ in servers}),
        # interference and swap describe the host while that server ran, not the server
        ("interference on the server's cores, p95", {k: Cell([(x or {}).get("interference_p95") for x in mems[k]], pct, unit="%", context=True) for k, _ in servers}),
        ("interference, max single sample", {k: Cell([(x or {}).get("interference") for x in mems[k]], pct, unit="%", context=True) for k, _ in servers}),
        ("max swap", {k: Cell([(x or {}).get("swap_max") for x in mems[k]], mib, unit="MiB", context=True) for k, _ in servers}),
    ]
    m["sample_ms"] = meta.get("mem_sample_ms", "?")

    # the work list: every divergence from the oracle, per server
    counts = {k: [load(os.path.join(d, "counts.json")) for d in ds] for k, ds in per.items()}
    work = {}
    for k, _ in servers:
        if k == ORACLE or not shapes[k][0] or not oracle_shape:
            continue
        items = []
        for n in sorted(oracle_shape):
            r = comparable(shapes[k][0], oracle_shape, [n])
            if r and " failed (" not in r:  # the oracle's own failures are not this server's work
                items.append(r)
        if counts[k][0] and counts[ORACLE][0] and counts[k][0] != counts[ORACLE][0]:
            for n in counts[ORACLE][0]:
                if counts[k][0].get(n) != counts[ORACLE][0].get(n):
                    items.append(f"count {n}: {counts[k][0].get(n)} vs {counts[ORACLE][0].get(n)}")
        work[k] = items
    m["work"] = work
    m["oracle_failures"] = [n for n in sorted(oracle_shape or {}) if oracle_failed(oracle_shape, [n])]
    m["work_total"] = len(oracle_shape or {}) - len(m["oracle_failures"])
    return m


# ── markdown (the README tables) ────────────────────────────────────────────
def render_md(m):
    meta, servers, out = m["meta"], m["servers"], []
    notes = Notes()
    p = out.append
    p(f"## Ferrofin vs Jellyfin — {len(m['runs'])} run(s), commit {meta.get('sha', '?')}, {meta.get('date', '')[:10]}")
    p(f"Host {meta.get('cpu', '?')} · server on cpus {meta.get('server_cpus', '?')} · {meta.get('memory_limit', '?')} limit · "
      f"test data {meta.get('testdata_counts', {})} · windows {meta.get('window_s', '?')} s · "
      + " · ".join(f"{lvl} {m['levels'][lvl]['rate']} screens/s" for lvl in LOAD_LEVELS if lvl in m["levels"]))
    p("Cells are the median across runs"
      + ("; where the runs disagreed, the range they spanned follows in brackets" if len(m["runs"]) > 1 else "")
      + f". `⚠[n]` means the server did different work than {ORACLE_LABEL} (status / record count / "
      "missing fields), so the number is not comparable: it is kept for the work list, not for publication, "
      f"and note `n` says why. `X.Y× faster` compares the cell with {ORACLE_LABEL} on the same row. It is "
      "shown on flagged cells too — the note says the two servers did not do identical work, so read it as "
      "an indication rather than a like-for-like result.\n")
    head = "| {} | " + " | ".join(f"{label} p50 / p95 / p99 ms (err)" for _, label in servers) + " |\n|" + "---|" * (len(servers) + 1)
    for level, lv in m["levels"].items():
        p(f"### Latency — {level} ({lv['rate']} screens/s)\n")
        p(head.format("screen"))
        for name, cells in lv["screens"]:
            p(f"| {name} | " + " | ".join(
                md_cell(cells[k], notes, f"{lbl} · {name} ({level} screens)",
                        cells.get(ORACLE) if k != ORACLE else None) for k, lbl in servers) + " |")
        p("")
        p(head.format("endpoint"))
        for name, cells in lv["endpoints"]:
            p(f"| {name} | " + " | ".join(
                md_cell(cells[k], notes, f"{lbl} · {name} ({level} endpoints)",
                        cells.get(ORACLE) if k != ORACLE else None) for k, lbl in servers) + " |")
        p("")
    p("### Time to first screen\n")
    p("| | " + " | ".join(label for _, label in servers) + " |\n|" + "---|" * (len(servers) + 1))
    for name, cells in m["ttfs"]:
        p(f"| {name} | " + " | ".join(
            md_cell(cells[k], notes, f"{lbl} · {name} (time to first screen)",
                    cells.get(ORACLE) if k != ORACLE else None) for k, lbl in servers) + " |")
    p(f"\n### Memory (anon, cache excluded, {m['sample_ms']} ms samples)\n")
    p("| | " + " | ".join(label for _, label in servers) + " |\n|" + "---|" * (len(servers) + 1))
    for name, cells in m["memory"]:
        p(f"| {name} | " + " | ".join(
            md_cell(cells[k], notes, f"{lbl} · {name} (memory)",
                    cells.get(ORACLE) if k != ORACLE else None) for k, lbl in servers) + " |")
    if m["work"]:
        p(f"\n### Response shape vs {ORACLE_LABEL} (supporting evidence, not the parity number)\n")
        for k, items in m["work"].items():
            p(f"**{k}**: {len(items)} divergence(s) across {m['work_total']} compared requests")
            for it in items:
                p(f"- {it}")
            p("")
    if m["oracle_failures"]:
        p(f"**{ORACLE_LABEL} failed**: " + ", ".join(m["oracle_failures"]) + "\n")
    if m["missing"]:
        p("### Missing phases\n")
        for x in sorted(set(m["missing"])):
            p(f"- {x}")
        p("")
    if notes:
        p("")
        p("### Notes\n")
        for n, text, sources in notes.items:
            p(f"{n}. {text}")
            if sources:
                p(f"   — {'; '.join(sources)}")
    return "\n".join(out) + "\n"


# ── README section (headline table + Mermaid charts) ────────────────────────
README_BEGIN = "<!-- BEGIN GENERATED BENCHMARKS"
README_END = "<!-- END GENERATED BENCHMARKS -->"
#: The level the README quotes: five screens a second is the closest of the three to
#: ordinary use (unloaded is a control, stress exists to find the knee).
README_LEVEL = "loaded"
#: The time-to-first-screen and memory rows the README headlines, by row name. HLS is
#: excluded on purpose: the three servers pick different transcode parameters, so no two
#: of them are comparable. The interference/swap rows describe the run, not the server.
README_TTFS = ("cold start (restart → home screen)", "direct-play TTFB (1 MiB range)")
README_MEMORY = ("peak under load", "steady idle")
#: x-axis labels for the headline chart — the table keeps the full row names.
README_SHORT = {"cold start (restart → home screen)": "cold start", "direct-play TTFB (1 MiB range)": "direct-play TTFB",
                "peak under load": "peak memory", "steady idle": "idle memory"}


def _ratio(c, oracle_cell):
    """theirs/mine as a float, or None — the number behind `speedup()`'s sentence."""
    if c is None or oracle_cell is None or c.context:
        return None
    mine, theirs = displayed(c), displayed(oracle_cell)
    return theirs / mine if mine and theirs else None


def _mermaid_bars(title, labels, values, y_label):
    """One single-series `xychart-beta` block. GitHub renders these natively in both
    themes and Mermaid has no legend, which is why the README charts plot one series —
    Ferrofin's multiple over the oracle — and leave absolute numbers to the table."""
    top = max(values + [1.0])
    top = float(int(top) + 1) if top == int(top) else float(int(top) + 1)
    labs = ", ".join(f'"{l}"' for l in labels)
    vals = ", ".join(f"{v:.1f}" for v in values)
    horizontal = " horizontal"  # category labels never collide on the y axis
    # The viewer's accent, so the README and the comparison page read as one instrument.
    init = '%%{init: {"themeVariables": {"xyChart": {"plotColorPalette": "#2E6E8E"}}}}%%'
    return (f"```mermaid\n{init}\nxychart-beta{horizontal}\n  title \"{title}\"\n  x-axis [{labs}]\n"
            f"  y-axis \"{y_label}\" 0 --> {top:g}\n  bar [{vals}]\n```")


def render_readme(m, run_dirs):
    """The README `## Benchmarks` body: setup sentence, the headline table with a
    "Ferrofin vs oracle" column, two Mermaid speedup charts (screens + headline rows,
    then every endpoint), the spread sentence computed from the runs, and the pointers to
    the full tables. Wrapped in markers so `--readme README.md` can replace it in place;
    everything inside the markers is generated — edit the prose here, not in the README."""
    meta, servers = m["meta"], m["servers"]
    lv = m["levels"].get(README_LEVEL)
    if not lv:
        sys.exit(f"the runs have no '{README_LEVEL}' level; the README section needs it")
    tc = meta.get("testdata_counts", {})
    tag = meta.get("name") or os.path.basename(run_dirs[0].rstrip("/"))
    nruns = len(m["runs"])
    notes = Notes()
    out = []
    p = out.append
    cmd = "python3 bench/report.py --readme README.md " + " ".join(f"bench/runs/{os.path.basename(r.rstrip('/'))}" for r in run_dirs)
    p(f"{README_BEGIN} — do not edit by hand. Regenerate with:\n     {cmd} -->")
    cpus = str(meta.get("server_cpus", ""))
    ncores = sum(int(b) - int(a) + 1 if "-" in part else 1 for part in cpus.split(",") if part for a, b in [part.split("-")] ) if cpus and all(x.replace("-", "").isdigit() for x in cpus.split(",")) else "?"
    limit = str(meta.get("memory_limit", "?")).replace("g", " GiB")
    article = "an" if limit[:1] in "8aeiou" else "a"
    p(f"Ferrofin `{tag}` against **{servers[0][1]}** and **{servers[1][1]}**, measured {meta.get('date', '')[:10]} "
      f"on one machine ({meta.get('cpu', '?').replace(' 16-Core Processor', '')}), each server alone in a container pinned to "
      f"{ncores} dedicated cores with {article} {limit} limit and no swap, over the same library of "
      f"{tc.get('movies', '?'):,} movies, {tc.get('series', '?'):,} series and {tc.get('episodes', '?'):,} episodes. "
      f"Every figure is the **median of {nruns} full run{'s' if nruns != 1 else ''}**. No request failed on any server at any load level.\n")
    p(f"The screen rows are what a client actually does: each is the exact request set jellyfin-web issues for that "
      f"screen, replayed at {lv['rate']} screens per second (the \"{README_LEVEL}\" level). Latency reads "
      f"**p50 / p95 / p99 in milliseconds**; the last column compares p50 with {ORACLE_LABEL}.\n")
    # ── headline table ──
    p("| | " + " | ".join(f"**{lbl}**" if k == "ferrofin" else lbl for k, lbl in servers) + f" | Ferrofin vs {ORACLE_LABEL.split()[1]} |")
    p("|---|" + "---|" * (len(servers) + 1))
    chart_labels, chart_values = [], []

    def row(name, cells, label_md, unit_suffix=""):
        cols = []
        for k, _ in servers:
            c = cells[k]
            # `value()` already carries the unit for single-number cells; latency
            # cells print p50 / p95 / p99 with the unit named in the intro sentence.
            txt = " / ".join(part.value() for part in c.parts()) if "p95" in c.sub else c.value()
            cols.append(f"**{txt}**" if k == "ferrofin" else txt)
        f, o = cells["ferrofin"], cells.get(ORACLE)
        sp = speedup(f, o) or "—"
        mark = ""
        if provisional(f, o):
            n = notes.add((f.flag or o.flag), f"{name}")
            mark = f" ⚠[{n}]"
        p(f"| {label_md} | " + " | ".join(cols) + f" | **{sp}**{mark} |")
        r = _ratio(f, o)
        if r:
            chart_labels.append(README_SHORT.get(name, name))
            chart_values.append(r)

    for name, cells in lv["screens"]:
        row(name, cells, f"**{name}** screen")
    for name, cells in m["ttfs"]:
        if name in README_TTFS:
            row(name, cells, name)
    for name, cells in m["memory"]:
        if name in README_MEMORY:
            row(name, cells, f"{name} memory")
    p("")
    p(_mermaid_bars(f"Times faster (memory: lighter) than {ORACLE_LABEL} — p50, {README_LEVEL}",
                    chart_labels, chart_values, "× better"))
    p("")
    # ── endpoints chart ──
    ep_labels, ep_values = [], []
    for name, cells in lv["endpoints"]:
        r = _ratio(cells["ferrofin"], cells.get(ORACLE))
        if r:
            ep_labels.append(name)
            ep_values.append(r)
    p(f"Per endpoint, same level and runs (the full three-server table with p95/p99 is in "
      f"[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md)):\n")
    p(_mermaid_bars(f"Per endpoint: times faster than {ORACLE_LABEL} — p50, {README_LEVEL}", ep_labels, ep_values, "× faster"))
    p("")
    # ── notes on marked rows ──
    if notes:
        p("⚠ marks a row where the servers did not do identical work, so the multiple is an indication rather "
          "than a like-for-like result:\n")
        for n, text, sources in notes.items:
            p(f"{n}. {', '.join(sources)}: {text}")
        p("")
        p(f"Ferrofin implements the Jellyfin 10.11.8 contract, so a field that 12.0 added and 10.11.8 lacks shows "
          f"up here as missing on both of them; the row is still Ferrofin's own work for everything else in it.\n")
    # ── spread sentence, computed ──
    def moved(part):
        return len(part.vals) > 1 and part.median and (max(part.vals) - min(part.vals)) > 0.15 * part.median
    p50s = tails = p50_moved = tail_moved = 0
    for _, cells in lv["screens"] + lv["endpoints"]:
        for k, _ in servers:
            parts = cells[k].parts()
            if not parts or parts[0].median is None:
                continue
            p50s += 1
            p50_moved += moved(parts[0])
            for part in parts[1:]:
                tails += 1
                tail_moved += moved(part)
    if nruns > 1:
        p(f"How steady are these numbers? Across the {nruns} runs, {p50_moved} of the {p50s} medians in the "
          f"{README_LEVEL}-level tables moved by more than 15 % of their value, against {tail_moved} of the {tails} "
          f"p95/p99 tails, so read the p50s as the numbers and the tails as shape. Run-to-run ranges for every cell, "
          f"and every place the servers did different work, are in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md); "
          f"the harness and the one-sentence definition of each number are in [`bench/README.md`](bench/README.md). "
          f"It is a deliberate, local instrument, not a CI job.")
    p(README_END)
    return "\n".join(out) + "\n"


def inject_readme(path, section):
    """Replace the block between the GENERATED BENCHMARKS markers in `path` with
    `section` (which carries its own markers). Refuses to guess when they are absent."""
    text = open(path, encoding="utf-8").read()
    i, j = text.find(README_BEGIN), text.find(README_END)
    if i < 0 or j < 0:
        sys.exit(f"{path} has no {README_BEGIN} … {README_END} block to replace")
    j += len(README_END)
    if text[j:j + 1] == "\n":
        j += 1
    open(path, "w", encoding="utf-8").write(text[:i] + section + text[j:])


# ── html (the comparison page) ──────────────────────────────────────────────
CSS = """
:root{--bg:#F5F6F8;--panel:#FFFFFF;--ink:#1A1E24;--muted:#5B636E;--rule:#D8DCE2;--rule-soft:#E9ECF0;--accent:#2E6E8E;--accent-ink:#1F4E66;--flag:#9A6A12;--flag-bg:#FBF3E2;--err:#A83A2E;--good:#2E7D4F;--bad:#A83A2E}
@media (prefers-color-scheme:dark){:root:not([data-theme="light"]){--bg:#13161B;--panel:#1B1F26;--ink:#E7EAEE;--muted:#9AA3AE;--rule:#333A44;--rule-soft:#262C35;--accent:#6FB3D2;--accent-ink:#A9D6EA;--flag:#E0B25A;--flag-bg:#2B2517;--err:#E07A6E;--good:#6CC08B;--bad:#E07A6E}}
:root[data-theme="dark"]{--bg:#13161B;--panel:#1B1F26;--ink:#E7EAEE;--muted:#9AA3AE;--rule:#333A44;--rule-soft:#262C35;--accent:#6FB3D2;--accent-ink:#A9D6EA;--flag:#E0B25A;--flag-bg:#2B2517;--err:#E07A6E;--good:#6CC08B;--bad:#E07A6E}
body{background:var(--bg);color:var(--ink);font:15px/1.55 system-ui,-apple-system,"Segoe UI",sans-serif;margin:0}
main{max-width:1080px;margin:0 auto;padding:40px 24px 80px}
h1{font:700 32px/1.1 system-ui,-apple-system,"Segoe UI",sans-serif;letter-spacing:-.01em;margin:0 0 6px;text-wrap:balance}
h2{font:600 20px/1.2 system-ui,-apple-system,"Segoe UI",sans-serif;margin:44px 0 10px}
h3{font:600 12.5px/1.2 system-ui,-apple-system,"Segoe UI",sans-serif;text-transform:uppercase;letter-spacing:.06em;color:var(--muted);margin:0 0 10px}
.lede{color:var(--muted);max-width:70ch;margin:0 0 6px}
.meta{font:12.5px/1.5 ui-monospace,"SF Mono",Menlo,Consolas,monospace;color:var(--muted)}
/* Two tiles per row at most. The old auto-fit went to three across on a wide window and
   held a 300px track floor on a narrow one, and either way the three server columns
   inside a tile could not shrink — so the page itself scrolled sideways (559px of
   content in a 320px viewport, 1245px in a 1200px one). */
.tiles{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:14px;margin-top:22px}
@media (max-width:700px){.tiles{grid-template-columns:1fr}}
.tile{background:var(--panel);border:1px solid var(--rule);border-radius:6px;padding:16px 18px}
/* The columns re-flow (three, then two, then one) instead of being pinned to three, and
   the floor is a real width rather than 0. Pinned at three, a narrow tile pushes the page
   sideways; with a 0 floor the page fits, but only because a column shrinks under its own
   text, which cannot wrap — the comparison is nowrap — and so paints over its neighbour.
   150px clears the widest comparison ("about the same", 96.6px) and the widest value. */
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px}
/* A column carries a comparison only if it has one to make, and the range line beneath
   wraps at some widths and not others, so all three differ in height. Subgrid puts the
   label, the value and the range on the tile's own rows, which is what makes the three
   range lines start level; bottom-pinning them only levelled the bottoms. `row-gap:0`
   because a subgrid inherits its parent's gutter, and 12px of column gutter arriving as
   vertical space would push the three rows apart. The range line's leading is set here
   too — 1.35 against the body's 1.55 — so it reads as a caption under the number rather
   than as another line of body text. */
.stat{display:grid;grid-template-rows:subgrid;grid-row:span 3;row-gap:0}
.stat .tail{line-height:1.35}
.stat .who{font-size:11.5px;color:var(--muted)}
.stat .val{font:500 24px/1.2 ui-monospace,"SF Mono",Menlo,Consolas,monospace;font-variant-numeric:tabular-nums;margin-top:2px}
/* The comparison starts its own line rather than trailing the number, so the number
   never has to share its line. A forced break, NOT display:block — the footnote marker
   is emitted after the comparison, and a block box would strand that superscript on a
   third line of its own, where an amber "13" reads as one more measured number. The
   inherited nowrap stays. It is not load-bearing at any width this page reaches — the
   comparison owns its own line, so it has the whole column — but `white-space:normal`
   would split "about the same" once a column fell under ~100px (~113px where a footnote
   marker follows it, since no whitespace separates the two), and the halves would be
   held 28.8px apart by the value's line box, reading as two separate things. And the
   marker is sized here, or it inherits `smaller` of the 24px value — 20px, larger than
   the comparison it annotates and amber against a page of numbers. */
.stat .val .ratio,.stat .val .delta{margin-left:0;font-size:11.5px}
.stat .val .ratio::before,.stat .val .delta::before{content:"\\A";white-space:pre}
.stat .val sup.fn{font-size:11.5px}
.stat .unit{font-size:12px;color:var(--muted);margin-left:4px}
.stat.ferrofin .val{color:var(--accent-ink)}
.stat .ratio,.ratio{font:12px ui-monospace,"SF Mono",Menlo,Consolas,monospace;color:var(--muted);margin-left:6px;white-space:nowrap}
.ratio.win{color:var(--good);font-weight:600}
.ratio.provisional,.delta.provisional{color:var(--flag);font-style:italic;font-weight:500}
.delta{font:12px ui-monospace,"SF Mono",Menlo,Consolas,monospace;margin-left:6px}.delta.good{color:var(--good)}.delta.bad{color:var(--bad)}
.legend{display:flex;flex-wrap:wrap;gap:18px;font-size:12.5px;color:var(--muted);margin:8px 0 14px}
.legend .chip{display:inline-block;width:10px;height:10px;border-radius:2px;background:var(--flag-bg);border:1px solid var(--flag);vertical-align:-1px;margin-right:6px}
.scroll{overflow-x:auto;background:var(--panel);border:1px solid var(--rule);border-radius:6px}
table{border-collapse:collapse;width:100%;font-size:13.5px}
th,td{padding:8px 12px;border-bottom:1px solid var(--rule-soft);text-align:left;vertical-align:top}
thead th{font:600 11.5px/1.3 system-ui,-apple-system,"Segoe UI",sans-serif;text-transform:uppercase;letter-spacing:.06em;color:var(--muted);border-bottom:1px solid var(--rule)}
thead th.ferrofin{color:var(--accent-ink)}
tbody th{font-weight:500;white-space:nowrap}
td.num{font-family:ui-monospace,"SF Mono",Menlo,Consolas,monospace;font-variant-numeric:tabular-nums;white-space:nowrap}
.p50{font-weight:500}.tail{color:var(--muted)}
td.num.has-title{cursor:help}  /* only where the title actually says something */
.err{color:var(--err);font-size:12px;margin-left:6px;font-weight:600}
td.flagged{background:var(--flag-bg)}td.flagged .p50,td.flagged .tail{opacity:.7}
details{margin-top:10px}summary{cursor:pointer;color:var(--accent-ink);font-size:13.5px}
.work{background:var(--panel);border:1px solid var(--rule);border-radius:6px;padding:14px 18px;margin-top:12px}
.work h3 .n{text-transform:none;letter-spacing:0;font-weight:400;color:var(--muted);margin-left:8px}
.work ul{margin:0;padding-left:18px;font-size:13.5px}.work li{margin:3px 0;overflow-wrap:anywhere}
code{font:12.5px ui-monospace,"SF Mono",Menlo,Consolas,monospace}
sup.fn{margin-left:3px}sup.fn a{color:var(--flag);font-weight:600;text-decoration:none}
ol.notes{font-size:13px;color:var(--muted);max-width:90ch;padding-left:22px}
ol.notes li{margin:8px 0;overflow-wrap:anywhere}ol.notes li:target{color:var(--ink);font-weight:500}
ol.notes .src{font:12px ui-monospace,"SF Mono",Menlo,Consolas,monospace;color:var(--muted);opacity:.85;margin-top:2px}
"""


def provisional(c, oracle_cell):
    """Whether a comparison of these two cells is caveated — either side measured
    different work than the other, so the number is informative but not like for like.
    The cell keeps its amber ground and its note; the comparison is shown anyway,
    because "how far apart are they" is still the question being asked. Run-to-run
    spread is not a caveat here: it is reported, not judged (see the module docstring).
    Both places this comparison is drawn put the range in front of the reader — beneath
    the number in a headline tile, on the cell's hover text in a table — so a wide spread
    is there to be weighed without colouring the comparison."""
    return bool(c.flag or (oracle_cell is not None and oracle_cell.flag))


def displayed(c):
    """The cell's median as the page prints it — the number a reader can check a
    multiple against. `None` when it rounds away to zero at that precision."""
    if c.median is None:
        return None
    digits = "".join(ch for ch in c.fmt(c.median) if ch.isdigit() or ch == ".")
    try:
        return float(digits) or None
    except ValueError:
        return None


def speedup(c, oracle_cell):
    """`X.Y× faster` against the oracle, or None when there is nothing checkable to
    compare (a context row, a missing median, or either side rounding to zero at the
    printed precision — `13.7×` derived from `1 ms` vs `7 ms` is spurious). A raw ratio
    like `×0.10` makes the reader do the division and invert it; this is the sentence
    they were going to write anyway, computed from the printed numbers so it can be
    checked by eye. Memory says lighter/heavier. A caveated comparison is still
    returned — `provisional` decides how it is presented."""
    if c.context or oracle_cell is None:
        return None
    mine, theirs = displayed(c), displayed(oracle_cell)
    if not mine or not theirs:
        return None
    better, worse = ("lighter", "heavier") if c.unit == "MiB" else ("faster", "slower")
    ratio = theirs / mine
    if f"{ratio:.1f}" == "1.0":
        return "about the same"
    return f"{ratio:.1f}× {better}" if ratio > 1 else f"{1 / ratio:.1f}× {worse}"


def ratio_html(c, oracle_cell):
    """The speedup against the oracle, for a table cell. A caveated one is muted rather
    than green and says so on hover, so a win that is not like for like never reads as
    a clean one."""
    text = speedup(c, oracle_cell)
    if not text:
        return ""
    caveat = provisional(c, oracle_cell)
    won = "faster" in text or "lighter" in text
    cls = "ratio provisional" if caveat else ("ratio win" if won else "ratio")
    title = f"vs {ORACLE_LABEL}" + (" — not like for like, see the note" if caveat else "")
    return f"<span class='{cls}' title='{html.escape(title)}'>{html.escape(text)}</span>"


def same_load_shape(dirs):
    """Whether every run dir measured the same load levels. The memory rows are defined
    by which windows ran — "peak under load" spans them and "steady idle" follows the
    last — so a median taken across runs that ran different levels silently blends two
    definitions of the row, which no other rule here would catch."""
    return len({frozenset(run_levels(d)) for d in dirs}) <= 1


def comparable_levels(m, base):
    """Whether two run sets measured the same load levels. The memory rows change
    meaning across that boundary — "peak under load" spans a heavier set of windows and
    "steady idle" follows a different last window — so a delta between them would report
    a definition change as a regression."""
    return base is None or set(m["levels"]) == set(base["levels"])


def delta_html(c, base):
    """Change against the same server in the baseline run; lower is better for every non-context
    number. Only when both numbers stand (a flagged cell is not a valid number to move from or to)."""
    if c.context or base is None or not c.median or not base.median:
        return ""
    ch = (c.median - base.median) / base.median * 100
    if c.flag or base.flag:
        cls = "provisional"
    else:
        cls = "good" if ch < -DELTA_NOISE_PCT else "bad" if ch > DELTA_NOISE_PCT else ""
    return f"<span class='delta {cls}' title='vs baseline {base.fmt(base.median)}'>{ch:+.0f}%</span>"


def td_html(c, oracle_cell, base, notes, source=None):
    """One table cell: the numbers, then markers. The spread lands in the hover text
    and the comparability reason in a numbered note, because inline they made a
    three-server table unreadable."""
    if not c.vals:
        return "<td class='num'>—</td>"
    e = html.escape
    def one(part, cls):
        """One number. The ranges live on the `<td>`'s own hover text, which labels all
        three parts at once; a second title on the span would mask it with less."""
        return f"<span class='{cls}'>{e(part.value())}</span>"

    parts = c.parts()
    body = one(parts[0], "p50")
    for part in parts[1:]:
        body += " <span class='tail'>/</span> " + one(part, "tail")
    if c.sub.get("err", 0) > 0:
        body += f" <span class='err'>{c.sub['err']:.2f}% err</span>"
    body += ratio_html(c, oracle_cell) + delta_html(c, base)
    cls = "num"
    if c.flag:
        cls += " flagged"
        n = notes.add(c.flag, source)
        body += f"<sup class='fn'><a href='#n{n}'>{n}</a></sup>"
    title = c.spreads()
    if title:
        cls += " has-title"
    attr = f" title='{e(title)}'" if title else ""
    return f"<td class='{cls}'{attr}>{body}</td>"


def notes_html(notes):
    """The numbered reasons the cells point at, each with the cells that point at it."""
    if not notes:
        return ""
    e = html.escape
    lis = ""
    for n, text, sources in notes.items:
        src = f"<div class='src'>{e('; '.join(sources))}</div>" if sources else ""
        lis += f"<li id='n{n}'>{e(text)}{src}</li>"
    return f"<h2>Notes</h2><ol class='notes'>{lis}</ol>"


def table_html(first, rows, servers, base_rows, notes, where=""):
    e = html.escape
    head = "".join(f"<th class='{k}'>{e(lbl)}</th>" for k, lbl in servers)
    body = []
    for name, cells in rows:
        bcells = base_rows.get(name, {}) if base_rows else {}
        tds = "".join(td_html(cells[k], cells.get(ORACLE) if k != ORACLE else None, bcells.get(k), notes,
                              f"{lbl} · {name}" + (f" ({where})" if where else ""))
                      for k, lbl in servers)
        body.append(f"<tr><th>{e(name)}</th>{tds}</tr>")
    return f"<div class='scroll'><table><thead><tr><th>{e(first)}</th>{head}</tr></thead><tbody>{''.join(body)}</tbody></table></div>"


def prewalk_notes(m, notes):
    """Assign the note numbers in the markdown's order before the page is built.
    The viewer renders its headline tiles first, so without this the same reason is
    note 2 in one output and note 14 in the other — and the two are read side by side."""
    for level, lv in m["levels"].items():
        for kind in ("screens", "endpoints"):
            for name, cells in lv[kind]:
                for k, lbl in m["servers"]:
                    if cells[k].flag:
                        notes.add(cells[k].flag, f"{lbl} · {name} ({level} {kind})")
    for label, rows in (("time to first screen", m["ttfs"]), ("memory", m["memory"])):
        for name, cells in rows:
            for k, lbl in m["servers"]:
                if cells[k].flag:
                    notes.add(cells[k].flag, f"{lbl} · {name} ({label})")


def render_html(m, base=None, picker=""):
    e = html.escape
    notes = Notes()
    prewalk_notes(m, notes)
    meta, servers = m["meta"], m["servers"]
    base_l = (base or {}).get("levels", {})
    base_ttfs = dict((base or {}).get("ttfs", []))
    # The memory rows mean different things either side of a change in which load levels
    # ran, so a delta across that boundary would report a redefinition as a regression.
    base_mem = dict((base or {}).get("memory", [])) if comparable_levels(m, base) else {}
    tc = meta.get("testdata_counts", {})
    title = f"Ferrofin Benchmark {meta.get('sha', '')}".strip()
    parts = ["<!doctype html><html lang='en'><head><meta charset='utf-8'><meta name='viewport' content='width=device-width, initial-scale=1'>",
             f"<title>{e(title)}</title><style>{CSS}</style></head><body><main>{picker}",
             f"<h1>Ferrofin vs Jellyfin — {len(m['runs'])} run{'s' if len(m['runs']) != 1 else ''}, commit {e(meta.get('sha', '?'))}</h1>",
             "<p class='lede'>Every number is the median across the runs given"
             + (", and where those runs disagreed you can hover a cell for the range they spanned"
                if len(m["runs"]) > 1 else "")
             + ". An amber cell is <em>not comparable</em>: "
             f"the server did different work than {ORACLE_LABEL} — the number stays for the work list, not for the README, "
             "and the superscript points at the reason under Notes. "
             f"<b>X.Y× faster</b> compares the cell with {ORACLE_LABEL} on the same row (memory says lighter); "
             "on an amber cell it is shown in amber italics, because the two servers did not do identical work. "
             "A coloured % is the change against the baseline run.</p>",
             f"<p class='meta'>{e(meta.get('date', '')[:16].replace('T', ' '))} UTC · {e(meta.get('cpu', '?'))} · server cores {e(str(meta.get('server_cpus', '?')))} · "
             f"{e(str(meta.get('memory_limit', '?')))} limit, no swap · test data {e(str(tc.get('movies', '?')))} movies / {e(str(tc.get('series', '?')))} series / {e(str(tc.get('episodes', '?')))} episodes · "
             f"windows {e(str(meta.get('window_s', '?')))} s · runs: {e(', '.join(os.path.basename(r.rstrip('/')) for r in m['runs']))}"
             + (f" · baseline: {e(os.path.basename(base['runs'][0].rstrip('/')))}" if base else "") + "</p>"]
    # tiles: the four headline numbers
    tiles = []
    for name, cells in m["ttfs"][:2] + m["memory"][:2]:
        stats = ""
        for k, lbl in servers:
            c = cells[k]
            val = sig3(c.median) if c.median is not None else "—"
            unit = c.unit
            extra = ratio_html(c, cells.get(ORACLE) if k != ORACLE else None) + delta_html(c, (base_ttfs.get(name) or base_mem.get(name) or {}).get(k))
            why = ""
            if c.flag:
                n = notes.add(c.flag, f"{lbl} · {name} (headline)")
                extra += f"<sup class='fn'><a href='#n{n}'>{n}</a></sup>"
            rng = c.spread_text()
            if rng:
                why = f"<div class='tail' style='font-size:12px'>{rng}</div>"
            stats += f"<div class='stat {k}'><div class='who'>{e(lbl)}</div><div class='val'>{val}<span class='unit'>{unit}</span>{extra}</div>{why}</div>"
        tiles.append(f"<section class='tile'><h3>{e(name)}</h3><div class='stats'>{stats}</div></section>")
    parts.append(f"<div class='tiles'>{''.join(tiles)}</div>")
    for level, lv in m["levels"].items():
        if not lv["any_data"]:
            continue
        parts.append(f"<h2>Screens — {level}, {e(str(lv['rate']))} screens/s</h2>")
        parts.append("<div class='legend'><span>p50 <span style='color:var(--muted)'>/ p95 / p99</span> ms per screen (all of its requests, concurrently)</span>" + ("<span>hover a number for the range its runs spanned</span>" if len(m["runs"]) > 1 else "") + "<span><span class='chip'></span>not comparable — superscript links to the reason</span></div>")
        bl = base_l.get(level, {})
        parts.append(table_html("screen", lv["screens"], servers, dict(bl.get("screens", [])), notes, f"{level} screens"))
        parts.append(f"<details><summary>Per endpoint, {level}</summary>{table_html('endpoint', lv['endpoints'], servers, dict(bl.get('endpoints', [])), notes, f'{level} endpoints')}</details>")
    parts.append("<h2>Time to first screen</h2>" + table_html("", m["ttfs"], servers, base_ttfs, notes, "time to first screen"))
    mem_note = "" if comparable_levels(m, base) else (
        "<p class='lede'>The baseline ran different load levels, so no change is shown on the "
        "memory rows: <em>peak under load</em> spans a different set of windows and "
        "<em>steady idle</em> follows a different one, which would read as a regression.</p>")
    parts.append(f"<h2>Memory — anon, cache excluded, {e(str(m['sample_ms']))} ms samples</h2>"
                 + mem_note + table_html("", m["memory"], servers, base_mem, notes, "memory"))
    if m["work"]:
        parts.append(f"<h2>The work list — divergences from {ORACLE_LABEL}</h2><p class='lede'>From the shape pass (status, record count, field set) and the item counts. Each is a server fix or a recorded, accepted divergence.</p>")
        for k, items in m["work"].items():
            lis = "".join(f"<li>{e(it)}</li>" for it in items)
            parts.append(f"<div class='work'><h3>{e(dict(servers)[k])} <span class='n'>{len(items)} divergence(s) across {m['work_total']} compared requests</span></h3><ul>{lis}</ul></div>")
    if m["oracle_failures"]:
        parts.append(f"<p class='lede'><b>{ORACLE_LABEL} failed</b>: " + e(", ".join(m["oracle_failures"])) + "</p>")
    if m["missing"]:
        parts.append("<h2>Missing phases</h2><ul>" + "".join(f"<li>{e(x)}</li>" for x in sorted(set(m["missing"]))) + "</ul>")
    parts.append(notes_html(notes))
    parts.append("<p class='meta'>Methodology: bench/README.md · the raw file behind every cell lives in the run dir.</p></main></body></html>")
    return "\n".join(parts)


PICKER_CSS = """
.picker{background:var(--panel);border:1px solid var(--rule);border-radius:6px;padding:12px 16px;margin-bottom:26px;font-size:13.5px}
.picker h3{margin-bottom:6px}.picker label{display:block;margin:2px 0;font-family:ui-monospace,"SF Mono",Menlo,Consolas,monospace;font-size:12.5px}
.picker .row{display:flex;flex-wrap:wrap;gap:18px;align-items:flex-start}.picker .col{min-width:260px}
.picker select,.picker button{font:inherit;padding:3px 8px}.picker button{background:var(--accent);color:#fff;border:0;border-radius:4px;padding:5px 12px;cursor:pointer}
"""


def list_runs(runs_dir):
    """Run dirs under runs_dir (anything with a run.json), newest first by the run's own date."""
    out = []
    for name in os.listdir(runs_dir):
        d = os.path.join(runs_dir, name)
        meta = load(os.path.join(d, "run.json"))
        if isinstance(meta, dict):
            srv = [k for k, _ in SERVERS if os.path.isdir(os.path.join(d, k))]
            out.append((meta.get("date", ""), name, f"{name}  ·  {meta.get('date', '')[:16].replace('T', ' ')}  ·  {meta.get('sha', '?')}  ·  {', '.join(srv)}"))
    return [(n, lbl) for _, n, lbl in sorted(out, reverse=True)]


def picker_html(runs, selected, baseline):
    e = html.escape
    boxes = "".join(f"<label><input type='checkbox' name='run' value='{e(n)}'{' checked' if n in selected else ''}> {e(lbl)}</label>" for n, lbl in runs)
    opts = "<option value=''>— none —</option>" + "".join(f"<option value='{e(n)}'{' selected' if n == baseline else ''}>{e(n)}</option>" for n, _ in runs)
    return (f"<style>{PICKER_CSS}</style><form class='picker' method='get' action='/'><div class='row'>"
            f"<div class='col'><h3>Runs to render (several = median + ranges)</h3>{boxes or '<i>no runs yet</i>'}</div>"
            f"<div class='col'><h3>Baseline (change vs this run)</h3><select name='baseline'>{opts}</select><div style='margin-top:10px'><button type='submit'>Compare</button></div></div>"
            f"</div></form>")


def serve(port, runs_dir):
    runs_dir = os.path.abspath(runs_dir)

    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            if urllib.parse.urlparse(self.path).path != "/":
                self.send_error(404)
                return
            q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            runs = list_runs(runs_dir)
            names = {n for n, _ in runs}
            sel = list(dict.fromkeys(r for r in q.get("run", []) if r in names))
            base = next((b for b in q.get("baseline", []) if b in names), None)
            picker = picker_html(runs, sel, base)
            status = 200
            try:
                if sel:
                    m = build([os.path.join(runs_dir, r) for r in sel])
                    body = render_html(m, build([os.path.join(runs_dir, base)]) if base else None, picker)
                else:
                    body = f"<!doctype html><html lang='en'><head><meta charset='utf-8'><title>Ferrofin Benchmark</title><style>{CSS}</style></head><body><main><h1>Ferrofin benchmark runs</h1>{picker}</main></body></html>"
            except Exception as ex:  # a run still being written, or a malformed file: say which, keep the picker
                status = 500
                body = (f"<!doctype html><html lang='en'><head><meta charset='utf-8'><title>Ferrofin Benchmark</title><style>{CSS}</style></head><body><main>"
                        f"<h1>Could not render {html.escape(', '.join(sel))}</h1><p class='lede'>{html.escape(type(ex).__name__)}: {html.escape(str(ex))} — "
                        f"a run that is still in progress renders once it finishes.</p>{picker}</main></body></html>")
            data = body.encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def log_message(self, fmt, *args):  # one line per request is enough
            sys.stderr.write(f"{self.address_string()} {fmt % args}\n")

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
    print(f"viewer: http://127.0.0.1:{port}/  (runs from {runs_dir}; Ctrl-C to stop)")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


def main():
    args = sys.argv[1:]
    if args and args[0] == "--serve":
        port = int(args[1]) if len(args) > 1 and args[1].isdigit() else 8097
        runs_dir = args[2] if len(args) > 2 else (args[1] if len(args) > 1 and not args[1].isdigit() else os.path.join(os.path.dirname(os.path.abspath(__file__)), "runs"))
        if not os.path.isdir(runs_dir):
            sys.exit(f"{runs_dir} is not a directory")
        serve(port, runs_dir)
        return
    if args and args[0] == "--readme":
        target = None
        rest = args[1:]
        if rest and rest[0].lower().endswith(".md"):
            target, rest = rest[0], rest[1:]
        if not rest:
            sys.exit(__doc__)
        for r in rest:
            if not os.path.isfile(os.path.join(r, "run.json")):
                sys.exit(f"{r} is not a run dir (no run.json)")
        section = render_readme(build(rest), rest)
        if target:
            inject_readme(target, section)
            sys.stderr.write(f"replaced the generated benchmarks block in {target}\n")
        else:
            sys.stdout.write(section)
        return
    if not args or any(a.startswith("--") for a in args):
        sys.exit(__doc__)
    for r in args:
        if not os.path.isfile(os.path.join(r, "run.json")):
            sys.exit(f"{r} is not a run dir (no run.json)")
    sys.stdout.write(render_md(build(args)))


if __name__ == "__main__":
    main()
