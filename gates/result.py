"""The one structured result every gate writes, `<run>/result.json` (stdlib only), and its validator.

    {"schema": 1, "gate": "verify", "verdict": "passed", "counts": {"pass": 24, ...},
     "source": {"commit", "image", "image_digest", "oracle", "host_class", "fixture"},
     "started": "2026-10-25T12:00:00+00:00", "ended": "...", "detail": {...}}

`detail` is the gate's own structured data. A gate's report.html, the session page (gates/report.py),
the viewer (gates/bench/report.py --serve) and the release summary are all rendered from this file,
never by parsing a gate's internal files or its text output. The gate's other files stay beside it.
A source field the gate has no value for is null (verify runs no Jellyfin, so its oracle is null).
"""

import json
import os

SCHEMA = 1
GATES = ("verify", "adoption", "parity", "bench")
SOURCE = ("commit", "image", "image_digest", "oracle", "host_class", "fixture")
NAME = "result.json"


def host_class():
    """The host described by class only, never by name: its logical cores and its memory in whole GiB
    (None when /proc/meminfo cannot be read)."""
    memory = None
    try:
        with open("/proc/meminfo", encoding="ascii") as f:
            for line in f:
                if line.startswith("MemTotal:"):
                    memory = round(int(line.split()[1]) / 1024 ** 2)
                    break
    except (OSError, ValueError, IndexError):
        pass
    return {"cores": os.cpu_count(), "memory_gib": memory}


def problems(doc):
    """What is wrong with `doc` as a result envelope; empty when it is valid."""
    if not isinstance(doc, dict):
        return ["the result is not a JSON object"]
    out = []
    expected = {"schema", "gate", "verdict", "counts", "source", "started", "ended", "detail"}
    if missing := sorted(expected - doc.keys()):
        out.append(f"missing {', '.join(missing)}")
    if extra := sorted(doc.keys() - expected):
        out.append(f"unknown {', '.join(extra)}")
    if "schema" in doc and doc["schema"] != SCHEMA:
        out.append(f"schema is {doc['schema']!r}, not {SCHEMA}")
    if "gate" in doc and doc["gate"] not in GATES:
        out.append(f"gate {doc['gate']!r} is not one of {', '.join(GATES)}")
    if "verdict" in doc and not (isinstance(doc["verdict"], str) and doc["verdict"]):
        out.append("verdict is not a non-empty string")
    counts = doc.get("counts")
    if "counts" in doc and not (isinstance(counts, dict) and all(
            isinstance(k, str) and isinstance(v, int) and not isinstance(v, bool) for k, v in counts.items())):
        out.append("counts is not an object of whole numbers")
    source = doc.get("source")
    if "source" in doc:
        if not isinstance(source, dict) or set(source) != set(SOURCE):
            out.append(f"source does not hold exactly {', '.join(SOURCE)}")
        else:
            for key in SOURCE:
                value = source[key]
                kinds = (dict,) if key == "host_class" else (str,)
                if value is not None and not isinstance(value, kinds):
                    out.append(f"source.{key} is neither null nor a {'object' if key == 'host_class' else 'string'}")
    for key in ("started", "ended"):
        if key in doc and doc[key] is not None and not isinstance(doc[key], str):
            out.append(f"{key} is neither null nor a timestamp string")
    if "detail" in doc and not isinstance(doc["detail"], dict):
        out.append("detail is not an object")
    return out


def envelope(gate, verdict, counts, source, started, ended, detail):
    """A valid result; ValueError naming what is wrong otherwise."""
    doc = {"schema": SCHEMA, "gate": gate, "verdict": verdict, "counts": counts,
           "source": {key: source.get(key) for key in SOURCE}, "started": started, "ended": ended, "detail": detail}
    if found := problems(doc):
        raise ValueError(f"not a valid {NAME}: {'; '.join(found)}")
    return doc


def write(run, doc):
    """Write `doc` to RUN/result.json once it validates (a reader never sees half a file); its path."""
    if found := problems(doc):
        raise ValueError(f"not a valid {NAME}: {'; '.join(found)}")
    path = os.path.join(run, NAME)
    with open(path + ".tmp", "w", encoding="utf-8") as f:
        json.dump(doc, f, indent=1)
        f.write("\n")
    os.replace(path + ".tmp", path)
    return path


def read(run, gate=None):
    """RUN/result.json, None when there is none; ValueError when it is not JSON, not a valid envelope,
    or (with `gate`) another gate's."""
    path = os.path.join(run, NAME)
    try:
        with open(path, encoding="utf-8") as f:
            doc = json.load(f)
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as error:
        raise ValueError(f"{NAME} cannot be read ({error})") from error
    if found := problems(doc):
        raise ValueError(f"{NAME} is not a valid result: {'; '.join(found)}")
    if gate and doc["gate"] != gate:
        raise ValueError(f"{NAME} is {doc['gate']}'s result, not {gate}'s")
    return doc
