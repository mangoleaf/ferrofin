#!/usr/bin/env python3
"""The parity harness: send requests to Jellyfin and Ferrofin, compare, match the ledger, report.

    diff.py run --oracle-url U --ferrofin-url U --ids IDS.json --requests FILE.jsonl --out DIR
                [--normalise FILE] [--ledger FILE] [--request-timeout S] [--deadline EPOCH]
                [--reset-command CMD]
        send every read to both servers, then each write on a fresh database copy (CMD resets both
        servers; the runner passes parity/reset.sh) followed by its read-backs; compare the
        normalised bodies on values, JSON types and nulls, classify each difference as ledgered or
        new, and write DIR/results.json and DIR/report.html. Exit 0 when nothing is new, 1 on a new
        difference or a run failure (the cap passed, a reset failed), 2 when an input is invalid.
    diff.py check [--requests FILE] [--ledger FILE] [--normalise FILE] [--ids IDS.json]
        validate the input files, and with --ids that every request template resolves (the runner
        does this before it boots anything)
    diff.py login --url U --ids IDS.json
        print an access token for the corpus's browse user
    diff.py failure --out DIR --reason TEXT [--cap-exceeded]
        write the report of a run that could not complete (a server did not boot, the cap passed)

A run that completes always produces a report. Python and the standard library only.

Request file: JSON lines, one object per request: `id` (unique, without `/`), `method` (GET, or a
write: POST, DELETE), `path` (a template, `{name}` is read from the corpus's ids.json, `{views.movies}`
reaches into an object), `query` (object), `body` (a write's JSON body, an object or an array;
templates in its strings are filled too), `source` (where jellyfin-web issues it), `oracle_trap` (why
the oracle is known to stall or fail on it), `expect_status` (the status the oracle answers, default
200: two servers that agree on an error are not a pass unless the request says an error is expected),
and on a write `readbacks`: the GETs (`id`, `path`, `query`, `source`, `oracle_trap`, `expect_status`)
sent after it and compared like any read, reported as `<write id>/<read-back id>`, and `setup`: writes
(fields as a request's) sent on the fresh copy before it, to build the state it acts on (a favourite
for an unfavourite to clear), compared too and reported as `<write id>/<step id>`. A read-back's
`{write.Id}` is read from what each server answered to the write, so each server is asked about
the thing it created.

Normalisation file (normalise.toml) and ledger (ledger.toml) are described in parity/README.md.
"""

import argparse
import concurrent.futures
import contextlib
import dataclasses
import fnmatch
import hashlib
import html
import http.client
import json
import os
import re
import shlex
import signal
import subprocess
import sys
import threading
import time
import tomllib
import urllib.parse
from dataclasses import dataclass
from pathlib import Path
from typing import NamedTuple

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
VERIFIED_SOURCE = "crates/ferrofin-api/src/handlers/mod.rs"
#: The per-request timeout defaults to 30 s and may be set from 10 to 120 s (run.sh holds the same range).
REQUEST_TIMEOUT_DEFAULT_S = 30
REQUEST_TIMEOUT_RANGE_S = (10, 120)
#: Each difference is one of these.
KINDS = ("status", "value", "type", "null", "missing", "extra", "length")
RULE_KINDS = ("opaque", "opaque-keys", "unordered")
#: The methods the harness sends; a write runs on a fresh database copy, after every read.
WRITES = ("POST", "DELETE")
METHODS = ("GET", "HEAD", *WRITES)
PLACEHOLDER = re.compile(r"\{([A-Za-z_][\w.]*)\}")
#: Templates under these prefixes are read from an answer on the same server: `{write.Id}` (in a
#: read-back) from the write's, `{setup.<step>.Id}` (in a later step, the write or a read-back) from a setup step's.
CAPTURES = ("write.", "setup.")


class ParityError(Exception):
    """An input file or argument is invalid; the run cannot start."""


class CaptureMissing(ParityError):
    """A read-back names a field of the write's answer that this server's answer does not have."""


class ResetError(Exception):
    """Resetting the servers to a fresh database copy failed; `timed_out` when the cap stopped it."""

    def __init__(self, message, timed_out=False):
        super().__init__(message)
        self.timed_out = timed_out


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
    readbacks: tuple = ()    # a write's read-backs (Requests whose id is `<write>/<read-back>`)
    of: str = ""             # a read-back's or setup step's write
    setup: tuple = ()        # requests sent on the fresh copy before this write (ids `<write>/<step>`)
    file: str = ""           # the request file it came from (the report's time by request file)

    @property
    def is_write(self):
        return self.method in WRITES


#: `capture` for templates outside a write test: `{write.…}` and `{setup.…}` are errors there.
NO_CAPTURE = None


class Available(frozenset):
    """`capture` for checking templates before anything is sent: the answers (`write`, `setup.<step>`)
    that will exist when the request is sent; any field of those resolves."""


def scalar(value):
    return not isinstance(value, bool) and isinstance(value, (str, int))


def capture_key(name):
    """`("setup.pl", "Id")` for `setup.pl.Id`, `("write", "A.B")` for `write.A.B`."""
    root, _, rest = name.partition(".")
    if root == "setup":
        step, _, rest = rest.partition(".")
        return f"setup.{step}", rest
    return root, rest


def walk(value, field):
    """`field` ("A.0.B": object keys and list indices) read from a JSON value; KeyError when absent."""
    for part in field.split(".") if field else ():
        if isinstance(value, dict) and part in value:
            value = value[part]
        elif isinstance(value, list) and part.isdigit() and int(part) < len(value):
            value = value[int(part)]
        else:
            raise KeyError(part)
    return value


def lookup(ids, name, capture=NO_CAPTURE):
    """`name` ("user", "views.movies") read from the ids file, or `write.Id` / `setup.<step>.Id` read from
    an answer this server gave earlier in the same write test (`capture`: those answers' JSON by key); a
    string or integer."""
    if name.startswith(CAPTURES):
        if capture is NO_CAPTURE:
            raise ParityError(f"`{{{name}}}` reads an earlier answer: it is allowed only in a write's setup steps and read-backs")
        key, field = capture_key(name)
        if isinstance(capture, Available):
            if key not in capture:
                raise ParityError(f"`{{{name}}}` names no step sent before it")
            return "x"
        whose = "the write's answer" if key == "write" else f"setup step `{key.split('.', 1)[1]}`'s answer"
        try:
            value = walk(capture[key], field)
        except (KeyError, TypeError):
            raise CaptureMissing(f"{whose} has no `{name}`") from None
        if not scalar(value):
            raise CaptureMissing(f"{whose} has no `{name}` (it is not a string or an integer)")
        return str(value)
    value = ids
    for part in name.split("."):
        if not isinstance(value, dict) or part not in value:
            raise ParityError(f"the ids file has no `{name}`")
        value = value[part]
    if not scalar(value):
        raise ParityError(f"`{name}` in the ids file is not a string or an integer")
    return str(value)


def fill(text, ids, capture=NO_CAPTURE):
    return PLACEHOLDER.sub(lambda found: lookup(ids, found.group(1), capture), text)


SETUP_FIELDS = {"id", "method", "path", "query", "body", "source", "oracle_trap", "expect_status"}
REQUEST_FIELDS = SETUP_FIELDS | {"readbacks", "setup"}
READBACK_FIELDS = {"id", "path", "query", "source", "oracle_trap", "expect_status"}


def parse_request(row, where, of=None, kind=""):
    """One request object as a Request; with `of`, a `kind` ("read-back" or "setup") of that write."""
    if not isinstance(row, dict):
        raise ParityError(f"{where} is a JSON object" if of else f"{where}: a request is a JSON object")
    unknown = set(row) - {"read-back": READBACK_FIELDS, "setup": SETUP_FIELDS}.get(kind, REQUEST_FIELDS)
    if unknown:
        raise ParityError(f"{where}: unknown field(s) {', '.join(sorted(unknown))}")
    for key in ("id", "path") if kind == "read-back" else ("id", "method", "path"):
        if not isinstance(row.get(key), str) or not row[key]:
            raise ParityError(f"{where}: `{key}` is a non-empty string")
    if "/" in row["id"]:
        raise ParityError(f"{where}: `id` has no `/` in it (a read-back or setup step is reported as `<write id>/<its id>`)")
    method = "GET" if kind == "read-back" else row["method"]
    if method not in METHODS:
        raise ParityError(f"{where}: `{method}` is not a method the harness sends ({', '.join(METHODS)})")
    if kind == "setup" and method not in WRITES and row.get("body") is not None:
        raise ParityError(f"{where}: a setup step has no body unless it is a write")
    if not row["path"].startswith("/"):
        raise ParityError(f"{where}: `path` starts with a slash")
    query = row.get("query") or {}
    pairs = isinstance(query, list) and all(isinstance(pair, list) and len(pair) == 2 and isinstance(pair[0], str) for pair in query)
    if not isinstance(query, dict) and not pairs:
        raise ParityError(f"{where}: `query` is an object or a list of [name, value] pairs (a parameter sent more than once)")
    body = row.get("body")
    if body is not None and method == "GET":
        raise ParityError(f"{where}: a GET carries no body")
    if body is not None and not isinstance(body, (dict, list)):
        raise ParityError(f"{where}: `body` is a JSON object, an array or null")
    expect = row.get("expect_status", 200)
    if isinstance(expect, bool) or not isinstance(expect, int) or not 100 <= expect <= 599:
        raise ParityError(f"{where}: `expect_status` is an HTTP status code")
    children, seen = {}, set()
    for field, child_kind, plural in (("setup", "setup", "setup steps"), ("readbacks", "read-back", "read-backs")):
        rows = row.get(field, [])
        if rows and method not in WRITES:
            raise ParityError(f"{where}: only a write has {plural}")
        if not isinstance(rows, list):
            raise ParityError(f"{where}: `{field}` is a list")
        children[field] = []
        for child_row in rows:
            child = parse_request(child_row, f"{where}: {child_kind}", of=row["id"], kind=child_kind)
            if child.id in seen:
                raise ParityError(f"{where}: id `{child.id.split('/', 1)[1]}` is used twice among its setup steps and read-backs")
            seen.add(child.id)
            children[field].append(child)
    return Request(f"{of}/{row['id']}" if of else row["id"], method, row["path"], query, body,
                   row.get("source", ""), row.get("oracle_trap", ""), expect, tuple(children["readbacks"]), of or "",
                   tuple(children["setup"]))


def request_lines(path):
    """`(file name, where, line)` for every line of the request file `path`, or of every `*.jsonl` file in
    the directory `path` in name order (a request set split into files)."""
    path = Path(path)
    for file in sorted(path.glob("*.jsonl")) if path.is_dir() else [path]:
        for number, line in enumerate(file.read_text().splitlines(), 1):
            yield file.name, f"{file}:{number}", line


def load_requests(path, ids=None):
    """The requests in `path` (a file or a directory of them); with `ids`, every template must also
    resolve against it (`{write.…}` and `{setup.…}` are only known once those are answered)."""
    requests, seen = [], set()
    for file, where, line in request_lines(path):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as err:
            raise ParityError(f"{where}: not JSON ({err})") from err
        request = parse_request(row, where)
        request = dataclasses.replace(request, file=file, setup=tuple(dataclasses.replace(s, file=file) for s in request.setup),
                                      readbacks=tuple(dataclasses.replace(r, file=file) for r in request.readbacks))
        if request.id in seen:
            raise ParityError(f"{where}: request id `{request.id}` is used twice")
        seen.add(request.id)
        requests.append(request)
    if not requests:
        raise ParityError(f"{path}: no requests")
    for request in requests if ids is not None else ():
        for step, available in scopes(request):
            target(step, ids, available)
            body_of(step, ids, available)
    return requests


def scopes(request):
    """`(request, Available)` for a request and each of its setup steps and read-backs, in the order they
    are sent: what each may read with `{setup.…}` and `{write.…}`."""
    if not request.is_write:
        yield request, NO_CAPTURE
        return
    seen = []
    for step in request.setup:
        yield step, Available(seen)
        seen.append("setup." + step.id.split("/", 1)[1])
    yield request, Available(seen)
    for readback in request.readbacks:
        yield readback, Available([*seen, "write"])


def every_id(requests):
    """The ids of the requests, their setup steps and read-backs, as results report them."""
    return [r.id for request in requests for r in (request, *request.setup, *request.readbacks)]


# ── names discovered from the oracle ────────────────────────────────────────
def load_discover(path):
    """`{name: Request-like spec}` from a discovery file: names the corpus does not record (a person, an
    artist), read from the oracle once it is up and filled into `{found.<name>}` templates."""
    data = tomllib.loads(Path(path).read_text())
    unknown = set(data) - {"version", "found"}
    if unknown:
        raise ParityError(f"{path}: unknown key(s) {', '.join(sorted(unknown))}")
    spec = {}
    for name, row in data.get("found", {}).items():
        where = f"{path}: found.{name}"
        if not isinstance(row, dict) or set(row) - {"path", "query", "pick"}:
            raise ParityError(f"{where}: unknown key(s); a name has `path`, `query` and `pick`")
        if not isinstance(row.get("path"), str) or not row["path"].startswith("/"):
            raise ParityError(f"{where}: `path` starts with a slash")
        if not isinstance(row.get("query", {}), dict):
            raise ParityError(f"{where}: `query` is a table")
        if not isinstance(row.get("pick"), str) or not row["pick"]:
            raise ParityError(f"{where}: `pick` is the field of the answer that holds the name (`Items.0.Name`)")
        spec[name] = (Request(f"found-{name}", "GET", row["path"], row.get("query", {}), None, "", "", 200), row["pick"])
    return spec


def used_names(requests):
    """The `{found.<name>}` names the requests (with their setup steps and read-backs) use."""
    names = set()
    for request in requests:
        for one in (request, *request.setup, *request.readbacks):
            text = json.dumps([one.path, one.query, one.body])
            names |= {name.split(".", 1)[1] for name in PLACEHOLDER.findall(text) if name.startswith("found.")}
    return names


def placeholder_ids(ids, spec):
    """The ids with a stand-in for every discovered name, to check templates before anything boots."""
    return {**ids, "found": {name: "x" for name in spec}}


def discover(base, token, spec, ids, timeout, send=None):
    """`{name: value}` read from the oracle for each discovered name; ParityError when one is missing."""
    send = send or fetch
    found = {}
    for name, (request, pick) in spec.items():
        reply = send(base, "GET", target(request, ids), auth_header(token), timeout)
        value, why = None, f"it answered {reply.status or reply.error}"
        if reply.status == 200:
            try:
                value = walk(json.loads(reply.body), pick)
            except (ValueError, KeyError, TypeError):
                why = f"its answer has no `{pick}`"
        if not scalar(value):
            raise ParityError(f"cannot discover `{name}` from the oracle ({request.path}): {why}")
        found[name] = str(value)
    return found


def query_value(value, ids, capture=NO_CAPTURE):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, list):
        return ",".join(query_value(item, ids, capture) for item in value)
    return fill(str(value), ids, capture)


def target(request, ids, capture=NO_CAPTURE):
    """The request line's path and query, templates filled from `ids` (and a read-back's `{write.…}`
    from `capture`, the JSON this server answered to the write)."""
    path = urllib.parse.quote(fill(request.path, ids, capture), safe="/%")
    if request.query:
        items = request.query.items() if isinstance(request.query, dict) else request.query
        pairs = [(key, query_value(value, ids, capture)) for key, value in items]
        path += "?" + urllib.parse.urlencode(pairs, quote_via=urllib.parse.quote, safe=",")
    return path


def filled(value, ids, capture=NO_CAPTURE):
    if isinstance(value, str):
        return fill(value, ids, capture)
    if isinstance(value, list):
        return [filled(item, ids, capture) for item in value]
    if isinstance(value, dict):
        return {key: filled(item, ids, capture) for key, item in value.items()}
    return value


def body_of(request, ids, capture=NO_CAPTURE):
    """A write's JSON body, its templates filled, as bytes; None for a request without one."""
    return None if request.body is None else json.dumps(filled(request.body, ids, capture)).encode()


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
    requests: tuple = ()     # request ids or globs the rule is limited to; empty: every request


def load_rules(path):
    data = tomllib.loads(Path(path).read_text())
    unknown = set(data) - {"version", "rule"}
    if unknown:
        raise ParityError(f"{path}: unknown key(s) {', '.join(sorted(unknown))}")
    rules, claimed = [], {}
    for number, row in enumerate(data.get("rule", []), 1):
        where = f"{path}: rule {number}"
        unknown = set(row) - {"paths", "kind", "reason", "by", "requests"}
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
        scope = row.get("requests", None)
        if scope is not None and (not isinstance(scope, list) or not scope or not all(isinstance(g, str) and g for g in scope)):
            raise ParityError(f"{where}: `requests` lists request ids or globs the rule is limited to")
        for p in paths:
            # A path belongs to one rule for every request, or to rules each limited to some requests
            # (check_rule_scopes makes sure no request has two of those).
            for earlier, limited in claimed.get(p, ()):
                if not limited or not scope:
                    raise ParityError(f"{where}: `{p}` is already a path of rule {earlier}"
                                      + ("" if limited else " for every request"))
            claimed.setdefault(p, []).append((number, bool(scope)))
        rules.append(Rule(tuple(paths), row["kind"], row["reason"].strip(), row.get("by", ""), tuple(scope or ())))
    return rules


def rules_for(rules, request_id):
    """The rules that apply to the request `request_id`, in file order."""
    return [rule for rule in rules
            if not rule.requests or any(fnmatch.fnmatchcase(request_id, glob) for glob in rule.requests)]


def check_rule_scopes(rules, request_ids):
    """No request may have one path in two rules (two rules limited to requests can share a path, but
    never on the same request: which one applied would depend on the order)."""
    for request_id in request_ids:
        owner = {}
        for number, rule in enumerate(rules, 1):
            if rule not in rules_for([rule], request_id):
                continue
            for p in rule.paths:
                if p in owner:
                    raise ParityError(f"`{p}` of request `{request_id}` is a path of rules {owner[p]} and {number} "
                                      "of the normalisation file")
                owner[p] = number


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


#: How much of a differing body results.json keeps: a video stream is compared whole, never stored whole.
STORED_BODY_BYTES = 256 * 1024


def stored(body):
    """A body as results.json keeps it: whole when small, else its start, its size and its digest."""
    if len(body) <= STORED_BODY_BYTES:
        return body.decode("utf-8", "replace")
    head = body[:STORED_BODY_BYTES].decode("utf-8", "replace")
    return f"{head}\n… {len(body)} bytes in all, sha256 {hashlib.sha256(body).hexdigest()}"


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


class Outcome(NamedTuple):
    """What run_requests did: the results, the rules used, whether the cap stopped it, why it failed."""
    results: list
    used: set
    cap_exceeded: bool
    failure: str
    timing: dict   # per phase: {"reads": {requests, seconds, concurrency}, "writes": {requests, seconds, resets_s}}


ROLES = ("oracle", "ferrofin")
#: Reads in flight per server at once (PARITY_CONCURRENCY): default 4, 1 to 8 (run.sh holds the same range).
CONCURRENCY_DEFAULT = 4
CONCURRENCY_RANGE = (1, 8)


def run_requests(requests, ids, oracle_url, ferrofin_url, tokens, rules, entries, request_timeout, deadline,
                 now=time.monotonic, wall=time.time, send=fetch, reset=None, concurrency=1):
    """Send every read to both servers, `concurrency` pairs at a time (so each server has at most that many
    in flight), then each write one at a time, each on a fresh database copy (`reset(timeout)` gives both
    servers one and returns new tokens), after its setup steps and followed by its read-backs, until
    `deadline` (epoch seconds, None for none). Writes never run beside reads: every read is answered first."""
    low, high = CONCURRENCY_RANGE
    if not low <= concurrency <= high:
        raise ParityError(f"the read concurrency is {low} to {high}")
    reads = [r for r in requests if not r.is_write]
    writes = [r for r in requests if r.is_write]
    if writes and reset is None:
        raise ParityError("a write needs a fresh database copy on both servers: give the run a reset (--reset-command)")
    planned = len(every_id(requests))
    urls, tokens = dict(zip(ROLES, (oracle_url, ferrofin_url))), dict(tokens)
    used, results = set(), []

    def budget():
        """`(seconds this call may take, whether the cap is already spent)`: the request timeout, held to the time left."""
        remaining = None if deadline is None else deadline - wall()
        if remaining is not None and remaining <= 0:
            return 0, True
        return (request_timeout if remaining is None else min(request_timeout, remaining)), False

    def exchange(request, paths, bodies=None, unsent=None):
        """Both servers' answers, or None when the cap cut the pair short. A side in `unsent` is not
        asked: its answer is the given stand-in (a step an earlier answer gave nothing to ask about)."""
        replies = []
        for role in ROLES:   # each call gets what is left, not the pair's first share
            if unsent and role in unsent:
                replies.append(unsent[role])
                continue
            timeout, spent = budget()
            if spent:
                return None
            headers = auth_header(tokens[role])
            body = (bodies or {}).get(role)
            if body is not None:
                headers["Content-Type"] = "application/json"
            reply = send(urls[role], request.method, paths[role], headers, timeout, now, body=body)
            if timeout < request_timeout and reply.error == "timeout":
                return None   # cut short by the cap, not slow: the request does not count against Ferrofin
            replies.append(reply)
        return replies

    def record(request, paths, a, b, state=None, keep=True, **extra):
        differences = []
        if state is None:
            state, differences = examine(request, a, b, rules_for(rules, request.id), used)
        rows = []
        for difference in differences:
            entry = matching_entry(entries, request.id, difference)
            rows.append({**difference.as_dict(), "ledger": entry.link() if entry else None,
                         "ledger_reason": entry.reason if entry else None,
                         "ledger_entry": entries.index(entry) if entry else None})
        if state == "differences" and all(row["ledger"] for row in rows):
            state = "ledgered"
        result = {"id": request.id, "method": request.method, "target": paths["oracle"], "source": request.source,
                  "state": state, "oracle": a.brief(), "ferrofin": b.brief(), "differences": rows,
                  "bodies": {"oracle": stored(a.body), "ferrofin": stored(b.body)} if state == "differences" else None, **extra}
        if paths["ferrofin"] != paths["oracle"]:
            result["ferrofin_target"] = paths["ferrofin"]
        if request.of:
            result["of"] = request.of
        if request.file:
            result["file"] = request.file
        if state == "oracle failed" and request.oracle_trap:
            result["oracle_trap"] = request.oracle_trap
        if keep:
            results.append(result)
        return result

    def past_deadline():
        return deadline is not None and wall() >= deadline

    timing = {"reads": {"requests": 0, "seconds": 0.0, "concurrency": concurrency},
              "writes": {"requests": 0, "seconds": 0.0, "resets_s": 0.0}}
    phase = {"name": "reads", "started": now()}

    def finish(cap_exceeded=False, failure=""):
        timing[phase["name"]]["seconds"] = round(now() - phase["started"], 3)
        timing["reads"]["requests"] = sum(1 for r in results if r["method"] in ("GET", "HEAD") and "of" not in r)
        timing["writes"]["requests"] = len(results) - timing["reads"]["requests"]
        timing["writes"]["resets_s"] = round(sum(r.get("reset_s", 0.0) for r in results), 3)
        return Outcome(results, used, cap_exceeded, failure, timing)

    def read(request):
        """One read sent to both servers: its result, or None when the cap cut it short."""
        path = target(request, ids)
        replies = exchange(request, {"oracle": path, "ferrofin": path})
        return None if replies is None else record(request, {"oracle": path, "ferrofin": path}, *replies, keep=False)

    if concurrency == 1:
        for request in reads:
            result = read(request)
            if result is None:
                return finish(cap_exceeded=True)
            results.append(result)
            if past_deadline():
                return finish(cap_exceeded=len(results) < planned)
    elif reads:
        # Each worker sends one read to the oracle, then to Ferrofin: neither server has more than
        # `concurrency` in flight. Results keep the request file's order.
        with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
            done = list(pool.map(read, reads))
        results.extend(result for result in done if result is not None)
        if None in done or (past_deadline() and len(results) < planned):
            return finish(cap_exceeded=True)

    timing["reads"]["seconds"] = round(now() - phase["started"], 3)
    phase.update(name="writes", started=now())
    for write in writes:
        remaining = None if deadline is None else deadline - wall()
        if remaining is not None and remaining <= 0:
            return finish(cap_exceeded=True)
        started = now()
        try:
            tokens = dict(reset(remaining))
        except ResetError as err:
            if err.timed_out and remaining is not None:
                return finish(cap_exceeded=True)
            return finish(failure=f"resetting the servers before `{write.id}` failed: {err}")
        reset_s = round(now() - started, 3)
        # Each server's answers so far in this write test, for the `{setup.…}` and `{write.…}` templates.
        captures = {role: {} for role in ROLES}
        steps = [(step, "setup." + step.id.split("/", 1)[1]) for step in write.setup] + [(write, "write")]
        steps += [(readback, None) for readback in write.readbacks]
        skipped = None   # why the rest of the test is not sent: the oracle failed a step the rest builds on
        for request, key in steps:
            extra = {"reset_s": reset_s} if request is write else {}
            if skipped:
                stand_in = Response(None, b"", 0.0, f"not sent: {skipped}")
                record(request, {"oracle": request.path, "ferrofin": request.path}, stand_in, stand_in, state="oracle failed", **extra)
                continue
            paths, bodies, unsent = {}, {}, {}
            for role in ROLES:
                try:
                    paths[role] = target(request, ids, captures[role])
                    bodies[role] = body_of(request, ids, captures[role])
                except CaptureMissing as err:   # what an earlier answer on this server lacked: this server is not asked
                    paths[role], bodies[role] = request.path, None
                    unsent[role] = Response(None, b"", 0.0, f"not sent: {err}")
            replies = exchange(request, paths, bodies, unsent=unsent)
            if replies is None:
                return finish(cap_exceeded=True)
            # A trap excuses an oracle that stalled or failed, never a server that was not asked at all.
            result = record(dataclasses.replace(request, oracle_trap="") if unsent else request, paths, *replies, **extra)
            if key:
                for role, reply in zip(ROLES, replies):
                    captures[role][key] = decode(reply)[0]
                if result["state"] == "oracle failed":
                    skipped = "a setup step failed on the oracle" if key != "write" else "the write failed on the oracle"
        if past_deadline():
            return finish(cap_exceeded=len(results) < planned)
    return finish()


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
             ("Request timeout", f"{summary['request_timeout_s']} s" if summary.get("request_timeout_s") else ""),
             ("Reads", reads_fact(summary.get("timing"))), ("Writes", writes_fact(summary.get("timing"))),
             ("Resets before writes", resets_fact(results))]
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
    out.append(table(["Request", "Jellyfin answered", "Why the oracle is excused", "Ferrofin answered"],
                     [[f"<code>{esc(r['id'])}</code>", esc(r['oracle']['error'] or r['oracle']['status']), esc(r.get("oracle_trap", "")),
                       esc(r['ferrofin']['error'] or r['ferrofin']['status'])] for r in failed]) if failed else "<p class='mut'>None.</p>")
    out.append("<h2>Normalisation rules that matched nothing</h2>")
    unused = summary["unused_rules"]
    out.append("<ul>" + "".join(f"<li><code>{esc(', '.join(u['paths']))}</code> <span class='mut'>{esc(u['reason'])}</span></li>"
                                 for u in unused) + "</ul>" if unused else "<p class='mut'>None.</p>")
    stale = summary["unused_ledger"]
    out.append("<h2>Ledger entries that matched nothing</h2>")
    out.append("<ul>" + "".join(f"<li><code>{esc(u['request'])} {esc(u['path'])}</code> <span class='mut'>{esc(u['link'])}</span></li>"
                                 for u in stale) + "</ul>" if stale else "<p class='mut'>None.</p>")
    files = time_by_file(results)
    if files:
        out.append("<h2>Time by request file</h2><p class='mut'>Server time is the two servers' answer times added up; "
                   "reads overlap PARITY_CONCURRENCY at a time, so it is more than the time the reads took.</p>")
        out.append(table(["File", "Requests", "Jellyfin s", "Ferrofin s", "Resets s"],
                         [[f"<code>{esc(name)}</code>", str(row["requests"]), f"{row['oracle_s']:.1f}", f"{row['ferrofin_s']:.1f}",
                           f"{row['resets_s']:.1f}"] for name, row in files.items()]))
    writes = [r for r in results if "reset_s" in r]
    if writes:   # each write on its fresh copy, with what its read-backs found
        out.append(f"<h2>Writes ({len(writes)})</h2>")
        out.append(table(["Write", "Reset", "Answer", "Setup steps and read-backs"],
                         [[f"<code>{esc(w['method'])} {esc(w['id'])}</code>", f"{w['reset_s']:.1f} s", esc(w["state"]),
                           "<br>".join(f"<code>{esc(r['id'])}</code> {esc(r['state'])}" for r in results if r.get("of") == w["id"]) or "none"]
                          for w in writes]))
    clean = [r for r in results if r["state"] == "match"]
    out.append(f"<h2>Identical ({len(clean)})</h2>")
    out.append(table(["Request", "Jellyfin ms", "Ferrofin ms"],
                     [[f"<code>{esc(r['id'])}</code>", f"{r['oracle']['seconds'] * 1000:.0f}", f"{r['ferrofin']['seconds'] * 1000:.0f}"]
                      for r in clean]) if clean else "<p class='mut'>None.</p>")
    return ("<!doctype html><html lang='en'><head><meta charset='utf-8'><meta name='viewport' content='width=device-width,initial-scale=1'>"
            f"<title>Parity run {esc(run.get('name', ''))}</title><style>{CSS}</style></head><body><main>{''.join(out)}</main></body></html>")


def time_by_file(results):
    """`{file: {requests, oracle_s, ferrofin_s, resets_s}}` in the order the files were first sent."""
    files = {}
    for r in results:
        if "file" not in r:
            continue
        row = files.setdefault(r["file"], {"requests": 0, "oracle_s": 0.0, "ferrofin_s": 0.0, "resets_s": 0.0})
        row["requests"] += 1
        row["oracle_s"] += r["oracle"]["seconds"]
        row["ferrofin_s"] += r["ferrofin"]["seconds"]
        row["resets_s"] += r.get("reset_s", 0.0)
    return files


def reads_fact(timing):
    reads = (timing or {}).get("reads")
    if not reads or not reads["requests"]:
        return ""
    return f"{reads['requests']} reads in {reads['seconds']:.1f} s, {reads['concurrency']} at a time per server"


def writes_fact(timing):
    writes = (timing or {}).get("writes")
    if not writes or not writes["requests"]:
        return ""
    return f"{writes['requests']} write requests in {writes['seconds']:.1f} s, {writes['resets_s']:.1f} s of it resets"


def resets_fact(results):
    """How many resets the writes took and how long, which the cap counts: "" without writes."""
    times = [r["reset_s"] for r in results if "reset_s" in r]
    if not times:
        return ""
    return f"{len(times)} reset{'' if len(times) == 1 else 's'}, {sum(times):.1f} s in all, longest {max(times):.1f} s"


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
    spec = load_discover(args.discover)
    requests = load_requests(args.requests, None if ids is None else placeholder_ids(ids, spec))
    rules = load_rules(args.normalise)
    check_rule_scopes(rules, every_id(requests))
    entries = load_ledger(args.ledger, args.source)
    return requests, rules, entries


class ResetCommand:
    """The runner's reset (parity/reset.sh RUN_DIR), run in a session of its own so that the cap, or a
    signal to this process, stops it together with everything it started."""

    def __init__(self, command):
        self.argv = shlex.split(command)
        self.process = None

    def __call__(self, timeout):
        self.process = subprocess.Popen(self.argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                        stderr=subprocess.PIPE, text=True, start_new_session=True)
        try:
            _, err = self.process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.stop()
            raise ResetError(f"the reset did not finish in the {timeout:.0f} s the cap left", timed_out=True) from None
        code, self.process = self.process.returncode, None
        if code:
            lines = [line for line in err.splitlines() if line.strip()]
            # reset.sh prints, per server that failed, the end of what docker said and then its verdict
            raise ResetError("; ".join(lines[-8:]) if lines else f"the reset command exited {code}")

    def stop(self):
        """Kill the reset in flight, if any, and whatever it started."""
        if self.process is None:
            return
        with contextlib.suppress(ProcessLookupError):
            os.killpg(self.process.pid, signal.SIGKILL)
        # `timeout` puts the docker calls it wraps in process groups of their own, still in this session
        subprocess.run(["pkill", "-KILL", "-s", str(self.process.pid)], check=False)
        self.process.communicate()
        self.process = None


def command_run(args):
    low, high = REQUEST_TIMEOUT_RANGE_S
    if not low <= args.request_timeout <= high:
        raise ParityError(f"--request-timeout is {low} to {high} seconds")
    ids = json.loads(Path(args.ids).read_text())
    requests, rules, entries = check_inputs(args, ids)   # every template resolves before the first request
    low, high = CONCURRENCY_RANGE
    if not low <= args.concurrency <= high:
        raise ParityError(f"--concurrency is {low} to {high}")
    if any(r.is_write for r in requests) and not args.reset_command:
        raise ParityError("the request file has writes: they need --reset-command, which gives both servers a fresh database copy")
    started = time.monotonic()
    urls = dict(zip(ROLES, (args.oracle_url, args.ferrofin_url)))
    tokens = {role: login(url, ids, args.request_timeout) for role, url in urls.items()}
    # Names the corpus does not record are read from the oracle; both servers serve the same library.
    spec = load_discover(args.discover)
    found = discover(args.oracle_url, tokens["oracle"], {name: spec[name] for name in sorted(used_names(requests))}, ids, args.request_timeout)
    ids = {**ids, "found": found}
    resetter = ResetCommand(args.reset_command) if args.reset_command else None

    def reset(timeout):
        resetter(timeout)
        left = None if args.deadline is None else args.deadline - time.time()
        if left is not None and left <= 0:
            raise ResetError("the cap ran out during the reset", timed_out=True)
        try:   # the fresh copy holds no session this run made
            return {role: login(url, ids, args.request_timeout if left is None else min(args.request_timeout, left))
                    for role, url in urls.items()}
        except ParityError as err:
            raise ResetError(f"cannot sign in after the reset: {err}") from err

    def interrupted(signum, _frame):   # a reset in flight runs in its own session: stop it too
        resetter.stop()
        raise SystemExit(128 + signum)

    handlers = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT)} if resetter else {}
    try:
        outcome = run_requests(requests, ids, args.oracle_url, args.ferrofin_url, tokens, rules, entries,
                               args.request_timeout, args.deadline, reset=reset if resetter else None,
                               concurrency=args.concurrency)
    finally:
        for sig, handler in handlers.items():
            signal.signal(sig, handler)
    results = outcome.results
    verdict, reason = verdict_of(results, outcome.cap_exceeded, outcome.failure)
    matched = {row["ledger_entry"] for r in results for row in r["differences"] if row["ledger"]}
    run = read_run(args.out)
    # the cap counts from the start of the whole run (run.json), boot included
    elapsed = time.time() - run["started"] if "started" in run else time.monotonic() - started
    outcome.timing["files"] = time_by_file(results)
    summary = {"verdict": verdict, "reason": reason, "cap_exceeded": outcome.cap_exceeded, "planned": len(every_id(requests)),
               "requests": results, "elapsed_s": elapsed, "run": run, "resets": sum(1 for r in results if "reset_s" in r),
               "timing": outcome.timing, "found": found,
               "request_timeout_s": args.request_timeout, "cap_s": run.get("cap_s"),
               "unused_rules": [{"paths": list(r.paths), "reason": r.reason} for r in rules if r not in outcome.used],
               "unused_ledger": [{"request": e.request, "path": e.path, "link": e.link()} for index, e in enumerate(entries)
                                 if index not in matched]}
    write_results(args.out, summary)
    print(f"parity: {verdict}: {reason}; report: {Path(args.out) / 'report.html'}")
    return exit_code(verdict)


def command_failure(args):
    run = read_run(args.out)
    summary = {"verdict": "run failed", "reason": args.reason, "cap_exceeded": args.cap_exceeded, "planned": 0,
               "requests": [], "resets": 0, "elapsed_s": time.time() - run["started"] if "started" in run else 0.0, "run": run, "request_timeout_s": None,
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
        p.add_argument("--discover", default=str(HERE / "discover.toml"), help="names read from the oracle for {found.*}")

    run = sub.add_parser("run")
    inputs(run)
    run.add_argument("--oracle-url", required=True)
    run.add_argument("--ferrofin-url", required=True)
    run.add_argument("--ids", required=True)
    run.add_argument("--out", required=True)
    run.add_argument("--request-timeout", type=int, default=REQUEST_TIMEOUT_DEFAULT_S)
    run.add_argument("--deadline", type=float, default=None, help="epoch seconds at which the run must stop")
    run.add_argument("--concurrency", type=int, default=CONCURRENCY_DEFAULT, help="reads in flight per server, 1 to 8")
    run.add_argument("--reset-command", default=None,
                     help="resets both servers to a fresh database copy before each write (the runner passes parity/reset.sh RUN_DIR)")
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
