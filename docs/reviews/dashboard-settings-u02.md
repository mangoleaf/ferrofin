# U02: administrator, disabled and hidden accounts

Disabled users are now rejected when resolving existing device tokens. Previously
the dashboard disable action revoked tokens, but automatic lockout left those
tokens usable. The shared authorization context now carries the saved user policy
and rejects disabled users on both cold and cached resolutions. Policy changes
invalidate it through the existing generation-protected cache. Warm requests share
the policy by Arc and require no additional database reads; cold resolutions
assemble the policy through UserManager.

All 57 focused authorization/cache/user-manager tests, formatting, strict workspace
Clippy and the server build pass. Real HTTP on the same disposable fixture verifies
hidden users disappear from public listings but can log in by name, administrator
promotion/demotion immediately changes access to `/Devices`, and manual disable
rejects both tokens and new logins. Automatic lockout now changes the existing
token's `/Users/Me` response from erroneous **200 to 401**, without a restart.

Ten warmups and 50 `/Users/Me` requests measured median **0.530 → 0.493 ms**,
p95 **0.692 → 0.659 ms**. These small differences are shared-host noise. Final
workspace tests, doctests and per-crate coverage remain batch-end gates.
