# U04: maximum active sessions

Session admission already correctly enforces positive limits across concurrent
logins. Its rejection now returns **403**, matching Jellyfin's SecurityException,
instead of incorrectly treating the valid credentials as a 401.

All 46 session-manager tests, formatting, strict workspace Clippy and the build
pass. New regressions and real HTTP verify that -1 and 0 admit all eight attempted
sessions; limits 1 and 2 admit exactly one and two. At capacity, even logging in
again on the same device is rejected before token replacement, matching upstream.
Lowering a cap keeps existing sessions usable and rejects additional logins.

On the same disposable fixture, eight-login bursts measured **11.874 → 12.713 ms**
at -1, **8.225 → 8.477 ms** at 0, **6.994 → 8.991 ms** at 1, and
**6.630 → 8.822 ms** at 2. The only production change is the error category;
these small shared-host differences are not a performance claim. Final workspace
tests, doctests and per-crate coverage remain batch-end gates.
