#!/usr/bin/env python3
"""Jellyfin's boot on a round-trip copy, read from its console log (stdlib only).

    checks_boot.py migrations LOG   a line per migration Jellyfin began and never finished, with the error it
                                    died of (the SQLite error when there is one)
    checks_boot.py errors LOG       ERR and FTL entries outside ALLOWED: how many, and the first few

Exit status 0 when there is nothing to say, 1 with the reasons on stdout, 2 when the log cannot be read or
holds no entry at all (an empty capture, or a format this no longer parses, must never pass).
The log is the container's output: entries such as `[07:53:39] [INF] [10] Main: Jellyfin version: 12.4.0`,
a multi-line entry continuing on the lines that do not start one.
"""

import re
import sys

ENTRY = re.compile(r"^\[\d\d:\d\d:\d\d\] \[(VRB|DBG|INF|WRN|ERR|FTL)\] \[\d+\] ([^:]+): ?(.*)$")
BEGUN = re.compile(r"^Perform migration (\S+)$")
DONE = re.compile(r"^Migration (\S+) was successfully applied$")
SQLITE = re.compile(r"SQLite Error \d+: '[^\n]*?'")
#: The errors a round trip expects, each with why: (pattern on "<source>: <first line>", reason). Only what a
#: live run has logged is here: a provider call behind the proxy gets an entry once one is seen, not before.
ALLOWED = (
    (re.compile(r"^Emby\.Server\.Implementations\.Updates\.InstallationManager: An error occurred while accessing "
                r"the plugin manifest"), "the plugin repository, behind the unreachable proxy"),
)
#: How many unexpected errors a reason quotes before it only counts the rest.
MAX_SHOWN = 3


def entries(text):
    """[(level, source, first line, whole text)] in log order."""
    out = []
    for line in text.splitlines():
        match = ENTRY.match(line)
        if match:
            out.append([match[1], match[2], match[3], match[3]])
        elif out:
            out[-1][3] += "\n" + line
    return [tuple(entry) for entry in out]


def migrations(log):
    found, begun = entries(log), {}
    for index, (_, _, first, _) in enumerate(found):
        if match := BEGUN.match(first):
            begun[match[1]] = index
        elif match := DONE.match(first):
            begun.pop(match[1], None)
    reasons = []
    for migration, index in begun.items():
        after = [entry for entry in found[index + 1:] if entry[0] in ("ERR", "FTL")]
        sqlite = next((match[0] for entry in after if (match := SQLITE.search(entry[3]))), None)
        cause = sqlite or (f"{after[0][1]}: {after[0][2]}" if after else "no error logged after it began")
        reasons.append(f"migration {migration} failed: {cause}")
    return reasons


def errors(log):
    unexpected = [f"[{level}] {source}: {first}" for level, source, first, _ in entries(log)
                  if level in ("ERR", "FTL") and not any(p.match(f"{source}: {first}") for p, _ in ALLOWED)]
    if not unexpected:
        return []
    more = f"; and {len(unexpected) - MAX_SHOWN} more" if len(unexpected) > MAX_SHOWN else ""
    return [f"{len(unexpected)} unexpected error(s): {'; '.join(e[:200] for e in unexpected[:MAX_SHOWN])}{more}"]


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 2 or argv[0] not in ("migrations", "errors"):
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    try:
        with open(argv[1], encoding="utf-8", errors="replace") as handle:
            log = handle.read()
    except OSError as error:
        print(f"cannot read the Jellyfin log: {error}")
        return 2
    if not entries(log):
        print(f"no Jellyfin log entry in {argv[1]}")
        return 2
    reasons = migrations(log) if argv[0] == "migrations" else errors(log)
    for reason in reasons:
        print(reason)
    return int(bool(reasons))


if __name__ == "__main__":
    sys.exit(main())
