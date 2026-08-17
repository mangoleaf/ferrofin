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
    "BENCH_MIN_SAMPLES": 1000,
    "BENCH_MIN_WINDOW_SECS": 5,
    "BENCH_DURATION_SECS": 30,
    "BENCH_RATE": 25,
    "BENCH_RATE_FRACTION": 0.5,
    "BENCH_RATE_MAX": 2000,
    "BENCH_RATE_TOLERANCE": 0.99,
    # warm/cold
    "BENCH_GLOBAL_WARMUP_SECS": 60,
    "BENCH_WARMUP_SECS": 10,
    "BENCH_WARMUP_MIN_CALLS": 30,
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
    """Parse `raw` with the type of the code default → (value, parsed_ok).
    A malformed value falls back to the default (never crash a run) and
    reports parsed_ok=False so the SOURCE can say so — a meta block claiming
    'env' for a value that actually came from the default would lie
    (review finding, round 1)."""
    default = DEFAULTS[key]
    try:
        if isinstance(default, bool):  # pragma: no cover - no bool knobs today
            return raw.lower() in ("1", "true", "yes"), True
        if isinstance(default, int):
            return int(raw), True
        if isinstance(default, float):
            return float(raw), True
        return raw, True
    except ValueError:
        print(f"!! bench config: {key}={raw!r} is not a {type(default).__name__}; using default {default}")
        return default, False


def load():
    """Resolve every knob and remember its source layer."""
    conf = _parse_conf()
    values, sources = {}, {}
    for key, default in DEFAULTS.items():
        # A PRESENT env var wins even when empty — "" is how a caller disables
        # a list knob (e.g. BENCH_COLD_ENDPOINTS="" turns the cold leg off);
        # an empty NUMERIC env var falls back to the default via _cast.
        if key in os.environ:
            raw, layer = os.environ[key], "env"
        elif key in conf:
            raw, layer = conf[key], "bench.conf"
        else:
            values[key], sources[key] = default, "default"
            continue
        values[key], ok = _cast(key, raw)
        sources[key] = layer if ok else f"default (invalid {layer})"
    return values, sources


CONFIG, CONFIG_SOURCES = load()


def resolved_meta():
    """The self-describing block run records carry in meta.bench_config."""
    return {"values": dict(CONFIG), "sources": dict(CONFIG_SOURCES)}


if __name__ == "__main__":
    for k in sorted(CONFIG):
        print(f"{k}={CONFIG[k]}  ({CONFIG_SOURCES[k]})")
