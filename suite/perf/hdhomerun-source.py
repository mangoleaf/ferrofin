#!/usr/bin/env python3
"""A fake HDHomeRun device on the compose network, for the parity lab.

An HDHomeRun's server-facing interface is three plain HTTP documents plus a UDP
discovery datagram — there is no proprietary transport a server has to speak to
enumerate and play a channel:

  UDP  :65001            answers the broadcast discovery datagram
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

`ModelNumber` is `HDTC-2US` — a real SiliconDust EXTEND — deliberately:
`DiscoverResponse.SupportsTranscoding` is `ModelNumber.Contains("hdtc")`, and it
is the ONLY input that opens `GetChannelStreamMediaSources`' transcoding fan-out
(HdHomerunHost.cs:339-379). With an `HDHR3-US` here (the first draft) the two
servers agreed on a single `native` media source and five of `GetMediaSource`'s
six profile arms — heavy / internet540 / internet480 / internet360 /
internet240 / mobile, each with its own width, height, bitrate, codec and NAL
length — were never compared against Jellyfin at all. An EXTEND plus
`AllowHWTranscoding` on the tuner host makes both servers emit all seven
sources, so every arm is diffed; `native` is still among them, so nothing is
lost. The device serves the same MPEG-TS whatever `?transcode=` asks for, which
is what a real EXTEND does when a profile is unavailable.

DISCOVERY. `GET /LiveTv/Tuners/Discover` is a UDP broadcast, not an HTTP call:
`HdHomerunHost.DiscoverDevices` (HdHomerunHost.cs:481-518) sends the 20-byte
`HDHOMERUN_TYPE_DISCOVER_REQ` to 255.255.255.255:65001 and accepts any datagram
longer than 13 bytes whose second byte is 3 (the REPLY type), then fetches
`http://{sender ip}/discover.json`. So this fake also listens on UDP 65001 and
answers a real reply frame — the framing is confirmed against upstream's own
verbatim request datagram, whose trailing CRC-32 this module's `frame()`
reproduces byte for byte — and serves its HTTP documents on port 80 as well as
`PORT`, because the address the servers derive from a reply carries no port.
Without that, `Discover` is a both-empty agreement and the `newDevicesOnly`
filter has nothing to filter.

  python3 hdhomerun-source.py [port] [advertised-host]

Runs inside the `hdhomerun-source` compose service (python:3-alpine, stdlib only).
"""
import http.server
import json
import socket
import socketserver
import struct
import sys
import threading
import zlib

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
    "ModelNumber": "HDTC-2US",
    "FirmwareName": "hdhomerun_dvcr_atsc",
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


# ---------------------------------------------------------------------------
# The UDP discovery half.

#: `HDHOMERUN_TYPE_DISCOVER_REQ` / `_RPY`. A server only checks that a datagram
#: is longer than 13 bytes and that its second byte is 3, but answering a REAL
#: frame is what makes this a fake device rather than a shaped blob.
DISCOVER_REQ = 0x0002
DISCOVER_RPY = 0x0003
HD_HOMERUN_PORT = 65001

#: The reply's TLVs: device type (tuner), device id, tuner count. Tag/length/
#: value, the same encoding the request's two wildcard TLVs use.
TAG_DEVICE_TYPE = 0x01
TAG_DEVICE_ID = 0x02
TAG_TUNER_COUNT = 0x2A
DEVICE_TYPE_TUNER = 0x00000001


def frame(packet_type, payload):
    """One HDHomeRun control packet: type, payload length, payload, CRC-32.

    The CRC is the standard IEEE CRC-32 over the header and payload, appended
    LITTLE-endian. Verified against upstream's own hard-coded discovery
    datagram (HdHomerunHost.cs:494): the CRC of its first 16 bytes is
    0x8f7dcc73, which is exactly the `73 cc 7d 8f` those bytes end with.
    """
    header = struct.pack(">HH", packet_type, len(payload)) + payload
    return header + struct.pack("<I", zlib.crc32(header) & 0xFFFFFFFF)


def tlv(tag, value):
    return bytes([tag, len(value)]) + value


DISCOVER_REPLY = frame(DISCOVER_RPY, (
    tlv(TAG_DEVICE_TYPE, struct.pack(">I", DEVICE_TYPE_TUNER))
    + tlv(TAG_DEVICE_ID, bytes.fromhex(DISCOVER["DeviceID"]))
    + tlv(TAG_TUNER_COUNT, bytes([DISCOVER["TunerCount"]]))
))


def serve_discovery():
    """Answer the broadcast discovery datagram, forever.

    A real device replies from its own address, which is the ONLY thing the
    servers take from the reply — they then fetch `http://{that address}/
    discover.json`. Replying to any DISCOVER_REQ (the request's device-type and
    device-id TLVs are the `FFFFFFFF` wildcards) is what a device on the
    segment does.
    """
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    sock.bind(("", HD_HOMERUN_PORT))
    while True:
        try:
            data, sender = sock.recvfrom(8192)
        except OSError:
            continue
        if len(data) >= 4 and struct.unpack(">H", data[:2])[0] == DISCOVER_REQ:
            sock.sendto(DISCOVER_REPLY, sender)


if __name__ == "__main__":
    threading.Thread(target=serve_discovery, daemon=True).start()
    # Port 80 as well as PORT: a discovered device is reached at
    # `"http://" + deviceIP` with no port (HdHomerunHost.cs:511), so the
    # documents must be there too or discovery finds an address it cannot read.
    if PORT != 80:
        bare = Server(("", 80), Device)
        threading.Thread(target=bare.serve_forever, daemon=True).start()
    with Server(("", PORT), Device) as httpd:
        httpd.serve_forever()
