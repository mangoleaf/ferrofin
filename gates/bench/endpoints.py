#!/usr/bin/env python3
"""The per-endpoint latency pass: every GET of the parity catalogue, one endpoint at a time (stdlib only).

    endpoints.py run URL IDS_JSON PHASE_DIR [--count N] [--max-s S] [--timeout S] [--set PATH ...]
        measure every endpoint on the server at URL and write PHASE_DIR/endpoints.json; exit 1 when
        the server answered no request at all, 2 on bad input
    endpoints.py plan [--bound] [--count N] [--max-s S] [--timeout S] [--set PATH ...]
        print the schedule (how many endpoints, which gate, which are skipped and why) without a
        server; with --bound, only the worst-case seconds of the pass (run.sh's client timeout)

B13 and decision DE of the "Benchmark coverage" milestone. The catalogue is the parity harness's
(`parity/README.md`): every GET of `parity/requests/sweep/` and `parity/requests/edge/`, read with
`parity/diff.py`, so the two harnesses cannot drift apart. HEAD requests, writes and a write's
read-backs are not endpoints a browsing client times. The templates are filled from the run's
ids file and the `{found.*}` names of `parity/discover.toml`, discovered on the server under test
(each server serves the same library); a name the server does not give skips only the endpoints
that use it, and says so.

The schedule (DE): discovery, then **one shared warm-up**, every endpoint once in catalogue order,
discarded; then each endpoint alone, `--count` sequential requests (200) or `--max-s` seconds (10),
whichever comes first: the request that crosses the limit still counts, and none is ever topped up.
One keep-alive connection, as the browse role of `screens.js` (its token and device). A latency
is the time from sending the request to having read the whole body, in ms.

`SKIP` names the GETs that are not measured, each with its reason; the report lists them. Only the
`bench/screens.js` shapes (`sweep/screens.jsonl`) and the edge cases (`edge/edge.jsonl`) gate
(`GATED_FILES`); every other endpoint is reported.

The A/B verdict per endpoint (`gate.endpoint_verdicts`) is the release gate's: each run's p50, the D4 rule of
`gate.verdict` over the interleaved pairs, judged by the `[endpoints]` row of gate.toml (D2) or a
per-cell band `endpoints/<id>`. An endpoint whose answers differ between runs (another status, a
status that changed within a run, the request filled differently) cannot be judged, and a gated
one then blocks, like every gated cell that cannot be judged.
"""

import argparse
import http.client
import json
import os
import sys
import time
import urllib.parse
from dataclasses import dataclass

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "parity"))
import diff  # noqa: E402  (the parity harness's request format, templates and discovery)

SETS = ("parity/requests/sweep", "parity/requests/edge")
DISCOVER = os.path.join(ROOT, "parity", "discover.toml")
#: The request files whose endpoints gate (DE, amended): jellyfin-web's screen shapes and the edge cases.
GATED_FILES = ("sweep/screens.jsonl", "edge/edge.jsonl")
#: The browse role of screens.js: its device, so the run's session checks see the session they expect.
DEVICE = "bench-browse"
PERCENTILES = (50, 95, 99)

MEDIA = ("media bytes, not an API answer: 200 reads of the file measure the disk, and starting "
         "playback is the time-to-first-screen phase's number")
TRANSCODE = ("playback, not browsing: an HLS request prepares or runs an ffmpeg transcode whose work "
             "outlives the request and would load the cores the next endpoints are measured on")
REMOTE = "asks an internet provider: its latency is the provider's, and 200 requests in a row would hammer it"
#: The catalogue GETs this pass does not measure, by request id, each with its reason.
SKIP = {
    "GetVideoStream": MEDIA, "GetVideoStreamByContainer": MEDIA, "GetAudioStream": MEDIA,
    "GetAudioStreamByContainer": MEDIA, "GetUniversalAudioStream": MEDIA, "GetDownload": MEDIA,
    "GetMasterHlsVideoPlaylist": TRANSCODE, "GetVariantHlsVideoPlaylist": TRANSCODE, "GetLiveHlsStream": TRANSCODE,
    "GetHlsVideoSegment": TRANSCODE, "GetMasterHlsAudioPlaylist": TRANSCODE, "GetVariantHlsAudioPlaylist": TRANSCODE,
    "GetHlsAudioSegment": TRANSCODE, "GetHlsVideoSegmentLegacy": TRANSCODE, "GetHlsPlaylistLegacy": TRANSCODE,
    "GetHlsAudioSegmentLegacyAac": TRANSCODE, "GetHlsAudioSegmentLegacyMp3": TRANSCODE,
    "GetRemoteImages": REMOTE, "SearchRemoteSubtitles": REMOTE, "SearchRemoteLyrics": REMOTE, "GetRemoteSubtitles": REMOTE,
}


def default_sets():
    return [os.path.join(ROOT, s) for s in SETS]


# ── the catalogue ───────────────────────────────────────────────────────────
@dataclass(frozen=True)
class Endpoint:
    request: diff.Request
    gates: bool
    skip: str     # why it is not measured, or ""


def catalogue(requests):
    """Every GET of the parsed request sets, in their order: whether it gates and why it is skipped."""
    return [Endpoint(r, r.file in GATED_FILES, SKIP.get(r.id, "")) for r in requests if r.method == "GET"]


def bound_s(endpoints, max_s, timeout):
    """The pass's worst case in seconds, if every request ends within its timeout (`Client`): discovery
    and the warm-up at one timeout per request, and each endpoint's last request starting just before
    `max_s`. Not strict: a retried request or a trickled body can take longer (`Client`)."""
    measured = sum(1 for e in endpoints if not e.skip)
    return int(len(diff.load_discover(DISCOVER)) * timeout + measured * (timeout + max_s + timeout))


# ── the pure part: percentiles, the loop's stop rule, a summary ─────────────
def percentile(vals, q):
    """The q-th percentile of `vals` by linear interpolation between ranks (k6's and numpy's default)."""
    if not vals:
        return None
    vals = sorted(vals)
    k = (len(vals) - 1) * q / 100
    lo = int(k)
    hi = min(lo + 1, len(vals) - 1)
    return vals[lo] + (vals[hi] - vals[lo]) * (k - lo)


def measure(send, path, count, max_s, clock=time.monotonic):
    """`(status, ms)` of up to `count` sequential requests, stopping once `max_s` seconds have passed
    (the request that crosses it still counts)."""
    samples, start = [], clock()
    while len(samples) < count:
        samples.append(send(path))
        if clock() - start >= max_s:
            break
    return samples


def summarise(samples, expect_status):
    """n, p50/p95/p99 (ms), each status's count (`none`: no answer) and the answers that were not `expect_status`."""
    ms = [t for _, t in samples]
    statuses = {}
    for status, _ in samples:
        key = "none" if status is None else str(status)
        statuses[key] = statuses.get(key, 0) + 1
    out = {"n": len(samples), "statuses": statuses, "errors": sum(s != expect_status for s, _ in samples)}
    out.update({f"p{q}": percentile(ms, q) for q in PERCENTILES})
    return out


# ── talking to the server ───────────────────────────────────────────────────
class Client:
    """One keep-alive connection; `get(path)` is `(status or None, ms)` with the whole body read.
    `timeout` bounds each socket operation (connect, each read), not the whole request: a server that
    trickles its body can take longer, and only run.sh's client timeout stops it. `reconnects` counts the
    times the server had closed the keep-alive connection."""

    def __init__(self, base, headers, timeout, clock=time.perf_counter):
        parts = urllib.parse.urlsplit(base)
        self.host, self.port, self.headers, self.timeout, self.clock = parts.hostname, parts.port, headers, timeout, clock
        self.conn = None
        self.reconnects = 0

    def get(self, path):
        for attempt in (1, 2):
            reused = self.conn is not None
            if not reused:
                self.conn = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)
            start = self.clock()
            try:
                self.conn.request("GET", path, headers=self.headers)
                reply = self.conn.getresponse()
                reply.read()
                if reply.will_close:
                    self.close()
                return reply.status, (self.clock() - start) * 1000
            except (http.client.RemoteDisconnected, BrokenPipeError, ConnectionResetError):
                # The server closed the idle connection: once, on a fresh one, timed from there.
                self.close()
                self.reconnects += reused
                if attempt == 2:
                    return None, (self.clock() - start) * 1000
            except (OSError, http.client.HTTPException):
                self.close()
                return None, (self.clock() - start) * 1000
        return None, 0.0

    def close(self):
        if self.conn is not None:
            self.conn.close()
            self.conn = None


def browse_headers(ids):
    token = diff.lookup(ids, "browse_token")
    return {"Authorization": f'MediaBrowser Client="bench", Device="bench", DeviceId="{DEVICE}", Version="3", Token="{token}"'}


def discover(base, headers, spec, ids, timeout):
    """`({name: value}, {name: why not})` for each name, read from the server under test."""
    found, missing = {}, {}

    def send(url, method, path, _headers, t):
        return diff.fetch(url, method, path, headers, t)
    for name, one in spec.items():
        try:
            found.update(diff.discover(base, "", {name: one}, ids, timeout, send=send))
        except diff.ParityError as err:
            missing[name] = str(err)
    return found, missing


def progress(line):
    print(line, flush=True)   # run.sh's console is a pipe: unflushed, the progress arrives at the end


def run(base, ids_path, out_dir, sets=None, count=200, max_s=10, timeout=10, discover_path=DISCOVER, log=progress):
    """Measure every endpoint and write `out_dir/endpoints.json`; returns the document."""
    started = time.time()
    with open(ids_path) as f:
        ids = json.load(f)
    sets = [str(s) for s in (sets or default_sets())]
    cat = catalogue(diff.load_requests(sets))
    spec = diff.load_discover(discover_path)
    used = diff.used_names([e.request for e in cat if not e.skip])
    headers = browse_headers(ids)
    found, missing = discover(base, headers, {n: spec[n] for n in sorted(used) if n in spec}, ids, timeout)
    filled_ids = {**ids, "found": found}
    rows, plan = [], []
    for e in cat:
        r = e.request
        row = {"id": r.id, "file": r.file, "source": r.source, "gates": e.gates, "expect_status": r.expect_status}
        if not e.skip:
            try:
                row["target"] = diff.target(r, filled_ids)
            except diff.ParityError as err:
                row["skipped"] = f"cannot be filled: {err}"
        else:
            row["skipped"] = e.skip
        rows.append(row)
        if "target" in row:
            plan.append(row)
    client = Client(base, headers, timeout)
    t0 = time.monotonic()
    for row in plan:   # the one shared warm-up
        status, ms = client.get(row["target"])
        row["warmup"] = {"status": status, "ms": ms}
    warmup_s = time.monotonic() - t0
    log(f"  endpoints: warm-up of {len(plan)} endpoints in {warmup_s:.0f} s")
    t1 = time.monotonic()
    for i, row in enumerate(plan, 1):
        t = time.monotonic()
        before = client.reconnects
        row.update(summarise(measure(client.get, row["target"], count, max_s), row["expect_status"]))
        row["reconnects"] = client.reconnects - before
        row["seconds"] = time.monotonic() - t
        if i % 100 == 0 or i == len(plan):
            log(f"  endpoints: {i}/{len(plan)} measured, {time.monotonic() - t1:.0f} s")
    client.close()
    doc = {"params": {"count": count, "max_s": max_s, "timeout_s": timeout, "sets": [os.path.relpath(s, ROOT) for s in sets],
                      "gated_files": list(GATED_FILES)},
           "found": found, "undiscovered": missing, "started": started, "warmup_s": warmup_s,
           "measure_s": time.monotonic() - t1, "endpoints": rows}
    tmp = os.path.join(out_dir, "endpoints.json.tmp")
    with open(tmp, "w") as f:
        json.dump(doc, f, indent=1)
    os.replace(tmp, os.path.join(out_dir, "endpoints.json"))
    return doc


# ── the A/B verdict ─────────────────────────────────────────────────────────
def by_id(doc):
    return {e["id"]: e for e in (doc or {}).get("endpoints", [])}


def uniform_status(e):
    """The one status every answer of a measured endpoint had, or None."""
    statuses = e.get("statuses") or {}
    return next(iter(statuses)) if len(statuses) == 1 else None


def problem(runs):
    """Why an endpoint's runs (its entries, in schedule order, None where a run lacks it) cannot be
    set beside each other, or None."""
    if any(e is None for e in runs):
        return "missing from a run"
    if any(e.get("skipped") for e in runs):
        return next(e["skipped"] for e in runs if e.get("skipped"))
    if any(e.get("p50") is None for e in runs):
        return "not measured in a run"
    if len({e.get("target") for e in runs}) > 1:
        return "asked differently: " + " vs ".join(sorted({e.get("target") or "?" for e in runs}))
    statuses = [uniform_status(e) for e in runs]
    if None in statuses:
        mixed = next(e for e, s in zip(runs, statuses) if s is None)
        return "answered with mixed statuses in a run: " + ", ".join(f"{k} × {v}" for k, v in sorted(mixed["statuses"].items()))
    if len(set(statuses)) > 1:
        return "answered " + " vs ".join(sorted(set(statuses)))
    return None


# ── the command line ────────────────────────────────────────────────────────
def main(argv):
    parser = argparse.ArgumentParser(prog="endpoints.py", description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("run", "plan"):
        p = sub.add_parser(name)
        if name == "run":
            p.add_argument("url")
            p.add_argument("ids")
            p.add_argument("out")
        else:
            p.add_argument("--bound", action="store_true")
        p.add_argument("--count", type=int, default=200)
        p.add_argument("--max-s", type=float, default=10)
        p.add_argument("--timeout", type=float, default=10)
        p.add_argument("--set", action="append", dest="sets")
    args = parser.parse_args(argv)
    sets = [os.path.join(ROOT, s) if not os.path.isabs(s) else s for s in args.sets] if args.sets else default_sets()
    try:
        if args.command == "plan":
            cat = catalogue(diff.load_requests(sets))
            if args.bound:
                print(bound_s(cat, args.max_s, args.timeout))
                return 0
            measured = [e for e in cat if not e.skip]
            print(f"{len(cat)} GETs: {len(measured)} measured, {sum(e.gates for e in measured)} of them gate "
                  f"({', '.join(GATED_FILES)}), {len(cat) - len(measured)} skipped; {args.count} requests or "
                  f"{args.max_s:g} s each after one shared warm-up; at most {bound_s(cat, args.max_s, args.timeout)} s")
            for e in cat:
                if e.skip:
                    print(f"  skipped {e.request.id} ({e.request.path}): {e.skip}")
            return 0
        doc = run(args.url, args.ids, args.out, sets, args.count, args.max_s, args.timeout)
    except (OSError, ValueError, diff.ParityError) as err:
        print(f"endpoints.py: {err}", file=sys.stderr)
        return 2
    # A server that never answered (every status `none`) measured nothing.
    return 0 if any(set(e.get("statuses", {})) - {"none"} for e in doc["endpoints"]) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
