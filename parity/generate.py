#!/usr/bin/env python3
"""Generate the parity sweep from the vendored contract: at least one request for every operation.

    generate.py            write parity/requests/sweep/contract.jsonl and combos.jsonl
    generate.py --check    exit 1 when the checked-in files are not what this would write

Every operation of contracts/jellyfin-openapi-10.11.8.json gets a request (contract.jsonl). Its path
parameters come from `[path]` in parity/sweep.toml (corpus ids from ids.json, names discovered from the
oracle, captures of a setup step), its required query parameters from `[query]`, and its expected status
is the lowest success status the contract documents. `[op.<operationId>]` overrides any of that for one
operation: a body, setup steps, an `oracle_trap`, the status an operation answers on this corpus with
its reason, or `readback_of`, which sends a read that needs something a write creates (a playlist) as a
read-back of that write. `[combos]` adds documented-parameter combinations for the query-heavy
operations (combos.jsonl): one request per value of each of their enumerated and boolean parameters.

Writes are written as requests here; the runner sends each on a fresh database copy. The generated files
are checked in, so a change to the request set is a reviewable diff, and a self-test fails when they are
stale or when an operation has no request. Python and the standard library only.
"""

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
CONTRACT = ROOT / "contracts" / "jellyfin-openapi-10.11.8.json"
CONFIG = HERE / "sweep.toml"
OUT = HERE / "requests" / "sweep"
GENERATED = ("contract.jsonl", "combos.jsonl")
OVERRIDE_KEYS = {"path", "query", "body", "setup", "readbacks", "expect_status", "oracle_trap", "readback_of", "source", "variants"}


class GenerateError(Exception):
    """The contract or parity/sweep.toml cannot produce a request set."""


def operations(contract):
    """`(id, METHOD, path, operation)` for every operation of the contract, in contract order. The id is
    the operationId; the contract gives two plugin operations the same one, so a repeat is told apart by
    its path (`GetPluginMetadata`, `GetPluginMetadata_IntroSkipper`)."""
    seen = set()
    for path, item in contract["paths"].items():
        for method, operation in item.items():
            if method == "parameters":
                continue
            op_id = operation["operationId"]
            if op_id in seen:
                op_id += "_" + re.sub(r"[^A-Za-z0-9]+", "", path.title())
            seen.add(op_id)
            yield op_id, method.upper(), path, operation


def covered(requests):
    """`{"METHOD /path"}`: the contract operations the requests (and their read-backs) send: the one each
    cites in its `source`, counted only when the request's method and path are that operation's."""
    found = set()
    for request in requests:
        for one in (request, *request.get("readbacks", [])):
            source = one.get("source", "")
            if not source.startswith("contract: "):
                continue
            operation = source[len("contract: "):].split(",")[0]
            method, _, template = operation.partition(" ")
            shape = "^" + re.sub(r"\\\{[^}]+\\\}", "[^/]+", re.escape(template)) + "$"
            if one.get("method", "GET") == method and re.match(shape, one["path"].split("?")[0]):
                found.add(operation)
    return found


def expected_status(operation):
    codes = sorted(int(code) for code in operation["responses"] if code.startswith("2"))
    return codes[0] if codes else 200


def path_value(name, path, config):
    """The template for path parameter `name` of `path`: `[path.by_prefix]` first (a `name` means a
    genre under /Genres and a person under /Persons), then `[path]`."""
    for prefix, values in config.get("path_by_prefix", {}).items():
        if path.startswith(prefix) and name in values:
            return values[name]
    if name not in config["path"]:
        raise GenerateError(f"{path}: no value for the path parameter `{name}` ([path] in sweep.toml)")
    return config["path"][name]


def base_request(op_id, method, path, operation, config):
    """The default request for one operation, before its override."""
    filled = re.sub(r"\{([^}]+)\}", lambda m: path_value(m.group(1), path, config), path)
    query = {}
    for parameter in operation.get("parameters", []):
        if parameter["in"] == "query" and parameter.get("required"):
            name = parameter["name"]
            if name not in config["query"]:
                raise GenerateError(f"{op_id}: no value for the required query parameter `{name}` ([query] in sweep.toml)")
            query[name] = config["query"][name]
    request = {"id": op_id, "method": method, "path": filled, "source": f"contract: {method} {path}"}
    if query:
        request["query"] = query
    # A JSON body the contract documents is sent, empty unless an override fills it: without one the
    # request has no Content-Type and both servers refuse it (415) before reaching the operation.
    if "application/json" in operation.get("requestBody", {}).get("content", {}) and method in ("POST", "DELETE"):
        request["body"] = {}
    status = expected_status(operation)
    if status != 200:
        request["expect_status"] = status
    return request


def apply_override(request, override, op_id):
    unknown = set(override) - OVERRIDE_KEYS
    if unknown:
        raise GenerateError(f"[op.{op_id}]: unknown key(s) {', '.join(sorted(unknown))}")
    request = dict(request)
    for key in ("path", "body", "setup", "readbacks", "expect_status", "oracle_trap", "source"):
        if key in override:
            request[key] = override[key]
    if "query" in override:   # a table merges into the required parameters; a list of pairs replaces them
        query = override["query"]
        request["query"] = query if isinstance(query, list) else {**request.get("query", {}), **query}
    return request


SETUP_REFERENCE = re.compile(r"\{setup\.([A-Za-z0-9_-]+)\.")


def with_setups(request, config):
    """The request with the named setup sequences (`[setups.<name>]`) it reads with `{setup.<name>.…}`
    put in front of its own setup steps, each once, in the order they are first referenced."""
    own = request.get("setup", [])
    text = json.dumps({key: request.get(key) for key in ("path", "query", "body", "readbacks")}) + json.dumps(own)
    if request["method"] not in ("POST", "DELETE"):
        if SETUP_REFERENCE.search(text):
            raise GenerateError(f"{request['id']}: a read cannot read a setup step; send it as a read-back (`readback_of`)")
        return request
    steps, seen = [], {step["id"] for step in own}
    for name in dict.fromkeys(SETUP_REFERENCE.findall(text)):
        if name in seen:
            continue
        if name not in config.get("setups", {}):
            raise GenerateError(f"{request['id']}: `{{setup.{name}.…}}` names no [setups.{name}] in sweep.toml")
        for step in config["setups"][name]:
            if step["id"] not in seen:
                steps.append(dict(step))
                seen.add(step["id"])
    return {**request, "setup": steps + own} if steps else request


def as_readback(request):
    """A read sent as a read-back: the fields a read-back has."""
    return {key: request[key] for key in ("id", "path", "query", "source", "oracle_trap", "expect_status") if key in request}


def contract_requests(contract, config):
    """The requests of contract.jsonl: one per operation (plus an override's `variants`), the reads an
    override sends as read-backs folded into their write."""
    overrides = config.get("op", {})
    known = {op_id for op_id, *_ in operations(contract)}
    stray = set(overrides) - known
    if stray:
        raise GenerateError(f"[op.*] names operations the contract does not have: {', '.join(sorted(stray))}")
    requests, readbacks = {}, {}
    for op_id, method, path, operation in operations(contract):
        override = overrides.get(op_id, {})
        request = apply_override(base_request(op_id, method, path, operation, config), override, op_id)
        built = [request] + [apply_override({**request, "id": f"{op_id}.{n}"}, variant, op_id)
                             for n, variant in enumerate(override.get("variants", []), 2)]
        for one in built:
            if "readback_of" in override:
                if method not in ("GET", "HEAD"):
                    raise GenerateError(f"[op.{op_id}]: only a read is sent as a read-back")
                readbacks.setdefault(override["readback_of"], []).append(as_readback(one))
            else:
                requests[one["id"]] = with_setups(one, config)
    for write_id, extra in readbacks.items():
        if write_id not in requests or requests[write_id]["method"] not in ("POST", "DELETE"):
            raise GenerateError(f"readback_of `{write_id}` is not a write of the sweep")
        requests[write_id]["readbacks"] = requests[write_id].get("readbacks", []) + extra
    return list(requests.values())


def schema_of(parameter, contract):
    schema = parameter.get("schema", {})
    if "$ref" in schema:
        schema = contract["components"]["schemas"][schema["$ref"].rsplit("/", 1)[1]]
    if "allOf" in schema and len(schema["allOf"]) == 1 and "$ref" in schema["allOf"][0]:
        schema = contract["components"]["schemas"][schema["allOf"][0]["$ref"].rsplit("/", 1)[1]]
    return schema


def values_of(parameter, contract, config):
    """The values a combination sends for one query parameter: each enum value, true and false, or the
    values `[combos.values]` names; none for a parameter it does not cover."""
    name = parameter["name"]
    if name in config["combos"].get("values", {}):
        return config["combos"]["values"][name]
    schema = schema_of(parameter, contract)
    if schema.get("type") == "array":
        schema = schema.get("items", {})
        if "$ref" in schema:
            schema = contract["components"]["schemas"][schema["$ref"].rsplit("/", 1)[1]]
    if "enum" in schema:
        return list(schema["enum"])
    if schema.get("type") == "boolean":
        return [True, False]
    return []


def combo_requests(contract, config):
    """combos.jsonl: for each operation in `[combos] ops`, its base query plus one parameter at a time."""
    combos = config["combos"]
    by_id = {op_id: (method, path, operation) for op_id, method, path, operation in operations(contract)}
    skip = set(combos.get("skip", []))
    requests = []
    for op_id in combos["ops"]:
        if op_id not in by_id:
            raise GenerateError(f"[combos] ops names `{op_id}`, which the contract does not have")
        method, path, operation = by_id[op_id]
        base = apply_override(base_request(op_id, method, path, operation, config), config.get("op", {}).get(op_id, {}), op_id)
        base.pop("variants", None)
        base_query = {**base.get("query", {}), **combos.get("base", {}).get(op_id, combos.get("base_query", {}))}
        for parameter in operation.get("parameters", []):
            if parameter["in"] != "query" or parameter["name"] in skip or parameter["name"] in base_query:
                continue
            for value in values_of(parameter, contract, config):
                slug = re.sub(r"[^A-Za-z0-9]+", "-", str(value).lower()).strip("-") or "empty"
                requests.append({**base, "id": f"{op_id}.{parameter['name']}.{slug}",
                                 "query": {**base_query, parameter["name"]: value},
                                 "source": f"contract: {method} {path}, documented parameter {parameter['name']}"})
    return requests


def render(requests):
    return "".join(json.dumps(request, ensure_ascii=False) + "\n" for request in requests)


def generate(contract_path=CONTRACT, config_path=CONFIG):
    """`{file name: text}` for the generated request files."""
    contract = json.loads(Path(contract_path).read_text())
    config = tomllib.loads(Path(config_path).read_text())
    return {"contract.jsonl": render(contract_requests(contract, config)), "combos.jsonl": render(combo_requests(contract, config))}


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--check", action="store_true", help="exit 1 when the checked-in files are stale")
    args = parser.parse_args(argv)
    try:
        files = generate()
    except (GenerateError, KeyError, ValueError) as err:
        print(f"generate: {err}", file=sys.stderr)
        return 2
    stale = [name for name, text in files.items() if not (OUT / name).exists() or (OUT / name).read_text() != text]
    if args.check:
        for name in stale:
            print(f"generate: {OUT / name} is stale: run parity/generate.py", file=sys.stderr)
        return 1 if stale else 0
    OUT.mkdir(parents=True, exist_ok=True)
    for name, text in files.items():
        (OUT / name).write_text(text)
    print(f"generate: {', '.join(f'{name} ({text.count(chr(10))} requests)' for name, text in files.items())}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
