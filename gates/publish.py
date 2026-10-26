#!/usr/bin/env python3
"""The published results (stdlib only): one summary file per release, and the README Results section
and its graphs drawn from those files alone.

    publish.py summarise --version vX.Y.Z --bench RUN RUN RUN [--session DIR] [--out DIR]
        DIR/<version>/summary.json (DIR defaults to docs/results) from the result.json of three or more
        Jellyfin-comparison benchmark runs the operator names and, with --session, the adoption result of
        that gates session: only the numbers the README shows, each with its source. Nothing is picked
        automatically, and every input is a local run.
    publish.py readme [--check] [--results DIR] [--readme README.md]
        draw every graph of the newest DIR/*/summary.json into DIR/<version>/ (a light and a dark SVG
        each) and replace the generated Results block of README.md. The same files give byte-identical
        output; --check writes nothing and exits 1 when the images or the block differ from what the
        files give.

Every graph follows one style (`STYLE`): a fixed colour for Ferrofin and for Jellyfin, a third for a
cell where Ferrofin is slower, values on the bars, a unit on every axis and a caption naming its source.
Every comparable cell is drawn, sorted by speed-up so the wins lead; a withheld cell (the servers did
different work, or the evidence is incomplete) is listed under the graphs with its reason.
"""

import argparse
import html
import json
import math
import os
import re
import statistics
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
if HERE not in sys.path:
    sys.path.append(HERE)
import result  # noqa: E402 — gates/result.py, the envelope every gate's result.json is written in
SCHEMA = 1
MIN_RUNS = 3
LEVELS = (("loaded", "5 screens/s"), ("stress", "25 screens/s"))
#: The peak and idle memory rows the benchmark's result.json holds, and what the README calls them.
MEMORY = {"peak under load": "peak memory under load", "steady idle": "idle memory"}
#: The time-to-first-screen rows, by the benchmark's names, and their short names in a graph.
TTFS = {"cold start (restart → authenticated views; host caches retained)": "cold start",
        "HLS first segment (forced transcode)": "HLS first segment", "direct-play TTFB (1 MiB range)": "direct-play first byte"}
BEGIN = "<!-- BEGIN GENERATED RESULTS"
END = "<!-- END GENERATED RESULTS -->"
#: The two themes every graph is drawn in. Ferrofin keeps the benchmark reports' accent; Jellyfin a
#: neutral grey; a cell where Ferrofin is slower is amber, never hidden.
STYLE = {
    "light": {"ink": "#1A1E24", "muted": "#5B636E", "grid": "#E1E5EA", "ferrofin": "#2E6E8E",
              "jellyfin": "#A3ACB7", "slower": "#B7791F", "on_bar": "#FFFFFF",
              "PASS": "#2E7D4F", "FAIL": "#A83A2E", "SKIP": "#8A939E", "INCOMPLETE": "#B7791F"},
    "dark": {"ink": "#E7EAEE", "muted": "#9AA3AE", "grid": "#2B323C", "ferrofin": "#6FB3D2",
             "jellyfin": "#5B636E", "slower": "#E0B25A", "on_bar": "#13161B",
             "PASS": "#6CC08B", "FAIL": "#E07A6E", "SKIP": "#6B7480", "INCOMPLETE": "#E0B25A"},
}
FONT = "system-ui,-apple-system,'Segoe UI',Helvetica,Arial,sans-serif"
WIDTH = 760


class Refused(Exception):
    """Inputs the summary cannot be built from honestly."""


# ── the summary ─────────────────────────────────────────────────────────────
def sig3(v):
    """Three significant figures, as gates/bench/report.py prints a number."""
    return f"{v:.0f}" if abs(v) >= 100 else f"{v:.1f}" if abs(v) >= 10 else f"{v:.2f}"


def shown(v, whole):
    """The number as the reports print it (whole milliseconds for a screen, else three significant
    figures): a ratio is taken between printed numbers, so a reader can check it."""
    return float(f"{v:.0f}" if whole else sig3(v))


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def bench_results(runs):
    """Each run's benchmark result.json, validated (gates/result.py); Refused names what is missing."""
    docs = []
    for run in runs:
        try:
            doc = result.read(run, "bench")
        except ValueError as error:
            raise Refused(f"{run}: {error}") from error
        if doc is None:
            raise Refused(f"{run} has no result.json (python3 gates/bench/report.py --html {run} writes it)")
        if "levels" not in doc["detail"]:
            raise Refused(f"{run} is a gate session, not a benchmark run")
        if "jellyfin" not in (doc["detail"].get("servers") or {}):
            raise Refused(f"{run} did not measure Jellyfin: the README compares against it")
        docs.append(doc)
    return docs


def combine(name, per_run, whole, unit=None):
    """One published row from the same row of every run: each server's median of the run medians and
    their range, the ratio of the printed medians (Jellyfin over Ferrofin), and why it is withheld
    when any run withheld it or lacks it."""
    missing = [i + 1 for i, r in enumerate(per_run) if r is None]
    reasons = sorted({r["withheld"] for r in per_run if r and r.get("withheld")})
    if missing:
        reasons.append(f"missing from run {', '.join(map(str, missing))}")
    row = {"name": name, "unit": unit, "withheld": None, "ratio": None}
    for server in ("ferrofin", "jellyfin"):
        values = [r["servers"][server]["median"] for r in per_run
                  if r and server in r["servers"] and isinstance(r["servers"][server].get("median"), (int, float))]
        if len(values) < len(per_run) - len(missing):
            reasons.append(f"{server} has no value in every run")
        row[server] = ({"median": round(statistics.median(values), 4), "min": round(min(values), 4),
                        "max": round(max(values), 4)} if values else None)
        present = next((r for r in per_run if r and server in r["servers"]), None)
        if row["unit"] is None and present:
            row["unit"] = present["servers"][server].get("unit")
    row["withheld"] = "; ".join(reasons) or None
    if not row["withheld"]:
        if row["ferrofin"] is None or row["jellyfin"] is None:
            row["withheld"] = "a server has no value"
        else:
            mine, theirs = shown(row["ferrofin"]["median"], whole), shown(row["jellyfin"]["median"], whole)
            if mine and theirs:
                row["ratio"] = round(theirs / mine, 4)
            else:
                row["withheld"] = "a median rounds to zero at the printed precision"
    return row


def combine_rows(docs, pick, whole):
    """Every row `pick(detail)` gives in any run (first seen first), combined over all of them by name; a
    run whose detail lacks the group (`pick` raises KeyError) lacks every row of it."""
    def rows(d):
        try:
            return pick(d["detail"])
        except KeyError:
            return []
    by_run = [{r["name"]: r for r in rows(d)} for d in docs]
    names = list(dict.fromkeys(n for run in by_run for n in run))
    return [combine(n, [run.get(n) for run in by_run], whole) for n in names]


def bench_summary(docs, names, min_runs=MIN_RUNS):
    """The README's benchmark numbers from `docs` (one result per run) with their source; Refused when
    there are fewer than `min_runs` or the runs measured different builds, fixtures or hosts."""
    if len(docs) < min_runs:
        raise Refused(f"{len(docs)} benchmark run{'s' if len(docs) != 1 else ''}: publishable numbers are the "
                      f"median of at least {min_runs} (one run is noise)")
    starts = [d["started"] for d in docs]
    if not all(starts) or len(set(starts)) < len(starts):
        raise Refused("every run needs its own start time: the same run named twice is still one run")
    for key in ("commit", "image_digest", "fixture", "host_class", "oracle"):
        if any(not d["source"][key] for d in docs):
            raise Refused(f"a run records no {key}: a published number names its source")
        values = {json.dumps(d["source"][key], sort_keys=True) for d in docs}
        if len(values) > 1:
            raise Refused(f"the runs differ in {key}: one summary is one build on one host and fixture")
    first = docs[0]
    levels = {}
    for level, _ in LEVELS:
        having = [d for d in docs if level in d["detail"]["levels"]]
        if not having:
            continue
        if len({json.dumps(d["detail"]["levels"][level]["rate"]) for d in having}) > 1:
            raise Refused(f"the runs measured the {level} level at different rates")
        levels[level] = {"rate": having[0]["detail"]["levels"][level]["rate"],
                         "screens": combine_rows(docs, lambda d, lv=level: d["levels"][lv]["screens"], True),
                         "endpoints": combine_rows(docs, lambda d, lv=level: d["levels"][lv]["endpoints"], False)}
    source = {"runs": names, "run_count": len(docs), "commit": first["source"]["commit"],
              "image_digest": first["source"]["image_digest"], "oracle": first["detail"]["oracle_label"],
              "oracle_image": first["source"]["oracle"], "host_class": first["source"]["host_class"],
              "fixture": first["source"]["fixture"],
              "date": max((d["started"] or "")[:10] for d in docs) or None}
    return {"source": source, "levels": levels,
            "ttfs": combine_rows(docs, lambda d: d["ttfs"], False),
            "memory": [r for r in combine_rows(docs, lambda d: d["memory"], False) if r["name"] in MEMORY],
            "cpu_per_screen": combine_rows(docs, lambda d: d["cpu_per_screen"], False)}


#: How long a failed adoption row's reason may be in the summary: the full one is in the session's report.
REASON_CHARS = 200
ADOPTION_VERDICTS = ("PASS", "FAIL", "SKIP", "INCOMPLETE")
#: How an adoption verdict reads in the README and its grid.
VERDICT_WORDS = {"PASS": "passed", "FAIL": "failed", "SKIP": "skipped", "INCOMPLETE": "unfinished"}


def adoption_summary(session, bench_commit=None, publishing=True):
    """The adoption status of a gates session (D4: the release's local session, not CI): every row's
    fixture, kind, expected generation and verdict (a skipped row stays skipped), the first part of a
    failure's reason, and the session's commit, image and date. When `publishing`, refused for a session
    of uncommitted code, or one that measured another commit than the benchmark runs."""
    record = load(os.path.join(session, "session.json")) if os.path.isfile(os.path.join(session, "session.json")) else None
    if not isinstance(record, dict):
        raise Refused(f"{session} is not a gates session (no session.json)")
    if publishing and record.get("dirty"):
        raise Refused(f"{session} ran with uncommitted changes: the commit does not identify the code")
    commit = record.get("commit") or ""
    if bench_commit and not commit.startswith(str(bench_commit)):
        raise Refused(f"{session} measured {commit[:12]}, the benchmark runs {bench_commit}")
    try:
        doc = result.read(os.path.join(session, "adoption"), "adoption")
    except ValueError as error:
        raise Refused(f"{session}/adoption: {error}") from error
    if doc is None:
        raise Refused(f"{session}/adoption has no result.json (python3 gates/adoption/adoption_report.py build "
                      f"{session}/adoption writes it)")
    if publishing and doc["verdict"] == "nothing ran":
        raise Refused(f"{session}/adoption ran nothing (every row skipped, or none recorded): there is no status to publish")
    rows = [{"fixture": r["fixture"], "kind": r["kind"], "expected": r["expected"], "verdict": r["verdict"],
             "reason": "" if r["verdict"] == "PASS" else str(r.get("detail") or "")[:REASON_CHARS]}
            for r in doc["detail"]["rows"]]
    source = {"session": os.path.basename(os.path.normpath(session)), "commit": commit,
              "image_digest": (record.get("image") or {}).get("id"), "oracle": doc["source"]["oracle"],
              "date": (record.get("started") or "")[:10] or None}
    return {"source": source, "verdict": doc["verdict"], "rows": rows}


def summarise(version, bench_runs, session=None):
    """The summary file of release `version`."""
    if not re.fullmatch(r"v\d+\.\d+\.\d+", version):
        raise Refused(f"{version!r} is not a release version (vX.Y.Z)")
    if len({os.path.realpath(r) for r in bench_runs}) < len(bench_runs):
        raise Refused("a run is named twice: the same run twice is still one run")
    docs = bench_results(bench_runs)
    names = [os.path.basename(os.path.normpath(r)) if os.path.basename(os.path.normpath(r)) != "bench"
             else os.path.basename(os.path.dirname(os.path.normpath(r))) + "/bench" for r in bench_runs]
    out = {"schema": SCHEMA, "version": version, "bench": bench_summary(docs, names)}
    if session:
        out["adoption"] = adoption_summary(session, out["bench"]["source"]["commit"])
    return out


def summary_problems(doc):
    """What is wrong with `doc` as a summary file; empty when it is valid."""
    if not isinstance(doc, dict):
        return ["not a JSON object"]
    out = []
    if doc.get("schema") != SCHEMA:
        out.append(f"schema is {doc.get('schema')!r}, not {SCHEMA}")
    if not re.fullmatch(r"v\d+\.\d+\.\d+", str(doc.get("version"))):
        out.append("version is not vX.Y.Z")
    bench = doc.get("bench")
    if not isinstance(bench, dict):
        return out + ["bench is not an object"]
    source = bench.get("source")
    if not isinstance(source, dict):
        return out + ["bench.source is not an object"]
    for key in ("runs", "run_count", "commit", "image_digest", "oracle", "host_class", "fixture", "date"):
        if not source.get(key):
            out.append(f"bench.source has no {key}")
    if source.get("host_class") and not isinstance(source["host_class"], dict):
        out.append("bench.source.host_class is not an object")
    if isinstance(source.get("run_count"), int) and source["run_count"] < MIN_RUNS:
        out.append(f"bench.source.run_count is under {MIN_RUNS}")
    groups = []
    levels = bench.get("levels")
    if not isinstance(levels, dict) or not all(isinstance(lv, dict) for lv in levels.values()):
        out.append("bench.levels is not an object of levels")
    else:
        groups += [(f"levels.{name}.{g}", lv.get(g)) for name, lv in levels.items() for g in ("screens", "endpoints")]
    groups += [(g, bench.get(g)) for g in ("ttfs", "memory", "cpu_per_screen")]
    number = lambda v: isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)
    for where, rows in groups:
        if not isinstance(rows, list):
            out.append(f"bench.{where} is not a list")
            continue
        for r in rows:
            if not isinstance(r, dict) or not {"name", "unit", "ferrofin", "jellyfin", "ratio", "withheld"} <= r.keys():
                out.append(f"a row of bench.{where} is not shaped as expected: {str(r)[:80]}")
            elif r["ratio"] is not None and r["withheld"]:
                out.append(f"bench row {r['name']} has a ratio and is withheld")
            elif r["ratio"] is not None and not (number(r["ratio"]) and r["ratio"] > 0 and all(
                    isinstance(r[s], dict) and number(r[s].get("median")) and r[s]["median"] > 0 for s in ("ferrofin", "jellyfin"))):
                out.append(f"bench row {r['name']} has a ratio but not two positive medians and a positive ratio")
            elif r["ratio"] is None and not r["withheld"]:
                out.append(f"bench row {r['name']} has neither a ratio nor a reason it is withheld")
    if "adoption" in doc:
        adoption = doc["adoption"]
        if not isinstance(adoption, dict) or not isinstance(adoption.get("source"), dict) or not isinstance(adoption.get("rows"), list):
            out.append("adoption is not an object with a source and rows")
        else:
            out += [f"adoption.source has no {k}" for k in ("session", "commit", "date") if not adoption["source"].get(k)]
            for r in adoption["rows"]:
                if not isinstance(r, dict) or r.get("verdict") not in ADOPTION_VERDICTS or not r.get("fixture"):
                    out.append(f"an adoption row is not shaped as expected: {str(r)[:80]}")
    return out


def read_text(path):
    """The file's text, None when there is no such file."""
    try:
        with open(path, encoding="utf-8") as f:
            return f.read()
    except FileNotFoundError:
        return None


def write_json(path, doc):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path + ".tmp", "w", encoding="utf-8") as f:
        json.dump(doc, f, indent=1, sort_keys=True, ensure_ascii=False)
        f.write("\n")
    os.replace(path + ".tmp", path)


# ── graphs ──────────────────────────────────────────────────────────────────
def speedup_text(ratio, unit):
    """`3.2× faster`, `1.3× slower`, `about the same`; memory says lighter and heavier."""
    better, worse = ("lighter", "heavier") if unit == "MiB" else ("faster", "slower")
    if f"{ratio:.1f}" == "1.0":
        return "about the same"
    return f"{ratio:.1f}× {better}" if ratio > 1 else f"{1 / ratio:.1f}× {worse}"


def value_text(v, unit, whole=False):
    number = f"{v:.0f}" if whole else sig3(v)
    return f"{number} {unit}" if unit else number


def nice_step(top, ticks=5):
    """A tick step of 1, 2, 2.5 or 5 × 10ⁿ giving at most `ticks` intervals up to `top`."""
    raw = top / ticks
    power = 10 ** math.floor(math.log10(raw)) if raw > 0 else 1
    return next(m * power for m in (1, 2, 2.5, 5, 10) if m * power >= raw)


def text(x, y, body, size=12, fill="ink", anchor="start", weight=None, theme=None):
    w = f" font-weight='{weight}'" if weight else ""
    return (f"<text x='{x:.1f}' y='{y:.1f}' font-size='{size}' fill='{theme[fill]}' text-anchor='{anchor}'{w}>"
            f"{html.escape(body)}</text>")


def label_width(labels, size=12):
    """An estimate of the widest label's width: SVG has no text metrics without a browser."""
    return max((len(s) for s in labels), default=0) * size * 0.58


def caption_lines(caption, y, theme):
    """The caption under a graph, split at its first sentence so it fits the width."""
    first, _, rest = caption.partition(". ")
    return text(0, y, first + ".", 11, "muted", theme=theme) + (text(0, y + 15, rest, 11, "muted", theme=theme) if rest else "")


def frame(title, height, body, theme_name, legend, alt=""):
    """The SVG around a graph: its accessible title, its title, the legend, and the shared type."""
    t = STYLE[theme_name]
    parts = [f"<title>{html.escape(alt or title)}</title>", text(0, 18, title, 15, weight="600", theme=t)]
    x = 0.0
    for key, name in legend:
        parts.append(f"<rect x='{x:.1f}' y='30' width='11' height='11' rx='2' fill='{t[key]}'/>")
        parts.append(text(x + 16, 40, name, 12, "muted", theme=t))
        x += 16 + label_width([name]) + 22
    return (f"<svg xmlns='http://www.w3.org/2000/svg' width='{WIDTH}' height='{height:.0f}' "
            f"viewBox='0 0 {WIDTH} {height:.0f}' font-family=\"{FONT}\" role='img'>"
            + "".join(parts) + body + "</svg>\n")


def paired_graph(title, rows, unit, axis, caption, theme_name, oracle, whole=False, alt=""):
    """Ferrofin's and Jellyfin's bars side by side per row, on one linear axis in `unit`, with each
    value on its bar and the speed-up at the right. Ferrofin's bar is amber where it is slower."""
    t = STYLE[theme_name]
    left = label_width([r["name"] for r in rows]) + 14
    values = [value_text(r[s]["median"], unit, whole) for r in rows for s in ("ferrofin", "jellyfin")]
    # the widest value beside its bar, then the widest multiplier: neither may reach the other
    right = label_width(values, 11) + 10 + label_width([speedup_text(r["ratio"], unit) for r in rows], 12) + 8
    plot = WIDTH - left - right
    top = max(max(r["ferrofin"]["median"], r["jellyfin"]["median"]) for r in rows)
    step = nice_step(top)
    scale_top = step * math.ceil(top / step)
    x = lambda v: left + plot * v / scale_top
    row_h, bar_h, y0 = 36, 12, 62
    body = []
    end = y0 + row_h * len(rows)
    for i in range(int(round(scale_top / step)) + 1):
        tx = x(i * step)
        body.append(f"<line x1='{tx:.1f}' y1='{y0 - 6}' x2='{tx:.1f}' y2='{end:.1f}' stroke='{t['grid']}' stroke-width='1'/>")
        body.append(text(tx, end + 15, f"{i * step:g}", 11, "muted", "middle", theme=t))
    body.append(text(left + plot / 2, end + 32, axis, 12, "muted", "middle", theme=t))
    for n, r in enumerate(rows):
        y = y0 + n * row_h
        slower = r["ratio"] < 1 and f"{r['ratio']:.1f}" != "1.0"
        body.append(text(left - 10, y + bar_h + 4, r["name"], 12, anchor="end", theme=t))
        for k, (server, colour) in enumerate((("ferrofin", "slower" if slower else "ferrofin"), ("jellyfin", "jellyfin"))):
            v = r[server]["median"]
            by = y + k * (bar_h + 3)
            body.append(f"<rect x='{left:.1f}' y='{by:.1f}' width='{max(x(v) - left, 1):.1f}' height='{bar_h}' rx='2' fill='{t[colour]}'/>")
            body.append(text(x(v) + 5, by + bar_h - 2, value_text(v, unit, whole), 11, "ink" if k == 0 else "muted", theme=t))
        body.append(text(WIDTH - 4, y + bar_h + 4, speedup_text(r["ratio"], unit), 12,
                         "slower" if slower else "ferrofin", "end", "600", theme=t))
    body.append(caption_lines(caption, end + 54, t))
    return frame(title, end + 76, "".join(body), theme_name, (("ferrofin", "Ferrofin"), ("jellyfin", oracle)), alt)


def speedup_graph(title, rows, caption, theme_name, oracle, measure="p50", alt=""):
    """One bar per row from 1× on a log axis: how many times faster Ferrofin is than the oracle, wins to
    the right in Ferrofin's colour, slower cells to the left in amber; each bar carries its multiple and
    the two medians it comes from."""
    t = STYLE[theme_name]
    labels = [r["name"] for r in rows]
    notes = [f"{speedup_text(r['ratio'], r['unit'])}  ·  {value_text(r['ferrofin']['median'], r['unit'])} vs "
             f"{value_text(r['jellyfin']['median'], r['unit'])}" for r in rows]
    left = label_width(labels) + 14
    right = label_width(notes, 11) + 12   # the longest bar's note fits after it
    plot = WIDTH - left - right
    lo = min(min(r["ratio"] for r in rows), 1.0)
    hi = max(max(r["ratio"] for r in rows), 1.0)
    lo_exp, hi_exp = math.floor(math.log2(lo)), max(math.ceil(math.log2(hi)), 1)
    lo_exp = min(lo_exp, -1) if lo < 1 else 0
    x = lambda v: left + plot * (math.log2(v) - lo_exp) / (hi_exp - lo_exp)
    row_h, bar_h, y0 = 22, 14, 60
    end = y0 + row_h * len(rows)
    body = []
    for e in range(lo_exp, hi_exp + 1):
        tx = x(2 ** e)
        body.append(f"<line x1='{tx:.1f}' y1='{y0 - 6}' x2='{tx:.1f}' y2='{end:.1f}' stroke='{t['grid' if e else 'muted']}' stroke-width='1'/>")
        body.append(text(tx, end + 15, f"{2 ** e:g}×", 11, "muted", "middle", theme=t))
    body.append(text(left + plot / 2, end + 32, f"times faster than {oracle} ({measure}, log scale; right of 1× is faster)",
                     12, "muted", "middle", theme=t))
    for n, r in enumerate(rows):
        y = y0 + n * row_h
        slower = r["ratio"] < 1 and f"{r['ratio']:.1f}" != "1.0"
        a, b = sorted((x(1), x(r["ratio"])))
        colour = "slower" if slower else "ferrofin"
        body.append(text(left - 10, y + bar_h - 3, r["name"], 12, anchor="end", theme=t))
        body.append(f"<rect x='{a:.1f}' y='{y:.1f}' width='{max(b - a, 1):.1f}' height='{bar_h}' rx='2' fill='{t[colour]}'/>")
        body.append(text(max(b, x(1)) + 5, y + bar_h - 3, notes[n], 11, colour, theme=t))
    body.append(caption_lines(caption, end + 54, t))
    return frame(title, end + 76, "".join(body), theme_name, (("ferrofin", "Ferrofin faster"), ("slower", "Ferrofin slower")), alt)


def status_graph(title, rows, caption, theme_name, alt=""):
    """One line per adoption row: the fixture, what kind of copy it is and the generation it is checked
    at, and its verdict as a coloured chip (a skipped row is grey, never green)."""
    t = STYLE[theme_name]
    names = [r["fixture"].removeprefix("jellyfin-") for r in rows]
    left = label_width(names) + 14
    row_h, y0 = 24, 62
    end = y0 + row_h * len(rows)
    body = [text(0, y0 - 10, "Jellyfin database", 11, "muted", theme=t), text(left + 10, y0 - 10, "kind", 11, "muted", theme=t),
            text(left + 90, y0 - 10, "checked at schema of", 11, "muted", theme=t), text(WIDTH - 4, y0 - 10, "result", 11, "muted", "end", theme=t)]
    for n, (name, r) in enumerate(zip(names, rows)):
        y = y0 + n * row_h
        body.append(f"<line x1='0' y1='{y - 4:.1f}' x2='{WIDTH}' y2='{y - 4:.1f}' stroke='{t['grid']}' stroke-width='1'/>")
        body.append(text(left - 10, y + 12, name, 12, anchor="end", theme=t))
        body.append(text(left + 10, y + 12, r["kind"], 12, "muted", theme=t))
        body.append(text(left + 90, y + 12, f"Jellyfin {r['expected']}", 12, "muted", theme=t))
        word = VERDICT_WORDS[r["verdict"]]
        body.append(f"<rect x='{WIDTH - 84}' y='{y:.1f}' width='80' height='16' rx='8' fill='{t[r['verdict']]}'/>")
        body.append(text(WIDTH - 44, y + 12, word, 11, "on_bar", "middle", "600", theme=t))
    body.append(caption_lines(caption, end + 22, t))
    return frame(title, end + 44, "".join(body), theme_name, (("PASS", "passed"), ("FAIL", "failed"), ("SKIP", "skipped")), alt)


def adoption_section(adoption, version, rel, results_dir):
    """`(files, README lines)` of the adoption status."""
    src, rows = adoption["source"], adoption["rows"]
    passed = sum(r["verdict"] == "PASS" for r in rows)
    skipped = sum(r["verdict"] == "SKIP" for r in rows)
    title = "Adopting an existing Jellyfin database: every supported version and route"
    alt = (f"{title}: {passed} of {len(rows)} passed"
           + "".join(f"; {r['fixture']} {VERDICT_WORDS[r['verdict']]}" for r in rows if r["verdict"] != "PASS") + ".")
    cap = (f"{ferrofin(version)} (commit {src['commit'][:8]}), gates session {src['session']}, {src['date']}. "
           "What each row checks: gates/adoption/README.md")
    files = {os.path.join(results_dir, version, f"adoption{'' if theme == 'light' else '-dark'}.svg"):
             status_graph(title, rows, cap, theme, alt) for theme in STYLE}
    lines = ["### Adopting an existing Jellyfin database\n",
             f"Each supported Jellyfin version and upgrade route, adopted in place by Ferrofin **{version}** "
             f"(commit `{src['commit'][:12]}`) in a local gates session on {src['date']}: "
             f"**{passed} of {len(rows)} passed**" + (f" ({skipped} skipped: their fixtures were not built)" if skipped else "")
             + ". Adoption is one-way; see "
             "[Migrating from Jellyfin](#migrating-from-jellyfin).\n",
             f'<picture>\n  <source media="(prefers-color-scheme: dark)" srcset="{rel}/adoption-dark.svg">\n'
             f'  <img alt="{html.escape(alt)}" src="{rel}/adoption.svg">\n</picture>\n',
             "<details><summary>Adoption: every row</summary>\n",
             "| Jellyfin database | Kind | Checked at | Result | Why |", "|---|---|---|---|---|"]
    lines += [f"| {md(r['fixture'])} | {md(r['kind'])} | {md(r['expected'])} | {VERDICT_WORDS[r['verdict']]} | {md(r['reason'])} |"
              for r in rows]
    lines += ["\n</details>\n"]
    return files, lines


def ranked(rows):
    """The comparable rows, the biggest speed-up first (ties by name, so the order is stable)."""
    return sorted((r for r in rows if r["ratio"] is not None), key=lambda r: (-r["ratio"], r["name"]))


def graphs(bench):
    """`[(file stem, title, kind, rows, unit, axis label)]`: every graph of a release's benchmark
    summary that has a comparable row."""
    out = []
    for level, rate in LEVELS:
        lv = bench["levels"].get(level)
        if not lv:
            continue
        out.append((f"screens-{level}", f"Screens at {rate} ({level}): p50 latency", "paired", ranked(lv["screens"]),
                    "ms", "p50 latency of a screen's requests (ms)"))
        out.append((f"endpoints-{level}", f"Endpoints at {rate} ({level}): speed-up", "speedup", ranked(lv["endpoints"]),
                    "ms", "p50"))
    ttfs = [{**r, "name": TTFS.get(r["name"], r["name"])} for r in bench["ttfs"]]
    out.append(("startup", "Time to first screen: speed-up", "speedup", ranked(ttfs), "ms", "median"))
    memory = [{**r, "name": MEMORY.get(r["name"], r["name"])} for r in bench["memory"]]
    out.append(("memory", "Memory", "paired", ranked(memory), "MiB", "resident memory (MiB)"))
    cpu = [{**r, "name": f"{r['name']} level"} for r in bench["cpu_per_screen"]]
    out.append(("cpu", "CPU per completed screen", "paired", ranked(cpu), "ms", "CPU milliseconds per screen"))
    return [g for g in out if g[3]]


def ferrofin(version):
    """`Ferrofin vX.Y.Z`, or `Ferrofin` alone for a build no release names (a gates session's)."""
    return f"Ferrofin {version}" if version else "Ferrofin"


def caption(src, version):
    return (f"{ferrofin(version)} (commit {str(src['commit'])[:8]}) against {src['oracle']}, the median of {src['run_count']} "
            f"run{'s' if src['run_count'] != 1 else ''} on one host ({host_text(src['host_class'])}), {src['date']}. "
            "Every number and its range: docs/BENCHMARKS.md")


def host_text(host):
    host = host or {}
    return f"{host.get('cores')} cores, {host.get('memory_gib')} GiB"


def render_svg(graph, bench, version, theme_name):
    stem, title, kind, rows, unit, axis = graph
    src = bench["source"]
    cap = caption(src, version)
    alt = alt_text(graph, src["oracle"])
    if kind == "paired":
        return paired_graph(title, rows, unit, axis, cap, theme_name, src["oracle"], stem.startswith("screens-"), alt)
    return speedup_graph(title, rows, cap, theme_name, src["oracle"], axis, alt)


def alt_text(graph, oracle):
    stem, title, kind, rows, unit, _ = graph
    faster = [r for r in rows if r["ratio"] >= 1 or f"{r['ratio']:.1f}" == "1.0"]
    lo, hi = min(r["ratio"] for r in rows), max(r["ratio"] for r in rows)
    return (f"{title}: Ferrofin against {oracle} on {len(rows)} comparable cells, as fast or faster on "
            f"{len(faster)}; from {speedup_text(lo, unit)} to {speedup_text(hi, unit)}.")


def headlines(bench):
    """The multipliers above the graphs, from the comparable rows only."""
    out = []
    for r in bench["ttfs"]:
        if r["ratio"] is not None and r["name"].startswith("cold start"):
            out.append(f"{speedup_text(r['ratio'], 'ms')} cold start")
    lv = bench["levels"].get("loaded")
    screens = [r for r in (lv or {}).get("screens", []) if r["ratio"] is not None]
    if screens:
        lo, hi = min(r["ratio"] for r in screens), max(r["ratio"] for r in screens)
        faster = sum(r["ratio"] > 1 and f"{r['ratio']:.1f}" != "1.0" for r in screens)
        out.append(f"{lo:.1f}–{hi:.1f}× the speed on {len(screens)} screens at 5 screens/s"
                   if faster == len(screens) else f"faster on {faster} of {len(screens)} screens at 5 screens/s")
    for r in bench["memory"]:
        if r["ratio"] is not None:
            out.append(f"{speedup_text(r['ratio'], 'MiB')} {MEMORY.get(r['name'], r['name'])}")
    return out


def md(text_):
    """Text safe in a Markdown table cell or list item: no pipe ends a cell, no angle bracket opens a tag."""
    return html.escape(str(text_), quote=False).replace("|", "\\|").replace("\n", " ")


def numbers_table(rows, oracle, whole=False):
    lines = [f"| | Ferrofin | {md(oracle)} | |", "|---|---|---|---|"]
    for r in rows:
        lines.append(f"| {md(r['name'])} | {value_text(r['ferrofin']['median'], r['unit'], whole)} | "
                     f"{value_text(r['jellyfin']['median'], r['unit'], whole)} | {speedup_text(r['ratio'], r['unit'])} |")
    return "\n".join(lines)


def withheld_rows(bench):
    """`[(where, name, reason)]`: every cell with no ratio, by level and group."""
    rows = [(f"{level} · {g[:-1]}", r["name"], r) for level, _ in LEVELS for g in ("screens", "endpoints")
            for r in (bench["levels"].get(level) or {}).get(g, [])]
    rows += [("time to first screen", TTFS.get(r["name"], r["name"]), r) for r in bench["ttfs"]]
    rows += [("memory", MEMORY.get(r["name"], r["name"]), r) for r in bench["memory"]]
    rows += [("CPU per screen", f"{r['name']} level", r) for r in bench["cpu_per_screen"]]
    return [(where, name, r["withheld"]) for where, name, r in rows if r["ratio"] is None]


def render(results_dir, readme_text, readme_dir=ROOT):
    """`(files, README text)`: every SVG of the newest summary, by path, and the README with its Results
    block replaced, its image links relative to `readme_dir`. Pure: reads the summary files, writes
    nothing."""
    summaries = []
    if not os.path.isdir(results_dir):
        raise Refused(f"no {results_dir}: run summarise first")
    for name in sorted(os.listdir(results_dir)):
        path = os.path.join(results_dir, name, "summary.json")
        if os.path.isfile(path):
            doc = load(path)
            if problems := summary_problems(doc):
                raise Refused(f"{path}: {'; '.join(problems)}")
            if doc["version"] != name:
                raise Refused(f"{path} holds {doc['version']}'s results, not {name}'s")
            summaries.append(doc)
    if not summaries:
        raise Refused(f"no {results_dir}/*/summary.json: run summarise first")
    latest = max(summaries, key=lambda d: tuple(int(p) for p in d["version"][1:].split(".")))
    version, bench = latest["version"], latest["bench"]
    rel = os.path.relpath(os.path.join(results_dir, version), readme_dir).replace(os.sep, "/")
    files, block = {}, []
    src = bench["source"]
    block.append(f"{BEGIN} — do not edit by hand. Regenerate with: make publish-results (gates/publish.py) -->")
    block.append("### Speed and memory against Jellyfin\n")
    block.append(f"Ferrofin **{version}** against **{src['oracle']}**: the median of {src['run_count']} runs on one host "
                 f"({host_text(src['host_class'])}), each server alone in its container, over the same library, "
                 f"{src['date']}. Every comparable cell is drawn, the biggest speed-up first.\n")
    if heads := headlines(bench):
        block.append(" · ".join(f"**{h}**" for h in heads) + "\n")
    for graph in graphs(bench):
        stem = graph[0]
        for theme in STYLE:
            name = f"{stem}{'' if theme == 'light' else '-dark'}.svg"
            files[os.path.join(results_dir, version, name)] = render_svg(graph, bench, version, theme)
        block.append(f'<picture>\n  <source media="(prefers-color-scheme: dark)" srcset="{rel}/{stem}-dark.svg">\n'
                     f'  <img alt="{html.escape(alt_text(graph, src["oracle"]))}" src="{rel}/{stem}.svg">\n</picture>\n')
        block.append(f"<details><summary>{html.escape(graph[1])}: the numbers</summary>\n\n"
                     f"{numbers_table(graph[3], src['oracle'], stem.startswith('screens-'))}\n\n</details>\n")
    if withheld := withheld_rows(bench):
        block.append("Withheld, because the servers did different work or the evidence is incomplete:\n")
        block += [f"- {where} · {md(name)}: {md(reason)}" for where, name, reason in withheld]
        block.append("")
    block.append(f"Source: {md(', '.join(src['runs']))}; commit `{src['commit']}`, image `{str(src['image_digest'])[:19]}`, "
                 f"fixture `{str(src['fixture'])[:12]}`. The full tables are in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md); "
                 "what each number means is in [`gates/bench/README.md`](gates/bench/README.md).\n")
    if "adoption" in latest:
        more, lines = adoption_section(latest["adoption"], version, rel, results_dir)
        files.update(more)
        block += lines
    block.append(END)
    section = "\n".join(block) + "\n"
    i, j = readme_text.find(BEGIN), readme_text.find(END)
    if i < 0 or j < i:
        raise Refused(f"README has no {BEGIN} … {END} block to replace")
    j += len(END)
    if readme_text[j:j + 1] == "\n":
        j += 1
    return files, readme_text[:i] + section + readme_text[j:]


def session_graphs(bench_result, version=""):
    """The same graphs, inline, for one benchmark result (the gates session page): drawn from a summary
    of that one run, light and dark, the theme the reader's browser prefers shown."""
    bench = bench_summary([bench_result], [bench_result["detail"]["runs"][0]], min_runs=1)
    out = []
    for graph in graphs(bench):
        for theme in STYLE:
            out.append(f"<div class='graph {theme}'>{render_svg(graph, bench, version, theme)}</div>")
    return "".join(out)


def session_adoption_graph(session, version=""):
    """The README's adoption grid, inline, for a gates session's adoption result (the session page)."""
    adoption = adoption_summary(session, publishing=False)
    files, _ = adoption_section(adoption, version, ".", session)
    return "".join(f"<div class='graph {theme}'>{svg}</div>"
                   for theme, svg in zip(STYLE, (files[p] for p in sorted(files, key=lambda p: p.endswith("-dark.svg")))))


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    s = sub.add_parser("summarise")
    s.add_argument("--version", required=True)
    s.add_argument("--bench", nargs="+", required=True, metavar="RUN")
    s.add_argument("--session", metavar="DIR", help="the release's gates session: its adoption status")
    s.add_argument("--out", default=os.path.join(ROOT, "docs", "results"))
    r = sub.add_parser("readme")
    r.add_argument("--check", action="store_true")
    r.add_argument("--results", default=os.path.join(ROOT, "docs", "results"))
    r.add_argument("--readme", default=os.path.join(ROOT, "README.md"))
    args = parser.parse_args(argv)
    try:
        if args.command == "summarise":
            doc = summarise(args.version, args.bench, args.session)
            path = os.path.join(args.out, args.version, "summary.json")
            write_json(path, doc)
            print(path)
            return 0
        current = read_text(args.readme)
        if current is None:
            raise Refused(f"no {args.readme}")
        files, readme = render(args.results, current, os.path.dirname(os.path.abspath(args.readme)))
    except Refused as error:
        sys.exit(f"publish: {error}")
    stale = [p for p, body in files.items() if read_text(p) != body]
    drawn = {os.path.dirname(p) for p in files}   # a graph the summary no longer gives is stale too
    stale += sorted(os.path.join(d, n) for d in drawn for n in os.listdir(d)
                    if n.endswith(".svg") and os.path.join(d, n) not in files)
    if readme != current:
        stale.append(args.readme)
    if args.check:
        if stale:
            sys.exit("publish: out of date, run make publish-results: " + ", ".join(os.path.relpath(p, ROOT) for p in stale))
        print("publish: the README Results block and its graphs match the summary files")
        return 0
    for path in stale:
        if path not in files and path != args.readme:
            os.remove(path)
    for path, body in files.items():
        with open(path + ".tmp", "w", encoding="utf-8") as f:
            f.write(body)
        os.replace(path + ".tmp", path)
    with open(args.readme + ".tmp", "w", encoding="utf-8") as f:
        f.write(readme)
    os.replace(args.readme + ".tmp", args.readme)
    print(f"publish: wrote {len(files)} graphs and the README Results block")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
