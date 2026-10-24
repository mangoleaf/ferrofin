#!/usr/bin/env python3
"""The Jellyfin oracle pin, shared by the benchmark and the parity harness (stdlib only).

    oracle.py check [--gate]
        print the oracle as JSON ({"tag", "image"}) and check it against the latest Jellyfin
        release. A pin behind it, or a lookup that failed, warns on stderr; with --gate (a
        release-gate run) it refuses instead and exits 1. Exits 2 when the pin cannot be read.
    oracle.py matches VERSION
        exit 1 unless VERSION (what the pinned image's Jellyfin reports) is UPSTREAM_TAG's release

The oracle is the Jellyfin release named by `UPSTREAM_TAG` in the Rust source (always the latest
Jellyfin release), pinned by digest in `testdata/jellyfin-image.txt`. When a newer release is
out, the pin is stale: moving it means moving `UPSTREAM_TAG`, pinning that release's image and
preparing the corpus again (`testdata/build.sh --prepare-jellyfin`).
"""

import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
UPSTREAM_SOURCE = "crates/ferrofin-api/src/handlers/mod.rs"
PIN = "bench/testdata/jellyfin-image.txt"
REPO = "https://github.com/jellyfin/jellyfin"
#: A release tag; release candidates (`v12.0-rc7`) and anything else do not match.
RELEASE = re.compile(r"v\d+(\.\d+)*")


def upstream_tag(root=ROOT):
    with open(os.path.join(root, UPSTREAM_SOURCE)) as f:
        found = re.search(r'pub const UPSTREAM_TAG: &str = "([^"]+)";', f.read())
    if not found:
        raise ValueError(f"no `pub const UPSTREAM_TAG: &str = \"…\";` in {UPSTREAM_SOURCE}")
    return found.group(1)


def pinned_image(root=ROOT):
    with open(os.path.join(root, PIN)) as f:
        return f.read().strip()


def version(tag):
    """`v12.2` → (12, 2); trailing zeros dropped, so v12.2 and v12.2.0 are the same release."""
    parts = [int(p) for p in tag.lstrip("v").split(".")]
    while len(parts) > 1 and parts[-1] == 0:
        parts.pop()
    return tuple(parts)


def release_tags(listing):
    """The release tags in `git ls-remote --tags` output, without peeled `^{}` refs."""
    tags = (line.split("refs/tags/", 1)[-1] for line in listing.splitlines() if "refs/tags/" in line)
    return [t for t in tags if RELEASE.fullmatch(t)]


def latest(tags):
    return max(tags, key=version) if tags else None


def ls_remote():
    # ponytail: reuses run.sh's HTTP_TIMEOUT_S (default 10 s) rather than a knob of its own.
    timeout = float(os.environ.get("HTTP_TIMEOUT_S", "10"))
    return subprocess.run(["git", "ls-remote", "--tags", REPO], check=True, capture_output=True,
                          text=True, timeout=timeout).stdout


def check(root=ROOT, ls_remote=ls_remote):
    """`(oracle, problem)`: the pinned oracle, and why it cannot be trusted to be the latest
    Jellyfin release (None when it is)."""
    info = {"tag": upstream_tag(root), "image": pinned_image(root)}
    try:
        newest = latest(release_tags(ls_remote()))
    except (OSError, subprocess.SubprocessError) as err:
        detail = (getattr(err, "stderr", None) or "").strip()
        return info, f"cannot look up the latest Jellyfin release ({err}{': ' + detail if detail else ''})"
    if newest is None:
        return info, f"no Jellyfin release tags found at {REPO}"
    if version(newest) > version(info["tag"]):
        return info, (f"Jellyfin {newest} is out; the oracle pin is {info['tag']}. Move UPSTREAM_TAG, "
                      f"pin {newest}'s image in {PIN} and prepare the corpus again")
    return info, None


def main(argv):
    if argv[:1] == ["matches"] and len(argv) == 2:
        # The pinned digest's release is only known once it runs: what Jellyfin reports.
        tag = upstream_tag()
        if RELEASE.fullmatch("v" + argv[1].lstrip("v")) and version(argv[1]) == version(tag):
            return 0
        print(f"oracle: the pinned image {pinned_image()} reports Jellyfin {argv[1]}, not the oracle "
              f"{tag}: pin {tag}'s image in {PIN} and prepare the corpus again", file=sys.stderr)
        return 1
    if not argv or argv[0] != "check" or set(argv[1:]) - {"--gate"}:
        sys.exit(__doc__)
    gate = "--gate" in argv
    try:
        info, problem = check()
    except (OSError, ValueError) as err:
        print(f"oracle: {err}", file=sys.stderr)
        return 2
    print(json.dumps(info))
    if problem:
        print(f"oracle: {'refusing' if gate else 'warning'}: {problem}", file=sys.stderr)
        return 1 if gate else 0
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
