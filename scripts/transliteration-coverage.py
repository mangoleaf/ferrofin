#!/usr/bin/env python3
"""How much of Jellyfin's xUnit data Ferrofin has transliterated, per upstream test project.

    FERROFIN_UPSTREAM_DIR=<jellyfin checkout at UPSTREAM_COMMIT> scripts/transliteration-coverage.py

For each upstream test project (`tests/<Project>/`), it prints the Ferrofin rstest `#[case]`s
that cite one of that project's tests next to the project's `[InlineData]` count. A Rust test
cites an upstream test when the comment and attribute block attached to it names an upstream
test class (`LocalizationManagerTests`) or `[Fact]`/`[Theory]` method
(`GetRatingLevel_GivenValidString_Success`), or when its own name is that method in
snake_case. A comment over a group of tests counts only for the test it is attached to.

Skips with a message, exit 0, when FERROFIN_UPSTREAM_DIR is unset or the checkout is not at
UPSTREAM_COMMIT (crates/ferrofin-api/src/handlers.rs), as contract_superset does.
"""
from collections import Counter
from pathlib import Path
import os
import re
import subprocess
import sys

REPO = Path(__file__).resolve().parents[1]
TEST_ATTR = re.compile(r"\[(Fact|Theory)\b")
CS_METHOD = re.compile(r"^\s*public\s.*?(\w+)\s*\(")
RUST_FN = re.compile(r"^\s*(?:pub(?:\(\w+\))?\s+)?(?:async\s+)?fn\s+(\w+)")
WORD = re.compile(r"\w+")


def squash(name):
    """`GetX_Y` and `get_x_y` compare equal."""
    return name.replace("_", "").lower()


def upstream_index(checkout):
    """Map every upstream test class and test method name to its project, and count each
    project's `[InlineData]`."""
    # ponytail: a name two projects define counts for the first (sorted); attribute by the
    # citing file's own `*Tests.cs` mention if a collision ever skews a row.
    names, inline = {}, Counter()
    for project in sorted(p for p in (checkout / "tests").iterdir() if p.is_dir()):
        inline[project.name] += 0
        for cs in sorted(project.rglob("*.cs")):
            if cs.stem.endswith("Tests"):
                names.setdefault(cs.stem, project.name)
            armed = False
            for line in cs.read_text(encoding="utf-8", errors="replace").splitlines():
                if line.lstrip().startswith("//"):
                    continue
                inline[project.name] += line.count("[InlineData")
                if TEST_ATTR.search(line):
                    armed = True
                elif armed and (m := CS_METHOD.match(line)):
                    names.setdefault(m.group(1), project.name)
                    armed = False
    return names, inline


def attached_block(lines, fn_index):
    """The comment and attribute lines directly above line `fn_index`, up to a blank line or
    the end of the previous item."""
    block = []
    for line in reversed(lines[:fn_index]):
        s = line.strip()
        if not s or s in ("}", "};") or s.endswith("{"):
            break
        block.append(line)
    return block


def rust_cases(src_roots, names):
    """Count, per upstream project, the `#[case]`s of Rust tests that cite an upstream test."""
    by_squashed = {squash(n): p for n, p in names.items()}
    counts = Counter()
    for root in src_roots:
        for rs in sorted(root.rglob("*.rs")):
            lines = rs.read_text(encoding="utf-8", errors="replace").splitlines()
            for i, line in enumerate(lines):
                if not (m := RUST_FN.match(line)):
                    continue
                block = attached_block(lines, i)
                cases = sum(b.count("#[case") for b in block)
                if not cases:
                    continue
                words = WORD.findall("\n".join(reversed(block)))
                project = by_squashed.get(squash(m.group(1))) or next(
                    (names[w] for w in words if w in names), None)
                if project:
                    counts[project] += cases
    return counts


def report(inline, counts):
    """The per-project table, as Markdown lines."""
    out = ["| upstream test project | transliterated `#[case]`s | upstream `[InlineData]` |",
           "|---|---:|---:|"]
    for project in sorted(inline):
        out.append(f"| {project} | {counts[project]} | {inline[project]} |")
    out.append(f"| **total** | **{sum(counts.values())}** | **{sum(inline.values())}** |")
    return out


def upstream_commit():
    """UPSTREAM_COMMIT from the Rust source."""
    src = (REPO / "crates/ferrofin-api/src/handlers.rs").read_text(encoding="utf-8")
    m = re.search(r'pub const UPSTREAM_COMMIT: &str = "([0-9a-f]+)";', src)
    if not m:
        sys.exit("UPSTREAM_COMMIT not found in crates/ferrofin-api/src/handlers.rs")
    return m.group(1)


def main():
    pin = upstream_commit()
    dir_ = os.environ.get("FERROFIN_UPSTREAM_DIR")
    if not dir_:
        print(f"SKIP: set FERROFIN_UPSTREAM_DIR to a jellyfin checkout at {pin} to report "
              "transliteration coverage")
        return
    checkout = Path(dir_)
    head = subprocess.run(["git", "-C", str(checkout), "rev-parse", "HEAD"],
                          capture_output=True, text=True, check=False).stdout.strip()
    if not head.startswith(pin):
        print(f"SKIP: FERROFIN_UPSTREAM_DIR {checkout} is at {head!r}, not UPSTREAM_COMMIT {pin}")
        return
    names, inline = upstream_index(checkout)
    counts = rust_cases([REPO / "crates", REPO / "apps"], names)
    print(f"Transliteration coverage against jellyfin {pin}:\n")
    print("\n".join(report(inline, counts)))


if __name__ == "__main__":
    main()
