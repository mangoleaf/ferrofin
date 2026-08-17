#!/usr/bin/env python3
"""Phase-harness bootstrap: bring one server to a scanned, ready state ONCE
(wizard → auth → add libraries → wait for the scan to settle → item pick →
context enrichment), so the per-endpoint phase legs can hit a warm server
without re-scanning. Replaces bootstrap.js.

Writes the ready context to results/raw/<target>-ctx.json — the phase legs
read it instead of re-deriving ids (and instead of the old CAPTURE_CREDS
log-grep channel, which existed only because k6's setup() couldn't reach
handleSummary()).
"""

import argparse
import json
from pathlib import Path

import benchlib

RAW = Path(__file__).resolve().parent / "results" / "raw"


def load_ctx(target):
    """The ctx a prior bootstrap/compare wrote for `target`, or None."""
    try:
        return json.loads((RAW / f"{target}-ctx.json").read_text())
    except OSError:
        return None


def ready_ctx(target, base):
    """A ready context: reuse the one on disk when the server is still up and
    the token works; otherwise provision from scratch and persist it."""
    ctx = load_ctx(target)
    if ctx and benchlib.item_count(base, ctx) >= 0:
        return ctx
    ctx = benchlib.bring_up(base, target)
    benchlib.pick_items(base, ctx)
    benchlib.enrich_context(base, ctx)
    RAW.mkdir(parents=True, exist_ok=True)
    (RAW / f"{target}-ctx.json").write_text(json.dumps(ctx, indent=2) + "\n")
    return ctx


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--target", required=True, choices=["ferrofin", "jellyfin"])
    ap.add_argument("--base", required=True)
    args = ap.parse_args()
    ctx = ready_ctx(args.target, args.base)
    print(f"[{args.target}] ready: {ctx['itemsFound']} items")


if __name__ == "__main__":
    main()
