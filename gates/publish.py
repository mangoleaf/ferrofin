#!/usr/bin/env python3
"""The published results (stdlib only): one summary file per release, and the README Results section
and its graphs drawn from those files alone.

    publish.py summarise --version vX.Y.Z --bench RUN RUN RUN [--session DIR] [--ab DIR] [--out DIR]
        DIR/<version>/summary.json (DIR defaults to docs/results) from the result.json of three or more
        Jellyfin-comparison benchmark runs the operator names and, with --session, the adoption and parity
        results of that gates session (both required), and with --ab, the release A/B gate's verdict cells
        against the previous release: only the numbers the README shows, each with its source. Nothing is picked
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
              "PASS": "#2E7D4F", "FAIL": "#A83A2E", "SKIP": "#8A939E", "INCOMPLETE": "#B7791F",
              "identical": "#2E7D4F", "accepted": "#2E6E8E", "filed": "#B7791F", "new": "#A83A2E", "oracle_failed": "#C3C9D1"},
    "dark": {"ink": "#E7EAEE", "muted": "#9AA3AE", "grid": "#2B323C", "ferrofin": "#6FB3D2",
             "jellyfin": "#5B636E", "slower": "#E0B25A", "on_bar": "#13161B",
             "PASS": "#6CC08B", "FAIL": "#E07A6E", "SKIP": "#6B7480", "INCOMPLETE": "#E0B25A",
             "identical": "#6CC08B", "accepted": "#6FB3D2", "filed": "#E0B25A", "new": "#E07A6E", "oracle_failed": "#4A515B"},
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


#: A parity request's outcome, as gates/parity/diff.py counts it, and how the README names it.
PARITY_OUTCOMES = {"identical": "identical", "accepted": "accepted differences", "filed": "filed as issues",
                   "new": "new differences", "oracle_failed": "not compared (Jellyfin failed them)"}
#: How many API areas the parity graph draws, busiest first; the rest are one "other areas" bar.
PARITY_AREAS = 12


def same_commit(a, b):
    """Whether two commit ids (either may be abbreviated) name the same commit."""
    a, b = str(a or ""), str(b or "")
    return bool(a and b) and (a.startswith(b) or b.startswith(a))


def parity_summary(session, commit, publishing=True):
    """The parity overview of a gates session's parity run: each request's outcome counted overall and by
    API area, the Jellyfin version compared against, and the session's commit and date. When publishing,
    refused for a run that did not finish (it failed, or sent fewer requests than it planned), one of
    another commit, or one with no Jellyfin version; otherwise None for a run that compared nothing."""
    try:
        doc = result.read(os.path.join(session, "parity"), "parity")
    except ValueError as error:
        raise Refused(f"{session}/parity: {error}") from error
    if doc is None:
        raise Refused(f"{session}/parity has no result.json (python3 gates/parity/diff.py result --out {session}/parity writes it)")
    detail = doc["detail"]
    if not publishing and not detail.get("sent"):
        return None
    if publishing:
        if doc["verdict"] == "run failed" or not detail.get("sent") or detail.get("sent") != detail.get("planned"):
            raise Refused(f"{session}/parity did not finish ({detail.get('reason') or doc['verdict']}; "
                          f"{detail.get('sent', 0)} of {detail.get('planned', 0)} requests sent): there is no overview to publish")
        if not same_commit(doc["source"]["commit"], commit):
            raise Refused(f"{session}/parity measured {doc['source']['commit']}, the benchmark runs {commit}")
        if not detail.get("oracle_version"):
            raise Refused(f"{session}/parity records no Jellyfin version: the overview names what it compared against")
    record = load(os.path.join(session, "session.json"))
    return {"source": {"session": os.path.basename(os.path.normpath(session)), "commit": commit,
                       "oracle": f"Jellyfin {detail.get('oracle_version')}" if detail.get("oracle_version") else None,
                       "date": (record.get("started") or "")[:10] or None},
            "verdict": doc["verdict"], "counts": {k: doc["counts"].get(k, 0) for k in PARITY_OUTCOMES},
            "sent": detail["sent"], "planned": detail.get("planned", 0), "new_differences": detail.get("new_differences", 0),
            "filed_issues": detail.get("filed_issues", []),
            "areas": [{"area": a["area"], "requests": a["requests"], **{k: a.get(k, 0) for k in PARITY_OUTCOMES}}
                      for a in detail.get("areas", [])]}


#: How an A/B verdict reads in the README, and the colour of its bar.
AB_VERDICTS = {"improved": ("improved", "ferrofin"), "within noise": ("within noise", "jellyfin"),
               "regressed": ("regressed", "FAIL"), "not comparable": ("not comparable", "SKIP")}


def ab_summary(path):
    """The release A/B gate's gated cells (gates/bench/gate.py): each cell's previous and candidate medians,
    change and verdict, and the session's images, pairs and date. PATH is a gates session run with
    PREVIOUS= (its bench/ folder is the A/B gate) or the gate.py session itself."""
    d = os.path.join(path, "bench") if os.path.isfile(os.path.join(path, "session.json")) else path
    try:
        doc = result.read(d, "bench")
    except ValueError as error:
        raise Refused(f"{d}: {error}") from error
    if doc is None:
        raise Refused(f"{d} has no result.json (python3 gates/bench/gate.py render {d} writes it)")
    detail = doc["detail"]
    if detail.get("mode") != "ab":
        raise Refused(f"{d} is not an A/B gate session (its mode is {detail.get('mode')!r})")
    cells = [{"cell": key, "kind": v.get("kind"), "unit": v.get("unit"), "a_median": v.get("a_median"),
              "b_median": v.get("b_median"), "pct": v.get("pct"), "verdict": v.get("verdict"), "reason": v.get("reason")}
             for key, v in sorted(detail.get("verdicts", {}).items()) if v.get("gated")]
    if not cells:
        raise Refused(f"{d} judged no gated cell: there is nothing to publish")
    images = detail.get("images") or {}
    # each image's runs (a session from before `runs` was recorded: the values each verdict cell holds)
    pairs = (detail.get("runs") or {}).get("b") or max((len(v.get("b") or []) for v in detail.get("verdicts", {}).values()), default=0)
    if pairs < MIN_RUNS:
        raise Refused(f"{d} ran {pairs} pair{'s' if pairs != 1 else ''}: a release A/B gate is at least {MIN_RUNS}")
    return {"source": {"session": os.path.basename(os.path.normpath(path)), "previous": (images.get("a") or {}).get("ref"),
                       "candidate": (images.get("b") or {}).get("ref"), "commit": doc["source"].get("commit"),
                       "pairs": pairs, "date": (doc.get("started") or "")[:10] or None},
            "verdict": doc["verdict"], "blocking": detail.get("blocking", []), "cells": cells}


def summarise(version, bench_runs, session=None, ab=None):
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
        out["parity"] = parity_summary(session, out["bench"]["source"]["commit"])
    if ab:
        out["ab"] = ab_summary(ab)
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
    if "ab" in doc:
        ab = doc["ab"]
        if not isinstance(ab, dict) or not isinstance(ab.get("source"), dict) or not isinstance(ab.get("cells"), list):
            out.append("ab is not an object with a source and cells")
        else:
            out += [f"ab.source has no {k}" for k in ("previous", "candidate", "commit", "date") if not ab["source"].get(k)]
            if not isinstance(ab["source"].get("pairs"), int) or ab["source"]["pairs"] < MIN_RUNS:
                out.append(f"ab.source.pairs is under {MIN_RUNS}")
            for c in ab["cells"]:
                if (not isinstance(c, dict) or c.get("verdict") not in AB_VERDICTS or not c.get("cell")
                        or any(c.get(k) is not None and not isinstance(c.get(k), (int, float)) for k in ("a_median", "b_median", "pct"))):
                    out.append(f"an A/B cell is not shaped as expected: {str(c)[:80]}")
    if "parity" in doc:
        parity = doc["parity"]
        if not isinstance(parity, dict) or not isinstance(parity.get("counts"), dict) or not isinstance(parity.get("areas"), list):
            out.append("parity is not an object with counts and areas")
        else:
            out += [f"parity.source has no {k}" for k in ("session", "commit", "oracle", "date") if not (parity.get("source") or {}).get(k)]
            if sum(parity["counts"].get(k, 0) for k in PARITY_OUTCOMES) != parity.get("sent"):
                out.append("parity's counts do not add up to the requests sent")
            if sum(a.get("requests", 0) for a in parity["areas"] if isinstance(a, dict)) != parity.get("sent"):
                out.append("parity's areas do not add up to the requests sent")
            for a in parity["areas"]:
                if not isinstance(a, dict) or sum(a.get(k, 0) for k in PARITY_OUTCOMES) != a.get("requests") or not a.get("requests"):
                    out.append(f"a parity area is not shaped as expected: {str(a)[:80]}")
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


def parity_rows(areas):
    """The busiest areas, then one "other areas" row summing the rest."""
    top, rest = areas[:PARITY_AREAS], areas[PARITY_AREAS:]
    if rest:
        top = top + [{"area": f"{len(rest)} other areas", "requests": sum(a["requests"] for a in rest),
                      **{k: sum(a[k] for a in rest) for k in PARITY_OUTCOMES}}]
    return top


def parity_graph(title, parity, caption, theme_name, alt=""):
    """One stacked bar per API area, its length the requests sent there, split by outcome: identical,
    accepted differences, filed, new and not compared, with the area's share answered identically or
    with an accepted difference at the right."""
    t = STYLE[theme_name]
    rows = parity_rows(parity["areas"])
    left = label_width([r["area"] for r in rows]) + 14
    right = 150
    plot = WIDTH - left - right
    top = max(r["requests"] for r in rows)
    step = nice_step(top)
    scale_top = step * math.ceil(top / step)
    x = lambda v: left + plot * v / scale_top
    row_h, bar_h, y0 = 24, 14, 62
    end = y0 + row_h * len(rows)
    body = []
    for i in range(int(round(scale_top / step)) + 1):
        tx = x(i * step)
        body.append(f"<line x1='{tx:.1f}' y1='{y0 - 6}' x2='{tx:.1f}' y2='{end:.1f}' stroke='{t['grid']}' stroke-width='1'/>")
        body.append(text(tx, end + 15, f"{i * step:g}", 11, "muted", "middle", theme=t))
    body.append(text(left + plot / 2, end + 32, "requests jellyfin-web makes, by API area", 12, "muted", "middle", theme=t))
    for n, r in enumerate(rows):
        y = y0 + n * row_h
        body.append(text(left - 10, y + bar_h - 3, r["area"], 12, anchor="end", theme=t))
        at = left
        for key in PARITY_OUTCOMES:
            width = plot * r[key] / scale_top
            if width > 0:
                body.append(f"<rect x='{at:.1f}' y='{y:.1f}' width='{width:.1f}' height='{bar_h}' fill='{t[key]}'/>")
                at += width
        compared = r["requests"] - r["oracle_failed"]
        alike = r["identical"] + r["accepted"]
        # rounded down, so an area reads 100% only when every compared request is alike
        share = (("100" if alike == compared else f"{math.floor(1000 * alike / compared) / 10:g}") + "% alike"
                 if compared else "not compared")
        body.append(text(at + 5, y + bar_h - 3, f"{r['requests']}  ·  {share}", 11, "muted", theme=t))
    body.append(caption_lines(caption, end + 54, t))
    legend = tuple((k, PARITY_OUTCOMES[k].split(" (")[0]) for k in PARITY_OUTCOMES)
    return frame(title, end + 76, "".join(body), theme_name, legend, alt)


def parity_section(parity, version, rel, results_dir):
    """`(files, README lines)` of the parity overview."""
    src, c = parity["source"], parity["counts"]
    title = f"Answering jellyfin-web like {src['oracle']}"
    compared = parity["sent"] - c["oracle_failed"]
    alt = (f"{title}: of {parity['sent']} requests, " + ", ".join(f"{c[k]} {PARITY_OUTCOMES[k]}" for k in PARITY_OUTCOMES) + ".")
    cap = (f"{ferrofin(version)} (commit {src['commit'][:8]}) against {src['oracle']}, gates session {src['session']}, "
           f"{src['date']}. How differences are judged: gates/parity/README.md")
    files = {os.path.join(results_dir, version, f"parity{'' if theme == 'light' else '-dark'}.svg"):
             parity_graph(title, parity, cap, theme, alt) for theme in STYLE}
    issues = ", ".join(f"gl-{i}" if str(i).isdigit() else str(i) for i in parity["filed_issues"])
    lines = ["### Answering like Jellyfin\n",
             f"The parity gate replays the **{parity['sent']:,}** requests jellyfin-web makes against {src['oracle']} and "
             f"Ferrofin **{version}** (commit `{src['commit'][:12]}`, a local gates session on {src['date']}) and compares the "
             f"answers. Of the {compared:,} it could compare, **{c['identical']:,} are identical** and **{c['accepted']:,} differ "
             "in accepted ways** (each recorded with its reason in the parity ledger); "
             f"**{c['filed']} {'is filed as an issue' if c['filed'] == 1 else 'are filed as issues'}** and "
             f"**{c['new']} {'is' if c['new'] == 1 else 'are'} new**. {c['oracle_failed']} "
             f"{'was' if c['oracle_failed'] == 1 else 'were'} not compared because Jellyfin itself failed "
             f"{'it' if c['oracle_failed'] == 1 else 'them'}.\n",
             f'<picture>\n  <source media="(prefers-color-scheme: dark)" srcset="{rel}/parity-dark.svg">\n'
             f'  <img alt="{html.escape(alt)}" src="{rel}/parity.svg">\n</picture>\n',
             "<details><summary>Parity: every API area</summary>\n",
             "| Area | Requests | Identical | Accepted | Filed | New | Not compared |", "|---|---|---|---|---|---|---|"]
    lines += [f"| {md(a['area'])} | {a['requests']} | " + " | ".join(str(a[k]) for k in PARITY_OUTCOMES) + " |"
              for a in parity["areas"]]
    lines += ["", f"Filed as issues: {md(issues)}." if issues else "", "\n</details>\n"]
    return files, lines


def ab_ratio(cell):
    """How many times better the candidate is than the previous release on one cell (above 1 is better):
    the previous median over the candidate's (an A/B gate's cells are latencies and memory, lower is
    better); None when either median is missing or zero."""
    a, b = cell.get("a_median"), cell.get("b_median")
    if not (isinstance(a, (int, float)) and isinstance(b, (int, float)) and a > 0 and b > 0):
        return None
    return a / b


def ab_graph(title, cells, previous, caption, theme_name, alt=""):
    """One bar per gated cell from 1× on a log axis, its colour its verdict (improved, within noise,
    regressed), the biggest gain first; each bar carries its multiple and the two medians."""
    t = STYLE[theme_name]
    rows = sorted(((c, ab_ratio(c)) for c in cells if c["verdict"] != "not comparable" and ab_ratio(c)),
                  key=lambda cr: (-cr[1], cr[0]["cell"]))
    notes = [f"{speedup_text(r, c['unit'])}  ·  {value_text(c['b_median'], c['unit'])} vs {value_text(c['a_median'], c['unit'])}"
             f"  ·  {c['verdict']}" for c, r in rows]   # what is drawn after each bar, measured whole for the margin
    left = label_width([c["cell"] for c, _ in rows]) + 14
    right = label_width(notes, 11) + 12
    plot = WIDTH - left - right
    lo, hi = min([r for _, r in rows] + [1.0]), max([r for _, r in rows] + [1.0])
    lo_exp = min(math.floor(math.log2(lo)), -1) if lo < 1 else 0
    hi_exp = max(math.ceil(math.log2(hi)), 1)
    x = lambda v: left + plot * (math.log2(v) - lo_exp) / (hi_exp - lo_exp)
    row_h, bar_h, y0 = 22, 14, 60
    end = y0 + row_h * len(rows)
    body = []
    for e in range(lo_exp, hi_exp + 1):
        tx = x(2 ** e)
        body.append(f"<line x1='{tx:.1f}' y1='{y0 - 6}' x2='{tx:.1f}' y2='{end:.1f}' stroke='{t['grid' if e else 'muted']}' stroke-width='1'/>")
        body.append(text(tx, end + 15, f"{2 ** e:g}×", 11, "muted", "middle", theme=t))
    body.append(text(left + plot / 2, end + 32, f"times better than {previous} (median, log scale; right of 1× is better)",
                     12, "muted", "middle", theme=t))
    for n, ((c, r), note) in enumerate(zip(rows, notes)):
        y = y0 + n * row_h
        a, b = sorted((x(1), x(r)))
        colour = AB_VERDICTS[c["verdict"]][1]
        body.append(text(left - 10, y + bar_h - 3, c["cell"], 12, anchor="end", theme=t))
        body.append(f"<rect x='{a:.1f}' y='{y:.1f}' width='{max(b - a, 1):.1f}' height='{bar_h}' rx='2' fill='{t[colour]}'/>")
        body.append(text(max(b, x(1)) + 5, y + bar_h - 3, note, 11, "muted", theme=t))
    body.append(caption_lines(caption, end + 54, t))
    legend = tuple((AB_VERDICTS[v][1], AB_VERDICTS[v][0]) for v in ("improved", "within noise", "regressed"))
    return frame(title, end + 76, "".join(body), theme_name, legend, alt)


def release_name(ref):
    """`Ferrofin v1.3.2` for an image tagged with a release, else the image reference as given."""
    tag = re.search(r":(v\d+\.\d+\.\d+)$", ref or "")
    return f"Ferrofin {tag.group(1)}" if tag else str(ref)


def ab_section(ab, version, rel, results_dir):
    """`(files, README lines)` of the release against the previous one, from its A/B gate."""
    src, cells = ab["source"], ab["cells"]
    previous = release_name(src["previous"])
    count = {v: sum(c["verdict"] == v for c in cells) for v in AB_VERDICTS}
    title = f"{ferrofin(version)} against {previous}"
    alt = (f"{title}, the release A/B gate: of {len(cells)} gated cells, {count['improved']} improved, "
           f"{count['within noise']} within noise, {count['regressed']} regressed, {count['not comparable']} not comparable.")
    cap = (f"{ferrofin(version)} (commit {str(src['commit'])[:8]}) against {previous}, {src['pairs']} interleaved pairs, "
           f"{src['date']}. Images: {src['candidate']} against {src['previous']}; how cells are judged: gates/bench/README.md")
    files = {os.path.join(results_dir, version, f"previous-release{'' if theme == 'light' else '-dark'}.svg"):
             ab_graph(title, cells, previous, cap, theme, alt) for theme in STYLE}
    blocks = (f"; **{len(ab['blocking'])} gated cell{'s' if len(ab['blocking']) != 1 else ''} blocked the release**"
              if ab["blocking"] else "; nothing blocked the release")
    unjudged = (f", and {count['not comparable']} could not be compared (it blocks like a regression)"
                if count["not comparable"] else "")
    lines = ["### Against the previous release\n",
             f"Ferrofin **{version}** against **{previous}** (`{src['previous']}`) in the release's A/B gate ({src['pairs'] or '?'} "
             f"interleaved pairs on one host, {src['date']}): of {len(cells)} gated cells, **{count['improved']} improved**, "
             f"{count['within noise']} {'is' if count['within noise'] == 1 else 'are'} within noise and "
             f"**{count['regressed']} regressed**{unjudged}{blocks}.\n",
             f'<picture>\n  <source media="(prefers-color-scheme: dark)" srcset="{rel}/previous-release-dark.svg">\n'
             f'  <img alt="{html.escape(alt)}" src="{rel}/previous-release.svg">\n</picture>\n',
             f"<details><summary>Against {md(previous)}: every gated cell</summary>\n",
             "| Cell | Previous | Candidate | Change | Verdict |", "|---|---|---|---|---|"]
    fmt = lambda v, unit: value_text(v, unit) if isinstance(v, (int, float)) else "—"
    change = lambda c: format(c["pct"], "+.1f") + "%" if isinstance(c.get("pct"), (int, float)) else "—"
    why = lambda c: (": " + c["reason"] if c.get("reason") else
                     " (not drawn: a median is missing)" if c["verdict"] != "not comparable" and ab_ratio(c) is None else "")
    lines += [f"| {md(c['cell'])} | {fmt(c['a_median'], c['unit'])} | {fmt(c['b_median'], c['unit'])} | {change(c)} | "
              f"{md(c['verdict'] + why(c))} |" for c in cells]
    lines += ["\n</details>\n"]
    return files, lines


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


def version_key(version):
    return tuple(int(p) for p in version[1:].split("."))


def history_value(bench, metric):
    """`{server: value}` of one headline number of a release's benchmark summary, or None when it is
    withheld (the servers did different work) or missing."""
    if metric == "screens":
        screens = [r for r in (bench["levels"].get("loaded") or {}).get("screens", []) if r["ratio"] is not None]
        return {s: statistics.median(r[s]["median"] for r in screens) for s in ("ferrofin", "jellyfin")} if screens else None
    group, name = metric
    row = next((r for r in bench[group] if r["name"] == name), None)
    return {s: row[s]["median"] for s in ("ferrofin", "jellyfin")} if row and row["ratio"] is not None else None


#: The headline numbers drawn over the releases: (panel title, unit, what `history_value` reads).
HISTORY = (("Cold start", "ms", ("ttfs", "cold start (restart → authenticated views; host caches retained)")),
           ("Screens at 5 screens/s: median p50 of the comparable screens", "ms", "screens"),
           ("Peak memory under load", "MiB", ("memory", "peak under load")),
           ("Idle memory", "MiB", ("memory", "steady idle")))


def history_series(summaries):
    """The releases in order, each with its series: the host class and benchmark fixture it was measured
    on. A release on another host class or fixture than the one before starts a new series, because a moved
    host would read as a regression or a win."""
    out, last, series = [], None, 0
    for doc in sorted(summaries, key=lambda d: version_key(d["version"])):
        src = doc["bench"]["source"]
        key = json.dumps([src["host_class"], src["fixture"]], sort_keys=True)
        if last is not None and key != last:
            series += 1
        last = key
        out.append({"version": doc["version"], "oracle": src["oracle"], "series": series, "bench": doc["bench"]})
    return out


def history_graph(releases, theme_name, alt=""):
    """One panel per headline number: a point per Ferrofin release (Ferrofin's in its colour, the Jellyfin
    release it was measured against in grey), joined only within one series (a release a number is withheld
    in breaks its line), each labelled with its value. A dashed rule inside each plot marks where a new series
    starts."""
    # ponytail: one column per release; the x labels crowd from about seven releases, thin them out then
    t = STYLE[theme_name]
    left, right, panel_h, top_pad = 64, 40, 150, 62
    plot = WIDTH - left - right
    n = len(releases)
    x = lambda i: left + plot * (i + 0.5) / n
    body, y0 = [], top_pad
    for title, unit, metric in HISTORY:
        values = [history_value(r["bench"], metric) for r in releases]
        top = max([v[s] for v in values if v for s in v], default=0) or 1   # every number withheld, or zero
        step = nice_step(top, 4)
        scale_top = step * math.ceil(top * 1.15 / step)
        plot_top, plot_h = y0 + 22, panel_h - 50
        y = lambda v: plot_top + plot_h * (1 - v / scale_top)
        for i in range(1, n):   # where a new series starts, inside the plot (clear of the panel's title)
            if releases[i]["series"] != releases[i - 1]["series"]:
                sx = (x(i - 1) + x(i)) / 2
                body.append(f"<line x1='{sx:.1f}' y1='{plot_top:.1f}' x2='{sx:.1f}' y2='{plot_top + plot_h:.1f}' "
                            f"stroke='{t['muted']}' stroke-width='1' stroke-dasharray='4 4'/>")
        body.append(text(0, y0 + 10, f"{title} ({unit})", 13, weight="600", theme=t))
        for i in range(int(round(scale_top / step)) + 1):
            ty = y(i * step)
            body.append(f"<line x1='{left}' y1='{ty:.1f}' x2='{WIDTH - right}' y2='{ty:.1f}' stroke='{t['grid']}' stroke-width='1'/>")
            body.append(text(left - 8, ty + 4, f"{i * step:g}", 11, "muted", "end", theme=t))
        for server, colour in (("jellyfin", "jellyfin"), ("ferrofin", "ferrofin")):
            for i, v in enumerate(values):
                if not v:
                    continue
                prev = values[i - 1] if i else None
                if prev and releases[i - 1]["series"] == releases[i]["series"]:
                    body.append(f"<line x1='{x(i - 1):.1f}' y1='{y(prev[server]):.1f}' x2='{x(i):.1f}' y2='{y(v[server]):.1f}' "
                                f"stroke='{t[colour]}' stroke-width='2'/>")
                body.append(f"<circle cx='{x(i):.1f}' cy='{y(v[server]):.1f}' r='4' fill='{t[colour]}'/>")
                # the higher of the two values labelled above its point, the lower below, clear of the lines
                # (Ferrofin above on a tie)
                other = v["jellyfin" if server == "ferrofin" else "ferrofin"]
                above = v[server] > other or (v[server] == other and server == "ferrofin")
                body.append(text(x(i), y(v[server]) + (-9 if above else 17), value_text(v[server], unit, metric == "screens"), 11,
                                 "ink" if server == "ferrofin" else "muted", "middle", theme=t))
        for i, r in enumerate(releases):
            if not values[i]:
                body.append(text(x(i), plot_top + plot_h / 2, "withheld", 11, "muted", "middle", theme=t))
        y0 += panel_h
    for i, r in enumerate(releases):
        body.append(text(x(i), y0 + 4, r["version"], 12, anchor="middle", weight="600", theme=t))
        body.append(text(x(i), y0 + 19, f"vs {r['oracle']}", 11, "muted", "middle", theme=t))
        if i and r["series"] != releases[i - 1]["series"]:
            body.append(text((x(i - 1) + x(i)) / 2, y0 + 34, "new host or fixture", 10, "muted", "middle", theme=t))
    body.append(caption_lines("One point per Ferrofin release, against the Jellyfin release it was measured with. Points "
                              "measured on another host class or fixture start a new line.", y0 + 58, t))
    return frame("Release over release", y0 + 90, "".join(body), theme_name,
                 (("ferrofin", "Ferrofin"), ("jellyfin", "Jellyfin")), alt)


def history_section(summaries, rel_root, results_dir):
    """`(files, README lines)` of the release-over-release graph, drawn from every summary file."""
    releases = history_series(summaries)
    latest = releases[-1]
    numbers = ", ".join(f"{title.split(':')[0].lower()} {value_text(v['ferrofin'], unit, metric == 'screens')} vs "
                        f"{value_text(v['jellyfin'], unit, metric == 'screens')}"
                        for title, unit, metric in HISTORY for v in [history_value(latest["bench"], metric)] if v)
    alt = (f"Release over release: {len(releases)} release{'s' if len(releases) != 1 else ''}, "
           f"{releases[0]['version']} to {latest['version']}, on {latest['series'] + 1} host and fixture "
           f"series; the latest, {latest['version']} against {latest['oracle']}: {numbers or 'every number withheld'}.")
    files = {os.path.join(results_dir, f"history{'' if theme == 'light' else '-dark'}.svg"): history_graph(releases, theme, alt)
             for theme in STYLE}
    lines = ["### Release over release\n",
             f"The same headline numbers for every release published so far ({', '.join(r['version'] for r in releases)}), "
             "each against the Jellyfin release it was measured with. Only releases measured on the same host class and "
             "fixture are joined by a line.\n",
             f'<picture>\n  <source media="(prefers-color-scheme: dark)" srcset="{rel_root}/history-dark.svg">\n'
             f'  <img alt="{html.escape(alt)}" src="{rel_root}/history.svg">\n</picture>\n']
    return files, lines


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
    latest = max(summaries, key=lambda d: version_key(d["version"]))
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
    if "ab" in latest:
        more, lines = ab_section(latest["ab"], version, rel, results_dir)
        files.update(more)
        block += lines
    if "adoption" in latest:
        more, lines = adoption_section(latest["adoption"], version, rel, results_dir)
        files.update(more)
        block += lines
    if "parity" in latest:
        more, lines = parity_section(latest["parity"], version, rel, results_dir)
        files.update(more)
        block += lines
    more, lines = history_section(summaries, os.path.relpath(results_dir, readme_dir).replace(os.sep, "/"), results_dir)
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


def session_parity_graph(session, commit, version=""):
    """The README's parity overview graph, inline, for a gates session's parity result (the session page)."""
    parity = parity_summary(session, commit, publishing=False)
    if parity is None:   # a run that compared nothing: its row on the page already says why
        return ""
    files, _ = parity_section(parity, version, ".", session)
    return "".join(f"<div class='graph {theme}'>{svg}</div>"
                   for theme, svg in zip(STYLE, (files[p] for p in sorted(files, key=lambda p: p.endswith("-dark.svg")))))


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
    s.add_argument("--ab", metavar="DIR", help="the release's A/B gate session: its verdicts against the previous release")
    s.add_argument("--out", default=os.path.join(ROOT, "docs", "results"))
    r = sub.add_parser("readme")
    r.add_argument("--check", action="store_true")
    r.add_argument("--results", default=os.path.join(ROOT, "docs", "results"))
    r.add_argument("--readme", default=os.path.join(ROOT, "README.md"))
    args = parser.parse_args(argv)
    try:
        if args.command == "summarise":
            doc = summarise(args.version, args.bench, args.session, args.ab)
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
