#!/usr/bin/env python3
"""GitHub CI job and step times as medians over recent successful runs.

    scripts/ci-timings.py [--runs N] [--after ISO] [--before ISO]

GitHub's runners differ in speed by about 1.8x from one run to the next, so one run
measured against a fixed baseline proves nothing. Compare the medians of a window of runs
before a change with a window after it (--after/--before select runs by creation time,
newest first).

For each successful run of the CI workflow, from any branch or event (cancelled and failed
runs are skipped), it reads the job and step times through `gh api`. For each Rust job (one
with a rust-cache or a test step) it also reads the job log to:
- split the test step into compile (step start to cargo's "Finished `test` profile" line)
  and execution (that line to the step's end), when the step ran one cargo test build;
- count the tests (nextest's "Summary" lines, or the sum of libtest's "test result:" lines);
- see whether rust-cache restored a cache (a partial restore-keys match counts as a hit).
An aggregate check (`lint`, `coverage`) also gets a span: from the first of the jobs it gates
to start, to its own end, the time the check takes to report. Its total is only its own few
seconds once it gates other jobs, so a window spanning the split compares spans, not totals.
Steps are matched by what they run, not by position, so a changed job layout still reports.

Every figure is median [min-max] (samples), in seconds. Jobs are keyed by name, so a window
spanning a job's rename or change of role mixes the two. Needs `gh`, logged in.
"""
import argparse
import datetime as dt
import json
import re
import statistics
import subprocess
import sys

REPO = "mangoleaf/ferrofin"
WORKFLOW = "ci.yml"

# Report label -> pattern over the step name (GitHub names an unnamed step "Run <command>").
STEPS = {
    "free disk": re.compile(r"free.*disk", re.I),  # gone since gl-553; older runs have it
    "rust-cache restore": re.compile(r"^Run Swatinem/rust-cache@"),
    "clippy": re.compile(r"cargo clippy"),
    "tests": re.compile(r"cargo nextest run|cargo test(?!.*--doc)|^coverage gate|^Run scripts/coverage-gate"),
}
# Aggregate check -> the jobs it gates (a matrix job is named "<job> (<value>)"). A run from
# before a check was split has the check alone, so its span is its total.
GROUPS = {"lint": ("clippy", "lint-tests"), "coverage": ("coverage-shard",)}
METRICS = ["total", "span", "free disk", "rust-cache restore", "clippy",
           "tests", "test compile", "test execution", "tests run"]

ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
FINISHED = re.compile(r"Finished `test` profile")
NEXTEST = re.compile(r"^\s*Summary \[.*?\] (\d+) tests? run")
LIBTEST = re.compile(r"test result: \w+\. (\d+) passed; (\d+) failed")


def gh(*args):
    """stdout of `gh api <args>`; exits with gh's own message when it fails."""
    try:
        return subprocess.run(["gh", "api", *args], check=True, capture_output=True,
                              encoding="utf-8", errors="replace").stdout
    except FileNotFoundError:
        sys.exit("ci-timings: needs the gh CLI")
    except subprocess.CalledProcessError as e:
        sys.exit(f"ci-timings: gh api {args[-1]}: {e.stderr.strip()}")


def ts(stamp):
    """A GitHub timestamp: the API's whole seconds, or a log's 7-digit fraction (beyond
    what fromisoformat takes), both ending in Z."""
    base, _, frac = stamp.rstrip("Z").partition(".")
    return dt.datetime.fromisoformat(f"{base}.{frac[:6]:0<6}+00:00")


def parse_log(text):
    """[(timestamp, line)] of a job log, each raw line being `<timestamp> <text>`. The first
    line carries a byte-order mark; ANSI colour codes are dropped."""
    out = []
    for raw in text.splitlines():
        stamp, _, line = raw.lstrip("\ufeff").partition(" ")
        try:
            out.append((ts(stamp), ANSI.sub("", line)))
        except ValueError:
            continue
    return out


def window(lines, start, end):
    """The log lines of a step. The API truncates step times to the second, so a step's first
    second can hold the previous step's last lines and its last second the next step's first
    ones. Each step starts with a `##[group]Run` header: for a step longer than a second, its
    own header is the last one in its first second, and the next step's the first one in its
    last."""
    win = [(t, line) for t, line in lines if start <= t.replace(microsecond=0) <= end]
    if start < end:
        heads = [(t.replace(microsecond=0), i) for i, (t, line) in enumerate(win)
                 if line.startswith("##[group]Run ")]
        own = [i for t, i in heads if t == start]
        nxt = [i for t, i in heads if t == end]
        win = win[own[-1] if own else 0:nxt[0] if nxt else len(win)]
    return win


def cache_hit(lines):
    """True when rust-cache restored a cache, False when it found none, else None."""
    for _, line in lines:
        if "Restored from cache key" in line:
            return True
        if "No cache found" in line:
            return False
    return None


def read_test_step(lines, start, end):
    """(compile s, execution s, tests run) of one test step. Compile and execution are None
    unless the step ran exactly one cargo test build (a coverage shard builds per crate)."""
    win = window(lines, start, end)
    finished = [t for t, line in win if FINISHED.search(line)]
    nextest = [int(m[1]) for _, line in win if (m := NEXTEST.search(line))]
    libtest = [int(m[1]) + int(m[2]) for _, line in win if (m := LIBTEST.search(line))]
    count = sum(nextest) if nextest else sum(libtest) if libtest else None
    if len(finished) != 1:
        return None, None, count
    return (finished[0] - start).total_seconds(), (end - finished[0]).total_seconds(), count


def successful_runs(n, after, before):
    """The newest n successful CI runs created in the window."""
    created = (f"{after}..{before}" if after and before
               else f">={after}" if after else f"<={before}" if before else None)
    found, page = [], 1
    while len(found) < n:
        args = ["-X", "GET", f"repos/{REPO}/actions/workflows/{WORKFLOW}/runs",
                "-f", "status=success", "-f", "per_page=100", "-f", f"page={page}"]
        if created:
            args += ["-f", f"created={created}"]
        batch = json.loads(gh(*args))["workflow_runs"]
        found += batch
        if len(batch) < 100:
            break
        page += 1
    return found[:n]


def collect(runs):
    """([run wall-clock], {job: {metric: [values]}})."""
    wall, samples = [], {}

    def add(job, metric, value):
        if value is not None:
            samples.setdefault(job, {}).setdefault(metric, []).append(value)

    for run in runs:
        jobs = json.loads(gh("-X", "GET", f"repos/{REPO}/actions/runs/{run['id']}/jobs",
                             "-f", "per_page=100"))["jobs"]
        jobs = [j for j in jobs if j["conclusion"] == "success"]
        if not jobs:  # every job skipped by its `if:`
            continue
        end = max(ts(j["completed_at"]) for j in jobs)
        wall.append((end - ts(run["run_started_at"])).total_seconds())
        for check, gated in GROUPS.items():
            own = [j for j in jobs if j["name"] == check]
            if own:
                start = min(ts(j["started_at"]) for j in jobs
                            if j["name"] == check or j["name"].split(" (")[0] in gated)
                add(check, "span", (ts(own[0]["completed_at"]) - start).total_seconds())
        for job in jobs:
            name = job["name"]
            add(name, "total", (ts(job["completed_at"]) - ts(job["started_at"])).total_seconds())
            steps = {label: [s for s in job["steps"]
                             if pat.search(s["name"]) and s["conclusion"] == "success"]
                     for label, pat in STEPS.items()}
            for label, matched in steps.items():
                if matched:
                    add(name, label, sum((ts(s["completed_at"]) - ts(s["started_at"])).total_seconds()
                                         for s in matched))
            if not (steps["rust-cache restore"] or steps["tests"]):
                continue
            lines = parse_log(gh("--allow-escape-sequences", f"repos/{REPO}/actions/jobs/{job['id']}/logs"))
            for s in steps["rust-cache restore"]:
                hit = cache_hit(window(lines, ts(s["started_at"]), ts(s["completed_at"])))
                if hit is not None:
                    add(name, "cache hit", int(hit))
            for s in steps["tests"]:
                compile_s, exec_s, count = read_test_step(lines, ts(s["started_at"]), ts(s["completed_at"]))
                add(name, "test compile", compile_s)
                add(name, "test execution", exec_s)
                add(name, "tests run", count)
    return wall, samples


def fmt(values):
    """median [min-max] (samples)."""
    return f"{statistics.median(values):.0f} [{min(values):.0f}-{max(values):.0f}] ({len(values)})"


def report(runs, wall, samples):
    """The report text."""
    first, last = runs[-1]["created_at"], runs[0]["created_at"]
    out = [f"{len(runs)} successful CI runs created {first} to {last}:",
           "  " + " ".join(str(r["id"]) for r in runs),
           "seconds, as median [min-max] (samples)",
           f"{'run wall-clock':<40}{fmt(wall)}"]
    for job, metrics in sorted(samples.items()):
        for metric in METRICS:
            if metric not in metrics:
                continue
            label = job if metric == "total" else f"  {metric}"
            line = f"{label:<40}{fmt(metrics[metric])}"
            if metric == "rust-cache restore" and "cache hit" in metrics:
                hits = metrics["cache hit"]
                line += f"  cache hit {sum(hits)}/{len(hits)}"
            out.append(line)
    return "\n".join(out)


def positive(value):
    """A --runs value."""
    if int(value) < 1:
        raise ValueError(value)
    return int(value)


def iso(value):
    """An --after/--before value: a typo fails here, not as an empty GitHub result."""
    dt.datetime.fromisoformat(value)
    return value


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--runs", type=positive, default=5, help="successful runs to take, newest first (default 5)")
    p.add_argument("--after", type=iso, help="only runs created at or after this ISO date/time")
    p.add_argument("--before", type=iso, help="only runs created at or before this ISO date/time")
    a = p.parse_args()
    runs = successful_runs(a.runs, a.after, a.before)
    if not runs:
        sys.exit("no successful CI runs in that window")
    if len(runs) < a.runs:
        print(f"only {len(runs)} successful runs found, not {a.runs}", file=sys.stderr)
    print(report(runs, *collect(runs)))


if __name__ == "__main__":
    main()
