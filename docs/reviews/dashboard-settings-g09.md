# G09: verify dashboard overview widgets

The overview services are supported. Added a regression proving both system-info
and storage DTOs resolve effective cache, image-cache, metadata and default
transcode paths after saved directory changes. Clearing metadata restores its
default immediately; clearing cache continues reporting the current root until
restart. This closes the widget verification; encoding-specific destination
controls remain in their own findings.

All 56 focused system/session/activity tests, formatting, strict workspace Clippy
and the server build pass. Real HTTP checks on both builds verify:

- The fixture's authenticated device appears in `/Sessions` with its user and
  effective client address.
- A real scan reports Running, progress from 0 through approximately 99%, then
  Idle with a Completed execution record; all 16 new videos have playback data.
- The activity widget contains actual authentication/session entries.
- `/System/Info` and `/System/Info/Storage` agree on changed paths. Storage includes
  the configured library location and real nonzero filesystem usage.

Production code is unchanged. On the same disposable fixture, 10 warmups and
50 storage requests measured median **1.728 → 1.464 ms**, p95
**2.328 → 3.018 ms**. This is ordinary shared-host spread, not an optimization.
Final workspace tests, doctests and per-crate coverage remain batch-end gates.
