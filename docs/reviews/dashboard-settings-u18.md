# U18: SyncPlay access

SyncPlay authorization now retains the administrator override inherited from
Jellyfin's default authorization requirement. An administrator can create and
join groups even with `JoinGroups` or `None` saved. Ordinary users keep the
existing create/join/no-access rules. Revoking access while a user is in a group
still permits group operations and leaving, but denies listing or joining other
groups; after leaving, group operations are denied too.

Compared with Jellyfin `v12.0-rc7` (`4910aafa1a`),
`SyncPlayAccessRequirement`, both authorization handlers and
`UserManager.GetUserById`. The specialized handler never issues an unconditional
failure that would cancel the default handler's administrator success. API keys
still reach user resolution: their empty user ID causes an argument exception,
so the matching response is 400 rather than the previous 403.

**39 API session/playstate tests** pass, including the administrator/API-key
matrix, ordinary policy modes and active membership. Formatting, strict
workspace Clippy, SQL boundary and the server build pass. Broader validation is
recorded in the batch checkpoint.

Twenty-one real HTTP probes verified saved mode changes for ordinary and
administrator accounts, active-member revocation and API-key requests. Admin
create with `JoinGroups` changed from 403 to 200; with `None`, create/join changed
from 403 to 200/204. Ordinary-user behavior remained correct. API-key List
changed from 403 to 400. Disposable groups and accounts were removed afterwards.

Fifty warm List requests after ten warmups measured median **0.593 → 0.691 ms**,
p95 **0.763 → 1.303 ms**. These shared-host observations are not a performance
claim.
