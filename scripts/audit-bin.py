#!/usr/bin/env python3
"""`cargo audit bin` on release binaries, held to the rules `cargo deny check` applies.

    scripts/audit-bin.py BINARY...      (needs cargo-audit on PATH; Python 3.11+)

The release workflow runs this on the image's binary and on every archive and package before
it publishes them. deny.toml's [advisories] ignores are passed through, and its unmaintained
and unsound notices fail here as they fail cargo-deny (any scope but `none` denies them, since
cargo-audit has no scopes; a yanked crate only warns in both), so the two gates agree.

Each binary must carry the crate list `cargo auditable build` embeds. Without it cargo-audit
only warns and guesses the crates from panic messages; this fails instead. Exit 1 on a
vulnerability, a denied notice or a missing record, 2 when no binary is named.
"""
from pathlib import Path
import subprocess
import sys
import tomllib

DENY_TOML = Path(__file__).resolve().parents[1] / "deny.toml"


def audit_command(advisories, binaries):
    """The cargo-audit command line for `binaries` under deny.toml's [advisories] table."""
    # cargo-audit itself, not `cargo audit`: no Rust toolchain needed where the image is audited.
    cmd = ["cargo-audit", "audit", "--color", "never"]
    for kind in ("unmaintained", "unsound"):
        if advisories.get(kind, "all") != "none":  # cargo-deny's default is "all"
            cmd += ["--deny", kind]
    for entry in advisories.get("ignore", []):
        advisory = entry if isinstance(entry, str) else entry.get("id")
        if not advisory:
            sys.exit(f"deny.toml: cargo-audit can only ignore an advisory id, not {entry}")
        cmd += ["--ignore", advisory]
    # Ferrofin's binaries are near cargo-audit's 100 MiB read limit and are ours, so no limit:
    # u64::MAX, because 0.22.2 enforces the "0 for unlimited" its help documents as 0 bytes.
    return cmd + ["bin", "--max-binary-size", str(2**64 - 1), *binaries]


def main(binaries):
    """Audit `binaries`; the exit status (see the module docstring)."""
    if not binaries:
        print(__doc__, file=sys.stderr)
        return 2
    advisories = tomllib.loads(DENY_TOML.read_text())["advisories"]
    run = subprocess.run(audit_command(advisories, binaries), stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, text=True, check=False)
    print(run.stdout, end="", flush=True)
    # cargo-audit's status line for a binary with a record (src/presenter.rs in 0.22.2).
    missing = [b for b in binaries if f"'cargo auditable' data in {b} (" not in run.stdout]
    for b in missing:
        print(f"{b}: no cargo-auditable record; build it with `cargo auditable build`",
              file=sys.stderr)
    return 1 if run.returncode or missing else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
