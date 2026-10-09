# G06: apply the library scan fanout limit

`LibraryScanFanoutConcurrency` now controls a shared scanner budget for folder
resolution and speculative media probes. Positive values are exact ceilings;
nonpositive values use `max(1, usable processors - 3)`, matching Jellyfin
v12.0-rc7 (`4910aafa1a`), `LimitedConcurrencyLibraryScheduler`. The production
reader observes committed configuration updates. Existing work drains after a
limit reduction; subsequent admission uses the new ceiling.

Nested folder work shares the same budget and proceeds inline when saturated.
Each worker owns its naming/context state; results and deletion guards merge in
input order. Workers inherit series/season ownership so extras retain their
owners. Cancellation joins folder workers and stops before writes or pruning.
The ordered foreground metadata/refresh lane reserves one slot, while speculative
probes acquire additional slots for their actual lifetime. The separate ffprobe
ceiling still applies. Metadata persistence remains ordered; this change does not
add parallel remote-provider refresh or close the separate
`LibraryMetadataRefreshConcurrency` finding.

## Verification

All 280 scanner-related tests, formatting, strict workspace Clippy and the server build pass.
Regressions cover live limit changes, processor-count defaults, shared nested
limits, cancellation, exact serial/parallel plans across library types, and
probe-to-item matching at limits 1, 2 and 4. The original serial planner and
unconfigured probe-window regressions also pass.

A real HTTP save followed by scanning 16 fresh videos at each setting on the
same disposable fixture changed the observed ffprobe peak from **8 → 1** at
limit 1, and **8 → 3** at limit 4. All 16 items in each pass had valid playback
metadata. Limit 4 leaves room for the ordered foreground work. Scan wall times
(including polling/readback) were **0.422 → 0.810 s** and **0.391 → 0.529 s**:
restricting concurrency intentionally trades throughput for resource usage.
The after fixture also retained the earlier scanned items; these are smoke
measurements, not a throughput benchmark. Final workspace tests, doctests,
per-crate coverage and the combined browse benchmark are batch-end gates.
