#!/usr/bin/env python3
"""The saturation ramp's pure part: when does a step fail, where is the ceiling (stdlib only).

    ramp.py check K6_JSON BOUND_MS     exit 0 when the step passed, 1 (reason on stdout) when it did not
    ramp.py finish PHASE_DIR           write PHASE_DIR/ramp.json from its k6-ramp-<rate>.json files, print the
                                       ceiling; exit 1 when no step passed
    Both exit 2 when a file is missing or malformed.

`run.sh` owns the loop (it holds the server, the sampler-free k6 runs and the idle-core
check); this file owns every decision, so the decisions are unit-tested
(`test_ramp.py`) on constructed step series instead of on a 10-minute run.

A step is one fixed-rate loaded-mix window, the same workload as the `loaded` level at a
higher rate. The step's latency is its **worst screen's p95**: the mix has no single p95 in
k6's per-screen summary, and the slowest screen is the one a client notices first. The
ceiling is the last rate that passed before the first step that failed (D5).
"""

import glob
import json
import os
import re
import sys

#: Why a step stopped the ramp. Only `p95` and `errors` say something about the server;
#: `client` (k6 could not hold the rate) and `invalid` (no usable result) do not.
SERVER_LIMITS = ("p95", "errors")


def worst_p95(summary):
    """The largest per-screen p95 (ms) of a screens.js summary, or None without screens."""
    p95s = [s["p95"] for s in (summary.get("screens") or {}).values() if isinstance(s.get("p95"), (int, float))]
    return max(p95s) if p95s else None


def step_record(rate, summary):
    """What the ramp keeps of one step's k6 summary."""
    screens = (summary.get("screens") or {}).values()
    oks = [s["ok"] for s in screens if isinstance(s.get("ok"), (int, float))]
    return {"rate": rate, "p95": worst_p95(summary), "ok": min(oks) if oks else None,
            "dropped": summary.get("dropped_iterations") or 0, "iterations": summary.get("iterations") or 0}


def failure(step, bound_ms):
    """`(kind, reason)` when the step ends the ramp, else None. Checked in order: an
    unusable result, a request that failed, a client that fell behind its rate, then p95."""
    if not step["iterations"] or step["p95"] is None or step["ok"] is None:
        return "invalid", "no usable result"
    if step["ok"] < 1:
        return "errors", f"{(1 - step['ok']) * 100:.2f}% of a screen's requests failed"
    if step["dropped"]:
        return "client", f"k6 dropped {step['dropped']} iterations: the client, not the server, is the limit"
    if step["p95"] > bound_ms:
        return "p95", f"worst screen p95 {step['p95']:.0f} ms is over the {bound_ms:g} ms bound"
    return None


def ceiling(steps, bound_ms):
    """The throughput ceiling of ascending `steps`: the last rate before the first failing
    step. `limit` is what ended the ramp (`cap` when no step failed: the ceiling is then a
    lower bound). `ceiling` is None when even the first step failed."""
    last = None
    for step in sorted(steps, key=lambda s: s["rate"]):
        bad = failure(step, bound_ms)
        if bad:
            return {"ceiling": last, "limit": bad[0], "reason": bad[1], "stopped_at": step["rate"]}
        last = step["rate"]
    return {"ceiling": last, "limit": "cap", "reason": "no step failed before the cap", "stopped_at": None}


def comparable(result):
    """Whether a ramp's ceiling can be set beside another's, with the reason when not."""
    if result.get("ceiling") is None:
        return False, f"no passing step ({result.get('reason') or 'missing ramp'})"
    if result["limit"] not in (*SERVER_LIMITS, "cap"):
        return False, result["reason"]
    return True, None


def finish(phase_dir):
    """Collect `k6-ramp-<rate>.json` into `ramp.json` next to them and return it. The
    parameters come from `ramp-params.json`, which `run.sh` writes before the first step."""
    with open(os.path.join(phase_dir, "ramp-params.json")) as f:
        params = json.load(f)
    bound = params["bound_ms"]
    steps = []
    for path in glob.glob(os.path.join(phase_dir, "k6-ramp-*.json")):
        m = re.fullmatch(r"k6-ramp-(\d+)\.json", os.path.basename(path))
        if m:
            with open(path) as f:
                steps.append(step_record(int(m.group(1)), json.load(f)))
    steps.sort(key=lambda s: s["rate"])
    doc = {**params, **ceiling(steps, bound), "steps": steps}
    with open(os.path.join(phase_dir, "ramp.json"), "w") as f:
        json.dump(doc, f, indent=2)
    return doc


def rows(doc):
    """The step table of a `ramp.json`: rate, worst p95, success, dropped, verdict of the step."""
    bound = doc["bound_ms"]
    out = []
    for s in doc["steps"]:
        bad = failure(s, bound)
        out.append([s["rate"], "—" if s["p95"] is None else f"{s['p95']:.0f}",
                    "—" if s["ok"] is None else f"{s['ok'] * 100:.2f}%", s["dropped"], f"stopped: {bad[1]}" if bad else "passed"])
    return out


#: Exit status for "could not judge" (a missing or malformed file), apart from 1, which is
#: the answer: the step failed, or the ramp has no passing step.
CRASH = 2


def main(argv):
    try:
        if len(argv) == 3 and argv[0] == "check":
            with open(argv[1]) as f:
                bad = failure(step_record(0, json.load(f)), float(argv[2]))
            if bad:
                print(bad[1])
            return 1 if bad else 0
        if len(argv) == 2 and argv[0] == "finish":
            doc = finish(argv[1])
            print(f"  ramp: ceiling {doc['ceiling']} screens/s (limit: {doc['limit']}; {doc['reason']})")
            return 0 if doc["ceiling"] is not None else 1
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"ramp.py: {error!r}", file=sys.stderr)
        return CRASH
    sys.exit(__doc__)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
