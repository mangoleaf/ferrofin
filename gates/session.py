#!/usr/bin/env python3
"""The record of one gates session, `session.json` (stdlib only), which gates/run writes.

    session.py new RUNS --gates G[,G…] --branch B --commit C --dirty true|false --image I --image-id ID
                   --oracle-image O [--previous P --previous-id ID]
        make RUNS/<branch>-<YYYYMMDD-HHMM> (slashes and other characters a directory name should not
        hold become dashes; a rerun in the same minute gets -run2, -run3 …), write its session.json
        with every gate pending, and print the directory
    session.py gate SESSION GATE STATUS [--exit N] [--note TEXT]
        record a gate's status: running stamps its start; passed, failed and skipped its end, with
        the exit status and a note (interrupted, say) when given
    session.py end SESSION
        stamp the session's end; a gate still running is failed (interrupted), one still pending skipped
"""

import json
import os
import re
import sys
from datetime import datetime

GATES = ("verify", "adoption", "parity", "bench")
STATUSES = ("pending", "running", "passed", "failed", "skipped")


def now():
    return datetime.now().astimezone().isoformat(timespec="seconds")


def load(session):
    with open(os.path.join(session, "session.json")) as f:
        return json.load(f)


def save(session, record):
    path = os.path.join(session, "session.json")
    with open(path + ".tmp", "w") as f:
        json.dump(record, f, indent=2)
        f.write("\n")
    os.replace(path + ".tmp", path)   # a reader never sees half a file


def new(runs, gates, branch, commit, dirty, image, image_id, oracle_image, previous=None, previous_id=None,
        when=None):
    """The new session's directory, made with os.mkdir so two sessions never share one."""
    unknown = [g for g in gates if g not in GATES]
    if not gates or unknown:
        raise ValueError(f"unknown gate {', '.join(unknown) or '(none)'}: choose from {', '.join(GATES)}")
    base = f"{re.sub(r'[^A-Za-z0-9._-]+', '-', branch).strip('-.') or 'detached'}-{(when or datetime.now()):%Y%m%d-%H%M}"
    os.makedirs(runs, exist_ok=True)
    n = 1
    while True:
        session = os.path.join(runs, base if n == 1 else f"{base}-run{n}")
        try:
            os.mkdir(session)
            break
        except FileExistsError:
            n += 1
    record = {"name": os.path.basename(session), "branch": branch, "commit": commit, "dirty": dirty,
              "image": {"ref": image, "id": image_id}, "oracle": {"image": oracle_image},
              "started": now(), "ended": None,
              "gates": {g: {"status": "pending", "started": None, "ended": None, "exit": None} for g in gates}}
    if previous:
        record["previous"] = {"ref": previous, "id": previous_id}
    save(session, record)
    return session


def gate(session, name, status, exit_code=None, note=None):
    if status not in STATUSES[1:]:
        raise ValueError(f"unknown status {status}: choose from {', '.join(STATUSES[1:])}")
    record = load(session)
    if name not in record["gates"]:
        raise ValueError(f"{name} is not a gate of this session")
    entry = record["gates"][name]
    entry["status"] = status
    entry["started" if status == "running" else "ended"] = now()
    if exit_code is not None:
        entry["exit"] = exit_code
    if note:
        entry["note"] = note
    save(session, record)


def end(session):
    record = load(session)
    for entry in record["gates"].values():
        if entry["status"] == "running":
            entry.update(status="failed", ended=now(), note=entry.get("note") or "interrupted")
        elif entry["status"] == "pending":
            entry.update(status="skipped", note=entry.get("note") or "not reached")
    record["ended"] = now()
    save(session, record)


def main(argv):
    args, opts = [], {}
    while argv:
        a = argv.pop(0)
        if a.startswith("--"):
            if not argv:
                sys.exit(f"session: {a} needs a value")
            opts[a[2:]] = argv.pop(0)
        else:
            args.append(a)
    try:
        if args[:1] == ["new"] and len(args) == 2:
            print(new(args[1], opts.get("gates", "").split(","), opts["branch"], opts["commit"],
                      opts["dirty"] == "true", opts["image"], opts["image-id"], opts["oracle-image"],
                      opts.get("previous"), opts.get("previous-id")))
        elif args[:1] == ["gate"] and len(args) == 4:
            gate(args[1], args[2], args[3], int(opts["exit"]) if "exit" in opts else None, opts.get("note"))
        elif args[:1] == ["end"] and len(args) == 2:
            end(args[1])
        else:
            sys.exit(__doc__)
    except KeyError as missing:
        sys.exit(f"session: --{missing.args[0]} is required")
    except (OSError, ValueError) as error:
        sys.exit(f"session: {error}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
