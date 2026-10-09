# U06: built-in password recovery and reset-provider selection

The reset-provider selector has one registered choice. Jellyfin also falls back
to its built-in provider for empty or unknown saved IDs, so adding a registry of
unavailable providers would not fix this setting. Verified all three selections
against the actual built-in recovery flow and corrected its behavioral gaps.

Only a known user on the effective local network now receives a PIN file. Unknown,
blank and remote requests receive the same PinCode response shape without creating
a file. Paths use the invariant-uppercase username hash under ProgramDataPath,
matching Jellyfin. Files are atomically replaced with mode 0600; PINs are no longer
logged. Comparison ignores dashes but preserves case. The exact entered PIN becomes
the password, a failed password update preserves the file, successful recovery
records password-change activity, and serialized redemption consumes it once.
Expired records are removed; unmatched/replayed PINs return 404.

All 21 focused API user tests, formatting, strict workspace Clippy and the build
pass. Tests exercise failed password storage, missing users, expiration, concurrent
redemption and file permissions. Real HTTP on the same fixture verifies provider
fallback, uniform unknown/blank responses, lowercase rejection, dashless-password
login, replay rejection and remote requests through a configured trusted proxy.
Remote requests previously created files; they no longer do.

One full PIN redemption (including password hashing) measured
**4088.463 → 2218.474 ms** while builds/coverage ran on the shared host. This is a
functional-run observation, not an optimization claim. Final workspace tests,
doctests and per-crate coverage remain batch-end gates.
