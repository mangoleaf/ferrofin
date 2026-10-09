# U08: allow media playback

Item DTOs now derive requested `PlayAccess` from the saved `EnableMediaPlayback`
permission instead of always returning `Full`. The existing content-permissions
query supplies the flag once per page. Disabled playback yields `None`, including
for administrators, and saved changes take effect on the next request.

The original audit described a stronger boundary than Jellyfin implements.
Compared with Jellyfin `v12.0-rc7`, commit `4910aafa1a`, `BaseItem.GetPlayAccess`,
`DtoService`, and `SessionManager.SendPlayCommand`: this permission controls item
play access and session Play commands. It does not itself reject PlaybackInfo,
direct stream or HLS requests. Ferrofin's session Play check was already present;
jellyfin-web also checks the user's flag before playing. This setting is not a
server-side ban on fetching media through known URLs.

Formatting, strict workspace Clippy, the server build, the SQL boundary check,
and **174 focused core tests** pass. Added real-database regressions cover single
and page DTOs, live saved policy changes, ordinary users and administrators,
multiple item types, omitted fields, and requests without a user.

Real HTTP before/after on the same generated-media fixture verified six saved
policy combinations: both user types toggled off/on/off. Previously every item
and page advertised `Full`; now they return `None`/`Full`/`None`. PlaybackInfo
remains HTTP 200, matching the upstream boundary.

Fifty warm item-page requests after ten warmups measured median **1.927 → 1.998 ms**,
p95 **2.910 → 2.980 ms** on the shared host. These observations verify the exercised
path and do not establish a performance change.
