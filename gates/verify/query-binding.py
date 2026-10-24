#!/usr/bin/env python3
"""Check D4 over real HTTP on a disposable server; optionally measure Sessions."""

import argparse
import http.client
import json
import os
from pathlib import Path
import socket
import statistics
import subprocess
import tempfile
import time


def run(binary, baseline, samples):
    """Boot one isolated fixture, authenticate, probe binding and stop the server."""
    with tempfile.TemporaryDirectory(prefix="ferrofin-query-binding-") as directory:
        root = Path(directory)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("FERROFIN_", "OTEL_"))}
        env.update(FERROFIN_ADMIN_USER="admin",
                   FERROFIN_ADMIN_PASSWORD="query-binding-disposable-fixture",
                   FERROFIN_CACHE_DIR=str(root / "cache"), FERROFIN_LOG="error")
        with (root / "server.log").open("w") as log:
            server = subprocess.Popen(
                [str(binary), "--data-dir", str(root / "data"),
                 "--bind", "127.0.0.1", "--port", str(port)],
                env=env, stdout=log, stderr=log,
            )
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            token = None

            def request(path, body=None, authenticated=True):
                headers = {"Authorization": 'MediaBrowser Client="D4", Device="D4", '
                           'DeviceId="probe1", Version="1"'}
                if token and authenticated:
                    headers["Authorization"] += f', Token="{token}"'
                if body is not None:
                    headers["Content-Type"] = "application/json"
                    body = json.dumps(body)
                connection.request("POST" if body is not None else "GET", path,
                                   body=body, headers=headers)
                response = connection.getresponse()
                payload = response.read()
                return response.status, payload

            try:
                deadline = time.monotonic() + 60
                while True:
                    if server.poll() is not None:
                        raise RuntimeError("server exited before becoming ready")
                    try:
                        if request("/System/Info/Public")[0] == 200:
                            break
                    except (OSError, http.client.HTTPException):
                        connection.close()
                    if time.monotonic() > deadline:
                        raise RuntimeError("server did not become ready within 60 seconds")
                    time.sleep(0.1)
                status, payload = request("/Users/AuthenticateByName", {
                    "Username": "admin", "Pw": env["FERROFIN_ADMIN_PASSWORD"]})
                assert status == 200, f"authentication status: {status}"
                auth = json.loads(payload)
                token = auth["AccessToken"]
                user_id = auth["User"]["Id"]
                checks = []

                def check(label, path, expected_status, expected_count=None, **kwargs):
                    status, payload = request(path, **kwargs)
                    passed = status == expected_status
                    count = None
                    if expected_count is not None and status == 200:
                        count = len(json.loads(payload))
                        passed &= count == expected_count
                    # Report labels, never URLs containing authentication tokens.
                    checks.append(dict(case=label, status=status, count=count, passed=passed))
                    if not baseline or label == "single scalar":
                        assert passed, f"{label}: status={status}, count={count}"

                check("single scalar", "/Sessions?deviceId=probe1", 200, 1)
                check("exact duplicate", "/Sessions?deviceId=probe1&deviceId=nope", 200, 1)
                check("case variant duplicate", "/Sessions?deviceid=probe1&DeviceId=nope", 200, 1)
                check("reversed order", "/Sessions?DeviceId=nope&deviceid=probe1", 200, 0)
                check("three occurrences", "/Sessions?DEVICEID=probe1&deviceId=nope&deviceid=third", 200, 1)
                check("encoded key", "/Sessions?%64eviceId=probe1&deviceId=nope", 200, 1)
                check("literal comma", "/Sessions?deviceId=probe1%2Cnope&deviceId=probe1", 200, 0)
                check("valid first integer", "/Sessions?activeWithinSeconds=3600&activeWithinSeconds=bad", 200, 1)
                check("invalid first integer", "/Sessions?activeWithinSeconds=bad&activeWithinSeconds=3600", 400)
                check("valid first boolean", "/Items?recursive=true&recursive=bad", 200)
                check("invalid first boolean", "/Items?recursive=bad&recursive=true", 400)
                check("valid first UUID", f"/Items?userId={user_id}&userId=bad", 200)
                check("invalid first UUID", f"/Items?userId=bad&userId={user_id}", 400)
                check("empty first nullable UUID", f"/Items?userId=&userId={user_id}", 200)
                # AuthorizationContext reads StringValues directly, bypassing
                # scalar model binding: nonempty token values are comma-joined.
                check("query authentication", f"/System/Info?ApiKey={token}", 200,
                      authenticated=False)
                for label, query in [
                    ("duplicate credentials", f"ApiKey={token}&ApiKey=bad"),
                    ("reversed credentials", f"ApiKey=bad&ApiKey={token}"),
                    ("case variant credentials", f"apikey={token}&APIKEY=bad"),
                    ("encoded duplicate credentials", f"%41piKey={token}&ApiKey=bad"),
                    ("identical duplicate credentials", f"ApiKey={token}&ApiKey={token}"),
                ]:
                    check(label, f"/System/Info?{query}", 401, authenticated=False)
                check("encoded credential", f"/System/Info?%41piKey={token}", 200,
                      authenticated=False)
                check("empty credential before valid", f"/System/Info?ApiKey=&APIKEY={token}",
                      200, authenticated=False)
                check("bare credential before valid", f"/System/Info?ApiKey&ApiKey={token}",
                      200, authenticated=False)
                check("only empty credentials", "/System/Info?ApiKey=&APIKEY", 401,
                      authenticated=False)
                check("header token retains precedence", "/System/Info?ApiKey=bad&ApiKey=worse", 200)
                check("collections", "/Items?includeItemTypes=Movie&INCLUDEITEMTYPES=Series"
                      "&genres=News%2CSport&GENRES=Drama", 200)
                # Normalization adds '=' to bare fields. At the URI length
                # limit this must reject the empty bool normally, never panic.
                prefix = "/Items?RECURSIVE&userId&parentId&searchTerm&nameStartsWith&fields&unknown="
                check("URI size limit", prefix + "x" * (65_534 - len(prefix)), 400)

                timings = []
                if samples:
                    # Same canonical query and response on baseline/fixed binaries.
                    path = "/Sessions?deviceId=probe1&activeWithinSeconds=3600"
                    for _ in range(50):
                        assert request(path)[0] == 200
                    for _ in range(samples):
                        start = time.perf_counter_ns()
                        status, payload = request(path)
                        timings.append((time.perf_counter_ns() - start) / 1000)
                        assert status == 200 and len(json.loads(payload)) == 1
                return dict(checks=checks, samples=samples,
                            median_us=statistics.median(timings) if timings else None)
            finally:
                connection.close()
                server.terminate()
                try:
                    server.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--baseline", action="store_true",
                        help="report known baseline divergences without failing")
    parser.add_argument("--samples", type=int, default=0,
                        help="measure this many warmed keep-alive Sessions requests")
    args = parser.parse_args()
    if args.samples < 0:
        parser.error("samples must be nonnegative")
    print(json.dumps(run(args.binary.resolve(), args.baseline, args.samples), indent=2))


if __name__ == "__main__":
    main()
