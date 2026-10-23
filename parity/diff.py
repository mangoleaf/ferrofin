#!/usr/bin/env python3
"""The parity harness: send requests to Jellyfin and Ferrofin, compare, match the ledger, report.

    diff.py run --oracle-url U --ferrofin-url U --ids IDS.json --requests FILE.jsonl --out DIR
                [--normalise FILE] [--ledger FILE] [--request-timeout S] [--deadline EPOCH]
        send every request to both servers, compare the normalised bodies on values, JSON types
        and nulls, classify each difference as ledgered or new, and write DIR/results.json and
        DIR/report.html. Exit 0 when nothing is new, 1 on a new difference or a run failure
        (the cap passed), 2 when an input is invalid.
    diff.py check [--requests FILE] [--ledger FILE] [--normalise FILE] [--ids IDS.json]
        validate the input files, and with --ids that every request template resolves (the runner
        does this before it boots anything)
    diff.py login --url U --ids IDS.json
        print an access token for the corpus's browse user
    diff.py failure --out DIR --reason TEXT [--cap-exceeded]
        write the report of a run that could not complete (a server did not boot, the cap passed)

A run that completes always produces a report. Python and the standard library only.

Request file: JSON lines, one object per request: `id` (unique), `method`, `path` (a template,
`{name}` is read from the corpus's ids.json, `{views.movies}` reaches into an object), `query`
(object), `body` (object or null), `source` (where jellyfin-web issues it), `oracle_trap` (why the
oracle is known to stall or fail on it), `expect_status` (the status the oracle answers, default 200:
two servers that agree on an error are not a pass unless the request says an error is expected). Only GET is sent for now: writes need a database copy
each, which the write phase adds.

Normalisation file (normalise.toml) and ledger (ledger.toml) are described in parity/README.md.
"""

import argparse
import contextlib
import fnmatch
import html
import http.client
import json
import re
import sys
import threading
import time
import tomllib
import urllib.parse
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
VERIFIED_SOURCE = "crates/ferrofin-api/src/handlers/mod.rs"
#: The per-request timeout defaults to 30 s and may be set from 10 to 120 s (run.sh holds the same range).
REQUEST_TIMEOUT_DEFAULT_S = 30
REQUEST_TIMEOUT_RANGE_S = (10, 120)
#: Each difference is one of these.
KINDS = ("status", "value", "type", "null", "missing", "extra", "length")
RULE_KINDS = ("opaque", "opaque-keys", "unordered")
METHODS = ("GET",)
PLACEHOLDER = re.compile(r"\{([A-Za-z_][\w.]*)\}")


class ParityError(Exception):
    """An input file or argument is invalid; the run cannot start."""


# ── requests ────────────────────────────────────────────────────────────────
@dataclass(frozen=True)
class Request:
    id: str
    method: str
    path: str
    query: dict
    body: object
    source: str
    oracle_trap: str
    expect_status: int


def lookup(ids, name):
    """`name` ("user", "views.movies") read from the ids file; a string or integer."""
    value = ids
    for part in name.split("."):
        if not isinstance(value, dict) or part not in value:
            raise ParityError(f"the ids file has no `{name}`")
        value = value[part]
    if isinstance(value, bool) or not isinstance(value, (str, int)):
        raise ParityError(f"`{name}` in the ids file is not a string or an integer")
    return str(value)


def fill(text, ids):
    return PLACEHOLDER.sub(lambda found: lookup(ids, found.group(1)), text)


def load_requests(path, ids=None):
    """The requests in `path`; with `ids`, every template must also resolve against it."""
    requests, seen = [], set()
    for number, line in enumerate(Path(path).read_text().splitlines(), 1):
        if not line.strip():
            continue
        where = f"{path}:{number}"
        try:
            row = json.loads(line)
        except json.JSONDecodeError as err:
            raise ParityError(f"{where}: not JSON ({err})") from err
        if not isinstance(row, dict):
            raise ParityError(f"{where}: a request is a JSON object")
        unknown = set(row) - {"id", "method", "path", "query", "body", "source", "oracle_trap", "expect_status"}
        if unknown:
            raise ParityError(f"{where}: unknown field(s) {', '.join(sorted(unknown))}")
        for key in ("id", "method", "path"):
            if not isinstance(row.get(key), str) or not row[key]:
                raise ParityError(f"{where}: `{key}` is a non-empty string")
        if row["id"] in seen:
            raise ParityError(f"{where}: request id `{row['id']}` is used twice")
        seen.add(row["id"])
        if row["method"] not in METHODS:
            raise ParityError(f"{where}: `{row['method']}` is not sent yet (only {', '.join(METHODS)}: "
                              "writes need a fresh database copy each)")
        if not row["path"].startswith("/"):
            raise ParityError(f"{where}: `path` starts with a slash")
        query = row.get("query") or {}
        if not isinstance(query, dict):
            raise ParityError(f"{where}: `query` is an object")
        if row.get("body") is not None:
            raise ParityError(f"{where}: a GET carries no body")
        expect = row.get("expect_status", 200)
        if isinstance(expect, bool) or not isinstance(expect, int) or not 100 <= expect <= 599:
            raise ParityError(f"{where}: `expect_status` is an HTTP status code")
        requests.append(Request(row["id"], row["method"], row["path"], query, None,
                                row.get("source", ""), row.get("oracle_trap", ""), expect))
    if not requests:
        raise ParityError(f"{path}: no requests")
    for request in requests if ids is not None else ():
        target(request, ids)
    return requests


def query_value(value, ids):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, list):
        return ",".join(query_value(item, ids) for item in value)
    return fill(str(value), ids)


def target(request, ids):
    """The request line's path and query, templates filled from `ids`."""
    path = urllib.parse.quote(fill(request.path, ids), safe="/%")
    if request.query:
        pairs = [(key, query_value(value, ids)) for key, value in request.query.items()]
        path += "?" + urllib.parse.urlencode(pairs, quote_via=urllib.parse.quote, safe=",")
    return path


# ── talking to a server ─────────────────────────────────────────────────────
@dataclass
class Response:
    status: "int | None"      # None: no answer
    body: bytes
    seconds: float
    error: str = ""           # "timeout" or "connection: …" when status is None

    def brief(self):
        return {"status": self.status, "seconds": round(self.seconds, 3), "error": self.error}


def fetch(base, method, path, headers, timeout, now=time.monotonic, body=None):
    """One request, bounded to `timeout` seconds in total. The request runs in a worker the caller
    stops waiting for at the deadline, so a server that trickles its answer, in the headers or the
    body, is cut off like one that stays silent (a socket timeout alone applies to each read)."""
    parts = urllib.parse.urlsplit(base)
    start = now()
    connection = http.client.HTTPConnection(parts.hostname, parts.port, timeout=timeout)
    box = []

    def work():
        try:
            connection.request(method, path, body=body, headers=headers)
            reply = connection.getresponse()
            box.append(Response(reply.status, reply.read(), now() - start))
        except TimeoutError:   # socket.timeout is TimeoutError
            box.append(Response(None, b"", now() - start, "timeout"))
        except Exception as err:   # fetch always returns a Response: an unusable request line is a connection failure
            box.append(Response(None, b"", now() - start, f"connection: {err}"))

    worker = threading.Thread(target=work, daemon=True)
    worker.start()
    worker.join(timeout)
    if worker.is_alive():
        with contextlib.suppress(OSError):
            connection.close()   # frees the abandoned worker's socket
        return Response(None, b"", now() - start, "timeout")
    connection.close()
    return box[0]


def auth_header(token="", device="parity"):
    value = f'MediaBrowser Client="parity", Device="parity", DeviceId="{device}", Version="1"'
    return {"Authorization": value + (f', Token="{token}"' if token else "")}


def login(base, ids, timeout):
    """An access token for the corpus's browse user (`user_name`, `password`)."""
    body = json.dumps({"Username": lookup(ids, "user_name"), "Pw": lookup(ids, "password")})
    reply = fetch(base, "POST", "/Users/AuthenticateByName", {**auth_header(), "Content-Type": "application/json"}, timeout, body=body)
    if reply.status is None:
        raise ParityError(f"{base}: cannot sign in ({reply.error})")
    if reply.status != 200:
        raise ParityError(f"{base}: signing in answered {reply.status}")
    try:
        answer = json.loads(reply.body)
    except ValueError as err:
        raise ParityError(f"{base}: cannot sign in (the answer is not JSON)") from err
    token = answer.get("AccessToken") if isinstance(answer, dict) else None
    if not token:
        raise ParityError(f"{base}: signing in returned no access token")
    return token


# ── normalisation and comparison ────────────────────────────────────────────
@dataclass(frozen=True)
class Rule:
    paths: tuple
    kind: str
    reason: str
    by: str = ""


def load_rules(path):
    data = tomllib.loads(Path(path).read_text())
    unknown = set(data) - {"version", "rule"}
    if unknown:
        raise ParityError(f"{path}: unknown key(s) {', '.join(sorted(unknown))}")
    rules, claimed = [], {}
    for number, row in enumerate(data.get("rule", []), 1):
        where = f"{path}: rule {number}"
        unknown = set(row) - {"paths", "kind", "reason", "by"}
        if unknown:
            raise ParityError(f"{where}: unknown key(s) {', '.join(sorted(unknown))}")
        paths = row.get("paths")
        if not isinstance(paths, list) or not paths or not all(isinstance(p, str) and p for p in paths):
            raise ParityError(f"{where}: `paths` lists the exact field paths it applies to")
        if any("*" in p or "?" in p for p in paths):
            raise ParityError(f"{where}: a path names exact fields; there are no wildcards")
        if row.get("kind") not in RULE_KINDS:
            raise ParityError(f"{where}: `kind` is one of {', '.join(RULE_KINDS)}")
        if not isinstance(row.get("reason"), str) or not row["reason"].strip():
            raise ParityError(f"{where}: `reason` says why the rule exists")
        if row["kind"] == "unordered" and (not isinstance(row.get("by"), str) or not row["by"]):
            raise ParityError(f"{where}: an unordered rule names the field its elements are keyed by (`by`)")
        for p in paths:
            if p in claimed:
                raise ParityError(f"{where}: `{p}` is already a path of rule {claimed[p]}")
            claimed[p] = number
        rules.append(Rule(tuple(paths), row["kind"], row["reason"].strip(), row.get("by", "")))
    return rules


def json_type(value):
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, str):
        return "string"
    return "array" if isinstance(value, list) else "object"


def brief(value):
    """A value as the report shows it: scalars whole, containers by size."""
    if isinstance(value, list):
        return f"array({len(value)})"
    if isinstance(value, dict):
        return f"object({len(value)})"
    return json.dumps(value, ensure_ascii=False)


@dataclass(frozen=True)
class Difference:
    path: str          # generalised: Items[].Name
    at: str            # where in this response: Items[3].Name
    kind: str
    oracle: str
    ferrofin: str

    def as_dict(self):
        return {"path": self.path, "at": self.at, "kind": self.kind, "oracle": self.oracle, "ferrofin": self.ferrofin}


class Comparison:
    """Compares two bodies, applying the rules; `used` collects the rules that matched a field."""

    def __init__(self, rules):
        self.by_path = {p: rule for rule in rules for p in rule.paths}
        self.used = set()
        self.differences = []

    def add(self, path, at, kind, oracle, ferrofin):
        self.differences.append(Difference(path or "$", at or "$", kind, brief(oracle), brief(ferrofin)))

    def walk(self, a, b, path, at):
        rule = self.by_path.get(path)
        if rule and rule.kind == "opaque":
            self.used.add(rule)
            if (a is None) != (b is None):
                self.add(path, at, "null", a, b)
            elif json_type(a) != json_type(b):
                self.add(path, at, "type", a, b)
            return
        if json_type(a) != json_type(b):
            self.add(path, at, "null" if a is None or b is None else "type", a, b)
        elif rule and rule.kind == "opaque-keys" and isinstance(a, dict):
            self.opaque_keys(a, b, path, at, rule)
        elif isinstance(a, dict):
            for key in sorted(set(a) | set(b)):
                child = f"{path}.{key}" if path else key
                where = f"{at}.{key}" if at else key
                if key not in b:
                    self.add(child, where, "missing", a[key], None)
                elif key not in a:
                    self.add(child, where, "extra", None, b[key])
                else:
                    self.walk(a[key], b[key], child, where)
        elif isinstance(a, list):
            self.arrays(a, b, path, at, rule)
        elif a != b:
            self.add(path, at, "value", a, b)

    def opaque_keys(self, a, b, path, at, rule):
        """Objects whose keys are opaque (an image tag): the keys may differ, the number of entries
        and the values may not. Values are paired in their own sorted order, so the keys never decide."""
        self.used.add(rule)
        if len(a) != len(b):
            self.add(path, at, "length", a, b)
        by_value = lambda items: sorted(items, key=lambda item: json.dumps(item[1], sort_keys=True))
        for (key, x), (_, y) in zip(by_value(a.items()), by_value(b.items())):
            self.walk(x, y, f"{path}.{{key}}", f"{at}.{key}")

    def arrays(self, a, b, path, at, rule):
        if rule and rule.kind == "unordered":
            self.used.add(rule)
            key = rule.by
            if not all(isinstance(x, dict) and key in x for x in a + b):
                self.add(path, at, "type", a, b)
                return
            a, b = sorted(a, key=lambda x: str(x[key])), sorted(b, key=lambda x: str(x[key]))
        if len(a) != len(b):
            self.add(path, at, "length", a, b)
        for index, (x, y) in enumerate(zip(a, b)):
            self.walk(x, y, f"{path}[]", f"{at}[{index}]")


def compare_bodies(oracle_body, ferrofin_body, rules, used):
    comparison = Comparison(rules)
    comparison.walk(oracle_body, ferrofin_body, "", "")
    used |= comparison.used
    return comparison.differences


def decode(response):
    """The JSON of a response, or (None, reason) when it has none."""
    if not response.body:
        return None, "empty"
    try:
        return json.loads(response.body), ""
    except ValueError:
        return None, "not JSON"


# ── the ledger ──────────────────────────────────────────────────────────────
@dataclass(frozen=True)
class Entry:
    request: str
    path: str
    kinds: tuple
    verified: str
    issue: int
    reason: str

    def link(self):
        return f"VERIFIED {self.verified}" if self.verified else f"issue gl-{self.issue}"


def verified_rows(source=None):
    """`{"GET /Shows/NextUp", …}`: the rows of the parity record (`handlers::VERIFIED`)."""
    text = Path(source or ROOT / VERIFIED_SOURCE).read_text()
    if "pub const VERIFIED:" not in text:
        raise ParityError(f"{source or VERIFIED_SOURCE} has no `pub const VERIFIED:`")
    block = text.split("pub const VERIFIED:", 1)[1].split("\n];", 1)[0]
    return {f"{method.upper()} {path}" for method, path in re.findall(r'method:\s*"(\w+)",\s*path:\s*"([^"]+)"', block)}


def load_ledger(path, source=None):
    data = tomllib.loads(Path(path).read_text())
    unknown = set(data) - {"version", "entry"}
    if unknown:
        raise ParityError(f"{path}: unknown key(s) {', '.join(sorted(unknown))}")
    rows = None
    entries = []
    for number, row in enumerate(data.get("entry", []), 1):
        where = f"{path}: entry {number}"
        unknown = set(row) - {"request", "path", "kind", "verified", "issue", "reason"}
        if unknown:
            raise ParityError(f"{where}: unknown key(s) {', '.join(sorted(unknown))}")
        for key in ("request", "path", "reason"):
            if not isinstance(row.get(key), str) or not row[key].strip():
                raise ParityError(f"{where}: `{key}` is required")
        if ("verified" in row) == ("issue" in row):
            raise ParityError(f"{where}: needs exactly one link, `verified = \"<METHOD> <path>\"` or `issue = <iid>`")
        kinds = row.get("kind")
        kinds = [kinds] if isinstance(kinds, str) else kinds
        if not isinstance(kinds, list) or not kinds or set(kinds) - set(KINDS):
            raise ParityError(f"{where}: `kind` is required: one or more of {', '.join(KINDS)}")
        verified, issue = row.get("verified", ""), row.get("issue", 0)
        if "verified" in row and (not isinstance(verified, str) or not verified.strip()):
            raise ParityError(f"{where}: `verified` is \"<METHOD> <path>\"")
        if verified:
            rows = verified_rows(source) if rows is None else rows
            method, _, route = verified.partition(" ")
            if f"{method.upper()} {route}" not in rows:
                raise ParityError(f"{where}: `{verified}` is not a row of the parity record (handlers::VERIFIED)")
        elif isinstance(issue, bool) or not isinstance(issue, int) or issue <= 0:
            raise ParityError(f"{where}: `issue` is a GitLab issue number")
        entries.append(Entry(row["request"], row["path"], tuple(kinds), verified, issue, row["reason"].strip()))
    return entries


def matching_entry(entries, request_id, difference):
    for entry in entries:
        if (fnmatch.fnmatchcase(request_id, entry.request) and entry.path == difference.path
                and difference.kind in entry.kinds):
            return entry
    return None


# ── one request, both servers ───────────────────────────────────────────────
def examine(request, oracle, ferrofin, rules, used):
    """`(state, differences)` for one request. State is `match`, `differences` or `oracle failed`."""
    unexpected = oracle.status != request.expect_status
    if unexpected and request.oracle_trap:
        return "oracle failed", []
    differences = []
    answer = lambda r: str(r.status) if r.status is not None else r.error
    if oracle.status != ferrofin.status:
        differences.append(Difference("$status", "$status", "status", answer(oracle), answer(ferrofin)))
    elif oracle.status is None:
        differences.append(Difference("$status", "$status", "status", oracle.error, ferrofin.error))
    else:
        (a, why_a), (b, why_b) = decode(oracle), decode(ferrofin)
        if why_a or why_b:
            if (why_a, oracle.body) != (why_b, ferrofin.body):
                differences.append(Difference("$", "$", "value", why_a or "body", why_b or "body"))
        else:
            differences += compare_bodies(a, b, rules, used)
        if unexpected:   # both answered the same error: that is not parity, it is a request that does not work
            differences.append(Difference("$status", "$status", "status", f"{oracle.status} (expected {request.expect_status})",
                                          f"{ferrofin.status}"))
    return ("differences" if differences else "match"), differences


def run_requests(requests, ids, oracle_url, ferrofin_url, tokens, rules, entries, request_timeout, deadline,
                 now=time.monotonic, wall=time.time, send=fetch):
    """Send every request to both servers until `deadline` (epoch seconds, None for none)."""
    used, results, cap_exceeded = set(), [], False

    def budget():
        """`(seconds this call may take, whether the cap is already spent)`: the request timeout, held to the time left."""
        remaining = None if deadline is None else deadline - wall()
        if remaining is not None and remaining <= 0:
            return 0, True
        return (request_timeout if remaining is None else min(request_timeout, remaining)), False

    for request in requests:
        path = target(request, ids)
        replies = []
        for url, role in ((oracle_url, "oracle"), (ferrofin_url, "ferrofin")):   # each call gets what is left, not the pair's first share
            timeout, spent = budget()
            if spent:
                cap_exceeded = True
                break
            reply = send(url, request.method, path, auth_header(tokens[role]), timeout, now)
            if timeout < request_timeout and reply.error == "timeout":
                cap_exceeded = True   # cut short by the cap, not slow: the request does not count against Ferrofin
                break
            replies.append(reply)
        if cap_exceeded:
            break
        a, b = replies
        state, differences = examine(request, a, b, rules, used)
        rows = []
        for difference in differences:
            entry = matching_entry(entries, request.id, difference)
            rows.append({**difference.as_dict(), "ledger": entry.link() if entry else None,
                         "ledger_reason": entry.reason if entry else None,
                         "ledger_entry": entries.index(entry) if entry else None})
        if state == "differences" and all(row["ledger"] for row in rows):
            state = "ledgered"
        results.append({"id": request.id, "method": request.method, "target": path, "source": request.source,
                        "state": state, "oracle": a.brief(), "ferrofin": b.brief(), "differences": rows,
                        "bodies": {"oracle": a.body.decode("utf-8", "replace"),
                                   "ferrofin": b.body.decode("utf-8", "replace")} if state == "differences" else None})
        if deadline is not None and wall() >= deadline:
            cap_exceeded = len(results) < len(requests)
            break
    return results, used, cap_exceeded


def verdict_of(results, cap_exceeded, failure=""):
    if failure:
        return "run failed", failure
    if cap_exceeded:
        return "run failed", "cap exceeded: the run passed its time limit before every request was sent"
    if results and all(r["state"] == "oracle failed" for r in results):
        return "run failed", "no request was compared: the oracle failed on every one"
    new = sum(1 for r in results for d in r["differences"] if not d["ledger"])
    if new:
        return "new differences", f"{new} new difference(s) in {sum(1 for r in results if r['state'] == 'differences')} request(s)"
    return "pass", "no new differences"


def exit_code(verdict):
    return 0 if verdict == "pass" else 1


# ── the report ──────────────────────────────────────────────────────────────
CSS = """
:root{color-scheme:light dark;--bg:#fff;--fg:#1c1f24;--mut:#5b6470;--line:#d6dae0;--bad:#b3261e;--ok:#1a7f37;--warn:#9a6700;--card:#f6f8fa}
@media (prefers-color-scheme:dark){:root{--bg:#14171c;--fg:#e6e9ee;--mut:#9aa4b2;--line:#2c323b;--bad:#ff8a80;--ok:#56d364;--warn:#e3b341;--card:#1b2027}}
body{background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif;margin:0}main{max-width:70rem;margin:0 auto;padding:1.5rem 1rem 4rem}
h1{font-size:1.5rem;margin:0 0 .25rem}h2{font-size:1.15rem;margin:2rem 0 .5rem}code,pre{font:13px/1.45 ui-monospace,monospace}
pre{background:var(--card);border:1px solid var(--line);padding:.6rem;overflow:auto;max-height:28rem}
table{border-collapse:collapse;width:100%;margin:.5rem 0}th,td{border-bottom:1px solid var(--line);padding:.35rem .5rem;text-align:left;vertical-align:top;overflow-wrap:anywhere}
.v{display:inline-block;padding:.15rem .6rem;border-radius:.3rem;font-weight:600;border:1px solid currentColor}.pass{color:var(--ok)}.bad{color:var(--bad)}.warn{color:var(--warn)}
.mut{color:var(--mut)}details{margin:.4rem 0}summary{cursor:pointer}
@media (max-width:40rem){table,thead,tbody,tr,th,td{display:block}thead{display:none}tr{border-bottom:1px solid var(--line);padding:.4rem 0}}
"""


def esc(value):
    return html.escape(str(value))


def table(headers, rows):
    head = "".join(f"<th>{esc(h)}</th>" for h in headers)
    body = "".join("<tr>" + "".join(f"<td>{cell}</td>" for cell in row) + "</tr>" for row in rows)
    return f"<table><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table>"


def difference_rows(result, only_new):
    for d in result["differences"]:
        if bool(d["ledger"]) != only_new:
            yield [f"<code>{esc(result['id'])}</code>", f"<code>{esc(d['at'])}</code>", esc(d["kind"]),
                   f"<code>{esc(d['oracle'])}</code>", f"<code>{esc(d['ferrofin'])}</code>"]


def render(summary):
    """`summary` (see write_results) as one self-contained HTML page."""
    results, run = summary["requests"], summary.get("run", {})
    verdict, reason = summary["verdict"], summary["reason"]
    css = "pass" if verdict == "pass" else "bad"
    out = [f"<h1>Parity run {esc(run.get('name', ''))}</h1>",
           f"<p><span class='v {css}'>{esc(verdict.upper())}</span> {esc(reason)}</p>"]
    facts = [("Oracle", run.get("oracle", {}).get("image", "")), ("Ferrofin", run.get("ferrofin", {}).get("image", "")),
             ("Requests sent", f"{len(results)} of {summary['planned']}"),
             ("Boot", " / ".join(f"{name} {run[side]['boot_s']} s" for name, side in (("Jellyfin", "oracle"), ("Ferrofin", "ferrofin"))
                                 if run.get(side, {}).get("boot_s") is not None)),
             ("Elapsed", f"{summary['elapsed_s']:.1f} s of the {summary['cap_s']} s cap, boot included" if summary.get("cap_s") else f"{summary['elapsed_s']:.1f} s"),
             ("Request timeout", f"{summary['request_timeout_s']} s" if summary.get("request_timeout_s") else "")]
    out.append(table(["", ""], [[esc(k), esc(v)] for k, v in facts if v != ""]))
    new = [r for r in results if r["state"] == "differences"]
    out.append(f"<h2>New differences ({sum(1 for r in new for d in r['differences'] if not d['ledger'])})</h2>")
    if new:
        out.append(table(["Request", "Where", "Kind", "Jellyfin", "Ferrofin"],
                         [row for r in new for row in difference_rows(r, True)]))
        for r in new:
            out.append(f"<details open><summary><code>{esc(r['method'])} {esc(r['target'])}</code> "
                       f"(Jellyfin {esc(r['oracle']['status'] or r['oracle']['error'])}, "
                       f"Ferrofin {esc(r['ferrofin']['status'] or r['ferrofin']['error'])})</summary>")
            for side in ("oracle", "ferrofin"):
                out.append(f"<p class='mut'>{'Jellyfin' if side == 'oracle' else 'Ferrofin'} response</p><pre>{esc(pretty(r['bodies'][side]))}</pre>")
            out.append("</details>")
    else:
        out.append("<p class='mut'>None.</p>")
    ledgered = {}
    for r in results:
        for d in r["differences"]:
            if d["ledger"]:
                ledgered.setdefault(d["ledger"], []).append((r["id"], d))
    out.append(f"<h2>Matched by the ledger ({sum(len(v) for v in ledgered.values())})</h2>")
    for link, rows in sorted(ledgered.items()):
        out.append(f"<h3>{esc(link)}</h3>" + table(["Request", "Where", "Kind", "Jellyfin", "Ferrofin"],
                   [[f"<code>{esc(i)}</code>", f"<code>{esc(d['at'])}</code>", esc(d["kind"]), f"<code>{esc(d['oracle'])}</code>",
                     f"<code>{esc(d['ferrofin'])}</code>"] for i, d in rows]))
    if not ledgered:
        out.append("<p class='mut'>None.</p>")
    failed = [r for r in results if r["state"] == "oracle failed"]
    out.append(f"<h2>Oracle failures ({len(failed)})</h2>")
    out.append(table(["Request", "Jellyfin answered"], [[f"<code>{esc(r['id'])}</code>", esc(r['oracle']['error'] or r['oracle']['status'])]
                                                     for r in failed]) if failed else "<p class='mut'>None.</p>")
    out.append("<h2>Normalisation rules that matched nothing</h2>")
    unused = summary["unused_rules"]
    out.append("<ul>" + "".join(f"<li><code>{esc(', '.join(u['paths']))}</code> <span class='mut'>{esc(u['reason'])}</span></li>"
                                 for u in unused) + "</ul>" if unused else "<p class='mut'>None.</p>")
    stale = summary["unused_ledger"]
    out.append("<h2>Ledger entries that matched nothing</h2>")
    out.append("<ul>" + "".join(f"<li><code>{esc(u['request'])} {esc(u['path'])}</code> <span class='mut'>{esc(u['link'])}</span></li>"
                                 for u in stale) + "</ul>" if stale else "<p class='mut'>None.</p>")
    clean = [r for r in results if r["state"] == "match"]
    out.append(f"<h2>Identical ({len(clean)})</h2>")
    out.append(table(["Request", "Jellyfin ms", "Ferrofin ms"],
                     [[f"<code>{esc(r['id'])}</code>", f"{r['oracle']['seconds'] * 1000:.0f}", f"{r['ferrofin']['seconds'] * 1000:.0f}"]
                      for r in clean]) if clean else "<p class='mut'>None.</p>")
    return ("<!doctype html><html lang='en'><head><meta charset='utf-8'><meta name='viewport' content='width=device-width,initial-scale=1'>"
            f"<title>Parity run {esc(run.get('name', ''))}</title><style>{CSS}</style></head><body><main>{''.join(out)}</main></body></html>")


def pretty(body):
    try:
        return json.dumps(json.loads(body), indent=2, ensure_ascii=False, sort_keys=True)
    except ValueError:
        return body


def write_results(out, summary):
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    (out / "results.json").write_text(json.dumps(summary, indent=1, sort_keys=True))
    (out / "report.html").write_text(render(summary))


def read_run(out):
    """The run's own metadata (run.json), or {} when it is missing or unreadable: a report of a run that
    failed early is the one most likely to find it broken."""
    path = Path(out) / "run.json"
    try:
        run = json.loads(path.read_text())
    except (OSError, ValueError):
        return {}
    return run if isinstance(run, dict) else {}


# ── commands ────────────────────────────────────────────────────────────────
def check_inputs(args, ids=None):
    requests = load_requests(args.requests, ids)
    rules = load_rules(args.normalise)
    entries = load_ledger(args.ledger, args.source)
    return requests, rules, entries


def command_run(args):
    low, high = REQUEST_TIMEOUT_RANGE_S
    if not low <= args.request_timeout <= high:
        raise ParityError(f"--request-timeout is {low} to {high} seconds")
    ids = json.loads(Path(args.ids).read_text())
    requests, rules, entries = check_inputs(args, ids)   # every template resolves before the first request
    started = time.monotonic()
    tokens = {"oracle": login(args.oracle_url, ids, args.request_timeout),
              "ferrofin": login(args.ferrofin_url, ids, args.request_timeout)}
    results, used, cap_exceeded = run_requests(requests, ids, args.oracle_url, args.ferrofin_url, tokens, rules,
                                               entries, args.request_timeout, args.deadline)
    verdict, reason = verdict_of(results, cap_exceeded)
    matched = {row["ledger_entry"] for r in results for row in r["differences"] if row["ledger"]}
    run = read_run(args.out)
    # the cap counts from the start of the whole run (run.json), boot included
    elapsed = time.time() - run["started"] if "started" in run else time.monotonic() - started
    summary = {"verdict": verdict, "reason": reason, "cap_exceeded": cap_exceeded, "planned": len(requests),
               "requests": results, "elapsed_s": elapsed, "run": run,
               "request_timeout_s": args.request_timeout, "cap_s": read_run(args.out).get("cap_s"),
               "unused_rules": [{"paths": list(r.paths), "reason": r.reason} for r in rules if r not in used],
               "unused_ledger": [{"request": e.request, "path": e.path, "link": e.link()} for index, e in enumerate(entries)
                                 if index not in matched]}
    write_results(args.out, summary)
    print(f"parity: {verdict}: {reason}; report: {Path(args.out) / 'report.html'}")
    return exit_code(verdict)


def command_failure(args):
    run = read_run(args.out)
    summary = {"verdict": "run failed", "reason": args.reason, "cap_exceeded": args.cap_exceeded, "planned": 0,
               "requests": [], "elapsed_s": time.time() - run["started"] if "started" in run else 0.0, "run": run, "request_timeout_s": None,
               "cap_s": run.get("cap_s"), "unused_rules": [], "unused_ledger": []}
    write_results(args.out, summary)
    print(f"parity: run failed: {args.reason}; report: {Path(args.out) / 'report.html'}")
    return 1


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)

    def inputs(p):
        p.add_argument("--requests", default=str(HERE / "requests" / "tracer.jsonl"))
        p.add_argument("--normalise", default=str(HERE / "normalise.toml"))
        p.add_argument("--ledger", default=str(HERE / "ledger.toml"))
        p.add_argument("--source", default=None, help="the Rust file holding handlers::VERIFIED (tests)")

    run = sub.add_parser("run")
    inputs(run)
    run.add_argument("--oracle-url", required=True)
    run.add_argument("--ferrofin-url", required=True)
    run.add_argument("--ids", required=True)
    run.add_argument("--out", required=True)
    run.add_argument("--request-timeout", type=int, default=REQUEST_TIMEOUT_DEFAULT_S)
    run.add_argument("--deadline", type=float, default=None, help="epoch seconds at which the run must stop")
    check = sub.add_parser("check")
    inputs(check)
    check.add_argument("--ids", default=None, help="also resolve every request template against this ids.json")
    log = sub.add_parser("login")
    log.add_argument("--url", required=True)
    log.add_argument("--ids", required=True)
    log.add_argument("--timeout", type=int, default=REQUEST_TIMEOUT_DEFAULT_S)
    fail = sub.add_parser("failure")
    fail.add_argument("--out", required=True)
    fail.add_argument("--reason", required=True)
    fail.add_argument("--cap-exceeded", action="store_true")
    return parser


def main(argv):
    args = build_parser().parse_args(argv)
    try:
        if args.command == "run":
            return command_run(args)
        if args.command == "check":
            requests, rules, entries = check_inputs(args, json.loads(Path(args.ids).read_text()) if args.ids else None)
            print(f"parity: {len(requests)} request(s), {len(rules)} rule(s), {len(entries)} ledger entr{'y' if len(entries) == 1 else 'ies'}: valid")
            return 0
        if args.command == "login":
            print(login(args.url, json.loads(Path(args.ids).read_text()), args.timeout))
            return 0
        return command_failure(args)
    except (ParityError, OSError, ValueError) as err:
        print(f"parity: {err}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
