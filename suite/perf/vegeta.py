#!/usr/bin/env python3
"""The vegeta engine seam — the ONLY module that talks to the load generator.

vegeta (pinned in mise.toml) is the measurement hot-path: a compiled,
open-model generator that dispatches requests on a constant arrival schedule
regardless of response times. That is the coordinated-omission fix (workstream
G): a closed loop (fixed VUs waiting on responses) under-samples stalls and
self-throttles load to whatever the server under test can absorb, so the two
servers are never measured under the same workload.

This module builds vegeta's JSON target lines, runs an attack, decodes the
per-request results, and reduces them to the summary shape the suite records.
Fairness rules enforced here, not left to callers:

- only responses with the endpoint's expected status enter the latency
  distribution (an error path is cheap and would fake a win); the ok-rate is
  reported alongside so a broken row is flagged, never hidden;
- the achieved arrival rate is measured and returned — a leg whose generator
  couldn't hold the schedule has silently degraded to a closed loop and must
  be failed by the caller (BENCH_RATE_TOLERANCE).
"""

import base64
import json
import shutil
import subprocess
import tempfile

from benchlib import render_body, render_path, token_headers


def vegeta_cmd():
    """The vegeta invocation: on PATH, else through mise (pinned in ./mise.toml)."""
    if shutil.which("vegeta"):
        return ["vegeta"]
    return ["mise", "exec", "--", "vegeta"]


def version():
    """The generator's version string, recorded into run meta (self-describing)."""
    try:
        out = subprocess.run(vegeta_cmd() + ["-version"], capture_output=True, text=True,
                             timeout=30, check=False).stdout
        for line in out.splitlines():
            if line.startswith("Version:"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return "unknown"


def build_targets(base, e, ctx, count=1):
    """vegeta JSON target lines for one ENDPOINTS entry.

    `count` > 1 emits that many distinct targets — used by the login storm,
    where every request needs a fresh DeviceId (both servers key sessions on
    it, and reusing one revokes the measurement token): uniqueness is target
    DATA, not generator code; vegeta rotates through the list.
    """
    url = f"{base}{render_path(e, ctx)}"
    targets = []
    for i in range(count):
        if e["name"] == "auth_login":
            headers = {
                "Content-Type": "application/json",
                "Authorization": f'MediaBrowser Client="bench", Device="bench", '
                                 f'DeviceId="bench-login-{i}", Version="1.0"',
            }
        elif e["auth"]:
            headers = token_headers(ctx["token"])
        else:
            headers = {"Content-Type": "application/json"}
        t = {"method": e["method"], "url": url,
             "header": {k: [v] for k, v in headers.items()}}
        if e["body"] is not None:
            t["body"] = base64.b64encode(
                json.dumps(render_body(e["body"], ctx)).encode()).decode()
        targets.append(t)
    return targets


def _run_attack(targets, extra_args, duration_secs):
    """Shared attack plumbing: write targets, run attack|encode, decode to
    (status_code, latency_ms) per request. Raises on generator failure (a
    broken generator must never read as a fast server)."""
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as tf:
        for t in targets:
            tf.write(json.dumps(t) + "\n")
        targets_path = tf.name
    atk = subprocess.Popen(
        vegeta_cmd() + ["attack", "-format=json", f"-targets={targets_path}",
                        f"-duration={duration_secs}s"] + extra_args,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    enc = subprocess.Popen(vegeta_cmd() + ["encode", "-to=json"],
                           stdin=atk.stdout, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    atk.stdout.close()
    out, enc_err = enc.communicate(timeout=duration_secs + 120)
    _, atk_err = atk.communicate(timeout=30)
    if atk.returncode != 0:
        raise RuntimeError(f"vegeta attack failed: {atk_err.decode(errors='replace')[:400]}")
    if enc.returncode != 0:
        raise RuntimeError(f"vegeta encode failed: {enc_err.decode(errors='replace')[:400]}")
    records = []
    for line in out.splitlines():
        r = json.loads(line)
        records.append((r["code"], r["latency"] / 1e6))  # ns → ms
    return records


def attack(targets, rate, duration_secs, *, timeout_secs=30):
    """One constant-rate (open-loop) attack → [(status_code, latency_ms)].

    Returns every dispatched request (all statuses) so callers can compute
    both the ok-rate and the achieved arrival rate."""
    return _run_attack(targets, [f"-rate={rate}", f"-timeout={timeout_secs}s"], duration_secs)


def max_attack(targets, duration_secs, *, workers=64, timeout_secs=30):
    """Max-throughput probe: vegeta's `-rate=0` mode (as fast as `workers`
    connections allow). Deliberately closed-loop — used ONLY for calibration
    (capacity estimates, generator ceiling), never for a measured comparison
    window."""
    return _run_attack(targets, ["-rate=0", f"-max-workers={workers}",
                                 f"-timeout={timeout_secs}s"], duration_secs)


def percentile(sorted_vals, p):
    """Linear-interpolation percentile (matches k6/numpy 'linear'), p in [0,100]."""
    if not sorted_vals:
        return None
    k = (len(sorted_vals) - 1) * p / 100
    lo, hi = int(k), min(int(k) + 1, len(sorted_vals) - 1)
    return sorted_vals[lo] + (sorted_vals[hi] - sorted_vals[lo]) * (k - lo)


def summarize(records, ok_status, duration_secs, rate=None):
    """Reduce per-request records to the suite's summary row.

    Latency percentiles are over expected-status responses ONLY; `okPct` and
    `achieved_rate` expose everything else. `rate_held` is the open-loop
    honesty flag the caller gates on.
    """
    ok_lat = sorted(lat for code, lat in records if code == ok_status)
    achieved = len(records) / duration_secs if duration_secs else 0
    out = {
        "p50": round(percentile(ok_lat, 50), 2) if ok_lat else None,
        "p95": round(percentile(ok_lat, 95), 2) if ok_lat else None,
        "p99": round(percentile(ok_lat, 99), 2) if ok_lat else None,
        "count": len(ok_lat),
        "rps": round(len(ok_lat) / duration_secs, 1) if duration_secs else 0,
        "okPct": round(100 * len(ok_lat) / len(records), 1) if records else 0,
        "achieved_rate": round(achieved, 1),
    }
    if rate is not None:
        out["target_rate"] = rate
    return out
