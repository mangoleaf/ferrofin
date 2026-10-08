# U03: saved lockout thresholds and concurrent failures

Authentication now serializes each user's credential check and counter update,
reloading the user inside the lock as Jellyfin does. Concurrent policy saves share
that lock; unrelated users have separate locks, and idle lock entries are weak.
Counter changes invalidate cached user data. Threshold -1 remains unlimited and
0 remains the default of 3; other negative thresholds now trigger lockout on the
first failure instead of silently becoming unlimited.

All 63 focused authorization/cache/user-manager tests, formatting, strict workspace
Clippy and the build pass. Real HTTP on the same fixture verifies saved thresholds
0, -1, -2 and 2, rejection after lockout, and successful administrator unlocks.
Most decisively, 24 simultaneous failures now record **24 attempts instead of 3**.
The burst took **19.343 → 36.005 ms**; serialization performs the previously lost
updates and intentionally prevents same-account authentication races. These are
empty-password fixture accounts, isolating bookkeeping from password hashing.

Final workspace tests, doctests and per-crate coverage remain batch-end gates.
