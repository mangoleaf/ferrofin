# U17: remote control and shared devices

The manager now enforces Jellyfin's `AssertCanControl` before delivering a
command. Controlling one's own primary/additional-user session, a public target,
or a trusted userless/API-key operation remains allowed. Controlling another
user's session requires `EnableRemoteControlOfOtherUsers`, including for an
administrator with that permission disabled.

The same check now protects both capability endpoints, now-viewing reports and
guest-user add/remove. Attaching a different user also requires administrator
privileges, preventing changes to that user's playback history through guest
attachment. API keys can issue commands to an explicit target without resolving
a nonexistent user row. Trait implementations lacking control authorization
fail closed.

`EnableSharedDeviceControl` now filters public sessions from the controllable
list according to the **requested user's** preference, including administrator
and API-key requests on that user's behalf. The caller's device-access rules
also filter this list. Device permissions are resolved once per page, after
releasing the session mutex. Missing requested users return an empty list.

Compared with Jellyfin `v12.0-rc7` (`4910aafa1a`), `SessionManager.AssertCanControl`,
`AssertCanAttachUser`, `GetSessions` and the controller's mutating calls. Upstream
uses the shared-device setting as a list filter; it does not forbid commands to
public sessions. The implementation preserves that distinction.

**48 core session tests and 38 API session/playstate tests** pass, including
message delivery/absence, live permission changes, primary/additional/public
sessions, administrator and API-key cases, guest attachment and device filters.
Formatting, strict workspace Clippy, SQL boundary and the server build pass.

Real HTTP and WebSocket checks verified seven operations across ordinary/admin
saved off/on/off changes. Previously every disabled-user command/mutation
returned 204 and Pause/Mute reached the target socket; afterwards they return
403 with no command delivered. Enabled requests still deliver. An ordinary user
can no longer attach a different user; administrators can. A disallowed
persistent target device disappears from the list, and an API-key Pause changes
from 400 to 204. Public-session filtering is covered by the real-manager tests.

Fifty warm Pause requests after ten warmups measured median **0.565 → 0.775 ms**,
p95 **0.698 → 0.952 ms**. The disposable before/after servers ran on a shared
host; these are observations, not a performance claim.
