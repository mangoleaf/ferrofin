# U21: device selection

Compared the permission consumer, session admission, public-user picker and
user-update behavior with Jellyfin `4910aafa1a`: `DeviceManager.CanAccessDevice`,
`SessionManager.AuthenticateNewSession`, `UserController.Get`, and
`Users/DeviceAccessHost` plus `UserManager.RenameUser`.

Existing all-device, selected-device, case-insensitive matching, administrator
and nonpersistent-ID exceptions are correct. Fixed three missing details:

- Device admission failures return 403, matching the upstream security error.
- After setup, authenticated `/Users/Public` callers see only users allowed on
  their device. Anonymous headers do not create authenticated claims; anonymous
  callers and the startup wizard retain upstream behavior.
- Renaming a restricted user logs out existing disallowed devices and clears
  their cached tokens. In this pinned source, policy/configuration saves do not
  emit `OnUserUpdated`, so changing the device list alone retains existing tokens.
  New authentication and session-target filtering use the saved policy immediately.

Validation: 61 device/session tests, six real-manager HTTP tests, all 891 API
tests, SQL boundary, formatting, strict workspace Clippy and server build pass.
The account-update unit fixture now explicitly declares unrestricted device
access; real-manager regressions cover restricted accounts. A native server
passed all 12 live observations, including transient devices, both setup states,
case-insensitive admission, administrator override and actual token revocation.

Native debug-build `/Users/Public` median: 0.652 → 0.832 ms, p95:
0.811 → 1.591 ms (50 measured requests after ten warmups, two fixture users,
authenticated caller). Shared-host measurements, not a production performance
claim. Results: `/tmp/ferrofin-dashboard-u21-{before,after}-results.json`;
quality checks: `/tmp/ferrofin-dashboard-u21-checks.json`.
