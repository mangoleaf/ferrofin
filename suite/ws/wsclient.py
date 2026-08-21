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
_PARSED = urllib.parse.urlparse(BASE)
if _PARSED.scheme not in ("http", ""):
    # create_connection speaks plain TCP; an https base would fail the
    # handshake with a confusing error rather than an honest one.
    raise SystemExit(f"FERROFIN_BASE must be http:// (got {BASE!r}) — TLS is not supported")
HOST = _PARSED.hostname or "127.0.0.1"
PORT = _PARSED.port or 80


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


def http(method, path, token=None, body=None, **ident):
    """Returns (status, parsed-json-or-bytes)."""
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method)
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


class WS:
    """One open WebSocket, pumping received JSON messages into a list."""

    def __init__(self, path):
        self.sock = socket.create_connection((HOST, PORT), timeout=15)
        key = base64.b64encode(os.urandom(16)).decode()
        handshake = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {HOST}:{PORT}\r\n"
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
        head, self.buf = buf.split(b"\r\n\r\n", 1)
        status_line = head.split(b"\r\n")[0].decode(errors="replace")
        if "101" not in status_line:
            raise RuntimeError(f"upgrade refused: {status_line} :: {head[:300]!r}")
        self.msgs = []
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
                    with self.lock:
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

    def types(self):
        with self.lock:
            return [m.get("MessageType") for m in self.msgs]

    def drain(self):
        with self.lock:
            out, self.msgs = list(self.msgs), []
        return out

    def close(self):
        try:
            self._frame(b"", 0x8)
            self.sock.close()
        except Exception:
            pass
