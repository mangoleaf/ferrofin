#!/usr/bin/env python3
"""HTTP access to a server running on a round-trip copy, shared by the stages (stdlib only).

Credentials come from the copy's own database at run time (its newest API key) and are never
written anywhere, as for gates/adoption/smoke.sh. The client is gates/adoption/metadata.py's, which
already waits for a library scan the way the adoption gate does; this adds requests with a body.

    api.py scan   --base-url URL --data-dir DIR [--timeout S]   POST /Library/Refresh, wait until it completed
    api.py settle --base-url URL --data-dir DIR [--timeout S]   wait until no scheduled task is running

Exit status 0 when done, 1 with one line on stdout saying why not.
"""

import argparse
import http.client
import json
from pathlib import Path
import sqlite3
import sys
import time
import urllib.error
import urllib.request

sys.path.append(str(Path(__file__).resolve().parents[1] / "adoption"))
import metadata  # noqa: E402  (gates/adoption/metadata.py: Api, CheckError)

CheckError = metadata.CheckError


def database(data_dir):
    """The copy's jellyfin.db: <data dir>/data/jellyfin.db, the layout Ferrofin and Jellyfin's image share."""
    return Path(data_dir) / "data" / "jellyfin.db"


class Server(metadata.Api):
    """A server on a copy, as its newest API key (an administrator)."""

    def __init__(self, base, data_dir):
        super().__init__(base, database(data_dir))

    def request(self, path, method="GET", image=False, missing_ok=False):
        """metadata.Api.request (the scan and settle polls), with a connection the server drops a CheckError,
        as in send."""
        try:
            return super().request(path, method, image=image, missing_ok=missing_ok)
        except (OSError, http.client.HTTPException) as error:
            raise CheckError(f"{method} {path.split('?')[0]}: no answer ({type(error).__name__})") from None

    def send(self, method, path, body=None, auth=None, content_type="application/json"):
        """(status, decoded JSON or None) for one request; never raises for an HTTP status.
        auth: the Authorization header to send instead of the API key's. A bytes body is sent as it is,
        with content_type (an image upload); any other body as JSON."""
        data = body if isinstance(body, bytes) else None if body is None else json.dumps(body).encode()
        headers = {"Authorization": auth or self.auth}
        if data is not None:
            headers["Content-Type"] = content_type
        request = urllib.request.Request(self.base + path, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                raw = response.read()
                status = response.status
        except urllib.error.HTTPError as error:
            status, raw = error.code, b""
            error.close()
        # URLError and timeouts are OSErrors; a connection the server drops is http.client's own.
        except (OSError, http.client.HTTPException) as error:
            raise CheckError(f"{method} {path.split('?')[0]}: no answer ({type(error).__name__})") from None
        try:
            return status, (json.loads(raw) if raw else None)
        except json.JSONDecodeError:
            return status, None

    def settle(self, timeout, interval=2):
        """Wait until no scheduled task is running (a library scan, a startup task): idle on two polls
        `interval` apart, since a startup task can begin a moment after the server first answers."""
        deadline, idle = time.monotonic() + timeout, 0
        while True:
            tasks = self.request("/ScheduledTasks")
            if not isinstance(tasks, list):
                raise CheckError("the scheduled task list is not a list")
            idle = idle + 1 if all(isinstance(t, dict) and t.get("State") == "Idle" for t in tasks) else 0
            if idle == 2:
                return
            if time.monotonic() >= deadline:
                raise CheckError(f"a scheduled task was still running after {timeout:.0f} s")
            time.sleep(interval)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("mode", choices=("scan", "settle"))
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--data-dir", required=True)
    parser.add_argument("--timeout", type=float, default=600)
    args = parser.parse_args(argv)
    try:
        server = Server(args.base_url, args.data_dir)
        if args.mode == "scan":
            server.scan(args.timeout)
        else:
            server.settle(args.timeout)
    except CheckError as error:
        print(error)
        return 1
    except (OSError, ValueError, sqlite3.Error) as error:
        print(f"cannot reach the server or read the copy: {type(error).__name__}: {error}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
