# U15: subtitle-management permission

Upload, remote search, remote download and remote-file retrieval now require
`EnableSubtitleManagement`, with the administrator/API-key overrides and
remote-access/schedule checks supplied by the shared user-permission policy.
They read the already-resolved authentication policy, so the added gate needs
no extra warm-request policy query. Saved changes apply on the next request.

Compared with Jellyfin `v12.0-rc7` (`4910aafa1a`), `SubtitleController` and the
`UserPermissionRequirement` handlers. The original audit overstated the deletion
gap: upstream intentionally requires administrator elevation for deletion even
when subtitle management is enabled. That rule remains in place. Subtitle
playback/conversion routes keep their separate upstream authorization rules.

**25 subtitle/Live TV permission tests** pass, including an ordinary denied user,
an enabled user, an administrator with the flag disabled and an API key across
all five management actions. Formatting, strict workspace Clippy, SQL boundary
and server build pass.

The batch run also caught an older malformed-subtitle test whose ordinary-user
fixture lacked the newly required permission. The fixture now explicitly grants
subtitle management so the request reaches input validation; all 14 video tests
pass, including the expected malformed-base64 400. The separate denied-user
matrix continues to verify that authorization happens first.

Thirty live HTTP probes verified saved off/on/off changes for ordinary and admin
accounts. Before the fix, disabled ordinary users could upload a real subtitle
(204), search (200), and reach remote download/fetch. Afterwards all four return
403; enabled users can upload/search/download, and deletion remains 403 for all
ordinary users. Administrators retain their override. No remote provider account
was used: an unknown remote subtitle ID exercises the provider error path after
authorization, not successful third-party delivery. U19 item visibility remains
an independent dependency.

Fifty warm searches after ten warmups measured median **0.787 → 0.867 ms**, p95
**1.255 → 1.063 ms** on the shared host, without a performance claim.
