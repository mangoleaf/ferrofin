#!/usr/bin/env python3
"""The adoption report: what a run checked, stage by stage, as one self-contained report.html.

gates/adoption/run.sh and `synthetic.py run` record what they do as they go, one JSON line per event in
WORK/results.jsonl, and build WORK/report.html from it when they end (however they end: a killed
run leaves its partial results). The page opens from disk, is uploaded as the CI artifact, and is
listed in the Adoption tab of the shared viewer (`gates/bench/report.py --serve --adoption-dir DIR`).

    adoption_report.py start  WORK --title T [--image I] [--oracle O]    begin a run (forgets the previous one)
    adoption_report.py begin  WORK FIXTURE KIND EXPECTED                 a fixture has started (unfinished if no row follows)
    adoption_report.py check  WORK FIXTURE STAGE CHECK PASS|FAIL [--detail D | --detail-stdin]
    adoption_report.py row    WORK FIXTURE KIND EXPECTED PASS|FAIL|SKIP [--detail D | --detail-stdin]
    adoption_report.py attach WORK FIXTURE KEY FILE [--text]             data for the page: JSON (shapes,
                                                                         scale) or text (table differences)
    adoption_report.py build  WORK                                       write WORK/report.html

Recording never changes a run's verdict: nothing here raises into a caller for an I/O or rendering
problem (it says so on stderr and goes on; run.sh sends that to /dev/null and notes a missing page
itself), and callers ignore this script's exit status. Stdlib only.
"""

import argparse
import html
import json
import sys
import time
from pathlib import Path

RESULTS = "results.jsonl"
REPORT = "report.html"
VERDICTS = ("PASS", "FAIL", "SKIP")
#: What a fixture row is: an upgrade path, a messy copy, the scale copy, or a preservation-matrix path.
KINDS = ("path", "messy", "scale", "matrix")
#: The table-digest differences of a stage embedded in the page: the first this many, then a count of
#: the rest, so a fixture that loses everything does not make a page of tens of megabytes. run.sh keeps
#: the full list in the `<fixture>.tables-*.log` beside the report; `synthetic.py run` prints it in the
#: error that ends the run.
MAX_DIFF_LINES = 500


def warn(message):
    print(f"adoption report: {message}", file=sys.stderr)


def capped(text, limit=MAX_DIFF_LINES):
    """`text` cut to its first `limit` lines, with a last line counting what was left out."""
    lines = text.splitlines()
    if len(lines) <= limit:
        return text
    return "\n".join(lines[:limit] + [f"… and {len(lines) - limit} more; all of them are in the table log beside the report (run.sh) or in the run's error (synthetic.py run)"])


def joined(items, limit=MAX_DIFF_LINES):
    """`items` as one line, "; "-joined, cut to the first `limit` with a count of the rest."""
    items = list(items)
    more = f"; … and {len(items) - limit} more" if len(items) > limit else ""
    return "; ".join(items[:limit]) + more


class Recorder:
    """Appends events for one run to WORK/results.jsonl. An I/O error is reported once and never raised."""

    def __init__(self, work):
        self.work = Path(work)
        self.path = self.work / RESULTS
        self.started = False
        self.where = None   # (fixture, stage) the run is in, for the caller's own error reporting
        self._warned = False

    def _write(self, event):
        try:
            self.work.mkdir(parents=True, exist_ok=True)
            with open(self.path, "a", encoding="utf-8") as output:
                output.write(json.dumps(event, sort_keys=True) + "\n")
        except OSError as error:
            if not self._warned:
                self._warned = True
                warn(f"cannot record ({error}); the run goes on without its report")

    def start(self, title, image="", oracle=""):
        try:
            self.work.mkdir(parents=True, exist_ok=True)
            self.path.write_text("", encoding="utf-8")
            (self.work / REPORT).unlink(missing_ok=True)
        except OSError as error:
            warn(f"cannot start a report ({error}); the run goes on without one")
        self.started = True
        self._write({"event": "run", "title": title, "image": image, "oracle": oracle, "started": time.time()})

    def begin(self, fixture, kind, expected):
        """A fixture has started: if the run ends before its row, the report shows it unfinished."""
        self._write({"event": "begin", "fixture": fixture, "kind": kind_of(kind), "expected": expected})

    def check(self, fixture, stage, check, verdict, detail=""):
        self._write({"event": "check", "fixture": fixture, "stage": stage, "check": check,
                     "verdict": verdict_of(verdict), "detail": detail})

    def row(self, fixture, kind, expected, verdict, detail=""):
        self.where = None   # the fixture is done: whatever stops the run next is not its stage's
        self._write({"event": "row", "fixture": fixture, "kind": kind_of(kind), "expected": expected,
                     "verdict": verdict_of(verdict), "detail": detail})

    def attach(self, fixture, key, data):
        if key.startswith("tables:") and isinstance(data, str):
            data = capped(data)
        self._write({"event": "attach", "fixture": fixture, "key": key, "data": data})

    def build(self):
        """WORK/report.html, or None when it could not be written (said on stderr)."""
        try:
            return build(self.work)
        except Exception as error:   # a rendering or I/O problem must not take the run's verdict with it
            warn(f"the report was not written ({type(error).__name__}: {error})")
            return None


def verdict_of(value):
    if value not in VERDICTS:
        raise ValueError(f"a verdict is one of {', '.join(VERDICTS)}, not {value!r}")
    return value


def kind_of(value):
    if value not in KINDS:
        raise ValueError(f"a row is one of {', '.join(KINDS)}, not {value!r}")
    return value


# ── reading a run back ──────────────────────────────────────────────────────
def well_formed(event):
    """Whether an event carries the fields its kind needs, of the right type (a hand-edited file may not)."""
    kind = event["event"]
    if not isinstance(kind, str):
        return False
    strings = {"check": ("stage", "check"), "attach": ("key",), "row": ("kind", "expected"), "begin": ("kind", "expected")}
    if not all(isinstance(event.get(key), str) for key in strings.get(kind, ())):
        return False
    if kind in ("check", "row") and event.get("verdict") not in VERDICTS:
        return False
    return kind in ("check", "row", "begin", "attach")


def load(work):
    """`(run, rows, skipped)`: the run's header, its fixtures in the order they appeared, and how many
    unreadable lines were ignored (a run killed mid-write leaves a truncated last line)."""
    run, rows, skipped = {}, {}, 0
    path = Path(work) / RESULTS
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines() if path.is_file() else []
    for line in lines:
        try:
            event = json.loads(line)
            kind = event["event"]
        except (ValueError, KeyError, TypeError):
            skipped += 1
            continue
        if kind == "run":
            run = event
            continue
        fixture = event.get("fixture")
        if not isinstance(fixture, str) or not well_formed(event):
            skipped += 1
            continue
        row = rows.setdefault(fixture, {"fixture": fixture, "kind": "", "expected": "", "verdict": None, "detail": "",
                                        "checks": [], "attachments": {}})
        if kind == "check":
            row["checks"].append(event)
        elif kind in ("row", "begin"):
            row.update({key: event[key] for key in ("kind", "expected", "verdict", "detail") if key in event})
        elif kind == "attach":
            row["attachments"][event["key"]] = event.get("data")
    return run, list(rows.values()), skipped


def row_verdict(row):
    """A row with no verdict of its own never finished; one whose checks failed is failed whatever it says."""
    if row["verdict"] is None:
        return "INCOMPLETE"
    if row["verdict"] == "PASS" and any(c["verdict"] == "FAIL" for c in row["checks"]):
        return "FAIL"
    return row["verdict"]


def overall(rows):
    verdicts = [row_verdict(row) for row in rows]
    if not verdicts:
        return "NOTHING RAN"
    if any(v in ("FAIL", "INCOMPLETE") for v in verdicts):
        return "FAIL"
    return "PASS" if "PASS" in verdicts else "NOTHING RAN"


# ── the page ────────────────────────────────────────────────────────────────
CSS = """
:root{color-scheme:light dark;--bg:#fff;--fg:#1c1f24;--mut:#5b6470;--line:#d6dae0;--bad:#b3261e;--ok:#1a7f37;--warn:#9a6700;--card:#f6f8fa}
@media (prefers-color-scheme:dark){:root{--bg:#14171c;--fg:#e6e9ee;--mut:#9aa4b2;--line:#2c323b;--bad:#ff8a80;--ok:#56d364;--warn:#e3b341;--card:#1b2027}}
body{background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif;margin:0}main{max-width:72rem;margin:0 auto;padding:1.5rem 1rem 4rem}
h1{font-size:1.5rem;margin:0 0 .25rem}h2{font-size:1.15rem;margin:2rem 0 .5rem}h3{font-size:1rem;margin:1.2rem 0 .3rem}code,pre{font:13px/1.45 ui-monospace,monospace}
pre{background:var(--card);border:1px solid var(--line);padding:.6rem;overflow:auto;max-height:28rem}
table{border-collapse:collapse;width:100%;margin:.5rem 0}th,td{border-bottom:1px solid var(--line);padding:.35rem .5rem;text-align:left;vertical-align:top;overflow-wrap:anywhere}
.v{display:inline-block;padding:.1rem .55rem;border-radius:.3rem;font-weight:600;border:1px solid currentColor}
.PASS{color:var(--ok)}.FAIL,.INCOMPLETE{color:var(--bad)}.SKIP,.NOTHING{color:var(--warn)}.mut{color:var(--mut)}
details{margin:.5rem 0;border:1px solid var(--line);border-radius:.3rem;padding:.3rem .7rem}summary{cursor:pointer;font-weight:600}
@media (max-width:40rem){table,thead,tbody,tr,th,td{display:block}thead{display:none}tr{border-bottom:1px solid var(--line);padding:.4rem 0}}
"""


def printable(text):
    """`text` as valid Unicode. Under a C locale Python decodes a non-ASCII command-line argument to surrogate
    characters; the original bytes are recovered, and anything that still cannot be written is replaced."""
    try:
        return text.encode("utf-8", "surrogateescape").decode("utf-8", "replace")
    except UnicodeEncodeError:
        return text.encode("utf-8", "replace").decode("utf-8")


def esc(value):
    return html.escape("" if value is None else printable(str(value)))


def badge(verdict):
    return f"<span class='v {esc(verdict.split()[0])}'>{esc(verdict)}</span>"


def table(headers, rows):
    head = "".join(f"<th>{esc(h)}</th>" for h in headers)
    body = "".join("<tr>" + "".join(f"<td>{cell}</td>" for cell in row) + "</tr>" for row in rows)
    return f"<table><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table>"


def format_bytes(count):
    for unit in ("B", "KiB", "MiB", "GiB"):
        if count < 1024 or unit == "GiB":
            return f"{count:.0f} {unit}" if unit == "B" else f"{count:.1f} {unit}"
        count /= 1024


def checks_html(checks):
    return table(["Stage", "Check", "Verdict", "Why"],
                 [[esc(c["stage"]), esc(c["check"]), badge(c["verdict"]), esc(c.get("detail", ""))] for c in checks])


def shapes_html(shapes):
    rows = [[f"<code>{esc(s['shape'])}</code>", badge(s["verdict"]), f"<code>{esc(json.dumps(s['got'], sort_keys=True))}</code>",
             f"<code>{esc(json.dumps(s['want'], sort_keys=True))}</code>", esc(s["source"])] for s in shapes]
    return "<h3>Messy shapes: Ferrofin's end state against Jellyfin 12.2</h3>" + table(
        ["Shape", "Verdict", "Ferrofin", "Expected", "Expected from"], rows)


def scale_html(result):
    seconds = lambda v: "not measured" if v is None else f"{v:.1f} s"
    size = lambda v: "not measured" if v is None else format_bytes(v)
    rows = [("Migration time", seconds(result.get("migration_seconds"))), ("Peak memory (cgroup, page cache included)", size(result.get("memory_bytes"))),
            ("Anonymous memory", size(result.get("anon_bytes"))), ("Disk high-water (WAL and .pre-* snapshots included)", size(result.get("disk_bytes"))),
            ("WAL peak", size(result.get("wal_bytes"))), ("Every-boot foreign_key_check", seconds(result.get("foreign_key_check_seconds")))]
    out = "<h3>Scale measurements (recorded, not gated)</h3>" + table(["Measure", "Value"], [[esc(k), esc(v)] for k, v in rows])
    missing = result.get("missing") or []
    return out + ("<p class='mut'>Not measured: " + esc("; ".join(missing)) + "</p>" if missing else "")


def section(render, *args):
    """One section's HTML; a section that cannot be rendered says so instead of taking the page down."""
    try:
        return render(*args)
    except Exception as error:
        return f"<p class='mut'>This section could not be rendered ({esc(type(error).__name__)}: {esc(error)}).</p>"


def row_html(row):
    verdict = row_verdict(row)
    parts = []
    if row["checks"]:
        parts.append(section(checks_html, row["checks"]))
    for key, text in sorted(row["attachments"].items()):
        if key.startswith("tables:"):
            parts.append(f"<h3>Table-digest differences, {esc(key.removeprefix('tables:'))}</h3><pre>{esc(text)}</pre>")
    if isinstance(row["attachments"].get("shapes"), list):
        parts.append(section(shapes_html, row["attachments"]["shapes"]))
    if isinstance(row["attachments"].get("scale"), dict):
        parts.append(section(scale_html, row["attachments"]["scale"]))
    if row["detail"] and not row["checks"]:   # a row with checks has its reasons in their table
        parts.append(f"<p class='mut'>{esc(row['detail'])}</p>")
    opened = " open" if verdict in ("FAIL", "INCOMPLETE") else ""
    return (f"<details{opened}><summary><code>{esc(row['fixture'])}</code> {badge(verdict)} "
            f"<span class='mut'>{esc(row['kind'])} {esc(row['expected'])}</span></summary>{''.join(parts) or '<p class=mut>No checks recorded.</p>'}</details>")


def render(run, rows, skipped=0, finished=None):
    verdict = overall(rows)
    counts = {v: sum(1 for r in rows if row_verdict(r) == v) for v in ("PASS", "FAIL", "INCOMPLETE", "SKIP")}
    title = run.get("title") or "Adoption run"
    seconds = (finished - run["started"]) if finished and run.get("started") else None
    facts = [("Image", run.get("image", "")), ("Jellyfin oracle", run.get("oracle", "")),
             ("Duration", f"{seconds:.0f} s" if seconds is not None else ""),
             ("Fixtures", ", ".join(f"{n} {v.lower()}" for v, n in counts.items() if n))]
    out = [f"<h1>{esc(title)}</h1>", f"<p>{badge(verdict)}</p>",
           table(["", ""], [[esc(k), esc(v)] for k, v in facts if v])]
    if skipped:
        out.append(f"<p class='mut'>{skipped} unreadable line(s) in {RESULTS} were ignored.</p>")
    out.append("<h2>Fixtures</h2>")
    out.append(table(["Fixture", "Kind", "Generation", "Verdict", "Detail"],
                     [[f"<code>{esc(r['fixture'])}</code>", esc(r["kind"]), esc(r["expected"]), badge(row_verdict(r)), esc(r["detail"])] for r in rows])
               if rows else "<p class='mut'>No fixture was recorded.</p>")
    out.append("<h2>Stages and checks</h2>")
    out.extend(row_html(r) for r in rows)
    return ("<!doctype html><html lang='en'><head><meta charset='utf-8'><meta name='viewport' content='width=device-width,initial-scale=1'>"
            f"<title>{esc(title)}</title><style>{CSS}</style></head><body><main>{''.join(out)}</main></body></html>")


def build(work):
    run, rows, skipped = load(work)
    page = render(run, rows, skipped, finished=time.time())
    path = Path(work) / REPORT
    path.write_text(page, encoding="utf-8")
    return path


# ── command line ────────────────────────────────────────────────────────────
def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="mode", required=True)

    def detail(p):
        p.add_argument("--detail", default="", help="why (a value that starts with '-' goes as --detail=VALUE)")
        p.add_argument("--detail-stdin", action="store_true", help="read the detail from standard input (no size limit)")

    start = sub.add_parser("start")
    start.add_argument("work", type=Path)
    start.add_argument("--title", required=True)
    start.add_argument("--image", default="")
    start.add_argument("--oracle", default="")
    begin = sub.add_parser("begin")
    begin.add_argument("work", type=Path)
    for name in ("fixture", "kind", "expected"):
        begin.add_argument(name)
    check = sub.add_parser("check")
    check.add_argument("work", type=Path)
    for name in ("fixture", "stage", "check", "verdict"):
        check.add_argument(name)
    detail(check)
    row = sub.add_parser("row")
    row.add_argument("work", type=Path)
    for name in ("fixture", "kind", "expected", "verdict"):
        row.add_argument(name)
    detail(row)
    attach = sub.add_parser("attach")
    attach.add_argument("work", type=Path)
    attach.add_argument("fixture")
    attach.add_argument("key")
    attach.add_argument("file", type=Path)
    attach.add_argument("--text", action="store_true", help="the file is plain text, not JSON")
    sub.add_parser("build").add_argument("work", type=Path)
    args = parser.parse_args(argv)
    try:
        recorder = Recorder(args.work)
        why = (sys.stdin.buffer.read().decode("utf-8", "replace") if getattr(args, "detail_stdin", False)
               else printable(getattr(args, "detail", "")))
        if args.mode == "start":
            recorder.start(args.title, args.image, args.oracle)
        elif args.mode == "begin":
            recorder.begin(args.fixture, args.kind, args.expected)
        elif args.mode == "check":
            recorder.check(args.fixture, args.stage, args.check, args.verdict, why)
        elif args.mode == "row":
            recorder.row(args.fixture, args.kind, args.expected, args.verdict, why)
        elif args.mode == "attach":
            text = args.file.read_text(encoding="utf-8", errors="replace")
            recorder.attach(args.fixture, args.key, text if args.text else json.loads(text))
        else:
            path = recorder.build()
            if path is None:
                return 1
            print(path)
        return 0
    except (OSError, ValueError) as error:
        print(f"adoption report: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
