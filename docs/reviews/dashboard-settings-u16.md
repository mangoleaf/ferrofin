# U16: Live TV access and management

Verified supported against Jellyfin `v12.0-rc7` (`4910aafa1a`). Live TV read
routes use `LiveTvAccess`, recording/timer mutations use `LiveTvManagement`, and
tuner/provider configuration retains administrator elevation. Management does
not implicitly grant viewing, nor does viewing grant recording management.
Administrators and API keys retain the upstream overrides; the shared policy
still checks remote-access restrictions before the administrator override.

No production change is needed. All seven existing permission tests pass,
covering denied/allowed handlers, API keys, administrators, remote access,
schedules and anonymous callers. The U15 check also passes strict workspace
Clippy, formatting, SQL boundary and build.

A disposable server passed **80 live HTTP checks** across both roles and all
four access/management combinations, then restored both flags to false. Info,
guide, channels and timer lists were tested separately from cancellation and
tuner configuration. Missing ordinary timers produce 204 and missing series
timers produce 404 after authorization; disallowed requests produce 403 before
lookup. An unknown tuner type remains 404 for an administrator and 403 for an
ordinary user regardless of either permission. No real recording was touched.
Fifty warm Info requests after ten warmups measured median **0.463 ms**, p95
**0.795 ms** on the shared host, without a performance claim.

This checks these dashboard flags at their controller boundaries. Generic item
and playback visibility remains U19; selecting individual plugin channels is
U20, and DVR settings have their own later findings.
