#!/usr/bin/env python3
"""The memory soak's pure part: the anon-memory slope and the A/B verdict (stdlib only).

    soak.py finish PHASE_DIR     write PHASE_DIR/soak.json from its mem.csv, windows.json and
                                 soak-params.json; exit 1 when no slope could be fitted,
                                 2 when a file is missing or malformed

`run.sh` calls `finish` only after `report.py --resource-check` has found the sampler's
coverage of the soak window complete (it brackets the window and has no gap over
`SAMPLE_GAP_INTERVALS` intervals), so `finish` fits whatever the window holds.

`run.sh --only soak` holds the server under the loaded mix for one hour (`SOAK_S`) while
`mem_sample.py` records cgroup anon memory every `MEM_SAMPLE_MS`. The slope is the
least-squares line through those samples, in MiB per hour, leaving out the window's first
`SOAK_SKIP_S` seconds: heap growth in the first minutes is warm-up, not a leak.

Verdict (D7 of the "Benchmark coverage" milestone): the candidate **regressed** when its
slope exceeds the limit (`[soak]` in gate.toml) and the previous release's does not;
**improved** when the previous release's exceeds it and the candidate's does not; anything
else is within noise, so a candidate that leaks faster than a previous release that already
leaked is not a new problem (the numbers are still in the table).
"""

import csv
import html
import json
import os
import statistics
import sys

MIB = 2 ** 20
#: Seconds in the unit the slope is reported in.
HOUR = 3600
#: Exit status for "could not judge" (a missing or malformed file), apart from 1, which is
#: the answer: no slope could be fitted.
CRASH = 2
CELL = "soak/slope"


def slope(samples):
    """Least-squares slope, in MiB per hour, of `(unix seconds, anon bytes)` samples; None
    without two distinct times."""
    if len(samples) < 2:
        return None
    t0 = samples[0][0]
    xs = [t - t0 for t, _ in samples]
    ys = [anon / MIB for _, anon in samples]
    mx, my = statistics.fmean(xs), statistics.fmean(ys)
    spread = sum((x - mx) ** 2 for x in xs)
    if not spread:
        return None
    return sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / spread * HOUR


def fit(samples, window, skip_s):
    """The slope over `window` (`{"start", "end"}`, unix seconds) after its first `skip_s`
    seconds, with what it was fitted on."""
    lo, hi = window["start"] + skip_s, window["end"]
    inside = [(t, anon) for t, anon in samples if lo <= t <= hi]
    result = {"slope_mib_per_hour": slope(inside), "samples": len(inside), "fit_from": lo, "fit_to": hi}
    if result["slope_mib_per_hour"] is None:
        result["reason"] = f"{len(inside)} samples between {lo:.0f} and {hi:.0f}: not enough to fit a slope"
    else:
        result["peak_mib"] = max(anon for _, anon in inside) / MIB
    return result


def verdict(previous, candidate, limit):
    """D7 over the two slopes (MiB/h); `not comparable` when either is missing."""
    if previous is None or candidate is None:
        return "not comparable"
    if candidate > limit >= previous:
        return "regressed"
    if previous > limit >= candidate:
        return "improved"
    return "within noise"


def load_samples(path):
    """`(t, anon bytes)` rows of a sampler csv."""
    with open(path, newline="") as f:
        return [(float(r["t"]), int(r["anon"])) for r in csv.DictReader(f)]


def finish(phase_dir):
    """Fit the soak window of a run's server dir and write `soak.json` beside the inputs."""
    with open(os.path.join(phase_dir, "soak-params.json")) as f:
        params = json.load(f)
    with open(os.path.join(phase_dir, "windows.json")) as f:
        window = json.load(f)["soak"]
    doc = {**params, **fit(load_samples(os.path.join(phase_dir, "mem.csv")), window, params["skip_s"])}
    with open(os.path.join(phase_dir, "soak.json"), "w") as f:
        json.dump(doc, f, indent=2)
    return doc


# ── the A/B session (gate.py soak) ──────────────────────────────────────────
def session_rows(docs_a, docs_b, gate):
    """The one gated cell of an A/B soak, `soak/slope`: each side's median slope over its
    runs (None when any run left no slope), the limit and the D7 verdict."""
    limit = gate["soak"]["max_slope_mib_per_hour"]
    slopes = [[(d or {}).get("slope_mib_per_hour") for d in docs] for docs in (docs_a, docs_b)]
    medians = [None if not s or None in s else statistics.median(s) for s in slopes]
    row = {"kind": "soak", "unit": "MiB/h", "limit": limit, "gated": True, "a": slopes[0], "b": slopes[1],
           "a_median": medians[0], "b_median": medians[1], "verdict": verdict(*medians, limit)}
    if row["verdict"] == "not comparable":
        row["reason"] = "a soak left no slope (a run that failed, or too few samples)"
    return {CELL: row}


def blocking(rows):
    """The gated cells that stop the release: regressed, or not judgeable."""
    return sorted(k for k, r in rows.items() if r["gated"] and r["verdict"] in ("regressed", "not comparable"))


HEAD = ("cell", "previous", "candidate", "limit", "gates", "verdict")
RUN_HEAD = ("run", "slope", "samples", "fit window")


def _fmt(v):
    return "—" if v is None else f"{v:.1f} MiB/h"


def _verdict_rows(rows):
    for key, r in rows.items():
        yield (key, _fmt(r["a_median"]), _fmt(r["b_median"]), f"{r['limit']} MiB/h", "yes" if r["gated"] else "reported",
               r["verdict"] if "reason" not in r else f"{r['verdict']}: {r['reason']}")


def _run_rows(soaks):
    for role, docs in soaks.items():
        for i, d in enumerate(docs, 1):
            d = d or {}
            yield (f"{role} {i}", _fmt(d.get("slope_mib_per_hour")), d.get("samples", "—"),
                   f"after {d['skip_s']} s of {d['soak_s']} s" if "skip_s" in d else "—")


def _md_table(head, rows):
    lines = ["| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    lines += ["| " + " | ".join(str(c).replace("|", "\\|") for c in row) + " |" for row in rows]
    return "\n".join(lines) + "\n"


def render_md(header, result):
    verdict_text = (f"**BLOCKED**: {', '.join(result['blocking'])}\n" if result["blocking"]
                    else "**PASS**: no new leak.\n")
    return (f"# Benchmark gate — SOAK\n\n{header}\n\n## Anon-memory slope\n\n"
            + _md_table(HEAD, _verdict_rows(result["verdicts"])) + "\n## Per run\n\n"
            + _md_table(RUN_HEAD, _run_rows(result["soaks"])) + "\n" + verdict_text)


def _html_table(head, rows, verdict_col=None):
    e = html.escape
    out = ["<table><thead><tr>" + "".join(f"<th>{e(h)}</th>" for h in head) + "</tr></thead><tbody>"]
    for row in rows:
        tds = []
        for i, v in enumerate(row):
            cls = ""
            if i == verdict_col:
                cls = "bad" if str(v).startswith("regressed") else "good" if str(v).startswith("improved") else (
                    "provisional" if str(v).startswith("not comparable") else "")
            tds.append(f"<td class='num'><span class='delta {cls}'>{e(str(v))}</span></td>" if cls else f"<td class='num'>{e(str(v))}</td>")
        out.append("<tr>" + "".join(tds) + "</tr>")
    return "".join(out) + "</tbody></table>"


def render_html(header, result, css):
    e = html.escape
    text = (f"<b>Blocked</b> by {e(', '.join(result['blocking']))}" if result["blocking"]
            else "<b>Pass</b>: no new leak.")
    return ("<!doctype html><html lang='en'><head><meta charset='utf-8'><meta name='viewport' content='width=device-width, initial-scale=1'>"
            f"<title>Benchmark gate SOAK</title><style>{css}</style></head><body><main><h1>Benchmark gate SOAK</h1>"
            f"<p class='meta'>{e(header)}</p><h2>Anon-memory slope</h2>"
            f"<p class='lede'>{text} The slope is the least-squares line through the cgroup anon samples of the soak "
            "window after its warm-up, in MiB per hour; lower is better.</p>"
            + _html_table(HEAD, _verdict_rows(result["verdicts"]), verdict_col=5)
            + "<h2>Per run</h2>" + _html_table(RUN_HEAD, _run_rows(result["soaks"])) + "</main></body></html>")


def rows(doc):
    """The run report's lines for one `soak.json`: label and value."""
    if doc.get("slope_mib_per_hour") is None:
        return [("slope", f"none: {doc.get('reason', 'not fitted')}")]
    return [("slope", f"{doc['slope_mib_per_hour']:.1f} MiB/h"),
            ("samples fitted", doc["samples"]),
            ("fit window", f"after the first {doc['skip_s']} s of the {doc['soak_s']} s soak at {doc['rate']} screens/s"),
            ("peak anon memory in the fit window", f"{doc['peak_mib']:.0f} MiB")]


def main(argv):
    try:
        if len(argv) == 2 and argv[0] == "finish":
            doc = finish(argv[1])
            if doc["slope_mib_per_hour"] is None:
                print(f"  soak: no slope ({doc['reason']})")
                return 1
            print(f"  soak: {doc['slope_mib_per_hour']:.1f} MiB/h over {doc['samples']} samples")
            return 0
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"soak.py: {error!r}", file=sys.stderr)
        return CRASH
    sys.exit(__doc__)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
