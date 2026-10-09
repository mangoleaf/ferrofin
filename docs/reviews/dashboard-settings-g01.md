# G01: advertise saved server names immediately

The application host now subscribes to committed server-configuration updates.
Saving `ServerName` updates the cached friendly name before the save completes;
public/private system information and the next discovery response use it without
a restart. The subscription holds a weak host reference so restarting can release
the old host and its configuration manager.

Jellyfin v12.0-rc7 (`4910aafa1a`), `ApplicationHost.FriendlyName`, falls back to the
machine name for an empty name and preserves whitespace-only names. Ferrofin now
matches that distinction instead of trimming whitespace to empty.

## Verification

All 14 application-host tests and five real UDP discovery tests pass. The discovery
regression now saves through the configuration manager without manually refreshing
the host. Host tests cover new names, empty-name fallback, whitespace preservation,
unrelated named updates, and releasing a stopped host. Formatting, strict workspace
Clippy, and the server build pass.

Real HTTP before/after checks used the same disposable fixture. The baseline kept
advertising `ferrofin` after saving a new name. The changed server immediately
returns the new name from both `/System/Info/Public` and `/System/Info`, preserves
whitespace exactly, and returns to the fallback when the saved name is empty.

Authenticated curl GETs used dev builds, 10 warmups and 50 samples per endpoint.
The shared host was running other work, so these local measurements are noisy:

| Endpoint | Median before / after (ms) | p95 before / after (ms) |
|---|---|---|
| `/System/Info/Public` | 1.033 / 1.590 | 2.107 / 2.422 |
| `/System/Info` | 1.909 / 1.233 | 4.193 / 2.821 |

The complete workspace suite, doctests and per-crate coverage run at the end of
the first 20 findings.
