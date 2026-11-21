#!/usr/bin/env python3
"""The round-trip report: every route, stage and check of a run, as one self-contained report.html, and the
run's result.json in the envelope every gate writes (gates/result.py), which a gates/run session reads.

run.sh records as it goes with gates/adoption/adoption_report.py (start, begin, check, row: one JSON line
per event in WORK/results.jsonl, a route being a fixture row of its kind, path, messy or scale), and builds
both with

    report.py build WORK        write WORK/report.html and WORK/result.json; prints the page's path

the adoption report's page and result, as the roundtrip gate's. Stdlib only.
"""

from pathlib import Path
import sys

sys.path.append(str(Path(__file__).resolve().parents[1] / "adoption"))
import adoption_report  # noqa: E402  (gates/adoption/adoption_report.py: build)


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 2 or argv[0] != "build":
        print("usage: report.py build WORK", file=sys.stderr)
        return 2
    try:
        print(adoption_report.build(argv[1], gate="roundtrip"))
    except (OSError, ValueError, KeyError) as error:
        print(f"round-trip report: not written ({error})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
