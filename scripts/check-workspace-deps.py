#!/usr/bin/env python3
"""Every dependency of a workspace member comes from the root [workspace.dependencies].

    scripts/check-workspace-deps.py [ROOT]     (ROOT defaults to this checkout; Python 3.11+)

CLAUDE.md: a crate references a dependency as `dep.workspace = true` and never inlines a
version (or a path or git source) of its own. cargo-deny's `bans.workspace-dependencies` only
catches a crate that two or more members declare separately; a lone inline version passes it.
This reads every member's [dependencies], [dev-dependencies] and [build-dependencies],
target-specific ones included, and lists each entry without `workspace = true`. Exit 1 when
there is one, and 2 when the root lists no member manifest (nothing would be checked).
"""
from pathlib import Path
import sys
import tomllib

SECTIONS = ("dependencies", "dev-dependencies", "build-dependencies")


def inline_entries(manifest):
    """The dependency entries of one parsed member manifest that bypass the workspace."""
    tables = [("", manifest)]
    tables += [(f"target.{cfg}.", table) for cfg, table in manifest.get("target", {}).items()]
    return [
        f"{prefix}{section}.{name}"
        for prefix, table in tables
        for section in SECTIONS
        for name, spec in table.get(section, {}).items()
        if not (isinstance(spec, dict) and spec.get("workspace") is True)
    ]


def members(root):
    """The member manifests the root Cargo.toml lists (globs expanded, `exclude` honoured)."""
    workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
    excluded = {(root / e).resolve() for e in workspace.get("exclude", [])}
    found = []
    for pattern in workspace.get("members", []):
        for path in sorted(root.glob(pattern)):
            if (path / "Cargo.toml").is_file() and path.resolve() not in excluded:
                found.append(path / "Cargo.toml")
    return found


def main(argv):
    root = Path(argv[1]) if len(argv) > 1 else Path(__file__).resolve().parents[1]
    manifests = members(root)
    if not manifests:
        print(f"{root / 'Cargo.toml'} lists no workspace member manifest")
        return 2
    bad = [
        f"{manifest.relative_to(root)}: {entry}"
        for manifest in manifests
        for entry in inline_entries(tomllib.loads(manifest.read_text()))
    ]
    for line in bad:
        print(f"{line} does not use the workspace dependency (`.workspace = true`)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
