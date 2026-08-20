# `suite/ws` — server→client push verification

The parity sweep diffs HTTP responses. It cannot see a WebSocket message at
all — so for remote control (casting) and SyncPlay, where the entire
observable behaviour *is* the pushed message, it verified nothing but a status
code. Everything else in the repo tests these at the seam: a fake bus records a
string, or a mock manager records that a handler called it. Nothing opened a
real socket until this.

- `wsclient.py` — a ~150-line stdlib RFC6455 client (no dependencies).
- `seed_library.sh` — one movie + one two-episode series via ffmpeg, registered
  as libraries. Content-dependent checks self-skip without it.
- `probe_remote_control.py` — two authenticated sessions with live `/socket`s,
  driving every remote-control and SyncPlay verb and asserting on what the
  **receiving socket** actually got.

## Running

```bash
cargo build -p ferrofin-server
./target/debug/ferrofin-server --data-dir /tmp/ferrofin-probe --bind 127.0.0.1 --port 18099 &
# a fresh DB seeds a passwordless `admin` and logs it

FERROFIN_BASE=http://127.0.0.1:18099 FERROFIN_USER=admin FERROFIN_PASS= \
  suite/run.sh push
```

`suite/run.sh push` seeds the fixture then runs the probe. To skip seeding, run
`probe_remote_control.py` directly — content-dependent checks report `SKIP`
rather than passing vacuously. Exit code is non-zero if any expected push never
arrived.

The fixture lives in `/tmp/ferrofin-probe-media` (`FERROFIN_PROBE_MEDIA` to
move it), deliberately separate from `suite/perf/fixtures` — those are sized
for the benchmark and regenerating them would invalidate the perf baseline.

## What it covers

All 22 `/SyncPlay/*` ops and the remote-control surface: capabilities →
`SupportsRemoteControl`, `Play` (including folder expansion and
shuffle/instant-mix translation), `Playstate`, `GeneralCommand`,
`DisplayMessage`, `System`, `Viewing`, the `NowPlayingItem`/`PlayState` return
path, the full group lifecycle and queue editing, and the `SyncPlayAccess`
policy table.

## Gotchas

- Closing a socket from another thread while the pump thread is blocked in
  `recv()` does **not** send FIN — the server sees the client linger until its
  own timeout. Use `shutdown(SHUT_RDWR)` to simulate a real abrupt disconnect.
  (This cost an afternoon and a false "ghost group" bug report.)
- Group membership is per *user*, not per session (C#
  `ISyncPlayManager.IsUserActive`). Both probe sessions authenticate as the
  same user, so both must leave before that user is out of every group.
- A queue change is refused unless every member can see the items, so queue
  and transport verbs need a real library item — a synthetic id is rejected by
  design, not a bug.
