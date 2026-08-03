#!/usr/bin/env python3
"""suite/migrate-history.py — one-shot: wrap the old bench-data.json runs into the merged shape.

Pre-merge benchmark runs have no parity status and no fixture hash, so they can never be a fair
speed comparison. We keep them VISIBLE in the trend as a greyed "legacy" era (legacy:true,
everything comparable:false) rather than deleting history or pretending it's comparable. Do NOT
retro-compute parity for old runs — that's the point of the flag.

Run once: python3 suite/migrate-history.py   (idempotent — replaces any prior legacy entries).
"""
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "suite" / "results"


def variant_to_op():
    reg = json.loads((ROOT / "suite" / "registry.json").read_text())["operations"]
    return {v["id"]: (e["op"], e["tag"]) for e in reg for v in e["variants"]}


def main():
    v2op = variant_to_op()
    bench = json.loads((ROOT / "benchmark" / "bench-data.json").read_text())

    legacy = []
    for ver in bench.get("versions", []):
        ops = []
        for e in ver.get("endpoints", []):
            op, tag = v2op.get(e["name"], (f"GET ?/{e['name']}", "_legacy"))
            ops.append({
                "op": op, "tag": tag,
                "parity": {"depth": None, "deep_verified": False, "classification": None},
                "perf": {"variant": e["name"],
                         "h_p50": e.get("h_p50"), "h_p95": e.get("h_p95"), "h_p99": e.get("h_p99"),
                         "h_rps": e.get("h_rps"), "h_ok": e.get("h_ok"),
                         "j_p50": e.get("j_p50"), "j_p95": e.get("j_p95"), "j_p99": e.get("j_p99"),
                         "j_rps": e.get("j_rps"), "j_ok": e.get("j_ok"),
                         "speedup": e.get("speedup"), "win_all_three": False, "tail_loss": False,
                         "comparable": False, "reason": "legacy: pre-merge run, no parity/fixture hash"},
            })
        legacy.append({
            "meta": {"hermit": ver.get("hermit", "?"), "hermit_sha": ver.get("hermit", "?"),
                     "jellyfin_image": ver.get("jellyfin"), "fixture_hash": None,
                     "cpus": None, "mem": None, "load": {"vus": None, "duration": None},
                     "perf_source": "legacy", "when": ver.get("when"), "legacy": True},
            "headline": {"comparable_rows": 0, "median_speedup": None, "win_rate": None,
                         "tail_losses": [], "parity_coverage": None},
            "operations": ops,
        })

    RESULTS.mkdir(parents=True, exist_ok=True)
    runs_file = RESULTS / "runs.json"
    existing = json.loads(runs_file.read_text())["runs"] if runs_file.exists() else []
    live = [r for r in existing if not r["meta"].get("legacy")]
    runs_file.write_text(json.dumps({"runs": legacy + live}, indent=2) + "\n")
    print(f">> migrated {len(legacy)} legacy runs (greyed) + kept {len(live)} live → {runs_file.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
