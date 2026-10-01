#!/usr/bin/env python3
"""Build and validate small synthetic Jellyfin adoption fixtures.

All media and accounts are invented. Jellyfin creates the source state through its
API; each supported release supplies its own expected responses before adoption.
"""

import argparse
from contextlib import contextmanager
import json
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys

from metadata import CheckError
from synthetic_fixture import Api, USERS, seed
from synthetic_media import generate


VERSIONS = (
    ("jellyfin-10.11.8", "10.11.8", None),
    ("jellyfin-10.11.9", "10.11.9", "jellyfin-10.11.8"),
    ("jellyfin-10.11.10", "10.11.10", "jellyfin-10.11.8"),
    ("jellyfin-10.11.11", "10.11.11", "jellyfin-10.11.8"),
    ("jellyfin-12.0", "12.0", "jellyfin-10.11.8"),
    ("jellyfin-12.1-from-10", "12.1", "jellyfin-10.11.8"),
    ("jellyfin-12.1-from-12", "12.1", "jellyfin-12.0"),
)


def recipe():
    directory = Path(__file__).parent
    return hashlib.sha256(b"".join((directory / name).read_bytes() for name in (
        "synthetic.py", "synthetic_media.py", "synthetic_fixture.py", "preservation.py", "user_accounts.py"))).hexdigest()


def save(path, data):
    with open(path, "w") as output:
        os.fchmod(output.fileno(), 0o600)
        json.dump(data, output, indent=2, sort_keys=True)


@contextmanager
def server(image, config, media, name, port, ferrofin=False):
    cache = config.parent / (config.name + "-cache")
    cache.mkdir(exist_ok=True)
    command = ["docker", "run", "-d", "--name", name, "--user", f"{os.getuid()}:{os.getgid()}",
               "-p", f"127.0.0.1:{port}:8096", "-v", f"{config}:/config", "-v", f"{cache}:/cache",
               "-v", f"{media}:/media:ro", "-e", "HTTP_PROXY=http://127.0.0.1:9",
               "-e", "HTTPS_PROXY=http://127.0.0.1:9", "-e", "NO_PROXY=127.0.0.1,localhost"]
    if ferrofin:
        command += ["-e", "FERROFIN_DATA_DIR=/config", "-e", "FERROFIN_CACHE_DIR=/cache", "-e", "FERROFIN_ENABLE_METRICS=true"]
    subprocess.run(command + [image], check=True, stdout=subprocess.DEVNULL)
    try:
        api = Api(f"http://127.0.0.1:{port}")
        api.ready()
        yield api
    finally:
        subprocess.run(["docker", "stop", "-t", "60", name], check=True, stdout=subprocess.DEVNULL)
        with open(config.parent / (config.name + ".server.log"), "w") as output:
            subprocess.run(["docker", "logs", name], stdout=output, stderr=subprocess.STDOUT, check=True)
        subprocess.run(["docker", "rm", name], check=True, stdout=subprocess.DEVNULL)


def build(root, only, port):
    root.mkdir(parents=True, exist_ok=True)
    if not (root / "media").exists():
        generate(root / "media", "jellyfin/jellyfin:10.11.8")
    for name, version, parent in VERSIONS:
        if only and name != only:
            continue
        config = root / name
        if config.exists():
            ready = config / "synthetic-ready.json"
            if ready.exists() and json.loads(ready.read_text()).get("fixture") == name:
                if json.loads(ready.read_text()).get("recipe") != recipe():
                    raise CheckError("synthetic recipe changed; build in a new fixture directory")
                print(name + " already built", flush=True)
                continue
            if not (config / "synthetic.json").exists():
                raise CheckError("incomplete synthetic fixture exists; inspect it before rebuilding")
        elif parent:
            shutil.copytree(root / parent, config)
        else:
            config.mkdir()
        with server("jellyfin/jellyfin:" + version, config, root / "media", "synthetic-builder", port) as api:
            if (config / "synthetic.json").exists():
                manifest = json.loads((config / "synthetic.json").read_text())
                api = api.login(USERS[0])
                api.idle()
                if parent:
                    api.scan(300, interval=1)
            else:
                manifest = seed(api.base)
                api = api.login(USERS[0])
            save(config / "synthetic.json", manifest)
            # Added by the preservation checker; a source-version baseline is
            # required so migrations between Jellyfin versions are not guessed.
            from preservation import snapshot, validate
            baseline = snapshot(api, manifest)
            validate(baseline, manifest)
            save(config / "preservation.json", baseline)
        save(config / "synthetic-ready.json", {"fixture": name, "version": version, "recipe": recipe()})
        print(name + " built and validated", flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("build", "run"))
    parser.add_argument("--fixtures", required=True, type=Path)
    parser.add_argument("--only", choices=[row[0] for row in VERSIONS])
    parser.add_argument("--port", type=int, default=18120)
    parser.add_argument("--image", default="ferrofin:bench")
    args = parser.parse_args(argv)
    os.umask(0o077)
    root = args.fixtures.resolve()
    try:
        if args.mode == "build":
            build(root, args.only, args.port)
        else:
            from preservation import run_matrix
            run_matrix(root, args.image, args.only, args.port)
        return 0
    except (CheckError, OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        # This runner handles invented fixtures only. Keep reports bounded and
        # leave detailed server logs beside the disposable copies.
        print(f"synthetic adoption failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
