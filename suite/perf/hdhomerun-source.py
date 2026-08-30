#!/usr/bin/env python3
"""A fake HDHomeRun device on the compose network, for the parity lab.

An HDHomeRun's server-facing interface is three plain HTTP documents plus a UDP
discovery datagram — there is no proprietary transport a server has to speak to
enumerate and play a channel:

  GET /discover.json     the device's identity and where its lineup lives
  GET /lineup_status.json  whether a channel scan is in progress (dashboard only)
  GET /lineup.json       the channels, each with the URL that plays it

So the port of `HdHomerunHost` is verifiable without hardware by answering those
three faithfully, exactly as upstream's own no-hardware oracle does
(`tests/Jellyfin.LiveTv.Tests/HdHomerunHostTests.cs` mocks the HTTP handler and
reads JSON fixtures). This is the lab's differential version of that: BOTH
Ferrofin and Jellyfin point a `hdhomerun` tuner host at this service, so the two
implementations are compared against one device rather than against each other's
mocks.

The channel URLs point at the EXISTING `livetv-source` broadcast
(`http://livetv-source:8000/live.ts`), so a channel that is tuned really plays a
real, endless MPEG-TS — the same source the M3U tuner uses.

`ModelNumber` is `HDHR3-US` deliberately: `DiscoverResponse.SupportsTranscoding`
is `ModelNumber.Contains("hdtc")`, and only the EXTEND (`HDTC-2US`) transcodes.
An HDHR3 therefore offers the native profile alone, which is the shape both
servers must agree on without either of them inventing profile URLs this fake
does not serve.

  python3 hdhomerun-source.py [port] [advertised-host]

Runs inside the `hdhomerun-source` compose service (python:3-alpine, stdlib only).
"""
import http.server
import json
import socketserver
import sys

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8100
# The name the SERVERS reach this device by, which is what must go into the
# self-referential BaseURL/LineupURL: a device advertises its own address.
HOST = sys.argv[2] if len(sys.argv) > 2 else "hdhomerun-source"
BASE = f"http://{HOST}:{PORT}"

# Fixed identity: a parity fixture must be byte-identical for both servers on
# every run, so nothing here is derived from the clock, the container id or the
# interface address.
DISCOVER = {
    "FriendlyName": "Parity HDHomeRun",
    "ModelNumber": "HDHR3-US",
    "FirmwareName": "hdhomerun3_atsc",
    "FirmwareVersion": "20200225",
    "DeviceID": "1040A0A1",
    "DeviceAuth": "parityfixture",
    "TunerCount": 2,
    "BaseURL": BASE,
    "LineupURL": f"{BASE}/lineup.json",
}

# `lineup_status.json` is what the dashboard polls during a channel scan. A
# device that is not scanning reports exactly this.
LINEUP_STATUS = {
    "ScanInProgress": 0,
    "ScanPossible": 1,
    "Source": "Antenna",
    "SourceList": ["Antenna", "Cable"],
}

# Two channels off the shared broadcast, mirroring the M3U fixture's two. `HD`
# and `Favorite` are NUMBERS, not bools — that is the real device's spelling and
# the reason `JsonBoolNumberConverter` exists upstream ("This is needed for
# HDHomerun"), so the fake must not soften it to `true`/`false`.
LINEUP = [
    {
        "GuideNumber": "10.1",
        "GuideName": "Parity HDHR One",
        "VideoCodec": "MPEG2",
        "AudioCodec": "AC3",
        "HD": 1,
        "Favorite": 1,
        "URL": "http://livetv-source:8000/live.ts?ch=hdhr1",
    },
    {
        "GuideNumber": "10.2",
        "GuideName": "Parity HDHR Two",
        "VideoCodec": "MPEG2",
        "AudioCodec": "AC3",
        "URL": "http://livetv-source:8000/live.ts?ch=hdhr2",
    },
]

ROUTES = {
    "/discover.json": DISCOVER,
    "/lineup_status.json": LINEUP_STATUS,
    "/lineup.json": LINEUP,
}


class Device(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):   # noqa: N802 — BaseHTTPRequestHandler's naming
        # A real device ignores the query string on these documents.
        path = self.path.split("?", 1)[0]
        document = ROUTES.get(path)
        if document is None:
            # An HDHR4 answers 404 for /discover.json, which is the branch
            # `HdHomerunHost.GetModelInfo` falls back on; every other unknown
            # path is a 404 on every model.
            self.send_error(404)
            return
        body = json.dumps(document).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):   # quiet: the suite's logs are the servers'
        pass


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    with Server(("", PORT), Device) as httpd:
        httpd.serve_forever()
