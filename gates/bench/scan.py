#!/usr/bin/env python3
"""Library-scan and adoption-migration wall time and peak memory of one image (B7, B8).

    scan.py IMAGE TESTDATA OUT_DIR

scan       on an empty data dir: the startup wizard and the three bench libraries, set up as
           testdata/seed.py builds them (no remote fetchers, trickplay or chapter images),
           tasks drained and SETTLE_S idle, then one full library scan, timed from
           POST /Library/Refresh until the RefreshLibrary task reports a newer successful run.
migration  on a fresh copy of the test data's config, the bench corpus's own Jellyfin
           10.11.8 database: from the container's start to its first authenticated 200 on
           /UserViews, polled every POLL_MS as coldstart.py does.
After each, the movie, series, episode, album and track counts are recorded and must be at
least media/summary.json's, so a server that finds or keeps less is never "faster". Peak memory is
the largest cgroup `anon` sample (mem_sample.py) inside each timed span. The server gets
run.sh's cores and memory limit, and the tunables below share run.sh's names and defaults.
Writes OUT_DIR/scan.json, or exits non-zero. stdlib only.
"""

import csv
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time
import xml.etree.ElementTree as ET

BENCH = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(BENCH, "testdata"))
import coldstart  # noqa: E402
import seed  # noqa: E402

CONTAINER = "bench-scan"
PORT = 18105  # run.sh uses 18101 and 18103, build.sh 18096 and 18097
URL = f"http://127.0.0.1:{PORT}"
SERVER_CPUS = os.environ.get("SERVER_CPUS", "8-15")
CLIENT_CPUS = os.environ.get("CLIENT_CPUS", "16-19")
MEMORY = os.environ.get("MEMORY", "8g")
CORE_IDLE_MIN = float(os.environ.get("CORE_IDLE_MIN", "0.90"))
MEM_SAMPLE_MS = os.environ.get("MEM_SAMPLE_MS", "100")
SETTLE_S = int(os.environ.get("SETTLE_S", "30"))
POLL_MS = int(os.environ.get("POLL_MS", "10"))
READY_TIMEOUT_S = int(os.environ.get("READY_TIMEOUT_S", "300"))  # bounds the migration boot
HTTP_TIMEOUT_S = int(os.environ.get("HTTP_TIMEOUT_S", "10"))      # each docker command
CLEANUP_TIMEOUT_S = int(os.environ.get("CLEANUP_TIMEOUT_S", "30"))
SCAN_TIMEOUT_S = int(os.environ.get("SCAN_TIMEOUT_S", "1800"))
SCAN_POLL_S = 0.5  # the scan's resolution: under 1% of a two-minute scan, one small GET each
MIN_ITEMS = 10_000  # B7: the corpus holds at least this many items
COUNTS = (("movies", "Movie"), ("series", "Series"), ("episodes", "Episode"), ("albums", "MusicAlbum"),
          ("tracks", "Audio"))


def docker(*args):
    try:
        return subprocess.run(["docker", *args], check=True, capture_output=True, text=True,
                              timeout=HTTP_TIMEOUT_S).stdout.strip()
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"docker {args[0]}: {error.stderr.strip() or f'exit {error.returncode}'}") from error


def peak_anon_mib(csv_path, t0, t1):
    """The largest `anon` sample taken in [t0, t1], in MiB."""
    with open(csv_path) as f:
        vals = [int(r["anon"]) for r in csv.DictReader(f) if t0 <= float(r["t"]) <= t1]
    if not vals:
        raise RuntimeError("no memory sample inside the timed span")
    return max(vals) / 2**20


def disable_realtime_monitor(config):
    """The copy's libraries without a filesystem watcher, as run.sh provisions every copy."""
    files = list(Path(config).glob("root/default/*/options.xml"))
    if not files:
        raise RuntimeError(f"no library options under {config}/root/default")
    for path in files:
        tree = ET.parse(path)
        node = tree.getroot().find("EnableRealtimeMonitor")
        if node is None:
            node = ET.SubElement(tree.getroot(), "EnableRealtimeMonitor")
        node.text = "false"
        tree.write(path, encoding="utf-8", xml_declaration=True)


def counts(api, user, summary):
    """Item counts after a measurement; fewer than the fixture holds fails the run. More is
    recorded, not failed: listing alternate versions separately is not less work."""
    got = {k: api.get("/Items", userId=user, recursive="true", includeItemTypes=t, limit=0)["TotalRecordCount"]
           for k, t in COUNTS}
    short = {k: f"{v} (fixture {summary[k]})" for k, v in got.items() if v < summary[k]}
    if short:
        raise RuntimeError(f"fewer items than media/summary.json: {short}")
    return got


class Server:
    """The image under test, created stopped on `config`, the media read-only. On exit the
    sampler is stopped, the log goes to `<dir>/server.log` and the container is removed;
    a cleanup failure is raised only when the measurement itself succeeded."""

    def __init__(self, image, d, config, media):
        self.d, self.sampler = d, None
        os.makedirs(os.path.join(d, "cache"))
        docker("create", "--name", CONTAINER, "--user", f"{os.getuid()}:{os.getgid()}", "--cpuset-cpus", SERVER_CPUS,
               "--memory", MEMORY, "--memory-swap", MEMORY, "-p", f"127.0.0.1:{PORT}:8096",
               "-e", "FERROFIN_DATA_DIR=/config", "-e", "FERROFIN_CACHE_DIR=/cache",
               "-v", f"{config}:/config", "-v", f"{d}/cache:/cache", "-v", f"{media}:/media:ro", image)

    def __enter__(self):
        return self

    def sample(self):
        csv_path = os.path.join(self.d, "mem.csv")
        self.sampler = subprocess.Popen(["taskset", "-c", CLIENT_CPUS, sys.executable, os.path.join(BENCH, "mem_sample.py"),
                                         CONTAINER, csv_path, MEM_SAMPLE_MS, SERVER_CPUS])
        return csv_path

    def stop_sampler(self):
        sampler, self.sampler = self.sampler, None
        if sampler is None:
            return
        if sampler.poll() is None:
            sampler.terminate()
        try:
            rc = sampler.wait(CLEANUP_TIMEOUT_S)
        except subprocess.TimeoutExpired:
            sampler.kill()
            sampler.wait()
            raise RuntimeError("memory sampler did not stop") from None
        if rc:
            raise RuntimeError(f"memory sampler exited {rc}")

    def __exit__(self, failed, *_):
        errors = []
        for step in (self.stop_sampler, self.save_log, lambda: docker("rm", "-f", CONTAINER)):
            try:
                step()
            except (OSError, subprocess.SubprocessError, RuntimeError) as error:
                errors.append(error)
        # The measurement's own error is the one to report; cleanup failures are printed.
        raised = errors.pop(0) if errors and not failed else None
        for error in errors:
            print(f"scan: cleanup: {error}", file=sys.stderr)
        if raised:
            raise raised

    def save_log(self):
        with open(os.path.join(self.d, "server.log"), "w") as log:
            subprocess.run(["docker", "logs", CONTAINER], stdout=log, stderr=subprocess.STDOUT,
                           timeout=CLEANUP_TIMEOUT_S)


def measure_scan(image, testdata, out, summary):
    d = os.path.join(out, "scan")
    os.makedirs(os.path.join(d, "config"))
    with Server(image, d, os.path.join(d, "config"), os.path.join(testdata, "media")) as server:
        docker("start", CONTAINER)
        api = seed.Api(URL, timeout=HTTP_TIMEOUT_S)
        user = seed.setup(api)
        seed.drain(api, settle=SETTLE_S, label="startup: ")
        csv_path = server.sample()
        deadline = time.monotonic() + HTTP_TIMEOUT_S
        while not (os.path.exists(csv_path) and os.path.getsize(csv_path)):
            if time.monotonic() > deadline:
                raise RuntimeError("memory sampler wrote nothing")
            time.sleep(0.1)
        t0 = time.time()
        seed.refresh_and_wait(api, SCAN_TIMEOUT_S, poll=SCAN_POLL_S)
        t1 = time.time()
        server.stop_sampler()
        return {"wall_s": t1 - t0, "peak_anon_mib": peak_anon_mib(csv_path, t0, t1), "counts": counts(api, user, summary),
                "image_id": docker("inspect", "-f", "{{.Image}}", CONTAINER)}


def measure_migration(image, testdata, out, ids, summary):
    d = os.path.join(out, "migration")
    os.makedirs(d)
    config = os.path.join(d, "config")
    subprocess.run(["cp", "-r", "--reflink=auto", os.path.join(testdata, "config"), config], check=True)
    disable_realtime_monitor(config)
    headers = {"Authorization": f'MediaBrowser Client="bench", Device="bench", DeviceId="bench-migration", '
                                f'Version="3", Token="{ids["token"]}"'}
    with Server(image, d, config, os.path.join(testdata, "media")) as server:
        stop, started, result = threading.Event(), threading.Event(), []
        poller = threading.Thread(target=lambda: result.append(coldstart.poll(
            f"{URL}/UserViews?userId={ids['user']}", headers, READY_TIMEOUT_S, POLL_MS / 1000, stop, started)))
        poller.start()
        try:
            if not started.wait(2):
                raise RuntimeError("readiness poller did not start")
            docker("start", CONTAINER)
            csv_path = server.sample()
            t0 = coldstart.started_at(CONTAINER)
            poller.join(READY_TIMEOUT_S + 1)
        finally:
            stop.set()
            poller.join(2)  # each poll is bounded by one second
        t1 = result[0] if result else None
        if t1 is None:
            raise RuntimeError("no authenticated 200 before READY_TIMEOUT_S (a stale ids.json token never gets one)")
        server.stop_sampler()
        api = seed.Api(URL, timeout=HTTP_TIMEOUT_S, attempts=1)
        api.token = ids["token"]
        return {"wall_s": t1 - t0, "peak_anon_mib": peak_anon_mib(csv_path, t0, t1), "counts": counts(api, ids["user"], summary),
                "image_id": docker("inspect", "-f", "{{.Image}}", CONTAINER)}


def main(argv):
    if len(argv) != 3:
        sys.exit(__doc__)
    image, testdata, out = argv[0], os.path.abspath(argv[1]), os.path.abspath(argv[2])
    if os.path.exists(os.path.join(out, "scan.json")):
        sys.exit(f"scan: {out} already holds a measurement")
    with open(os.path.join(testdata, "ids.json")) as f:
        ids = json.load(f)
    with open(os.path.join(testdata, "media", "summary.json")) as f:
        summary = json.load(f)
    items = sum(summary[k] for k, _ in COUNTS)
    if items < MIN_ITEMS:
        sys.exit(f"scan: the test data holds {items} items; the scan phase needs at least {MIN_ITEMS}")
    mem = [sys.executable, os.path.join(BENCH, "mem_sample.py")]
    cpus = json.loads(subprocess.check_output([*mem, "--topology", SERVER_CPUS, CLIENT_CPUS], text=True))["checked_cpus"]
    idle = float(subprocess.check_output([*mem, "--check", ",".join(map(str, cpus))], text=True))
    if idle < CORE_IDLE_MIN:
        sys.exit(f"scan: cores {SERVER_CPUS},{CLIENT_CPUS} are only {idle:.0%} idle (need {CORE_IDLE_MIN:.0%})")
    # gate.py stops a run with SIGTERM: unwind, so the container and the sampler are removed.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    result = {"image": image, "preflight_idle": idle, "server_cpus": SERVER_CPUS, "memory_limit": MEMORY,
              "date": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    result["scan"] = measure_scan(image, testdata, out, summary)
    print(f"scan: {result['scan']['wall_s']:.1f} s, peak anon {result['scan']['peak_anon_mib']:.0f} MiB", flush=True)
    result["migration"] = measure_migration(image, testdata, out, ids, summary)
    print(f"migration: {result['migration']['wall_s']:.2f} s, peak anon {result['migration']['peak_anon_mib']:.0f} MiB", flush=True)
    result["image_id"] = result["scan"]["image_id"]
    with open(os.path.join(out, "scan.json"), "w") as f:
        json.dump(result, f, indent=2)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except (OSError, subprocess.SubprocessError, RuntimeError, KeyError, ValueError) as error:
        sys.exit(f"scan: {type(error).__name__}: {error}")
