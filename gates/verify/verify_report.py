#!/usr/bin/env python3
"""verify's result.json and report.html (stdlib only), from what gates/verify/run.sh leaves in its run
folder. run.sh writes both however its run ends, once its server log is saved.

    verify_report.py RUN --image IMAGE [--image-id ID] [--commit SHA] [--started TIME]
        RUN/result.json (gates/result.py's envelope) from RUN/verify.json, then RUN/report.html from it

The page shows, for each library, every row with what it checks, PASS, FAIL or SKIP, and each check's
observed and expected value; a failed row opens on its checks and links the server log. A library the
check never reached is shown as such, never left out. The page opens from disk: no external assets.
"""

import argparse
import html
import importlib.util
import json
import os
import sys
from datetime import datetime

GATES = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if GATES not in sys.path:
    sys.path.insert(0, GATES)
import result  # noqa: E402

LIBRARIES = ("movies", "tvshows")
SIGNS = {"eq": "=", "le": "≤", "ge": "≥"}


def now():
    return datetime.now().astimezone().isoformat(timespec="seconds")


def tally(libraries):
    """`(verdict, counts)` over every library's rows: failed when a row failed, nothing verified when no
    row ran, incomplete when a library has no rows (the check ended before it), else passed."""
    counts = {"pass": 0, "fail": 0, "skip": 0}
    for rows in libraries.values():
        for row in (rows or {}).get("rows", []):
            counts[{"PASS": "pass", "FAIL": "fail"}.get(row.get("result"), "skip")] += 1
    missing = [kind for kind, rows in libraries.items() if rows is None]
    verdict = ("failed" if counts["fail"] else "nothing verified" if not sum(counts.values())
               else "incomplete" if missing else "passed")
    return verdict, counts


def build(run, image, image_id=None, commit=None, started=None, ended=None):
    """verify's envelope from RUN/verify.json (every library null when there is none)."""
    try:
        with open(os.path.join(run, "verify.json"), encoding="utf-8") as f:
            raw = json.load(f)
    except FileNotFoundError:
        raw = {"exit": None, "libraries": dict.fromkeys(LIBRARIES)}
    libraries = {}
    for kind, rows in (raw.get("libraries") or {}).items():
        libraries[kind] = None if rows is None else {
            "rows": [{**row, "checks": row.get("checks") or []} for row in rows.get("rows", [])],
            "summary": rows.get("summary") or {}}
    verdict, counts = tally(libraries)
    source = {"commit": commit or None, "image": image, "image_digest": image_id or None, "oracle": None,
              "host_class": result.host_class(), "fixture": None}
    return result.envelope("verify", verdict, counts, source, started or None, ended or now(),
                           {"exit": raw.get("exit"), "libraries": libraries})


def css():
    """The benchmark reports' stylesheet, as the session page loads it (gates/report.py), so this page reads like them."""
    spec = importlib.util.spec_from_file_location("gates_session_report", os.path.join(GATES, "report.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.css()


PAGE_CSS = ("span.chip{display:inline-block;min-width:3.2em;text-align:center;font:600 11.5px/1.6 ui-monospace,monospace;"
            "border-radius:4px;border:1px solid currentColor}span.chip.pass{color:var(--good)}span.chip.fail{color:var(--bad)}"
            "span.chip.skip{color:var(--muted)}tr.fail th{color:var(--bad)}dl.facts{display:grid;"
            "grid-template-columns:max-content 1fr;gap:4px 16px}dl.facts dt{color:var(--muted)}dl.facts dd{margin:0}"
            "table.checks{margin-top:6px}table.checks td.bad{color:var(--bad);font-weight:600}.problem{color:var(--bad)}")


def value(v):
    return "—" if v is None else json.dumps(v) if isinstance(v, str) else str(v)


def checks_html(checks):
    e = html.escape
    if not checks:
        return "<p class='lede'>No checks recorded for this row.</p>"
    rows = "".join(
        f"<tr><td>{e(c.get('what', ''))}</td><td class='num{'' if c.get('ok') else ' bad'}'>{e(value(c.get('observed')))}</td>"
        f"<td class='num'>{e('' if c.get('op') is None else SIGNS.get(c['op'], c['op']) + ' ' + value(c.get('expected')))}</td>"
        f"<td>{'ok' if c.get('ok') else 'failed'}</td></tr>" for c in checks)
    return (f"<table class='checks'><thead><tr><th>Check</th><th>Observed</th><th>Wanted</th><th></th></tr></thead>"
            f"<tbody>{rows}</tbody></table>")


LOG_LINK = "<a href='server.log'>server log</a>"


def library_html(kind, rows, exit_code, log):
    e = html.escape
    out = [f"<h2>The {e(kind)} library</h2>"]
    if rows is None:
        return "".join(out) + (f"<p class='problem'>Did not run: the check ended before this library's rows (the run's exit "
                               f"{e(str(exit_code))}){'; see the ' + LOG_LINK if log else ''}.</p>")
    _, counts = tally({kind: rows})
    out.append(f"<p class='meta'>{counts['pass']} PASS · {counts['fail']} FAIL · {counts['skip']} SKIP</p>")
    body = []
    for row in rows.get("rows", []):
        word = row.get("result", "?")
        tone = {"PASS": "pass", "FAIL": "fail"}.get(word, "skip")
        failed = tone == "fail"
        extra = f" {LOG_LINK}" if failed and log else ""
        checks = (f"<details{' open' if failed else ''}><summary>{len(row.get('checks', []))} check{'' if len(row.get('checks', [])) == 1 else 's'}</summary>"
                  f"{checks_html(row.get('checks', []))}</details>") if tone != "skip" else ""
        body.append(f"<tr class='{tone}'><th>{e(str(row.get('row', '')))}</th><td><span class='chip {tone}'>{e(word)}</span></td>"
                    f"<td>{e(row.get('scenario', ''))}</td><td><code>{e(row.get('detail', ''))}</code>{extra}{checks}</td></tr>")
    out.append("<div class='scroll'><table><thead><tr><th>Row</th><th>Result</th><th>What it checks</th><th>Observed</th>"
               f"</tr></thead><tbody>{''.join(body)}</tbody></table></div>")
    return "".join(out)


def render(doc, log=True, style=None):
    """report.html from verify's result. `log`: whether server.log sits beside the page, to link it."""
    e = html.escape
    source, detail = doc["source"], doc["detail"]
    host = source.get("host_class") or {}
    facts = [("verdict", doc["verdict"]), ("image", source.get("image")), ("image id", source.get("image_digest")),
             ("commit", source.get("commit")), ("host", f"{host.get('cores')} cores, {host.get('memory_gib')} GiB"),
             ("started", doc.get("started")), ("ended", doc.get("ended")), ("exit", detail.get("exit"))]
    head = "".join(f"<dt>{e(k)}</dt><dd>{e(str(v if v is not None else '—'))}</dd>" for k, v in facts)
    libraries = "".join(library_html(kind, rows, detail.get("exit"), log)
                        for kind, rows in (detail.get("libraries") or {}).items())
    return (f"<!doctype html><html lang='en'><head><meta charset='utf-8'>"
            f"<meta name='viewport' content='width=device-width, initial-scale=1'><title>Verify {e(doc['verdict'])}</title>"
            f"<style>{style if style is not None else css()}{PAGE_CSS}</style></head><body><main>"
            f"<h1>Verify: {e(doc['verdict'])}</h1><p class='lede'>The scan-behaviour check (gates/verify/README.md) on a "
            f"fresh server: each row times a scan through /metrics and checks what it created, updated, probed and asked "
            f"the providers.</p><dl class='facts'>{head}</dl>"
            + (f"<p>{LOG_LINK}</p>" if log else "") + f"{libraries}</main></body></html>")


def write_report(run, doc):
    """RUN/report.html from `doc`; its path."""
    page = render(doc, log=os.path.exists(os.path.join(run, "server.log")))
    path = os.path.join(run, "report.html")
    with open(path + ".tmp", "w", encoding="utf-8") as f:
        f.write(page)
    os.replace(path + ".tmp", path)
    return path


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("run")
    parser.add_argument("--image", required=True)
    parser.add_argument("--image-id")
    parser.add_argument("--commit")
    parser.add_argument("--started")
    args = parser.parse_args(argv)
    doc = build(args.run, args.image, args.image_id, args.commit, args.started)
    result.write(args.run, doc)
    print(write_report(args.run, doc))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
