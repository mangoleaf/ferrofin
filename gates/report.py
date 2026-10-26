#!/usr/bin/env python3
"""The combined report of one gates session (stdlib only): SESSION/index.html and SESSION/report.md,
from SESSION/session.json and what each gate left in its own folder. It reads those files and runs
nothing. gates/run writes it whenever a session ends, and it can be written again for any session.

    report.py SESSION

Every gate gets a row: its status, its verdict, the counts that matter and links to its own report and
log. A gate the session did not ask for, did not reach or start, is still running, ended before writing
its result or its report, or left a result that cannot be read is shown as such, never left out. A gate's
verdict comes from that gate's own code (adoption_report.py, diff.py, gate.py's gated cells), not a copy
of its rules. The page opens from disk: every link is relative.
"""

import html
import importlib.util
import json
import os
import sys
from collections import Counter

GATES = ("verify", "adoption", "parity", "bench")
HERE = os.path.dirname(os.path.abspath(__file__))


class Unreadable(Exception):
    """A result file that exists but cannot be read."""


def gate_module(path):
    """A gate's own module, loaded by its path under gates/ (several are named `report`), with its
    directory on sys.path for its own imports; loaded once."""
    name = "gates_" + path.replace("/", "_").removesuffix(".py")
    if name not in sys.modules:
        full = os.path.join(HERE, path)
        if os.path.dirname(full) not in sys.path:
            sys.path.insert(0, os.path.dirname(full))
        spec = importlib.util.spec_from_file_location(name, full)
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        try:
            spec.loader.exec_module(module)
        except BaseException:
            del sys.modules[name]
            raise
    return sys.modules[name]


def read_json(path):
    """The parsed file, None when it does not exist; Unreadable when it is not JSON."""
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as error:
        raise Unreadable(f"{os.path.basename(path)} cannot be read ({error})") from error


def found(verdict=None, counts=(), problems=(), bad=False):
    """What a reader found in a gate's folder: its verdict, counts, the problems worth showing, and
    whether the result itself says the gate failed (the row's colour follows that too)."""
    return {"verdict": verdict, "counts": list(counts), "problem": "; ".join(p for p in problems if p) or None, "bad": bad}


def link(session, path, label):
    """A relative link to `path` (inside the session) when it exists, else None."""
    return (label, os.path.relpath(path, session)) if os.path.exists(path) else None


def plural(n, word):
    return f"{n} {word}{'' if n == 1 else 's'}"


def gate_result(d, gate):
    """The gate's result.json (gates/result.py), None when there is none; Unreadable when it is invalid."""
    try:
        return gate_module("result.py").read(d, gate)
    except ValueError as error:
        raise Unreadable(str(error)) from error


def verify(d, record):
    result = gate_result(d, "verify")
    if result is None and os.path.exists(os.path.join(d, "verify.json")):
        return found(problems=["no result.json: a run from before verify wrote one (python3 gates/verify/verify_report.py "
                               "DIR --image IMAGE writes it from verify.json)"], bad=True)
    if result is None:
        return found(problems=["no result.json: the gate ended before writing its result"], bad=True)
    counts, detail = result["counts"], result["detail"]
    missing = [kind for kind, rows in (detail.get("libraries") or {}).items() if rows is None]
    problem = None
    if missing:
        libraries = " and ".join(missing) + (" libraries" if len(missing) > 1 else " library")
        problem = f"no rows from the {libraries}: the check ended before them (the run's exit {detail.get('exit')}, see the log)"
    verdict = f"{counts.get('fail', 0)} failed" if result["verdict"] == "failed" else result["verdict"]
    return found(verdict, [("checks passed", counts.get("pass", 0)), ("failed", counts.get("fail", 0)),
                           ("skipped", counts.get("skip", 0))], [problem], bad=result["verdict"] != "passed")


def adoption(d, record):
    if not os.path.exists(os.path.join(d, "results.jsonl")):
        return found(problems=["no results.jsonl: the gate ended before it recorded anything"], bad=True)
    module = gate_module("adoption/adoption_report.py")
    _, rows, skipped = module.load(d)   # adoption's own reading: a fixture begun but never finished is INCOMPLETE
    verdicts = Counter(module.row_verdict(row) for row in rows)
    overall = module.overall(rows)
    return found({"PASS": "passed", "FAIL": "failed"}.get(overall, "nothing ran"),
                 [("rows passed", verdicts["PASS"]), ("failed", verdicts["FAIL"]), ("unfinished", verdicts["INCOMPLETE"]),
                  ("skipped", verdicts["SKIP"])],
                 [skipped and f"{plural(skipped, 'unreadable line')} in results.jsonl skipped"], bad=overall != "PASS")


def parity(d, record):
    results = read_json(os.path.join(d, "results.json"))
    if results is None:
        return found(problems=["no results.json: the gate ended before its comparison finished"], bad=True)
    new = read_json(os.path.join(d, "new-differences.json"))
    differences = [diff for r in (new or {}).get("requests", []) for diff in r.get("differences", [])]
    tag_path = gate_module("parity/diff.py").tag_path   # the groups `diff.py findings` files issues by
    unfiled = {(tag_path(diff.get("path", "")), diff.get("kind")) for diff in differences if not diff.get("filed")}
    verdict = results.get("verdict", "?")
    return found(verdict + (f": {results['reason']}" if results.get("reason") else ""),
                 [("new differences", len(differences)), ("groups not filed", len(unfiled)),
                  ("requests sent", len(results.get("requests") or [])), ("planned", results.get("planned", 0))],
                 [new is None and "no new-differences.json: its counts are unknown"], bad=verdict != "pass")


def bench(d, record):
    result = gate_result(d, "bench")
    if result is None and os.path.exists(os.path.join(d, "verdict.json" if record.get("previous") else "report.md")):
        how = "gates/bench/gate.py render DIR" if record.get("previous") else "gates/bench/report.py --html DIR"
        return found(problems=[f"no result.json: a run from before the benchmark wrote one (python3 {how} writes it)"],
                     bad=True)
    if record.get("previous"):   # the release A/B gate (gates/run --previous): a gate.py session
        if result is None:
            return found(problems=["no result.json: the A/B gate ended before it judged its runs"], bad=True)
        counts, blocking = result["counts"], result["detail"].get("blocking") or []
        return found(f"blocked by {plural(len(blocking), 'cell')}" if blocking else "nothing blocks",
                     [("gated cells regressed", counts.get("regressed", 0)), ("not comparable", counts.get("not comparable", 0)),
                      ("improved", counts.get("improved", 0)), ("within noise", counts.get("within noise", 0))],
                     bad=bool(blocking))
    if result is None:
        return found(problems=["no result.json: the run ended before writing its report and result"], bad=True)
    counts = result["counts"]
    # gates/run measures both servers: one with no results at all never started
    absent = [s for s in ("ferrofin", "jellyfin") if s not in (result["detail"].get("servers") or {})]
    verdict = (f"{plural(counts.get('failed', 0), 'phase')} failed" if result["verdict"] == "failed"
               else "incomplete" if result["verdict"] == "incomplete" or absent else result["verdict"])
    return found(verdict, [("phases completed", counts.get("completed", 0)), ("failed", counts.get("failed", 0)),
                           ("unfinished", counts.get("unfinished", 0)), ("skipped", counts.get("skipped", 0))],
                 [absent and f"no results for {' or '.join(absent)}: its run never started"],
                 bad=verdict != "measured")


READERS = {"verify": verify, "adoption": adoption, "parity": parity, "bench": bench}
#: Each gate's own pages in its folder, linked whenever they exist, whatever its result says. The first
#: page is the gate's report: a gate that ended without one says so. The rest are its raw files.
OWN_PAGES = {"verify": (("report.html", "verify report"), ("scan-behaviour-movies.txt", "movies rows"),
                        ("scan-behaviour-tvshows.txt", "tvshows rows"), ("result.json", "result.json"),
                        ("verify.json", "verify.json"), ("server.log", "server log")),
             "adoption": (("report.html", "adoption report"),),
             "parity": (("report.html", "parity report"),),
             "bench": (("report.html", "benchmark report"),)}


def summarise(session):
    """`(record, rows)`: session.json, and per gate (every one) its status, exit, note, verdict, counts,
    links, problem and whether its result says it failed."""
    try:
        record = read_json(os.path.join(session, "session.json"))
    except Unreadable as error:
        raise SystemExit(f"report: {session}: {error}") from error
    if not isinstance(record, dict):
        raise SystemExit(f"report: {session} has no session.json, or not one gates/session.py wrote")
    rows = []
    for gate in GATES:   # every gate, the ones this session did not ask for too
        entry = record.get("gates", {}).get(gate) or {"status": "not requested"}
        status = entry.get("status", "?")
        row = {"gate": gate, "status": status, "exit": entry.get("exit"), "note": entry.get("note"),
               "verdict": None, "counts": [], "links": [], "problem": None, "bad": False}
        d = os.path.join(session, gate)
        if status in ("pending", "skipped", "not requested"):
            row["verdict"] = "not run"
        elif status == "running":
            row["verdict"] = "running"
        elif not entry.get("started"):   # failed before it started (an image retagged under the session)
            row["verdict"] = "not started"
        else:
            try:
                row.update(READERS[gate](d, record))
            except Unreadable as error:
                row["problem"] = str(error)
            except Exception as error:  # noqa: BLE001 — a result shaped unlike the reader expects
                row["problem"] = f"its result is not shaped as expected ({type(error).__name__}: {error})"
            pages = OWN_PAGES[gate]
            if not os.path.exists(os.path.join(d, pages[0][0])):
                row["problem"] = "; ".join(filter(None, [row["problem"],
                                                          f"no {pages[0][0]}: the gate ended before writing its report"]))
        label = {"bench": "A/B gate report"} if record.get("previous") else {}
        row["links"] = [x for x in (link(session, os.path.join(d, name), label.get(gate, text))
                                    for name, text in OWN_PAGES[gate]) if x]
        log = link(session, os.path.join(session, f"{gate}.log"), "log")
        if log:
            row["links"].append(log)
        rows.append(row)
    return record, rows


def short_id(image_id):
    """An image id with its digest cut to the 12 hex characters docker's short ids show."""
    algorithm, _, digest = (image_id or "").partition(":")
    return f"{algorithm}:{digest[:12]}" if digest else algorithm


def facts(record):
    image = record.get("image") or {}
    lines = [("branch", record.get("branch")),
             ("commit", (record.get("commit") or "")[:12] + (" (uncommitted changes)" if record.get("dirty") else "")),
             ("image", f"{image.get('ref')} ({short_id(image.get('id'))})"),
             ("Jellyfin oracle", (record.get("oracle") or {}).get("image")),
             ("started", record.get("started")), ("ended", record.get("ended") or "still running")]
    if record.get("previous"):
        lines.insert(3, ("previous release", record["previous"].get("ref")))
    return lines


def status_word(row):
    word = row["status"]
    if row["exit"] is not None:
        word += f" (exit {row['exit']})"
    return word + (f", {row['note']}" if row.get("note") else "")


def md_cell(text):
    """A Markdown table cell: no pipe or line break may end the cell early."""
    return str(text).replace("|", "\\|").replace("\n", " ")


def render_md(record, rows):
    out = [f"# Gates session {record.get('name')}", ""]
    out += [f"- **{k}:** {md_cell(v)}" for k, v in facts(record)]
    out += ["", "| Gate | Status | Verdict | Counts | Report |", "|---|---|---|---|---|"]
    for r in rows:
        counts = ", ".join(f"{label} {n}" for label, n in r["counts"])
        links = ", ".join(f"[{label}]({href})" for label, href in r["links"])
        verdict = (r["verdict"] or "no result") + (f" — {r['problem']}" if r["problem"] else "")
        out.append("| " + " | ".join(md_cell(c) for c in (r["gate"], status_word(r), verdict, counts)) + f" | {links} |")
    return "\n".join(out) + "\n"


#: Used when the benchmark reports' stylesheet cannot be loaded: the page is written whatever happens.
FALLBACK_CSS = (":root{--ink:#1A1E24;--muted:#5B636E;--good:#2E7D4F;--bad:#A83A2E}"
                "@media (prefers-color-scheme:dark){:root{--ink:#E7EAEE;--muted:#9AA3AE;--good:#6CC08B;--bad:#E07A6E}"
                "body{background:#13161B}}body{color:var(--ink);font:15px/1.55 system-ui,sans-serif;margin:2rem}")


def css():
    """The benchmark reports' stylesheet (gates/bench/report.py), so this page reads like them."""
    try:
        return gate_module("bench/report.py").CSS
    except Exception:  # noqa: BLE001 — the report is written however the session ended
        return FALLBACK_CSS


def bench_graphs(session, record):
    """The README's graphs for the session's benchmark run (gates/publish.py), inline; an empty string
    for an A/B gate session or a run with no comparable result, and a note when they cannot be drawn."""
    if record.get("previous") or (record.get("gates", {}).get("bench") or {}).get("status") in (None, "pending", "skipped"):
        return ""
    try:
        doc = gate_result(os.path.join(session, "bench"), "bench")
        if not doc or "jellyfin" not in (doc["detail"].get("servers") or {}):
            return ""
        graphs = gate_module("publish.py").session_graphs(doc)
    except Exception as error:  # noqa: BLE001 — the page is written whatever the result holds
        return f"<p class='problem'>The benchmark graphs could not be drawn ({html.escape(type(error).__name__)}: {html.escape(str(error))}).</p>"
    return ("<h2>Benchmark against Jellyfin</h2><p class='lede'>The README's graphs for this session's one run "
            f"(the published ones are the median of three).</p>{graphs}") if graphs else ""


def render_html(record, rows, graphs=""):
    e = html.escape
    name = e(str(record.get("name")))
    head = "".join(f"<dt>{e(k)}</dt><dd>{e(str(v))}</dd>" for k, v in facts(record))
    body = []
    for r in rows:
        tone = "bad" if r["status"] == "failed" or r["problem"] or r["bad"] else "good" if r["status"] == "passed" else ""
        verdict = e(r["verdict"] or "no result") + (f"<br><span class='problem'>{e(r['problem'])}</span>" if r["problem"] else "")
        counts = "<br>".join(f"{e(label)}: {n}" for label, n in r["counts"])
        anchors = [f"<a href='{e(href)}'>{e(label)}</a>" for label, href in r["links"]]
        if r["links"] and r["links"][0][1].endswith("/report.html"):   # the gate's report leads, its files follow
            anchors[0] = f"<b>{anchors[0]}</b>" + ("<br><span class='meta'>" if len(anchors) > 1 else "")
            links = anchors[0] + " · ".join(anchors[1:]) + ("</span>" if len(anchors) > 1 else "")
        else:
            links = " · ".join(anchors)
        body.append(f"<tr class='{tone}'><th>{e(r['gate'])}</th><td>{e(status_word(r))}</td><td>{verdict}</td>"
                    f"<td>{counts}</td><td>{links}</td></tr>")
    return (f"<!doctype html><html lang='en'><head><meta charset='utf-8'>"
            f"<meta name='viewport' content='width=device-width, initial-scale=1'><title>Gates {name}</title>"
            f"<style>{css()}dl.facts{{display:grid;grid-template-columns:max-content 1fr;gap:4px 16px}}"
            f"dl.facts dt{{color:var(--muted)}}dl.facts dd{{margin:0}}tr.bad th,.problem{{color:var(--bad)}}"
            f"tr.good th{{color:var(--good)}}a{{color:var(--accent-ink)}}table.gates td,table.gates th{{text-align:left;vertical-align:top;"
            f"padding:6px 12px}}.graph svg{{max-width:100%;height:auto;margin:14px 0}}.graph.dark{{display:none}}"
            f"@media (prefers-color-scheme:dark){{.graph.light{{display:none}}.graph.dark{{display:block}}}}"
            f"</style></head><body><main>"
            f"<h1>Gates session {name}</h1><dl class='facts'>{head}</dl>"
            f"<table class='gates'><thead><tr><th>Gate</th><th>Status</th><th>Verdict</th><th>Counts</th>"
            f"<th>Report</th></tr></thead><tbody>{''.join(body)}</tbody></table>{graphs}</main></body></html>")


def write(session):
    record, rows = summarise(session)
    for name, text in (("index.html", render_html(record, rows, bench_graphs(session, record))),
                       ("report.md", render_md(record, rows))):
        path = os.path.join(session, name)
        with open(path + ".tmp", "w", encoding="utf-8") as f:
            f.write(text)
        os.replace(path + ".tmp", path)
    return os.path.join(session, "index.html")


def main(argv):
    if len(argv) != 1:
        sys.exit(__doc__)
    print(write(argv[0]))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
