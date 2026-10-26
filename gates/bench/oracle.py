#!/usr/bin/env python3
"""The Jellyfin oracle pin, shared by the benchmark and the parity harness (stdlib only).

    oracle.py check
        print the oracle as JSON ({"tag", "image"}, and "note" when a newer Jellyfin release is
        out or the latest cannot be looked up; the note is also printed on stderr). A newer
        release never refuses a run, a release gate's included. Exits 2 when the pin cannot be
        read: no UPSTREAM_TAG, one that is not a release tag, or no pinned image.
    oracle.py matches VERSION
        exit 1 unless VERSION (what the pinned image's Jellyfin reports) is UPSTREAM_TAG's release

The oracle is the Jellyfin release named by `UPSTREAM_TAG` in the Rust source, pinned by digest
in `testdata/jellyfin-image.txt`. It is the latest Jellyfin release when a release branch is cut,
and the branch keeps it: a release is measured against the Jellyfin release its branch pins, and
the next release branch moves to the newest one (owner decision 2026-10-25). So a newer release
is noted, and recorded wherever the oracle is, never refused. Moving the pin means moving
`UPSTREAM_TAG`, pinning that release's image and preparing the corpus again
(`testdata/build.sh --prepare-jellyfin`).
"""

import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
UPSTREAM_SOURCE = "crates/ferrofin-api/src/handlers/mod.rs"
PIN = "gates/bench/testdata/jellyfin-image.txt"
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
    """The pinned oracle, `{"tag", "image"}`, with a `note` when a newer Jellyfin release is out
    or the latest cannot be looked up: what a run records beside the oracle it was measured
    against, never a reason to refuse it. Raises OSError or ValueError when the pin cannot be
    read, before anything is looked up."""
    tag, image = upstream_tag(root), pinned_image(root)
    if not RELEASE.fullmatch(tag):
        raise ValueError(f"UPSTREAM_TAG {tag} in {UPSTREAM_SOURCE} is not a Jellyfin release tag")
    if not image:
        raise ValueError(f"{PIN} is empty")
    info = {"tag": tag, "image": image}
    pinned = f"measured against Jellyfin {tag}, the release this branch pins"
    try:   # ValueError: an HTTP_TIMEOUT_S that is not a number, a lookup problem like the others
        newest = latest(release_tags(ls_remote()))
    except (OSError, ValueError, subprocess.SubprocessError) as err:
        detail = (getattr(err, "stderr", None) or "").strip()
        info["note"] = (f"{pinned}; the latest Jellyfin release could not be looked up "
                        f"({err}{': ' + detail if detail else ''})")
        return info
    if newest is None:
        info["note"] = f"{pinned}; no Jellyfin release tags found at {REPO}"
    elif version(newest) > version(tag):
        info["note"] = f"{pinned}; {newest} is out"
    return info


def main(argv):
    if argv[:1] == ["matches"] and len(argv) == 2:
        # The pinned digest's release is only known once it runs: what Jellyfin reports.
        tag = upstream_tag()
        if RELEASE.fullmatch("v" + argv[1].lstrip("v")) and version(argv[1]) == version(tag):
            return 0
        print(f"oracle: the pinned image {pinned_image()} reports Jellyfin {argv[1]}, not the oracle "
              f"{tag}: pin {tag}'s image in {PIN} and prepare the corpus again", file=sys.stderr)
        return 1
    if argv != ["check"]:
        sys.exit(__doc__)
    try:
        info = check()
    except (OSError, ValueError) as err:
        print(f"oracle: cannot read the pin: {err}", file=sys.stderr)
        return 2
    print(json.dumps(info))
    if "note" in info:
        print(f"oracle: {info['note']}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
