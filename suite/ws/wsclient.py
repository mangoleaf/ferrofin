#!/usr/bin/env python3
"""Minimal stdlib RFC6455 WebSocket client + Jellyfin-auth HTTP helper.

The repo has no WebSocket client anywhere, so every server->client push
(remote control, SyncPlay) is untested past the "a fake bus recorded a string"
level. This is the smallest thing that can watch a real socket.

ponytail: text frames only, no fragmentation, no permessage-deflate. Ferrofin
sends one JSON text frame per message; add reassembly if that changes.
"""

import base64
import json
import os
import socket
import struct
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = os.environ.get("FERROFIN_BASE", "http://127.0.0.1:8096").rstrip("/")


def hostport(base):
    """`(host, port)` for one base URL.

    Resolved per call rather than once at import: the two-server push
    differential (suite/parity/push.py) drives Ferrofin AND Jellyfin from a
    single process, which a module-global HOST/PORT made structurally
    impossible.
    """
    parsed = urllib.parse.urlparse(base)
    if parsed.scheme not in ("http", ""):
        # create_connection speaks plain TCP; an https base would fail the
        # handshake with a confusing error rather than an honest one.
        raise SystemExit(f"base must be http:// (got {base!r}) — TLS is not supported")
    return parsed.hostname or "127.0.0.1", parsed.port or 80


HOST, PORT = hostport(BASE)


def auth_header(token=None, client="Probe", device="Probe", device_id="probe", version="1"):
    parts = [
        f'Client="{client}"',
        f'Device="{device}"',
        f'DeviceId="{device_id}"',
        f'Version="{version}"',
    ]
    if token:
        parts.append(f'Token="{token}"')
    return "MediaBrowser " + ", ".join(parts)


def http(method, path, token=None, body=None, base=None, **ident):
    """Returns (status, parsed-json-or-bytes). `base` defaults to the module BASE."""
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request((base or BASE) + path, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    req.add_header("Authorization", auth_header(token, **ident))
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            raw = r.read()
            ctype = r.headers.get("Content-Type", "")
            if raw and "json" in ctype:
                return r.status, json.loads(raw)
            return r.status, raw
    except urllib.error.HTTPError as e:
        return e.code, e.read()[:500]


#: Frames that belong to the socket's own keep-alive protocol rather than to any
#: operation: Jellyfin's `ForceKeepAlive` prompt and either server's `KeepAlive`
#: acknowledgement.
LIFECYCLE_MESSAGES = frozenset({"ForceKeepAlive", "KeepAlive"})


class WS:
    """One open WebSocket, pumping received JSON messages into a list."""

    def __init__(self, path, base=None):
        host, port = hostport(base) if base else (HOST, PORT)
        self.base = base or BASE
        self.sock = socket.create_connection((host, port), timeout=15)
        key = base64.b64encode(os.urandom(16)).decode()
        handshake = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(handshake.encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError("connection closed during handshake")
            buf += chunk
        # The 15 s connect/handshake timeout must NOT survive into the read
        # loop. A pushed message is an event, not a response: a socket that is
        # simply quiet for 15 s is normal, but a timed-out `recv` raises out of
        # the pump thread, which then exits and takes every LATER message with
        # it. That failure is silent and it reads as agreement — both servers
        # "pushed nothing" — which is the worst possible way for a differential
        # to fail. Blocking reads, and `closed` says when the peer really went.
        self.sock.settimeout(None)
        head, self.buf = buf.split(b"\r\n\r\n", 1)
        status_line = head.split(b"\r\n")[0].decode(errors="replace")
        if "101" not in status_line:
            raise RuntimeError(f"upgrade refused: {status_line} :: {head[:300]!r}")
        self.msgs = []
        # Socket-lifecycle frames are kept apart from operation output: they are
        # driven by an inactivity timer, not by anything a probe asked for, so
        # letting one land inside a `collect()` window would read as "one server
        # pushed an extra message". They are still handed to the caller — see
        # `drain_lifecycle` — never dropped.
        self.lifecycle = []
        self.closed = False
        self.lock = threading.Lock()
        # The pump thread answers pings while the main thread sends — two
        # concurrent sendall()s would interleave header/mask/payload and desync
        # the stream, which surfaces later as an unrelated probe failure.
        self.write_lock = threading.Lock()
        threading.Thread(target=self._pump, daemon=True).start()

    def _read(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def _pump(self):
        try:
            while True:
                b1, b2 = self._read(2)
                opcode = b1 & 0x0F
                length = b2 & 0x7F
                if length == 126:
                    length = struct.unpack(">H", self._read(2))[0]
                elif length == 127:
                    length = struct.unpack(">Q", self._read(8))[0]
                mask = self._read(4) if b2 & 0x80 else None
                payload = self._read(length) if length else b""
                if mask:
                    payload = bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
                if opcode == 0x1:
                    try:
                        msg = json.loads(payload.decode())
                    except ValueError:
                        msg = {"MessageType": "<unparseable>", "Raw": payload.decode(errors="replace")}
                    if msg.get("MessageType") == "ForceKeepAlive":
                        # ANSWER IT. `SessionWebSocketListener` sends this after
                        # `WebSocketLostTimeout * 0.75` of silence and closes the
                        # socket at the full timeout unless the client replies
                        # (SessionWebSocketListener.cs:160-230). A probe that
                        # never replies simply goes deaf partway through a long
                        # run, and every later leg reads as "neither server
                        # pushed anything" — a false agreement, which is worse
                        # than a failure.
                        try:
                            self.send_json({"MessageType": "KeepAlive"})
                        except Exception:
                            pass
                    with self.lock:
                        if msg.get("MessageType") in LIFECYCLE_MESSAGES:
                            self.lifecycle.append(msg)
                        else:
                            self.msgs.append(msg)
                elif opcode == 0x9:
                    self._frame(payload, 0xA)
                elif opcode == 0x8:
                    break
        except Exception:
            pass
        self.closed = True

    def _frame(self, payload, opcode=0x1):
        mask = os.urandom(4)
        masked = bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
        header = bytes([0x80 | opcode])
        n = len(payload)
        if n < 126:
            header += bytes([0x80 | n])
        elif n < 1 << 16:
            header += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            header += bytes([0x80 | 127]) + struct.pack(">Q", n)
        with self.write_lock:
            self.sock.sendall(header + mask + masked)

    def send_json(self, obj):
        self._frame(json.dumps(obj).encode())

    def wait(self, message_type, timeout=5.0, predicate=None):
        """Waits for (and returns) a message of that type, else None."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                for m in self.msgs:
                    if m.get("MessageType") == message_type and (predicate is None or predicate(m)):
                        return m
            time.sleep(0.05)
        return None

    def collect(self, quiet=0.75, timeout=5.0):
        """Every message that arrives, bounded twice; an empty list is a real answer.

        `wait()` answers "did message X show up?" and cannot express "Jellyfin
        pushed two messages and Ferrofin pushed one" — exactly the class of defect
        a push differential exists to catch. So this returns the whole arrival SET.

        The wait is bounded two ways. Once at least one message has arrived, it
        returns as soon as `quiet` seconds pass with no new frame — so a burst is
        collected whole, not truncated at the first one. If NOTHING arrives it
        waits the full `timeout` before saying so, because "nothing arrived" is a
        verdict the probe reports as a difference and must not be a race.
        """
        deadline = time.time() + timeout
        seen = 0
        last_growth = None
        while time.time() < deadline:
            with self.lock:
                n = len(self.msgs)
            if n > seen:
                seen, last_growth = n, time.time()
            elif last_growth is not None and time.time() - last_growth >= quiet:
                break
            time.sleep(0.05)
        return self.drain()

    def types(self):
        with self.lock:
            return [m.get("MessageType") for m in self.msgs]

    def drain(self):
        with self.lock:
            out, self.msgs = list(self.msgs), []
        return out

    def drain_lifecycle(self):
        """The socket-lifecycle frames received since the last call.

        Kept out of `msgs` so they cannot be mistaken for an operation's output,
        and handed back here so a caller can still REPORT them — the counts
        differ between the two servers, and that difference is a real finding.
        """
        with self.lock:
            out, self.lifecycle = list(self.lifecycle), []
        return out

    def close(self):
        try:
            self._frame(b"", 0x8)
            self.sock.close()
        except Exception:
            pass
