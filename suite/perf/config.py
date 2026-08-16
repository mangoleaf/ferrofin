#!/usr/bin/env python3
"""Three-layer benchmark configuration: code default < suite/bench.conf < env.

Every methodology knob has a hardcoded default here, so a missing or partial
``bench.conf`` never breaks a run; the committed ``suite/bench.conf`` lists all
of them in one place; process environment wins for one-off overrides. The
resolved values (plus where each came from) are recorded into run ``meta`` so a
published record is self-describing.

Usage::

    from config import CONFIG
    rate = CONFIG["BENCH_RATE"]          # typed (int/float/str by default's type)
    meta = resolved_meta()               # {"values": {...}, "sources": {...}}
"""

import os
from pathlib import Path

SUITE = Path(__file__).resolve().parent.parent
CONF_FILE = SUITE / "bench.conf"

# Code defaults — the last-resort layer. Types here drive parsing: an int
# default is parsed as int, a float as float, everything else kept as str.
DEFAULTS = {
    # statistics
    "BENCH_RUNS": 5,
    "BENCH_NOISE_FLOOR_MS": 3.0,
    # open-loop load
    "BENCH_DURATION_SECS": 30,
    "BENCH_RATE": 25,
    "BENCH_RATE_FRACTION": 0.5,
    "BENCH_RATE_TOLERANCE": 0.99,
    # warm/cold
    "BENCH_WARMUP_SECS": 90,
    "BENCH_COLD_REQUESTS": 10,
    "BENCH_COLD_ENDPOINTS": "info_public user_me items_sortname items_mixed item_detail "
                            "persons studios suggestions movie_recommendations "
                            "items_filters2 image_primary",
    # saturation sweep
    "BENCH_KNEE_P99_MS": 250.0,
    # login storm
    "BENCH_LOGIN_RATE": 10,
    "BENCH_LOGIN_DURATION_SECS": 15,
    # regression gate
    "PERF_GATE_FACTOR": 1.5,
    "PERF_GATE_SECONDS": 10,
    "PERF_GATE_RATE": 25,
}


def _parse_conf(path=CONF_FILE):
    """KEY=value lines; '#' comments and blanks ignored; unknown keys kept
    verbatim (forward-compat: an older config.py under a newer bench.conf
    must not crash)."""
    out = {}
    try:
        text = path.read_text()
    except OSError:
        return out
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, _, v = line.partition("=")
        out[k.strip()] = v.strip()
    return out


def _cast(key, raw):
    """Parse `raw` with the type of the code default; fall back to the default
    (never crash a run on a malformed line — but say so)."""
    default = DEFAULTS[key]
    try:
        if isinstance(default, bool):  # pragma: no cover - no bool knobs today
            return raw.lower() in ("1", "true", "yes")
        if isinstance(default, int):
            return int(raw)
        if isinstance(default, float):
            return float(raw)
        return raw
    except ValueError:
        print(f"!! bench.conf: {key}={raw!r} is not a {type(default).__name__}; using default {default}")
        return default


def load():
    """Resolve every knob and remember its source layer."""
    conf = _parse_conf()
    values, sources = {}, {}
    for key, default in DEFAULTS.items():
        if os.environ.get(key) not in (None, ""):
            values[key], sources[key] = _cast(key, os.environ[key]), "env"
        elif key in conf:
            values[key], sources[key] = _cast(key, conf[key]), "bench.conf"
        else:
            values[key], sources[key] = default, "default"
    return values, sources


CONFIG, CONFIG_SOURCES = load()


def resolved_meta():
    """The self-describing block run records carry in meta.bench_config."""
    return {"values": dict(CONFIG), "sources": dict(CONFIG_SOURCES)}


if __name__ == "__main__":
    for k in sorted(CONFIG):
        print(f"{k}={CONFIG[k]}  ({CONFIG_SOURCES[k]})")
