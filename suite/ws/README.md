# `suite/ws` — server→client push probes

Everything else in the repo verifies pushes at the *seam*: a fake bus records a
string, or a mock manager records that a handler called it. Nothing opened a
real socket. These two files do.

- `wsclient.py` — a ~150-line stdlib RFC6455 client (no deps). Text frames only.
- `probe_remote_control.py` — two authenticated sessions with live `/socket`s;
  drives every remote-control (cast) and SyncPlay verb and asserts on what the
  **receiving socket** actually got.

```bash
cargo build -p ferrofin-server
./target/debug/ferrofin-server --data-dir /tmp/ferrofin-probe --bind 127.0.0.1 --port 18099 &
# a fresh DB seeds a passwordless `admin`
FERROFIN_BASE=http://127.0.0.1:18099 FERROFIN_USER=admin FERROFIN_PASS= \
  python3 suite/ws/probe_remote_control.py
```

Exit code is non-zero if any expected push never arrived. Checks that reference
library content (folder expansion, `NowPlayingItem` round-trip) self-skip when
the server has no library.

Gotcha when writing your own: closing the socket from another thread while the
pump thread is blocked in `recv()` does **not** send FIN — the server sees the
client hang around until its own timeout. Use `shutdown(SHUT_RDWR)` to simulate
a real abrupt disconnect.
