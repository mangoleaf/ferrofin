# U09: audio/video transcoding and remuxing permissions

PlaybackInfo now reapplies the resolved user's saved permissions after device
profile negotiation. Audio requires audio-transcoding permission; video can
advertise the negotiated HLS path when at least one of audio transcoding, video
transcoding or remuxing is allowed. A client transcoding veto still wins. An
administrator requesting playback for another user uses that user's policy.

The authenticated policy snapshot now reaches every HLS planning route,
including segment requests, and the progressive-stream fallback. Video requests
force video/audio copy independently when the corresponding permission is off,
even when a client requests encoding or incompatible output. The cache key
includes the saved permissions so the same URL cannot reuse an encoder job
created under a different policy.

Compared with Jellyfin `v12.0-rc7`, commit `4910aafa1a`: `MediaSourceManager`,
`MediaInfoHelper.SetDeviceSpecificData`, `StreamingHelpers.GetStreamingState`
and `EncodingHelper.TryStreamCopy`. The original audit's proposed hard HTTP
boundary was too broad: upstream remux permission controls negotiation, and
`TryStreamCopy` runs only for video requests. A direct audio-only URL does not
invoke that policy check. Userless API keys retain their existing behavior;
there is no administrator exception to a user's encoding restrictions.

Regressions cover all eight saved permission combinations, client vetoes,
audio-versus-video negotiation, each planning route, query-policy spoofing,
actual encoder arguments, cache separation and the upstream audio-only exception.

Formatting, strict workspace Clippy, the SQL boundary gate, the server build and
**103 focused API/planner tests** pass. Real HTTP checks on the same generated
H.264/AAC fixture exercised all eight policies and a repeated all-disabled case:

- Before: every policy advertised a transcode URL and the same segment URL kept
  serving the original video/audio encoding job.
- After: all-disabled negotiation advertises neither transcoding nor a transcode
  URL; other combinations retain the permitted HLS path. Actual FFmpeg logs and
  served segments confirm independent video/audio copy restrictions, including
  remux-only users and reuse after toggling back to an earlier policy.

Fifty warm posted PlaybackInfo requests after ten warmups measured median
**1.538 → 1.716 ms**, p95 **1.798 → 2.192 ms**. These are shared-host observations,
not a performance claim. Negotiation reuses the cached caller policy and reads
the item's media type once; an administrator acting for another user resolves
that user's policy. HLS planning adds no policy database read.
