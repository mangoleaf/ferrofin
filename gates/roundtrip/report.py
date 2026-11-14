#!/usr/bin/env python3
"""The round-trip report: every route, stage and check of a run, as one self-contained report.html.

run.sh records as it goes with gates/adoption/adoption_report.py (start, begin, check, row: one JSON line
per event in WORK/results.jsonl, a route being a fixture row of kind "path"), and builds the page with

    report.py build WORK        write WORK/report.html; prints its path

the adoption report's page, without its result.json: the round trip is not a gate of gates/run yet, so it
writes no result in the gates' envelope. Stdlib only.
"""

from pathlib import Path
import sys
import time

sys.path.append(str(Path(__file__).resolve().parents[1] / "adoption"))
import adoption_report  # noqa: E402  (gates/adoption/adoption_report.py: load, render)


def build(work):
    run, rows, skipped = adoption_report.load(work)
    path = Path(work) / adoption_report.REPORT
    path.write_text(adoption_report.render(run, rows, skipped, finished=time.time()), encoding="utf-8")
    return path


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 2 or argv[0] != "build":
        print("usage: report.py build WORK", file=sys.stderr)
        return 2
    try:
        print(build(argv[1]))
    except (OSError, ValueError, KeyError) as error:
        print(f"round-trip report: not written ({error})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
