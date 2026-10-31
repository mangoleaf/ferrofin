#!/usr/bin/env python3
"""The previous release a release's A/B gate compares against (stdlib only).

    baseline.py check PREVIOUS [--release vX.Y.Z]
        exit 2, naming the image to use, when PREVIOUS is not the release's baseline; 0 when it is,
        or when there is no release to hold it to (an A/B of work in progress may use any image)

A release vX.Y.Z is compared with the latest release of the minor line before it (owner decision
2026-10-29): v1.4.1 and v1.4.2 with the newest v1.3.*, v1.5.0 with the newest v1.4.*, a vX.0.0 with
the newest release of major X-1, never with an earlier release of its own minor line. The release is
`--release`, else the checkout's `release/vX.Y.Z` branch; a patch cut from `main` names it with
`--release`. The baseline is the newest final tag (no `-rc`) of that line in the checkout's tags.
"""

import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
IMAGE = "ghcr.io/mangoleaf/ferrofin"
RELEASE = re.compile(r"v(\d+)\.(\d+)\.(\d+)")


def line_before(release):
    """`v1.3` for v1.4.1, `v1` for v2.0.0; None for a v0.0.x, which has no line before it."""
    major, minor, _ = (int(n) for n in RELEASE.fullmatch(release).groups())
    if minor:
        return f"v{major}.{minor - 1}"
    return f"v{major - 1}" if major else None


def baseline(release, tags):
    """The newest final release tag of the line before `release`, or None."""
    line = line_before(release)
    final = [t for t in tags if RELEASE.fullmatch(t) and line and t.startswith(line + ".")]
    return max(final, key=lambda t: tuple(int(n) for n in RELEASE.fullmatch(t).groups()), default=None)


def tag_of(image):
    """The tag an image reference names: `v1.3.2` for `host:5000/ferrofin:v1.3.2@sha256:…`; '' for none."""
    name = image.split("@", 1)[0]
    last = name.rsplit("/", 1)[-1]
    return last.rsplit(":", 1)[1] if ":" in last else ""


def release_of(branch, release=None):
    """The release being gated: `release` when given, else the branch's `release/vX.Y.Z`; None for
    any other branch. Refuses a `release` that is not vX.Y.Z."""
    if release:
        if not RELEASE.fullmatch(release):
            raise ValueError(f"--release {release} is not a release version (vX.Y.Z)")
        return release
    match = re.fullmatch(r"release/(v\d+\.\d+\.\d+)", branch or "")
    return match.group(1) if match else None


def problem(previous, release, tags):
    """Why `previous` is not `release`'s baseline, or None when it is (or there is no release)."""
    if not release:
        return None
    line, want = line_before(release), baseline(release, tags)
    if not want:
        return f"no {line or 'earlier'} release tag in this checkout to compare release {release} against: git fetch --tags"
    if tag_of(previous) != want:
        return (f"the release A/B gate of {release} compares against {want}, the latest {line} release: "
                f"use {IMAGE}:{want} as the previous image, not {previous}")
    return None


def git(*args):
    """What git prints in the checkout this file is in (wherever the caller was started, and whatever
    repository a git hook's environment points at), or '' where there is no git or no repository (then
    no branch and no tags)."""
    env = {k: v for k, v in os.environ.items() if k not in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR")}
    try:
        return subprocess.run(["git", *args], capture_output=True, text=True, check=False, cwd=ROOT, env=env).stdout
    except OSError:
        return ""


def check(previous, release=None):
    """`problem` for this checkout: its branch and its tags."""
    branch = git("rev-parse", "--abbrev-ref", "HEAD").strip()
    release = release_of(branch, release)
    return problem(previous, release, git("tag", "-l", "v*").split()) if release else None


def main(argv):
    if len(argv) not in (2, 4) or argv[0] != "check" or (len(argv) == 4 and argv[2] != "--release"):
        sys.exit(__doc__)
    try:
        why = check(argv[1], argv[3] if len(argv) == 4 else None)
    except ValueError as err:
        why = str(err)
    if why:
        print(why, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
