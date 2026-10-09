# U10: force transcoding of remote sources

Playback negotiation now consumes `ForceRemoteSourceTranscoding`. A remote
source cannot advertise direct play/stream when the flag is enabled, and its
delivery URL disables both video and audio copying. Local sources retain their
normal decisions. Saved changes use the policy cache's existing invalidation.
The output cache also distinguishes copy vetoes so changed delivery requests
cannot reuse the opposite encoding mode.

Compared with Jellyfin `v12.0-rc7`, commit `4910aafa1a`,
`MediaInfoHelper.SetDeviceSpecificData`: remote means the source's `IsRemote`,
not the client's network location. Upstream preserves the negotiated container
and protocol in its direct-play branch, including an HTTP `Static=true` URL.
That behavior is covered explicitly; this flag does not universally guarantee
encoding or authorize operations forbidden by U09's permissions.

Formatting, strict workspace Clippy, the SQL boundary gate, the server build and
**104 focused API/planner tests** pass. Regressions cover local/remote sources,
off/on/off changes, direct and HLS decisions, encoding permission restrictions,
and separation of copied and encoded cache files.

The live fixture also exposed an independent source-resolution gap, recorded as
**S01 (open)** in the living review: scanned `.strm` files remain local sources
without playable remote metadata. The next implementation must resolve remote
shortcut targets, propagate their protocol/`IsRemote`, and probe them on demand,
while preserving upstream's rejection of local-file shortcut targets. U10's
real HTTP verification uses the existing remote M3U tuner-source implementation.

Real HTTP before/after on the same tuner fixture verified twelve combinations:
remote/local sources, direct-play requested/vetoed, and the flag off/on/off.
Previously neither URL copy veto was emitted; now both appear only while the
remote source's flag is enabled, and disappear immediately when it is disabled.
Fifty warm requests after ten warmups measured median **1.515 → 1.672 ms**,
p95 **2.663 → 2.721 ms** on the shared host, without a performance claim.

Tracing also identified **S02 (open)**: audio-specific device-profile selection
has not been ported; negotiation always uses the video StreamBuilder entry point.
The living document records the required audio profile/URL port and validation.
