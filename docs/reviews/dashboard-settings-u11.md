# U11: internet streaming bitrate limit

PlaybackInfo now combines the client's explicit limit with the effective saved
remote limit. A positive user `RemoteClientBitrateLimit` replaces the server
default; zero/negative user values inherit it. The smaller client/remote limit
wins outside the LAN. LAN membership uses the live network configuration and
the effective client address normalized through trusted proxies. Saved user,
server, proxy and LAN changes take effect on the next request.

Compared with Jellyfin `v12.0-rc7`, commit `4910aafa1a`,
`MediaInfoHelper.GetMaxBitrate`. This is negotiation, not traffic shaping or a
second permission gate on arbitrary stream URLs. Upstream preserves explicit
nonpositive client limits; those values are covered without changing their
meaning. Device-profile defaults remain the builder's fallback when negotiation
supplies no explicit limit. U09's forced-copy restrictions still take precedence
over encoding a stream to a lower bitrate.

The negotiated video/audio limits already reach HLS and FFmpeg. Their values now
also distinguish cache files, preventing a lower-cap request from reusing output
encoded under an earlier cap with the same session/codec tuple.

Formatting, strict workspace Clippy, the SQL boundary gate, the server build and
**106 focused API/planner tests** pass. Real HTTP before/after on the same
generated high-bitrate fixture verified:

| Case | Before total negotiated bitrate | After |
|---|---:|---:|
| Inherit 1 Mbps server cap, client asks for 5 Mbps | 5 Mbps | 1 Mbps |
| User overrides server with 2 Mbps | 5 Mbps | 2 Mbps |
| Client asks for 0.5 Mbps | 0.5 Mbps | 0.5 Mbps |
| Negative user limit inherits server | 5 Mbps | 1 Mbps |
| No client limit, profile default 8 Mbps | 8 Mbps | 1 Mbps |
| Server cap disabled | 5 Mbps | 5 Mbps |
| LAN client with saved caps | 5 Mbps | 5 Mbps |
| Trusted forwarded IPv6 remote client, user cap 0.5 Mbps | 5 Mbps | 0.5 Mbps |
| Saved LAN expansion includes forwarded client | 5 Mbps | 5 Mbps |
| Forwarding header from an untrusted proxy is ignored | 5 Mbps | 5 Mbps |

Following the actual master/variant URLs produced real media segments. With a
0.5 Mbps saved cap and a 5 Mbps client request, FFmpeg's video `-maxrate` changed
from **4,930,306 to 430,306 bps** (the audio budget was 69,694 bps). First segment
size changed from **916,876 to 180,104 bytes**. Fifty warm PlaybackInfo requests
after ten warmups measured median **2.492 → 2.178 ms**, p95 **5.752 → 5.759 ms**;
the shared host makes these observations unsuitable for a performance claim.

This also implements the server-wide cap described by P01; that row retains its
place for a separate dashboard-section review.
