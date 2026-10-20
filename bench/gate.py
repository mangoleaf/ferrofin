#!/usr/bin/env python3
"""The release gate: an A/A noise measurement and an interleaved A/B (stdlib only).

    gate.py aa IMAGE [--runs N] [--testdata DIR] [--out DIR] [--dry-run]
        the same image N times (default 6), Ferrofin only: a per-cell noise table (range and
        spread of p50, p95, peak and steady anon memory) and the gate thresholds it suggests
    gate.py ab PREVIOUS CANDIDATE [--pairs N] [--testdata DIR] [--out DIR] [--dry-run]
        PREVIOUS CANDIDATE PREVIOUS CANDIDATE … (default 3 pairs), Ferrofin only: per cell,
        regressed / within noise / improved; exits 1 when a gated cell regressed or could
        not be judged
    gate.py ramp PREVIOUS CANDIDATE [--pairs N] [--testdata DIR] [--out DIR] [--dry-run]
        the saturation ramp (`run.sh --only ramp`, Ferrofin only) interleaved the same way: the
        throughput ceiling in screens/s per run, regressed / within noise / improved on the
        same rule with the ceiling as a higher-is-better cell (gate.toml `[ramp]`)
    gate.py scan PREVIOUS CANDIDATE [--pairs N] [--testdata DIR] [--out DIR] [--dry-run]
        the same interleaved pairs, each run `scan.py` (library-scan and adoption-migration
        wall time and peak anon memory, B7/B8) instead of run.sh; every cell gates (D6)
    gate.py soak PREVIOUS CANDIDATE [--pairs N] [--testdata DIR] [--out DIR] [--dry-run]
        the one-hour memory soak (`run.sh --only soak`, Ferrofin only) back to back, default one
        pair: the anon-memory slope in MiB/h per run, regressed when the candidate's exceeds the
        gate.toml `[soak]` limit and the previous release's does not (soak.py has the rule)
    gate.py render SESSION_DIR
        rewrite a finished session's report.md and report.html from its run dirs

Before anything starts, `oracle.check` must find the Jellyfin oracle pin (UPSTREAM_TAG) on the
latest Jellyfin release; a stale pin or a failed lookup refuses the session (decision DO).

Each run is `run.sh --servers ferrofin --only PHASES[mode]` (below; `scan` runs `scan.py`
instead) into SESSION_DIR/NN-a or
NN-b. The shape pass is what makes a cell comparable: without it every cell is caveated
for a missing phase and the change in the responses behind a cell cannot be seen.
`ferrofin:bench` is only ever inspected by run.sh, so the driver retags it before every run,
checks the run measured the image it was meant to, and puts the original tag back at the
end. A run that fails stops the session: it is reported, never retried.

Verdict rule (gate.toml, decisions D1–D4): a cell regressed when the median of the
candidate's runs exceeds the median of the previous release's by more than BOTH thresholds
of its row (relative and absolute) AND the candidate is slower in at least 2 of the 3
adjacent pairs; improved is the mirror image; anything else is within noise. Only the
unloaded and loaded levels gate, and an endpoint cell gates only when a median is 1 ms or
more (B5); the rest is reported.
"""

import html
import json
import math
import os
import signal
import statistics
import subprocess
import sys
from datetime import datetime, timezone

import oracle
import ramp
import report
import soak

BENCH = os.path.dirname(os.path.abspath(__file__))
TAG = "ferrofin:bench"
PHASES = {"aa": "shape,unloaded,loaded", "ab": "shape,unloaded,loaded", "ramp": "ramp", "soak": "soak"}
#: The levels whose cells can block a release; `stress` is reported, never gated (B5).
GATED_LEVELS = ("unloaded", "loaded")
#: An endpoint cell whose medians are both under this many ms is reported, never gated (B5).
SUB_MS = 1.0
#: A scan session's cells: (cell, scan.json part, field, unit, gate.toml row).
SCAN_CELLS = (("scan/wall time", "scan", "wall_s", "s", "scan"),
              ("scan/peak anon", "scan", "peak_anon_mib", "MiB", "memory"),
              ("migration/wall time", "migration", "wall_s", "s", "scan"),
              ("migration/peak anon", "migration", "peak_anon_mib", "MiB", "memory"))


# ── the pure part: cells, noise, verdicts ───────────────────────────────────
def cells(m):
    """Every gated Ferrofin cell of a `report.build` model, by its gate.toml key."""
    out = {}
    for level in m["levels"].values():
        for _, per in level["screens"] + level["endpoints"]:
            if "ferrofin" in per:
                out[per["ferrofin"].key] = per["ferrofin"]
    for _, per in m["memory"]:
        c = per.get("ferrofin")
        if c is not None and c.gate:
            out[c.key] = c
    return out


def spread(vals):
    """min, median, max, range as % of the median, and the coefficient of variation."""
    med = statistics.median(vals)
    cv = statistics.stdev(vals) / statistics.mean(vals) * 100 if len(vals) > 1 and statistics.mean(vals) else 0.0
    return {"n": len(vals), "min": min(vals), "median": med, "max": max(vals),
            "range": max(vals) - min(vals),
            "range_pct": (max(vals) - min(vals)) / med * 100 if med else 0.0, "cv_pct": cv}


def noise_table(m, gate):
    """Per cell of an A/A model (the same image, every run): the spread of p50 and p95
    (latency) or of the value (memory), the band that cell is judged by, and whether the
    runs alone already spanned more than that band (the band would raise false alarms)."""
    out = {}
    for key, c in sorted(cells(m).items()):
        row = {"kind": c.gate, "unit": c.unit, "flag": c.own_flag, "band": report.band(c, gate)}
        for metric, part in (("p50" if c.gate != "memory" else "value", c), ("p95", c.sub.get("p95"))):
            if part is not None and len(part.vals) >= 2:
                row[metric] = spread(part.vals)
        main = row.get("p50") or row.get("value")
        b = row["band"]
        row["exceeds_band"] = bool(main and b and main["range_pct"] > b["relative_pct"] and main["range"] > b["absolute"])
        out[key] = row
    return out


def round_up(v):
    """Up to one decimal, without float error turning 4.0 into 4.1."""
    return math.ceil(round(v * 10, 6)) / 10


def kind_summary(noise, gate):
    """Per gate.toml row: how many cells the A/A spanned past it, and the widest relative
    and absolute range any of its cells spanned — what a re-derived row is read against."""
    out = {}
    for row in noise.values():
        main = row.get("p50") or row.get("value")
        if not main:
            continue
        k = out.setdefault(row["kind"], {"cells": 0, "exceeding": 0, "max_range_pct": 0.0, "max_range": 0.0,
                                         "unit": row["unit"], "row": gate.get(row["kind"])})
        k["cells"] += 1
        k["exceeding"] += row["exceeds_band"]
        k["max_range_pct"] = max(k["max_range_pct"], main["range_pct"])
        k["max_range"] = max(k["max_range"], main["range"])
    return out


def proposed_cells(noise):
    """A per-cell gate.toml band for every cell whose A/A runs spanned more than its row:
    the range those runs spanned, rounded up. A proposal for the owner, not a decision."""
    return {key: {"relative_pct": math.ceil(row[m]["range_pct"]), "absolute": round_up(row[m]["range"])}
            for key, row in noise.items() if row["exceeds_band"]
            for m in ("p50", "value") if m in row}


def is_gated(key, a_median, b_median):
    level, kind = key.split("/")[:2]
    if kind == "endpoints":
        return level in GATED_LEVELS and max(a_median, b_median) >= SUB_MS
    return level in GATED_LEVELS or level == "memory"


def verdict(a, b, thresholds, higher_is_better=False):
    """The D4 rule over per-run values in schedule order (`a[i]` ran just before `b[i]`).
    Lower is better unless `higher_is_better` (the throughput ceiling). Returns (verdict,
    median delta, % delta, pairs in which b was worse)."""
    ma, mb = statistics.median(a), statistics.median(b)
    diff = mb - ma
    pct = diff / ma * 100 if ma else None
    sign = -1 if higher_is_better else 1  # sign * change > 0 is a change for the worse
    worse = sum(sign * (y - x) > 0 for x, y in zip(a, b))
    better = sum(sign * (y - x) < 0 for x, y in zip(a, b))
    need = len(a) // 2 + 1  # a majority of the pairs: 2 of 3
    beyond = thresholds and pct is not None and abs(pct) > thresholds["relative_pct"] and abs(diff) > thresholds["absolute"]
    if beyond and sign * diff > 0 and worse >= need:
        return "regressed", diff, pct, worse
    if beyond and sign * diff < 0 and better >= need:
        return "improved", diff, pct, worse
    return "within noise", diff, pct, worse


def ramp_verdicts(docs_a, docs_b, gate):
    """The one ramp cell, `ramp/ceiling`: per-run ceilings (screens/s, higher is better) of
    the previous release and the candidate, from their `ramp.json` documents (None when a
    run left none). A ramp that ended on the client or has no passing step cannot be
    judged, and an unjudgeable gated cell blocks like a regression."""
    key = "ramp/ceiling"
    band = (gate.get("cells") or {}).get(key) or gate.get("ramp")
    row = {"kind": "ramp", "unit": "screens/s", "band": band, "gated": True,
           "a": [(d or {}).get("ceiling") for d in docs_a], "b": [(d or {}).get("ceiling") for d in docs_b]}
    problems = sorted({f"{side} run {i}: {why}" for side, docs in (("previous", docs_a), ("candidate", docs_b))
                       for i, d in enumerate(docs, 1) for ok, why in [ramp.comparable(d or {})] if not ok})
    if problems or not row["a"] or len(row["a"]) != len(row["b"]):
        row.update(verdict="not comparable", reason="; ".join(problems) or "missing ramps")
    else:
        v, diff, pct, worse = verdict(row["a"], row["b"], band, higher_is_better=True)
        row.update(verdict=v, diff=diff, pct=pct, slower=worse, a_median=statistics.median(row["a"]),
                   b_median=statistics.median(row["b"]))
    return {key: row}


def verdicts(model_a, model_b, gate):
    """Per cell present on both sides: the verdict, or `not comparable` with the reason when
    either side's own evidence is flagged or the server did other work than in A."""
    ca, cb = cells(model_a), cells(model_b)
    out = {}
    for key in sorted(ca.keys() & cb.keys()):
        a, b = ca[key], cb[key]
        caveat = report.baseline_caveat(b, a)
        row = {"kind": a.gate, "unit": a.unit, "a": a.vals, "b": b.vals, "band": report.band(a, gate)}
        if caveat or not a.vals or len(a.vals) != len(b.vals):
            row.update(verdict="not comparable", reason=caveat or "missing values",
                       gated=is_gated(key, a.median or 0, b.median or 0))
        else:
            v, diff, pct, slower = verdict(a.vals, b.vals, row["band"])
            row.update(verdict=v, diff=diff, pct=pct, slower=slower, a_median=a.median, b_median=b.median,
                       gated=is_gated(key, a.median, b.median))
        out[key] = row
    return out


def scan_verdicts(results, gate):
    """Per SCAN_CELLS cell: the D4 verdict over each role's scan.json values in schedule
    order (`results[role]`), judged by the cell's own band or its gate.toml row."""
    out = {}
    for key, part, field, unit, kind in SCAN_CELLS:
        a, b = ([r[part][field] for r in results[role]] for role in ("a", "b"))
        band = (gate.get("cells") or {}).get(key) or gate.get(kind)
        v, diff, pct, slower = verdict(a, b, band)
        out[key] = {"kind": kind, "unit": unit, "a": a, "b": b, "band": band, "verdict": v, "diff": diff, "pct": pct,
                    "slower": slower, "a_median": statistics.median(a), "b_median": statistics.median(b), "gated": True}
    return out


def blocking(rows):
    """The gated cells that stop the release: regressed, or not judgeable."""
    return sorted(k for k, r in rows.items() if r["gated"] and r["verdict"] in ("regressed", "not comparable"))


# ── the driver ──────────────────────────────────────────────────────────────
def schedule(mode, images, count):
    """`(run dir name, role, image)` in run order. A/A: one image `count` times; A/B: the
    previous release then the candidate, `count` pairs, so every candidate run has the
    previous release's run just before it as its pair."""
    roles = ["a"] * count if mode == "aa" else ["a", "b"] * count
    return [(f"{i:02d}-{role}", role, images[role]) for i, role in enumerate(roles, 1)]


def docker(*args):
    return subprocess.run(["docker", *args], check=True, capture_output=True, text=True).stdout.strip()


def image_ref(image):
    try:
        return docker("image", "inspect", "-f", "{{.Id}}", image)
    except subprocess.CalledProcessError:
        return None


def run_sh(cmd):
    """A run (run.sh or scan.py) to completion. On Ctrl-C it gets SIGTERM and time to run
    its own cleanup (stop its container, finish phases.json): a killed run would leave a
    bench-* container behind that refuses the next run."""
    proc = subprocess.Popen(cmd)
    try:
        return proc.wait()
    except KeyboardInterrupt:
        proc.send_signal(signal.SIGTERM)
        proc.wait()
        raise


def run_session(session, plan, testdata):
    """Run the schedule; stop at the first run that fails or measured the wrong image."""
    if plan.get("mode") == "scan":
        return scan_session(session, plan, testdata)
    original = image_ref(TAG)
    try:
        for name, role, image in plan["schedule"]:
            out = os.path.join(session, name)
            print(f"== {name}: {role} = {image}", flush=True)
            docker("tag", plan["images"][role]["id"], TAG)
            cmd = ["bash", os.path.join(BENCH, "run.sh"), "--servers", "ferrofin", "--only", PHASES[plan["mode"]], "--out", out]
            if testdata:
                cmd += ["--testdata", testdata]
            rc = run_sh(cmd)
            if rc:
                sys.exit(f"gate: {name} failed (run.sh exit {rc}); the session stops here, see {out}")
            # image.txt is what the container actually ran ("<config image> <image id>"); run.json's
            # ferrofin_image is only what the tag pointed at when run.sh started.
            identity = (report.image_id(os.path.join(out, "ferrofin")) or "").split()
            measured = identity[-1] if identity else None
            if measured != plan["images"][role]["id"]:
                sys.exit(f"gate: {name} measured {measured or 'no image'}, not {image} — "
                         f"was {TAG} retagged during the run? The session stops here.")
    finally:
        if original:
            try:
                docker("tag", original, TAG)
            except subprocess.CalledProcessError as error:
                print(f"gate: could not restore {TAG} to {original}: {error.stderr}", file=sys.stderr)
        else:
            print(f"gate: {TAG} did not exist before the session; it now names the last image run", file=sys.stderr)


def scan_session(session, plan, testdata):
    """scan.py per run, given the image ID itself, so nothing is retagged; the ID its
    containers ran is checked all the same."""
    for name, role, image in plan["schedule"]:
        out = os.path.join(session, name)
        print(f"== {name}: {role} = {image}", flush=True)
        rc = run_sh([sys.executable, os.path.join(BENCH, "scan.py"), plan["images"][role]["id"],
                     testdata or os.path.join(BENCH, "testdata"), out])
        if rc:
            sys.exit(f"gate: {name} failed (scan.py exit {rc}); the session stops here, see {out}")
        measured = (report.load(os.path.join(out, "scan.json")) or {}).get("image_id")
        if measured != plan["images"][role]["id"]:
            sys.exit(f"gate: {name} measured {measured or 'no image'}, not {image}. The session stops here.")


def run_dirs(session, plan):
    by_role = {}
    for name, role, _ in plan["schedule"]:
        by_role.setdefault(role, []).append(os.path.join(session, name))
    return by_role


def models(session, plan):
    by_role = run_dirs(session, plan)
    return {role: report.build(runs) for role, runs in by_role.items()}, by_role


def soak_session(session, plan, gate):
    """The soak A/B's result: its cell, and each run's `soak.json` by role."""
    soaks = {role: [report.load(os.path.join(d, "ferrofin", "soak.json")) for d in dirs]
             for role, dirs in run_dirs(session, plan).items()}
    rows = soak.session_rows(soaks["a"], soaks["b"], gate)
    return {"verdicts": rows, "soaks": soaks, "blocking": soak.blocking(rows)}


def render(session):
    """Compute the session's tables, write report.md, report.html and the machine-readable
    noise.json (A/A) or verdict.json (A/B); returns the blocking cells."""
    plan = report.load(os.path.join(session, "gate.json"))
    if not plan:
        raise ValueError(f"{session} has no gate.json")
    gate = report.load_gate()
    if plan["mode"] == "soak":
        result = soak_session(session, plan, gate)
        with open(os.path.join(session, "verdict.json"), "w") as f:
            json.dump(result, f, indent=2, default=str)
        md = soak.render_md(header(plan), result)
        with open(os.path.join(session, "report.md"), "w") as f:
            f.write(md)
        with open(os.path.join(session, "report.html"), "w") as f:
            f.write(soak.render_html(header(plan), result, report.CSS))
        return md, result["blocking"]
    if plan["mode"] == "ramp":
        docs = {role: [report.load(os.path.join(d, "ferrofin", "ramp.json")) for d in dirs]
                for role, dirs in run_dirs(session, plan).items()}
        result = {"verdicts": ramp_verdicts(docs["a"], docs["b"], gate), "ramps": docs}
        name, block = "verdict.json", blocking(result["verdicts"])
    elif plan["mode"] == "scan":
        results = {}
        for run, role, _ in plan["schedule"]:
            measured = report.load(os.path.join(session, run, "scan.json"))
            if not measured:
                raise ValueError(f"{session}/{run} has no scan.json")
            results.setdefault(role, []).append(measured)
        result = {"verdicts": scan_verdicts(results, gate)}
        name, block = "verdict.json", blocking(result["verdicts"])
    elif plan["mode"] == "aa":
        built, by_role = models(session, plan)
        runs = by_role["a"]
        # The A/A runs split into the interleaved halves an A/B would compare: any verdict
        # but "within noise" there is a false alarm the thresholds would raise.
        odd, even = report.build(runs[0::2]), report.build(runs[1::2])
        result = {"noise": noise_table(built["a"], gate), "false_alarms": verdicts(odd, even, gate)}
        result["proposed_cells"] = proposed_cells(result["noise"])
        result["kinds"] = kind_summary(result["noise"], gate)
        name, block = "noise.json", []
    else:
        built, _ = models(session, plan)
        result = {"verdicts": verdicts(built["a"], built["b"], gate)}
        name, block = "verdict.json", blocking(result["verdicts"])
    result["blocking"] = block
    with open(os.path.join(session, name), "w") as f:
        json.dump(result, f, indent=2, default=str)
    md = render_md(plan, result)
    with open(os.path.join(session, "report.md"), "w") as f:
        f.write(md)
    with open(os.path.join(session, "report.html"), "w") as f:
        f.write(render_html(plan, result))
    return md, block


def fmt(v, unit):
    return "—" if v is None else f"{report.sig3(v)} {unit}"


def noise_rows(noise):
    for key, r in noise.items():
        main = r.get("p50") or r.get("value")
        if not main:
            continue
        p95 = r.get("p95")
        b = r["band"]
        yield (key, fmt(main["min"], r["unit"]), fmt(main["median"], r["unit"]), fmt(main["max"], r["unit"]),
               f"{main['range_pct']:.0f}%", f"{main['cv_pct']:.1f}%",
               f"{p95['range_pct']:.0f}%" if p95 else "—",
               f"{b['relative_pct']}% / {b['absolute']} {r['unit']}" if b else "—",
               "exceeds band" if r["exceeds_band"] else "", r["flag"] or "")


def verdict_rows(rows):
    for key, r in rows.items():
        b = r["band"]
        if "diff" in r:
            yield (key, fmt(r["a_median"], r["unit"]), fmt(r["b_median"], r["unit"]),
                   ("—" if r["pct"] is None else f"{r['pct']:+.0f}%") + f" ({r['diff']:+.2f} {r['unit']})", f"{r['slower']}/{len(r['a'])}",
                   f"{b['relative_pct']}% / {b['absolute']} {r['unit']}" if b else "—",
                   "yes" if r["gated"] else "reported", r["verdict"])
        else:
            yield (key, "—", "—", "—", "—", "—", "yes" if r["gated"] else "reported", f"not comparable: {r['reason']}")


def kind_rows(kinds):
    for kind, k in kinds.items():
        b = k["row"]
        yield (kind, k["cells"], k["exceeding"], f"{k['max_range_pct']:.0f}%", fmt(k["max_range"], k["unit"]),
               f"{b['relative_pct']}% / {b['absolute']} {k['unit']}" if b else "—")


KIND_HEAD = ("gate.toml row", "cells", "cells past the row", "widest range", "widest absolute range", "row now")
NOISE_HEAD = ("cell", "min", "median", "max", "range", "CV", "p95 range", "band", "", "flag")
VERDICT_HEAD = ("cell", "previous", "candidate", "change", "slower pairs", "band", "gates", "verdict")
RAMP_HEAD = ("cell", "previous", "candidate", "change", "worse pairs", "band", "gates", "verdict")
RAMP_RUN_HEAD = ("run", "ceiling", "limit", "stopped at", "why")


def ramp_run_rows(ramps):
    """Per run of each role: its ceiling, what ended the ramp and at which rate."""
    for role, docs in ramps.items():
        for i, d in enumerate(docs, 1):
            d = d or {}
            yield (f"{role} {i}", fmt(d.get("ceiling"), "screens/s"), d.get("limit", "no ramp.json"),
                   d.get("stopped_at") or "—", d.get("reason", "—"))


def md_table(head, rows):
    lines = ["| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    lines += ["| " + " | ".join(str(c).replace("|", "\\|") for c in row) + " |" for row in rows]
    return "\n".join(lines) + "\n"


def header(plan):
    imgs = "; ".join(f"{role} = {i['ref']} ({i['id'][:19]})" for role, i in plan["images"].items())
    o = plan.get("oracle")
    pin = f"; Jellyfin oracle {o['tag']} ({o['image']})" if o else ""
    return f"{plan['mode'].upper()} session {plan['name']}, {len(plan['schedule'])} runs, {plan['date']}: {imgs}{pin}"


def render_md(plan, result):
    parts = [f"# Benchmark gate — {plan['mode'].upper()}\n\n{header(plan)}\n"]
    if plan["mode"] == "aa":
        parts += ["\n## Noise per gate.toml row (what re-derived D1–D3 rows are read against)\n\n",
                  md_table(KIND_HEAD, kind_rows(result["kinds"])),
                  "\n## Noise per cell (p50 for latency, the value for memory)\n\n", md_table(NOISE_HEAD, noise_rows(result["noise"])),
                  "\n## Interleaved halves of the A/A (any verdict but within noise is a false alarm)\n\n",
                  md_table(VERDICT_HEAD, verdict_rows(result["false_alarms"]))]
        if result["proposed_cells"]:
            parts.append("\n## Proposed per-cell bands for gate.toml (owner decision, D8)\n\n```toml\n" + "".join(
                f'[cells."{k}"]\nrelative_pct = {v["relative_pct"]}\nabsolute = {v["absolute"]}\n\n'
                for k, v in result["proposed_cells"].items()).rstrip() + "\n```\n")
    elif plan["mode"] == "ramp":
        parts += ["\n## Throughput ceiling\n\n", md_table(RAMP_HEAD, verdict_rows(result["verdicts"])),
                  "\n## Per run\n\n", md_table(RAMP_RUN_HEAD, ramp_run_rows(result["ramps"])),
                  "\n" + (f"**BLOCKED**: {', '.join(result['blocking'])}\n" if result["blocking"] else "**PASS**: the ceiling did not regress.\n")]
    else:
        parts += ["\n## Verdict per cell\n\n", md_table(VERDICT_HEAD, verdict_rows(result["verdicts"])),
                  "\n" + (f"**BLOCKED**: {', '.join(result['blocking'])}\n" if result["blocking"] else "**PASS**: no gated cell regressed.\n")]
    return "".join(parts)


def marker(v):
    """The delta colour a verdict or a noise marker is shown in, if any."""
    v = str(v)
    if v in ("regressed", "improved"):
        return "bad" if v == "regressed" else "good"
    return "provisional" if v.startswith("not comparable") or v == "exceeds band" else ""


def html_table(head, rows, verdict_col=None):
    e = html.escape
    out = ["<table><thead><tr>" + "".join(f"<th>{e(h)}</th>" for h in head) + "</tr></thead><tbody>"]
    for row in rows:
        tds = []
        for i, v in enumerate(row):
            c = marker(v) if i == verdict_col or v == "exceeds band" else ""
            tds.append(f"<td class='num'><span class='delta {c}'>{e(str(v))}</span></td>" if c else f"<td class='num'>{e(str(v))}</td>")
        out.append("<tr>" + "".join(tds) + "</tr>")
    return "".join(out) + "</tbody></table>"


def render_html(plan, result):
    e = html.escape
    title = f"Benchmark gate {plan['mode'].upper()}"
    parts = ["<!doctype html><html lang='en'><head><meta charset='utf-8'><meta name='viewport' content='width=device-width, initial-scale=1'>",
             f"<title>{e(title)}</title><style>{report.CSS}</style></head><body><main><h1>{e(title)}</h1>",
             f"<p class='meta'>{e(header(plan))}</p>"]
    if plan["mode"] == "aa":
        parts += ["<h2>Noise per gate.toml row</h2><p class='lede'>The widest range any cell of a row spanned: "
                  "what a re-derived row is read against.</p>", html_table(KIND_HEAD, kind_rows(result["kinds"])),
                  "<h2>Noise per cell</h2><p class='lede'>The same image every run: the range and spread are what "
                  "run-to-run noise looks like for each cell. A cell marked <em>exceeds band</em> spanned more than its "
                  "gate.toml thresholds with no code change at all.</p>", html_table(NOISE_HEAD, noise_rows(result["noise"])),
                  "<h2>Interleaved halves</h2><p class='lede'>The A/A runs compared as an A/B would compare them; any "
                  "verdict but within noise is a false alarm.</p>",
                  html_table(VERDICT_HEAD, verdict_rows(result["false_alarms"]), verdict_col=7)]
    elif plan["mode"] == "ramp":
        verdict_text = (f"<b>Blocked</b> by {e(', '.join(result['blocking']))}" if result["blocking"]
                        else "<b>Pass</b>: the ceiling did not regress.")
        parts += [f"<h2>Throughput ceiling</h2><p class='lede'>{verdict_text} The ceiling is the last loaded-mix rate "
                  "(screens/s) whose worst-screen p95 stayed within the bound; higher is better.</p>",
                  html_table(RAMP_HEAD, verdict_rows(result["verdicts"]), verdict_col=7),
                  "<h2>Per run</h2>", html_table(RAMP_RUN_HEAD, ramp_run_rows(result["ramps"]))]
    else:
        verdict_text = (f"<b>Blocked</b> by {e(', '.join(result['blocking']))}" if result["blocking"]
                        else "<b>Pass</b>: no gated cell regressed.")
        parts += [f"<h2>Verdict per cell</h2><p class='lede'>{verdict_text}</p>",
                  html_table(VERDICT_HEAD, verdict_rows(result["verdicts"]), verdict_col=7)]
    return "".join(parts) + "</main></body></html>"


def main(argv):
    if len(argv) < 2 or argv[0] not in ("aa", "ab", "ramp", "scan", "soak", "render"):
        sys.exit(__doc__)
    mode, args = argv[0], argv[1:]
    if mode == "render":
        if len(args) != 1:
            sys.exit(__doc__)
        md, block = render(os.path.abspath(args[0]))
        print(md)
        return 1 if block else 0
    # One soak pair: each is an hour long (D7 of the "Benchmark coverage" milestone).
    opts = {"--runs": "6", "--pairs": "1" if mode == "soak" else "3", "--testdata": None, "--out": None}
    flags, positional = {"--dry-run"} & set(args), []
    rest = [a for a in args if a != "--dry-run"]
    while rest:
        a = rest.pop(0)
        if a in opts and rest:
            opts[a] = rest.pop(0)
        elif a.startswith("--"):
            sys.exit(f"unknown option {a}\n{__doc__}")
        else:
            positional.append(a)
    if len(positional) != (1 if mode == "aa" else 2):
        sys.exit(__doc__)
    count = opts["--runs"] if mode == "aa" else opts["--pairs"]
    # The A/A is judged in two interleaved halves; the D4 majority needs at least one pair.
    if not (count.isascii() and count.isdigit()) or int(count) < 1 or (mode == "aa" and int(count) % 2):
        sys.exit("gate: --runs must be an even number of at least 2, --pairs at least 1")
    count = int(count)
    images = {"a": positional[0]} if mode == "aa" else {"a": positional[0], "b": positional[1]}
    stamp = datetime.now(timezone.utc)
    session = os.path.abspath(opts["--out"] or os.path.join(BENCH, "runs", f"gate-{mode}-{stamp:%Y%m%d-%H%M}"))
    plan = {"mode": mode, "name": os.path.basename(session), "date": stamp.isoformat(timespec="seconds"),
            "schedule": schedule(mode, images, count)}
    print(f"{mode} session {session}:")
    what = "scan.py" if mode == "scan" else f"run.sh --servers ferrofin --only {PHASES[mode]}"
    for name, role, image in plan["schedule"]:
        print(f"  {name}  {role}  {image}  {what}")
    if flags:
        return 0
    if os.path.exists(session):
        sys.exit(f"gate: {session} exists — a session directory is never reused")
    # A release gate never runs against a Jellyfin pin behind the latest release (DO).
    try:
        plan["oracle"], problem = oracle.check()
    except (OSError, ValueError) as err:
        problem = f"cannot read the Jellyfin oracle pin ({err})"
    if problem:
        sys.exit(f"gate: refusing: {problem}")
    plan["images"] = {}
    for role, image in images.items():
        ref = image_ref(image)
        if not ref:
            sys.exit(f"gate: image {image} missing")
        plan["images"][role] = {"ref": image, "id": ref}
    os.makedirs(session)
    with open(os.path.join(session, "gate.json"), "w") as f:
        json.dump(plan, f, indent=2)
    run_session(session, plan, opts["--testdata"])
    md, block = render(session)
    print(md)
    print(f"report: {session}/report.html")
    return 1 if block else 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except ValueError as error:
        sys.exit(str(error))
